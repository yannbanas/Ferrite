//! Hand-written binary codec for the on-disk representation of a `Row`.
//!
//! Rows are not encoded with `serde` on purpose: an on-disk format must be
//! stable against refactors of the in-memory types, and a derive-driven
//! encoding silently changes whenever a variant is reordered. Every tag
//! below is therefore pinned by this module and must never be reused for a
//! different meaning.

use ferrite_common::{FerriteError, Row, Value};

const TAG_NULL: u8 = 0;
const TAG_BOOLEAN: u8 = 1;
const TAG_INT4: u8 = 2;
const TAG_INT8: u8 = 3;
const TAG_FLOAT8: u8 = 4;
const TAG_TEXT: u8 = 5;
const TAG_TIMESTAMP: u8 = 6;
const TAG_UUID: u8 = 7;
const TAG_JSON: u8 = 8;

fn corrupt(what: &str) -> FerriteError {
    FerriteError::Storage(format!("corrupt row encoding: {what}"))
}

/// Reads little-endian scalars out of a byte slice, reporting truncation
/// as a storage error rather than panicking — the input may be a damaged
/// page.
pub struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    pub fn take(&mut self, len: usize) -> Result<&'a [u8], FerriteError> {
        if self.remaining() < len {
            return Err(corrupt("unexpected end of buffer"));
        }
        let out = &self.bytes[self.pos..self.pos + len];
        self.pos += len;
        Ok(out)
    }

    pub fn u8(&mut self) -> Result<u8, FerriteError> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16, FerriteError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn u32(&mut self) -> Result<u32, FerriteError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn u64(&mut self) -> Result<u64, FerriteError> {
        let b = self.take(8)?;
        let mut buf = [0u8; 8];
        buf.copy_from_slice(b);
        Ok(u64::from_le_bytes(buf))
    }

    pub fn u128(&mut self) -> Result<u128, FerriteError> {
        let b = self.take(16)?;
        let mut buf = [0u8; 16];
        buf.copy_from_slice(b);
        Ok(u128::from_le_bytes(buf))
    }
}

/// Appends little-endian scalars to a growable buffer.
#[derive(Default)]
pub struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn u8(&mut self, v: u8) {
        self.bytes.push(v);
    }

    pub fn u16(&mut self, v: u16) {
        self.bytes.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u32(&mut self, v: u32) {
        self.bytes.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.bytes.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u128(&mut self, v: u128) {
        self.bytes.extend_from_slice(&v.to_le_bytes());
    }

    pub fn bytes(&mut self, v: &[u8]) {
        self.bytes.extend_from_slice(v);
    }

    pub fn len_prefixed(&mut self, v: &[u8]) {
        self.u32(v.len() as u32);
        self.bytes(v);
    }

    pub fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

pub fn encode_row(row: &Row) -> Vec<u8> {
    let mut w = Writer::new();
    w.u16(row.values.len() as u16);
    for value in &row.values {
        encode_value(&mut w, value);
    }
    w.finish()
}

fn encode_value(w: &mut Writer, value: &Value) {
    match value {
        Value::Null => w.u8(TAG_NULL),
        Value::Boolean(b) => {
            w.u8(TAG_BOOLEAN);
            w.u8(u8::from(*b));
        }
        Value::Int4(v) => {
            w.u8(TAG_INT4);
            w.u32(*v as u32);
        }
        Value::Int8(v) => {
            w.u8(TAG_INT8);
            w.u64(*v as u64);
        }
        Value::Float8(v) => {
            w.u8(TAG_FLOAT8);
            w.u64(v.to_bits());
        }
        Value::Text(s) => {
            w.u8(TAG_TEXT);
            w.len_prefixed(s.as_bytes());
        }
        Value::Timestamp(v) => {
            w.u8(TAG_TIMESTAMP);
            w.u64(*v as u64);
        }
        Value::Uuid(v) => {
            w.u8(TAG_UUID);
            w.u128(*v);
        }
        Value::Json(s) => {
            w.u8(TAG_JSON);
            w.len_prefixed(s.as_bytes());
        }
    }
}

pub fn decode_row(bytes: &[u8]) -> Result<Row, FerriteError> {
    let mut r = Reader::new(bytes);
    let count = r.u16()? as usize;
    let mut values = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        values.push(decode_value(&mut r)?);
    }
    if !r.is_empty() {
        return Err(corrupt("trailing bytes after row"));
    }
    Ok(Row::new(values))
}

fn decode_value(r: &mut Reader<'_>) -> Result<Value, FerriteError> {
    let tag = r.u8()?;
    Ok(match tag {
        TAG_NULL => Value::Null,
        TAG_BOOLEAN => Value::Boolean(r.u8()? != 0),
        TAG_INT4 => Value::Int4(r.u32()? as i32),
        TAG_INT8 => Value::Int8(r.u64()? as i64),
        TAG_FLOAT8 => Value::Float8(f64::from_bits(r.u64()?)),
        TAG_TEXT => Value::Text(decode_string(r)?),
        TAG_TIMESTAMP => Value::Timestamp(r.u64()? as i64),
        TAG_UUID => Value::Uuid(r.u128()?),
        TAG_JSON => Value::Json(decode_string(r)?),
        other => return Err(corrupt(&format!("unknown value tag {other}"))),
    })
}

fn decode_string(r: &mut Reader<'_>) -> Result<String, FerriteError> {
    let len = r.u32()? as usize;
    let bytes = r.take(len)?;
    String::from_utf8(bytes.to_vec()).map_err(|_| corrupt("invalid utf-8 in text value"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(row: Row) {
        let encoded = encode_row(&row);
        let decoded = decode_row(&encoded).expect("decode");
        assert_eq!(row, decoded);
    }

    #[test]
    fn roundtrips_every_variant() {
        roundtrip(Row::new(vec![
            Value::Null,
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Int4(i32::MIN),
            Value::Int8(i64::MAX),
            Value::Float8(-0.5),
            Value::Text("héllo wörld".into()),
            Value::Timestamp(-1),
            Value::Uuid(u128::MAX),
            Value::Json("{\"a\":1}".into()),
        ]));
    }

    #[test]
    fn roundtrips_empty_row() {
        roundtrip(Row::new(vec![]));
    }

    #[test]
    fn float_nan_survives_as_bits() {
        let encoded = encode_row(&Row::new(vec![Value::Float8(f64::NAN)]));
        let decoded = decode_row(&encoded).unwrap();
        match decoded.values[0] {
            Value::Float8(v) => assert!(v.is_nan()),
            ref other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn rejects_truncated_input() {
        let encoded = encode_row(&Row::new(vec![Value::Int8(7)]));
        for cut in 0..encoded.len() {
            assert!(decode_row(&encoded[..cut]).is_err(), "cut at {cut}");
        }
    }

    #[test]
    fn rejects_unknown_tag() {
        let bytes = [1u8, 0, 200];
        assert!(decode_row(&bytes).is_err());
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut encoded = encode_row(&Row::new(vec![Value::Int4(1)]));
        encoded.push(0);
        assert!(decode_row(&encoded).is_err());
    }
}
