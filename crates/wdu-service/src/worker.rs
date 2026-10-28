//! What the service does: answers requests, runs scans and installations on a
//! worker thread, and schedules periodic checks.
//!
//! Every request is validated here: installs only accept IDs proposed by the
//! last scan, configuration goes through [`ServiceConfig::validate`], and only
//! one scan or installation runs at a time.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result};
use wdu_core::driver_store;
use wdu_core::engine::{self, ScanOptions, ScanResult};
use wdu_core::history::{History, Outcome};
use wdu_core::inventory;
use wdu_core::ipc::{
    PROTOCOL_VERSION, Request, Response, ServiceConfig, ServiceState, ServiceStatus,
};
use wdu_core::oem::Http;
use wdu_core::session::{InstallSession, SessionOptions};
use wdu_core::time;

/// Delay before the first automatic check after the service starts.
const STARTUP_DELAY: Duration = Duration::from_secs(120);
const SCHEDULER_TICK: Duration = Duration::from_secs(30);

enum Job {
    /// `automatic`: started by the schedule, so `auto_install` applies.
    Scan {
        automatic: bool,
    },
    Install(Vec<String>),
    RemovePackages(Vec<String>),
}

struct State {
    status: ServiceStatus,
    config: ServiceConfig,
    scan: Option<ScanResult>,
    last_scan_unix: Option<u64>,
}

pub struct Service {
    state: Mutex<State>,
    jobs: Sender<Job>,
    data_dir: PathBuf,
}

impl Service {
    /// Loads the saved state and starts the worker and the scheduler.
    pub fn start(data_dir: PathBuf, stop: Arc<AtomicBool>) -> Result<Arc<Service>> {
        fs::create_dir_all(&data_dir)
            .with_context(|| format!("cannot create {}", data_dir.display()))?;
        let config: ServiceConfig = read_json(&data_dir.join("config.json")).unwrap_or_default();
        let scan_path = data_dir.join("last_scan.json");
        let scan: Option<ScanResult> = read_json(&scan_path);
        let last_scan_unix = scan.as_ref().and_then(|_| modified_unix(&scan_path));

        let (jobs, receiver) = mpsc::channel();
        let service = Arc::new(Service {
            state: Mutex::new(State {
                status: ServiceStatus {
                    last_scan: scan.as_ref().map(|s| s.at.clone()),
                    ..Default::default()
                },
                config,
                scan,
                last_scan_unix,
            }),
            jobs,
            data_dir,
        });
        service.state().update_next_scan();

        let http = Http::new()?.with_cache(service.data_dir.join("cache"));
        let worker = service.clone();
        std::thread::Builder::new()
            .name("worker".into())
            .spawn(move || worker.work(receiver, http))?;
        let scheduler = service.clone();
        std::thread::Builder::new()
            .name("scheduler".into())
            .spawn(move || scheduler.schedule(stop))?;
        Ok(service)
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn handle(&self, request: Request) -> Response {
        match request {
            Request::Hello => Response::Hello {
                version: env!("CARGO_PKG_VERSION").into(),
                protocol: PROTOCOL_VERSION,
            },
            Request::Status => Response::Status {
                status: self.state().status.clone(),
            },
            Request::Scan => self.enqueue(ServiceState::Scanning, Job::Scan { automatic: false }),
            Request::Plan => Response::Plan {
                scan: self.state().scan.clone().map(Box::new),
            },
            Request::Install { ids } => {
                if ids.is_empty() {
                    return Response::error("no update selected");
                }
                {
                    let state = self.state();
                    let Some(scan) = &state.scan else {
                        return Response::error("no scan yet: search for updates first");
                    };
                    if let Some(unknown) = ids.iter().find(|id| scan.proposal(id).is_none()) {
                        return Response::error(format!(
                            "`{unknown}` is not among the proposed updates"
                        ));
                    }
                }
                self.enqueue(ServiceState::Installing, Job::Install(ids))
            }
            Request::History => match History::new(self.data_dir.join("history.jsonl")).read() {
                Ok(records) => Response::History { records },
                Err(err) => Response::error(format!("{err:#}")),
            },
            Request::Devices => match inventory::enumerate_devices(Default::default()) {
                Ok(devices) => Response::Devices { devices },
                Err(err) => Response::error(format!("{err:#}")),
            },
            Request::DriverPackages => match driver_store::enumerate_packages() {
                Ok(packages) => Response::DriverPackages { packages },
                Err(err) => Response::error(format!("{err:#}")),
            },
            Request::RemovePackages { names } => {
                if names.is_empty() {
                    return Response::error("no driver package selected");
                }
                // Checked against the store now, not against what the client saw.
                let packages = match driver_store::enumerate_packages() {
                    Ok(packages) => packages,
                    Err(err) => return Response::error(format!("{err:#}")),
                };
                for name in &names {
                    let package = packages
                        .iter()
                        .find(|p| p.published_name.eq_ignore_ascii_case(name));
                    if !package.is_some_and(|p| p.is_old_unused()) {
                        return Response::error(format!(
                            "`{name}` is not an older driver version that no device uses"
                        ));
                    }
                }
                self.enqueue(ServiceState::Removing, Job::RemovePackages(names))
            }
            Request::GetConfig => Response::Config {
                config: self.state().config.clone(),
            },
            Request::SetConfig { config } => {
                if let Err(err) = config.validate() {
                    return Response::error(format!("{err:#}"));
                }
                if let Err(err) = write_json(&self.data_dir.join("config.json"), &config) {
                    return Response::error(format!("{err:#}"));
                }
                let mut state = self.state();
                state.config = config.clone();
                state.update_next_scan();
                tracing::info!(?config, "configuration changed");
                Response::Config { config }
            }
        }
    }

    /// Marks the service busy and queues the job, unless something already runs.
    fn enqueue(&self, busy: ServiceState, job: Job) -> Response {
        let mut state = self.state();
        if state.status.state != ServiceState::Idle {
            return Response::error(format!("busy: {:?}", state.status.state).to_lowercase());
        }
        state.status.state = busy;
        state.status.last_error = None;
        if self.jobs.send(job).is_err() {
            state.status.state = ServiceState::Idle;
            return Response::error("the worker stopped");
        }
        Response::Accepted
    }

    fn work(&self, jobs: Receiver<Job>, http: Http) {
        for job in jobs {
            match job {
                Job::Scan { automatic } => self.scan(&http, automatic),
                Job::Install(ids) => self.install(&http, &ids),
                Job::RemovePackages(names) => self.remove_packages(&names),
            }
            let mut state = self.state();
            state.status.state = ServiceState::Idle;
            state.status.current = None;
        }
    }

    fn scan(&self, http: &Http, automatic: bool) {
        {
            let mut state = self.state();
            state.status.state = ServiceState::Scanning;
            state.status.current = None;
            state.status.scan_steps.clear();
        }
        let (options, policy) = {
            let state = self.state();
            let options = ScanOptions {
                oem: state.config.oem_catalog,
                wu: state.config.windows_update,
                ..Default::default()
            };
            (options, state.config.policy.clone())
        };
        tracing::info!(automatic, "scan started");
        let result = engine::scan_with_progress(http, &options, &policy, |steps| {
            self.state().status.scan_steps = steps.to_vec();
        });
        let auto_install = {
            let mut state = self.state();
            state.last_scan_unix = Some(time::unix_now());
            state.update_next_scan();
            match result {
                Ok(scan) => {
                    tracing::info!(proposals = scan.plan.proposals.len(), "scan finished");
                    if let Err(err) = write_json(&self.data_dir.join("last_scan.json"), &scan) {
                        tracing::warn!("cannot save the scan: {err:#}");
                    }
                    state.status.last_scan = Some(scan.at.clone());
                    let ids: Vec<String> = scan
                        .plan
                        .proposals
                        .iter()
                        .map(|p| p.candidate.id.clone())
                        .collect();
                    state.scan = Some(scan);
                    (automatic && state.config.auto_install && !ids.is_empty()).then_some(ids)
                }
                Err(err) => {
                    tracing::error!("scan failed: {err:#}");
                    state.status.last_error = Some(format!("{err:#}"));
                    None
                }
            }
        };
        if let Some(ids) = auto_install {
            tracing::info!(count = ids.len(), "installing by policy");
            self.install(http, &ids);
        }
    }

    fn install(&self, http: &Http, ids: &[String]) {
        let proposals: Vec<_> = {
            let mut state = self.state();
            state.status.state = ServiceState::Installing;
            state.status.warnings.clear();
            let Some(scan) = &state.scan else { return };
            ids.iter()
                .filter_map(|id| scan.proposal(id).cloned())
                .collect()
        };
        let options = SessionOptions {
            data_dir: self.data_dir.clone(),
            ..Default::default()
        };
        let mut session = match InstallSession::begin(http, options) {
            Ok(session) => session,
            Err(err) => {
                self.state().status.last_error = Some(format!("{err:#}"));
                return;
            }
        };
        let mut failed = Vec::new();
        for proposal in &proposals {
            self.state().status.current = Some(proposal.candidate.id.clone());
            let record = session.install(proposal);
            tracing::info!(id = %record.id, outcome = ?record.outcome, detail = ?record.detail, "installed");
            if record.outcome != Outcome::Succeeded {
                failed.push(record.id.clone());
            }
            if record.reboot_required {
                self.state().status.reboot_required = true;
            }
        }
        let warnings = session.finish();
        {
            let mut state = self.state();
            state.status.warnings = warnings;
            if !failed.is_empty() {
                state.status.last_error = Some(format!(
                    "not installed: {} (see history)",
                    failed.join(", ")
                ));
            }
        }
        // Refresh the plan: installed updates drop out of it.
        self.scan(http, false);
    }

    fn remove_packages(&self, names: &[String]) {
        self.state().status.warnings.clear();
        let mut failed = Vec::new();
        for name in names {
            self.state().status.current = Some(name.clone());
            if let Err(err) = driver_store::delete_package(name) {
                tracing::warn!("{err:#}");
                failed.push(name.as_str());
            }
        }
        if !failed.is_empty() {
            self.state().status.last_error = Some(format!(
                "not removed: {} (see the service log)",
                failed.join(", ")
            ));
        }
    }

    fn schedule(&self, stop: Arc<AtomicBool>) {
        let mut wait = STARTUP_DELAY;
        loop {
            while !wait.is_zero() {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                let step = wait.min(Duration::from_secs(1));
                std::thread::sleep(step);
                wait -= step;
            }
            wait = SCHEDULER_TICK;
            let due = {
                let state = self.state();
                let interval = u64::from(state.config.check_interval_hours) * 3600;
                interval > 0
                    && state.status.state == ServiceState::Idle
                    && state
                        .last_scan_unix
                        .is_none_or(|last| time::unix_now() >= last + interval)
            };
            if due {
                let _ = self.enqueue(ServiceState::Scanning, Job::Scan { automatic: true });
            }
        }
    }
}

impl State {
    fn update_next_scan(&mut self) {
        let interval = u64::from(self.config.check_interval_hours) * 3600;
        self.status.next_scan = (interval > 0).then(|| {
            let next = self
                .last_scan_unix
                .map_or(time::unix_now(), |last| last + interval);
            time::rfc3339(next.max(time::unix_now()))
        });
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> Option<T> {
    let bytes = fs::read(path).ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(value) => Some(value),
        Err(err) => {
            tracing::warn!(path = %path.display(), "ignoring unreadable file: {err}");
            None
        }
    }
}

fn write_json<T: serde::Serialize>(path: &std::path::Path, value: &T) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    fs::rename(&tmp, path).with_context(|| format!("cannot write {}", path.display()))
}

fn modified_unix(path: &std::path::Path) -> Option<u64> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}
