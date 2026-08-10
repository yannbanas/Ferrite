//! Mapping between `ferrite_common` types and the PostgreSQL type system.
//!
//! Clients identify types by OID, so Ferrite reuses PostgreSQL's own OIDs
//! for the eight scalar types it supports. That is what lets an unmodified
//! driver decode a `DataRow` without ever querying `pg_type`.

use ferrite_common::datetime::{parse_timestamp, parts_from_micros};
use ferrite_common::{DataType, Value};

use crate::error::{ProtocolError, Result};

pub type Oid = i32;

pub mod oid {
    use super::Oid;

    pub const BOOL: Oid = 16;
    pub const INT8: Oid = 20;
    pub const INT4: Oid = 23;
    pub const TEXT: Oid = 25;
    pub const JSON: Oid = 114;
    pub const FLOAT8: Oid = 701;
    pub const TIMESTAMPTZ: Oid = 1184;
    pub const UUID: Oid = 2950;
}

/// Wire representation requested for a column or parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    #[default]
    Text,
    Binary,
}

impl Format {
    pub(crate) fn from_code(code: i16) -> Result<Self> {
        match code {
            0 => Ok(Format::Text),
            1 => Ok(Format::Binary),
            other => Err(ProtocolError::malformed(format!(
                "unknown format code {other}"
            ))),
        }
    }

    pub(crate) fn code(self) -> i16 {
        match self {
            Format::Text => 0,
            Format::Binary => 1,
        }
    }
}

/// PostgreSQL OID for a Ferrite scalar type.
pub fn type_oid(ty: DataType) -> Oid {
    match ty {
        DataType::Boolean => oid::BOOL,
        DataType::Int4 => oid::INT4,
        DataType::Int8 => oid::INT8,
        DataType::Float8 => oid::FLOAT8,
        DataType::Text => oid::TEXT,
        DataType::Timestamp => oid::TIMESTAMPTZ,
        DataType::Uuid => oid::UUID,
        DataType::Json => oid::JSON,
    }
}

/// Inverse of [`type_oid`]. Unknown OIDs (including 0, "unspecified") map
/// to `None`; callers treat that as "let the server decide".
pub fn type_from_oid(oid: Oid) -> Option<DataType> {
    match oid {
        oid::BOOL => Some(DataType::Boolean),
        oid::INT4 => Some(DataType::Int4),
        oid::INT8 => Some(DataType::Int8),
        oid::FLOAT8 => Some(DataType::Float8),
        oid::TEXT | 1042 | 1043 => Some(DataType::Text),
        oid::TIMESTAMPTZ | 1114 => Some(DataType::Timestamp),
        oid::UUID => Some(DataType::Uuid),
        oid::JSON | 3802 => Some(DataType::Json),
        _ => None,
    }
}

/// Fixed on-wire width in bytes, or `-1` for variable-length types.
pub fn type_size(ty: DataType) -> i16 {
    match ty {
        DataType::Boolean => 1,
        DataType::Int4 => 4,
        DataType::Int8 | DataType::Float8 | DataType::Timestamp => 8,
        DataType::Uuid => 16,
        DataType::Text | DataType::Json => -1,
    }
}

/// Microseconds between the Unix epoch and PostgreSQL's 2000-01-01 epoch.
const PG_EPOCH_OFFSET_US: i64 = 946_684_800_000_000;

/// Encodes a value for a `DataRow` field. `None` means SQL NULL, which the
/// caller writes as a `-1` length prefix.
pub fn encode_value(value: &Value, format: Format) -> Option<Vec<u8>> {
    if value.is_null() {
        return None;
    }
    Some(match format {
        Format::Text => encode_text(value).into_bytes(),
        Format::Binary => encode_binary(value),
    })
}

fn encode_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Boolean(b) => if *b { "t" } else { "f" }.to_owned(),
        Value::Int4(v) => v.to_string(),
        Value::Int8(v) => v.to_string(),
        Value::Float8(v) => format_float8(*v),
        Value::Text(v) => v.clone(),
        Value::Timestamp(us) => format_timestamp(*us),
        Value::Uuid(v) => format_uuid(*v),
        Value::Json(v) => v.clone(),
    }
}

fn encode_binary(value: &Value) -> Vec<u8> {
    match value {
        Value::Null => Vec::new(),
        Value::Boolean(b) => vec![u8::from(*b)],
        Value::Int4(v) => v.to_be_bytes().to_vec(),
        Value::Int8(v) => v.to_be_bytes().to_vec(),
        Value::Float8(v) => v.to_be_bytes().to_vec(),
        Value::Text(v) => v.as_bytes().to_vec(),
        Value::Timestamp(us) => (us - PG_EPOCH_OFFSET_US).to_be_bytes().to_vec(),
        Value::Uuid(v) => v.to_be_bytes().to_vec(),
        Value::Json(v) => v.as_bytes().to_vec(),
    }
}

/// Decodes a bound parameter. A `None` payload is SQL NULL.
pub fn decode_value(ty: DataType, format: Format, raw: Option<&[u8]>) -> Result<Value> {
    let Some(raw) = raw else {
        return Ok(Value::Null);
    };
    match format {
        Format::Text => decode_text(ty, raw),
        Format::Binary => decode_binary(ty, raw),
    }
}

fn decode_text(ty: DataType, raw: &[u8]) -> Result<Value> {
    let s = std::str::from_utf8(raw)
        .map_err(|_| ProtocolError::malformed("parameter is not valid UTF-8"))?;
    let bad = |what: &str| ProtocolError::malformed(format!("invalid {what} literal {s:?}"));
    Ok(match ty {
        DataType::Boolean => match s {
            "t" | "true" | "TRUE" | "1" | "yes" | "on" => Value::Boolean(true),
            "f" | "false" | "FALSE" | "0" | "no" | "off" => Value::Boolean(false),
            _ => return Err(bad("boolean")),
        },
        DataType::Int4 => Value::Int4(s.parse().map_err(|_| bad("int4"))?),
        DataType::Int8 => Value::Int8(s.parse().map_err(|_| bad("int8"))?),
        DataType::Float8 => Value::Float8(s.parse().map_err(|_| bad("float8"))?),
        DataType::Text => Value::Text(s.to_owned()),
        DataType::Timestamp => {
            Value::Timestamp(parse_timestamp(s).ok_or_else(|| bad("timestamp"))?)
        }
        DataType::Uuid => Value::Uuid(parse_uuid(s).ok_or_else(|| bad("uuid"))?),
        DataType::Json => Value::Json(s.to_owned()),
    })
}

fn decode_binary(ty: DataType, raw: &[u8]) -> Result<Value> {
    let width = |n: usize| -> Result<()> {
        if raw.len() == n {
            Ok(())
        } else {
            Err(ProtocolError::malformed(format!(
                "binary {ty:?} parameter is {} bytes, expected {n}",
                raw.len()
            )))
        }
    };
    Ok(match ty {
        DataType::Boolean => {
            width(1)?;
            Value::Boolean(raw[0] != 0)
        }
        DataType::Int4 => {
            width(4)?;
            Value::Int4(i32::from_be_bytes(raw.try_into().unwrap()))
        }
        DataType::Int8 => {
            width(8)?;
            Value::Int8(i64::from_be_bytes(raw.try_into().unwrap()))
        }
        DataType::Float8 => {
            width(8)?;
            Value::Float8(f64::from_be_bytes(raw.try_into().unwrap()))
        }
        DataType::Timestamp => {
            width(8)?;
            Value::Timestamp(i64::from_be_bytes(raw.try_into().unwrap()) + PG_EPOCH_OFFSET_US)
        }
        DataType::Uuid => {
            width(16)?;
            Value::Uuid(u128::from_be_bytes(raw.try_into().unwrap()))
        }
        DataType::Text | DataType::Json => {
            let s = std::str::from_utf8(raw)
                .map_err(|_| ProtocolError::malformed("parameter is not valid UTF-8"))?
                .to_owned();
            if matches!(ty, DataType::Json) {
                Value::Json(s)
            } else {
                Value::Text(s)
            }
        }
    })
}

/// PostgreSQL prints `float8` with the shortest round-tripping form and
/// spells out the three non-finite values, which Rust's `{}` does not.
fn format_float8(v: f64) -> String {
    if v.is_nan() {
        "NaN".to_owned()
    } else if v.is_infinite() {
        if v > 0.0 { "Infinity" } else { "-Infinity" }.to_owned()
    } else {
        format!("{v}")
    }
}

fn format_uuid(v: u128) -> String {
    let b = v.to_be_bytes();
    let hex = |r: &[u8]| r.iter().map(|x| format!("{x:02x}")).collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        hex(&b[0..4]),
        hex(&b[4..6]),
        hex(&b[6..8]),
        hex(&b[8..10]),
        hex(&b[10..16])
    )
}

fn parse_uuid(s: &str) -> Option<u128> {
    let hex: String = s.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u128::from_str_radix(&hex, 16).ok()
}

/// `ISO, MDY` rendering, e.g. `2026-08-10 09:15:00.123456+00`, matching the
/// `DateStyle` the server advertises at startup.
fn format_timestamp(micros: i64) -> String {
    let (y, m, d, h, min, s, us) = parts_from_micros(micros);
    let base = format!("{y:04}-{m:02}-{d:02} {h:02}:{min:02}:{s:02}");
    if us == 0 {
        format!("{base}+00")
    } else {
        let frac = format!("{us:06}");
        format!("{base}.{}+00", frac.trim_end_matches('0'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_common::datetime::days_from_civil;

    #[test]
    fn every_data_type_round_trips_through_its_oid() {
        for ty in [
            DataType::Boolean,
            DataType::Int4,
            DataType::Int8,
            DataType::Float8,
            DataType::Text,
            DataType::Timestamp,
            DataType::Uuid,
            DataType::Json,
        ] {
            assert_eq!(type_from_oid(type_oid(ty)), Some(ty));
        }
        assert_eq!(type_from_oid(0), None);
    }

    #[test]
    fn values_round_trip_in_both_formats() {
        let cases = [
            (DataType::Boolean, Value::Boolean(true)),
            (DataType::Int4, Value::Int4(-42)),
            (DataType::Int8, Value::Int8(i64::MIN)),
            (DataType::Float8, Value::Float8(1.5)),
            (DataType::Text, Value::Text("héllo".into())),
            (DataType::Timestamp, Value::Timestamp(1_754_820_900_123_456)),
            (
                DataType::Uuid,
                Value::Uuid(0x0190_1b2c_3d4e_5f60_7182_93a4_b5c6_d7e8),
            ),
            (DataType::Json, Value::Json(r#"{"a":1}"#.into())),
        ];
        for (ty, value) in cases {
            for format in [Format::Text, Format::Binary] {
                let raw = encode_value(&value, format).unwrap();
                let back = decode_value(ty, format, Some(&raw)).unwrap();
                assert_eq!(back, value, "{ty:?} in {format:?}");
            }
        }
    }

    #[test]
    fn nulls_encode_as_absent_payloads() {
        assert_eq!(encode_value(&Value::Null, Format::Text), None);
        assert_eq!(
            decode_value(DataType::Int4, Format::Binary, None).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn timestamps_render_like_postgres() {
        assert_eq!(format_timestamp(0), "1970-01-01 00:00:00+00");
        assert_eq!(format_timestamp(-1_000_000), "1969-12-31 23:59:59+00");
        assert_eq!(format_timestamp(500_000), "1970-01-01 00:00:00.5+00");
        assert_eq!(
            format_timestamp(1_754_820_900_123_456),
            "2025-08-10 10:15:00.123456+00"
        );
        // Leap-year boundary, the classic off-by-one in civil date math.
        assert_eq!(
            format_timestamp(days_from_civil(2024, 2, 29) * 86_400_000_000),
            "2024-02-29 00:00:00+00"
        );
    }

    #[test]
    fn timestamp_text_parsing_accepts_what_it_prints() {
        for us in [0i64, -1_000_000, 500_000, 1_754_820_900_123_456] {
            assert_eq!(parse_timestamp(&format_timestamp(us)), Some(us));
        }
        assert_eq!(parse_timestamp("2025-08-10"), None);
        assert_eq!(parse_timestamp("2025-13-01 00:00:00"), None);
        assert_eq!(parse_timestamp("not a timestamp"), None);
    }

    #[test]
    fn malformed_binary_parameters_are_rejected_not_panicked() {
        assert!(decode_value(DataType::Int4, Format::Binary, Some(&[1, 2])).is_err());
        assert!(decode_value(DataType::Uuid, Format::Binary, Some(&[])).is_err());
        assert!(decode_value(DataType::Text, Format::Binary, Some(&[0xff])).is_err());
        assert!(decode_value(DataType::Int8, Format::Text, Some(b"nope")).is_err());
    }

    #[test]
    fn non_finite_floats_use_postgres_spelling() {
        assert_eq!(format_float8(f64::NAN), "NaN");
        assert_eq!(format_float8(f64::INFINITY), "Infinity");
        assert_eq!(format_float8(1.0), "1");
    }
}
