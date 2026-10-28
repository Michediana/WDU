//! Italian labels for the service's codes, and dates in local time.

use wdu_core::driver_store::{DriverPackage, PackageUse};
use wdu_core::engine::{ScanStep, StepKind};
use wdu_core::history::Outcome;
use wdu_core::ipc::ServiceState;
use wdu_core::matching::{Reason, SkipReason};
use wdu_core::provider::Source;

pub fn source(source: Source) -> &'static str {
    match source {
        Source::Dell => "Dell",
        Source::Hp => "HP",
        Source::Lenovo => "Lenovo",
        Source::WindowsUpdate => "Windows Update",
    }
}

/// Source names in [`wdu_core::engine::SourceError`], which are free strings.
pub fn source_name(name: &str) -> &str {
    match name {
        "windows_update" | "wu" => "Windows Update",
        "oem" => "Catalogo del produttore",
        "dell" => "Dell",
        "hp" => "HP",
        "lenovo" => "Lenovo",
        other => other,
    }
}

pub fn state(state: ServiceState) -> &'static str {
    match state {
        ServiceState::Idle => "Inattivo",
        ServiceState::Scanning => "Ricerca in corso…",
        ServiceState::Installing => "Installazione in corso…",
        ServiceState::Removing => "Rimozione dei driver vecchi…",
    }
}

/// What a scan step does, and where.
pub fn scan_step(step: &ScanStep) -> (String, String) {
    let place = |default: &str| step.place.clone().unwrap_or_else(|| default.into());
    match step.kind {
        StepKind::Machine => ("Identificazione del PC".into(), "questo PC".into()),
        StepKind::Devices => (
            "Elenco dei dispositivi".into(),
            "Gestione dispositivi".into(),
        ),
        StepKind::Search {
            source: Some(Source::WindowsUpdate),
        } => (
            "Ricerca su Windows Update".into(),
            match &step.place {
                Some(server) => format!("server WSUS {server}"),
                None => "server Microsoft".into(),
            },
        ),
        StepKind::Search {
            source: Some(source),
        } => (
            format!("Catalogo driver {}", self::source(source)),
            place("sito del produttore"),
        ),
        StepKind::Search { source: None } => (
            "Catalogo del produttore".into(),
            place("sito del produttore"),
        ),
        StepKind::Matching => (
            "Confronto con i driver installati".into(),
            "questo PC".into(),
        ),
    }
}

/// What a finished step found: `182 dispositivi`, `12 driver offerti`.
pub fn scan_found(step: &ScanStep) -> Option<String> {
    let n = step.found?;
    Some(match step.kind {
        StepKind::Devices if n == 1 => "1 dispositivo".into(),
        StepKind::Devices => format!("{n} dispositivi"),
        StepKind::Search { .. } if n == 1 => "1 driver offerto".into(),
        StepKind::Search { .. } => format!("{n} driver offerti"),
        StepKind::Matching if n == 1 => "1 aggiornamento proposto".into(),
        StepKind::Matching => format!("{n} aggiornamenti proposti"),
        StepKind::Machine => return None,
    })
}

pub fn reason(reason: Reason) -> &'static str {
    match reason {
        Reason::NewerVersion => "versione più recente",
        Reason::NewerDate => "driver più recente",
        Reason::NoDriver => "nessun driver installato",
    }
}

/// Short form, used to count skipped candidates.
pub fn skip_kind(reason: &SkipReason) -> &'static str {
    match reason {
        SkipReason::NotADriver { .. } => "non è un driver (BIOS, firmware, applicazione)",
        SkipReason::NoHardwareIds => "il catalogo non indica i dispositivi",
        SkipReason::NoMatchingDevice => "nessun dispositivo corrispondente",
        SkipReason::ClassDenied { .. } => "classe esclusa",
        SkipReason::ClassNotAllowed { .. } => "classe non consentita",
        SkipReason::NotNewer => "già aggiornato",
        SkipReason::Downgrade { .. } => "sarebbe un downgrade",
        SkipReason::UnknownVersion => "versione non confrontabile",
        SkipReason::OemPreferred => "preferito il catalogo del produttore",
        SkipReason::BetterCandidate { .. } => "c'è un aggiornamento più adatto",
    }
}

pub fn skip(reason: &SkipReason) -> String {
    let kind = skip_kind(reason);
    match reason {
        SkipReason::Downgrade { installed, offered } => format!("{kind} ({installed} → {offered})"),
        SkipReason::ClassDenied { class } | SkipReason::ClassNotAllowed { class } => {
            format!("{kind} ({class})")
        }
        _ => kind.to_string(),
    }
}

pub fn outcome(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Succeeded => "Installato",
        Outcome::Failed => "Non riuscito",
        Outcome::RolledBack => "Non riuscito, driver precedente ripristinato",
    }
}

pub fn package_use(package: &DriverPackage) -> String {
    let n = package.devices.len();
    match package.usage() {
        PackageUse::InUse if n == 1 => "1 dispositivo".into(),
        PackageUse::InUse => format!("{n} dispositivi"),
        PackageUse::AbsentDevices => "solo dispositivi scollegati".into(),
        PackageUse::NoDevice => "nessun dispositivo".into(),
    }
}

/// Bytes with the decimal comma: `74,5 MB`.
pub fn size(bytes: u64) -> String {
    let (value, unit) = match bytes {
        b if b >= 1 << 30 => (b as f64 / (1u64 << 30) as f64, "GB"),
        b if b >= 1 << 20 => (b as f64 / (1u64 << 20) as f64, "MB"),
        b => (b as f64 / 1024.0, "KB"),
    };
    format!("{value:.1} {unit}").replace('.', ",")
}

const MONTHS: [&str; 12] = [
    "gen", "feb", "mar", "apr", "mag", "giu", "lug", "ago", "set", "ott", "nov", "dic",
];

/// An RFC 3339 UTC timestamp from the service, in local time:
/// `26 ott 2026, 14:05`.
pub fn when(rfc3339: Option<&str>) -> String {
    let Some(text) = rfc3339 else {
        return "–".into();
    };
    match parse_utc(text).and_then(to_local) {
        Some((y, mo, d, h, mi)) => format!("{d} {} {y}, {h:02}:{mi:02}", MONTHS[mo as usize - 1]),
        None => text.to_string(),
    }
}

/// A `YYYY-MM-DD` driver date: `21/6/2006`.
pub fn day(date: &str) -> String {
    let parts: Vec<u32> = date
        .get(..10)
        .unwrap_or(date)
        .split('-')
        .filter_map(|p| p.parse().ok())
        .collect();
    match parts[..] {
        [y, m, d] => format!("{d}/{m}/{y}"),
        _ => date.to_string(),
    }
}

type DateTime = (u16, u16, u16, u16, u16);

/// `YYYY-MM-DDTHH:MM[:SS...]Z` → (year, month, day, hour, minute).
fn parse_utc(text: &str) -> Option<DateTime> {
    let num = |range: std::ops::Range<usize>| text.get(range)?.parse::<u16>().ok();
    let date = (
        num(0..4)?,
        num(5..7)?,
        num(8..10)?,
        num(11..13)?,
        num(14..16)?,
    );
    (1..=12).contains(&date.1).then_some(date)
}

#[cfg(windows)]
fn to_local((year, month, day, hour, minute): DateTime) -> Option<DateTime> {
    use windows::Win32::Foundation::SYSTEMTIME;
    use windows::Win32::System::Time::SystemTimeToTzSpecificLocalTime;

    let utc = SYSTEMTIME {
        wYear: year,
        wMonth: month,
        wDay: day,
        wHour: hour,
        wMinute: minute,
        ..Default::default()
    };
    let mut local = SYSTEMTIME::default();
    unsafe { SystemTimeToTzSpecificLocalTime(None, &utc, &mut local) }.ok()?;
    Some((
        local.wYear,
        local.wMonth,
        local.wDay,
        local.wHour,
        local.wMinute,
    ))
}

#[cfg(not(windows))]
fn to_local(utc: DateTime) -> Option<DateTime> {
    Some(utc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(day("2006-06-21"), "21/6/2006");
        assert_eq!(day("boh"), "boh");
        assert_eq!(
            parse_utc("2026-10-26T13:05:00Z"),
            Some((2026, 10, 26, 13, 5))
        );
        assert_eq!(parse_utc("2026-13-26T13:05:00Z"), None);
        assert_eq!(when(None), "–");
        assert_eq!(when(Some("x")), "x");
        assert_eq!(size(512), "0,5 KB");
        assert_eq!(size(78_119_731), "74,5 MB");
        assert_eq!(size(3 << 30), "3,0 GB");
    }
}
