//! Lenovo: one catalog per machine type and Windows version, pointing at one
//! XML descriptor per package (as used by Lenovo System Update).

use anyhow::{Context, Result};

use super::xml::{Element, decode_text};
use super::{Catalog, CatalogEntry, CatalogQuery, EntryKind, Http};
use crate::model::Brand;

pub fn catalog_url(machine_type: &str, windows11: bool) -> String {
    let os = if windows11 { "Win11" } else { "Win10" };
    format!(
        "https://download.lenovo.com/catalog/{}_{os}.xml",
        machine_type.to_ascii_uppercase()
    )
}

/// A row of the machine type catalog.
#[derive(Debug, Clone, PartialEq)]
pub struct PackageRef {
    /// URL of the package descriptor XML.
    pub location: String,
    pub category: Option<String>,
    /// SHA-256 of the descriptor, when declared.
    pub descriptor_sha256: Option<String>,
}

impl PackageRef {
    /// Package ID from the descriptor file name: `.../n32ic03w_2_.xml` -> `n32ic03w`.
    pub fn id(&self) -> String {
        let file = self.location.rsplit('/').next().unwrap_or(&self.location);
        let stem = file.strip_suffix(".xml").unwrap_or(file);
        stem.split('_').next().unwrap_or(stem).to_string()
    }

    /// Entry with only what the catalog itself knows.
    fn to_entry(&self) -> CatalogEntry {
        let id = self.id();
        CatalogEntry {
            name: id.clone(),
            id,
            version: None,
            kind: kind_of(self.category.as_deref().unwrap_or_default()),
            category: self.category.clone(),
            release_date: None,
            download_url: None,
            size: None,
            sha256: None,
            hardware_ids: Vec::new(),
            os: Vec::new(),
            silent_install: None,
        }
    }
}

pub fn parse_catalog(xml: &str) -> Result<Vec<PackageRef>> {
    let root = Element::parse(xml)?;
    Ok(root
        .children("package")
        .filter_map(|p| {
            let checksum = p.child("checksum");
            Some(PackageRef {
                location: p.text_at(&["location"])?,
                category: p.text_at(&["category"]),
                descriptor_sha256: checksum
                    .filter(|c| c.attr("type") == Some("sha256"))
                    .and_then(Element::text)
                    .map(str::to_ascii_lowercase),
            })
        })
        .collect())
}

/// Completes a catalog row with its package descriptor.
pub fn parse_descriptor(xml: &str, package: &PackageRef) -> Result<CatalogEntry> {
    let root = Element::parse(xml)?;
    let mut entry = package.to_entry();
    if let Some(id) = root.attr("id") {
        entry.id = id.to_string();
    }
    entry.name = root
        .text_at(&["Title", "Desc"])
        .unwrap_or_else(|| entry.id.clone());
    entry.version = root.attr("version").map(str::to_string);
    // More reliable than the catalog category, which files firmware under the device type.
    match root.child("PackageType").and_then(|p| p.attr("type")) {
        Some("1") => entry.kind = EntryKind::Application,
        Some("2") => entry.kind = EntryKind::Driver,
        Some("3") => entry.kind = EntryKind::Bios,
        Some("4") => entry.kind = EntryKind::Firmware,
        _ => {}
    }
    entry.release_date = root.text_at(&["ReleaseDate"]);
    entry.silent_install = root.text_at(&["Install", "Cmdline"]);

    if let Some(file) = root.at(&["Files", "Installer", "File"]) {
        let base = package
            .location
            .rsplit_once('/')
            .map(|(dir, _)| dir)
            .unwrap_or_default();
        entry.download_url = file.text_at(&["Name"]).map(|name| format!("{base}/{name}"));
        entry.size = file.text_at(&["Size"]).and_then(|s| s.parse().ok());
        // <CRC> holds a SHA-256 in current descriptors.
        entry.sha256 = file
            .text_at(&["CRC"])
            .filter(|crc| crc.len() == 64)
            .map(|crc| crc.to_ascii_lowercase());
    }

    // Detection rules list the hardware IDs the package applies to.
    let mut ids: Vec<String> = root
        .descendants("HardwareID")
        .into_iter()
        .chain(root.descendants("_PnPID"))
        .filter_map(Element::text)
        .map(str::to_ascii_uppercase)
        .collect();
    ids.sort();
    ids.dedup();
    entry.hardware_ids = ids;

    let mut os: Vec<String> = root
        .descendants("OS")
        .into_iter()
        .filter_map(Element::text)
        .map(str::to_string)
        .collect();
    os.dedup();
    entry.os = os;
    Ok(entry)
}

fn kind_of(category: &str) -> EntryKind {
    let c = category.to_ascii_lowercase();
    if c.starts_with("bios") {
        EntryKind::Bios
    } else if c.contains("firmware") {
        EntryKind::Firmware
    } else if c.starts_with("software") {
        EntryKind::Application
    } else {
        EntryKind::Driver
    }
}

pub fn fetch(http: &Http, query: &CatalogQuery) -> Result<Catalog> {
    let url = catalog_url(&query.system_id, query.windows11);
    let packages = parse_catalog(&decode_text(&http.get(&url)?)?)
        .with_context(|| format!("cannot parse {url}"))?;

    let mut entries = Vec::with_capacity(packages.len());
    for package in &packages {
        if !query.resolve_packages {
            entries.push(package.to_entry());
            continue;
        }
        let entry = http.get(&package.location).and_then(|bytes| {
            if let Some(expected) = &package.descriptor_sha256 {
                super::verify_sha256(&bytes, expected, &package.location)?;
            }
            parse_descriptor(&decode_text(&bytes)?, package)
        });
        match entry {
            Ok(entry) => entries.push(entry),
            Err(err) => {
                tracing::warn!(location = %package.location, "skipping package: {err:#}");
                entries.push(package.to_entry());
            }
        }
    }
    Ok(Catalog {
        brand: Brand::Lenovo,
        system_id: query.system_id.to_ascii_uppercase(),
        model_name: None,
        source_url: url,
        entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CATALOG: &str = r#"<?xml version="1.0" encoding="utf-8"?><packages count="2">
  <package>
    <location>https://download.lenovo.com/pccbbs/mobiles/n32ic03w_2_.xml</location>
    <category>Motherboard Devices Backplanes core chipset onboard video PCIe switches</category>
    <checksum type="sha256">FE23</checksum>
  </package>
  <package>
    <location>https://download.lenovo.com/pccbbs/mobiles/n32uj29w.xml</location>
    <category>BIOS UEFI</category>
    <checksum type="md5">00</checksum>
  </package>
</packages>"#;

    const DESCRIPTOR: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<Package name="CHIP_N32IC" id="n32ic03w" version="10.1.18800.8277" hide="False">
  <Title default="EN"><Desc id="EN">Intel Chipset Device Software</Desc></Title>
  <DetectVersion type="Inf">
    <_PnPID><![CDATA[PCI\VEN_8086&DEV_A082]]></_PnPID>
  </DetectVersion>
  <ReleaseDate>2021-11-09</ReleaseDate>
  <PackageType type="2" />
  <Install rc="0,259,3010" type="cmd" default="EN">
    <Cmdline id="EN">%PACKAGEPATH%\n32ic03w.exe /VERYSILENT</Cmdline>
  </Install>
  <DetectInstall><And><_Driver>
    <HardwareID><![CDATA[PCI\VEN_8086&DEV_A082]]></HardwareID>
    <HardwareID><![CDATA[pci\ven_8086&dev_9a14]]></HardwareID>
    <Version>10.1.24.6^</Version>
  </_Driver></And></DetectInstall>
  <Dependencies><_OS><OS>WIN10</OS><OS>WIN11</OS></_OS></Dependencies>
  <Files><Installer><File>
    <Name>n32ic03w.exe</Name>
    <CRC>A016FFD4DCA48F2FA6CDB4932FBF84C408775B0D6C842A55FE331ADFC4F065F3</CRC>
    <Size>1049112</Size>
  </File></Installer></Files>
</Package>"#;

    #[test]
    fn catalog_rows() {
        let packages = parse_catalog(CATALOG).unwrap();
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].id(), "n32ic03w");
        assert_eq!(packages[0].descriptor_sha256.as_deref(), Some("fe23"));
        assert_eq!(packages[1].id(), "n32uj29w");
        assert_eq!(packages[1].descriptor_sha256, None);
        assert_eq!(packages[1].to_entry().kind, EntryKind::Bios);
    }

    #[test]
    fn descriptor_details() {
        let packages = parse_catalog(CATALOG).unwrap();
        let entry = parse_descriptor(DESCRIPTOR, &packages[0]).unwrap();
        assert_eq!(entry.name, "Intel Chipset Device Software");
        assert_eq!(entry.version.as_deref(), Some("10.1.18800.8277"));
        assert_eq!(entry.kind, EntryKind::Driver);
        assert_eq!(
            entry.download_url.as_deref(),
            Some("https://download.lenovo.com/pccbbs/mobiles/n32ic03w.exe")
        );
        assert_eq!(entry.size, Some(1_049_112));
        assert_eq!(
            entry.hardware_ids,
            ["PCI\\VEN_8086&DEV_9A14", "PCI\\VEN_8086&DEV_A082"]
        );
        assert_eq!(entry.os, ["WIN10", "WIN11"]);
        assert!(entry.silent_install.unwrap().contains("/VERYSILENT"));

        let firmware =
            DESCRIPTOR.replace(r#"<PackageType type="2" />"#, r#"<PackageType type="4" />"#);
        let entry = parse_descriptor(&firmware, &packages[0]).unwrap();
        assert_eq!(entry.kind, EntryKind::Firmware);
    }

    #[test]
    fn catalog_url_per_os() {
        assert_eq!(
            catalog_url("20xw", true),
            "https://download.lenovo.com/catalog/20XW_Win11.xml"
        );
    }
}
