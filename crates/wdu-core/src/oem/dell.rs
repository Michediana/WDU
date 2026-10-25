//! Dell: `CatalogIndexPC.cab` maps each System ID to a per-model catalog CAB.

use anyhow::{Context, Result};

use super::xml::{Element, decode_text};
use super::{
    Catalog, CatalogEntry, CatalogQuery, EntryKind, Http, as_driver_version, extract_single_cab,
    parse_month_name_date, verify_sha256,
};
use crate::model::Brand;
use crate::provider::{Candidate, Provider, Source};

pub const INDEX_URL: &str = "https://downloads.dell.com/catalog/CatalogIndexPC.cab";
pub const BASE_URL: &str = "https://downloads.dell.com/";

/// One per-model catalog listed in the index.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelManifest {
    pub system_ids: Vec<String>,
    pub name: Option<String>,
    /// Relative to [`BASE_URL`].
    pub path: String,
    pub sha256: Option<String>,
}

pub fn parse_index(xml: &str) -> Result<Vec<ModelManifest>> {
    let root = Element::parse(xml)?;
    let manifests = root
        .children("GroupManifest")
        .filter_map(|group| {
            let info = group.child("ManifestInformation")?;
            Some(ModelManifest {
                system_ids: group
                    .descendants("Model")
                    .into_iter()
                    .filter_map(|m| m.attr("systemID"))
                    .map(str::to_ascii_uppercase)
                    .collect(),
                name: group.text_at(&["Display"]),
                path: info.attr("path")?.to_string(),
                sha256: sha256_of(info),
            })
        })
        .collect();
    Ok(manifests)
}

pub fn parse_model_catalog(xml: &str) -> Result<Vec<CatalogEntry>> {
    let root = Element::parse(xml)?;
    Ok(root
        .children("SoftwareComponent")
        .filter_map(parse_component)
        .collect())
}

fn parse_component(c: &Element) -> Option<CatalogEntry> {
    let kind = match c.child("ComponentType")?.attr("value")? {
        "DRVR" => EntryKind::Driver,
        "BIOS" => EntryKind::Bios,
        "FRMW" => EntryKind::Firmware,
        "APAC" => EntryKind::Application,
        _ => EntryKind::Other,
    };
    let mut hardware_ids: Vec<String> = c
        .descendants("PCIInfo")
        .into_iter()
        .filter_map(pci_hardware_id)
        .chain(
            c.descendants("PnPInfo")
                .into_iter()
                .flat_map(acpi_hardware_ids),
        )
        .collect();
    hardware_ids.sort();
    hardware_ids.dedup();

    Some(CatalogEntry {
        id: c.attr("releaseID")?.to_string(),
        name: c.text_at(&["Name", "Display"]).unwrap_or_default(),
        version: c.attr("vendorVersion").map(str::to_string),
        driver_version: as_driver_version(c.attr("vendorVersion")),
        kind,
        category: c.text_at(&["Category", "Display"]),
        release_date: c.attr("releaseDate").and_then(parse_month_name_date),
        download_url: c.attr("path").map(|p| format!("{BASE_URL}{p}")),
        size: c.attr("size").and_then(|s| s.parse().ok()),
        sha256: sha256_of(c),
        hardware_ids,
        os: c
            .child("SupportedOperatingSystems")
            .map(|oses| {
                oses.children("OperatingSystem")
                    .filter_map(|os| os.attr("osCode"))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        silent_install: None,
    })
}

fn sha256_of(e: &Element) -> Option<String> {
    e.child("Cryptography")?
        .children("Hash")
        .find(|h| h.attr("algorithm") == Some("SHA256"))?
        .text()
        .map(str::to_ascii_lowercase)
}

/// `PCI\VEN_8086&DEV_7360&SUBSYS_58201028`, or without `SUBSYS` when absent.
fn pci_hardware_id(pci: &Element) -> Option<String> {
    let ven = pci.attr("vendorID")?;
    let dev = pci.attr("deviceID")?;
    let id = match (pci.attr("subDeviceID"), pci.attr("subVendorID")) {
        (Some(sd), Some(sv)) => format!("PCI\\VEN_{ven}&DEV_{dev}&SUBSYS_{sd}{sv}"),
        _ => format!("PCI\\VEN_{ven}&DEV_{dev}"),
    };
    Some(id.to_ascii_uppercase())
}

/// ACPI devices are enumerated either as `ACPI\INT33D5` or `ACPI\VEN_INT&DEV_33D5`.
fn acpi_hardware_ids(pnp: &Element) -> Vec<String> {
    let (Some(ven), Some(dev)) = (pnp.text_at(&["ACPIID"]), pnp.text_at(&["PnPProductID"])) else {
        return Vec::new();
    };
    vec![
        format!("ACPI\\{ven}{dev}").to_ascii_uppercase(),
        format!("ACPI\\VEN_{ven}&DEV_{dev}").to_ascii_uppercase(),
    ]
}

pub fn fetch(http: &Http, system_id: &str) -> Result<Catalog> {
    let system_id = system_id.to_ascii_uppercase();
    let index = decode_text(&extract_single_cab(&http.get(INDEX_URL)?)?)?;
    let manifest = parse_index(&index)?
        .into_iter()
        .find(|m| m.system_ids.contains(&system_id))
        .with_context(|| format!("Dell System ID {system_id} not found in {INDEX_URL}"))?;

    let url = format!("{BASE_URL}{}", manifest.path);
    let cab = http.get(&url)?;
    if let Some(expected) = &manifest.sha256 {
        verify_sha256(&cab, expected, &url)?;
    }
    let entries = parse_model_catalog(&decode_text(&extract_single_cab(&cab)?)?)?;
    Ok(Catalog {
        brand: Brand::Dell,
        system_id,
        model_name: manifest.name,
        source_url: url,
        entries,
    })
}

/// [`Provider`] over the Dell catalog of one System ID.
pub struct DellProvider {
    pub http: Http,
    pub query: CatalogQuery,
}

impl Provider for DellProvider {
    fn source(&self) -> Source {
        Source::Dell
    }

    fn candidates(&self) -> Result<Vec<Candidate>> {
        fetch(&self.http, &self.query.system_id)?.candidates(self.query.windows11)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INDEX: &str = r#"<?xml version="1.0" encoding="utf-16"?>
<ManifestIndex baseLocation="downloads.dell.com" xmlns="openmanage/cm/dm">
  <GroupManifest type="MTPDK" id="a">
    <Display lang="en"><![CDATA[ PDK Catalog for Latitude 5420]]></Display>
    <SupportedSystems>
      <Brand key="28" prefix="LAT">
        <Model systemID="0A20" systemIDType="BIOS"><Display lang="en"><![CDATA[5420]]></Display></Model>
        <Model systemID="0a21" systemIDType="BIOS"><Display lang="en"><![CDATA[5420 2]]></Display></Model>
      </Brand>
    </SupportedSystems>
    <ManifestInformation path="FOLDER15162537M/1/Latitude_0A20.cab" size="1">
      <Cryptography>
        <Hash algorithm="MD5">00</Hash>
        <Hash algorithm="SHA256">ABCDEF</Hash>
      </Cryptography>
    </ManifestInformation>
  </GroupManifest>
</ManifestIndex>"#;

    const CATALOG: &str = r#"<?xml version="1.0" encoding="utf-16"?>
<Manifest baseLocation="downloads.dell.com" xmlns="openmanage/cm/dm">
  <SoftwareComponent releaseID="N88NY" releaseDate="January 04, 2021" vendorVersion="6.8.0.22" dellVersion="A10" path="FOLDER06587964M/3/Dell-Wireless.EXE" size="55500992">
    <Name><Display lang="en"><![CDATA[Dell Wireless 5820e Driver]]></Display></Name>
    <ComponentType value="DRVR"><Display lang="en"><![CDATA[Driver]]></Display></ComponentType>
    <Category value="CM"><Display lang="en"><![CDATA[Communications]]></Display></Category>
    <SupportedDCHDevices>
      <Device componentID="1"><PCIInfo deviceID="7360" vendorID="8086" subDeviceID="5820" subVendorID="1028" /></Device>
    </SupportedDCHDevices>
    <SupportedDevices>
      <Device componentID="2"><PCIInfo deviceID="43e4" vendorID="8086" subDeviceID="" subVendorID="" /></Device>
      <Device componentID="3"><PnPInfo><ACPIID>INT</ACPIID><PnPProductID>33D5</PnPProductID></PnPInfo></Device>
    </SupportedDevices>
    <SupportedOperatingSystems>
      <OperatingSystem osCode="W10P4" osArch="x64" />
      <OperatingSystem osCode="W11AP" osArch="x64" />
    </SupportedOperatingSystems>
    <Cryptography><Hash algorithm="SHA256">BB5FAC</Hash></Cryptography>
  </SoftwareComponent>
  <SoftwareComponent releaseID="B1OS1" releaseDate="March 1, 2024" vendorVersion="1.30.0" path="x/bios.exe">
    <Name><Display lang="en"><![CDATA[BIOS]]></Display></Name>
    <ComponentType value="BIOS" />
  </SoftwareComponent>
</Manifest>"#;

    #[test]
    fn index_maps_system_ids_to_catalogs() {
        let manifests = parse_index(INDEX).unwrap();
        assert_eq!(manifests.len(), 1);
        let m = &manifests[0];
        assert_eq!(m.system_ids, ["0A20", "0A21"]);
        assert_eq!(m.path, "FOLDER15162537M/1/Latitude_0A20.cab");
        assert_eq!(m.sha256.as_deref(), Some("abcdef"));
        assert_eq!(m.name.as_deref(), Some("PDK Catalog for Latitude 5420"));
    }

    #[test]
    fn model_catalog_entries() {
        let entries = parse_model_catalog(CATALOG).unwrap();
        assert_eq!(entries.len(), 2);
        let wifi = &entries[0];
        assert_eq!(wifi.id, "N88NY");
        assert_eq!(wifi.kind, EntryKind::Driver);
        assert_eq!(wifi.version.as_deref(), Some("6.8.0.22"));
        assert_eq!(wifi.driver_version.as_deref(), Some("6.8.0.22"));
        assert_eq!(wifi.release_date.as_deref(), Some("2021-01-04"));
        assert_eq!(
            wifi.download_url.as_deref(),
            Some("https://downloads.dell.com/FOLDER06587964M/3/Dell-Wireless.EXE")
        );
        assert_eq!(wifi.sha256.as_deref(), Some("bb5fac"));
        assert_eq!(
            wifi.hardware_ids,
            [
                "ACPI\\INT33D5",
                "ACPI\\VEN_INT&DEV_33D5",
                "PCI\\VEN_8086&DEV_43E4",
                "PCI\\VEN_8086&DEV_7360&SUBSYS_58201028",
            ]
        );
        assert_eq!(wifi.os, ["W10P4", "W11AP"]);
        assert_eq!(entries[1].kind, EntryKind::Bios);
    }
}
