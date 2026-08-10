//! The real engine behind the wire protocol: storage + catalog + parser +
//! planner + executor + procedures, assembled once and shared by every
//! connection.
//!
//! [`Engine`] is the shared half. [`QueryHandler::connect`] hands each
//! session a [`Connection`], which is where the one piece of genuinely
//! per-session state lives: the transaction opened by `BEGIN`.
//!
//! Statements split three ways:
//!
//! - **DDL** (`CREATE`/`DROP TABLE`, `CREATE`/`DROP INDEX`) goes straight
//!   to `ferrite-catalog`. It is not in the v1 plan set, and the catalog's
//!   `*_in` methods let it join the session's transaction.
//! - **Transaction control** is handled here, since it *is* the session
//!   state.
//! - **Everything else** goes through `ferrite-planner` and
//!   `ferrite-exec`.
//!
//! Storage is synchronous and takes one engine-wide lock, so every
//! statement runs on `tokio::task::spawn_blocking`: a long scan must not
//! park a runtime worker other connections need.

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ferrite_catalog::SystemCatalog;
use ferrite_common::{
    Catalog, FerriteError, Identity, IndexCatalog, Permission, Row, Schema, StorageEngine, TxnId,
    Value,
};
use ferrite_exec::{QueryResult as ExecResult, Session};
use ferrite_planner::{Planner, DEFAULT_NAMESPACE};
use ferrite_proc::ProcRegistry;
use ferrite_protocol::{
    CommandTag, FieldDescription, QueryHandler, QueryResult, StatementDescription,
    TransactionStatus,
};
use ferrite_sql::ast as sql;
use ferrite_storage::{FerriteStorage, DATA_FILE};
use tracing::debug;

use crate::describe::parameter_types;

/// Everything shared between connections. Cloning is two atomic bumps.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

struct Inner {
    storage: Arc<FerriteStorage>,
    catalog: Arc<SystemCatalog>,
    procs: Arc<ProcRegistry>,
}

impl Engine {
    /// Open the database in `dir`, bootstrapping the catalog if the data
    /// file is not there yet.
    ///
    /// The freshness test is the presence of the data file rather than a
    /// failed `SystemCatalog::open`: falling back to `bootstrap` on any
    /// error would turn a corrupt catalog into a silently empty database.
    pub fn open(dir: impl AsRef<Path>, procs: ProcRegistry) -> Result<Self, FerriteError> {
        let dir = dir.as_ref();
        let fresh = !dir.join(DATA_FILE).exists();
        let storage = Arc::new(FerriteStorage::open(dir)?);

        let catalog = if fresh {
            SystemCatalog::bootstrap(storage.clone() as Arc<dyn StorageEngine>)?
        } else {
            SystemCatalog::open(storage.clone() as Arc<dyn StorageEngine>)?
        };

        Ok(Self {
            inner: Arc::new(Inner {
                storage,
                catalog: Arc::new(catalog),
                procs: Arc::new(procs),
            }),
        })
    }

    /// Flush every cached page and truncate the journal. Called on an
    /// orderly shutdown so a restart does not have to replay.
    pub fn checkpoint(&self) -> Result<(), FerriteError> {
        self.inner.storage.checkpoint()
    }

    fn session(&self) -> Connection {
        Connection {
            session: Arc::new(SessionHandle {
                inner: Arc::clone(&self.inner),
                state: Mutex::new(SessionState::default()),
            }),
        }
    }
}

/// The transaction a connection opened with `BEGIN`, if any.
#[derive(Default)]
struct SessionState {
    txn: Option<TxnId>,
    /// Set when DDL ran inside `txn`. `ferrite-catalog` updates its
    /// in-memory index optimistically, so an aborted transaction that ran
    /// DDL has to be followed by `reload()`.
    ddl: bool,
}

/// One client connection's view of the engine.
pub struct Connection {
    session: Arc<SessionHandle>,
}

/// The session itself, behind an `Arc` so a statement can be moved onto a
/// blocking thread without the session looking dropped when that thread is
/// done with it. Aborting the transaction belongs to *this* type's `Drop`,
/// not to [`Connection`]'s, for exactly that reason.
struct SessionHandle {
    inner: Arc<Inner>,
    state: Mutex<SessionState>,
}

impl SessionHandle {
    /// Run a whole SQL string. A simple `Query` message may carry several
    /// statements; each runs in order and the last one's result is what
    /// goes back, which is the part a client reads.
    fn run(
        &self,
        text: &str,
        params: &[Value],
        caller: Identity,
    ) -> Result<QueryResult, FerriteError> {
        let statements = ferrite_sql::parse(text)?;
        if statements.is_empty() {
            return Ok(QueryResult::empty_query());
        }
        let mut last = QueryResult::empty_query();
        for statement in &statements {
            last = self.run_one(statement, params, caller)?;
        }
        Ok(last)
    }

    fn run_one(
        &self,
        statement: &sql::Statement,
        params: &[Value],
        caller: Identity,
    ) -> Result<QueryResult, FerriteError> {
        match statement {
            sql::Statement::Begin => self.begin(),
            sql::Statement::Commit => self.commit(),
            sql::Statement::Rollback => self.rollback(),

            sql::Statement::CreateTable(_)
            | sql::Statement::DropTable(_)
            | sql::Statement::CreateIndex(_)
            | sql::Statement::DropIndex(_) => self.in_transaction(|txn| {
                self.inner
                    .procs
                    .authorize(caller, txn, Permission::CreateTable)?;
                self.ddl(statement, txn)
            }),

            other => self.in_transaction(|txn| {
                let plan = Planner::new(
                    self.inner.catalog.as_ref(),
                    self.inner.catalog.as_ref() as &dyn IndexCatalog,
                )
                .with_params(params)
                .plan(other)?;
                let session = Session::new(
                    self.inner.storage.as_ref(),
                    self.inner.catalog.as_ref(),
                    self.inner.procs.as_ref(),
                    caller,
                );
                Ok(to_wire(session.execute(txn, &plan)?, other))
            }),
        }
    }

    /// Run `body` under the session's transaction if one is open, otherwise
    /// under a transaction of this statement's own — PostgreSQL's implicit
    /// autocommit.
    ///
    /// The snapshot is re-taken per statement, which is read-committed:
    /// without it a transaction opened before a `CREATE TABLE` committed
    /// elsewhere could not see the new table at all.
    fn in_transaction<T>(
        &self,
        body: impl FnOnce(TxnId) -> Result<T, FerriteError>,
    ) -> Result<T, FerriteError> {
        if let Some(txn) = self.current_txn()? {
            self.inner.storage.snapshot(txn)?;
            return body(txn);
        }
        let txn = self.inner.storage.begin()?;
        match body(txn) {
            Ok(value) => {
                self.inner.storage.commit(txn)?;
                Ok(value)
            }
            Err(err) => {
                let _ = self.inner.storage.abort(txn);
                Err(err)
            }
        }
    }

    fn current_txn(&self) -> Result<Option<TxnId>, FerriteError> {
        Ok(self.lock()?.txn)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, SessionState>, FerriteError> {
        self.state
            .lock()
            .map_err(|_| FerriteError::Exec("session state lock poisoned".into()))
    }

    fn begin(&self) -> Result<QueryResult, FerriteError> {
        let mut state = self.lock()?;
        if state.txn.is_none() {
            state.txn = Some(self.inner.storage.begin()?);
        }
        Ok(QueryResult::command(CommandTag::Begin)
            .with_transaction(TransactionStatus::InTransaction))
    }

    fn commit(&self) -> Result<QueryResult, FerriteError> {
        let mut state = self.lock()?;
        if let Some(txn) = state.txn.take() {
            state.ddl = false;
            self.inner.storage.commit(txn)?;
        }
        Ok(QueryResult::command(CommandTag::Commit).with_transaction(TransactionStatus::Idle))
    }

    fn rollback(&self) -> Result<QueryResult, FerriteError> {
        let mut state = self.lock()?;
        if let Some(txn) = state.txn.take() {
            let _ = self.inner.storage.abort(txn);
            if std::mem::take(&mut state.ddl) {
                // The catalog updated its in-memory index when the DDL ran;
                // nothing else undoes that for an aborted transaction.
                self.inner.catalog.reload()?;
            }
        }
        Ok(QueryResult::command(CommandTag::Rollback).with_transaction(TransactionStatus::Idle))
    }

    fn ddl(&self, statement: &sql::Statement, txn: TxnId) -> Result<QueryResult, FerriteError> {
        if self.current_txn()? == Some(txn) {
            self.lock()?.ddl = true;
        }
        let catalog = self.inner.catalog.as_ref();

        let tag = match statement {
            sql::Statement::CreateTable(create) => {
                let (namespace, name) = create.name.split(DEFAULT_NAMESPACE);
                if catalog.table_id(namespace, name)?.is_some() {
                    if create.if_not_exists {
                        return Ok(QueryResult::command(CommandTag::Other(
                            "CREATE TABLE".into(),
                        )));
                    }
                    return Err(FerriteError::ObjectAlreadyExists(format!(
                        "{namespace}.{name}"
                    )));
                }
                catalog.create_table_in(txn, namespace, name, create.to_schema())?;
                "CREATE TABLE"
            }

            sql::Statement::DropTable(drop) => {
                for name in &drop.names {
                    let (namespace, table) = name.split(DEFAULT_NAMESPACE);
                    match catalog.table_id(namespace, table)? {
                        Some(id) => catalog.drop_table_in(txn, id)?,
                        None if drop.if_exists => {}
                        None => {
                            return Err(FerriteError::TableNotFound(format!("{namespace}.{table}")))
                        }
                    }
                }
                "DROP TABLE"
            }

            sql::Statement::CreateIndex(create) => {
                let (namespace, table) = create.table.split(DEFAULT_NAMESPACE);
                let id = catalog
                    .table_id(namespace, table)?
                    .ok_or_else(|| FerriteError::TableNotFound(format!("{namespace}.{table}")))?;
                if catalog.index_by_name(namespace, &create.name)?.is_some() {
                    if create.if_not_exists {
                        return Ok(QueryResult::command(CommandTag::Other(
                            "CREATE INDEX".into(),
                        )));
                    }
                    return Err(FerriteError::ObjectAlreadyExists(create.name.clone()));
                }
                catalog.create_index_in(txn, &create.name, id, &create.columns, create.unique)?;
                "CREATE INDEX"
            }

            sql::Statement::DropIndex(drop) => {
                match catalog.index_by_name(DEFAULT_NAMESPACE, &drop.name)? {
                    Some(def) => catalog.drop_index_in(txn, def.id)?,
                    None if drop.if_exists => {}
                    None => return Err(FerriteError::TableNotFound(drop.name.clone())),
                }
                "DROP INDEX"
            }

            other => {
                return Err(FerriteError::Plan(format!(
                    "not a DDL statement: {other:?}"
                )))
            }
        };
        Ok(QueryResult::command(CommandTag::Other(tag.into())))
    }

    fn describe_statement(&self, text: &str) -> Result<StatementDescription, FerriteError> {
        let statements = ferrite_sql::parse(text)?;
        let Some(statement) = statements.last() else {
            return Ok(StatementDescription::unknown());
        };
        let parameter_types = parameter_types(statement, self.inner.catalog.as_ref());

        // Result columns come from planning the statement. Placeholders are
        // stood in for by NULL: the output shape depends on the projection
        // and the table's schema, never on a bound value.
        let params = vec![Value::Null; parameter_types.len()];
        let fields = Planner::new(
            self.inner.catalog.as_ref(),
            self.inner.catalog.as_ref() as &dyn IndexCatalog,
        )
        .with_params(&params)
        .plan(statement)
        .ok()
        .and_then(|plan| plan.output_schema().map(fields_of));

        Ok(StatementDescription {
            parameter_types,
            fields,
        })
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        // A client that disconnects mid-transaction must not leave it open:
        // storage counts it as active, which holds back version pruning and
        // the reclaiming of dropped tables.
        if let Ok(mut state) = self.state.lock() {
            if let Some(txn) = state.txn.take() {
                debug!(txn, "connection closed with an open transaction: aborting");
                let _ = self.inner.storage.abort(txn);
                if state.ddl {
                    let _ = self.inner.catalog.reload();
                }
            }
        }
    }
}

#[async_trait]
impl QueryHandler for Engine {
    fn connect(&self) -> Option<Arc<dyn QueryHandler>> {
        Some(Arc::new(self.session()))
    }

    async fn execute(&self, sql: &str, caller: Identity) -> Result<QueryResult, FerriteError> {
        self.session().execute(sql, caller).await
    }

    async fn execute_params(
        &self,
        sql: &str,
        params: &[Value],
        caller: Identity,
    ) -> Result<QueryResult, FerriteError> {
        self.session().execute_params(sql, params, caller).await
    }

    async fn describe(
        &self,
        sql: &str,
        caller: Identity,
    ) -> Result<StatementDescription, FerriteError> {
        self.session().describe(sql, caller).await
    }
}

#[async_trait]
impl QueryHandler for Connection {
    async fn execute(&self, sql: &str, caller: Identity) -> Result<QueryResult, FerriteError> {
        self.execute_params(sql, &[], caller).await
    }

    async fn execute_params(
        &self,
        sql: &str,
        params: &[Value],
        caller: Identity,
    ) -> Result<QueryResult, FerriteError> {
        let (sql, params) = (sql.to_owned(), params.to_vec());
        offload(&self.session, move |s| s.run(&sql, &params, caller)).await
    }

    async fn describe(
        &self,
        sql: &str,
        _caller: Identity,
    ) -> Result<StatementDescription, FerriteError> {
        let sql = sql.to_owned();
        offload(&self.session, move |s| s.describe_statement(&sql)).await
    }
}

/// Move the work onto a blocking thread: everything below here is
/// synchronous and takes the storage engine's lock, and a long scan must
/// not park a runtime worker other connections need.
async fn offload<T, F>(session: &Arc<SessionHandle>, body: F) -> Result<T, FerriteError>
where
    T: Send + 'static,
    F: FnOnce(&SessionHandle) -> Result<T, FerriteError> + Send + 'static,
{
    let session = Arc::clone(session);
    tokio::task::spawn_blocking(move || body(session.as_ref()))
        .await
        .map_err(|err| FerriteError::Exec(format!("engine task failed: {err}")))?
}

fn fields_of(schema: &Schema) -> Vec<FieldDescription> {
    schema
        .columns
        .iter()
        .map(|column| FieldDescription::computed(&column.name, column.data_type))
        .collect()
}

/// The executor reports what happened; the wire wants a `CommandComplete`
/// tag naming the statement that made it happen.
fn to_wire(result: ExecResult, statement: &sql::Statement) -> QueryResult {
    match result {
        ExecResult::Rows { schema, rows } => QueryResult::rows(fields_of(&schema), rows),
        ExecResult::Affected(n) => {
            let n = n as u64;
            QueryResult::command(match statement {
                sql::Statement::Insert(_) => CommandTag::Insert(n),
                sql::Statement::Update(_) => CommandTag::Update(n),
                sql::Statement::Delete(_) => CommandTag::Delete(n),
                _ => CommandTag::Other(format!("AFFECTED {n}")),
            })
        }
        ExecResult::Value(value) => QueryResult::rows(
            vec![FieldDescription::computed(
                "result",
                value.data_type().unwrap_or(ferrite_common::DataType::Text),
            )],
            vec![Row::new(vec![value])],
        ),
    }
}
