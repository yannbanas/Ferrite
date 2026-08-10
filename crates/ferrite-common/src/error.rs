use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Error)]
pub enum FerriteError {
    #[error("table not found: {0}")]
    TableNotFound(String),
    #[error("column not found: {0}")]
    ColumnNotFound(String),
    #[error("type mismatch: expected {expected:?}, got {actual:?}")]
    TypeMismatch {
        expected: crate::DataType,
        actual: crate::DataType,
    },
    #[error("row not found")]
    RowNotFound,
    #[error("transaction {0} is not active")]
    TxnNotActive(crate::TxnId),
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("serialization failure: concurrent update conflict")]
    SerializationFailure,
    #[error("storage error: {0}")]
    Storage(String),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("planning error: {0}")]
    Plan(String),
    #[error("execution error: {0}")]
    Exec(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    /// A `CREATE` collided with something that already has this name
    /// (table, index, procedure, trigger…). Distinct from `Storage` so a
    /// protocol layer can map it to the right SQLSTATE (`42P07`-class)
    /// instead of a generic internal error.
    #[error("already exists: {0}")]
    ObjectAlreadyExists(String),
    /// A schema/DDL definition is well-formed SQL but not a valid object —
    /// e.g. a `PRIMARY KEY` naming a column that doesn't exist, a `CHECK`
    /// referencing an unknown function. Distinct from `Parse` (syntax) and
    /// `Plan` (a query that can't be executed against existing schema).
    #[error("invalid definition: {0}")]
    InvalidDefinition(String),
}
