//! Safe installation sessions.
//!
//! Sequence for every update: a System Restore point for the session, a
//! backup of the current driver of each target device, the installation, a
//! check of each device, and, when a device disappeared or reports a problem,
//! the previous driver reinstalled (forced) from the backup.
//!
//! Rolling back restores the driver only: whatever else a vendor installer
//! added stays in place.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::history::{History, HistoryRecord, Outcome};
use crate::install::{self, find_infs, safe_name, system32};
use crate::matching::Proposal;
use crate::model::Device;
use crate::oem::Http;
use crate::provider::Source;
use crate::restore::RestorePoint;
use crate::{inventory, wua};

/// `CM_PROB_NEED_RESTART`: the new driver loads after a restart.
const CM_PROB_NEED_RESTART: u32 = 14;
/// How long a device may take to start again after an installation.
const SETTLE_TIME: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
pub struct SessionOptions {
    /// Receives `history.jsonl`, `backup\` and `packages\`.
    pub data_dir: PathBuf,
    pub wu_timeout: Duration,
    pub restore_point: bool,
    /// Testing only: treat every installation as failed, to exercise rollback.
    pub inject_failure: bool,
}

impl Default for SessionOptions {
    fn default() -> Self {
        SessionOptions {
            data_dir: crate::engine::data_dir(),
            wu_timeout: wua::DEFAULT_SEARCH_TIMEOUT,
            restore_point: true,
            inject_failure: false,
        }
    }
}

/// The driver a device had before an installation, ready to be reinstalled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriverBackup {
    /// Published name, e.g. `oem42.inf`.
    pub inf_name: String,
    /// INF to reinstall: the exported copy, or the inbox INF in `%windir%\INF`.
    pub inf_path: PathBuf,
}

pub struct InstallSession<'a> {
    http: &'a Http,
    options: SessionOptions,
    history: History,
    restore_point: Option<RestorePoint>,
    backup_dir: PathBuf,
    touched_system: bool,
    /// Problems that do not stop the session, such as System Restore being off.
    pub warnings: Vec<String>,
}

impl<'a> InstallSession<'a> {
    pub fn begin(http: &'a Http, options: SessionOptions) -> Result<Self> {
        if !install::is_elevated() {
            bail!("installing drivers needs administrator rights");
        }
        let stamp = crate::time::now_rfc3339().replace(':', "-");
        let mut warnings = Vec::new();
        let restore_point = if options.restore_point {
            match RestorePoint::begin("WDU driver update") {
                Ok(point) => Some(point),
                Err(err) => {
                    warnings.push(format!("{err:#}; relying on driver backups only"));
                    None
                }
            }
        } else {
            None
        };
        Ok(InstallSession {
            http,
            history: History::new(options.data_dir.join("history.jsonl")),
            backup_dir: options.data_dir.join("backup").join(stamp),
            options,
            restore_point,
            touched_system: false,
            warnings,
        })
    }

    /// Installs one proposal and records the outcome in the history.
    pub fn install(&mut self, proposal: &Proposal) -> HistoryRecord {
        let c = &proposal.candidate;
        let mut record = HistoryRecord::new(
            c.source,
            &c.id,
            &c.title,
            proposal
                .devices
                .iter()
                .map(|d| d.instance_id.clone())
                .collect(),
        );
        match self.install_checked(proposal) {
            Ok(checked) => {
                record.outcome = checked.outcome;
                record.reboot_required = checked.reboot;
                record.detail = Some(checked.detail);
            }
            Err(err) => record.detail = Some(format!("{err:#}")),
        }
        if let Err(err) = self.history.append(&record) {
            self.warnings
                .push(format!("cannot record the outcome: {err:#}"));
        }
        record
    }

    /// Closes the restore point: kept if anything was installed, discarded otherwise.
    pub fn finish(mut self) -> Vec<String> {
        if let Some(point) = self.restore_point.take() {
            let result = if self.touched_system {
                point.end()
            } else {
                point.cancel()
            };
            if let Err(err) = result {
                self.warnings.push(format!("{err:#}"));
            }
        }
        self.warnings
    }

    fn install_checked(&mut self, proposal: &Proposal) -> Result<Checked> {
        // Any failure before the installation leaves the system untouched.
        let mut targets = Vec::new();
        for d in &proposal.devices {
            let device = inventory::find_device(&d.instance_id)?
                .with_context(|| format!("device {} not found", d.instance_id))?;
            let backup = backup_driver(&device, &self.backup_dir)
                .with_context(|| format!("cannot back up the driver of {}", d.instance_id))?;
            targets.push((device, backup));
        }

        self.touched_system = true;
        let installed = self.run(proposal);
        let checks: Vec<Check> = targets
            .iter()
            .map(|(before, _)| check(before))
            .collect::<Result<_>>()?;
        let changed = checks.iter().any(|c| c.changed);
        let broken: Vec<String> = targets
            .iter()
            .zip(&checks)
            .filter_map(|((d, _), c)| Some(format!("{}: {}", d.instance_id, c.problem.as_ref()?)))
            .collect();
        let needs_restart = checks.iter().any(|c| c.needs_restart);
        let injected = self.options.inject_failure;

        let cause = match installed {
            Ok((reboot, detail)) if broken.is_empty() && !injected => {
                let reboot = reboot || needs_restart;
                if !changed && !reboot {
                    return Ok(Checked {
                        outcome: Outcome::Failed,
                        reboot: false,
                        detail: format!("{detail}; the driver did not change on any device"),
                    });
                }
                return Ok(Checked {
                    outcome: Outcome::Succeeded,
                    reboot,
                    detail,
                });
            }
            // Failed before changing anything: nothing to undo.
            Err(err) if broken.is_empty() && !changed && !injected => return Err(err),
            Ok(_) if injected => "injected failure".to_string(),
            Ok(_) => format!("device check failed ({})", broken.join("; ")),
            Err(err) => format!("{err:#}"),
        };

        tracing::warn!(id = %proposal.candidate.id, cause, "rolling back");
        let mut notes = Vec::new();
        let mut restored_all = true;
        let mut reboot = false;
        for ((before, backup), c) in targets.iter().zip(&checks) {
            if !(injected || c.changed || c.problem.is_some()) {
                continue;
            }
            let id = &before.instance_id;
            let Some(backup) = backup else {
                restored_all = false;
                notes.push(format!("{id}: had no driver, nothing to restore"));
                continue;
            };
            match rollback(before, backup) {
                Ok(r) => {
                    reboot |= r;
                    notes.push(format!("{id}: restored {}", backup.inf_name));
                }
                Err(err) => {
                    restored_all = false;
                    notes.push(format!("{id}: rollback failed: {err:#}"));
                }
            }
        }
        Ok(Checked {
            outcome: if restored_all {
                Outcome::RolledBack
            } else {
                Outcome::Failed
            },
            reboot,
            detail: format!("{cause}; {}", notes.join("; ")),
        })
    }

    /// Returns whether a restart is needed, and how it was installed.
    fn run(&self, proposal: &Proposal) -> Result<(bool, String)> {
        let c = &proposal.candidate;
        if c.source == Source::WindowsUpdate {
            let result =
                wua::install_driver_updates(std::slice::from_ref(&c.id), self.options.wu_timeout)?
                    .into_iter()
                    .next()
                    .context("no result from Windows Update")?;
            if !result.succeeded() {
                bail!(
                    "{}",
                    result
                        .error
                        .unwrap_or_else(|| "Windows Update install failed".into())
                );
            }
            return Ok((result.reboot_required, "installed by Windows Update".into()));
        }
        let hardware_ids: Vec<String> = proposal
            .devices
            .iter()
            .flat_map(|d| d.hardware_ids.iter().cloned())
            .collect();
        let outcome = install::install_package(
            self.http,
            c,
            &hardware_ids,
            &self.options.data_dir.join("packages"),
        )?;
        Ok((
            outcome.reboot_required,
            serde_json::to_string(&outcome.method)?,
        ))
    }
}

struct Checked {
    outcome: Outcome,
    reboot: bool,
    detail: String,
}

struct Check {
    /// The driver differs from the one before the installation.
    changed: bool,
    needs_restart: bool,
    problem: Option<String>,
}

/// Reads the device again; a problem gets [`SETTLE_TIME`] to clear, since
/// PnP restarts devices asynchronously.
fn check(before: &Device) -> Result<Check> {
    let deadline = Instant::now() + SETTLE_TIME;
    loop {
        let now = inventory::find_device(&before.instance_id)?;
        let problem = match &now {
            None => Some("device disappeared".to_string()),
            Some(d) if !d.status.present => Some("device no longer present".to_string()),
            Some(d)
                if d.status.problem_code != 0 && d.status.problem_code != CM_PROB_NEED_RESTART =>
            {
                Some(format!("problem code {}", d.status.problem_code))
            }
            _ => None,
        };
        if problem.is_none() || Instant::now() >= deadline {
            return Ok(Check {
                changed: now.as_ref().is_none_or(|d| d.driver != before.driver),
                needs_restart: now
                    .as_ref()
                    .is_some_and(|d| d.status.problem_code == CM_PROB_NEED_RESTART),
                problem,
            });
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Keeps a copy of the driver package `device` uses now. Third-party
/// packages (`oemNN.inf`) are exported from the driver store, like
/// `pnputil /export-driver`; inbox drivers stay in `%windir%\INF`.
pub fn backup_driver(device: &Device, dir: &Path) -> Result<Option<DriverBackup>> {
    let Some(driver) = &device.driver else {
        return Ok(None);
    };
    let inf_name = driver
        .inf_name
        .clone()
        .context("the installed driver has no INF name")?;

    if !inf_name.to_ascii_lowercase().starts_with("oem") {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        let inf_path = Path::new(&root).join("INF").join(&inf_name);
        if !inf_path.exists() {
            bail!("inbox INF {} not found", inf_path.display());
        }
        return Ok(Some(DriverBackup { inf_name, inf_path }));
    }

    let dest = dir.join(safe_name(&device.instance_id));
    fs::create_dir_all(&dest)?;
    let out = Command::new(system32("pnputil.exe"))
        .arg("/export-driver")
        .arg(&inf_name)
        .arg(&dest)
        .output()
        .context("cannot run pnputil")?;
    if !out.status.success() {
        bail!(
            "pnputil /export-driver {inf_name} failed: {}",
            String::from_utf8_lossy(&out.stdout).trim()
        );
    }
    let ids: Vec<String> = device.all_ids().map(str::to_string).collect();
    let inf_path = find_infs(&dest, &ids)?
        .into_iter()
        .next()
        .with_context(|| format!("the export of {inf_name} has no INF for the device"))?;
    tracing::info!(inf_name, path = %inf_path.display(), "driver backed up");
    Ok(Some(DriverBackup { inf_name, inf_path }))
}

/// Reinstalls the backed up driver, forced, and checks the device again.
pub fn rollback(before: &Device, backup: &DriverBackup) -> Result<bool> {
    let ids: Vec<String> = before.all_ids().map(str::to_string).collect();
    let id = install::ids_in_inf(&backup.inf_path, &ids)?
        .into_iter()
        .next()
        .context("the backed up INF does not list the device")?;
    let reboot = install::update_driver(&backup.inf_path, &id, true)?
        .context("no device took the previous driver")?;
    let after = check(before)?;
    if let Some(problem) = after.problem {
        bail!("still failing after restoring the previous driver: {problem}");
    }
    Ok(reboot || after.needs_restart)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbox_driver_backup_points_at_windows_inf() {
        let devices = inventory::enumerate_devices(Default::default()).unwrap();
        let device = devices
            .iter()
            .find(|d| {
                d.driver
                    .as_ref()
                    .and_then(|dr| dr.inf_name.as_deref())
                    .is_some_and(|inf| !inf.to_ascii_lowercase().starts_with("oem"))
            })
            .expect("no device with an inbox driver");
        let backup = backup_driver(device, &std::env::temp_dir())
            .unwrap()
            .unwrap();
        assert!(backup.inf_path.exists());
        assert!(
            backup
                .inf_path
                .starts_with(std::env::var("SystemRoot").unwrap())
        );
    }
}
