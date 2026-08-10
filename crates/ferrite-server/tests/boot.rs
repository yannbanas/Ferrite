//! Boots the real binary as a child process and talks to it with a real
//! PostgreSQL client. This is the only test that covers `main` itself:
//! argument handling, TLS defaults and the listener wiring.

use std::net::TcpListener;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// Reserves a port by binding and immediately releasing it. A race is
/// possible in principle; in practice the child binds within milliseconds.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("local addr")
        .port()
}

struct ServerProcess(Child);

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn(port: u16, extra: &[(&str, &str)]) -> ServerProcess {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ferrite-server"));
    command
        .env("FERRITE_LISTEN", format!("127.0.0.1:{port}"))
        .env("FERRITE_USER", "ferrite")
        .env("FERRITE_PASSWORD", "hunter2")
        .env("FERRITE_LOG", "warn");
    for (key, value) in extra {
        command.env(key, value);
    }
    ServerProcess(command.spawn().expect("spawn ferrite-server"))
}

async fn wait_for_port(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("ferrite-server never started listening on {port}");
}

#[tokio::test]
async fn the_binary_serves_a_query_over_a_plaintext_listener() {
    let port = free_port();
    let _server = spawn(port, &[("FERRITE_TLS_DISABLE", "1")]);
    wait_for_port(port).await;

    let conn_str = format!(
        "host=127.0.0.1 port={port} user=ferrite password=hunter2 dbname=app sslmode=disable"
    );
    let (client, connection) = tokio_postgres::connect(&conn_str, tokio_postgres::NoTls)
        .await
        .expect("connect to the spawned server");
    tokio::spawn(async move {
        let _ = connection.await;
    });

    let row = client.query_one("SELECT 1", &[]).await.expect("query");
    assert_eq!(row.get::<_, i32>(0), 1);
}

#[tokio::test]
async fn the_binary_requires_tls_by_default() {
    let port = free_port();
    let _server = spawn(port, &[]);
    wait_for_port(port).await;

    // No TLS flags at all: the server must still have generated a
    // certificate and must refuse a cleartext session.
    let conn_str = format!(
        "host=127.0.0.1 port={port} user=ferrite password=hunter2 dbname=app sslmode=disable"
    );
    let result = tokio_postgres::connect(&conn_str, tokio_postgres::NoTls).await;
    assert!(
        result.is_err(),
        "a default listener must not accept a cleartext session"
    );
}
