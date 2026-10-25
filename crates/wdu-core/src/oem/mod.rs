//! OEM driver catalogs (Dell, HP, Lenovo): download and parsing.
//!
//! Each brand publishes its own format; every parser maps it to [`CatalogEntry`].

pub mod dell;
pub mod hp;
pub mod lenovo;
pub mod xml;

use std::io::{Cursor, Read};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::model::Brand;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    Driver,
    Bios,
    Firmware,
    Application,
    Other,
}

/// One downloadable package from an OEM catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogEntry {
    /// Release ID (Dell), SoftPaq number (HP) or package ID (Lenovo).
    pub id: String,
    pub name: String,
    pub version: Option<String>,
    pub kind: EntryKind,
    pub category: Option<String>,
    /// `YYYY-MM-DD`
    pub release_date: Option<String>,
    pub download_url: Option<String>,
    pub size: Option<u64>,
    /// Lowercase hex, as declared by the catalog.
    pub sha256: Option<String>,
    /// Hardware IDs the package targets, when the catalog declares them.
    pub hardware_ids: Vec<String>,
    /// OS identifiers in the OEM's own notation.
    pub os: Vec<String>,
    pub silent_install: Option<String>,
}

/// What to look up: the brand, its machine identifier and the target OS.
#[derive(Debug, Clone)]
pub struct CatalogQuery {
    pub brand: Brand,
    /// Dell System ID, HP platform ID or Lenovo machine type.
    pub system_id: String,
    pub windows11: bool,
    /// e.g. `24H2`
    pub display_version: Option<String>,
    /// Lenovo only: also download each package descriptor (one request per package).
    pub resolve_packages: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Catalog {
    pub brand: Brand,
    pub system_id: String,
    pub model_name: Option<String>,
    pub source_url: String,
    pub entries: Vec<CatalogEntry>,
}

pub fn fetch_catalog(http: &Http, query: &CatalogQuery) -> Result<Catalog> {
    match query.brand {
        Brand::Dell => dell::fetch(http, &query.system_id),
        Brand::Hp => hp::fetch(http, query),
        Brand::Lenovo => lenovo::fetch(http, query),
        Brand::Other => bail!("no OEM catalog for this manufacturer"),
    }
}

/// Hosts catalogs and packages may be downloaded from.
pub const ALLOWED_HOSTS: &[&str] = &[
    "downloads.dell.com",
    "hpia.hpcloud.hp.com",
    "ftp.hp.com",
    "download.lenovo.com",
];

/// Blocking HTTPS client restricted to [`ALLOWED_HOSTS`].
pub struct Http {
    client: reqwest::blocking::Client,
}

impl Http {
    pub fn new() -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .user_agent(concat!("wdu/", env!("CARGO_PKG_VERSION")))
            .https_only(true)
            .timeout(Duration::from_secs(120))
            .build()?;
        Ok(Http { client })
    }

    pub fn get(&self, url: &str) -> Result<Vec<u8>> {
        let parsed = reqwest::Url::parse(url).with_context(|| format!("invalid URL {url}"))?;
        let host = parsed.host_str().unwrap_or_default();
        if parsed.scheme() != "https" || !ALLOWED_HOSTS.contains(&host) {
            bail!("refusing to download from non allow-listed URL {url}");
        }
        tracing::debug!(url, "GET");
        let response = self
            .client
            .get(parsed)
            .send()
            .with_context(|| format!("GET {url} failed"))?
            .error_for_status()?;
        Ok(response.bytes()?.to_vec())
    }
}

/// Extracts the only (or first) file of a cabinet; OEM catalog CABs hold one XML.
pub fn extract_single_cab(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut cabinet = cab::Cabinet::new(Cursor::new(bytes)).context("not a valid CAB file")?;
    let name = cabinet
        .folder_entries()
        .flat_map(|folder| folder.file_entries())
        .map(|file| file.name().to_string())
        .next()
        .context("CAB file is empty")?;
    let mut out = Vec::new();
    cabinet.read_file(&name)?.read_to_end(&mut out)?;
    Ok(out)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn verify_sha256(bytes: &[u8], expected: &str, what: &str) -> Result<()> {
    let actual = sha256_hex(bytes);
    if !actual.eq_ignore_ascii_case(expected.trim()) {
        bail!("SHA-256 mismatch for {what}: expected {expected}, got {actual}");
    }
    Ok(())
}

/// Parses dates like `January 04, 2021` or `October 08,2026` into `YYYY-MM-DD`.
pub(crate) fn parse_month_name_date(s: &str) -> Option<String> {
    const MONTHS: [&str; 12] = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    let mut parts = s
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|p| !p.is_empty());
    let month_name = parts.next()?.to_ascii_lowercase();
    let month = MONTHS.iter().position(|m| *m == month_name)? + 1;
    let day: u32 = parts.next()?.parse().ok()?;
    let year: u32 = parts.next()?.parse().ok()?;
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_name_dates() {
        assert_eq!(
            parse_month_name_date("January 04, 2021").as_deref(),
            Some("2021-01-04")
        );
        assert_eq!(
            parse_month_name_date("October 08,2026").as_deref(),
            Some("2026-10-08")
        );
        assert_eq!(parse_month_name_date("Smarch 1, 2020"), None);
    }

    #[test]
    fn sha256_known_value() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(
            verify_sha256(
                b"abc",
                "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD",
                "x"
            )
            .is_ok()
        );
        assert!(verify_sha256(b"abd", "ba7816bf", "x").is_err());
    }

    #[test]
    fn http_rejects_foreign_hosts() {
        let http = Http::new().unwrap();
        assert!(http.get("https://example.com/catalog.cab").is_err());
        assert!(
            http.get("http://downloads.dell.com/catalog/CatalogIndexPC.cab")
                .is_err()
        );
    }
}
