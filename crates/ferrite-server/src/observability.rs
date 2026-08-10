//! The metrics endpoint and the background sampler that feeds it the
//! numbers no request path produces.
//!
//! Counters on the query path record themselves as work happens. Sizes and
//! transaction-id progress do not: nothing increments when a file grows.
//! They are sampled on a timer instead, off the runtime, because reading
//! them takes the storage lock.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ferrite_metrics::{metrics, Endpoint};
use ferrite_storage::{DATA_FILE, JOURNAL_FILE, MAX_TXN_ID};
use tracing::{info, warn};

use crate::engine::Engine;

/// How often the sampled gauges are refreshed. Well under a typical 30 s
/// Prometheus scrape interval, so a scrape never reads a stale sample.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(10);

/// Binds the endpoint and starts serving it.
///
/// Binding failure is fatal at startup rather than logged and ignored: a
/// deployment that asked for an endpoint and silently did not get one has
/// no health check either, which is the opposite of what this is for.
pub async fn serve(listen: &str) -> Result<(), Box<dyn std::error::Error>> {
    let endpoint = Endpoint::bind(listen).await?;
    info!(addr = %endpoint.local_addr()?, "metrics endpoint listening");
    tokio::spawn(async move {
        if let Err(err) = endpoint.run().await {
            warn!(error = %err, "metrics endpoint stopped");
        }
    });
    Ok(())
}

/// Starts the periodic sampler. Runs until the process exits.
pub fn spawn_sampler(engine: Arc<Engine>, data_dir: PathBuf) {
    tokio::spawn(async move {
        loop {
            let engine = Arc::clone(&engine);
            let dir = data_dir.clone();
            // `sample` blocks on the storage lock; keeping it off the async
            // workers means a long-running statement delays the sample
            // rather than a runtime thread.
            let _ = tokio::task::spawn_blocking(move || sample(&engine, &dir)).await;
            tokio::time::sleep(SAMPLE_INTERVAL).await;
        }
    });
}

/// One pass over everything that has to be read rather than counted.
pub fn sample(engine: &Engine, data_dir: &Path) {
    metrics()
        .data_file_bytes
        .set(file_size(&data_dir.join(DATA_FILE)));
    metrics()
        .journal_bytes
        .set(file_size(&data_dir.join(JOURNAL_FILE)));
    metrics().txn_id_ceiling.set(MAX_TXN_ID as i64);

    match engine.stats() {
        Ok(stats) => metrics().txn_id.set(stats.next_txn_id as i64),
        Err(err) => warn!(error = %err, "could not sample storage counters"),
    }
}

/// A file that is not there yet reads as zero rather than as an error: the
/// journal is absent between a clean checkpoint and the next write.
fn file_size(path: &Path) -> i64 {
    std::fs::metadata(path).map(|m| m.len() as i64).unwrap_or(0)
}
