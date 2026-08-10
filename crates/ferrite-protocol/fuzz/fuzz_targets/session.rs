//! Drives the whole connection state machine — startup, authentication,
//! both query flows — with the fuzzer supplying the client's bytes. Catches
//! the failures a per-message decoder fuzzer cannot: bad state transitions,
//! and any panic reachable only from a particular message ordering.
#![no_main]

use std::io::Cursor;
use std::sync::Arc;

use libfuzzer_sys::fuzz_target;

use ferrite_protocol::auth::{superuser_role, StaticAuthenticator};
use ferrite_protocol::mock::MockHandler;
use ferrite_protocol::{serve_connection, ServerConfig, TlsMode};

fuzz_target!(|data: &[u8]| {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("build a runtime");
    runtime.block_on(async {
        let config = Arc::new(ServerConfig::new(
            Arc::new(MockHandler::new()),
            Arc::new(StaticAuthenticator::new().with_user("ferrite", "hunter2", superuser_role())),
            // The fuzzer's job is the protocol, not the TLS stack, which
            // rustls fuzzes on its own.
            TlsMode::Disabled,
        ));
        let stream = tokio::io::join(Cursor::new(data.to_vec()), tokio::io::sink());
        let _ = serve_connection(stream, config).await;
    });
});
