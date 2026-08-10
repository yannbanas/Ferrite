//! The per-connection state machine: TLS negotiation, startup,
//! authentication, then the simple and extended query flows.

use std::collections::HashMap;
use std::sync::Arc;

use ferrite_common::{DataType, Identity, Row, Value};
use rand::Rng;
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{debug, info, warn};

use crate::auth::{may_connect, AuthOutcome};
use crate::codec::Framed;
use crate::config::ServerConfig;
use crate::error::{sqlstate, ProtocolError, Result};
use crate::handler::{
    CommandTag, FieldDescription, QueryHandler, QueryResult, StatementDescription,
};
use crate::message::{
    backend, resolve_formats, FieldMeta, Frontend, Severity, StartupParams, StartupRequest,
    TargetKind, TransactionStatus,
};
use crate::types::{self, Format, Oid};

/// Serves one client connection to completion.
///
/// Handles the `SSLRequest` handshake itself, so the caller only has to
/// hand over an accepted socket. Returns `Ok(())` for any orderly end of
/// session, including a client that disconnects mid-stream.
pub async fn serve_connection<S>(stream: S, config: Arc<ServerConfig>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let mut framed = Framed::new(stream, config.max_message_size);
    match negotiate(&mut framed, &config, false).await? {
        Negotiated::Startup(params) => run_session(framed, params, config).await,
        Negotiated::Closed => Ok(()),
        Negotiated::UpgradeTls => {
            let acceptor = config
                .tls
                .acceptor()
                .ok_or_else(|| ProtocolError::Tls("no TLS acceptor configured".into()))?
                .clone();
            framed.write_raw(b"S").await?;
            let stream = acceptor
                .accept(framed.into_inner())
                .await
                .map_err(|e| ProtocolError::Tls(e.to_string()))?;
            let mut framed = Framed::new(stream, config.max_message_size);
            match negotiate(&mut framed, &config, true).await? {
                Negotiated::Startup(params) => run_session(framed, params, config).await,
                Negotiated::Closed => Ok(()),
                // Unreachable: `negotiate` refuses a second SSLRequest once
                // the channel is already encrypted.
                Negotiated::UpgradeTls => Err(ProtocolError::malformed("nested SSLRequest")),
            }
        }
    }
}

enum Negotiated {
    Startup(StartupParams),
    UpgradeTls,
    Closed,
}

/// Consumes the untagged packets a client may send before `StartupMessage`.
async fn negotiate<S>(
    framed: &mut Framed<S>,
    config: &ServerConfig,
    encrypted: bool,
) -> Result<Negotiated>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    // libpq may send GSSENCRequest, then SSLRequest, then the startup
    // packet. Two refusals is the most any client sends; the bound stops a
    // peer from looping here for free.
    for _ in 0..3 {
        match framed.read_startup().await? {
            StartupRequest::SslRequest => {
                if encrypted {
                    return Err(ProtocolError::malformed(
                        "SSLRequest on an already encrypted connection",
                    ));
                }
                if config.tls.is_required() {
                    return Ok(Negotiated::UpgradeTls);
                }
                framed.write_raw(b"N").await?;
            }
            // GSSAPI encryption is not supported and is not planned: TLS
            // covers the same ground for every client Ferrite targets.
            StartupRequest::GssEncRequest => framed.write_raw(b"N").await?,
            StartupRequest::Cancel { process_id, .. } => {
                // Query cancellation is not implemented; the request is
                // acknowledged by closing, as PostgreSQL does for an
                // unmatched key.
                debug!(process_id, "ignoring cancel request: not implemented");
                return Ok(Negotiated::Closed);
            }
            StartupRequest::Startup(params) => {
                if config.tls.is_required() && !encrypted {
                    ferrite_metrics::metrics().connections_rejected_total.inc();
                    warn!(user = %params.user, "refused a cleartext startup: TLS is required");
                    framed.send(backend::error_response(
                        Severity::Fatal,
                        sqlstate::INVALID_AUTHORIZATION,
                        "this server requires TLS; reconnect with sslmode=require",
                    ));
                    framed.flush().await?;
                    return Err(ProtocolError::TlsRequired);
                }
                return Ok(Negotiated::Startup(params));
            }
        }
    }
    Err(ProtocolError::malformed(
        "too many pre-startup packets without a StartupMessage",
    ))
}

async fn run_session<S>(
    mut framed: Framed<S>,
    params: StartupParams,
    config: Arc<ServerConfig>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let outcome = match authenticate(&mut framed, &params, &config).await {
        Ok(outcome) => outcome,
        Err(err) => {
            ferrite_metrics::metrics().connections_rejected_total.inc();
            let (severity, state, message) = match &err {
                ProtocolError::AuthFailed(user) => (
                    Severity::Fatal,
                    sqlstate::INVALID_PASSWORD,
                    format!("password authentication failed for user {user:?}"),
                ),
                other => (
                    Severity::Fatal,
                    other.sqlstate(),
                    "authentication failed".to_owned(),
                ),
            };
            framed.send(backend::error_response(severity, state, &message));
            let _ = framed.flush().await;
            return Err(err);
        }
    };

    let (process_id, secret_key) = {
        let mut rng = rand::thread_rng();
        (rng.gen_range(1..i32::MAX), rng.gen())
    };
    framed.send(backend::authentication_ok());
    for (key, value) in config.parameter_status(&params) {
        framed.send(backend::parameter_status(&key, &value));
    }
    framed.send(backend::backend_key_data(process_id, secret_key));
    framed.send(backend::ready_for_query(TransactionStatus::Idle));
    framed.flush().await?;

    info!(
        user = %params.user,
        database = %params.database,
        role = %outcome.role.name,
        "session established"
    );

    // A handler with per-session state (an open transaction) hands out one
    // of its own here; a stateless one keeps the shared instance.
    let handler = config
        .handler
        .connect()
        .unwrap_or_else(|| Arc::clone(&config.handler));

    let mut session = Session {
        framed,
        handler,
        identity: outcome.identity,
        transaction: TransactionStatus::Idle,
        statements: HashMap::new(),
        portals: HashMap::new(),
        skip_until_sync: false,
    };
    session.run().await
}

async fn authenticate<S>(
    framed: &mut Framed<S>,
    params: &StartupParams,
    config: &ServerConfig,
) -> Result<AuthOutcome>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    framed.send(backend::authentication_cleartext_password());
    framed.flush().await?;
    let password = match framed.read_message().await? {
        // The password message is a NUL-terminated string; the terminator
        // is not part of the secret.
        Frontend::Password(mut raw) => {
            if raw.last() == Some(&0) {
                raw.pop();
            }
            raw
        }
        Frontend::Terminate => return Err(ProtocolError::Closed),
        _ => {
            return Err(ProtocolError::malformed(
                "expected a password message during authentication",
            ))
        }
    };

    let outcome = config
        .authenticator
        .authenticate(&params.user, &params.database, &password)
        .await
        .inspect_err(|_| {
            ferrite_metrics::metrics().auth_failures_total.inc();
            warn!(user = %params.user, "authentication failed");
        })?;

    if !may_connect(&outcome.role) {
        ferrite_metrics::metrics().auth_failures_total.inc();
        warn!(
            user = %params.user,
            role = %outcome.role.name,
            "connection denied: role lacks the Connect permission"
        );
        return Err(ProtocolError::AuthFailed(params.user.clone()));
    }
    Ok(outcome)
}

struct Prepared {
    sql: String,
    param_types: Vec<DataType>,
    fields: Vec<FieldDescription>,
    /// False when the handler could not describe the statement, in which
    /// case `Describe` answers `NoData` and the row description is only
    /// known once `Execute` has run.
    described: bool,
}

struct Portal {
    statement: String,
    params: Vec<Value>,
    result_formats: Vec<Format>,
    pending: Option<Pending>,
}

/// A partially delivered result set, kept so a row-limited `Execute` can be
/// resumed by the next one.
struct Pending {
    field_count: usize,
    rows: std::vec::IntoIter<Row>,
    tag: CommandTag,
    sent: u64,
    empty_query: bool,
}

struct Session<S> {
    framed: Framed<S>,
    handler: Arc<dyn QueryHandler>,
    identity: Identity,
    transaction: TransactionStatus,
    statements: HashMap<String, Prepared>,
    portals: HashMap<String, Portal>,
    skip_until_sync: bool,
}

impl<S> Session<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    async fn run(&mut self) -> Result<()> {
        loop {
            let message = match self.framed.read_message().await {
                Ok(message) => message,
                Err(ProtocolError::Closed) => return Ok(()),
                Err(err) => {
                    self.report_fatal(&err).await;
                    return Err(err);
                }
            };
            if matches!(message, Frontend::Terminate) {
                return Ok(());
            }
            match self.dispatch(message).await {
                Ok(()) => {}
                Err(err) if err.is_recoverable() => {
                    self.report_error(&err);
                    self.framed.flush().await?;
                }
                Err(ProtocolError::Closed) => return Ok(()),
                Err(err) => {
                    self.report_fatal(&err).await;
                    return Err(err);
                }
            }
        }
    }

    async fn dispatch(&mut self, message: Frontend) -> Result<()> {
        // Once a message in an extended-query batch fails, everything up to
        // the next Sync is discarded — that is what keeps the client and
        // server in step after an error.
        if self.skip_until_sync && !matches!(message, Frontend::Sync) {
            return Ok(());
        }
        match message {
            Frontend::Query(sql) => self.simple_query(&sql).await,
            Frontend::Parse {
                name,
                sql,
                param_types,
            } => self.parse(name, sql, &param_types).await,
            Frontend::Bind {
                portal,
                statement,
                param_formats,
                params,
                result_formats,
            } => {
                self.bind(portal, statement, &param_formats, params, &result_formats)
                    .await
            }
            Frontend::Describe { kind, name } => self.describe(kind, &name).await,
            Frontend::Execute { portal, max_rows } => self.execute(&portal, max_rows).await,
            Frontend::Close { kind, name } => {
                match kind {
                    TargetKind::Statement => {
                        self.statements.remove(&name);
                        self.portals.retain(|_, p| p.statement != name);
                    }
                    TargetKind::Portal => {
                        self.portals.remove(&name);
                    }
                }
                self.framed.send(backend::close_complete());
                Ok(())
            }
            Frontend::Sync => {
                self.skip_until_sync = false;
                self.framed.send(backend::ready_for_query(self.transaction));
                self.framed.flush().await
            }
            Frontend::Flush => self.framed.flush().await,
            Frontend::Terminate => Ok(()),
            Frontend::Password(_) => Err(ProtocolError::malformed(
                "unexpected password message outside authentication",
            )),
            Frontend::Unsupported(tag) => Err(ProtocolError::malformed(format!(
                "message type {:?} is not supported",
                tag as char
            ))),
        }
    }

    async fn simple_query(&mut self, sql: &str) -> Result<()> {
        // The whole string goes to the handler as one statement: splitting
        // a multi-statement query needs the SQL lexer, which lives in
        // `ferrite-sql`, not here.
        let result = if sql.trim().is_empty() {
            QueryResult::empty_query()
        } else {
            let handler = Arc::clone(&self.handler);
            match handler.execute(sql, self.identity).await {
                Ok(result) => result,
                Err(err) => {
                    let err = ProtocolError::Ferrite(err);
                    debug!(error = %err, "statement failed");
                    self.fail_transaction();
                    self.send_error(&err);
                    self.framed.send(backend::ready_for_query(self.transaction));
                    return self.framed.flush().await;
                }
            }
        };
        self.apply_transaction(&result);

        if result.empty_query {
            self.framed.send(backend::empty_query_response());
        } else {
            let formats = vec![Format::Text; result.fields.len()];
            if !result.fields.is_empty() {
                self.framed.send(row_description(&result.fields, &formats));
            }
            for row in &result.rows {
                self.framed.send(encode_row(row, &formats));
            }
            self.framed
                .send(backend::command_complete(&result.tag.to_wire()));
        }
        self.framed.send(backend::ready_for_query(self.transaction));
        self.framed.flush().await
    }

    async fn parse(&mut self, name: String, sql: String, declared: &[Oid]) -> Result<()> {
        let handler = Arc::clone(&self.handler);
        let description = handler
            .describe(&sql, self.identity)
            .await
            .map_err(ProtocolError::Ferrite)?;
        let param_types = merge_parameter_types(declared, &description);
        let (fields, described) = match description.fields {
            Some(fields) => (fields, true),
            None => (Vec::new(), false),
        };
        // An unnamed statement is silently replaced; a named one may not be
        // redefined while it exists, per the protocol spec.
        if !name.is_empty() && self.statements.contains_key(&name) {
            return Err(ProtocolError::Ferrite(ferrite_common::FerriteError::Exec(
                format!("prepared statement {name:?} already exists"),
            )));
        }
        self.statements.insert(
            name,
            Prepared {
                sql,
                param_types,
                fields,
                described,
            },
        );
        self.framed.send(backend::parse_complete());
        Ok(())
    }

    async fn bind(
        &mut self,
        portal: String,
        statement: String,
        param_formats: &[Format],
        raw_params: Vec<Option<Vec<u8>>>,
        result_formats: &[Format],
    ) -> Result<()> {
        let prepared = self.statement(&statement)?;
        let formats = resolve_formats(param_formats, raw_params.len())?;
        let mut params = Vec::with_capacity(raw_params.len());
        for (i, raw) in raw_params.iter().enumerate() {
            // An OID the client left unspecified and the engine could not
            // infer is decoded as text, which is what PostgreSQL does.
            let ty = prepared
                .param_types
                .get(i)
                .copied()
                .unwrap_or(DataType::Text);
            params.push(types::decode_value(ty, formats[i], raw.as_deref())?);
        }
        let field_count = prepared.fields.len();
        let result_formats = if result_formats.is_empty() {
            vec![Format::Text; field_count]
        } else {
            result_formats.to_vec()
        };
        self.portals.insert(
            portal,
            Portal {
                statement,
                params,
                result_formats,
                pending: None,
            },
        );
        self.framed.send(backend::bind_complete());
        Ok(())
    }

    async fn describe(&mut self, kind: TargetKind, name: &str) -> Result<()> {
        let frames = match kind {
            TargetKind::Statement => {
                let prepared = self.statement(name)?;
                let oids: Vec<Oid> = prepared
                    .param_types
                    .iter()
                    .map(|t| types::type_oid(*t))
                    .collect();
                // Result formats are only chosen at Bind, so a statement's
                // row description always advertises text.
                let formats = vec![Format::Text; prepared.fields.len()];
                let shape = if prepared.described && !prepared.fields.is_empty() {
                    row_description(&prepared.fields, &formats)
                } else {
                    backend::no_data()
                };
                vec![backend::parameter_description(&oids), shape]
            }
            TargetKind::Portal => {
                let portal = self.portal(name)?;
                let requested = portal.result_formats.clone();
                let prepared = self.statement(&portal.statement)?;
                if prepared.described && !prepared.fields.is_empty() {
                    let formats = resolve_formats(&requested, prepared.fields.len())?;
                    vec![row_description(&prepared.fields, &formats)]
                } else {
                    vec![backend::no_data()]
                }
            }
        };
        for frame in frames {
            self.framed.send(frame);
        }
        Ok(())
    }

    async fn execute(&mut self, name: &str, max_rows: i32) -> Result<()> {
        if self.portal(name)?.pending.is_none() {
            let (statement, params) = {
                let portal = self.portal(name)?;
                (portal.statement.clone(), portal.params.clone())
            };
            let sql = self.statement(&statement)?.sql.clone();
            let handler = Arc::clone(&self.handler);
            let result = match handler.execute_params(&sql, &params, self.identity).await {
                Ok(result) => result,
                Err(err) => {
                    self.fail_transaction();
                    return Err(ProtocolError::Ferrite(err));
                }
            };
            self.apply_transaction(&result);
            // A handler may only learn the shape of its output by running
            // the statement; remember it so a later Describe is accurate.
            if !result.fields.is_empty() {
                if let Some(prepared) = self.statements.get_mut(&statement) {
                    if !prepared.described {
                        prepared.fields.clone_from(&result.fields);
                        prepared.described = true;
                    }
                }
            }
            self.portal_mut(name)?.pending = Some(Pending {
                field_count: result.fields.len(),
                rows: result.rows.into_iter(),
                tag: result.tag,
                sent: 0,
                empty_query: result.empty_query,
            });
        }

        let (requested, empty_query, field_count) = {
            let portal = self.portal(name)?;
            let pending = portal.pending.as_ref().expect("pending was just installed");
            (
                portal.result_formats.clone(),
                pending.empty_query,
                pending.field_count,
            )
        };
        if empty_query {
            self.framed.send(backend::empty_query_response());
            self.portal_mut(name)?.pending = None;
            return Ok(());
        }

        let formats = resolve_formats(&requested, field_count)?;
        let limit = if max_rows == 0 {
            usize::MAX
        } else {
            max_rows as usize
        };
        let (frames, exhausted, tag) = {
            let pending = self
                .portal_mut(name)?
                .pending
                .as_mut()
                .expect("pending was just installed");
            let mut frames = Vec::new();
            while frames.len() < limit {
                match pending.rows.next() {
                    Some(row) => frames.push(encode_row(&row, &formats)),
                    None => break,
                }
            }
            pending.sent += frames.len() as u64;
            let exhausted = pending.rows.len() == 0;
            (frames, exhausted, finished_tag(&pending.tag, pending.sent))
        };
        for frame in frames {
            self.framed.send(frame);
        }
        if exhausted {
            self.framed.send(backend::command_complete(&tag));
            self.portal_mut(name)?.pending = None;
        } else {
            self.framed.send(backend::portal_suspended());
        }
        Ok(())
    }

    fn statement(&self, name: &str) -> Result<&Prepared> {
        self.statements.get(name).ok_or_else(|| {
            ProtocolError::Ferrite(ferrite_common::FerriteError::Exec(format!(
                "prepared statement {name:?} does not exist"
            )))
        })
    }

    fn portal(&self, name: &str) -> Result<&Portal> {
        self.portals.get(name).ok_or_else(|| {
            ProtocolError::Ferrite(ferrite_common::FerriteError::Exec(format!(
                "portal {name:?} does not exist"
            )))
        })
    }

    fn portal_mut(&mut self, name: &str) -> Result<&mut Portal> {
        self.portals.get_mut(name).ok_or_else(|| {
            ProtocolError::Ferrite(ferrite_common::FerriteError::Exec(format!(
                "portal {name:?} does not exist"
            )))
        })
    }

    fn apply_transaction(&mut self, result: &QueryResult) {
        if let Some(status) = result.transaction {
            self.transaction = status;
        }
    }

    fn fail_transaction(&mut self) {
        if self.transaction == TransactionStatus::InTransaction {
            self.transaction = TransactionStatus::Failed;
        }
    }

    fn send_error(&mut self, err: &ProtocolError) {
        self.framed.send(backend::error_response(
            Severity::Error,
            err.sqlstate(),
            &err.to_string(),
        ));
    }

    /// Extended-flow failure: report it and drop everything until the next
    /// `Sync`, which is how the protocol resynchronises after an error.
    fn report_error(&mut self, err: &ProtocolError) {
        debug!(error = %err, "statement failed");
        self.skip_until_sync = true;
        self.fail_transaction();
        self.send_error(err);
    }

    async fn report_fatal(&mut self, err: &ProtocolError) {
        if matches!(err, ProtocolError::Closed | ProtocolError::Io(_)) {
            return;
        }
        warn!(error = %err, "closing connection");
        self.framed.send(backend::error_response(
            Severity::Fatal,
            err.sqlstate(),
            &err.to_string(),
        ));
        let _ = self.framed.flush().await;
    }
}

/// A row count is only known once the rows have been delivered, so a
/// `SELECT` tag is rebuilt from what was actually sent.
fn finished_tag(tag: &CommandTag, sent: u64) -> String {
    match tag {
        CommandTag::Select(_) => CommandTag::Select(sent).to_wire(),
        other => other.to_wire(),
    }
}

/// Resolves each parameter's type from what the client declared in `Parse`
/// and what the engine inferred, in that order of precedence.
fn merge_parameter_types(declared: &[Oid], description: &StatementDescription) -> Vec<DataType> {
    let count = declared.len().max(description.parameter_types.len());
    (0..count)
        .map(|i| {
            types::type_from_oid(declared.get(i).copied().unwrap_or(0))
                .or_else(|| description.parameter_types.get(i).copied().flatten())
                // Unspecified on both sides: text is the safe fallback,
                // since every client can render a parameter as text.
                .unwrap_or(DataType::Text)
        })
        .collect()
}

fn row_description(fields: &[FieldDescription], formats: &[Format]) -> Vec<u8> {
    let metas: Vec<FieldMeta<'_>> = fields
        .iter()
        .enumerate()
        .map(|(i, f)| FieldMeta {
            name: &f.name,
            type_oid: types::type_oid(f.data_type),
            type_size: types::type_size(f.data_type),
            type_modifier: -1,
            table_oid: f.table_oid,
            column_id: f.column_id,
            format: formats.get(i).copied().unwrap_or_default(),
        })
        .collect();
    backend::row_description(&metas)
}

fn encode_row(row: &Row, formats: &[Format]) -> Vec<u8> {
    let values: Vec<Option<Vec<u8>>> = row
        .values
        .iter()
        .enumerate()
        .map(|(i, v)| types::encode_value(v, formats.get(i).copied().unwrap_or_default()))
        .collect();
    backend::data_row(&values)
}
