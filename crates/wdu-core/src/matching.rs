//! Matching candidates to installed devices, and the policy deciding what to propose.
//!
//! Rules, in order:
//! - only drivers (never BIOS, firmware or applications in v1);
//! - a candidate applies to a device when one of its hardware IDs equals one
//!   of the device's IDs; the earlier that ID in the device's list (hardware
//!   IDs, then compatible IDs) the more specific the match;
//! - class allow and deny lists;
//! - `DriverVer` version first, then date; never a downgrade;
//! - per device, OEM catalog before Windows Update, then the most specific
//!   match, then the newest driver.
//!
//! [`plan`] only decides: it is the dry run. Installing is up to the caller.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::model::Device;
use crate::provider::{Candidate, EntryKind, Source};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    /// Device classes (name such as `Net`, or class GUID) updates are limited
    /// to; empty means every class.
    pub allow_classes: Vec<String>,
    /// Device classes never updated, whatever the source.
    pub deny_classes: Vec<String>,
    /// Ignore Windows Update for devices the OEM catalog covers, even when it
    /// offers a newer driver: the OEM validated its own for this model.
    pub oem_exclusive: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            allow_classes: Vec::new(),
            deny_classes: Vec::new(),
            oem_exclusive: true,
        }
    }
}

impl Policy {
    pub fn from_json(json: &str) -> anyhow::Result<Self> {
        Ok(serde_json::from_str(json)?)
    }

    fn class_verdict(&self, class: Option<&str>, guid: Option<&str>) -> Option<SkipReason> {
        let names = [class, guid];
        let listed = |list: &[String]| {
            list.iter().any(|c| {
                names
                    .iter()
                    .flatten()
                    .any(|n| n.eq_ignore_ascii_case(c.trim()))
            })
        };
        let shown = || class.or(guid).unwrap_or("-").to_string();
        if listed(&self.deny_classes) {
            Some(SkipReason::ClassDenied { class: shown() })
        } else if !self.allow_classes.is_empty() && !listed(&self.allow_classes) {
            Some(SkipReason::ClassNotAllowed { class: shown() })
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    NewerVersion,
    /// Same version (or versions not comparable) and a later `DriverVer` date.
    NewerDate,
    /// The device has no driver at all.
    NoDriver,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum SkipReason {
    NotADriver {
        kind: EntryKind,
    },
    NoHardwareIds,
    NoMatchingDevice,
    ClassDenied {
        class: String,
    },
    ClassNotAllowed {
        class: String,
    },
    NotNewer,
    Downgrade {
        installed: String,
        offered: String,
    },
    /// Neither versions nor dates can be compared.
    UnknownVersion,
    /// The device is covered by the OEM catalog.
    OemPreferred,
    /// Another candidate fits the device better.
    BetterCandidate {
        by: String,
    },
}

impl SkipReason {
    pub fn kind(&self) -> &'static str {
        match self {
            SkipReason::NotADriver { .. } => "not a driver",
            SkipReason::NoHardwareIds => "no hardware IDs in catalog",
            SkipReason::NoMatchingDevice => "no matching device",
            SkipReason::ClassDenied { .. } => "class denied",
            SkipReason::ClassNotAllowed { .. } => "class not allowed",
            SkipReason::NotNewer => "not newer",
            SkipReason::Downgrade { .. } => "would downgrade",
            SkipReason::UnknownVersion => "version not comparable",
            SkipReason::OemPreferred => "OEM catalog preferred",
            SkipReason::BetterCandidate { .. } => "better candidate",
        }
    }
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SkipReason::NotADriver { kind } => write!(f, "not a driver ({kind:?})"),
            SkipReason::ClassDenied { class } => write!(f, "class {class} denied"),
            SkipReason::ClassNotAllowed { class } => write!(f, "class {class} not allowed"),
            SkipReason::Downgrade { installed, offered } => {
                write!(f, "would downgrade {installed} to {offered}")
            }
            SkipReason::BetterCandidate { by } => write!(f, "{by} fits better"),
            other => f.write_str(other.kind()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceUpdate {
    pub instance_id: String,
    pub name: Option<String>,
    pub class: Option<String>,
    /// All the device's hardware IDs, to pick the right INF at install time.
    pub hardware_ids: Vec<String>,
    pub matched_id: String,
    /// Position of `matched_id` among hardware then compatible IDs; 0 is the most specific.
    pub rank: usize,
    pub installed_version: Option<String>,
    pub installed_date: Option<String>,
    pub reason: Reason,
}

/// One candidate to install, and the devices it updates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Proposal {
    pub candidate: Candidate,
    pub devices: Vec<DeviceUpdate>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Skipped {
    pub source: Source,
    pub id: String,
    pub title: String,
    /// Instance ID, when the candidate matched a device.
    pub device: Option<String>,
    #[serde(flatten)]
    pub reason: SkipReason,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub proposals: Vec<Proposal>,
    pub skipped: Vec<Skipped>,
}

impl Plan {
    /// How many candidates were skipped for each reason.
    pub fn skipped_by_reason(&self) -> BTreeMap<&'static str, usize> {
        let mut counts = BTreeMap::new();
        for s in &self.skipped {
            *counts.entry(s.reason.kind()).or_default() += 1;
        }
        counts
    }
}

struct Option_ {
    candidate: usize,
    matched_id: String,
    rank: usize,
    reason: Reason,
}

/// Decides which candidates to install on which present devices.
pub fn plan(devices: &[Device], candidates: &[Candidate], policy: &Policy) -> Plan {
    let mut out = Plan::default();
    let skip = |c: &Candidate, device: Option<&Device>, reason| Skipped {
        source: c.source,
        id: c.id.clone(),
        title: c.title.clone(),
        device: device.map(|d| d.instance_id.clone()),
        reason,
    };

    let present: Vec<&Device> = devices.iter().filter(|d| d.status.present).collect();
    let mut options: Vec<Vec<Option_>> = present.iter().map(|_| Vec::new()).collect();
    let mut oem_covered = vec![false; present.len()];

    for (ci, c) in candidates.iter().enumerate() {
        if c.kind != EntryKind::Driver {
            out.skipped
                .push(skip(c, None, SkipReason::NotADriver { kind: c.kind }));
            continue;
        }
        if c.hardware_ids.is_empty() {
            out.skipped.push(skip(c, None, SkipReason::NoHardwareIds));
            continue;
        }
        let mut matched = false;
        for (di, d) in present.iter().enumerate() {
            let Some((rank, id)) = d
                .all_ids()
                .enumerate()
                .find(|(_, id)| c.hardware_ids.iter().any(|h| h.eq_ignore_ascii_case(id)))
            else {
                continue;
            };
            matched = true;
            if c.source.is_oem() {
                oem_covered[di] = true;
            }
            // OEM categories are free text; only Windows Update reports real class names.
            let class = d.class.as_deref().or(match c.source {
                Source::WindowsUpdate => c.class.as_deref(),
                _ => None,
            });
            if let Some(reason) = policy.class_verdict(class, d.class_guid.as_deref()) {
                out.skipped.push(skip(c, Some(d), reason));
                continue;
            }
            match compare(d, c) {
                Ok(reason) => options[di].push(Option_ {
                    candidate: ci,
                    matched_id: id.to_string(),
                    rank,
                    reason,
                }),
                Err(reason) => out.skipped.push(skip(c, Some(d), reason)),
            }
        }
        if !matched {
            out.skipped
                .push(skip(c, None, SkipReason::NoMatchingDevice));
        }
    }

    // Candidate index -> devices it was chosen for; keeps candidate order.
    let mut chosen: BTreeMap<usize, Vec<DeviceUpdate>> = BTreeMap::new();
    for (di, opts) in options.into_iter().enumerate() {
        let d = present[di];
        let oem_first = oem_covered[di]
            && (policy.oem_exclusive
                || opts.iter().any(|o| candidates[o.candidate].source.is_oem()));
        let (opts, rejected): (Vec<_>, Vec<_>) = opts
            .into_iter()
            .partition(|o| !oem_first || candidates[o.candidate].source.is_oem());
        for o in rejected {
            out.skipped.push(skip(
                &candidates[o.candidate],
                Some(d),
                SkipReason::OemPreferred,
            ));
        }

        let better = |a: &Option_, b: &Option_| -> Ordering {
            let (ca, cb) = (&candidates[a.candidate], &candidates[b.candidate]);
            b.rank
                .cmp(&a.rank)
                .then_with(|| ca.version().cmp(&cb.version()))
                .then_with(|| ca.driver_date.cmp(&cb.driver_date))
        };
        let Some(best) = opts.iter().max_by(|a, b| better(a, b)).map(|o| o.candidate) else {
            continue;
        };
        for o in opts {
            if o.candidate != best {
                let by = format!("{} {}", candidates[best].source, candidates[best].id);
                out.skipped.push(skip(
                    &candidates[o.candidate],
                    Some(d),
                    SkipReason::BetterCandidate { by },
                ));
                continue;
            }
            let driver = d.driver.as_ref();
            chosen.entry(best).or_default().push(DeviceUpdate {
                instance_id: d.instance_id.clone(),
                name: d.name.clone(),
                class: d.class.clone(),
                hardware_ids: d.hardware_ids.clone(),
                matched_id: o.matched_id,
                rank: o.rank,
                installed_version: driver.and_then(|dr| dr.version.clone()),
                installed_date: driver.and_then(|dr| dr.date.clone()),
                reason: o.reason,
            });
        }
    }
    out.proposals = chosen
        .into_iter()
        .map(|(ci, devices)| Proposal {
            candidate: candidates[ci].clone(),
            devices,
        })
        .collect();
    out
}

/// First word of a provider name, so `Intel` and `Intel Corporation` agree.
fn same_vendor(a: &str, b: &str) -> bool {
    let first = |s: &str| {
        s.split(|c: char| !c.is_alphanumeric())
            .find(|w| !w.is_empty())
            .unwrap_or_default()
            .to_ascii_lowercase()
    };
    first(a) == first(b)
}

/// Whether `candidate` is newer than the driver of `device`.
fn compare(device: &Device, candidate: &Candidate) -> Result<Reason, SkipReason> {
    let Some(installed) = &device.driver else {
        return Ok(Reason::NoDriver);
    };
    // Version numbering is per vendor: across vendors only dates compare.
    let comparable = match (candidate.provider.as_deref(), installed.provider.as_deref()) {
        (Some(a), Some(b)) => same_vendor(a, b),
        _ => true,
    };
    let versions = match (candidate.version(), device.driver_version()) {
        (Some(offered), Some(current)) if comparable => Some((offered, current)),
        _ => None,
    };
    // `YYYY-MM-DD` orders as text.
    let dates = match (candidate.driver_date.as_deref(), installed.date.as_deref()) {
        (Some(offered), Some(current)) => Some((offered, current)),
        _ => None,
    };
    let date_order = dates.map(|(o, c)| o.cmp(c));

    match (versions, date_order) {
        (Some((offered, current)), _) if offered > current => Ok(Reason::NewerVersion),
        (Some((offered, current)), _) if offered < current => Err(SkipReason::Downgrade {
            installed: current.to_string(),
            offered: offered.to_string(),
        }),
        (Some(_), Some(Ordering::Greater)) => Ok(Reason::NewerDate),
        (Some(_), _) => Err(SkipReason::NotNewer),
        (None, Some(Ordering::Greater)) => Ok(Reason::NewerDate),
        (None, Some(Ordering::Less)) => {
            let (offered, current) = dates.unwrap();
            Err(SkipReason::Downgrade {
                installed: current.to_string(),
                offered: offered.to_string(),
            })
        }
        (None, Some(Ordering::Equal)) => Err(SkipReason::NotNewer),
        (None, None) => Err(SkipReason::UnknownVersion),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DeviceStatus, DriverInfo};

    const WIFI: &str = "PCI\\VEN_8086&DEV_7360&SUBSYS_58201028";

    fn device(id: &str, class: &str, provider: &str, version: &str, date: &str) -> Device {
        Device {
            instance_id: format!("{id}\\1"),
            name: Some(format!("{class} device")),
            hardware_ids: vec![
                format!("{id}&REV_01"),
                id.into(),
                "PCI\\VEN_8086&DEV_7360".into(),
            ],
            compatible_ids: vec!["PCI\\CC_0280".into()],
            class: Some(class.into()),
            class_guid: None,
            manufacturer: None,
            driver: Some(DriverInfo {
                description: None,
                provider: Some(provider.into()),
                version: Some(version.into()),
                date: Some(date.into()),
                inf_name: Some("oem1.inf".into()),
            }),
            status: DeviceStatus {
                present: true,
                hidden: false,
                problem_code: 0,
            },
        }
    }

    fn candidate(
        source: Source,
        id: &str,
        hwid: &str,
        version: Option<&str>,
        date: Option<&str>,
    ) -> Candidate {
        Candidate {
            source,
            id: id.into(),
            title: id.into(),
            kind: EntryKind::Driver,
            hardware_ids: vec![hwid.into()],
            class: None,
            provider: (source == Source::WindowsUpdate).then(|| "Intel".into()),
            model: None,
            driver_version: version.map(Into::into),
            driver_date: date.map(Into::into),
            release_date: None,
            package: None,
        }
    }

    fn reasons(plan: &Plan) -> Vec<(&str, &SkipReason)> {
        plan.skipped
            .iter()
            .map(|s| (s.id.as_str(), &s.reason))
            .collect()
    }

    #[test]
    fn oem_before_windows_update() {
        let devices = [device(WIFI, "Net", "Intel", "22.200.0.6", "2023-01-01")];
        let candidates = [
            candidate(
                Source::WindowsUpdate,
                "wu",
                WIFI,
                Some("22.260.0.1"),
                Some("2024-05-01"),
            ),
            candidate(
                Source::Dell,
                "dell",
                "PCI\\VEN_8086&DEV_7360",
                Some("22.250.0.3"),
                None,
            ),
        ];
        let plan = plan(&devices, &candidates, &Policy::default());
        assert_eq!(plan.proposals.len(), 1);
        let p = &plan.proposals[0];
        assert_eq!(p.candidate.id, "dell");
        assert_eq!(p.devices[0].reason, Reason::NewerVersion);
        assert_eq!(p.devices[0].matched_id, "PCI\\VEN_8086&DEV_7360");
        assert_eq!(p.devices[0].rank, 2);
        assert_eq!(reasons(&plan), [("wu", &SkipReason::OemPreferred)]);
    }

    #[test]
    fn oem_exclusive_blocks_wu_even_when_oem_is_not_newer() {
        let devices = [device(WIFI, "Net", "Intel", "22.250.0.3", "2023-01-01")];
        let candidates = [
            candidate(Source::Dell, "dell", WIFI, Some("22.250.0.3"), None),
            candidate(
                Source::WindowsUpdate,
                "wu",
                WIFI,
                Some("22.260.0.1"),
                Some("2024-05-01"),
            ),
        ];
        let strict = plan(&devices, &candidates, &Policy::default());
        assert!(strict.proposals.is_empty());
        assert_eq!(
            reasons(&strict),
            [
                ("dell", &SkipReason::NotNewer),
                ("wu", &SkipReason::OemPreferred)
            ]
        );

        let relaxed = Policy {
            oem_exclusive: false,
            ..Policy::default()
        };
        let plan = plan(&devices, &candidates, &relaxed);
        assert_eq!(plan.proposals[0].candidate.id, "wu");
    }

    #[test]
    fn never_downgrade() {
        let devices = [device(WIFI, "Net", "Intel", "23.0.0.1", "2024-01-01")];
        let candidates = [candidate(
            Source::WindowsUpdate,
            "wu",
            WIFI,
            Some("22.260.0.1"),
            Some("2024-05-01"),
        )];
        let plan = plan(&devices, &candidates, &Policy::default());
        assert!(plan.proposals.is_empty());
        assert_eq!(
            plan.skipped[0].reason,
            SkipReason::Downgrade {
                installed: "23.0.0.1".into(),
                offered: "22.260.0.1".into()
            }
        );
    }

    #[test]
    fn same_version_newer_date() {
        let devices = [device(WIFI, "Net", "Intel", "22.260.0.1", "2024-01-01")];
        let candidates = [candidate(
            Source::WindowsUpdate,
            "wu",
            WIFI,
            Some("22.260.0.1"),
            Some("2024-05-01"),
        )];
        let plan = plan(&devices, &candidates, &Policy::default());
        assert_eq!(plan.proposals[0].devices[0].reason, Reason::NewerDate);
    }

    #[test]
    fn other_vendor_compares_dates_only() {
        // Inbox Microsoft driver: high version, old date.
        let devices = [device(
            WIFI,
            "Net",
            "Microsoft",
            "10.0.26100.1",
            "2006-06-21",
        )];
        let candidates = [candidate(
            Source::WindowsUpdate,
            "wu",
            WIFI,
            Some("22.260.0.1"),
            Some("2024-05-01"),
        )];
        let plan = plan(&devices, &candidates, &Policy::default());
        assert_eq!(plan.proposals[0].devices[0].reason, Reason::NewerDate);
    }

    #[test]
    fn most_specific_match_wins() {
        let devices = [device(WIFI, "Net", "Intel", "22.0.0.1", "2023-01-01")];
        let candidates = [
            candidate(
                Source::WindowsUpdate,
                "generic",
                "PCI\\VEN_8086&DEV_7360",
                Some("22.300.0.1"),
                Some("2024-06-01"),
            ),
            candidate(
                Source::WindowsUpdate,
                "specific",
                WIFI,
                Some("22.260.0.1"),
                Some("2024-05-01"),
            ),
        ];
        let plan = plan(&devices, &candidates, &Policy::default());
        assert_eq!(plan.proposals[0].candidate.id, "specific");
        assert_eq!(plan.proposals[0].devices[0].rank, 1);
        assert_eq!(
            reasons(&plan),
            [(
                "generic",
                &SkipReason::BetterCandidate {
                    by: "wu specific".into()
                }
            )]
        );
    }

    #[test]
    fn class_lists_and_kinds() {
        let devices = [device(WIFI, "Net", "Intel", "22.0.0.1", "2023-01-01")];
        let mut bios = candidate(Source::Dell, "bios", WIFI, Some("1.2.0.0"), None);
        bios.kind = EntryKind::Bios;
        let candidates = [
            bios,
            candidate(
                Source::WindowsUpdate,
                "wu",
                WIFI,
                Some("22.260.0.1"),
                Some("2024-05-01"),
            ),
            candidate(
                Source::Hp,
                "none",
                "PCI\\VEN_10EC&DEV_8168",
                Some("1.0.0.0"),
                None,
            ),
        ];
        let deny = Policy {
            deny_classes: vec!["net".into()],
            ..Policy::default()
        };
        let denied = plan(&devices, &candidates, &deny);
        assert!(denied.proposals.is_empty());
        assert_eq!(
            reasons(&denied),
            [
                (
                    "bios",
                    &SkipReason::NotADriver {
                        kind: EntryKind::Bios
                    }
                ),
                (
                    "wu",
                    &SkipReason::ClassDenied {
                        class: "Net".into()
                    }
                ),
                ("none", &SkipReason::NoMatchingDevice),
            ]
        );

        let allow = Policy {
            allow_classes: vec!["Display".into()],
            ..Policy::default()
        };
        let plan = plan(&devices, &candidates[1..2], &allow);
        assert_eq!(
            plan.skipped[0].reason,
            SkipReason::ClassNotAllowed {
                class: "Net".into()
            }
        );
        assert_eq!(plan.skipped_by_reason()["class not allowed"], 1);
    }

    #[test]
    fn device_without_driver_and_unknown_versions() {
        let mut bare = device(WIFI, "Net", "Intel", "1.0.0.0", "2020-01-01");
        bare.driver = None;
        let other = device(
            "PCI\\VEN_10EC&DEV_8168&SUBSYS_00011028",
            "Net",
            "Realtek",
            "10.1.0.0",
            "2022-01-01",
        );
        let candidates = [
            candidate(Source::Lenovo, "wifi", WIFI, None, None),
            candidate(
                Source::Lenovo,
                "lan",
                "PCI\\VEN_10EC&DEV_8168&SUBSYS_00011028",
                None,
                None,
            ),
        ];
        let plan = plan(&[bare, other], &candidates, &Policy::default());
        assert_eq!(plan.proposals.len(), 1);
        assert_eq!(plan.proposals[0].devices[0].reason, Reason::NoDriver);
        assert_eq!(reasons(&plan), [("lan", &SkipReason::UnknownVersion)]);
    }

    #[test]
    fn policy_json() {
        let p = Policy::from_json(r#"{"deny_classes":["Display"]}"#).unwrap();
        assert!(p.oem_exclusive);
        assert_eq!(p.deny_classes, ["Display"]);
        assert!(Policy::from_json(r#"{"deny":["x"]}"#).is_err());
    }
}
