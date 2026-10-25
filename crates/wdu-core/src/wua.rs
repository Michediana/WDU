//! Windows Update Agent (WUA) driver search via COM.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::UpdateAgent::{
    IUpdate, IUpdateSession, IWindowsDriverUpdate, UpdateSession,
};
use windows::core::{BSTR, Interface};

/// Applicable driver updates that are not installed yet.
pub const DEFAULT_DRIVER_CRITERIA: &str = "IsInstalled=0 and Type='Driver'";

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

/// Runs a WUA search. Blocking, and it can take minutes on a cold cache.
pub fn search_driver_updates(criteria: &str) -> Result<Vec<WuDriverUpdate>> {
    let _com = ComGuard::new()?;

    let session: IUpdateSession =
        unsafe { CoCreateInstance(&UpdateSession, None, CLSCTX_INPROC_SERVER) }
            .context("cannot create Microsoft.Update.Session")?;
    unsafe {
        session.SetClientApplicationID(&BSTR::from("WDU"))?;
        let searcher = session.CreateUpdateSearcher()?;
        tracing::info!(criteria, "searching Windows Update");
        let result = searcher
            .Search(&BSTR::from(criteria))
            .context("IUpdateSearcher::Search failed")?;
        tracing::debug!(code = result.ResultCode()?.0, "search finished");

        let updates = result.Updates()?;
        let count = updates.Count()?;
        let mut out = Vec::with_capacity(count.max(0) as usize);
        for i in 0..count {
            let update = updates.get_Item(i)?;
            out.push(map_update(&update)?);
        }
        Ok(out)
    }
}

unsafe fn map_update(update: &IUpdate) -> Result<WuDriverUpdate> {
    let opt =
        |s: windows::core::Result<BSTR>| s.ok().map(|b| b.to_string()).filter(|s| !s.is_empty());
    unsafe {
        let identity = update.Identity()?;
        let mut item = WuDriverUpdate {
            update_id: identity.UpdateID()?.to_string(),
            revision: identity.RevisionNumber()?,
            title: update.Title()?.to_string(),
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
