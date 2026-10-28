//! One scan: inventory, every provider, matching. Shared by the CLI and the service.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::matching::{self, Plan, Policy, Proposal};
use crate::model::MachineInfo;
use crate::oem::{self, CatalogQuery, Http};
use crate::provider::{Provider, Source};
use crate::wua::{self, UpdatePolicy};
use crate::{inventory, machine};

/// Machine-wide data: history, backups, downloaded packages, service state.
pub fn data_dir() -> PathBuf {
    std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("WDU")
}

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub oem: bool,
    pub wu: bool,
    pub wu_timeout: Duration,
    /// `None`: the catalog of this machine.
    pub catalog: Option<CatalogQuery>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            oem: true,
            wu: true,
            wu_timeout: wua::DEFAULT_SEARCH_TIMEOUT,
            catalog: None,
        }
    }
}

/// A source that could not be searched; the scan goes on without it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceError {
    pub source: String,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScanResult {
    /// RFC 3339, UTC.
    pub at: String,
    pub machine: MachineInfo,
    pub plan: Plan,
    pub source_errors: Vec<SourceError>,
    pub wu_policy: UpdatePolicy,
}

impl ScanResult {
    pub fn proposal(&self, id: &str) -> Option<&Proposal> {
        self.plan
            .proposals
            .iter()
            .find(|p| p.candidate.id.eq_ignore_ascii_case(id))
    }
}

/// One step of a scan: what is searched and where, for progress reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScanStep {
    pub kind: StepKind,
    /// A catalog URL or a WSUS server; `None` for steps on this PC, and for
    /// Windows Update on Microsoft's servers.
    pub place: Option<String>,
    pub state: StepState,
    /// Devices listed, drivers offered, or updates proposed.
    pub found: Option<usize>,
    /// The machine model, or the error of a failed step.
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    Machine,
    Devices,
    /// `None`: the OEM catalog of a machine whose brand is not known.
    Search {
        source: Option<Source>,
    },
    Matching,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    Running,
    Done,
    Failed,
}

/// The steps so far, sent to a callback on every change.
struct Progress<F: FnMut(&[ScanStep])> {
    steps: Vec<ScanStep>,
    report: F,
}

impl<F: FnMut(&[ScanStep])> Progress<F> {
    fn start(&mut self, kind: StepKind, place: Option<String>) {
        self.steps.push(ScanStep {
            kind,
            place,
            state: StepState::Running,
            found: None,
            detail: None,
        });
        (self.report)(&self.steps);
    }

    /// Closes the last step.
    fn end(&mut self, state: StepState, found: Option<usize>, detail: Option<String>) {
        if let Some(step) = self.steps.last_mut() {
            step.state = state;
            step.found = found;
            step.detail = detail;
        }
        (self.report)(&self.steps);
    }

    fn fail(&mut self, err: &anyhow::Error) {
        self.end(StepState::Failed, None, Some(format!("{err:#}")));
    }
}

pub fn scan(http: &Http, options: &ScanOptions, policy: &Policy) -> Result<ScanResult> {
    scan_with_progress(http, options, policy, |_| {})
}

/// [`scan`], calling `report` with every step started or ended.
pub fn scan_with_progress(
    http: &Http,
    options: &ScanOptions,
    policy: &Policy,
    report: impl FnMut(&[ScanStep]),
) -> Result<ScanResult> {
    let mut progress = Progress {
        steps: Vec::new(),
        report,
    };
    progress.start(StepKind::Machine, None);
    let machine = machine::read_machine_info().inspect_err(|err| progress.fail(err))?;
    progress.end(
        StepState::Done,
        None,
        Some(format!("{} {}", machine.manufacturer, machine.model)),
    );
    progress.start(StepKind::Devices, None);
    let devices =
        inventory::enumerate_devices(Default::default()).inspect_err(|err| progress.fail(err))?;
    progress.end(StepState::Done, Some(devices.len()), None);

    // OEM first: it is fast, and it is the preferred source.
    let mut providers: Vec<Box<dyn Provider>> = Vec::new();
    let mut source_errors = Vec::new();
    if options.oem {
        let query = match &options.catalog {
            Some(query) => Ok(query.clone()),
            None => CatalogQuery::for_machine(&machine),
        };
        match query.and_then(|q| oem::provider(http.clone(), q)) {
            Ok(provider) => providers.push(provider),
            Err(err) => {
                progress.start(StepKind::Search { source: None }, None);
                progress.fail(&err);
                source_errors.push(SourceError {
                    source: "oem".into(),
                    error: format!("{err:#}"),
                });
            }
        }
    }
    if options.wu {
        providers.push(Box::new(wua::WindowsUpdateProvider {
            timeout: options.wu_timeout,
            ..Default::default()
        }));
    }

    let mut candidates = Vec::new();
    for provider in &providers {
        tracing::info!(source = %provider.source(), "searching");
        progress.start(
            StepKind::Search {
                source: Some(provider.source()),
            },
            provider.location(),
        );
        match provider.candidates() {
            Ok(found) => {
                progress.end(StepState::Done, Some(found.len()), None);
                candidates.extend(found);
            }
            Err(err) => {
                progress.fail(&err);
                source_errors.push(SourceError {
                    source: provider.source().to_string(),
                    error: format!("{err:#}"),
                });
            }
        }
    }

    progress.start(StepKind::Matching, None);
    let plan = matching::plan(&devices, &candidates, policy);
    progress.end(StepState::Done, Some(plan.proposals.len()), None);

    Ok(ScanResult {
        at: crate::time::now_rfc3339(),
        plan,
        machine,
        source_errors,
        wu_policy: wua::read_update_policy(),
    })
}
