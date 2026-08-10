//! The Ferrite server binary.
//!
//! Today it wires the PostgreSQL wire protocol to
//! `ferrite_protocol::mock::MockHandler`, because the storage, catalog and
//! executor crates are still scaffolds. Everything below the
//! `QueryHandler` seam is therefore real — TLS, authentication, framing,
//! both query flows — and everything above it is a stand-in. Swapping in
//! the real engine is one line in [`build_handler`]; see the crate README.

use std::sync::Arc;

use ferrite_protocol::auth::{superuser_role, StaticAuthenticator};
use ferrite_protocol::mock::MockHandler;
use ferrite_protocol::{QueryHandler, Server, ServerConfig, TlsMode};
use rand::Rng;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

mod settings;

use settings::Settings;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("FERRITE_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let settings = Settings::from_env()?;
    let password = match settings.password.clone() {
        Some(password) => password,
        None => {
            let generated = random_password();
            warn!(
                user = %settings.user,
                password = %generated,
                "FERRITE_PASSWORD is unset; generated a one-off password for this run"
            );
            generated
        }
    };

    let tls = build_tls(&settings)?;
    let authenticator =
        StaticAuthenticator::new().with_user(&settings.user, &password, superuser_role());
    let config = ServerConfig::new(build_handler(), Arc::new(authenticator), tls);

    let server = Server::bind(&settings.listen, config).await?;
    info!(
        addr = %server.local_addr()?,
        tls = settings.tls_description(),
        "ferrite-server listening"
    );

    tokio::select! {
        result = server.run() => result?,
        _ = tokio::signal::ctrl_c() => info!("shutting down"),
    }
    Ok(())
}

/// The single point where the real engine gets plugged in: replace the mock
/// with whatever `ferrite-exec` ends up exposing as a
/// [`QueryHandler`](ferrite_protocol::QueryHandler).
fn build_handler() -> Arc<dyn QueryHandler> {
    warn!(
        "no storage engine is wired: serving ferrite-protocol's mock handler, \
         which answers a fixed set of statements and nothing else"
    );
    Arc::new(MockHandler::new())
}

fn build_tls(settings: &Settings) -> Result<TlsMode, Box<dyn std::error::Error>> {
    if settings.tls_disabled {
        warn!(
            "FERRITE_TLS_DISABLE is set: passwords will cross the network in the clear. \
             Only acceptable on a loopback or otherwise trusted transport."
        );
        return Ok(TlsMode::Disabled);
    }
    match (&settings.tls_cert, &settings.tls_key) {
        (Some(cert), Some(key)) => Ok(TlsMode::from_pem_files(cert, key)?),
        _ => {
            warn!(
                "FERRITE_TLS_CERT/FERRITE_TLS_KEY are unset: generated an ephemeral \
                 self-signed certificate. Clients must connect with sslmode=require \
                 rather than verify-full, and the certificate changes on every restart."
            );
            ephemeral_tls()
        }
    }
}

/// A self-signed certificate generated at startup, so that a fresh install
/// is encrypted by default instead of falling back to cleartext.
fn ephemeral_tls() -> Result<TlsMode, Box<dyn std::error::Error>> {
    let names = vec!["localhost".to_owned(), "127.0.0.1".to_owned(), hostname()];
    let signed = rcgen::generate_simple_self_signed(names)?;
    Ok(TlsMode::from_pkcs8_der(
        vec![signed.cert.der().to_vec()],
        signed.key_pair.serialize_der(),
    )?)
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "ferrite".to_owned())
}

fn random_password() -> String {
    const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::thread_rng();
    (0..24)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect()
}
