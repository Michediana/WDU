//! The files of a device's installed driver, as Device Manager's "Driver File
//! Details" lists them: the class installer queues the installed driver's
//! files with `DI_NOVCP`, so nothing is copied, and the queue is scanned.
//! Reading needs no elevation.

use std::collections::HashMap;
use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DI_FLAGSEX_INSTALLEDDRIVER, DI_NOVCP, DIF_INSTALLDEVICEFILES, SP_DEVINFO_DATA,
    SP_DEVINSTALL_PARAMS_W, SP_DRVINFO_DATA_V2_W, SPDIT_COMPATDRIVER, SPFILENOTIFY_QUEUESCAN,
    SPQ_SCAN_USE_CALLBACK, SetupCloseFileQueue, SetupDiBuildDriverInfoList,
    SetupDiCallClassInstaller, SetupDiCreateDeviceInfoList, SetupDiDestroyDriverInfoList,
    SetupDiEnumDriverInfoW, SetupDiGetDeviceInstallParamsW, SetupDiOpenDeviceInfoW,
    SetupDiSetDeviceInstallParamsW, SetupDiSetSelectedDriverW, SetupOpenFileQueue,
    SetupScanFileQueueW,
};
use windows::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Security::Cryptography::Catalog::{
    CATALOG_INFO, CryptCATAdminAcquireContext2, CryptCATAdminCalcHashFromFileHandle2,
    CryptCATAdminEnumCatalogFromHash, CryptCATAdminReleaseCatalogContext,
    CryptCATAdminReleaseContext, CryptCATCatalogInfoFromContext,
};
use windows::Win32::Security::WinTrust::DRIVER_ACTION_VERIFY;
use windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};
use windows::core::{HSTRING, PCWSTR};

use crate::inventory::DevInfoSet;

/// One file of a driver, with what its version resource and signature say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverFile {
    pub path: PathBuf,
    pub company: Option<String>,
    pub version: Option<String>,
    pub copyright: Option<String>,
    /// Signer of the file's catalog, or of the file itself.
    pub signer: Option<String>,
}

/// The files of the driver installed on `instance_id`, sorted by path.
pub fn driver_files(instance_id: &str) -> Result<Vec<DriverFile>> {
    let paths = queued_files(instance_id)?;
    // Most files share one catalog: verify each catalog once.
    let mut signers: HashMap<PathBuf, Option<String>> = HashMap::new();
    Ok(paths
        .into_iter()
        .map(|path| {
            let [company, version, copyright] =
                version_strings(&path, ["CompanyName", "FileVersion", "LegalCopyright"]);
            let signer = match catalog_of(&path) {
                Some(cat) => signers
                    .entry(cat)
                    .or_insert_with_key(|cat| signer(cat))
                    .clone(),
                None => signer(&path),
            };
            DriverFile {
                path,
                company,
                version,
                copyright,
                signer,
            }
        })
        .collect())
}

fn queued_files(instance_id: &str) -> Result<Vec<PathBuf>> {
    let set = DevInfoSet(
        unsafe { SetupDiCreateDeviceInfoList(None, None) }
            .context("SetupDiCreateDeviceInfoList failed")?,
    );
    let mut data = SP_DEVINFO_DATA {
        cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
        ..Default::default()
    };
    unsafe { SetupDiOpenDeviceInfoW(set.0, &HSTRING::from(instance_id), None, 0, Some(&mut data)) }
        .with_context(|| format!("device {instance_id} not found"))?;

    // The driver list then holds only the installed driver.
    let mut params = install_params(&set, &data)?;
    params.FlagsEx |= DI_FLAGSEX_INSTALLEDDRIVER;
    set_install_params(&set, &data, &params)?;
    unsafe { SetupDiBuildDriverInfoList(set.0, Some(&mut data), SPDIT_COMPATDRIVER) }
        .context("SetupDiBuildDriverInfoList failed")?;
    let result = scan_installed_driver(&set, &mut data);
    unsafe {
        let _ = SetupDiDestroyDriverInfoList(set.0, Some(&data), SPDIT_COMPATDRIVER);
    }
    let mut files = result?;
    files.sort();
    files.dedup();
    Ok(files)
}

fn scan_installed_driver(set: &DevInfoSet, data: &mut SP_DEVINFO_DATA) -> Result<Vec<PathBuf>> {
    let mut driver = SP_DRVINFO_DATA_V2_W {
        cbSize: size_of::<SP_DRVINFO_DATA_V2_W>() as u32,
        ..Default::default()
    };
    unsafe { SetupDiEnumDriverInfoW(set.0, Some(data), SPDIT_COMPATDRIVER, 0, &mut driver) }
        .context("the device has no installed driver")?;
    unsafe { SetupDiSetSelectedDriverW(set.0, Some(data), Some(&mut driver)) }
        .context("SetupDiSetSelectedDriverW failed")?;

    let queue = unsafe { SetupOpenFileQueue() };
    if queue.is_null() || queue == INVALID_HANDLE_VALUE.0 {
        bail!("SetupOpenFileQueue failed");
    }
    let mut params = install_params(set, data)?;
    params.FileQueue = queue;
    params.Flags |= DI_NOVCP;
    let mut files: Vec<PathBuf> = Vec::new();
    let result = set_install_params(set, data, &params).and_then(|()| {
        unsafe { SetupDiCallClassInstaller(DIF_INSTALLDEVICEFILES, set.0, Some(data)) }
            .context("cannot list the driver's files")?;
        let mut scan = 0u32;
        unsafe {
            SetupScanFileQueueW(
                queue,
                SPQ_SCAN_USE_CALLBACK,
                None,
                Some(collect),
                Some((&raw mut files).cast()),
                &mut scan,
            )
        }
        .context("SetupScanFileQueueW failed")
    });
    // The set must let go of the queue before it is closed.
    if let Ok(mut params) = install_params(set, data) {
        params.FileQueue = std::ptr::null_mut();
        params.Flags &= !DI_NOVCP;
        let _ = set_install_params(set, data, &params);
    }
    unsafe {
        let _ = SetupCloseFileQueue(queue);
    }
    result.map(|()| files)
}

fn install_params(set: &DevInfoSet, data: &SP_DEVINFO_DATA) -> Result<SP_DEVINSTALL_PARAMS_W> {
    let mut params = SP_DEVINSTALL_PARAMS_W {
        cbSize: size_of::<SP_DEVINSTALL_PARAMS_W>() as u32,
        ..Default::default()
    };
    unsafe { SetupDiGetDeviceInstallParamsW(set.0, Some(data), &mut params) }
        .context("SetupDiGetDeviceInstallParamsW failed")?;
    Ok(params)
}

fn set_install_params(
    set: &DevInfoSet,
    data: &SP_DEVINFO_DATA,
    params: &SP_DEVINSTALL_PARAMS_W,
) -> Result<()> {
    unsafe { SetupDiSetDeviceInstallParamsW(set.0, Some(data), params) }
        .context("SetupDiSetDeviceInstallParamsW failed")
}

/// `SPFILENOTIFY_QUEUESCAN`: `param1` is the target path of a queued file.
unsafe extern "system" fn collect(
    context: *const c_void,
    notification: u32,
    param1: usize,
    _param2: usize,
) -> u32 {
    if notification == SPFILENOTIFY_QUEUESCAN && param1 != 0 {
        let files = unsafe { &mut *(context as *mut Vec<PathBuf>) };
        if let Ok(path) = unsafe { PCWSTR(param1 as *const u16).to_string() } {
            files.push(PathBuf::from(path));
        }
    }
    0 // NO_ERROR: go on scanning.
}

/// Strings of the file's version resource, in its first language.
fn version_strings<const N: usize>(path: &Path, names: [&str; N]) -> [Option<String>; N] {
    let none = [const { None }; N];
    let file = HSTRING::from(path.as_os_str());
    let size = unsafe { GetFileVersionInfoSizeW(&file, None) };
    if size == 0 {
        return none;
    }
    let mut block = vec![0u8; size as usize];
    if unsafe { GetFileVersionInfoW(&file, None, size, block.as_mut_ptr().cast()) }.is_err() {
        return none;
    }
    let query = |sub: &str| -> Option<&[u8]> {
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        unsafe {
            VerQueryValueW(
                block.as_ptr().cast(),
                &HSTRING::from(sub),
                &mut ptr,
                &mut len,
            )
        }
        .as_bool()
        .then_some(())?;
        (!ptr.is_null())
            .then(|| unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize * 2) })
    };
    // (language, code page) pairs; US English Unicode when there are none.
    let (lang, page) = query(r"\VarFileInfo\Translation")
        .filter(|t| t.len() >= 4)
        .map_or((0x0409, 0x04B0), |t| {
            (
                u16::from_le_bytes([t[0], t[1]]),
                u16::from_le_bytes([t[2], t[3]]),
            )
        });
    names.map(|name| {
        // `len` counts UTF-16 units for strings, NUL included.
        let bytes = query(&format!(r"\StringFileInfo\{lang:04x}{page:04x}\{name}"))?;
        let wide: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .take_while(|&c| c != 0)
            .collect();
        let text = String::from_utf16_lossy(&wide).trim().to_string();
        (!text.is_empty()).then_some(text)
    })
}

/// The system catalog that holds the file's hash, if any.
fn catalog_of(path: &Path) -> Option<PathBuf> {
    let file = std::fs::File::open(path).ok()?;
    let handle = HANDLE(file.as_raw_handle());
    // Newer catalogs hash with SHA-256, older ones with SHA-1.
    ["SHA256", "SHA1"].into_iter().find_map(|algorithm| unsafe {
        let mut admin = 0isize;
        CryptCATAdminAcquireContext2(
            &mut admin,
            Some(&DRIVER_ACTION_VERIFY),
            &HSTRING::from(algorithm),
            None,
            None,
        )
        .ok()?;
        let catalog = (|| {
            let mut size = 0u32;
            CryptCATAdminCalcHashFromFileHandle2(admin, handle, &mut size, None, None).ok()?;
            let mut hash = vec![0u8; size as usize];
            CryptCATAdminCalcHashFromFileHandle2(
                admin,
                handle,
                &mut size,
                Some(hash.as_mut_ptr()),
                None,
            )
            .ok()?;
            let info = CryptCATAdminEnumCatalogFromHash(admin, &hash, None, None);
            if info == 0 {
                return None;
            }
            let mut catalog = CATALOG_INFO {
                cbStruct: size_of::<CATALOG_INFO>() as u32,
                ..Default::default()
            };
            let found = CryptCATCatalogInfoFromContext(info, &mut catalog, 0).ok();
            let _ = CryptCATAdminReleaseCatalogContext(admin, info, 0);
            found?;
            let end = catalog
                .wszCatalogFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(catalog.wszCatalogFile.len());
            Some(PathBuf::from(String::from_utf16_lossy(
                &catalog.wszCatalogFile[..end],
            )))
        })();
        let _ = CryptCATAdminReleaseContext(admin, 0);
        catalog
    })
}

fn signer(path: &Path) -> Option<String> {
    crate::trust::verify_file(path).ok()?.signer
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::{InventoryOptions, enumerate_devices};

    #[test]
    fn lists_files_of_installed_drivers() {
        let devices = enumerate_devices(InventoryOptions::default()).unwrap();
        // Volumes and disks always have a driver with a version resource.
        let device = devices
            .iter()
            .find(|d| d.class.as_deref() == Some("DiskDrive") && d.driver.is_some())
            .expect("a disk drive");
        let files = driver_files(&device.instance_id).unwrap();
        assert!(!files.is_empty());
        assert!(files.iter().any(|f| f.version.is_some()));
    }
}
