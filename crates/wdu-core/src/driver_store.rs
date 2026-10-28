//! Third-party driver packages in the driver store (`oemNN.inf`), with the
//! devices that use them and the versions superseded by a newer package.
//!
//! Inbox drivers are not listed: they ship with Windows and cannot be removed.

use std::cmp::Ordering;
use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::model::{Device, DriverVersion};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverPackage {
    /// Published name, e.g. `oem42.inf`.
    pub published_name: String,
    /// Name of the INF as shipped, e.g. `iigd_dch.inf`.
    pub original_name: Option<String>,
    /// Folder in `System32\DriverStore\FileRepository`.
    pub store_path: Option<String>,
    pub provider: Option<String>,
    pub class: Option<String>,
    pub class_guid: Option<String>,
    pub version: Option<String>,
    /// `YYYY-MM-DD`, from `DriverVer`.
    pub date: Option<String>,
    /// Bytes in the store folder.
    pub size: u64,
    /// Devices whose current driver is this package, connected or not.
    pub devices: Vec<PackageDevice>,
    /// Published name of a newer package with the same original INF and provider.
    pub superseded_by: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageDevice {
    pub instance_id: String,
    pub name: Option<String>,
    pub present: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageUse {
    /// At least one connected device uses it.
    InUse,
    /// Only devices that are not connected now use it.
    AbsentDevices,
    /// No device uses it. Not the same as unused: extensions, network
    /// filters, printers and services never show up as a device's driver.
    NoDevice,
}

impl DriverPackage {
    pub fn usage(&self) -> PackageUse {
        if self.devices.iter().any(|d| d.present) {
            PackageUse::InUse
        } else if self.devices.is_empty() {
            PackageUse::NoDevice
        } else {
            PackageUse::AbsentDevices
        }
    }

    /// An older version that no connected device uses: a candidate for cleanup.
    pub fn is_old_unused(&self) -> bool {
        self.superseded_by.is_some() && self.usage() != PackageUse::InUse
    }

    fn rank(&self) -> (Option<DriverVersion>, Option<&str>) {
        (
            self.version.as_deref().and_then(|v| v.parse().ok()),
            self.date.as_deref(),
        )
    }
}

/// Fills [`DriverPackage::devices`] from the inventory, matching the
/// published INF name each device reports.
pub fn attach_devices(packages: &mut [DriverPackage], devices: &[Device]) {
    let mut by_inf: HashMap<String, Vec<PackageDevice>> = HashMap::new();
    for d in devices {
        if let Some(inf) = d.driver.as_ref().and_then(|v| v.inf_name.as_deref()) {
            by_inf
                .entry(inf.to_lowercase())
                .or_default()
                .push(PackageDevice {
                    instance_id: d.instance_id.clone(),
                    name: d.name.clone(),
                    present: d.status.present,
                });
        }
    }
    for p in packages {
        p.devices = by_inf
            .remove(&p.published_name.to_lowercase())
            .unwrap_or_default();
    }
}

/// Marks every package that has a newer one with the same original INF name
/// and provider. Packages without a comparable version are left alone.
pub fn mark_superseded(packages: &mut [DriverPackage]) {
    let key = |p: &DriverPackage| {
        Some((
            p.original_name.as_deref()?.to_lowercase(),
            p.provider.as_deref().unwrap_or("").to_lowercase(),
        ))
    };
    let mut newest: HashMap<(String, String), usize> = HashMap::new();
    for (i, p) in packages.iter().enumerate() {
        let (Some(k), (Some(_), _)) = (key(p), p.rank()) else {
            continue;
        };
        newest
            .entry(k)
            .and_modify(|best| {
                if p.rank().cmp(&packages[*best].rank()) == Ordering::Greater {
                    *best = i;
                }
            })
            .or_insert(i);
    }
    let superseded: Vec<Option<String>> = packages
        .iter()
        .map(|p| {
            let best = &packages[*newest.get(&key(p)?)?];
            (p.rank().0.is_some() && p.rank() < best.rank()).then(|| best.published_name.clone())
        })
        .collect();
    for (p, by) in packages.iter_mut().zip(superseded) {
        p.superseded_by = by;
    }
}

/// `mm/dd/yyyy` from `DriverVer` to `YYYY-MM-DD`.
fn inf_date(date: &str) -> Option<String> {
    let mut parts = date.trim().split('/').map(|p| p.trim().parse::<u32>().ok());
    let (m, d, y) = (parts.next()??, parts.next()??, parts.next()??);
    ((1..=12).contains(&m) && (1..=31).contains(&d)).then(|| format!("{y:04}-{m:02}-{d:02}"))
}

/// Whether `name` is a published third-party INF name (`oemNN.inf`).
pub fn is_oem_inf(name: &str) -> bool {
    let name = name.to_lowercase();
    name.strip_prefix("oem")
        .and_then(|rest| rest.strip_suffix(".inf"))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(windows)]
pub use store::{delete_package, enumerate_packages};

#[cfg(windows)]
mod store {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use anyhow::{Context, Result, bail};
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        INF_STYLE_WIN4, INFCONTEXT, SetupCloseInfFile, SetupFindFirstLineW,
        SetupGetInfDriverStoreLocationW, SetupGetStringFieldW, SetupOpenInfFileW,
    };
    use windows::core::{HSTRING, PCWSTR};

    use super::*;
    use crate::install::system32;
    use crate::inventory::{self, InventoryOptions};

    /// Every `oemNN.inf` package, with its devices and superseded versions.
    pub fn enumerate_packages() -> Result<Vec<DriverPackage>> {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        let inf_dir = Path::new(&root).join("INF");
        let mut packages = Vec::new();
        for entry in
            fs::read_dir(&inf_dir).with_context(|| format!("cannot list {}", inf_dir.display()))?
        {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_oem_inf(&name) {
                packages.push(read_package(&entry.path(), name));
            }
        }
        let devices = inventory::enumerate_devices(InventoryOptions {
            include_absent: true,
        })?;
        attach_devices(&mut packages, &devices);
        mark_superseded(&mut packages);
        Ok(packages)
    }

    /// Removes a package from the driver store, like `pnputil /delete-driver`.
    /// Without `/force`, Windows refuses to remove a package a device uses.
    pub fn delete_package(published_name: &str) -> Result<()> {
        if !is_oem_inf(published_name) {
            bail!("`{published_name}` is not a third-party driver package");
        }
        let out = Command::new(system32("pnputil.exe"))
            .arg("/delete-driver")
            .arg(published_name)
            .output()
            .context("cannot run pnputil")?;
        if !out.status.success() {
            bail!(
                "pnputil /delete-driver {published_name} failed: {}",
                String::from_utf8_lossy(&out.stdout).trim()
            );
        }
        tracing::info!(published_name, "driver package removed");
        Ok(())
    }

    fn read_package(inf: &Path, published_name: String) -> DriverPackage {
        let store_inf = store_location(&published_name);
        let store_dir = store_inf.as_deref().and_then(Path::parent);
        let mut package = DriverPackage {
            original_name: store_inf
                .as_deref()
                .and_then(Path::file_name)
                .map(|n| n.to_string_lossy().into_owned()),
            store_path: store_dir.map(|d| d.display().to_string()),
            size: store_dir.map_or(0, dir_size),
            published_name,
            provider: None,
            class: None,
            class_guid: None,
            version: None,
            date: None,
            devices: Vec::new(),
            superseded_by: None,
        };
        match Inf::open(inf) {
            Some(inf) => {
                package.provider = inf.version_field("Provider", 1);
                package.class = inf.version_field("Class", 1);
                package.class_guid = inf.version_field("ClassGuid", 1);
                package.date = inf.version_field("DriverVer", 1).and_then(|d| inf_date(&d));
                package.version = inf.version_field("DriverVer", 2);
            }
            None => tracing::warn!(inf = %inf.display(), "cannot open INF"),
        }
        package
    }

    /// Path of the INF inside the driver store, which keeps its original name.
    fn store_location(published_name: &str) -> Option<PathBuf> {
        let mut buf = vec![0u16; 1024];
        unsafe {
            SetupGetInfDriverStoreLocationW(
                &HSTRING::from(published_name),
                None,
                PCWSTR::null(),
                &mut buf,
                None,
            )
        }
        .ok()?;
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(PathBuf::from(String::from_utf16_lossy(&buf[..end])))
    }

    fn dir_size(dir: &Path) -> u64 {
        let Ok(entries) = fs::read_dir(dir) else {
            return 0;
        };
        entries
            .flatten()
            .map(|e| match e.file_type() {
                Ok(t) if t.is_dir() => dir_size(&e.path()),
                Ok(_) => e.metadata().map_or(0, |m| m.len()),
                Err(_) => 0,
            })
            .sum()
    }

    struct Inf(*mut core::ffi::c_void);

    impl Inf {
        fn open(path: &Path) -> Option<Self> {
            let handle = unsafe {
                SetupOpenInfFileW(
                    &HSTRING::from(path.as_os_str()),
                    PCWSTR::null(),
                    INF_STYLE_WIN4,
                    None,
                )
            };
            // INVALID_HANDLE_VALUE
            (handle as isize != -1).then_some(Inf(handle))
        }

        /// One field of a `[Version]` line, with `%strkey%` tokens substituted.
        fn version_field(&self, key: &str, index: u32) -> Option<String> {
            let mut ctx = INFCONTEXT::default();
            unsafe {
                SetupFindFirstLineW(
                    self.0,
                    &HSTRING::from("Version"),
                    &HSTRING::from(key),
                    &mut ctx,
                )
            }
            .ok()?;
            let mut buf = vec![0u16; 512];
            unsafe { SetupGetStringFieldW(&ctx, index, Some(&mut buf), None) }.ok()?;
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            let value = String::from_utf16_lossy(&buf[..end]).trim().to_string();
            (!value.is_empty()).then_some(value)
        }
    }

    impl Drop for Inf {
        fn drop(&mut self) {
            unsafe { SetupCloseInfFile(self.0) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(published: &str, original: &str, version: &str, date: &str) -> DriverPackage {
        DriverPackage {
            published_name: published.into(),
            original_name: Some(original.into()),
            store_path: None,
            provider: Some("Intel".into()),
            class: Some("Display".into()),
            class_guid: None,
            version: Some(version.into()),
            date: Some(date.into()),
            size: 0,
            devices: Vec::new(),
            superseded_by: None,
        }
    }

    #[test]
    fn older_versions_are_superseded() {
        let mut packages = vec![
            package("oem3.inf", "iigd_dch.inf", "31.0.101.4502", "2023-06-01"),
            package("oem7.inf", "IIGD_DCH.INF", "31.0.101.5590", "2024-06-01"),
            package("oem9.inf", "iigd_dch.inf", "31.0.101.5590", "2024-06-01"),
            package("oem4.inf", "netwtw.inf", "22.0.0.1", "2023-01-01"),
        ];
        packages[3].provider = Some("Other".into());
        mark_superseded(&mut packages);
        assert_eq!(packages[0].superseded_by.as_deref(), Some("oem7.inf"));
        // Same version twice: neither is older.
        assert_eq!(packages[1].superseded_by, None);
        assert_eq!(packages[2].superseded_by, None);
        assert_eq!(packages[3].superseded_by, None);
    }

    #[test]
    fn same_version_newer_date_wins() {
        let mut packages = vec![
            package("oem1.inf", "a.inf", "1.0.0.0", "2020-01-01"),
            package("oem2.inf", "a.inf", "1.0.0.0", "2021-01-01"),
        ];
        mark_superseded(&mut packages);
        assert_eq!(packages[0].superseded_by.as_deref(), Some("oem2.inf"));
        assert_eq!(packages[1].superseded_by, None);
    }

    #[test]
    fn usage_from_devices() {
        let mut p = package("oem1.inf", "a.inf", "1.0", "2020-01-01");
        assert_eq!(p.usage(), PackageUse::NoDevice);
        p.devices.push(PackageDevice {
            instance_id: "USB\\X".into(),
            name: None,
            present: false,
        });
        assert_eq!(p.usage(), PackageUse::AbsentDevices);
        p.superseded_by = Some("oem2.inf".into());
        assert!(p.is_old_unused());
        p.devices[0].present = true;
        assert_eq!(p.usage(), PackageUse::InUse);
        assert!(!p.is_old_unused());
    }

    #[test]
    fn names_and_dates() {
        assert!(is_oem_inf("oem42.inf"));
        assert!(is_oem_inf("OEM1.INF"));
        assert!(!is_oem_inf("oem.inf"));
        assert!(!is_oem_inf("machine.inf"));
        assert_eq!(inf_date("06/21/2006").as_deref(), Some("2006-06-21"));
        assert_eq!(inf_date(" 1/2/2024 ").as_deref(), Some("2024-01-02"));
        assert_eq!(inf_date("2024-01-02"), None);
    }
}
