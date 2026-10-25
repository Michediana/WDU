//! Cross-checks the SetupAPI inventory against what Windows itself reports
//! (the PnP cmdlets behind Device Manager, and the signed driver table).
#![cfg(windows)]

use std::collections::{BTreeSet, HashMap};
use std::process::Command;

use serde::Deserialize;
use wdu_core::inventory::{InventoryOptions, enumerate_devices};
use wdu_core::model::{Device, Snapshot};

fn powershell_json<T: for<'de> Deserialize<'de>>(script: &str) -> Vec<T> {
    let script = format!(
        "[Console]::OutputEncoding = [Text.Encoding]::UTF8; @({script}) | ConvertTo-Json -Compress -Depth 2"
    );
    let out = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .expect("cannot run powershell.exe");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    let text = text.trim().trim_start_matches('\u{feff}');
    // A single object is not wrapped in an array by ConvertTo-Json.
    if text.starts_with('{') {
        vec![serde_json::from_str(text).unwrap()]
    } else {
        serde_json::from_str(text).unwrap()
    }
}

fn by_id(devices: &[Device]) -> HashMap<String, &Device> {
    devices
        .iter()
        .map(|d| (d.instance_id.to_ascii_uppercase(), d))
        .collect()
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PnpDevice {
    instance_id: String,
    class: Option<String>,
}

#[test]
fn present_devices_match_pnp() {
    let ours = enumerate_devices(InventoryOptions::default()).unwrap();
    let theirs: Vec<PnpDevice> =
        powershell_json("Get-PnpDevice -PresentOnly | Select-Object InstanceId, Class");

    let ours_ids: BTreeSet<String> = ours
        .iter()
        .map(|d| d.instance_id.to_ascii_uppercase())
        .collect();
    let theirs_ids: BTreeSet<String> = theirs
        .iter()
        .map(|d| d.instance_id.to_ascii_uppercase())
        .collect();
    let missing: Vec<_> = theirs_ids.difference(&ours_ids).collect();
    let extra: Vec<_> = ours_ids.difference(&theirs_ids).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "missing from inventory: {missing:#?}\nnot reported by Windows: {extra:#?}"
    );

    let ours = by_id(&ours);
    for pnp in &theirs {
        let device = ours[&pnp.instance_id.to_ascii_uppercase()];
        assert_eq!(device.class, pnp.class, "class of {}", pnp.instance_id);
        assert!(device.status.present);
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SignedDriver {
    #[serde(rename = "DeviceID")]
    device_id: Option<String>,
    driver_version: Option<String>,
    inf_name: Option<String>,
    driver_provider_name: Option<String>,
}

#[test]
fn driver_details_match_signed_driver_table() {
    let ours = enumerate_devices(InventoryOptions::default()).unwrap();
    let ours = by_id(&ours);
    let drivers: Vec<SignedDriver> = powershell_json(
        "Get-CimInstance Win32_PnPSignedDriver | Select-Object DeviceID, DriverVersion, InfName, DriverProviderName",
    );

    let mut compared = 0;
    for d in drivers {
        let (Some(id), Some(inf)) = (d.device_id, d.inf_name) else {
            continue;
        };
        let Some(device) = ours.get(&id.to_ascii_uppercase()) else {
            // The table also lists some non-present devices.
            continue;
        };
        let driver = device
            .driver
            .as_ref()
            .unwrap_or_else(|| panic!("{id} has driver {inf} but inventory has none"));
        assert!(
            driver
                .inf_name
                .as_deref()
                .is_some_and(|i| i.eq_ignore_ascii_case(&inf)),
            "INF of {id}: {:?} vs {inf}",
            driver.inf_name
        );
        assert_eq!(driver.version, d.driver_version, "version of {id}");
        assert_eq!(driver.provider, d.driver_provider_name, "provider of {id}");
        compared += 1;
    }
    assert!(compared > 0, "no driver compared");
}

#[test]
fn live_snapshot_round_trips() {
    let devices = enumerate_devices(InventoryOptions {
        include_absent: true,
    })
    .unwrap();
    assert!(devices.iter().any(|d| d.status.present));
    let snapshot = Snapshot::new(None, devices);
    let parsed = Snapshot::from_json(&snapshot.to_json().unwrap()).unwrap();
    assert_eq!(parsed, snapshot);
}

/// Hardware IDs every Hyper-V generation 2 VM exposes.
const HYPERV_GEN2_IDS: &[&str] = &[
    // Hyper-V Video
    "VMBUS\\{DA0A7802-E377-4AAC-8E77-0558EB1073F8}",
    // Hyper-V Network Adapter
    "VMBUS\\{F8615163-DF3E-46C5-913F-F2D2F965ED0E}",
    // Microsoft Hyper-V SCSI Controller
    "VMBUS\\{BA6163D9-04A1-4D29-B605-72E2FFB1DC7F}",
];

/// Run inside a Hyper-V gen 2 VM with `WDU_EXPECT_VM=hyperv cargo test -- --ignored`.
#[test]
#[ignore = "needs a Hyper-V generation 2 VM"]
fn hyperv_gen2_known_hardware() {
    assert_eq!(
        std::env::var("WDU_EXPECT_VM").as_deref(),
        Ok("hyperv"),
        "set WDU_EXPECT_VM=hyperv inside the VM"
    );
    let devices = enumerate_devices(InventoryOptions::default()).unwrap();
    for expected in HYPERV_GEN2_IDS {
        let device = devices
            .iter()
            .find(|d| d.all_ids().any(|id| id.eq_ignore_ascii_case(expected)))
            .unwrap_or_else(|| panic!("no device with ID {expected}"));
        assert!(device.driver.is_some(), "{expected} has no driver");
        assert_eq!(
            device.status.problem_code, 0,
            "{expected} reports a problem"
        );
    }
}
