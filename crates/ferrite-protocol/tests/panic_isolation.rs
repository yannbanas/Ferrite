//! A panic in the query engine must cost exactly one connection.
//!
//! `tokio::spawn` already keeps a panicking task away from the runtime, so
//! "the server stays up" was true before any of this. What was not true is
//! that the client is *told*: the task unwound, the socket was dropped, and
//! the peer saw a connection reset it could not tell from a network
//! failure. These tests assert both halves — a real `ErrorResponse` on the
//! connection that panicked, and nothing at all on the ones that did not.
//!
//! `MockHandler` panics on the statement `PANIC`, which is the test gate.

mod common;

use common::*;
use ferrite_protocol::TlsMode;
use tokio_postgres::NoTls;

async fn client_on(addr: std::net::SocketAddr) -> tokio_postgres::Client {
    let conn_str = format!(
        "host=127.0.0.1 port={} user={USER} password={PASSWORD} dbname={DATABASE} sslmode=disable",
        addr.port()
    );
    let (client, connection) = tokio_postgres::connect(&conn_str, NoTls)
        .await
        .expect("connect");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

#[tokio::test]
async fn a_panicking_statement_is_reported_to_the_client_that_caused_it() {
    let addr = start_plaintext().await;
    let client = client_on(addr).await;

    let err = client
        .simple_query("PANIC")
        .await
        .expect_err("a panicking handler must not answer with success");
    let db = err
        .as_db_error()
        .unwrap_or_else(|| panic!("expected an ErrorResponse, got a transport failure: {err}"));
    assert_eq!(db.code().code(), "XX000");
    assert!(
        db.message().contains("panicked"),
        "the error should name what happened, got {:?}",
        db.message()
    );
}

#[tokio::test]
async fn other_connections_and_the_listener_are_untouched() {
    let addr = start_plaintext().await;
    let survivor = client_on(addr).await;
    let doomed = client_on(addr).await;

    // The survivor is mid-session before the panic, not opened after it.
    assert!(survivor.simple_query("SELECT 1").await.is_ok());

    assert!(doomed.simple_query("PANIC").await.is_err());

    // Still serving the connection that was already open …
    let row = survivor
        .query_one("SELECT 1", &[])
        .await
        .expect("an unrelated session must be unaffected");
    assert_eq!(row.get::<_, i32>(0), 1);

    // … and still accepting new ones.
    let fresh = client_on(addr).await;
    assert_eq!(
        fresh
            .query_one("SELECT 1", &[])
            .await
            .expect("the listener must still accept")
            .get::<_, i32>(0),
        1
    );
}

/// Ten panics in a row, so that nothing accumulates: a leaked task, a
/// poisoned shared lock, a listener that stops accepting.
#[tokio::test]
async fn repeated_panics_do_not_wear_the_server_down() {
    let addr = start_plaintext().await;
    for _ in 0..10 {
        let client = client_on(addr).await;
        assert!(client.simple_query("PANIC").await.is_err());
    }
    let fresh = client_on(addr).await;
    assert_eq!(
        fresh
            .query_one("SELECT 1", &[])
            .await
            .expect("ten panics later, the server still answers")
            .get::<_, i32>(0),
        1
    );
}

/// The extended query flow reaches the handler through two other methods
/// (`describe` and `execute_params`); both are guarded.
#[tokio::test]
async fn the_extended_query_flow_is_guarded_too() {
    let addr = start(config(TlsMode::Disabled)).await;
    let client = client_on(addr).await;
    assert!(client.query("PANIC", &[]).await.is_err());

    let fresh = client_on(addr).await;
    assert_eq!(
        fresh
            .query_one("SELECT 1", &[])
            .await
            .expect("the listener survived a panic in the extended flow")
            .get::<_, i32>(0),
        1
    );
}
