//! OEM driver catalogs (Dell, HP, Lenovo): download, parsing and providers.
//!
//! Each brand publishes its own format; every parser maps it to [`CatalogEntry`],
//! and [`Catalog::candidates`] turns entries into the common [`Candidate`].

pub mod dell;
pub mod hp;
mod http;
pub mod lenovo;
pub mod xml;

use std::fs;
use std::io::{Cursor, Read};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use self::http::{ALLOWED_HOSTS, Http};
use crate::model::{Brand, DriverVersion, MachineInfo};
pub use crate::provider::EntryKind;
use crate::provider::{Candidate, Package, Provider, Source};

/// One downloadable package from an OEM catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogEntry {
    /// Release ID (Dell), SoftPaq number (HP) or package ID (Lenovo).
    pub id: String,
    pub name: String,
    /// Package version, in the OEM's own notation.
    pub version: Option<String>,
    /// `DriverVer` version of the driver inside, when the catalog makes it known.
    #[serde(default)]
    pub driver_version: Option<String>,
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

impl CatalogEntry {
    /// OS codes differ per brand (`W11AP`, `W11_24H2`, `WIN11`); entries whose
    /// codes are all unrecognised are kept rather than silently dropped.
    pub fn supports_os(&self, windows11: bool) -> bool {
        let codes: Vec<String> = self.os.iter().map(|o| o.to_ascii_uppercase()).collect();
        let is =
            |o: &str, v: &str| o.starts_with(&format!("W{v}")) || o.starts_with(&format!("WIN{v}"));
        if !codes.iter().any(|o| is(o, "10") || is(o, "11")) {
            return true;
        }
        let wanted = if windows11 { "11" } else { "10" };
        codes.iter().any(|o| is(o, wanted))
    }

    pub fn to_candidate(&self, source: Source) -> Candidate {
        Candidate {
            source,
            id: self.id.clone(),
            title: self.name.clone(),
            kind: self.kind,
            hardware_ids: self.hardware_ids.clone(),
            class: self.category.clone(),
            provider: None,
            model: None,
            driver_version: self.driver_version.clone(),
            driver_date: None,
            release_date: self.release_date.clone(),
            package: self.download_url.as_ref().map(|url| Package {
                url: url.clone(),
                size: self.size,
                sha256: self.sha256.clone(),
                silent_install: self.silent_install.clone(),
            }),
        }
    }
}

/// Keeps a version only when it reads as a `DriverVer` version (`a.b.c.d`).
pub(crate) fn as_driver_version(version: Option<&str>) -> Option<String> {
    let v = version?.trim();
    v.parse::<DriverVersion>().ok().map(|_| v.to_string())
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

impl CatalogQuery {
    /// The catalog of `machine`, for the Windows it runs.
    pub fn for_machine(machine: &MachineInfo) -> Result<Self> {
        let brand = machine.brand();
        if brand == Brand::Other {
            bail!(
                "manufacturer `{}` has no supported OEM catalog",
                machine.manufacturer
            );
        }
        Ok(CatalogQuery {
            brand,
            system_id: machine
                .oem_system_id()
                .context("cannot determine the OEM system identifier of this machine")?,
            windows11: machine.os.is_windows11(),
            display_version: machine.os.display_version.clone(),
            resolve_packages: false,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Catalog {
    pub brand: Brand,
    pub system_id: String,
    pub model_name: Option<String>,
    pub source_url: String,
    pub entries: Vec<CatalogEntry>,
}

impl Catalog {
    /// Entries for the target OS, as candidates.
    pub fn candidates(&self, windows11: bool) -> Result<Vec<Candidate>> {
        let source = Source::try_from(self.brand)?;
        Ok(self
            .entries
            .iter()
            .filter(|e| e.supports_os(windows11))
            .map(|e| e.to_candidate(source))
            .collect())
    }
}

pub fn fetch_catalog(http: &Http, query: &CatalogQuery) -> Result<Catalog> {
    match query.brand {
        Brand::Dell => dell::fetch(http, &query.system_id),
        Brand::Hp => hp::fetch(http, query),
        Brand::Lenovo => lenovo::fetch(http, query),
        Brand::Other => bail!("no OEM catalog for this manufacturer"),
    }
}

/// The provider for the brand of `query`.
pub fn provider(http: Http, query: CatalogQuery) -> Result<Box<dyn Provider>> {
    Ok(match query.brand {
        Brand::Dell => Box::new(dell::DellProvider { http, query }),
        Brand::Hp => Box::new(hp::HpProvider { http, query }),
        Brand::Lenovo => Box::new(lenovo::LenovoProvider { http, query }),
        Brand::Other => bail!("no OEM catalog for this manufacturer"),
    })
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

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Extracts every file of a cabinet below `dest`, refusing paths that escape it.
pub fn extract_cab_all(cab_path: &Path, dest: &Path) -> Result<Vec<PathBuf>> {
    let file =
        fs::File::open(cab_path).with_context(|| format!("cannot open {}", cab_path.display()))?;
    let mut cabinet = cab::Cabinet::new(file).context("not a valid CAB file")?;
    let names: Vec<String> = cabinet
        .folder_entries()
        .flat_map(|folder| folder.file_entries())
        .map(|file| file.name().to_string())
        .collect();
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let relative = PathBuf::from(name.replace('\\', "/"));
        if !relative
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
        {
            bail!("CAB entry `{name}` escapes the extraction directory");
        }
        let target = dest.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut reader = cabinet.read_file(&name)?;
        let mut writer = fs::File::create(&target)?;
        std::io::copy(&mut reader, &mut writer)?;
        out.push(target);
    }
    Ok(out)
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

    fn entry(os: &[&str]) -> CatalogEntry {
        CatalogEntry {
            id: "x".into(),
            name: "x".into(),
            version: Some("A10".into()),
            driver_version: as_driver_version(Some("6.8.0.22")),
            kind: EntryKind::Driver,
            category: None,
            release_date: Some("2024-01-01".into()),
            download_url: Some("https://downloads.dell.com/x.exe".into()),
            size: None,
            sha256: None,
            hardware_ids: vec!["PCI\\VEN_8086&DEV_7360".into()],
            os: os.iter().map(|s| s.to_string()).collect(),
            silent_install: None,
        }
    }

    #[test]
    fn os_filter_per_brand_notation() {
        assert!(entry(&["W10P4", "W11AP"]).supports_os(true));
        assert!(!entry(&["W10P4"]).supports_os(true));
        assert!(entry(&["W11_24H2"]).supports_os(true));
        assert!(!entry(&["WIN11"]).supports_os(false));
        // Unknown notation: kept.
        assert!(entry(&["WT64A"]).supports_os(true));
        assert!(entry(&[]).supports_os(false));
    }

    #[test]
    fn entry_to_candidate() {
        let c = entry(&[]).to_candidate(Source::Dell);
        assert_eq!(c.driver_version.as_deref(), Some("6.8.0.22"));
        assert_eq!(c.driver_date, None);
        assert_eq!(c.package.unwrap().url, "https://downloads.dell.com/x.exe");
        assert_eq!(as_driver_version(Some("A10")), None);
    }
}
