use serde::{Deserialize, Serialize};

use crate::value::{DataType, Value};

pub type TableId = u32;

/// What a column falls back to when a write does not supply a value.
///
/// Two shapes only, deliberately. A constant covers `DEFAULT 0`,
/// `DEFAULT ''`, `DEFAULT false` and every other literal; the current
/// timestamp covers the one non-constant default a real application
/// schema is full of. Anything else — an arbitrary expression, a sequence,
/// a function call — is refused at DDL time rather than half-evaluated,
/// because a default that is silently wrong is worse than one that is
/// rejected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ColumnDefault {
    /// A literal, already coerced to the column's declared type.
    Constant(Value),
    /// `CURRENT_TIMESTAMP` / `now()`: the statement's wall-clock time,
    /// evaluated once per statement rather than stored.
    CurrentTimestamp,
}

impl ColumnDefault {
    /// The value to use where a statement-time evaluation is impossible —
    /// filling a column into rows written before it existed. A volatile
    /// default has no defensible answer there, so it yields `Null`.
    pub fn constant(&self) -> Option<&Value> {
        match self {
            ColumnDefault::Constant(value) => Some(value),
            ColumnDefault::CurrentTimestamp => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
    /// The column's `DEFAULT` clause. `None` means the column falls back
    /// to `NULL`, which is what a nullable column without a `DEFAULT`
    /// does anyway.
    pub default: Option<ColumnDefault>,
}

impl ColumnDef {
    pub fn new(name: impl Into<String>, data_type: DataType, nullable: bool) -> Self {
        Self {
            name: name.into(),
            data_type,
            nullable,
            default: None,
        }
    }

    pub fn with_default(mut self, default: ColumnDefault) -> Self {
        self.default = Some(default);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Schema {
    pub columns: Vec<ColumnDef>,
}

impl Schema {
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }
}
