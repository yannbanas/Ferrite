//! Message-level encoding and decoding for protocol version 3.
//!
//! Decoding never trusts its input: unknown tags, bad lengths, negative
//! counts, non-UTF-8 text and trailing bytes all produce a
//! [`ProtocolError`] instead of a panic. See `tests/malformed.rs` and
//! `fuzz/` for the adversarial coverage.

use crate::buf::{Reader, Writer};
use crate::error::{ProtocolError, Result};
use crate::types::{Format, Oid};

/// Protocol version 3.0, the only one Ferrite speaks. Version 2 clients are
/// long gone and supporting them means a second framing layer.
pub const PROTOCOL_VERSION_3: i32 = 196_608;

pub const SSL_REQUEST_CODE: i32 = 80_877_103;
pub const GSSENC_REQUEST_CODE: i32 = 80_877_104;
pub const CANCEL_REQUEST_CODE: i32 = 80_877_102;

/// PostgreSQL's own cap on the startup packet. Applied before allocating.
pub const MAX_STARTUP_PACKET_LEN: usize = 10_000;

/// What a client sent on a freshly opened socket, before any tagged
/// message exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupRequest {
    Startup(StartupParams),
    SslRequest,
    GssEncRequest,
    /// Out-of-band request on a second connection asking to cancel a query.
    Cancel {
        process_id: i32,
        secret_key: i32,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartupParams {
    pub user: String,
    pub database: String,
    /// Every other key/value pair, e.g. `application_name`, `DateStyle`.
    pub options: Vec<(String, String)>,
}

impl StartupParams {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.options
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

impl StartupRequest {
    /// Decodes the body of a startup packet — everything after the four
    /// length bytes, which the caller has already read and validated.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let mut r = Reader::new(body);
        let code = r.i32()?;
        match code {
            SSL_REQUEST_CODE => {
                r.finish()?;
                Ok(StartupRequest::SslRequest)
            }
            GSSENC_REQUEST_CODE => {
                r.finish()?;
                Ok(StartupRequest::GssEncRequest)
            }
            CANCEL_REQUEST_CODE => {
                let process_id = r.i32()?;
                let secret_key = r.i32()?;
                r.finish()?;
                Ok(StartupRequest::Cancel {
                    process_id,
                    secret_key,
                })
            }
            PROTOCOL_VERSION_3 => Ok(StartupRequest::Startup(decode_startup_params(&mut r)?)),
            other => Err(ProtocolError::UnsupportedVersion {
                major: (other >> 16) as u16,
                minor: (other & 0xffff) as u16,
            }),
        }
    }
}

fn decode_startup_params(r: &mut Reader<'_>) -> Result<StartupParams> {
    let mut params = StartupParams::default();
    loop {
        if r.is_empty() {
            // A well-formed packet ends with the terminating NUL below; if
            // we ran out of bytes first the packet was truncated.
            return Err(ProtocolError::malformed("startup packet is not terminated"));
        }
        let key = r.cstr()?;
        if key.is_empty() {
            break;
        }
        let value = r.cstr()?;
        match key.as_str() {
            "user" => params.user = value,
            "database" => params.database = value,
            _ => params.options.push((key, value)),
        }
    }
    if params.user.is_empty() {
        return Err(ProtocolError::malformed("startup packet has no user"));
    }
    if params.database.is_empty() {
        params.database.clone_from(&params.user);
    }
    Ok(params)
}

/// Whether a `Describe`/`Close` targets a prepared statement or a portal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    Statement,
    Portal,
}

impl TargetKind {
    fn from_byte(b: u8) -> Result<Self> {
        match b {
            b'S' => Ok(TargetKind::Statement),
            b'P' => Ok(TargetKind::Portal),
            other => Err(ProtocolError::malformed(format!(
                "unknown describe/close target {:?}",
                other as char
            ))),
        }
    }
}

/// A decoded frontend (client to server) message.
#[derive(Debug, Clone, PartialEq)]
pub enum Frontend {
    Query(String),
    Parse {
        name: String,
        sql: String,
        param_types: Vec<Oid>,
    },
    Bind {
        portal: String,
        statement: String,
        param_formats: Vec<Format>,
        params: Vec<Option<Vec<u8>>>,
        result_formats: Vec<Format>,
    },
    Describe {
        kind: TargetKind,
        name: String,
    },
    Execute {
        portal: String,
        max_rows: i32,
    },
    Close {
        kind: TargetKind,
        name: String,
    },
    Sync,
    Flush,
    Terminate,
    Password(Vec<u8>),
    /// A tag this server does not implement (`FunctionCall`, `CopyData`,
    /// …). Kept as a value so the session can answer with a clean error
    /// rather than losing frame sync.
    Unsupported(u8),
}

impl Frontend {
    /// Decodes one message from its tag byte and already-framed body.
    pub fn decode(tag: u8, body: &[u8]) -> Result<Self> {
        let mut r = Reader::new(body);
        let msg = match tag {
            b'Q' => {
                let sql = r.cstr()?;
                Frontend::Query(sql)
            }
            b'P' => {
                let name = r.cstr()?;
                let sql = r.cstr()?;
                let n = r.count()?;
                let mut param_types = Vec::with_capacity(n.min(64));
                for _ in 0..n {
                    param_types.push(r.i32()?);
                }
                Frontend::Parse {
                    name,
                    sql,
                    param_types,
                }
            }
            b'B' => {
                let portal = r.cstr()?;
                let statement = r.cstr()?;
                let param_formats = decode_formats(&mut r)?;
                let n = r.count()?;
                let mut params = Vec::with_capacity(n.min(64));
                for _ in 0..n {
                    params.push(r.nullable_bytes()?.map(<[u8]>::to_vec));
                }
                let result_formats = decode_formats(&mut r)?;
                Frontend::Bind {
                    portal,
                    statement,
                    param_formats,
                    params,
                    result_formats,
                }
            }
            b'D' => {
                let kind = TargetKind::from_byte(r.u8()?)?;
                let name = r.cstr()?;
                Frontend::Describe { kind, name }
            }
            b'E' => {
                let portal = r.cstr()?;
                let max_rows = r.i32()?;
                if max_rows < 0 {
                    return Err(ProtocolError::malformed("negative row limit"));
                }
                Frontend::Execute { portal, max_rows }
            }
            b'C' => {
                let kind = TargetKind::from_byte(r.u8()?)?;
                let name = r.cstr()?;
                Frontend::Close { kind, name }
            }
            b'S' => Frontend::Sync,
            b'H' => Frontend::Flush,
            b'X' => Frontend::Terminate,
            b'p' => Frontend::Password(body.to_vec()),
            other => return Ok(Frontend::Unsupported(other)),
        };
        if !matches!(msg, Frontend::Password(_)) {
            r.finish()?;
        }
        Ok(msg)
    }
}

fn decode_formats(r: &mut Reader<'_>) -> Result<Vec<Format>> {
    let n = r.count()?;
    let mut out = Vec::with_capacity(n.min(64));
    for _ in 0..n {
        out.push(Format::from_code(r.i16()?)?);
    }
    Ok(out)
}

/// Resolves the per-item format codes of a `Bind` message: zero codes means
/// everything is text, one code applies to every item, otherwise there must
/// be exactly one code per item.
pub(crate) fn resolve_formats(codes: &[Format], count: usize) -> Result<Vec<Format>> {
    match codes.len() {
        0 => Ok(vec![Format::Text; count]),
        1 => Ok(vec![codes[0]; count]),
        n if n == count => Ok(codes.to_vec()),
        n => Err(ProtocolError::malformed(format!(
            "{n} format codes for {count} values"
        ))),
    }
}

/// One field of a `RowDescription`.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldMeta<'a> {
    pub name: &'a str,
    pub type_oid: Oid,
    pub type_size: i16,
    pub type_modifier: i32,
    pub table_oid: i32,
    pub column_id: i16,
    pub format: Format,
}

/// Transaction state reported by every `ReadyForQuery`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransactionStatus {
    #[default]
    Idle,
    InTransaction,
    /// A statement failed inside an explicit transaction; everything but
    /// `ROLLBACK` is rejected until the block ends.
    Failed,
}

impl TransactionStatus {
    fn code(self) -> u8 {
        match self {
            TransactionStatus::Idle => b'I',
            TransactionStatus::InTransaction => b'T',
            TransactionStatus::Failed => b'E',
        }
    }
}

/// Severity prefix of an `ErrorResponse`/`NoticeResponse`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Fatal,
    Notice,
}

impl Severity {
    fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "ERROR",
            Severity::Fatal => "FATAL",
            Severity::Notice => "NOTICE",
        }
    }
}

/// Backend (server to client) message encoders. Each returns a complete
/// frame including tag and length prefix.
pub mod backend {
    use super::*;

    pub fn authentication_ok() -> Vec<u8> {
        Writer::tagged(b'R').i32(0).finish()
    }

    pub fn authentication_cleartext_password() -> Vec<u8> {
        Writer::tagged(b'R').i32(3).finish()
    }

    pub fn parameter_status(key: &str, value: &str) -> Vec<u8> {
        Writer::tagged(b'S').cstr(key).cstr(value).finish()
    }

    pub fn backend_key_data(process_id: i32, secret_key: i32) -> Vec<u8> {
        Writer::tagged(b'K')
            .i32(process_id)
            .i32(secret_key)
            .finish()
    }

    pub fn ready_for_query(status: TransactionStatus) -> Vec<u8> {
        Writer::tagged(b'Z').u8(status.code()).finish()
    }

    pub fn row_description(fields: &[FieldMeta<'_>]) -> Vec<u8> {
        let mut w = Writer::tagged(b'T');
        w.i16(fields.len() as i16);
        for f in fields {
            w.cstr(f.name)
                .i32(f.table_oid)
                .i16(f.column_id)
                .i32(f.type_oid)
                .i16(f.type_size)
                .i32(f.type_modifier)
                .i16(f.format.code());
        }
        w.finish()
    }

    pub fn data_row(values: &[Option<Vec<u8>>]) -> Vec<u8> {
        let mut w = Writer::tagged(b'D');
        w.i16(values.len() as i16);
        for value in values {
            match value {
                None => {
                    w.i32(-1);
                }
                Some(bytes) => {
                    w.i32(bytes.len() as i32).bytes(bytes);
                }
            }
        }
        w.finish()
    }

    pub fn command_complete(tag: &str) -> Vec<u8> {
        Writer::tagged(b'C').cstr(tag).finish()
    }

    pub fn empty_query_response() -> Vec<u8> {
        Writer::tagged(b'I').finish()
    }

    pub fn no_data() -> Vec<u8> {
        Writer::tagged(b'n').finish()
    }

    pub fn parse_complete() -> Vec<u8> {
        Writer::tagged(b'1').finish()
    }

    pub fn bind_complete() -> Vec<u8> {
        Writer::tagged(b'2').finish()
    }

    pub fn close_complete() -> Vec<u8> {
        Writer::tagged(b'3').finish()
    }

    pub fn portal_suspended() -> Vec<u8> {
        Writer::tagged(b's').finish()
    }

    pub fn parameter_description(oids: &[Oid]) -> Vec<u8> {
        let mut w = Writer::tagged(b't');
        w.i16(oids.len() as i16);
        for oid in oids {
            w.i32(*oid);
        }
        w.finish()
    }

    pub fn error_response(severity: Severity, sqlstate: &str, message: &str) -> Vec<u8> {
        let mut w = Writer::tagged(if matches!(severity, Severity::Notice) {
            b'N'
        } else {
            b'E'
        });
        w.u8(b'S').cstr(severity.as_str());
        w.u8(b'V').cstr(severity.as_str());
        w.u8(b'C').cstr(sqlstate);
        w.u8(b'M').cstr(message);
        w.u8(0);
        w.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn startup_body(pairs: &[(&str, &str)]) -> Vec<u8> {
        let mut body = PROTOCOL_VERSION_3.to_be_bytes().to_vec();
        for (k, v) in pairs {
            body.extend_from_slice(k.as_bytes());
            body.push(0);
            body.extend_from_slice(v.as_bytes());
            body.push(0);
        }
        body.push(0);
        body
    }

    #[test]
    fn decodes_a_startup_packet() {
        let body = startup_body(&[
            ("user", "ferrite"),
            ("database", "app"),
            ("application_name", "psql"),
        ]);
        let StartupRequest::Startup(params) = StartupRequest::decode(&body).unwrap() else {
            panic!("expected a startup message");
        };
        assert_eq!(params.user, "ferrite");
        assert_eq!(params.database, "app");
        assert_eq!(params.get("application_name"), Some("psql"));
    }

    #[test]
    fn database_defaults_to_the_user_name() {
        let body = startup_body(&[("user", "ferrite")]);
        let StartupRequest::Startup(params) = StartupRequest::decode(&body).unwrap() else {
            panic!("expected a startup message");
        };
        assert_eq!(params.database, "ferrite");
    }

    #[test]
    fn decodes_the_special_request_codes() {
        for (code, expected) in [
            (SSL_REQUEST_CODE, StartupRequest::SslRequest),
            (GSSENC_REQUEST_CODE, StartupRequest::GssEncRequest),
        ] {
            assert_eq!(
                StartupRequest::decode(&code.to_be_bytes()).unwrap(),
                expected
            );
        }
        let mut body = CANCEL_REQUEST_CODE.to_be_bytes().to_vec();
        body.extend_from_slice(&7i32.to_be_bytes());
        body.extend_from_slice(&9i32.to_be_bytes());
        assert_eq!(
            StartupRequest::decode(&body).unwrap(),
            StartupRequest::Cancel {
                process_id: 7,
                secret_key: 9
            }
        );
    }

    #[test]
    fn rejects_protocol_version_2() {
        let err = StartupRequest::decode(&131_072i32.to_be_bytes()).unwrap_err();
        assert!(matches!(
            err,
            ProtocolError::UnsupportedVersion { major: 2, minor: 0 }
        ));
    }

    #[test]
    fn rejects_startup_packets_without_a_user() {
        let body = startup_body(&[("database", "app")]);
        assert!(StartupRequest::decode(&body).is_err());
    }

    #[test]
    fn rejects_unterminated_startup_packets() {
        let mut body = PROTOCOL_VERSION_3.to_be_bytes().to_vec();
        body.extend_from_slice(b"user\0ferrite\0");
        assert!(StartupRequest::decode(&body).is_err());
    }

    #[test]
    fn decodes_a_simple_query() {
        assert_eq!(
            Frontend::decode(b'Q', b"SELECT 1\0").unwrap(),
            Frontend::Query("SELECT 1".into())
        );
    }

    #[test]
    fn decodes_bind_with_binary_parameters() {
        let mut body = Vec::new();
        body.extend_from_slice(b"portal\0stmt\0");
        body.extend_from_slice(&1i16.to_be_bytes());
        body.extend_from_slice(&1i16.to_be_bytes());
        body.extend_from_slice(&1i16.to_be_bytes());
        body.extend_from_slice(&4i32.to_be_bytes());
        body.extend_from_slice(&42i32.to_be_bytes());
        body.extend_from_slice(&1i16.to_be_bytes());
        body.extend_from_slice(&1i16.to_be_bytes());
        let Frontend::Bind {
            portal,
            statement,
            param_formats,
            params,
            result_formats,
        } = Frontend::decode(b'B', &body).unwrap()
        else {
            panic!("expected a bind message");
        };
        assert_eq!((portal.as_str(), statement.as_str()), ("portal", "stmt"));
        assert_eq!(param_formats, vec![Format::Binary]);
        assert_eq!(params, vec![Some(42i32.to_be_bytes().to_vec())]);
        assert_eq!(result_formats, vec![Format::Binary]);
    }

    #[test]
    fn unknown_tags_decode_to_unsupported_rather_than_failing() {
        assert_eq!(
            Frontend::decode(b'F', b"whatever").unwrap(),
            Frontend::Unsupported(b'F')
        );
    }

    #[test]
    fn rejects_trailing_bytes_after_a_message() {
        assert!(Frontend::decode(b'Q', b"SELECT 1\0junk").is_err());
        assert!(Frontend::decode(b'S', b"junk").is_err());
    }

    #[test]
    fn rejects_negative_execute_row_limits() {
        let mut body = b"portal\0".to_vec();
        body.extend_from_slice(&(-1i32).to_be_bytes());
        assert!(Frontend::decode(b'E', &body).is_err());
    }

    #[test]
    fn format_code_lists_are_broadcast_or_matched() {
        assert_eq!(resolve_formats(&[], 2).unwrap(), vec![Format::Text; 2]);
        assert_eq!(
            resolve_formats(&[Format::Binary], 3).unwrap(),
            vec![Format::Binary; 3]
        );
        assert!(resolve_formats(&[Format::Binary, Format::Text], 3).is_err());
    }

    #[test]
    fn encodes_a_ready_for_query_per_transaction_state() {
        assert_eq!(
            backend::ready_for_query(TransactionStatus::InTransaction),
            vec![b'Z', 0, 0, 0, 5, b'T']
        );
    }

    #[test]
    fn encodes_null_data_row_fields_as_minus_one() {
        let row = backend::data_row(&[None, Some(b"hi".to_vec())]);
        assert_eq!(
            row,
            [
                &[b'D', 0, 0, 0, 16, 0, 2][..],
                &(-1i32).to_be_bytes()[..],
                &2i32.to_be_bytes()[..],
                b"hi",
            ]
            .concat()
        );
    }
}
