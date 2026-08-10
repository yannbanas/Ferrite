//! Compatibility proof with a client Ferrite did not write.
//!
//! `psql` is not available in the development sandbox, so the external
//! client here is `tokio-postgres`: an independent, widely used
//! implementation of the same protocol. It drives the extended query flow
//! (Parse/Describe/Bind/Execute) with **binary** parameter and result
//! formats, which is the path a hand-rolled test client is least likely to
//! get right by accident.

mod common;

use common::*;
use ferrite_protocol::TlsMode;
use tokio_postgres::NoTls;

async fn connect_plaintext() -> tokio_postgres::Client {
    let addr = start_plaintext().await;
    let conn_str = format!(
        "host=127.0.0.1 port={} user={USER} password={PASSWORD} dbname={DATABASE} sslmode=disable",
        addr.port()
    );
    let (client, connection) = tokio_postgres::connect(&conn_str, NoTls)
        .await
        .expect("tokio-postgres connect");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

#[tokio::test]
async fn tokio_postgres_completes_a_simple_query() {
    let client = connect_plaintext().await;
    let messages = client
        .simple_query("SELECT 1")
        .await
        .expect("simple_query round trip");
    let row = messages
        .iter()
        .find_map(|m| match m {
            tokio_postgres::SimpleQueryMessage::Row(row) => Some(row),
            _ => None,
        })
        .expect("a row");
    assert_eq!(row.get(0), Some("1"));
}

#[tokio::test]
async fn tokio_postgres_prepares_and_executes_with_binary_results() {
    let client = connect_plaintext().await;
    let row = client
        .query_one("SELECT 1", &[])
        .await
        .expect("prepared query");
    assert_eq!(row.get::<_, i32>(0), 1);
    assert_eq!(row.columns()[0].name(), "?column?");
}

#[tokio::test]
async fn tokio_postgres_round_trips_a_binary_parameter() {
    let client = connect_plaintext().await;
    let row = client
        .query_one("SELECT $1::int4", &[&41i32])
        .await
        .expect("parameterised query");
    assert_eq!(row.get::<_, i32>(0), 41);
}

#[tokio::test]
async fn tokio_postgres_decodes_every_column_type_and_nulls() {
    let client = connect_plaintext().await;
    let rows = client
        .query("SELECT * FROM pets", &[])
        .await
        .expect("multi-column query");
    assert_eq!(rows.len(), 3);

    let columns: Vec<&str> = rows[0].columns().iter().map(|c| c.name()).collect();
    assert_eq!(
        columns,
        vec![
            "id",
            "name",
            "adopted",
            "weight_kg",
            "born_at",
            "external_id",
            "profile"
        ]
    );
    let types: Vec<&tokio_postgres::types::Type> =
        rows[0].columns().iter().map(|c| c.type_()).collect();
    assert_eq!(
        types,
        vec![
            &tokio_postgres::types::Type::INT4,
            &tokio_postgres::types::Type::TEXT,
            &tokio_postgres::types::Type::BOOL,
            &tokio_postgres::types::Type::FLOAT8,
            &tokio_postgres::types::Type::TIMESTAMPTZ,
            &tokio_postgres::types::Type::UUID,
            &tokio_postgres::types::Type::JSON,
        ]
    );

    assert_eq!(rows[0].get::<_, i32>("id"), 1);
    assert_eq!(rows[0].get::<_, &str>("name"), "Rex");
    assert!(rows[0].get::<_, bool>("adopted"));
    assert_eq!(rows[0].get::<_, f64>("weight_kg"), 12.5);
    assert_eq!(rows[1].get::<_, &str>("name"), "Mœbius");
    assert_eq!(rows[2].get::<_, Option<i32>>("id"), None);
    assert_eq!(rows[2].get::<_, Option<&str>>("name"), None);
}

#[tokio::test]
async fn tokio_postgres_surfaces_a_server_error_and_keeps_the_connection() {
    let client = connect_plaintext().await;
    let err = client
        .query("SELECT nonsense", &[])
        .await
        .expect_err("the mock handler rejects this");
    assert_eq!(
        err.code(),
        Some(&tokio_postgres::error::SqlState::SYNTAX_ERROR)
    );
    assert_eq!(
        client
            .query_one("SELECT 1", &[])
            .await
            .unwrap()
            .get::<_, i32>(0),
        1
    );
}

#[tokio::test]
async fn tokio_postgres_connects_over_tls() {
    let cert = self_signed_cert();
    let tls_mode = TlsMode::from_der(cert.chain.clone(), cert.key.clone_key()).expect("TLS mode");
    let addr = start(config(tls_mode)).await;

    let connector =
        tokio_postgres_rustls::MakeRustlsConnect::new(client_tls_config(&cert.chain[0]));
    let conn_str = format!(
        "host=localhost port={} user={USER} password={PASSWORD} dbname={DATABASE} sslmode=require",
        addr.port()
    );
    let (client, connection) = tokio_postgres::connect(&conn_str, connector)
        .await
        .expect("tokio-postgres TLS connect");
    tokio::spawn(async move {
        let _ = connection.await;
    });

    assert_eq!(
        client
            .query_one("SELECT 1", &[])
            .await
            .unwrap()
            .get::<_, i32>(0),
        1
    );
}

#[tokio::test]
async fn tokio_postgres_is_refused_when_it_declines_tls() {
    let cert = self_signed_cert();
    let tls_mode = TlsMode::from_der(cert.chain.clone(), cert.key.clone_key()).expect("TLS mode");
    let addr = start(config(tls_mode)).await;
    let conn_str = format!(
        "host=127.0.0.1 port={} user={USER} password={PASSWORD} dbname={DATABASE} sslmode=disable",
        addr.port()
    );
    let result = tokio_postgres::connect(&conn_str, NoTls).await;
    assert!(result.is_err(), "a cleartext client must not get a session");
}
