//! Minimal date conversions for the timestamp formats Windows APIs return.

use std::time::{SystemTime, UNIX_EPOCH};

/// 100 ns intervals between 1601-01-01 (FILETIME epoch) and 1970-01-01.
const FILETIME_UNIX_OFFSET: u64 = 116_444_736_000_000_000;
/// Days between 1899-12-30 (OLE Automation DATE epoch) and 1970-01-01.
const OLE_UNIX_OFFSET_DAYS: f64 = 25_569.0;

/// Converts days since 1970-01-01 to a proleptic Gregorian (year, month, day).
/// Howard Hinnant's `civil_from_days`.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

fn format_date(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// FILETIME (100 ns since 1601) to `YYYY-MM-DD`.
pub fn filetime_to_date(filetime: u64) -> Option<String> {
    let secs = filetime.checked_sub(FILETIME_UNIX_OFFSET)? / 10_000_000;
    Some(format_date((secs / 86_400) as i64))
}

/// OLE Automation DATE (days since 1899-12-30) to `YYYY-MM-DD`.
pub fn ole_date_to_date(date: f64) -> Option<String> {
    if !date.is_finite() || date <= 0.0 {
        return None;
    }
    Some(format_date((date - OLE_UNIX_OFFSET_DAYS).floor() as i64))
}

pub fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (date, rem) = (secs / 86_400, secs % 86_400);
    format!(
        "{}T{:02}:{:02}:{:02}Z",
        format_date(date as i64),
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn filetime_and_ole() {
        // 2006-06-21, the date of most inbox drivers.
        assert_eq!(
            filetime_to_date(127_953_216_000_000_000).as_deref(),
            Some("2006-06-21")
        );
        assert_eq!(filetime_to_date(0), None);
        assert_eq!(ole_date_to_date(45_292.0).as_deref(), Some("2024-01-01"));
        assert_eq!(ole_date_to_date(0.0), None);
    }
}
