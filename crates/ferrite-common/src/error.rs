use thiserror::Error;

#[derive(Debug, Error)]
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
}
