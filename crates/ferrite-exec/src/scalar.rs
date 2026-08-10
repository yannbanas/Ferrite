//! Evaluation of the scalar functions in [`ScalarFunc`], and of `CAST`.

use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use ferrite_common::datetime::{
    eval_datetime, format_sqlite_datetime, now_micros, parse_datetime_text,
};
use ferrite_common::{DataType, FerriteError, Value};
use ferrite_planner::ScalarFunc;

/// Apply a scalar function to already-evaluated arguments.
pub fn call(func: ScalarFunc, args: &[Value]) -> Result<Value, FerriteError> {
    match func {
        ScalarFunc::Coalesce => Ok(args
            .iter()
            .find(|v| !v.is_null())
            .cloned()
            .unwrap_or(Value::Null)),
        ScalarFunc::Lower => map_text(&args[0], |s| s.to_lowercase()),
        ScalarFunc::Upper => map_text(&args[0], |s| s.to_uppercase()),
        // A collation is a no-op on a non-text value, so unlike `lower()`
        // this passes anything else through instead of rejecting it.
        ScalarFunc::Nocase => Ok(match &args[0] {
            Value::Text(s) => Value::Text(s.to_lowercase()),
            other => other.clone(),
        }),
        ScalarFunc::Substr => substr(args),
        ScalarFunc::Hex => map_text(&args[0], |s| {
            s.bytes().map(|b| format!("{b:02X}")).collect()
        }),
        ScalarFunc::Randomblob => randomblob(&args[0]),
        ScalarFunc::Datetime => eval_datetime(args, true),
        ScalarFunc::Date => eval_datetime(args, false),
    }
}

fn map_text(value: &Value, f: impl Fn(&str) -> String) -> Result<Value, FerriteError> {
    match value {
        Value::Null => Ok(Value::Null),
        Value::Text(s) | Value::Json(s) => Ok(Value::Text(f(s))),
        other => Err(FerriteError::TypeMismatch {
            expected: DataType::Text,
            actual: other.data_type().expect("Null was matched above"),
        }),
    }
}

/// `substr(s, start[, len])`. SQLite counts from 1, and a negative `start`
/// counts back from the end of the string.
fn substr(args: &[Value]) -> Result<Value, FerriteError> {
    let Value::Text(s) = &args[0] else {
        return match &args[0] {
            Value::Null => Ok(Value::Null),
            other => Err(FerriteError::TypeMismatch {
                expected: DataType::Text,
                actual: other.data_type().expect("Null was matched above"),
            }),
        };
    };
    let chars: Vec<char> = s.chars().collect();
    let start = match integer_arg(&args[1])? {
        None => return Ok(Value::Null),
        Some(start) => start,
    };
    let begin = match start {
        0 => 0,
        s if s > 0 => (s - 1) as usize,
        s => chars.len().saturating_sub((-s) as usize),
    };
    let begin = begin.min(chars.len());
    let end = match args.get(2) {
        None => chars.len(),
        Some(len) => match integer_arg(len)? {
            None => return Ok(Value::Null),
            Some(len) if len <= 0 => begin,
            Some(len) => begin.saturating_add(len as usize).min(chars.len()),
        },
    };
    Ok(Value::Text(chars[begin..end].iter().collect()))
}

fn integer_arg(value: &Value) -> Result<Option<i64>, FerriteError> {
    match value {
        Value::Null => Ok(None),
        Value::Int4(v) => Ok(Some(i64::from(*v))),
        Value::Int8(v) => Ok(Some(*v)),
        other => Err(FerriteError::TypeMismatch {
            expected: DataType::Int8,
            actual: other.data_type().expect("Null was matched above"),
        }),
    }
}

/// xorshift64* seeded from the clock, advanced by an atomic counter so two
/// calls in the same microsecond still differ. Not a cryptographic source;
/// `randomblob` feeds invite codes that are checked for uniqueness by a
/// `UNIQUE` index, not secrets.
fn randomblob(len: &Value) -> Result<Value, FerriteError> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let Some(len) = integer_arg(len)? else {
        return Ok(Value::Null);
    };
    let len = len.clamp(0, 1 << 20) as usize;
    let mut state = (now_micros() as u64)
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(COUNTER.fetch_add(1, AtomicOrdering::Relaxed) | 1);
    let mut out = String::with_capacity(len);
    for _ in 0..len {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        out.push(((state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8) as char);
    }
    Ok(Value::Text(out))
}

/// Explicit `CAST`, with PostgreSQL's strictness rather than SQLite's.
///
/// SQLite gives `CAST('abc' AS INTEGER)` the value `0`; here it is an
/// error. Silently substituting zero turns a malformed row into a row that
/// joins against id 0, which is worse than a failed query.
pub fn cast(value: &Value, target: DataType) -> Result<Value, FerriteError> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    let bad = |what: &str| FerriteError::Exec(format!("cannot cast {what} to {target:?}"));
    Ok(match target {
        DataType::Text => Value::Text(render(value)),
        DataType::Boolean => match value {
            Value::Boolean(b) => Value::Boolean(*b),
            Value::Int4(v) => Value::Boolean(*v != 0),
            Value::Int8(v) => Value::Boolean(*v != 0),
            Value::Text(s) => match s.trim().to_ascii_lowercase().as_str() {
                "t" | "true" | "y" | "yes" | "on" | "1" => Value::Boolean(true),
                "f" | "false" | "n" | "no" | "off" | "0" => Value::Boolean(false),
                _ => return Err(bad(&format!("`{s}`"))),
            },
            other => return Err(bad(&render(other))),
        },
        DataType::Int4 => Value::Int4(
            i32::try_from(to_integer(value).ok_or_else(|| bad(&render(value)))?)
                .map_err(|_| FerriteError::Exec("integer out of range".to_string()))?,
        ),
        DataType::Int8 => Value::Int8(to_integer(value).ok_or_else(|| bad(&render(value)))?),
        DataType::Float8 => Value::Float8(match value {
            Value::Int4(v) => f64::from(*v),
            Value::Int8(v) => *v as f64,
            Value::Float8(v) => *v,
            Value::Text(s) => s.trim().parse().map_err(|_| bad(&format!("`{s}`")))?,
            other => return Err(bad(&render(other))),
        }),
        DataType::Timestamp => match value {
            Value::Timestamp(us) => Value::Timestamp(*us),
            Value::Text(s) => {
                Value::Timestamp(parse_datetime_text(s).ok_or_else(|| bad(&format!("`{s}`")))?)
            }
            other => return Err(bad(&render(other))),
        },
        DataType::Json => match value {
            Value::Json(s) | Value::Text(s) => Value::Json(s.clone()),
            other => return Err(bad(&render(other))),
        },
        DataType::Uuid => match value {
            Value::Uuid(u) => Value::Uuid(*u),
            Value::Text(s) => Value::Uuid(
                u128::from_str_radix(&s.replace('-', ""), 16)
                    .map_err(|_| bad(&format!("`{s}`")))?,
            ),
            other => return Err(bad(&render(other))),
        },
    })
}

fn to_integer(value: &Value) -> Option<i64> {
    match value {
        Value::Boolean(b) => Some(i64::from(*b)),
        Value::Int4(v) => Some(i64::from(*v)),
        Value::Int8(v) => Some(*v),
        Value::Float8(v) => Some(v.trunc() as i64),
        Value::Text(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// The text a value casts to, which is also how `hex` and `||` see it.
fn render(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Boolean(b) => (if *b { "true" } else { "false" }).to_string(),
        Value::Int4(v) => v.to_string(),
        Value::Int8(v) => v.to_string(),
        Value::Float8(v) => v.to_string(),
        Value::Text(s) | Value::Json(s) => s.clone(),
        Value::Timestamp(us) => format_sqlite_datetime(*us),
        Value::Uuid(u) => format!("{u:032x}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Value {
        Value::Text(s.to_string())
    }

    #[test]
    fn coalesce_takes_the_first_non_null() {
        assert_eq!(
            call(ScalarFunc::Coalesce, &[Value::Null, Value::Int8(3)]).unwrap(),
            Value::Int8(3)
        );
        assert_eq!(
            call(ScalarFunc::Coalesce, &[Value::Null, Value::Null]).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn substr_is_one_based_and_takes_a_negative_start_from_the_end() {
        let call3 = |s, a, b| call(ScalarFunc::Substr, &[text(s), a, b]).unwrap();
        assert_eq!(call3("pf:42", Value::Int8(4), Value::Int8(9)), text("42"));
        assert_eq!(call3("abcdef", Value::Int8(-2), Value::Int8(2)), text("ef"));
        assert_eq!(
            call(ScalarFunc::Substr, &[text("pf:42"), Value::Int8(4)]).unwrap(),
            text("42")
        );
    }

    #[test]
    fn nocase_folds_text_and_leaves_everything_else_alone() {
        assert_eq!(
            call(ScalarFunc::Nocase, &[text("AdA")]).unwrap(),
            text("ada")
        );
        assert_eq!(
            call(ScalarFunc::Nocase, &[Value::Int8(7)]).unwrap(),
            Value::Int8(7)
        );
    }

    #[test]
    fn datetime_renders_sqlites_shape_and_applies_modifiers() {
        let base = text("2026-08-10 12:00:00");
        assert_eq!(
            call(ScalarFunc::Datetime, std::slice::from_ref(&base)).unwrap(),
            text("2026-08-10 12:00:00")
        );
        assert_eq!(
            call(ScalarFunc::Date, std::slice::from_ref(&base)).unwrap(),
            text("2026-08-10")
        );
        assert_eq!(
            call(ScalarFunc::Datetime, &[base, text("-30 days")]).unwrap(),
            text("2026-07-11 12:00:00")
        );
    }

    #[test]
    fn an_unknown_date_modifier_is_an_error_not_a_no_op() {
        let args = [text("2026-08-10 12:00:00"), text("start of month")];
        assert!(call(ScalarFunc::Datetime, &args).is_err());
    }

    #[test]
    fn now_is_accepted_as_the_first_argument() {
        let Value::Text(rendered) = call(ScalarFunc::Date, &[text("now")]).unwrap() else {
            panic!("date() returns text");
        };
        assert_eq!(rendered.len(), 10);
    }

    #[test]
    fn cast_to_integer_refuses_junk_rather_than_yielding_zero() {
        assert_eq!(cast(&text("42"), DataType::Int4).unwrap(), Value::Int4(42));
        assert!(cast(&text("abc"), DataType::Int4).is_err());
        assert_eq!(cast(&Value::Null, DataType::Int4).unwrap(), Value::Null);
    }

    #[test]
    fn cast_to_text_renders_every_scalar() {
        assert_eq!(cast(&Value::Int8(7), DataType::Text).unwrap(), text("7"));
        assert_eq!(
            cast(&Value::Boolean(true), DataType::Text).unwrap(),
            text("true")
        );
    }

    #[test]
    fn randomblob_is_the_requested_length_and_varies_between_calls() {
        let one = call(ScalarFunc::Randomblob, &[Value::Int8(8)]).unwrap();
        let two = call(ScalarFunc::Randomblob, &[Value::Int8(8)]).unwrap();
        let Value::Text(one) = &one else { panic!() };
        assert_eq!(one.chars().count(), 8);
        assert_ne!(Value::Text(one.clone()), two);
    }
}
