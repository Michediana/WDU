//! The common format every update source produces, and the trait they implement.
//!
//! Windows Update and the OEM catalogs describe drivers differently; each
//! provider maps its own records to [`Candidate`] so matching never needs to
//! know where an update came from beyond its [`Source`].

use std::fmt;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::model::{Brand, DriverVersion};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    Driver,
    Bios,
    Firmware,
    Application,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Dell,
    Hp,
    Lenovo,
    WindowsUpdate,
}

impl Source {
    pub fn is_oem(self) -> bool {
        self != Source::WindowsUpdate
    }

    pub fn brand(self) -> Option<Brand> {
        match self {
            Source::Dell => Some(Brand::Dell),
            Source::Hp => Some(Brand::Hp),
            Source::Lenovo => Some(Brand::Lenovo),
            Source::WindowsUpdate => None,
        }
    }
}

impl TryFrom<Brand> for Source {
    type Error = anyhow::Error;

    fn try_from(brand: Brand) -> Result<Self> {
        match brand {
            Brand::Dell => Ok(Source::Dell),
            Brand::Hp => Ok(Source::Hp),
            Brand::Lenovo => Ok(Source::Lenovo),
            Brand::Other => anyhow::bail!("no OEM catalog for this manufacturer"),
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Source::Dell => "dell",
            Source::Hp => "hp",
            Source::Lenovo => "lenovo",
            Source::WindowsUpdate => "wu",
        })
    }
}

/// A driver some source offers, in the format matching works on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub source: Source,
    /// Windows Update ID, or release ID / SoftPaq / package ID in the OEM catalog.
    pub id: String,
    pub title: String,
    pub kind: EntryKind,
    /// Uppercase hardware IDs the driver targets.
    pub hardware_ids: Vec<String>,
    pub class: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    /// `DriverVer` version of the driver it installs, when the source declares it.
    pub driver_version: Option<String>,
    /// `DriverVer` date (`YYYY-MM-DD`). Package release dates never go here:
    /// they are later than the driver date and would look like updates.
    pub driver_date: Option<String>,
    pub release_date: Option<String>,
    /// What to download for OEM candidates; Windows Update downloads by itself.
    pub package: Option<Package>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Package {
    pub url: String,
    pub size: Option<u64>,
    /// Lowercase hex, as declared by the catalog.
    pub sha256: Option<String>,
    /// Silent install command line, in the OEM's own notation.
    pub silent_install: Option<String>,
}

impl Candidate {
    pub fn version(&self) -> Option<DriverVersion> {
        self.driver_version.as_deref()?.parse().ok()
    }
}

/// An update source: Windows Update or one OEM catalog.
pub trait Provider {
    fn source(&self) -> Source;

    /// Driver updates this source offers for the current machine.
    fn candidates(&self) -> Result<Vec<Candidate>>;
}
