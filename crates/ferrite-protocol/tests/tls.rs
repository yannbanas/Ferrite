//! TLS is on by default: these tests hold that line.

mod common;

use common::*;
use ferrite_protocol::TlsMode;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

async fn start_tls() -> (std::net::SocketAddr, TestCert) {
    let cert = self_signed_cert();
    let tls = TlsMode::from_der(cert.chain.clone(), cert.key.clone_key()).expect("build TLS mode");
    let addr = start(config(tls)).await;
    (addr, cert)
}

#[tokio::test]
async fn a_full_session_runs_over_tls() {
    let (addr, cert) = start_tls().await;
    let stream = TcpStream::connect(addr).await.expect("connect");
    let tls = upgrade_to_tls(stream, &cert.chain[0]).await;

    let mut client = RawClient::wrapping(tls);
    client.login(USER, PASSWORD).await;
    let messages = client.simple_query("SELECT 1").await;
    assert_eq!(messages[1].row_values(), vec![Some("1".to_owned())]);
}

#[tokio::test]
async fn a_cleartext_startup_is_refused_when_tls_is_required() {
    let (addr, _cert) = start_tls().await;
    let mut client = RawClient::connect(addr).await;
    client.write(&startup_packet(USER, DATABASE)).await;

    let error = client.read_message().await;
    assert_eq!(error.tag, b'E');
    assert_eq!(error.severity().as_deref(), Some("FATAL"));
    assert_eq!(error.sqlstate().as_deref(), Some("28000"));
    assert!(error.error_message().unwrap().contains("TLS"));
    assert!(
        client.try_read_message().await.is_err(),
        "no password prompt may be offered in the clear"
    );
}

#[tokio::test]
async fn a_plaintext_listener_answers_n_to_an_ssl_request() {
    let addr = start_plaintext().await;
    let mut client = RawClient::connect(addr).await;
    client.write(&ssl_request()).await;
    assert_eq!(client.read_byte().await, b'N');

    // …and the client can then continue in the clear.
    client.login(USER, PASSWORD).await;
    let messages = client.simple_query("SELECT 1").await;
    assert_eq!(messages[1].row_values(), vec![Some("1".to_owned())]);
}

#[tokio::test]
async fn gssapi_encryption_is_refused_before_tls_is_offered() {
    let (addr, cert) = start_tls().await;
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let gssenc = {
        let mut packet = 8i32.to_be_bytes().to_vec();
        packet.extend_from_slice(&80_877_104i32.to_be_bytes());
        packet
    };
    stream.write_all(&gssenc).await.expect("GSSENCRequest");
    let mut answer = [0u8; 1];
    tokio::io::AsyncReadExt::read_exact(&mut stream, &mut answer)
        .await
        .expect("GSSENC answer");
    assert_eq!(&answer, b"N", "GSSAPI encryption is not supported");

    // TLS is still on the table after the refusal, which is the sequence
    // libpq uses when it is built with GSSAPI support.
    let tls = upgrade_to_tls(stream, &cert.chain[0]).await;
    let mut client = RawClient::wrapping(tls);
    client.login(USER, PASSWORD).await;
}

#[tokio::test]
async fn a_second_ssl_request_inside_tls_is_a_protocol_violation() {
    let (addr, cert) = start_tls().await;
    let stream = TcpStream::connect(addr).await.expect("connect");
    let tls = upgrade_to_tls(stream, &cert.chain[0]).await;

    let mut client = RawClient::wrapping(tls);
    client.write(&ssl_request()).await;
    // Either a fatal error or an immediate close is acceptable; what must
    // not happen is the server quietly accepting a nested handshake.
    if let Ok(msg) = client.try_read_message().await {
        assert_eq!(msg.tag, b'E');
    }
}

#[tokio::test]
async fn a_client_that_does_not_trust_the_certificate_cannot_connect() {
    let (addr, _cert) = start_tls().await;
    let other = self_signed_cert();
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream.write_all(&ssl_request()).await.expect("SSLRequest");
    let mut answer = [0u8; 1];
    tokio::io::AsyncReadExt::read_exact(&mut stream, &mut answer)
        .await
        .expect("SSL answer");
    assert_eq!(&answer, b"S");

    let connector =
        tokio_rustls::TlsConnector::from(std::sync::Arc::new(client_tls_config(&other.chain[0])));
    let result = connector
        .connect(
            tokio_rustls::rustls::pki_types::ServerName::try_from("localhost").unwrap(),
            stream,
        )
        .await;
    assert!(result.is_err(), "an untrusted certificate must be rejected");
}
