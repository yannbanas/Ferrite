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
    /// A write would have put two rows with the same key in a table
    /// carrying a `PRIMARY KEY` or `UNIQUE` constraint.
    ///
    /// Deliberately not `ObjectAlreadyExists`: that one is the
    /// `42710`-class "a *schema object* of this name exists" and is what a
    /// duplicate `CREATE` raises. A duplicate *row* is SQLSTATE `23505`,
    /// and every mainstream driver (`sqlx`, node-postgres, JDBC) keys the
    /// "already registered" branch of an application off that code
    /// specifically. Collapsing the two would make a duplicate username
    /// indistinguishable from a duplicate table name on the wire.
    #[error("duplicate key value violates unique constraint {constraint:?}: ({key})")]
    UniqueViolation { constraint: String, key: String },
    /// A statement or transaction ran past its configured time budget and
    /// was abandoned. Retryable, like a serialization failure.
    #[error("canceling statement due to timeout: {0}")]
    Timeout(String),
    /// A statement asked for more memory than the executor is allowed to
    /// materialize. Refusing is the point: an unbounded result set is how
    /// one pathological query takes the process down for every other
    /// connection too.
    #[error("resource limit exceeded: {0}")]
    ResourceLimit(String),
    /// A schema/DDL definition is well-formed SQL but not a valid object —
    /// e.g. a `PRIMARY KEY` naming a column that doesn't exist, a `CHECK`
    /// referencing an unknown function. Distinct from `Parse` (syntax) and
    /// `Plan` (a query that can't be executed against existing schema).
    #[error("invalid definition: {0}")]
    InvalidDefinition(String),
}
