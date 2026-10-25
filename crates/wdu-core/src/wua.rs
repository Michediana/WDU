//! Windows Update Agent (WUA) via COM: driver search, download and install,
//! and the policies that keep drivers out of Windows Update.
//!
//! WUA calls are slow and blocking, so every operation runs on a dedicated
//! thread with its own COM apartment; searches are asynchronous on the WUA
//! side and aborted when they exceed their timeout.

use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, VARIANT_BOOL, VARIANT_FALSE};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::UpdateAgent::{
    ISearchCompletedCallback, ISearchCompletedCallback_Impl, ISearchCompletedCallbackArgs,
    ISearchJob, IUpdate, IUpdateCollection, IUpdateSession, IWindowsDriverUpdate,
    OperationResultCode, UpdateCollection, UpdateSession, orcAborted, orcFailed, orcInProgress,
    orcNotStarted, orcSucceeded, orcSucceededWithErrors,
};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{BSTR, Interface, Ref, implement};

use crate::model::DriverVersion;
use crate::provider::{Candidate, EntryKind, Provider, Source};
use crate::registry::{read_dword, read_string};

/// Applicable driver updates that are not installed yet.
pub const DEFAULT_DRIVER_CRITERIA: &str = "IsInstalled=0 and Type='Driver'";

/// A cold search can take several minutes; past this it is aborted.
pub const DEFAULT_SEARCH_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WuDriverUpdate {
    pub update_id: String,
    pub revision: i32,
    pub title: String,
    pub hardware_id: Option<String>,
    pub driver_class: Option<String>,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    /// `YYYY-MM-DD` from the driver's `DriverVer`.
    pub driver_date: Option<String>,
    /// WUA does not expose the `DriverVer` version; driver titles end with it
    /// (`Intel - Display - 31.0.101.5590`).
    pub driver_version: Option<String>,
}

impl WuDriverUpdate {
    pub fn to_candidate(&self) -> Candidate {
        Candidate {
            source: Source::WindowsUpdate,
            id: self.update_id.clone(),
            title: self.title.clone(),
            kind: EntryKind::Driver,
            hardware_ids: self
                .hardware_id
                .iter()
                .map(|id| id.to_ascii_uppercase())
                .collect(),
            class: self.driver_class.clone(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            driver_version: self.driver_version.clone(),
            driver_date: self.driver_date.clone(),
            release_date: None,
            package: None,
        }
    }
}

fn version_from_title(title: &str) -> Option<String> {
    let last = title.rsplit(" - ").next()?.trim();
    last.parse::<DriverVersion>().ok().map(|_| last.to_string())
}

/// Keeps COM initialized on this thread for the guard's lifetime.
struct ComGuard {
    owned: bool,
}

impl ComGuard {
    fn new() -> Result<Self> {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr == RPC_E_CHANGED_MODE {
            // Already initialized as STA by someone else; WUA works there too.
            return Ok(ComGuard { owned: false });
        }
        hr.ok().context("CoInitializeEx failed")?;
        Ok(ComGuard { owned: true })
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.owned {
            unsafe { CoUninitialize() };
        }
    }
}

fn yes(b: VARIANT_BOOL) -> bool {
    b != VARIANT_FALSE
}

/// Runs `f` on a new thread with COM initialized, and waits for it.
fn on_wua_thread<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    thread::Builder::new()
        .name("wua".into())
        .spawn(move || {
            let _com = ComGuard::new()?;
            f()
        })
        .context("cannot start the Windows Update thread")?
        .join()
        .map_err(|_| anyhow!("Windows Update thread panicked"))?
}

fn new_session() -> Result<IUpdateSession> {
    let session: IUpdateSession =
        unsafe { CoCreateInstance(&UpdateSession, None, CLSCTX_INPROC_SERVER) }
            .context("cannot create Microsoft.Update.Session")?;
    unsafe { session.SetClientApplicationID(&BSTR::from("WDU"))? };
    Ok(session)
}

/// `BeginSearch` refuses a null callback; completion is polled instead.
#[implement(ISearchCompletedCallback)]
struct IgnoreCompletion;

impl ISearchCompletedCallback_Impl for IgnoreCompletion_Impl {
    fn Invoke(
        &self,
        _job: Ref<ISearchJob>,
        _args: Ref<ISearchCompletedCallbackArgs>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

/// Asynchronous search, polled so it can be aborted at `timeout`.
fn search(
    session: &IUpdateSession,
    criteria: &str,
    timeout: Duration,
) -> Result<IUpdateCollection> {
    unsafe {
        let searcher = session.CreateUpdateSearcher()?;
        tracing::info!(criteria, "searching Windows Update");
        let callback: ISearchCompletedCallback = IgnoreCompletion.into();
        let job = searcher
            .BeginSearch(&BSTR::from(criteria), &callback, &VARIANT::default())
            .context("IUpdateSearcher::BeginSearch failed")?;
        let deadline = Instant::now() + timeout;
        while !yes(job.IsCompleted()?) {
            if Instant::now() >= deadline {
                let _ = job.RequestAbort();
                let _ = job.CleanUp();
                bail!("Windows Update search aborted after {}s", timeout.as_secs());
            }
            thread::sleep(Duration::from_millis(250));
        }
        let result = searcher.EndSearch(&job);
        let _ = job.CleanUp();
        let result = result.context("IUpdateSearcher::EndSearch failed")?;
        let code = result.ResultCode()?;
        tracing::debug!(code = code.0, "search finished");
        if code != orcSucceeded && code != orcSucceededWithErrors {
            bail!("Windows Update search ended as {:?}", OpResult::from(code));
        }
        Ok(result.Updates()?)
    }
}

/// Searches Windows Update on a dedicated thread, aborting after `timeout`.
pub fn search_driver_updates(criteria: &str, timeout: Duration) -> Result<Vec<WuDriverUpdate>> {
    let criteria = criteria.to_string();
    on_wua_thread(move || {
        let session = new_session()?;
        let updates = search(&session, &criteria, timeout)?;
        unsafe {
            (0..updates.Count()?)
                .map(|i| map_update(&updates.get_Item(i)?))
                .collect()
        }
    })
}

unsafe fn map_update(update: &IUpdate) -> Result<WuDriverUpdate> {
    let opt =
        |s: windows::core::Result<BSTR>| s.ok().map(|b| b.to_string()).filter(|s| !s.is_empty());
    unsafe {
        let identity = update.Identity()?;
        let title = update.Title()?.to_string();
        let mut item = WuDriverUpdate {
            update_id: identity.UpdateID()?.to_string(),
            revision: identity.RevisionNumber()?,
            driver_version: version_from_title(&title),
            title,
            hardware_id: None,
            driver_class: None,
            manufacturer: None,
            model: None,
            provider: None,
            driver_date: None,
        };
        // Only driver updates implement IWindowsDriverUpdate.
        if let Ok(driver) = update.cast::<IWindowsDriverUpdate>() {
            item.hardware_id = opt(driver.DriverHardwareID());
            item.driver_class = opt(driver.DriverClass());
            item.manufacturer = opt(driver.DriverManufacturer());
            item.model = opt(driver.DriverModel());
            item.provider = opt(driver.DriverProvider());
            item.driver_date = driver
                .DriverVerDate()
                .ok()
                .and_then(crate::time::ole_date_to_date);
        }
        Ok(item)
    }
}

/// WUA `OperationResultCode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpResult {
    NotStarted,
    InProgress,
    Succeeded,
    SucceededWithErrors,
    Failed,
    Aborted,
    Unknown,
}

impl From<OperationResultCode> for OpResult {
    fn from(code: OperationResultCode) -> Self {
        match code {
            c if c == orcNotStarted => OpResult::NotStarted,
            c if c == orcInProgress => OpResult::InProgress,
            c if c == orcSucceeded => OpResult::Succeeded,
            c if c == orcSucceededWithErrors => OpResult::SucceededWithErrors,
            c if c == orcFailed => OpResult::Failed,
            c if c == orcAborted => OpResult::Aborted,
            _ => OpResult::Unknown,
        }
    }
}

impl OpResult {
    pub fn ok(self) -> bool {
        matches!(self, OpResult::Succeeded | OpResult::SucceededWithErrors)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WuInstallResult {
    pub update_id: String,
    pub title: Option<String>,
    pub download: Option<OpResult>,
    pub install: Option<OpResult>,
    /// Why the update was not installed: a WUA HRESULT or a plain message.
    pub error: Option<String>,
    /// The update needs a restart to complete; the restart is never forced.
    pub reboot_required: bool,
}

impl WuInstallResult {
    fn new(update_id: String, title: Option<String>) -> Self {
        WuInstallResult {
            update_id,
            title,
            download: None,
            install: None,
            error: None,
            reboot_required: false,
        }
    }

    pub fn succeeded(&self) -> bool {
        self.install.is_some_and(OpResult::ok)
    }
}

fn hresult(hr: i32) -> String {
    let hr = windows::core::HRESULT(hr);
    format!("0x{:08X} {}", hr.0 as u32, hr.message())
}

/// Downloads and installs the given updates (needs administrator rights).
///
/// Updates are looked up again by ID, so a list from an earlier search can be
/// passed. IDs Windows Update no longer offers come back with an error.
pub fn install_driver_updates(
    update_ids: &[String],
    search_timeout: Duration,
) -> Result<Vec<WuInstallResult>> {
    // IDs end up in a search criterion: allow GUID characters only.
    if let Some(bad) = update_ids
        .iter()
        .find(|id| id.is_empty() || !id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'))
    {
        bail!("invalid Windows Update ID `{bad}`");
    }
    let ids = update_ids.to_vec();
    on_wua_thread(move || unsafe {
        let session = new_session()?;
        let criteria = ids
            .iter()
            .map(|id| format!("UpdateID='{id}'"))
            .collect::<Vec<_>>()
            .join(" or ");
        let found = search(&session, &criteria, search_timeout)?;

        let new_collection = || -> Result<IUpdateCollection> {
            Ok(CoCreateInstance(
                &UpdateCollection,
                None,
                CLSCTX_INPROC_SERVER,
            )?)
        };
        let to_download = new_collection()?;
        let mut results = Vec::new();
        for i in 0..found.Count()? {
            let update = found.get_Item(i)?;
            if !yes(update.EulaAccepted()?) {
                update.AcceptEula()?;
            }
            to_download.Add(&update)?;
            results.push(WuInstallResult::new(
                update.Identity()?.UpdateID()?.to_string(),
                Some(update.Title()?.to_string()),
            ));
        }
        for id in &ids {
            if !results.iter().any(|r| r.update_id.eq_ignore_ascii_case(id)) {
                let mut missing = WuInstallResult::new(id.clone(), None);
                missing.error = Some("not offered by Windows Update".into());
                results.push(missing);
            }
        }
        if to_download.Count()? == 0 {
            return Ok(results);
        }

        let downloader = session.CreateUpdateDownloader()?;
        downloader.SetUpdates(&to_download)?;
        tracing::info!(
            count = to_download.Count()?,
            "downloading from Windows Update"
        );
        let downloaded = downloader
            .Download()
            .context("IUpdateDownloader::Download failed")?;

        let to_install = new_collection()?;
        let mut installing = Vec::new();
        for i in 0..to_download.Count()? {
            let r = downloaded.GetUpdateResult(i)?;
            let outcome = OpResult::from(r.ResultCode()?);
            let result = &mut results[i as usize];
            result.download = Some(outcome);
            if outcome.ok() {
                to_install.Add(&to_download.get_Item(i)?)?;
                installing.push(i as usize);
            } else {
                result.error = Some(format!("download failed: {}", hresult(r.HResult()?)));
            }
        }
        if installing.is_empty() {
            return Ok(results);
        }

        let installer = session.CreateUpdateInstaller()?;
        if yes(installer.IsBusy()?) {
            bail!("another Windows Update installation is in progress");
        }
        installer.SetAllowSourcePrompts(VARIANT_FALSE)?;
        installer.SetUpdates(&to_install)?;
        tracing::info!(count = installing.len(), "installing from Windows Update");
        let installed = installer
            .Install()
            .context("IUpdateInstaller::Install failed (administrator rights are required)")?;
        for (k, &index) in installing.iter().enumerate() {
            let r = installed.GetUpdateResult(k as i32)?;
            let result = &mut results[index];
            let outcome = OpResult::from(r.ResultCode()?);
            result.install = Some(outcome);
            result.reboot_required = yes(r.RebootRequired()?);
            if !outcome.ok() {
                result.error = Some(format!("install failed: {}", hresult(r.HResult()?)));
            }
        }
        Ok(results)
    })
}

const WU_POLICY: &str = r"SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate";
const AU_POLICY: &str = r"SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU";
const MDM_UPDATE: &str = r"SOFTWARE\Microsoft\PolicyManager\current\device\Update";
const DRIVER_SEARCHING: &str = r"SOFTWARE\Policies\Microsoft\Windows\DriverSearching";
const DEVICE_INSTALL: &str = r"SOFTWARE\Policies\Microsoft\Windows\DeviceInstall\Restrictions";

/// Policies (Group Policy or MDM) that limit driver updates. They are reported
/// and honoured, never bypassed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UpdatePolicy {
    /// Drivers are excluded from Windows Update quality updates.
    pub drivers_excluded: bool,
    /// WSUS server the client scans; it only offers drivers an admin approved.
    pub wsus_server: Option<String>,
    /// Device installation restrictions are set: any source may be refused.
    pub device_install_restricted: bool,
    /// One line per policy found.
    pub findings: Vec<String>,
}

impl UpdatePolicy {
    /// Builds the policy from registry readers, so it can be tested without a registry.
    fn from_registry(
        dword: impl Fn(&str, &str) -> Option<u32>,
        string: impl Fn(&str, &str) -> Option<String>,
    ) -> Self {
        let on = |key: &str, value: &str| dword(key, value) == Some(1);
        let mut p = UpdatePolicy::default();

        if on(WU_POLICY, "ExcludeWUDriversInQualityUpdate") {
            p.drivers_excluded = true;
            p.findings
                .push("Group Policy excludes drivers from Windows Update".into());
        }
        if on(MDM_UPDATE, "ExcludeWUDriversInQualityUpdate") {
            p.drivers_excluded = true;
            p.findings
                .push("MDM policy excludes drivers from Windows Update".into());
        }
        if on(AU_POLICY, "UseWUServer") {
            p.wsus_server = string(WU_POLICY, "WUServer").filter(|s| !s.trim().is_empty());
            p.findings.push(format!(
                "scans go to WSUS ({}): only approved drivers are offered",
                p.wsus_server.as_deref().unwrap_or("server not set")
            ));
        }
        if on(WU_POLICY, "SetPolicyDrivenUpdateSourceForDriverUpdates") {
            p.findings
                .push("driver updates come from WSUS by policy".into());
        }
        if on(WU_POLICY, "DoNotConnectToWindowsUpdateInternetLocations") {
            p.findings
                .push("connections to Windows Update internet locations are blocked".into());
        }
        if dword(DRIVER_SEARCHING, "SearchOrderConfig") == Some(0)
            || on(DRIVER_SEARCHING, "DontSearchWindowsUpdate")
        {
            p.findings.push(
                "Plug and Play does not search Windows Update for drivers (WUA scans still work)"
                    .into(),
            );
        }
        let restrictions = [
            "DenyUnspecified",
            "DenyDeviceIDs",
            "DenyDeviceClasses",
            "DenyInstanceIDs",
            "AllowDeviceIDs",
            "AllowDeviceClasses",
            "AllowInstanceIDs",
        ];
        let set: Vec<&str> = restrictions
            .into_iter()
            .filter(|v| on(DEVICE_INSTALL, v))
            .collect();
        if !set.is_empty() {
            p.device_install_restricted = true;
            p.findings.push(format!(
                "device installation restrictions are set ({}): installs may be refused",
                set.join(", ")
            ));
        }
        p
    }
}

pub fn read_update_policy() -> UpdatePolicy {
    UpdatePolicy::from_registry(read_dword, read_string)
}

/// [`Provider`] over Windows Update. Honours [`UpdatePolicy::drivers_excluded`].
pub struct WindowsUpdateProvider {
    pub criteria: String,
    pub timeout: Duration,
}

impl Default for WindowsUpdateProvider {
    fn default() -> Self {
        WindowsUpdateProvider {
            criteria: DEFAULT_DRIVER_CRITERIA.into(),
            timeout: DEFAULT_SEARCH_TIMEOUT,
        }
    }
}

impl Provider for WindowsUpdateProvider {
    fn source(&self) -> Source {
        Source::WindowsUpdate
    }

    fn candidates(&self) -> Result<Vec<Candidate>> {
        let policy = read_update_policy();
        if policy.drivers_excluded {
            bail!(
                "drivers are excluded from Windows Update by policy ({})",
                policy.findings.join("; ")
            );
        }
        Ok(search_driver_updates(&self.criteria, self.timeout)?
            .iter()
            .map(WuDriverUpdate::to_candidate)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn version_from_driver_titles() {
        assert_eq!(
            version_from_title("Intel - Display - 31.0.101.5590").as_deref(),
            Some("31.0.101.5590")
        );
        assert_eq!(
            version_from_title("Intel - System - 10/3/2016 12:00:00 AM - 10.1.1.38").as_deref(),
            Some("10.1.1.38")
        );
        assert_eq!(version_from_title("Some Driver Update"), None);
    }

    #[test]
    fn policy_from_registry() {
        let dwords: HashMap<(&str, &str), u32> = [
            ((WU_POLICY, "ExcludeWUDriversInQualityUpdate"), 1),
            ((AU_POLICY, "UseWUServer"), 1),
            ((DEVICE_INSTALL, "DenyDeviceClasses"), 1),
            ((DRIVER_SEARCHING, "SearchOrderConfig"), 1),
        ]
        .into();
        let p = UpdatePolicy::from_registry(
            |k, v| dwords.get(&(k, v)).copied(),
            |k, v| (k == WU_POLICY && v == "WUServer").then(|| "https://wsus:8531".into()),
        );
        assert!(p.drivers_excluded);
        assert!(p.device_install_restricted);
        assert_eq!(p.wsus_server.as_deref(), Some("https://wsus:8531"));
        assert_eq!(p.findings.len(), 3);

        let none = UpdatePolicy::from_registry(|_, _| None, |_, _| None);
        assert_eq!(none, UpdatePolicy::default());
    }

    #[test]
    fn rejects_malformed_update_ids() {
        let err = install_driver_updates(&["x' or 1=1".into()], Duration::from_secs(1));
        assert!(err.is_err());
    }
}
