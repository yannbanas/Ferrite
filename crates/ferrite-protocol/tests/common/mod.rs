//! Test harness: starts a real listener on a loopback port and drives it
//! with a hand-rolled client that writes protocol bytes directly, so the
//! assertions are about the wire format and not about this crate's own
//! encoders.
#![allow(dead_code)]

use std::sync::Arc;

use ferrite_common::{Permission, Role};
use ferrite_protocol::auth::{read_only_role, superuser_role, StaticAuthenticator};
use ferrite_protocol::mock::MockHandler;
use ferrite_protocol::{Server, ServerConfig, TlsMode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

pub const USER: &str = "ferrite";
pub const PASSWORD: &str = "hunter2";
pub const DATABASE: &str = "app";

/// A self-signed certificate for `localhost`, plus the DER the client needs
/// in order to trust it.
pub struct TestCert {
    pub chain: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
}

pub fn self_signed_cert() -> TestCert {
    // Both names, so a client may address the listener as `localhost` or
    // as the loopback address it is actually bound to.
    let signed =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])
            .expect("generate a self-signed certificate");
    TestCert {
        chain: vec![signed.cert.der().clone()],
        key: PrivateKeyDer::Pkcs8(signed.key_pair.serialize_der().into()),
    }
}

pub fn authenticator() -> StaticAuthenticator {
    StaticAuthenticator::new()
        .with_user(USER, PASSWORD, superuser_role())
        .with_user("reader", "readonly-pw", read_only_role())
        .with_user(
            "nologin",
            "nologin-pw",
            Role {
                name: "nologin".to_owned(),
                permissions: vec![Permission::Select],
            },
        )
}

pub fn config(tls: TlsMode) -> ServerConfig {
    ServerConfig::new(Arc::new(MockHandler::new()), Arc::new(authenticator()), tls)
}

/// Binds a listener on an ephemeral port and runs it on a background task
/// for the duration of the test.
pub async fn start(config: ServerConfig) -> std::net::SocketAddr {
    let server = Server::bind("127.0.0.1:0", config).await.expect("bind");
    let addr = server.local_addr().expect("local addr");
    tokio::spawn(async move {
        let _ = server.run().await;
    });
    addr
}

pub async fn start_plaintext() -> std::net::SocketAddr {
    start(config(TlsMode::Disabled)).await
}

/// One decoded backend message.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub tag: u8,
    pub body: Vec<u8>,
}

impl Message {
    /// The NUL-terminated strings in the body, which covers
    /// `CommandComplete` and the field values of `ErrorResponse`.
    pub fn strings(&self) -> Vec<String> {
        self.body
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect()
    }

    /// One field of an `ErrorResponse`/`NoticeResponse`, by its code byte.
    pub fn error_field(&self, code: u8) -> Option<String> {
        let mut rest = &self.body[..];
        while let Some(end) = rest.iter().position(|b| *b == 0) {
            if end > 0 && rest[0] == code {
                return Some(String::from_utf8_lossy(&rest[1..end]).into_owned());
            }
            rest = &rest[end + 1..];
        }
        None
    }

    pub fn sqlstate(&self) -> Option<String> {
        self.error_field(b'C')
    }

    pub fn severity(&self) -> Option<String> {
        self.error_field(b'S')
    }

    pub fn error_message(&self) -> Option<String> {
        self.error_field(b'M')
    }

    /// Column names of a `RowDescription`.
    pub fn field_names(&self) -> Vec<String> {
        let mut out = Vec::new();
        let count = i16::from_be_bytes([self.body[0], self.body[1]]) as usize;
        let mut pos = 2;
        for _ in 0..count {
            let end = pos + self.body[pos..].iter().position(|b| *b == 0).unwrap();
            out.push(String::from_utf8_lossy(&self.body[pos..end]).into_owned());
            // name + NUL, then 18 bytes of fixed field metadata.
            pos = end + 1 + 18;
        }
        out
    }

    /// Column type OIDs of a `RowDescription`.
    pub fn field_oids(&self) -> Vec<i32> {
        let mut out = Vec::new();
        let count = i16::from_be_bytes([self.body[0], self.body[1]]) as usize;
        let mut pos = 2;
        for _ in 0..count {
            let end = pos + self.body[pos..].iter().position(|b| *b == 0).unwrap();
            let meta = end + 1;
            out.push(i32::from_be_bytes(
                self.body[meta + 6..meta + 10].try_into().unwrap(),
            ));
            pos = meta + 18;
        }
        out
    }

    /// Field values of a `DataRow`, as text. `None` is SQL NULL.
    pub fn row_values(&self) -> Vec<Option<String>> {
        let count = i16::from_be_bytes([self.body[0], self.body[1]]) as usize;
        let mut pos = 2;
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            let len = i32::from_be_bytes(self.body[pos..pos + 4].try_into().unwrap());
            pos += 4;
            if len < 0 {
                out.push(None);
            } else {
                let len = len as usize;
                out.push(Some(
                    String::from_utf8_lossy(&self.body[pos..pos + len]).into_owned(),
                ));
                pos += len;
            }
        }
        out
    }
}

/// A minimal client that speaks protocol bytes directly.
pub struct RawClient<S> {
    stream: S,
}

impl RawClient<TcpStream> {
    pub async fn connect(addr: std::net::SocketAddr) -> Self {
        Self {
            stream: TcpStream::connect(addr).await.expect("connect"),
        }
    }
}

impl<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin> RawClient<S> {
    pub fn wrapping(stream: S) -> Self {
        Self { stream }
    }

    pub async fn write(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).await.expect("write");
        self.stream.flush().await.expect("flush");
    }

    pub async fn read_byte(&mut self) -> u8 {
        self.stream.read_u8().await.expect("read a byte")
    }

    pub async fn try_read_message(&mut self) -> std::io::Result<Message> {
        let tag = self.stream.read_u8().await?;
        let len = self.stream.read_i32().await?;
        let mut body = vec![0u8; (len - 4).max(0) as usize];
        self.stream.read_exact(&mut body).await?;
        Ok(Message { tag, body })
    }

    pub async fn read_message(&mut self) -> Message {
        self.try_read_message().await.expect("read a message")
    }

    /// Reads until `ReadyForQuery`, returning everything including it.
    pub async fn read_until_ready(&mut self) -> Vec<Message> {
        let mut out = Vec::new();
        loop {
            let msg = self.read_message().await;
            let done = msg.tag == b'Z';
            out.push(msg);
            if done {
                return out;
            }
        }
    }

    /// Startup plus cleartext password authentication, leaving the session
    /// idle and ready for a query.
    pub async fn login(&mut self, user: &str, password: &str) -> Vec<Message> {
        self.write(&startup_packet(user, DATABASE)).await;
        let auth = self.read_message().await;
        assert_eq!(auth.tag, b'R', "expected an authentication request");
        assert_eq!(
            i32::from_be_bytes(auth.body[0..4].try_into().unwrap()),
            3,
            "expected AuthenticationCleartextPassword"
        );
        self.write(&password_message(password)).await;
        self.read_until_ready().await
    }

    pub async fn simple_query(&mut self, sql: &str) -> Vec<Message> {
        self.write(&query(sql)).await;
        self.read_until_ready().await
    }
}

/// Wraps an already connected socket in TLS, trusting only `cert`.
pub async fn upgrade_to_tls(
    mut stream: TcpStream,
    cert: &CertificateDer<'static>,
) -> tokio_rustls::client::TlsStream<TcpStream> {
    stream.write_all(&ssl_request()).await.expect("SSLRequest");
    let mut answer = [0u8; 1];
    stream.read_exact(&mut answer).await.expect("SSL answer");
    assert_eq!(&answer, b"S", "server refused TLS");
    TlsConnector::from(Arc::new(client_tls_config(cert)))
        .connect(ServerName::try_from("localhost").unwrap(), stream)
        .await
        .expect("TLS handshake")
}

pub fn client_tls_config(cert: &CertificateDer<'static>) -> ClientConfig {
    let mut roots = RootCertStore::empty();
    roots.add(cert.clone()).expect("trust the test certificate");
    ClientConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("protocol versions")
    .with_root_certificates(roots)
    .with_no_client_auth()
}

// --- frontend message encoders -------------------------------------------

fn untagged(body: Vec<u8>) -> Vec<u8> {
    let mut out = ((body.len() + 4) as i32).to_be_bytes().to_vec();
    out.extend_from_slice(&body);
    out
}

fn tagged(tag: u8, body: Vec<u8>) -> Vec<u8> {
    let mut out = vec![tag];
    out.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    out.extend_from_slice(&body);
    out
}

fn cstr(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(s.as_bytes());
    out.push(0);
}

pub fn startup_packet(user: &str, database: &str) -> Vec<u8> {
    let mut body = 196_608i32.to_be_bytes().to_vec();
    cstr(&mut body, "user");
    cstr(&mut body, user);
    cstr(&mut body, "database");
    cstr(&mut body, database);
    cstr(&mut body, "application_name");
    cstr(&mut body, "ferrite-tests");
    body.push(0);
    untagged(body)
}

pub fn ssl_request() -> Vec<u8> {
    untagged(80_877_103i32.to_be_bytes().to_vec())
}

pub fn cancel_request(process_id: i32, secret_key: i32) -> Vec<u8> {
    let mut body = 80_877_102i32.to_be_bytes().to_vec();
    body.extend_from_slice(&process_id.to_be_bytes());
    body.extend_from_slice(&secret_key.to_be_bytes());
    untagged(body)
}

pub fn password_message(password: &str) -> Vec<u8> {
    let mut body = Vec::new();
    cstr(&mut body, password);
    tagged(b'p', body)
}

pub fn query(sql: &str) -> Vec<u8> {
    let mut body = Vec::new();
    cstr(&mut body, sql);
    tagged(b'Q', body)
}

pub fn parse(name: &str, sql: &str, param_oids: &[i32]) -> Vec<u8> {
    let mut body = Vec::new();
    cstr(&mut body, name);
    cstr(&mut body, sql);
    body.extend_from_slice(&(param_oids.len() as i16).to_be_bytes());
    for oid in param_oids {
        body.extend_from_slice(&oid.to_be_bytes());
    }
    tagged(b'P', body)
}

/// `Bind` with text-format parameters and text-format results.
pub fn bind(portal: &str, statement: &str, params: &[Option<&str>]) -> Vec<u8> {
    let mut body = Vec::new();
    cstr(&mut body, portal);
    cstr(&mut body, statement);
    body.extend_from_slice(&0i16.to_be_bytes());
    body.extend_from_slice(&(params.len() as i16).to_be_bytes());
    for param in params {
        match param {
            None => body.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(v) => {
                body.extend_from_slice(&(v.len() as i32).to_be_bytes());
                body.extend_from_slice(v.as_bytes());
            }
        }
    }
    body.extend_from_slice(&0i16.to_be_bytes());
    tagged(b'B', body)
}

pub fn describe(kind: u8, name: &str) -> Vec<u8> {
    let mut body = vec![kind];
    cstr(&mut body, name);
    tagged(b'D', body)
}

pub fn execute(portal: &str, max_rows: i32) -> Vec<u8> {
    let mut body = Vec::new();
    cstr(&mut body, portal);
    body.extend_from_slice(&max_rows.to_be_bytes());
    tagged(b'E', body)
}

pub fn close(kind: u8, name: &str) -> Vec<u8> {
    let mut body = vec![kind];
    cstr(&mut body, name);
    tagged(b'C', body)
}

pub fn sync() -> Vec<u8> {
    tagged(b'S', Vec::new())
}

pub fn terminate() -> Vec<u8> {
    tagged(b'X', Vec::new())
}
