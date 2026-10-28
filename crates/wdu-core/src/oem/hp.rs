//! HP: reference files per platform ID and OS release, as used by HP Image Assistant.

use std::collections::HashMap;

use anyhow::{Context, Result};

use super::xml::{Element, decode_text};
use super::{
    Catalog, CatalogEntry, CatalogQuery, EntryKind, Http, as_driver_version, extract_single_cab,
};
use crate::model::Brand;
use crate::provider::{Candidate, Provider, Source};

pub const PLATFORM_LIST_URL: &str = "https://hpia.hpcloud.hp.com/ref/platformList.cab";
pub const REFERENCE_BASE_URL: &str = "https://hpia.hpcloud.hp.com/ref/";

#[derive(Debug, Clone, PartialEq)]
pub struct Platform {
    pub system_ids: Vec<String>,
    pub product_names: Vec<String>,
    pub os: Vec<PlatformOs>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlatformOs {
    pub windows11: bool,
    /// Release tag used in file names, e.g. `24H2` or `2009`.
    pub release: String,
    pub build: u32,
    pub x64: bool,
}

impl PlatformOs {
    /// File name fragment, e.g. `11.0.24h2`.
    fn file_tag(&self) -> String {
        let major = if self.windows11 { "11.0" } else { "10.0" };
        format!("{major}.{}", self.release.to_ascii_lowercase())
    }
}

pub fn parse_platform_list(xml: &str) -> Result<Vec<Platform>> {
    let root = Element::parse(xml)?;
    Ok(root
        .children("Platform")
        .map(|p| Platform {
            system_ids: p
                .children("SystemID")
                .filter_map(Element::text)
                .flat_map(|s| s.split(','))
                .map(|s| s.trim().to_ascii_uppercase())
                .filter(|s| !s.is_empty())
                .collect(),
            product_names: p
                .children("ProductName")
                .filter_map(Element::text)
                .map(str::to_string)
                .collect(),
            os: p
                .children("OS")
                .filter_map(|os| {
                    Some(PlatformOs {
                        windows11: os.text_at(&["IsWindows11"]).as_deref() == Some("true"),
                        release: os.text_at(&["OSReleaseIdFilename"])?,
                        build: os.text_at(&["OSBuildId"])?.parse().ok()?,
                        x64: os.text_at(&["OSArchitecture"]).as_deref() == Some("64"),
                    })
                })
                .collect(),
        })
        .collect())
}

/// Picks the reference file for a platform: same Windows generation, the
/// exact release when HP publishes it, otherwise the newest one.
pub fn select_reference(
    platforms: &[Platform],
    system_id: &str,
    windows11: bool,
    display_version: Option<&str>,
) -> Option<(String, Option<String>)> {
    let system_id = system_id.to_ascii_uppercase();
    let platform = platforms
        .iter()
        .find(|p| p.system_ids.contains(&system_id))?;
    let candidates: Vec<&PlatformOs> = platform
        .os
        .iter()
        .filter(|os| os.x64 && os.windows11 == windows11)
        .collect();
    let os = display_version
        .and_then(|v| {
            candidates
                .iter()
                .find(|os| os.release.eq_ignore_ascii_case(v))
        })
        .or_else(|| candidates.iter().max_by_key(|os| os.build))?;
    let url = format!(
        "{REFERENCE_BASE_URL}{id}/{id}_64_{tag}.cab",
        id = system_id,
        tag = os.file_tag()
    );
    Some((url, platform.product_names.first().cloned()))
}

pub fn parse_reference(xml: &str) -> Result<Vec<CatalogEntry>> {
    let root = Element::parse(xml)?;

    // Hardware IDs live under <Devices>, each device pointing at SoftPaqs by IdRef.
    let mut ids_by_softpaq: HashMap<&str, Vec<String>> = HashMap::new();
    if let Some(devices) = root.child("Devices") {
        for device in devices.children("Device") {
            let ids: Vec<String> = ["DeviceId", "HardwareId"]
                .iter()
                .filter_map(|tag| device.text_at(&[tag]))
                .map(|s| s.to_ascii_uppercase())
                .collect();
            for info in device
                .child("Solutions")
                .into_iter()
                .flat_map(|s| s.children("UpdateInfo"))
            {
                if let Some(idref) = info.attr("IdRef") {
                    ids_by_softpaq
                        .entry(idref)
                        .or_default()
                        .extend(ids.iter().cloned());
                }
            }
        }
    }

    let solutions = root
        .child("Solutions")
        .context("reference file has no <Solutions>")?;
    Ok(solutions
        .children("UpdateInfo")
        .filter_map(|u| {
            let id = u.text_at(&["Id"])?;
            let category = u.text_at(&["Category"]);
            let mut hardware_ids = ids_by_softpaq.remove(id.as_str()).unwrap_or_default();
            hardware_ids.sort();
            hardware_ids.dedup();
            Some(CatalogEntry {
                name: u.text_at(&["Name"]).unwrap_or_default(),
                version: u.text_at(&["Version"]),
                driver_version: as_driver_version(u.text_at(&["Version"]).as_deref()),
                kind: kind_of(category.as_deref().unwrap_or_default()),
                category,
                release_date: u.text_at(&["DateReleased"]),
                download_url: u.text_at(&["Url"]).map(|url| https_url(&url)),
                size: u.text_at(&["Size"]).and_then(|s| s.parse().ok()),
                sha256: u.text_at(&["SHA256"]).map(|s| s.to_ascii_lowercase()),
                hardware_ids,
                os: u
                    .text_at(&["SupportedOS"])
                    .map(|s| s.split(',').map(|o| o.trim().to_string()).collect())
                    .unwrap_or_default(),
                silent_install: u.text_at(&["SilentInstall"]),
                id,
            })
        })
        .collect())
}

fn kind_of(category: &str) -> EntryKind {
    let c = category.to_ascii_lowercase();
    if c.contains("driver") {
        EntryKind::Driver
    } else if c.starts_with("bios") {
        EntryKind::Bios
    } else if c.contains("firmware") {
        EntryKind::Firmware
    } else if c.starts_with("software") || c.starts_with("utility") || c.starts_with("diagnostic") {
        EntryKind::Application
    } else {
        EntryKind::Other
    }
}

/// Catalog URLs have no scheme (`ftp.hp.com/pub/softpaq/...`); force HTTPS.
fn https_url(url: &str) -> String {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("ftp://"))
        .unwrap_or(url);
    format!("https://{rest}")
}

pub fn fetch(http: &Http, query: &CatalogQuery) -> Result<Catalog> {
    let list = decode_text(&extract_single_cab(&http.get(PLATFORM_LIST_URL)?)?)?;
    let platforms = parse_platform_list(&list)?;
    let (url, model_name) = select_reference(
        &platforms,
        &query.system_id,
        query.windows11,
        query.display_version.as_deref(),
    )
    .with_context(|| {
        format!(
            "HP platform {} has no reference file for Windows {}",
            query.system_id,
            if query.windows11 { 11 } else { 10 }
        )
    })?;
    let entries = parse_reference(&decode_text(&extract_single_cab(&http.get(&url)?)?)?)?;
    Ok(Catalog {
        brand: Brand::Hp,
        system_id: query.system_id.to_ascii_uppercase(),
        model_name,
        source_url: url,
        entries,
    })
}

/// [`Provider`] over the Hp catalog of one platform ID.
pub struct HpProvider {
    pub http: Http,
    pub query: CatalogQuery,
}

impl Provider for HpProvider {
    fn source(&self) -> Source {
        Source::Hp
    }

    fn location(&self) -> Option<String> {
        Some(PLATFORM_LIST_URL.to_string())
    }

    fn candidates(&self) -> Result<Vec<Candidate>> {
        fetch(&self.http, &self.query)?.candidates(self.query.windows11)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLATFORMS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<ImagePal>
  <Platform>
    <ProductName Id="1">HP EliteBook 840 G6 Notebook PC</ProductName>
    <OS><OSArchitecture>64</OSArchitecture><OSReleaseIdFilename>2009</OSReleaseIdFilename><OSBuildId>19042</OSBuildId><IsWindows11>false</IsWindows11></OS>
    <OS><OSArchitecture>64</OSArchitecture><OSReleaseIdFilename>23H2</OSReleaseIdFilename><OSBuildId>22631</OSBuildId><IsWindows11>true</IsWindows11></OS>
    <OS><OSArchitecture>64</OSArchitecture><OSReleaseIdFilename>24H2</OSReleaseIdFilename><OSBuildId>26100</OSBuildId><IsWindows11>true</IsWindows11></OS>
    <SystemID>8549</SystemID>
  </Platform>
</ImagePal>"#;

    const REFERENCE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<ImagePal>
  <SystemInfo><System><Solutions><UpdateInfo IdRef="sp999" /></Solutions></System></SystemInfo>
  <Devices>
    <Device>
      <DeviceId>PCI\VEN_8086&amp;DEV_9DF0&amp;SUBSYS_00348086</DeviceId>
      <HardwareId />
      <Solutions><UpdateInfo IdRef="sp150000" /></Solutions>
    </Device>
  </Devices>
  <Solutions>
    <UpdateInfo>
      <Id>sp150000</Id>
      <Name>Intel Wireless LAN Drivers</Name>
      <Category>Driver - Network</Category>
      <Version>22.250.1.2</Version>
      <SilentInstall>"Install.cmd"</SilentInstall>
      <Url>ftp.hp.com/pub/softpaq/sp150001-150500/sp150000.exe</Url>
      <SHA256>C550B470</SHA256>
      <Size>1024</Size>
      <DateReleased>2024-01-24</DateReleased>
      <SupportedOS>W11_23H2, W11_24H2</SupportedOS>
    </UpdateInfo>
    <UpdateInfo>
      <Id>sp174025</Id>
      <Name>HP System BIOS</Name>
      <Category>BIOS</Category>
    </UpdateInfo>
  </Solutions>
  <Solutions-Superseded><UpdateInfo><Id>sp1</Id><Name>old</Name></UpdateInfo></Solutions-Superseded>
</ImagePal>"#;

    #[test]
    fn selects_reference_for_os() {
        let platforms = parse_platform_list(PLATFORMS).unwrap();
        let (url, name) = select_reference(&platforms, "8549", true, Some("23H2")).unwrap();
        assert_eq!(
            url,
            "https://hpia.hpcloud.hp.com/ref/8549/8549_64_11.0.23h2.cab"
        );
        assert_eq!(name.as_deref(), Some("HP EliteBook 840 G6 Notebook PC"));
        // Unknown release: newest Windows 11 one.
        let (url, _) = select_reference(&platforms, "8549", true, Some("25H2")).unwrap();
        assert!(url.ends_with("8549_64_11.0.24h2.cab"));
        let (url, _) = select_reference(&platforms, "8549", false, None).unwrap();
        assert!(url.ends_with("8549_64_10.0.2009.cab"));
        assert!(select_reference(&platforms, "0000", true, None).is_none());
    }

    #[test]
    fn reference_entries_with_hardware_ids() {
        let entries = parse_reference(REFERENCE).unwrap();
        assert_eq!(entries.len(), 2);
        let wlan = &entries[0];
        assert_eq!(wlan.id, "sp150000");
        assert_eq!(wlan.kind, EntryKind::Driver);
        assert_eq!(
            wlan.download_url.as_deref(),
            Some("https://ftp.hp.com/pub/softpaq/sp150001-150500/sp150000.exe")
        );
        assert_eq!(
            wlan.hardware_ids,
            ["PCI\\VEN_8086&DEV_9DF0&SUBSYS_00348086"]
        );
        assert_eq!(wlan.os, ["W11_23H2", "W11_24H2"]);
        assert_eq!(wlan.sha256.as_deref(), Some("c550b470"));
        assert_eq!(entries[1].kind, EntryKind::Bios);
    }
}
