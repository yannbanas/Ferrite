use serde::{Deserialize, Serialize};

/// Scalar SQL types supported by Ferrite v1. Deliberately a small set —
/// see the project README for what's cut and why. `Json` stores validated
/// JSON text rather than a parsed tree; parsing on read keeps this crate
/// free of a JSON value representation of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DataType {
    Boolean,
    Int4,
    Int8,
    Float8,
    Text,
    /// Microseconds since the Unix epoch, UTC.
    Timestamp,
    /// UUID v7 is the default generator; the stored representation is a
    /// plain 128-bit value regardless of version.
    Uuid,
    Json,
}

/// A runtime value. One variant per `DataType`, plus `Null`, which is
/// valid for any nullable column regardless of its declared type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Null,
    Boolean(bool),
    Int4(i32),
    Int8(i64),
    Float8(f64),
    Text(String),
    Timestamp(i64),
    Uuid(u128),
    Json(String),
}

impl Value {
    pub fn data_type(&self) -> Option<DataType> {
        match self {
            Value::Null => None,
            Value::Boolean(_) => Some(DataType::Boolean),
            Value::Int4(_) => Some(DataType::Int4),
            Value::Int8(_) => Some(DataType::Int8),
            Value::Float8(_) => Some(DataType::Float8),
            Value::Text(_) => Some(DataType::Text),
            Value::Timestamp(_) => Some(DataType::Timestamp),
            Value::Uuid(_) => Some(DataType::Uuid),
            Value::Json(_) => Some(DataType::Json),
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
}

/// A single row: positional values matching a `Schema`'s column order.
/// Row identity (`RowId`) and MVCC version metadata live in the storage
/// crate, not here — a `Row` is just data, not a stored tuple.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub values: Vec<Value>,
}

impl Row {
    pub fn new(values: Vec<Value>) -> Self {
        Self { values }
    }
}
