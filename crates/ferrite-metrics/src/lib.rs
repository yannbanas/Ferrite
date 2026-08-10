//! Ferrite's metrics registry and the HTTP endpoint that exposes it.
//!
//! # Why a hand-rolled registry
//!
//! The exposition format is a dozen lines of text and the primitives are
//! atomics; pulling in `prometheus` or `metrics` + `metrics-exporter-
//! prometheus` would add a transitive tree (and a `cargo-audit` surface)
//! larger than the code it replaces. This crate is the same call the rest
//! of the workspace already makes for the PostgreSQL wire protocol itself:
//! the format is stable, small and fully specified, so it is implemented
//! rather than depended upon. See `registry.rs` for the primitives.
//!
//! # Why a separate port
//!
//! Metrics are served over plain HTTP on their own listener (default
//! `0.0.0.0:9187`, the port `postgres_exporter` conventionally uses) rather
//! than smuggled into the PostgreSQL protocol on 5432. Every scraper and
//! every orchestrator health check already speaks HTTP, nothing has to
//! authenticate as a database user to read a counter, and the exposition
//! path shares no state with the connection state machine — a wedged
//! session cannot take the endpoint down with it.
//!
//! The endpoint is unauthenticated by design, as Prometheus endpoints
//! generally are. It exposes counts and latencies, never row contents, but
//! it should still be kept on an internal network rather than published to
//! the world.
//!
//! # Recording
//!
//! [`metrics()`] hands out the process-wide registry, initialised on first
//! use. Recording never fails and never needs configuration, so
//! instrumentation can sit on the query path without a branch on whether
//! the endpoint happens to be enabled.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

pub mod http;
mod registry;

pub use http::Endpoint;
pub use registry::{Counter, CounterVec, Encoder, Gauge, Histogram, Label};

/// What kind of statement was executed, for `ferrite_queries_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementKind {
    Select,
    Insert,
    Update,
    Delete,
    Ddl,
    Transaction,
    Call,
    Other,
}

impl Label for StatementKind {
    const VALUES: &'static [&'static str] = &[
        "select",
        "insert",
        "update",
        "delete",
        "ddl",
        "transaction",
        "call",
        "other",
    ];
    const NAME: &'static str = "kind";

    fn index(self) -> usize {
        match self {
            StatementKind::Select => 0,
            StatementKind::Insert => 1,
            StatementKind::Update => 2,
            StatementKind::Delete => 3,
            StatementKind::Ddl => 4,
            StatementKind::Transaction => 5,
            StatementKind::Call => 6,
            StatementKind::Other => 7,
        }
    }
}

/// Error categories for `ferrite_query_errors_total`, one per
/// `ferrite_common::FerriteError` variant.
///
/// The mapping lives in `ferrite-server` rather than here so this crate
/// keeps no dependency on the rest of the workspace; the categories are
/// spelled out below so a dashboard can be written against a fixed set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    TableNotFound,
    ColumnNotFound,
    TypeMismatch,
    RowNotFound,
    TxnNotActive,
    PermissionDenied,
    SerializationFailure,
    Storage,
    Parse,
    Plan,
    Exec,
    Protocol,
    ObjectAlreadyExists,
    InvalidDefinition,
}

impl Label for ErrorKind {
    const VALUES: &'static [&'static str] = &[
        "table_not_found",
        "column_not_found",
        "type_mismatch",
        "row_not_found",
        "txn_not_active",
        "permission_denied",
        "serialization_failure",
        "storage",
        "parse",
        "plan",
        "exec",
        "protocol",
        "object_already_exists",
        "invalid_definition",
    ];
    const NAME: &'static str = "category";

    fn index(self) -> usize {
        match self {
            ErrorKind::TableNotFound => 0,
            ErrorKind::ColumnNotFound => 1,
            ErrorKind::TypeMismatch => 2,
            ErrorKind::RowNotFound => 3,
            ErrorKind::TxnNotActive => 4,
            ErrorKind::PermissionDenied => 5,
            ErrorKind::SerializationFailure => 6,
            ErrorKind::Storage => 7,
            ErrorKind::Parse => 8,
            ErrorKind::Plan => 9,
            ErrorKind::Exec => 10,
            ErrorKind::Protocol => 11,
            ErrorKind::ObjectAlreadyExists => 12,
            ErrorKind::InvalidDefinition => 13,
        }
    }
}

/// Everything the process reports. Fields are public so instrumentation
/// reads as `metrics().connections_total.inc()`.
#[derive(Debug)]
pub struct Metrics {
    /// Unix time the process started, used to derive uptime on scrape.
    start_unix: u64,

    pub connections_total: Counter,
    pub connections_active: Gauge,
    /// Sessions refused before authentication: cleartext startup against a
    /// TLS listener, malformed pre-startup traffic, throttled sources.
    pub connections_rejected_total: Counter,

    pub auth_failures_total: Counter,
    /// Attempts refused outright because the source was locked out.
    pub auth_throttled_total: Counter,
    /// Times a source crossed the failure threshold and got locked out.
    pub auth_lockouts_total: Counter,

    pub queries_total: CounterVec<StatementKind>,
    pub query_errors_total: CounterVec<ErrorKind>,
    pub query_duration_seconds: Histogram,

    pub transactions_active: Gauge,
    pub transactions_committed_total: Counter,
    pub transactions_aborted_total: Counter,

    pub data_file_bytes: Gauge,
    pub journal_bytes: Gauge,
    /// Highest transaction id allocated so far, against
    /// [`Metrics::txn_id_ceiling`]: the commit bitmap is finite (see
    /// `ferrite-storage/README.md`) and this is what says how close the
    /// database is to it.
    pub txn_id: Gauge,
    pub txn_id_ceiling: Gauge,

    /// Page checksum mismatches seen since start. Any value above zero is
    /// an incident, not a warning.
    pub checksum_failures_total: Counter,
    /// Writes the storage layer could not complete — a full or read-only
    /// disk is the usual cause.
    pub storage_write_failures_total: Counter,

    pub health_checks_total: Counter,
    pub health_failures_total: Counter,
    /// How long the most recent health probe took, in microseconds.
    health_probe_micros: AtomicU64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            start_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            connections_total: Counter::new(),
            connections_active: Gauge::new(),
            connections_rejected_total: Counter::new(),
            auth_failures_total: Counter::new(),
            auth_throttled_total: Counter::new(),
            auth_lockouts_total: Counter::new(),
            queries_total: CounterVec::new(),
            query_errors_total: CounterVec::new(),
            query_duration_seconds: Histogram::new(),
            transactions_active: Gauge::new(),
            transactions_committed_total: Counter::new(),
            transactions_aborted_total: Counter::new(),
            data_file_bytes: Gauge::new(),
            journal_bytes: Gauge::new(),
            txn_id: Gauge::new(),
            txn_id_ceiling: Gauge::new(),
            checksum_failures_total: Counter::new(),
            storage_write_failures_total: Counter::new(),
            health_checks_total: Counter::new(),
            health_failures_total: Counter::new(),
            health_probe_micros: AtomicU64::new(0),
        }
    }

    pub fn record_health_probe(&self, seconds: f64) {
        self.health_probe_micros
            .store((seconds * 1e6) as u64, Ordering::Relaxed);
    }

    /// Renders the whole registry in the Prometheus text exposition format.
    pub fn encode(&self) -> String {
        let mut out = Encoder::new();

        out.counter(
            "ferrite_connections_total",
            "Client connections accepted since start.",
            &self.connections_total,
        );
        out.gauge(
            "ferrite_connections_active",
            "Client connections currently open.",
            &self.connections_active,
        );
        out.counter(
            "ferrite_connections_rejected_total",
            "Connections refused before authentication completed.",
            &self.connections_rejected_total,
        );
        out.counter(
            "ferrite_auth_failures_total",
            "Failed authentication attempts.",
            &self.auth_failures_total,
        );
        out.counter(
            "ferrite_auth_throttled_total",
            "Authentication attempts refused because the source was locked out.",
            &self.auth_throttled_total,
        );
        out.counter(
            "ferrite_auth_lockouts_total",
            "Times a source crossed the failure threshold and was locked out.",
            &self.auth_lockouts_total,
        );
        out.counter_vec(
            "ferrite_queries_total",
            "Statements executed, by kind.",
            &self.queries_total,
        );
        out.counter_vec(
            "ferrite_query_errors_total",
            "Statements that failed, by error category.",
            &self.query_errors_total,
        );
        out.histogram(
            "ferrite_query_duration_seconds",
            "Wall-clock time to execute one statement.",
            &self.query_duration_seconds,
        );
        out.gauge(
            "ferrite_transactions_active",
            "Transactions currently open.",
            &self.transactions_active,
        );
        out.counter(
            "ferrite_transactions_committed_total",
            "Transactions committed.",
            &self.transactions_committed_total,
        );
        out.counter(
            "ferrite_transactions_aborted_total",
            "Transactions rolled back or aborted.",
            &self.transactions_aborted_total,
        );
        out.gauge(
            "ferrite_data_file_bytes",
            "Size of ferrite.db on disk.",
            &self.data_file_bytes,
        );
        out.gauge(
            "ferrite_journal_bytes",
            "Size of ferrite.wal on disk.",
            &self.journal_bytes,
        );
        out.gauge(
            "ferrite_txn_id",
            "Highest transaction id allocated so far.",
            &self.txn_id,
        );
        out.gauge(
            "ferrite_txn_id_ceiling",
            "Transaction ids the commit bitmap can still address.",
            &self.txn_id_ceiling,
        );
        out.counter(
            "ferrite_checksum_failures_total",
            "Page checksum mismatches detected.",
            &self.checksum_failures_total,
        );
        out.counter(
            "ferrite_storage_write_failures_total",
            "Storage writes that failed, typically a full or read-only disk.",
            &self.storage_write_failures_total,
        );
        out.counter(
            "ferrite_health_checks_total",
            "Health probes served.",
            &self.health_checks_total,
        );
        out.counter(
            "ferrite_health_failures_total",
            "Health probes that failed.",
            &self.health_failures_total,
        );

        let uptime = Gauge::new();
        uptime.set(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(self.start_unix)
                .saturating_sub(self.start_unix) as i64,
        );
        out.gauge(
            "ferrite_uptime_seconds",
            "Seconds since the process started.",
            &uptime,
        );

        let probe = Gauge::new();
        probe.set(self.health_probe_micros.load(Ordering::Relaxed) as i64);
        out.gauge(
            "ferrite_health_probe_micros",
            "Time the most recent health probe took, in microseconds.",
            &probe,
        );

        out.finish()
    }
}

static METRICS: OnceLock<Metrics> = OnceLock::new();

/// The process-wide registry, initialised on first use.
pub fn metrics() -> &'static Metrics {
    METRICS.get_or_init(Metrics::new)
}

/// Increments `connections_active` and decrements it again on drop, so a
/// connection that ends by panicking or by an early `?` still leaves the
/// gauge balanced.
pub struct ConnectionGuard(());

impl ConnectionGuard {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        metrics().connections_total.inc();
        metrics().connections_active.inc();
        Self(())
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        metrics().connections_active.dec();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_is_a_single_process_wide_instance() {
        assert!(std::ptr::eq(metrics(), metrics()));
    }

    #[test]
    fn every_family_carries_its_help_and_type() {
        let text = Metrics::new().encode();
        for name in [
            "ferrite_connections_total",
            "ferrite_connections_active",
            "ferrite_auth_failures_total",
            "ferrite_queries_total",
            "ferrite_query_errors_total",
            "ferrite_query_duration_seconds",
            "ferrite_transactions_active",
            "ferrite_data_file_bytes",
            "ferrite_uptime_seconds",
        ] {
            assert!(
                text.contains(&format!("# HELP {name} ")),
                "{name} has no HELP line"
            );
            assert!(
                text.contains(&format!("# TYPE {name} ")),
                "{name} has no TYPE line"
            );
        }
    }

    #[test]
    fn every_statement_kind_and_error_category_is_exposed() {
        let text = Metrics::new().encode();
        for value in StatementKind::VALUES {
            assert!(text.contains(&format!("ferrite_queries_total{{kind=\"{value}\"}}")));
        }
        for value in ErrorKind::VALUES {
            assert!(text.contains(&format!(
                "ferrite_query_errors_total{{category=\"{value}\"}}"
            )));
        }
    }

    #[test]
    fn a_connection_guard_balances_the_active_gauge() {
        let before = metrics().connections_active.get();
        {
            let _guard = ConnectionGuard::new();
            assert_eq!(metrics().connections_active.get(), before + 1);
        }
        assert_eq!(metrics().connections_active.get(), before);
    }
}
