//! A hard-wired [`QueryHandler`] with no storage behind it.
//!
//! It exists so the protocol layer can be developed, tested and demoed
//! end-to-end before `ferrite-exec` is usable — every integration test in
//! this crate, and `ferrite-server` when started without an engine, runs
//! against it. It answers a fixed set of statements and rejects everything
//! else; it is not a SQL implementation and must never be one.

use std::sync::Mutex;

use async_trait::async_trait;
use ferrite_common::{DataType, FerriteError, Identity, Row, Value};

use crate::handler::{
    CommandTag, FieldDescription, QueryHandler, QueryResult, StatementDescription,
};
use crate::message::TransactionStatus;

/// Rows returned by `SELECT * FROM pets`, one column per supported type.
fn pets_fields() -> Vec<FieldDescription> {
    vec![
        FieldDescription {
            name: "id".into(),
            data_type: DataType::Int4,
            table_oid: 16_384,
            column_id: 1,
        },
        FieldDescription {
            name: "name".into(),
            data_type: DataType::Text,
            table_oid: 16_384,
            column_id: 2,
        },
        FieldDescription {
            name: "adopted".into(),
            data_type: DataType::Boolean,
            table_oid: 16_384,
            column_id: 3,
        },
        FieldDescription {
            name: "weight_kg".into(),
            data_type: DataType::Float8,
            table_oid: 16_384,
            column_id: 4,
        },
        FieldDescription {
            name: "born_at".into(),
            data_type: DataType::Timestamp,
            table_oid: 16_384,
            column_id: 5,
        },
        FieldDescription {
            name: "external_id".into(),
            data_type: DataType::Uuid,
            table_oid: 16_384,
            column_id: 6,
        },
        FieldDescription {
            name: "profile".into(),
            data_type: DataType::Json,
            table_oid: 16_384,
            column_id: 7,
        },
    ]
}

fn pets_rows() -> Vec<Row> {
    vec![
        Row::new(vec![
            Value::Int4(1),
            Value::Text("Rex".into()),
            Value::Boolean(true),
            Value::Float8(12.5),
            Value::Timestamp(1_754_820_900_123_456),
            Value::Uuid(0x0190_1b2c_3d4e_5f60_7182_93a4_b5c6_d7e8),
            Value::Json(r#"{"species":"dog"}"#.into()),
        ]),
        Row::new(vec![
            Value::Int4(2),
            Value::Text("Mœbius".into()),
            Value::Boolean(false),
            Value::Float8(-0.5),
            Value::Timestamp(0),
            Value::Uuid(0),
            Value::Json("null".into()),
        ]),
        // Every column nullable, to exercise the -1 length prefix.
        Row::new(vec![Value::Null; 7]),
    ]
}

/// See the module docs. Records the last caller so tests can assert that
/// the authenticated identity reaches the handler.
#[derive(Default)]
pub struct MockHandler {
    last_caller: Mutex<Option<Identity>>,
}

impl MockHandler {
    pub fn new() -> Self {
        Self::default()
    }

    /// The [`Identity`] passed to the most recent call, if any.
    pub fn last_caller(&self) -> Option<Identity> {
        *self.last_caller.lock().expect("mock handler mutex")
    }

    fn record(&self, caller: Identity) {
        *self.last_caller.lock().expect("mock handler mutex") = Some(caller);
    }

    fn answer(&self, sql: &str, params: &[Value]) -> Result<QueryResult, FerriteError> {
        let normalized = sql.trim().trim_end_matches(';').trim().to_lowercase();
        Ok(match normalized.as_str() {
            "" => QueryResult::empty_query(),
            "select 1" => QueryResult::rows(
                vec![FieldDescription::computed("?column?", DataType::Int4)],
                vec![Row::new(vec![Value::Int4(1)])],
            ),
            "select version()" => QueryResult::rows(
                vec![FieldDescription::computed("version", DataType::Text)],
                vec![Row::new(vec![Value::Text(format!(
                    "Ferrite {} (PostgreSQL wire protocol 3.0)",
                    env!("CARGO_PKG_VERSION")
                ))])],
            ),
            "select * from pets" => QueryResult::rows(pets_fields(), pets_rows()),
            // Ten rows, enough for a row-limited Execute to suspend.
            "select * from generate_series" => QueryResult::rows(
                vec![FieldDescription::computed("n", DataType::Int4)],
                (1..=10).map(|n| Row::new(vec![Value::Int4(n)])).collect(),
            ),
            "select $1::int4" | "select $1" => {
                let value = params.first().cloned().unwrap_or(Value::Null);
                QueryResult::rows(
                    vec![FieldDescription::computed("?column?", DataType::Int4)],
                    vec![Row::new(vec![value])],
                )
            }
            "select $1::text, $2::int4" => QueryResult::rows(
                vec![
                    FieldDescription::computed("text", DataType::Text),
                    FieldDescription::computed("int4", DataType::Int4),
                ],
                vec![Row::new(params.to_vec())],
            ),
            "begin" => QueryResult::command(CommandTag::Begin)
                .with_transaction(TransactionStatus::InTransaction),
            "commit" => {
                QueryResult::command(CommandTag::Commit).with_transaction(TransactionStatus::Idle)
            }
            "rollback" => {
                QueryResult::command(CommandTag::Rollback).with_transaction(TransactionStatus::Idle)
            }
            "boom" => return Err(FerriteError::Exec("the mock handler exploded".into())),
            "select secret" => {
                return Err(FerriteError::PermissionDenied("relation secret".into()))
            }
            other if other.starts_with("insert") => QueryResult::command(CommandTag::Insert(1)),
            other if other.starts_with("update") => QueryResult::command(CommandTag::Update(2)),
            other if other.starts_with("delete") => QueryResult::command(CommandTag::Delete(0)),
            other if other.starts_with("create table") => {
                QueryResult::command(CommandTag::Other("CREATE TABLE".into()))
            }
            other => {
                return Err(FerriteError::Parse(format!(
                    "the mock handler does not know {other:?}"
                )))
            }
        })
    }
}

#[async_trait]
impl QueryHandler for MockHandler {
    async fn execute(&self, sql: &str, caller: Identity) -> Result<QueryResult, FerriteError> {
        self.record(caller);
        self.answer(sql, &[])
    }

    async fn execute_params(
        &self,
        sql: &str,
        params: &[Value],
        caller: Identity,
    ) -> Result<QueryResult, FerriteError> {
        self.record(caller);
        self.answer(sql, params)
    }

    async fn describe(
        &self,
        sql: &str,
        caller: Identity,
    ) -> Result<StatementDescription, FerriteError> {
        self.record(caller);
        let normalized = sql.trim().trim_end_matches(';').trim().to_lowercase();
        let (parameter_types, fields) = match normalized.as_str() {
            "select 1" => (
                vec![],
                Some(vec![FieldDescription::computed("?column?", DataType::Int4)]),
            ),
            "select version()" => (
                vec![],
                Some(vec![FieldDescription::computed("version", DataType::Text)]),
            ),
            "select * from pets" => (vec![], Some(pets_fields())),
            "select * from generate_series" => (
                vec![],
                Some(vec![FieldDescription::computed("n", DataType::Int4)]),
            ),
            "select $1::int4" | "select $1" => (
                vec![Some(DataType::Int4)],
                Some(vec![FieldDescription::computed("?column?", DataType::Int4)]),
            ),
            "select $1::text, $2::int4" => (
                vec![Some(DataType::Text), Some(DataType::Int4)],
                Some(vec![
                    FieldDescription::computed("text", DataType::Text),
                    FieldDescription::computed("int4", DataType::Int4),
                ]),
            ),
            _ => (vec![], None),
        };
        Ok(StatementDescription {
            parameter_types,
            fields,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn answers_the_wired_statements_and_rejects_the_rest() {
        let mock = MockHandler::new();
        let id = Identity([7u8; 32]);
        assert_eq!(
            mock.execute("SELECT 1;", id).await.unwrap().rows,
            vec![Row::new(vec![Value::Int4(1)])]
        );
        assert!(mock.execute("SELECT nonsense", id).await.is_err());
        assert_eq!(mock.last_caller(), Some(id));
    }

    #[tokio::test]
    async fn transaction_commands_report_the_new_state() {
        let mock = MockHandler::new();
        let id = Identity::ANONYMOUS;
        assert_eq!(
            mock.execute("BEGIN", id).await.unwrap().transaction,
            Some(TransactionStatus::InTransaction)
        );
        assert_eq!(
            mock.execute("COMMIT", id).await.unwrap().transaction,
            Some(TransactionStatus::Idle)
        );
    }
}
