//! The integration seam between the wire protocol and the query engine.
//!
//! `ferrite-protocol` deliberately does not depend on `ferrite-exec`: it
//! only knows how to turn bytes into a SQL string plus an [`Identity`] and
//! how to turn a [`QueryResult`] back into bytes. Anything that can
//! implement [`QueryHandler`] can be served over the PostgreSQL wire
//! protocol, which is what lets this crate be developed and tested against
//! [`crate::mock::MockHandler`] before the real executor exists.

use async_trait::async_trait;
use ferrite_common::{DataType, FerriteError, Identity, Row};

use crate::message::TransactionStatus;

/// One output column: everything needed to build a `RowDescription`.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldDescription {
    pub name: String,
    pub data_type: DataType,
    /// OID of the table this column came from, or 0 for a computed column.
    pub table_oid: i32,
    /// 1-based position within that table, or 0 for a computed column.
    pub column_id: i16,
}

impl FieldDescription {
    /// A computed column: no originating table, e.g. `SELECT 1 + 1`.
    pub fn computed(name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            name: name.into(),
            data_type,
            table_oid: 0,
            column_id: 0,
        }
    }
}

/// The `CommandComplete` tag. PostgreSQL clients parse these strings to
/// report affected-row counts, so the exact spelling matters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandTag {
    Select(u64),
    Insert(u64),
    Update(u64),
    Delete(u64),
    Begin,
    Commit,
    Rollback,
    /// Anything else, spelled exactly as it goes on the wire, e.g.
    /// `CREATE TABLE`.
    Other(String),
}

impl CommandTag {
    pub fn to_wire(&self) -> String {
        match self {
            CommandTag::Select(n) => format!("SELECT {n}"),
            // The zero is the legacy OID field; PostgreSQL still emits it.
            CommandTag::Insert(n) => format!("INSERT 0 {n}"),
            CommandTag::Update(n) => format!("UPDATE {n}"),
            CommandTag::Delete(n) => format!("DELETE {n}"),
            CommandTag::Begin => "BEGIN".to_owned(),
            CommandTag::Commit => "COMMIT".to_owned(),
            CommandTag::Rollback => "ROLLBACK".to_owned(),
            CommandTag::Other(s) => s.clone(),
        }
    }
}

/// The outcome of one statement.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    /// Empty for statements that return no rows; a non-empty list produces
    /// a `RowDescription` even when `rows` itself is empty.
    pub fields: Vec<FieldDescription>,
    pub rows: Vec<Row>,
    pub tag: CommandTag,
    /// Set when the statement changed transaction state, so the protocol
    /// can report the right status in `ReadyForQuery`. `None` leaves the
    /// session's current state untouched.
    pub transaction: Option<TransactionStatus>,
    /// True for a statement that was only whitespace/comments, which the
    /// protocol answers with `EmptyQueryResponse` instead of
    /// `CommandComplete`.
    pub empty_query: bool,
}

impl QueryResult {
    pub fn rows(fields: Vec<FieldDescription>, rows: Vec<Row>) -> Self {
        Self {
            tag: CommandTag::Select(rows.len() as u64),
            fields,
            rows,
            transaction: None,
            empty_query: false,
        }
    }

    pub fn command(tag: CommandTag) -> Self {
        Self {
            fields: Vec::new(),
            rows: Vec::new(),
            tag,
            transaction: None,
            empty_query: false,
        }
    }

    pub fn empty_query() -> Self {
        Self {
            empty_query: true,
            ..Self::command(CommandTag::Other(String::new()))
        }
    }

    pub fn with_transaction(mut self, status: TransactionStatus) -> Self {
        self.transaction = Some(status);
        self
    }
}

/// What `Describe` reports for a prepared statement, before any row is
/// produced.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StatementDescription {
    /// Inferred parameter types, positionally. `None` means the engine
    /// could not infer one; it goes on the wire as OID 0 ("unspecified"),
    /// and the protocol then decodes that parameter as text.
    pub parameter_types: Vec<Option<DataType>>,
    /// Output columns, or `None` when the statement returns no rows.
    pub fields: Option<Vec<FieldDescription>>,
}

impl StatementDescription {
    /// The answer for a handler that cannot describe statements ahead of
    /// execution: no parameters, no result columns.
    pub fn unknown() -> Self {
        Self::default()
    }
}

/// Executes SQL on behalf of an authenticated connection.
///
/// This is the seam `ferrite-server` will use to plug in `ferrite-exec`
/// once that crate is ready. Implementations must be `Send + Sync`: one
/// handler is shared by every connection.
///
/// The `caller` argument carries the authenticated [`Identity`] all the way
/// down to procedures and triggers, which is where Ferrite enforces access
/// (there is no separate row-level-security policy language — see
/// `docs/architecture.md` §Modèle de sécurité).
#[async_trait]
pub trait QueryHandler: Send + Sync + 'static {
    /// Runs a statement with no bound parameters. This is the whole of the
    /// simple query flow and the only required method.
    async fn execute(&self, sql: &str, caller: Identity) -> Result<QueryResult, FerriteError>;

    /// Runs a statement with bound parameters, for the extended query flow.
    ///
    /// The default implementation ignores the parameters and delegates to
    /// [`QueryHandler::execute`], which is correct only for statements that
    /// have none. A handler that accepts placeholders must override this.
    async fn execute_params(
        &self,
        sql: &str,
        params: &[ferrite_common::Value],
        caller: Identity,
    ) -> Result<QueryResult, FerriteError> {
        if !params.is_empty() {
            return Err(FerriteError::Exec(
                "this query handler does not support bound parameters".into(),
            ));
        }
        self.execute(sql, caller).await
    }

    /// Describes a statement's parameters and result columns without
    /// running it.
    ///
    /// The default returns [`StatementDescription::unknown`], which is
    /// enough for clients that only use the simple query flow. Drivers that
    /// prepare statements (tokio-postgres, sqlx, JDBC) need a real
    /// implementation to learn the result column types.
    async fn describe(
        &self,
        sql: &str,
        caller: Identity,
    ) -> Result<StatementDescription, FerriteError> {
        let _ = (sql, caller);
        Ok(StatementDescription::unknown())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_tags_match_postgres_spelling() {
        assert_eq!(CommandTag::Select(3).to_wire(), "SELECT 3");
        assert_eq!(CommandTag::Insert(1).to_wire(), "INSERT 0 1");
        assert_eq!(CommandTag::Delete(0).to_wire(), "DELETE 0");
        assert_eq!(
            CommandTag::Other("CREATE TABLE".into()).to_wire(),
            "CREATE TABLE"
        );
    }

    #[test]
    fn an_unknown_description_has_no_parameters_and_no_columns() {
        let d = StatementDescription::unknown();
        assert!(d.parameter_types.is_empty() && d.fields.is_none());
    }
}
