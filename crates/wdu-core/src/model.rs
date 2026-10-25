//! Platform-independent data model shared by inventory, providers and the CLI.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Bumped whenever the JSON layout of [`Snapshot`] changes incompatibly.
pub const SNAPSHOT_SCHEMA_VERSION: u32 = 1;

/// A PnP device as seen by SetupAPI, roughly one row of Device Manager.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub instance_id: String,
    /// Friendly name, falling back to the device description.
    pub name: Option<String>,
    /// Most specific first, as reported by the bus driver.
    pub hardware_ids: Vec<String>,
    pub compatible_ids: Vec<String>,
    pub class: Option<String>,
    pub class_guid: Option<String>,
    pub manufacturer: Option<String>,
    /// `None` when no driver is installed for the device.
    pub driver: Option<DriverInfo>,
    pub status: DeviceStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverInfo {
    pub provider: Option<String>,
    /// Four-part version from the INF `DriverVer` directive.
    pub version: Option<String>,
    /// `YYYY-MM-DD`, from the INF `DriverVer` directive.
    pub date: Option<String>,
    /// Published name in the driver store, e.g. `oem42.inf`.
    pub inf_name: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceStatus {
    pub present: bool,
    /// Hidden from Device Manager unless "Show hidden devices" is enabled.
    pub hidden: bool,
    /// Configuration Manager problem code (`CM_PROB_*`), 0 when the device works.
    pub problem_code: u32,
}

impl Device {
    /// Hardware IDs followed by compatible IDs: the order used for driver ranking.
    pub fn all_ids(&self) -> impl Iterator<Item = &str> {
        self.hardware_ids
            .iter()
            .chain(&self.compatible_ids)
            .map(String::as_str)
    }

    pub fn driver_version(&self) -> Option<DriverVersion> {
        self.driver.as_ref()?.version.as_deref()?.parse().ok()
    }
}

/// Identity of the machine, used to pick the right OEM catalog.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineInfo {
    pub manufacturer: String,
    pub model: String,
    /// SMBIOS SKU; on Dell this is the System ID used by the catalogs.
    pub system_sku: Option<String>,
    pub system_family: Option<String>,
    /// `Win32_ComputerSystemProduct.Version`; on Lenovo the marketing name.
    pub product_version: Option<String>,
    pub baseboard_manufacturer: Option<String>,
    /// On HP this is the platform ID used by the reference files.
    pub baseboard_product: Option<String>,
    pub bios_version: Option<String>,
    pub serial_number: Option<String>,
    pub uuid: Option<String>,
    pub os: OsInfo,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OsInfo {
    pub caption: String,
    /// e.g. `10.0.26100`
    pub version: String,
    pub build: u32,
    /// e.g. `24H2`
    pub display_version: Option<String>,
    pub architecture: String,
}

impl OsInfo {
    pub fn is_windows11(&self) -> bool {
        self.build >= 22000
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Brand {
    Dell,
    Hp,
    Lenovo,
    Other,
}

impl Brand {
    pub fn from_manufacturer(manufacturer: &str) -> Self {
        let m = manufacturer.to_ascii_lowercase();
        if m.contains("dell") || m.contains("alienware") {
            Brand::Dell
        } else if m.contains("lenovo") {
            Brand::Lenovo
        } else if m == "hp" || m.contains("hewlett") || m.starts_with("hp ") {
            Brand::Hp
        } else {
            Brand::Other
        }
    }
}

impl fmt::Display for Brand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Brand::Dell => "dell",
            Brand::Hp => "hp",
            Brand::Lenovo => "lenovo",
            Brand::Other => "other",
        })
    }
}

impl FromStr for Brand {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "dell" => Ok(Brand::Dell),
            "hp" => Ok(Brand::Hp),
            "lenovo" => Ok(Brand::Lenovo),
            other => Err(format!(
                "unknown brand `{other}` (expected dell, hp or lenovo)"
            )),
        }
    }
}

impl MachineInfo {
    pub fn brand(&self) -> Brand {
        Brand::from_manufacturer(&self.manufacturer)
    }

    /// The identifier each OEM uses to select a catalog for this machine.
    pub fn oem_system_id(&self) -> Option<String> {
        let id = match self.brand() {
            Brand::Dell => self.system_sku.clone()?,
            Brand::Hp => self.baseboard_product.clone()?,
            Brand::Lenovo => self.model.get(..4)?.to_string(),
            Brand::Other => return None,
        };
        let id = id.trim().to_ascii_uppercase();
        (!id.is_empty()).then_some(id)
    }
}

/// Serializable inventory of a machine, used for diagnostics and as test fixture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub schema_version: u32,
    /// RFC 3339, UTC.
    pub created_at: String,
    pub machine: Option<MachineInfo>,
    pub devices: Vec<Device>,
}

impl Snapshot {
    pub fn new(machine: Option<MachineInfo>, mut devices: Vec<Device>) -> Self {
        devices.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
        Snapshot {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            created_at: crate::time::now_rfc3339(),
            machine,
            devices,
        }
    }

    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    pub fn from_json(json: &str) -> anyhow::Result<Self> {
        let snapshot: Snapshot = serde_json::from_str(json)?;
        anyhow::ensure!(
            snapshot.schema_version == SNAPSHOT_SCHEMA_VERSION,
            "unsupported snapshot schema version {} (expected {SNAPSHOT_SCHEMA_VERSION})",
            snapshot.schema_version
        );
        Ok(snapshot)
    }

    pub fn device(&self, instance_id: &str) -> Option<&Device> {
        self.devices
            .iter()
            .find(|d| d.instance_id.eq_ignore_ascii_case(instance_id))
    }

    /// Devices shown by Device Manager in its default view.
    pub fn visible_devices(&self) -> impl Iterator<Item = &Device> {
        self.devices
            .iter()
            .filter(|d| d.status.present && !d.status.hidden)
    }
}

/// Four-part driver version (`a.b.c.d`) as written in `DriverVer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct DriverVersion(pub [u16; 4]);

impl FromStr for DriverVersion {
    type Err = String;

    /// Missing trailing parts count as zero, like Windows does.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = [0u16; 4];
        let mut count = 0;
        for (i, part) in s.trim().split('.').enumerate() {
            if i >= 4 {
                return Err(format!("too many parts in version `{s}`"));
            }
            parts[i] = part
                .trim()
                .parse()
                .map_err(|_| format!("invalid version part `{part}` in `{s}`"))?;
            count += 1;
        }
        if count == 0 {
            return Err("empty version".into());
        }
        Ok(DriverVersion(parts))
    }
}

impl Ord for DriverVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.cmp(&other.0)
    }
}

impl PartialOrd for DriverVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for DriverVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d] = self.0;
        write!(f, "{a}.{b}.{c}.{d}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn driver_version_parses_and_orders() {
        let a: DriverVersion = "31.0.101.5590".parse().unwrap();
        let b: DriverVersion = "31.0.101.4502".parse().unwrap();
        let c: DriverVersion = "10.1".parse().unwrap();
        assert!(a > b);
        assert_eq!(c, DriverVersion([10, 1, 0, 0]));
        assert_eq!(a.to_string(), "31.0.101.5590");
        assert!("1.2.3.4.5".parse::<DriverVersion>().is_err());
        assert!("1.x".parse::<DriverVersion>().is_err());
        assert!("70000.1".parse::<DriverVersion>().is_err());
    }

    fn device(id: &str, present: bool, hidden: bool) -> Device {
        Device {
            instance_id: id.into(),
            name: Some(id.into()),
            hardware_ids: vec![format!("{id}&REV_01"), id.into()],
            compatible_ids: vec!["PCI\\CC_0300".into()],
            class: Some("Display".into()),
            class_guid: Some("{4D36E968-E325-11CE-BFC1-08002BE10318}".into()),
            manufacturer: None,
            driver: Some(DriverInfo {
                provider: Some("Intel Corporation".into()),
                version: Some("31.0.101.5590".into()),
                date: Some("2024-06-11".into()),
                inf_name: Some("oem42.inf".into()),
            }),
            status: DeviceStatus {
                present,
                hidden,
                problem_code: 0,
            },
        }
    }

    #[test]
    fn snapshot_json_round_trip_and_filters() {
        let snapshot = Snapshot::new(
            None,
            vec![
                device("PCI\\VEN_8086&DEV_B", true, false),
                device("PCI\\VEN_8086&DEV_A", true, true),
                device("PCI\\VEN_8086&DEV_C", false, false),
            ],
        );
        // Sorted by instance ID for stable diffs.
        assert_eq!(snapshot.devices[0].instance_id, "PCI\\VEN_8086&DEV_A");
        let parsed = Snapshot::from_json(&snapshot.to_json().unwrap()).unwrap();
        assert_eq!(parsed, snapshot);

        let visible: Vec<_> = parsed
            .visible_devices()
            .map(|d| d.instance_id.as_str())
            .collect();
        assert_eq!(visible, ["PCI\\VEN_8086&DEV_B"]);
        let d = parsed.device("pci\\ven_8086&dev_b").unwrap();
        assert_eq!(d.driver_version(), Some(DriverVersion([31, 0, 101, 5590])));
        assert_eq!(d.all_ids().last(), Some("PCI\\CC_0300"));
    }

    #[test]
    fn snapshot_rejects_other_schema_versions() {
        let json = r#"{"schema_version":99,"created_at":"x","machine":null,"devices":[]}"#;
        assert!(Snapshot::from_json(json).is_err());
    }

    #[test]
    fn brand_detection() {
        assert_eq!(Brand::from_manufacturer("Dell Inc."), Brand::Dell);
        assert_eq!(Brand::from_manufacturer("HP"), Brand::Hp);
        assert_eq!(Brand::from_manufacturer("Hewlett-Packard"), Brand::Hp);
        assert_eq!(Brand::from_manufacturer("LENOVO"), Brand::Lenovo);
        assert_eq!(
            Brand::from_manufacturer("Micro-Star International Co., Ltd."),
            Brand::Other
        );
        assert_eq!(Brand::from_manufacturer("Shenzhen Something"), Brand::Other);
    }

    #[test]
    fn oem_system_id_per_brand() {
        let mut m = MachineInfo {
            manufacturer: "LENOVO".into(),
            model: "20xw0026us".into(),
            ..Default::default()
        };
        assert_eq!(m.oem_system_id().as_deref(), Some("20XW"));

        m.manufacturer = "Dell Inc.".into();
        m.system_sku = Some("0a20".into());
        assert_eq!(m.oem_system_id().as_deref(), Some("0A20"));

        m.manufacturer = "HP".into();
        m.baseboard_product = Some("8549".into());
        assert_eq!(m.oem_system_id().as_deref(), Some("8549"));

        m.manufacturer = "ASUS".into();
        assert_eq!(m.oem_system_id(), None);
    }
}
