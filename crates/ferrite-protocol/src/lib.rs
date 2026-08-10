//! The PostgreSQL frontend/backend protocol, version 3.
//!
//! Speaking the wire protocol is Ferrite's compatibility lever: psql,
//! JDBC/ODBC, `sqlx`, `tokio-postgres` and Diesel all work unmodified
//! against a server that gets the bytes right, so none of the client
//! ecosystem has to be reimplemented.
//!
//! # What this crate depends on
//!
//! Only [`ferrite_common`], for [`Row`](ferrite_common::Row),
//! [`Value`](ferrite_common::Value), [`Identity`](ferrite_common::Identity)
//! and [`FerriteError`](ferrite_common::FerriteError). It has no
//! dependency on the storage engine, the planner or the executor: SQL
//! reaches the engine through the [`QueryHandler`] trait, which anything
//! can implement. [`mock::MockHandler`] is the in-crate implementation used
//! to test the protocol end-to-end without an engine.
//!
//! # Security posture
//!
//! - TLS is required unless a listener is explicitly built with
//!   [`TlsMode::Disabled`]; a cleartext `StartupMessage` against a TLS
//!   listener is refused before authentication is even offered.
//! - Passwords are compared in constant time against salted digests
//!   ([`auth::PasswordVerifier`]); an unknown user costs the same as a
//!   wrong password.
//! - Connecting is deny-by-default: a role without
//!   [`Permission::Connect`](ferrite_common::Permission) cannot open a
//!   session.
//! - Every byte decoded here comes from an untrusted peer. Decoding is
//!   total: bad lengths, bad counts, non-UTF-8 text, unknown tags and
//!   truncation all produce a [`ProtocolError`], never a panic. See
//!   `fuzz/` for the `cargo-fuzz` target and `tests/malformed.rs` for the
//!   deterministic cases.
//!
//! # Example
//!
//! ```no_run
//! use std::sync::Arc;
//! use ferrite_protocol::{
//!     auth::{superuser_role, StaticAuthenticator},
//!     mock::MockHandler,
//!     Server, ServerConfig, TlsMode,
//! };
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let config = ServerConfig::new(
//!     Arc::new(MockHandler::new()),
//!     Arc::new(StaticAuthenticator::new().with_user("ferrite", "hunter2", superuser_role())),
//!     TlsMode::from_pem_files("server.crt", "server.key")?,
//! );
//! Server::bind("0.0.0.0:5432", config).await?.run().await?;
//! # Ok(())
//! # }
//! ```

pub mod auth;
mod buf;
mod codec;
mod config;
mod error;
mod handler;
pub mod message;
pub mod mock;
mod server;
mod session;
mod tls;
pub mod types;

pub use codec::DEFAULT_MAX_MESSAGE_SIZE;
pub use config::ServerConfig;
pub use error::{sqlstate, ProtocolError, Result};
pub use handler::{CommandTag, FieldDescription, QueryHandler, QueryResult, StatementDescription};
pub use message::TransactionStatus;
pub use server::Server;
pub use session::serve_connection;
pub use tls::TlsMode;
pub use types::Format;
