//! Proleptic-Gregorian calendar arithmetic, shared by the wire codec, the
//! planner's constant folding and the executor's date functions.
//!
//! Howard Hinnant's day-number algorithms, used instead of pulling in a
//! date crate for the one type that needs calendar math.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::{DataType, FerriteError, Value};

/// Microseconds since the Unix epoch, UTC.
pub fn now_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// Days from the Unix epoch to `y-m-d`, negative before 1970.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The inverse of [`days_from_civil`].
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Split microseconds since the epoch into `(year, month, day, hour,
/// minute, second, microsecond)`, UTC.
pub fn parts_from_micros(micros: i64) -> (i64, u32, u32, i64, i64, i64, i64) {
    let days = micros.div_euclid(86_400_000_000);
    let rem = micros.rem_euclid(86_400_000_000);
    let (y, m, d) = civil_from_days(days);
    (
        y,
        m,
        d,
        rem / 3_600_000_000,
        rem / 60_000_000 % 60,
        rem / 1_000_000 % 60,
        rem % 1_000_000,
    )
}

/// `YYYY-MM-DD HH:MM:SS`, UTC, no zone suffix and no fractional part —
/// the exact shape SQLite's `datetime()` returns.
pub fn format_sqlite_datetime(micros: i64) -> String {
    let (y, m, d, h, min, s, _) = parts_from_micros(micros);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{min:02}:{s:02}")
}

/// `YYYY-MM-DD`, UTC — the shape SQLite's `date()` returns.
pub fn format_sqlite_date(micros: i64) -> String {
    let (y, m, d, ..) = parts_from_micros(micros);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Parse `YYYY-MM-DD` followed by `T` or a space and
/// `HH:MM[:SS[.ffffff]]`, optionally suffixed by `Z` or `+HH`, into
/// microseconds since the epoch, UTC.
///
/// The time component is required — this is what the wire codec accepts
/// for a `TIMESTAMP` parameter. Use [`parse_datetime_text`] for the looser
/// grammar SQLite's date functions take.
pub fn parse_timestamp(s: &str) -> Option<i64> {
    let (date, rest) = s.trim().split_once([' ', 'T'])?;
    parse_parts(date, rest)
}

/// Like [`parse_timestamp`], but a bare `YYYY-MM-DD` is accepted and reads
/// as midnight UTC — the shape SQLite's `date()`/`datetime()` also take.
pub fn parse_datetime_text(s: &str) -> Option<i64> {
    let s = s.trim();
    match s.split_once([' ', 'T']) {
        Some((date, rest)) => parse_parts(date, rest),
        None => parse_parts(s, ""),
    }
}

fn parse_parts(date: &str, rest: &str) -> Option<i64> {
    let mut dparts = date.split('-');
    let y: i64 = dparts.next()?.parse().ok()?;
    let m: u32 = dparts.next()?.parse().ok()?;
    let d: u32 = dparts.next()?.parse().ok()?;
    if dparts.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    if rest.is_empty() {
        return Some(days_from_civil(y, m, d) * 86_400_000_000);
    }
    let time = rest
        .split_once('+')
        .map(|(t, _)| t)
        .unwrap_or(rest)
        .trim_end_matches('Z');
    let mut tparts = time.split(':');
    let h: i64 = tparts.next()?.parse().ok()?;
    let min: i64 = tparts.next()?.parse().ok()?;
    let (sec, frac) = match tparts.next() {
        Some(sec) => match sec.split_once('.') {
            Some((whole, frac)) => (whole.parse::<i64>().ok()?, {
                let f: String = frac.chars().take(6).collect();
                f.parse::<i64>().ok()? * 10i64.pow(6 - f.len() as u32)
            }),
            None => (sec.parse::<i64>().ok()?, 0),
        },
        None => (0, 0),
    };
    if tparts.next().is_some() || h > 23 || min > 59 || sec > 60 {
        return None;
    }
    let days = days_from_civil(y, m, d);
    Some((days * 86_400 + h * 3_600 + min * 60 + sec) * 1_000_000 + frac)
}

/// Apply one SQLite date modifier, e.g. `-30 days`, `+1 hour`, `-5 minutes`.
///
/// Only the `NNN unit` offsets PawChat uses are recognised. `start of …`,
/// `weekday N`, `unixepoch`, `localtime` and `utc` return `None` rather
/// than being silently ignored, because ignoring a modifier changes which
/// rows a query returns.
pub fn apply_modifier(micros: i64, modifier: &str) -> Option<i64> {
    let modifier = modifier.trim();
    let (amount, unit) = modifier.split_once(char::is_whitespace)?;
    let amount: i64 = amount.parse().ok()?;
    let unit = unit.trim().trim_end_matches('s');
    let scale = match unit {
        "second" => 1_000_000,
        "minute" => 60_000_000,
        "hour" => 3_600_000_000,
        "day" => 86_400_000_000,
        "month" | "year" => {
            let months = if unit == "year" { amount * 12 } else { amount };
            return Some(add_months(micros, months));
        }
        _ => return None,
    };
    amount
        .checked_mul(scale)
        .and_then(|d| micros.checked_add(d))
}

/// Calendar-aware month arithmetic: the day of month is clamped to the
/// target month's length, as SQLite does not do.
///
/// SQLite normalises `2026-01-31` plus one month to `2026-03-03`; this
/// clamps to `2026-02-28`, which is what every other engine does and what
/// a reader expects. No PawChat query uses a month or year modifier, so
/// the divergence is unreachable there — it is documented rather than left
/// implicit.
fn add_months(micros: i64, months: i64) -> i64 {
    let (y, m, d, h, min, s, us) = parts_from_micros(micros);
    let total = y * 12 + (m as i64 - 1) + months;
    let (ny, nm) = (total.div_euclid(12), total.rem_euclid(12) as u32 + 1);
    let d = d.min(days_in_month(ny, nm));
    (days_from_civil(ny, nm, d) * 86_400 + h * 3_600 + min * 60 + s) * 1_000_000 + us
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) => 29,
        _ => 28,
    }
}

/// `datetime(when, modifier…)` when `with_time`, `date(when, modifier…)`
/// otherwise — SQLite's semantics, rendered in SQLite's shape.
///
/// `when` is the literal `'now'`, a text timestamp, or a `TIMESTAMP`.
/// Every modifier must be one [`apply_modifier`] understands: an
/// unrecognised modifier is an error rather than a silently skipped
/// clause, because skipping one changes which rows a query returns.
///
/// This lives here rather than in the executor because the planner folds
/// `datetime('now')` to a literal once per statement — which is what makes
/// it constant across rows, as it is in SQLite.
pub fn eval_datetime(args: &[Value], with_time: bool) -> Result<Value, FerriteError> {
    let mismatch = |value: &Value| FerriteError::TypeMismatch {
        expected: DataType::Text,
        actual: value.data_type().expect("callers match Null first"),
    };
    let mut micros = match args.first() {
        None | Some(Value::Null) => return Ok(Value::Null),
        Some(Value::Timestamp(us)) => *us,
        Some(Value::Text(s)) if s.eq_ignore_ascii_case("now") => now_micros(),
        Some(Value::Text(s)) => parse_datetime_text(s)
            .ok_or_else(|| FerriteError::Exec(format!("`{s}` is not a valid date/time")))?,
        Some(other) => return Err(mismatch(other)),
    };
    for modifier in &args[1..] {
        let modifier = match modifier {
            Value::Null => return Ok(Value::Null),
            Value::Text(modifier) => modifier,
            other => return Err(mismatch(other)),
        };
        micros = apply_modifier(micros, modifier)
            .ok_or_else(|| FerriteError::Exec(format!("unsupported date modifier `{modifier}`")))?;
    }
    Ok(Value::Text(match with_time {
        true => format_sqlite_datetime(micros),
        false => format_sqlite_date(micros),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trips_across_a_leap_boundary() {
        for (y, m, d) in [(1970, 1, 1), (2024, 2, 29), (2026, 8, 10), (1899, 12, 31)] {
            assert_eq!(civil_from_days(days_from_civil(y, m, d)), (y, m, d));
        }
    }

    #[test]
    fn sqlite_shapes_have_no_zone_and_no_fraction() {
        let micros = days_from_civil(2026, 8, 10) * 86_400_000_000 + 12 * 3_600_000_000 + 999;
        assert_eq!(format_sqlite_datetime(micros), "2026-08-10 12:00:00");
        assert_eq!(format_sqlite_date(micros), "2026-08-10");
    }

    #[test]
    fn a_date_only_text_timestamp_parses_at_midnight_only_in_the_loose_grammar() {
        assert_eq!(
            parse_datetime_text("2026-08-10"),
            Some(days_from_civil(2026, 8, 10) * 86_400_000_000)
        );
        assert_eq!(parse_timestamp("2026-08-10"), None);
        assert_eq!(
            parse_timestamp("2026-08-10 06:30:00"),
            parse_timestamp("2026-08-10T06:30:00Z"),
        );
    }

    #[test]
    fn modifiers_move_by_the_unit_they_name() {
        let base = parse_timestamp("2026-08-10 12:00:00").unwrap();
        assert_eq!(
            apply_modifier(base, "-30 days").map(format_sqlite_datetime),
            Some("2026-07-11 12:00:00".to_string())
        );
        assert_eq!(
            apply_modifier(base, "-60 seconds").map(format_sqlite_datetime),
            Some("2026-08-10 11:59:00".to_string())
        );
        assert_eq!(
            apply_modifier(base, "-1 hour").map(format_sqlite_datetime),
            Some("2026-08-10 11:00:00".to_string())
        );
    }

    #[test]
    fn an_unrecognised_modifier_is_refused_rather_than_ignored() {
        let base = parse_timestamp("2026-08-10 12:00:00").unwrap();
        assert_eq!(apply_modifier(base, "start of month"), None);
        assert_eq!(apply_modifier(base, "localtime"), None);
    }

    #[test]
    fn month_arithmetic_clamps_instead_of_overflowing_into_the_next_month() {
        let base = parse_timestamp("2026-01-31 00:00:00").unwrap();
        assert_eq!(
            apply_modifier(base, "+1 month").map(format_sqlite_date),
            Some("2026-02-28".to_string())
        );
    }
}
