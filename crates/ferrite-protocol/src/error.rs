use ferrite_common::FerriteError;

/// Everything that can go wrong while speaking the wire protocol.
///
/// Decoding errors are always returned, never panicked: every byte handled
/// by this crate comes from an untrusted peer, so a malformed or truncated
/// frame must degrade to a clean `ErrorResponse` plus disconnect.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A frame was structurally invalid: bad length prefix, missing NUL
    /// terminator, trailing bytes, non-UTF-8 text, unknown message tag.
    #[error("malformed message: {0}")]
    Malformed(String),

    /// The declared frame length exceeded the configured limit. Enforced
    /// before allocating, so a hostile length prefix cannot exhaust memory.
    #[error("message of {len} bytes exceeds the {max} byte limit")]
    MessageTooLarge { len: usize, max: usize },

    #[error("unsupported protocol version {major}.{minor}")]
    UnsupportedVersion { major: u16, minor: u16 },

    #[error("authentication failed for user {0:?}")]
    AuthFailed(String),

    /// The peer tried to start a session in cleartext while the listener
    /// requires TLS.
    #[error("TLS is required on this listener")]
    TlsRequired,

    #[error("tls error: {0}")]
    Tls(String),

    /// The peer closed the connection at a point where more input was
    /// expected. Not an error worth logging loudly — clients disconnect.
    #[error("connection closed by peer")]
    Closed,

    /// Bubbled up from the `QueryHandler` implementation.
    #[error(transparent)]
    Ferrite(#[from] FerriteError),
}

impl ProtocolError {
    pub(crate) fn malformed(msg: impl Into<String>) -> Self {
        ProtocolError::Malformed(msg.into())
    }

    /// Whether the connection can keep going after this error. Only errors
    /// raised by the query handler are recoverable; anything at the framing
    /// or transport layer means the byte stream is no longer trustworthy.
    pub fn is_recoverable(&self) -> bool {
        matches!(self, ProtocolError::Ferrite(_))
    }

    /// The five-character SQLSTATE reported to the client.
    pub fn sqlstate(&self) -> &'static str {
        match self {
            ProtocolError::Io(_) | ProtocolError::Closed => sqlstate::CONNECTION_FAILURE,
            ProtocolError::Malformed(_) | ProtocolError::MessageTooLarge { .. } => {
                sqlstate::PROTOCOL_VIOLATION
            }
            ProtocolError::UnsupportedVersion { .. } => sqlstate::FEATURE_NOT_SUPPORTED,
            ProtocolError::AuthFailed(_) => sqlstate::INVALID_PASSWORD,
            ProtocolError::TlsRequired => sqlstate::INVALID_AUTHORIZATION,
            ProtocolError::Tls(_) => sqlstate::CONNECTION_FAILURE,
            ProtocolError::Ferrite(e) => ferrite_sqlstate(e),
        }
    }
}

impl From<ProtocolError> for FerriteError {
    fn from(err: ProtocolError) -> Self {
        match err {
            ProtocolError::Ferrite(inner) => inner,
            other => FerriteError::Protocol(other.to_string()),
        }
    }
}

fn ferrite_sqlstate(err: &FerriteError) -> &'static str {
    match err {
        FerriteError::TableNotFound(_) => sqlstate::UNDEFINED_TABLE,
        FerriteError::ColumnNotFound(_) => sqlstate::UNDEFINED_COLUMN,
        FerriteError::TypeMismatch { .. } => sqlstate::DATATYPE_MISMATCH,
        FerriteError::RowNotFound => sqlstate::NO_DATA_FOUND,
        FerriteError::TxnNotActive(_) => sqlstate::NO_ACTIVE_TRANSACTION,
        FerriteError::PermissionDenied(_) => sqlstate::INSUFFICIENT_PRIVILEGE,
        FerriteError::SerializationFailure => sqlstate::SERIALIZATION_FAILURE,
        FerriteError::Storage(_) => sqlstate::INTERNAL_ERROR,
        FerriteError::Parse(_) => sqlstate::SYNTAX_ERROR,
        FerriteError::Plan(_) | FerriteError::Exec(_) => sqlstate::INTERNAL_ERROR,
        FerriteError::Protocol(_) => sqlstate::PROTOCOL_VIOLATION,
    }
}

/// SQLSTATE codes used by this crate, spelled out so client-side error
/// matching (`sqlx`, JDBC, psql) behaves the same as against PostgreSQL.
pub mod sqlstate {
    pub const CONNECTION_FAILURE: &str = "08006";
    pub const PROTOCOL_VIOLATION: &str = "08P01";
    pub const FEATURE_NOT_SUPPORTED: &str = "0A000";
    pub const NO_DATA_FOUND: &str = "02000";
    pub const INVALID_PASSWORD: &str = "28P01";
    pub const INVALID_AUTHORIZATION: &str = "28000";
    pub const NO_ACTIVE_TRANSACTION: &str = "25P01";
    pub const IN_FAILED_TRANSACTION: &str = "25P02";
    pub const INSUFFICIENT_PRIVILEGE: &str = "42501";
    pub const SYNTAX_ERROR: &str = "42601";
    pub const UNDEFINED_COLUMN: &str = "42703";
    pub const UNDEFINED_TABLE: &str = "42P01";
    pub const DATATYPE_MISMATCH: &str = "42804";
    pub const INVALID_SQL_STATEMENT_NAME: &str = "26000";
    pub const SERIALIZATION_FAILURE: &str = "40001";
    pub const INTERNAL_ERROR: &str = "XX000";
}

pub type Result<T> = std::result::Result<T, ProtocolError>;
