use std::sync::Arc;

use crate::auth::Authenticator;
use crate::codec::DEFAULT_MAX_MESSAGE_SIZE;
use crate::handler::QueryHandler;
use crate::message::StartupParams;
use crate::throttle::AuthThrottle;
use crate::tls::TlsMode;

/// Concurrent sessions a listener serves by default. PostgreSQL's own
/// `max_connections` default is 100; this is higher because a Ferrite
/// connection costs a task rather than a process, and still bounded
/// because "as many as the client opens" is not a bound.
pub const DEFAULT_MAX_CONNECTIONS: usize = 512;

/// Everything a listener needs, shared by every connection it accepts.
pub struct ServerConfig {
    pub handler: Arc<dyn QueryHandler>,
    pub authenticator: Arc<dyn Authenticator>,
    /// TLS is required unless this is explicitly [`TlsMode::Disabled`].
    pub tls: TlsMode,
    /// Brute-force limiter, on by default: an unlimited number of guesses
    /// defeats any password check, however carefully it compares.
    pub throttle: Arc<AuthThrottle>,
    pub max_message_size: usize,
    /// Sessions the listener will serve at once. A refused connection is
    /// answered with SQLSTATE `53300` and closed, which is what a client
    /// pool understands; accepting without bound instead means one runaway
    /// client can exhaust file descriptors and memory for everyone.
    pub max_connections: usize,
    /// Reported as the `server_version` parameter. Clients gate features on
    /// it, so it claims a PostgreSQL version and names Ferrite alongside.
    pub server_version: String,
}

impl ServerConfig {
    pub fn new(
        handler: Arc<dyn QueryHandler>,
        authenticator: Arc<dyn Authenticator>,
        tls: TlsMode,
    ) -> Self {
        Self {
            handler,
            authenticator,
            tls,
            throttle: Arc::new(AuthThrottle::default()),
            max_message_size: DEFAULT_MAX_MESSAGE_SIZE,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            server_version: format!("16.0 (Ferrite {})", env!("CARGO_PKG_VERSION")),
        }
    }

    pub fn with_max_message_size(mut self, bytes: usize) -> Self {
        self.max_message_size = bytes;
        self
    }

    pub fn with_throttle(mut self, throttle: AuthThrottle) -> Self {
        self.throttle = Arc::new(throttle);
        self
    }

    pub fn with_max_connections(mut self, connections: usize) -> Self {
        self.max_connections = connections.max(1);
        self
    }

    /// The `ParameterStatus` set sent right after authentication. These are
    /// the keys libpq and the mainstream drivers expect to be told about
    /// without asking.
    pub(crate) fn parameter_status(&self, params: &StartupParams) -> Vec<(String, String)> {
        let mut out = vec![
            ("server_version".to_owned(), self.server_version.clone()),
            ("server_encoding".to_owned(), "UTF8".to_owned()),
            ("client_encoding".to_owned(), "UTF8".to_owned()),
            ("DateStyle".to_owned(), "ISO, MDY".to_owned()),
            ("TimeZone".to_owned(), "UTC".to_owned()),
            ("integer_datetimes".to_owned(), "on".to_owned()),
            ("standard_conforming_strings".to_owned(), "on".to_owned()),
            ("is_superuser".to_owned(), "off".to_owned()),
            ("session_authorization".to_owned(), params.user.clone()),
        ];
        if let Some(name) = params.get("application_name") {
            out.push(("application_name".to_owned(), name.to_owned()));
        }
        out
    }
}
