//! The Ferrite server binary: assembles storage, catalog, parser, planner,
//! executor and procedure registry behind the PostgreSQL wire protocol and
//! listens on 5432.
//!
//! [`build_handler`] is where the whole engine is put together, once, at
//! startup. Everything a connection touches afterwards hangs off the
//! [`Engine`](engine::Engine) it returns.

use std::sync::Arc;

use ferrite_common::Identity;
use ferrite_proc::ProcRegistry;
use ferrite_protocol::auth::{identity_for_user, superuser_role, StaticAuthenticator};
use ferrite_protocol::{AuthThrottle, QueryHandler, Server, ServerConfig, TlsMode};
use rand::Rng;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

mod banner;
mod describe;
mod engine;
mod observability;
mod settings;

use engine::{Engine, EngineLimits};
use settings::Settings;

/// Stack every runtime thread gets.
///
/// The planner and the executor walk an expression tree recursively, and
/// so does dropping one. `ferrite-sql` caps how tall a client can make
/// that tree (`MAX_DEPTH`), and this is the other half of the same
/// guarantee: enough room that the capped depth is nowhere near the
/// limit, on a platform whose default is 1–2 MiB. A stack overflow is not
/// a panic — it aborts the process, so no `catch_unwind` and no per-task
/// isolation can contain one.
const THREAD_STACK_SIZE: usize = 16 * 1024 * 1024;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(THREAD_STACK_SIZE)
        .build()?
        .block_on(run())
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    banner::banner();

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

    // One identity for the bootstrap account, derived from its name and
    // shared by both halves of the security model: the authenticator hands
    // it out on login, and the procedure registry is what actually grants
    // it permissions. Without the grant, a login would succeed and every
    // statement would then be denied — the model is deny-by-default and
    // network authentication alone confers nothing.
    let superuser = identity_for_user(&settings.user);
    let engine = build_handler(&settings, superuser)?;

    if let Some(listen) = &settings.metrics_listen {
        observability::serve(listen, Arc::clone(&engine)).await?;
    }
    // Outside the block above on purpose: the sampler feeds the gauges, but
    // it is also what notices a transaction left open and the distance to
    // the commit-bitmap ceiling. Those warnings belong in the log whether
    // or not anything is scraping.
    observability::spawn_sampler(Arc::clone(&engine), settings.data_dir.clone());

    let tls = build_tls(&settings)?;
    let authenticator = StaticAuthenticator::new().with_account(
        &settings.user,
        &password,
        superuser,
        superuser_role(),
    );
    let throttle = match settings.auth_throttle {
        Some(policy) => AuthThrottle::new(policy),
        None => {
            warn!(
                "FERRITE_AUTH_THROTTLE_DISABLE is set: password guessing is unlimited. \
                 Only acceptable when the listener is already unreachable from untrusted hosts."
            );
            AuthThrottle::disabled()
        }
    };
    let config = ServerConfig::new(
        Arc::clone(&engine) as Arc<dyn QueryHandler>,
        Arc::new(authenticator),
        tls,
    )
    .with_throttle(throttle)
    .with_max_connections(settings.max_connections);

    let server = Server::bind(&settings.listen, config).await?;
    info!(
        addr = %server.local_addr()?,
        tls = settings.tls_description(),
        data = %settings.data_dir.display(),
        max_connections = settings.max_connections,
        statement_timeout = ?settings.statement_timeout,
        transaction_timeout = ?settings.transaction_timeout,
        max_result_rows = settings.max_result_rows,
        "ferrite-server listening"
    );

    tokio::select! {
        result = server.run() => result?,
        _ = shutdown() => info!("shutting down"),
    }

    // Fold the journal into the data file so a restart has nothing to
    // replay. Recovery reaches the same state without it; this only makes
    // the next start cheap.
    if let Err(err) = engine.checkpoint() {
        warn!(error = %err, "checkpoint on shutdown failed");
    }
    Ok(())
}

/// Resolves when the process is asked to stop.
///
/// `docker stop` sends `SIGTERM` and only escalates to `SIGKILL` after the
/// grace period, so without this the container never shuts down cleanly and
/// every restart pays for journal recovery it did not need.
async fn shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(err) => {
                warn!(error = %err, "cannot listen for SIGTERM");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Builds the engine every connection is served from.
fn build_handler(
    settings: &Settings,
    superuser: Identity,
) -> Result<Arc<Engine>, Box<dyn std::error::Error>> {
    let mut procs = ProcRegistry::new();
    procs.grant_role(superuser, superuser_role());

    std::fs::create_dir_all(&settings.data_dir)?;
    Ok(Arc::new(Engine::open_with(
        &settings.data_dir,
        procs,
        EngineLimits {
            statement_timeout: settings.statement_timeout,
            transaction_timeout: settings.transaction_timeout,
            max_result_rows: settings.max_result_rows,
            checkpoint_journal_bytes: settings.checkpoint_journal_bytes,
        },
    )?))
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
