//! `parseTime` with the 8.64e12-seconds quirk, plus ISO helpers.
//!
//! Ports `parseTime` from `src/data.js` and the backup-filename timestamp
//! encoding from `src/backup.js`.
//!
//! Finite numerics (including numeric strings, via full JS `Number()`
//! coercion — trimming, hex/octal/binary prefixes, exponent notation) are
//! accepted VERBATIM as epoch seconds iff `0 <= v <= 8.64e12` — 8.64e12 is JS
//! Date's max range in MILLISECONDS, reused by the JS as a SECONDS bound, so
//! millisecond-magnitude timestamps like 1700000000000 are deliberately
//! misread as seconds. Reproduced exactly for parity. Negative numerics fall
//! through to date parsing.
//!
//! Non-numeric strings go through a documented NARROWING of `Date.parse`:
//! only RFC 3339 date-times (offset required, seconds required) and bare
//! `YYYY-MM-DD` dates (UTC midnight, per the ES date-only form) are accepted.
//! Exotic V8 legacy-parser formats such as `Jul 19 2026`, `2026/07/19`,
//! date-times WITHOUT an offset (which Node reads in local time), and bare
//! negative numbers (Node reads `'-1'` as local Jan 2001!) are NOT supported
//! and yield the standard error.

use crate::jsnum::js_number;
use serde_json::Value;

/// JS Date's range limit in milliseconds (`8.64e15`), and — quirk — the JS
/// code's bound for "verbatim epoch seconds" is this value / 1000 = `8.64e12`.
const MAX_DATE_MS: f64 = 8.64e15;
const MAX_VERBATIM_SECONDS: f64 = 8.64e12;

/// Port of `parseTime(value, name)` from `src/data.js`.
///
/// * `None` / `Some("")` -> `Ok(None)` (JS `undefined`/`null`/`''`).
/// * `Number(value)` finite and in `[0, 8.64e12]` -> returned verbatim as
///   epoch seconds (see module docs for the quirk).
/// * Otherwise the `Date.parse` subset (RFC 3339, bare `YYYY-MM-DD` as UTC
///   midnight), returning milliseconds / 1000.
/// * Failure -> `Err` with EXACTLY `<name> must be a Unix timestamp or ISO
///   date`.
pub fn parse_time(v: Option<&str>, name: &str) -> Result<Option<f64>, String> {
    let s = match v {
        None | Some("") => return Ok(None),
        Some(s) => s,
    };
    let numeric = js_number(&Value::String(s.to_string()));
    if numeric.is_finite() && (0.0..=MAX_VERBATIM_SECONDS).contains(&numeric) {
        return Ok(Some(numeric));
    }
    match date_parse_subset_ms(s) {
        Some(ms) => Ok(Some(ms / 1000.0)),
        None => Err(format!("{name} must be a Unix timestamp or ISO date")),
    }
}

/// `Date.parse` subset -> epoch milliseconds. `None` where Node would give
/// NaN (or where the format is outside the documented subset).
fn date_parse_subset_ms(s: &str) -> Option<f64> {
    // Bare YYYY-MM-DD: the ES date-only form, interpreted as UTC midnight.
    if let Some(days) = parse_bare_date_days(s) {
        return Some(days as f64 * 86_400_000.0);
    }
    // RFC 3339 date-time (chrono accepts lowercase 't'/'z' per the RFC; JS
    // milli precision: extra fractional digits are truncated to ms).
    let dt = chrono::DateTime::parse_from_rfc3339(s).ok()?;
    let ms = dt.timestamp_millis() as f64;
    if ms.abs() > MAX_DATE_MS {
        return None; // JS Date range check (unreachable via chrono's year range)
    }
    Some(ms)
}

/// Strict `YYYY-MM-DD` (exactly 4-2-2 digits) -> days since 1970-01-01, or
/// `None`. Calendar-validated: `2026-02-30` is rejected like `Date.parse`.
fn parse_bare_date_days(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let digits = |r: std::ops::Range<usize>| -> Option<i64> {
        let mut n: i64 = 0;
        for &c in &b[r] {
            if !c.is_ascii_digit() {
                return None;
            }
            n = n * 10 + i64::from(c - b'0');
        }
        Some(n)
    };
    let year = digits(0..4)?;
    let month = digits(5..7)?;
    let day = digits(8..10)?;
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    Some(days_from_civil(year, month, day))
}

fn is_leap_year(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Proleptic-Gregorian civil date -> days since 1970-01-01 (Howard Hinnant's
/// `days_from_civil`). Valid over the full JS Date year range (±271821..),
/// which exceeds chrono's — needed for the 8.64e12-seconds boundary where
/// `toISOString()` reaches year +275760.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 }; // [0, 11], March-based
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`]: days since epoch -> (year, month, day).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// JS `new Date(ms).toISOString()`: `YYYY-MM-DDTHH:MM:SS.mmmZ`, with the
/// expanded-year form `±YYYYYY-...` outside years 0..=9999 (ES
/// "extended years"). JS throws a RangeError beyond ±8.64e15 ms; since our
/// callers are range-guarded upstream, this defensively clamps instead.
fn js_to_iso_string(epoch_ms: i64) -> String {
    let ms = epoch_ms.clamp(-(MAX_DATE_MS as i64), MAX_DATE_MS as i64);
    let days = ms.div_euclid(86_400_000);
    let tod = ms.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    let (hh, mm, ss, mmm) = (tod / 3_600_000, tod / 60_000 % 60, tod / 1000 % 60, tod % 1000);
    let year_str = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else {
        // ES expanded year: sign + six digits ("+275760", "-271821").
        format!("{}{:06}", if year < 0 { '-' } else { '+' }, year.abs())
    };
    format!("{year_str}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}.{mmm:03}Z")
}

/// `new Date(epoch_s * 1000).toISOString().slice(0, 10)` — UTC day bucket.
///
/// The multiply happens in f64 and `new Date(number)` truncates toward zero
/// (ToIntegerOrInfinity), both reproduced here. Non-finite input (which would
/// make JS throw) defensively maps NaN -> epoch and ±inf to the clamp bounds.
/// Note that for expanded-year dates the JS `slice(0, 10)` cuts mid-field
/// (`"+275760-09"`); reproduced verbatim.
pub fn iso_date_utc(epoch_s: f64) -> String {
    let ms_f = epoch_s * 1000.0;
    let ms = if ms_f.is_nan() {
        0
    } else if ms_f >= MAX_DATE_MS {
        MAX_DATE_MS as i64
    } else if ms_f <= -MAX_DATE_MS {
        -(MAX_DATE_MS as i64)
    } else {
        ms_f.trunc() as i64
    };
    js_to_iso_string(ms).chars().take(10).collect()
}

/// `new Date(epoch_ms).toISOString().replace(/[:.]/g, '-')` — backup filename
/// timestamps (`src/backup.js`), e.g. `2026-07-19T12-34-56-789Z`.
pub fn iso_ts_for_filename(epoch_ms: i64) -> String {
    js_to_iso_string(epoch_ms)
        .chars()
        .map(|c| if c == ':' || c == '.' { '-' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ERR: &str = "t must be a Unix timestamp or ISO date";

    #[test]
    fn empty_and_none_are_null() {
        assert_eq!(parse_time(None, "t"), Ok(None));
        assert_eq!(parse_time(Some(""), "t"), Ok(None));
    }

    #[test]
    fn numeric_strings_verbatim_as_seconds() {
        assert_eq!(parse_time(Some("0"), "t"), Ok(Some(0.0)));
        assert_eq!(parse_time(Some("1700000000"), "t"), Ok(Some(1_700_000_000.0)));
        // The quirk: millisecond-magnitude numbers are misread as seconds.
        assert_eq!(
            parse_time(Some("1700000000000"), "t"),
            Ok(Some(1_700_000_000_000.0))
        );
        // JS Number() coercion niceties.
        assert_eq!(parse_time(Some("  10.5  "), "t"), Ok(Some(10.5)));
        assert_eq!(parse_time(Some("1e9"), "t"), Ok(Some(1e9)));
        assert_eq!(parse_time(Some("0x10"), "t"), Ok(Some(16.0)));
    }

    #[test]
    fn boundary_8_64e12() {
        // 8.64e12 passes verbatim...
        assert_eq!(parse_time(Some("8640000000000"), "t"), Ok(Some(8.64e12)));
        // ...one less passes...
        assert_eq!(
            parse_time(Some("8639999999999"), "t"),
            Ok(Some(8_639_999_999_999.0))
        );
        // ...one more falls through to Date.parse and fails.
        assert_eq!(parse_time(Some("8640000000001"), "t"), Err(ERR.to_string()));
    }

    #[test]
    fn negative_numbers_fall_through_and_fail() {
        // JS: numeric >= 0 guard, so negatives hit Date.parse. Node's exotic
        // legacy parser reads '-1' as local Jan 2001 — one of the documented
        // narrowings: our subset rejects it instead.
        assert_eq!(parse_time(Some("-1"), "t"), Err(ERR.to_string()));
        // -0 is >= 0 and passes verbatim.
        assert_eq!(parse_time(Some("-0"), "t"), Ok(Some(0.0)));
    }

    #[test]
    fn bare_date_is_utc_midnight() {
        assert_eq!(parse_time(Some("1970-01-01"), "t"), Ok(Some(0.0)));
        assert_eq!(parse_time(Some("2026-07-19"), "t"), Ok(Some(1_784_419_200.0)));
        // Calendar-invalid dates are NaN in Date.parse too.
        assert_eq!(parse_time(Some("2026-02-30"), "t"), Err(ERR.to_string()));
        assert_eq!(parse_time(Some("2026-13-01"), "t"), Err(ERR.to_string()));
        // Leap day.
        assert_eq!(parse_time(Some("2024-02-29"), "t"), Ok(Some(1_709_164_800.0)));
        assert_eq!(parse_time(Some("2023-02-29"), "t"), Err(ERR.to_string()));
    }

    #[test]
    fn rfc3339_datetimes() {
        assert_eq!(
            parse_time(Some("2026-07-19T00:00:00Z"), "t"),
            Ok(Some(1_784_419_200.0))
        );
        assert_eq!(
            parse_time(Some("2026-07-19T02:00:00+02:00"), "t"),
            Ok(Some(1_784_419_200.0))
        );
        assert_eq!(parse_time(Some("1970-01-01T00:00:00.500Z"), "t"), Ok(Some(0.5)));
    }

    #[test]
    fn documented_narrowing_of_date_parse() {
        // Node accepts these; our subset deliberately does not.
        assert_eq!(parse_time(Some("Jul 19 2026"), "t"), Err(ERR.to_string()));
        assert_eq!(parse_time(Some("2026/07/19"), "t"), Err(ERR.to_string()));
        // Offset-less date-times are local time in Node — unsupported.
        assert_eq!(parse_time(Some("2026-07-19T12:00:00"), "t"), Err(ERR.to_string()));
        assert_eq!(parse_time(Some("garbage"), "t"), Err(ERR.to_string()));
    }

    #[test]
    fn error_message_uses_name() {
        assert_eq!(
            parse_time(Some("nope"), "before"),
            Err("before must be a Unix timestamp or ISO date".to_string())
        );
    }

    #[test]
    fn iso_date_utc_basics() {
        assert_eq!(iso_date_utc(0.0), "1970-01-01");
        assert_eq!(iso_date_utc(1_784_419_200.0), "2026-07-19");
        // End of the previous UTC day.
        assert_eq!(iso_date_utc(-1.0), "1969-12-31");
        // ToInteger truncation toward zero: -0.5 ms -> 0 ms.
        assert_eq!(iso_date_utc(-0.0005), "1970-01-01");
        assert_eq!(iso_date_utc(1_784_419_200.999), "2026-07-19");
    }

    #[test]
    fn iso_date_utc_expanded_years() {
        // new Date(8.64e15).toISOString() === "+275760-09-13T00:00:00.000Z";
        // slice(0, 10) cuts mid-field. Verbatim parity.
        assert_eq!(iso_date_utc(8.64e12), "+275760-09");
        // new Date(-8.64e15).toISOString() === "-271821-04-20T00:00:00.000Z".
        assert_eq!(iso_date_utc(-8.64e12), "-271821-04");
    }

    #[test]
    fn full_iso_string_shape() {
        assert_eq!(js_to_iso_string(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(js_to_iso_string(1_784_419_200_123), "2026-07-19T00:00:00.123Z");
        assert_eq!(js_to_iso_string(-1), "1969-12-31T23:59:59.999Z");
        assert_eq!(
            js_to_iso_string(8_640_000_000_000_000),
            "+275760-09-13T00:00:00.000Z"
        );
        assert_eq!(
            js_to_iso_string(-8_640_000_000_000_000),
            "-271821-04-20T00:00:00.000Z"
        );
    }

    #[test]
    fn filename_timestamp_encoding() {
        assert_eq!(iso_ts_for_filename(0), "1970-01-01T00-00-00-000Z");
        assert_eq!(iso_ts_for_filename(1_784_419_200_123), "2026-07-19T00-00-00-123Z");
    }

    #[test]
    fn civil_roundtrip() {
        for &d in &[-1_000_000i64, -1, 0, 1, 20_000, 100_000, 1_000_000] {
            let (y, m, day) = civil_from_days(d);
            assert_eq!(days_from_civil(y, m as i64, day as i64), d);
        }
    }
}
