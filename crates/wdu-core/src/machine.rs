//! Machine identity from WMI/SMBIOS.

#![allow(non_camel_case_types, non_snake_case)]

use anyhow::{Context, Result};
use serde::Deserialize;
use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
use windows::core::w;
use wmi::WMIConnection;

use crate::model::{MachineInfo, OsInfo};

#[derive(Deserialize)]
struct Win32_ComputerSystem {
    Manufacturer: Option<String>,
    Model: Option<String>,
    SystemSKUNumber: Option<String>,
    SystemFamily: Option<String>,
}

#[derive(Deserialize)]
struct Win32_ComputerSystemProduct {
    Version: Option<String>,
    UUID: Option<String>,
}

#[derive(Deserialize)]
struct Win32_BaseBoard {
    Manufacturer: Option<String>,
    Product: Option<String>,
}

#[derive(Deserialize)]
struct Win32_BIOS {
    SMBIOSBIOSVersion: Option<String>,
    SerialNumber: Option<String>,
}

#[derive(Deserialize)]
struct Win32_OperatingSystem {
    Caption: Option<String>,
    Version: Option<String>,
    BuildNumber: Option<String>,
    OSArchitecture: Option<String>,
}

/// Placeholder strings that firmware vendors leave in unused SMBIOS fields.
fn clean(value: Option<String>) -> Option<String> {
    let v = value?.trim().to_string();
    let lower = v.to_ascii_lowercase();
    let placeholder = v.is_empty()
        || lower.contains("to be filled")
        || lower == "default string"
        || lower == "system product name"
        || lower == "none"
        || lower == "n/a";
    (!placeholder).then_some(v)
}

fn first<T: for<'de> Deserialize<'de>>(wmi: &WMIConnection) -> Result<Option<T>> {
    let rows: Vec<T> = wmi
        .query()
        .with_context(|| format!("WMI query for {} failed", std::any::type_name::<T>()))?;
    Ok(rows.into_iter().next())
}

pub fn read_machine_info() -> Result<MachineInfo> {
    let wmi = WMIConnection::new().context("cannot connect to WMI")?;

    let cs = first::<Win32_ComputerSystem>(&wmi)?.context("no Win32_ComputerSystem instance")?;
    let product = first::<Win32_ComputerSystemProduct>(&wmi)?;
    let board = first::<Win32_BaseBoard>(&wmi)?;
    let bios = first::<Win32_BIOS>(&wmi)?;
    let os = first::<Win32_OperatingSystem>(&wmi)?.context("no Win32_OperatingSystem instance")?;

    let (product_version, uuid) = product.map(|p| (p.Version, p.UUID)).unwrap_or_default();
    let (baseboard_manufacturer, baseboard_product) = board
        .map(|b| (b.Manufacturer, b.Product))
        .unwrap_or_default();
    let (bios_version, serial_number) = bios
        .map(|b| (b.SMBIOSBIOSVersion, b.SerialNumber))
        .unwrap_or_default();

    Ok(MachineInfo {
        manufacturer: clean(cs.Manufacturer).unwrap_or_default(),
        model: clean(cs.Model).unwrap_or_default(),
        system_sku: clean(cs.SystemSKUNumber),
        system_family: clean(cs.SystemFamily),
        product_version: clean(product_version),
        baseboard_manufacturer: clean(baseboard_manufacturer),
        baseboard_product: clean(baseboard_product),
        bios_version: clean(bios_version),
        serial_number: clean(serial_number),
        uuid: clean(uuid),
        os: OsInfo {
            caption: os.Caption.unwrap_or_default().trim().to_string(),
            version: os.Version.unwrap_or_default(),
            build: os.BuildNumber.and_then(|b| b.parse().ok()).unwrap_or(0),
            display_version: read_display_version(),
            architecture: os.OSArchitecture.unwrap_or_default(),
        },
    })
}

/// `DisplayVersion` (e.g. `24H2`) is not exposed by WMI, only by the registry.
fn read_display_version() -> Option<String> {
    let mut buf = [0u16; 64];
    let mut size = size_of_val(&buf) as u32;
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"),
            w!("DisplayVersion"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    }
    .ok()
    .ok()?;
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    clean(Some(String::from_utf16_lossy(&buf[..len])))
}
