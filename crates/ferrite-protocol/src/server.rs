use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, ToSocketAddrs};
use tracing::{debug, warn};

use crate::config::ServerConfig;
use crate::error::{sqlstate, ProtocolError, Result};
use crate::message::{backend, Severity};
use crate::session::serve_connection;

/// A TCP listener speaking the PostgreSQL wire protocol.
pub struct Server {
    listener: TcpListener,
    config: Arc<ServerConfig>,
    open: Arc<AtomicUsize>,
}

/// Decrements the live-connection count however the connection ends,
/// including a panic in the session task.
struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Server {
    pub async fn bind(addr: impl ToSocketAddrs, config: ServerConfig) -> Result<Self> {
        Ok(Self {
            listener: TcpListener::bind(addr).await?,
            config: Arc::new(config),
            open: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// The bound address. Useful with port 0, which is how the tests get a
    /// free port without racing each other.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.listener.local_addr()?)
    }

    /// Accepts connections until the listener fails. Each connection runs
    /// on its own task; one client's protocol error never takes down
    /// another's session.
    pub async fn run(self) -> Result<()> {
        loop {
            let (stream, peer) = match self.listener.accept().await {
                Ok(accepted) => accepted,
                Err(err) => {
                    warn!(error = %err, "accept failed");
                    return Err(ProtocolError::Io(err));
                }
            };
            // Interactive SQL is latency-bound and messages are small, so
            // waiting to coalesce writes only adds delay.
            let _ = stream.set_nodelay(true);
            let config = Arc::clone(&self.config);

            // Claim a slot before spawning. Refusing here, rather than
            // after the session has allocated its buffers and its
            // per-connection engine state, is the point of the bound.
            let open = self.open.fetch_add(1, Ordering::Relaxed) + 1;
            let slot = Slot(Arc::clone(&self.open));
            if open > config.max_connections {
                warn!(%peer, open, limit = config.max_connections, "refusing a connection");
                tokio::spawn(async move {
                    let _slot = slot;
                    refuse(stream, config.max_connections).await;
                });
                continue;
            }

            tokio::spawn(async move {
                let _slot = slot;
                match serve_connection(stream, config).await {
                    Ok(()) => debug!(%peer, "connection closed"),
                    Err(err) => debug!(%peer, error = %err, "connection ended with an error"),
                }
            });
        }
    }
}

/// Tells a client the listener is full and closes.
///
/// The `ErrorResponse` goes out before any startup exchange, which
/// PostgreSQL also does for `too_many_connections`: libpq and every pooler
/// built on it report the message rather than a bare connection reset.
async fn refuse(mut stream: tokio::net::TcpStream, limit: usize) {
    let message = format!("sorry, too many clients already (limit {limit})");
    let frame = backend::error_response(Severity::Fatal, sqlstate::TOO_MANY_CONNECTIONS, &message);
    let _ = stream.write_all(&frame).await;
    let _ = stream.flush().await;
}
