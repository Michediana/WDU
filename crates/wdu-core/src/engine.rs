//! One scan: inventory, every provider, matching. Shared by the CLI and the service.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::matching::{self, Plan, Policy, Proposal};
use crate::model::MachineInfo;
use crate::oem::{self, CatalogQuery, Http};
use crate::provider::Provider;
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

pub fn scan(http: &Http, options: &ScanOptions, policy: &Policy) -> Result<ScanResult> {
    let machine = machine::read_machine_info()?;
    let devices = inventory::enumerate_devices(Default::default())?;

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
            Err(err) => source_errors.push(SourceError {
                source: "oem".into(),
                error: format!("{err:#}"),
            }),
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
        match provider.candidates() {
            Ok(found) => candidates.extend(found),
            Err(err) => source_errors.push(SourceError {
                source: provider.source().to_string(),
                error: format!("{err:#}"),
            }),
        }
    }

    Ok(ScanResult {
        at: crate::time::now_rfc3339(),
        plan: matching::plan(&devices, &candidates, policy),
        machine,
        source_errors,
        wu_policy: wua::read_update_policy(),
    })
}
