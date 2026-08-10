use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::{TcpListener, ToSocketAddrs};
use tracing::{debug, warn};

use crate::config::ServerConfig;
use crate::error::{ProtocolError, Result};
use crate::session::serve_connection_from;

/// A TCP listener speaking the PostgreSQL wire protocol.
pub struct Server {
    listener: TcpListener,
    config: Arc<ServerConfig>,
}

impl Server {
    pub async fn bind(addr: impl ToSocketAddrs, config: ServerConfig) -> Result<Self> {
        Ok(Self {
            listener: TcpListener::bind(addr).await?,
            config: Arc::new(config),
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
            tokio::spawn(async move {
                // Counted here rather than after authentication: a peer that
                // opens a socket and never gets past the handshake still
                // holds a connection, and that is exactly the shape an
                // operator needs to see.
                let _counted = ferrite_metrics::ConnectionGuard::new();
                match serve_connection_from(stream, Some(peer.ip()), config).await {
                    Ok(()) => debug!(%peer, "connection closed"),
                    Err(err) => debug!(%peer, error = %err, "connection ended with an error"),
                }
            });
        }
    }
}
