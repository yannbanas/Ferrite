//! The metrics endpoint, the background sampler that feeds it the numbers
//! no request path produces, and the alerts derived from them.
//!
//! Counters on the query path record themselves as work happens. Sizes,
//! transaction-id progress and transaction age do not: nothing increments
//! when a file grows or when a client forgets to commit. They are sampled
//! on a timer instead, off the runtime, because reading them takes the
//! storage lock.
//!
//! The sampler is also where the *early* warnings live. A metric only
//! helps someone already looking at a dashboard; a limit that is a hard
//! stop rather than a slowdown has to reach the log well before it is hit,
//! because by then there is nothing to do about it online.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ferrite_metrics::{metrics, Endpoint, HealthProbe};
use ferrite_storage::{DATA_FILE, JOURNAL_FILE, MAX_TXN_ID};
use tracing::{info, warn};

use crate::engine::Engine;

/// How often the sampled gauges are refreshed. Well under a typical 30 s
/// Prometheus scrape interval, so a scrape never reads a stale sample.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(10);

/// Fraction of [`MAX_TXN_ID`] that gets a warning, and the fraction that
/// gets one on every sample.
///
/// The ceiling is a wall, not a slope: when the commit bitmap runs out of
/// directory space, commits stop. At 4 x 10^7 transactions of headroom the
/// first threshold leaves months of notice at any plausible rate, which is
/// what it takes to plan a dump and reload.
const TXN_WARN_FRACTION: f64 = 0.70;
const TXN_CRITICAL_FRACTION: f64 = 0.90;

/// Pages the data file can address, from the `u32` page counter: 2^32
/// pages of 8 KiB, i.e. 32 TiB. Far off, but finite, and finite limits are
/// the ones worth warning about.
const MAX_PAGES: f64 = u32::MAX as f64;
const PAGES_WARN_FRACTION: f64 = 0.80;

/// A transaction open this long is almost certainly a client that forgot
/// to commit rather than a query still running. It holds the MVCC pruning
/// horizon back for the whole database, so old row versions accumulate for
/// as long as it lives.
const LONG_TRANSACTION: Duration = Duration::from_secs(300);

/// Binds the endpoint and starts serving it.
///
/// Binding failure is fatal at startup rather than logged and ignored: a
/// deployment that asked for an endpoint and silently did not get one has
/// no health check either, which is the opposite of what this is for.
pub async fn serve(listen: &str, engine: Arc<Engine>) -> Result<(), Box<dyn std::error::Error>> {
    let endpoint = Endpoint::bind(listen).await?;
    info!(
        addr = %endpoint.local_addr()?,
        "observability endpoint listening on /metrics and /health"
    );
    let probe: Arc<dyn HealthProbe> = engine;
    tokio::spawn(async move {
        if let Err(err) = endpoint.run(probe).await {
            warn!(error = %err, "observability endpoint stopped");
        }
    });
    Ok(())
}

/// Starts the periodic sampler. Runs until the process exits.
pub fn spawn_sampler(engine: Arc<Engine>, data_dir: PathBuf) {
    let alerts = Arc::new(Alerts::default());
    tokio::spawn(async move {
        loop {
            let engine = Arc::clone(&engine);
            let dir = data_dir.clone();
            let alerts = Arc::clone(&alerts);
            // `sample` blocks on the storage lock; keeping it off the async
            // workers means a long-running statement delays the sample
            // rather than a runtime thread.
            let _ = tokio::task::spawn_blocking(move || sample(&engine, &dir, &alerts)).await;
            tokio::time::sleep(SAMPLE_INTERVAL).await;
        }
    });
}

/// Remembers which one-shot warnings have already been said.
///
/// A threshold crossed once stays crossed, and repeating the same line
/// every ten seconds is how an operator learns to filter the log out.
#[derive(Default)]
pub struct Alerts {
    txn_ceiling: AtomicBool,
    data_size: AtomicBool,
    long_transaction: AtomicBool,
}

impl Alerts {
    /// True the first time `crossed` is true, and never again afterwards.
    /// Resets once the condition clears, so a recurring problem is
    /// reported once per episode rather than once per process.
    fn edge(flag: &AtomicBool, crossed: bool) -> bool {
        if !crossed {
            flag.store(false, Ordering::Relaxed);
            return false;
        }
        !flag.swap(true, Ordering::Relaxed)
    }
}

/// One pass over everything that has to be read rather than counted.
pub fn sample(engine: &Engine, data_dir: &Path, alerts: &Alerts) {
    metrics()
        .data_file_bytes
        .set(file_size(&data_dir.join(DATA_FILE)));
    metrics()
        .journal_bytes
        .set(file_size(&data_dir.join(JOURNAL_FILE)));
    metrics().txn_id_ceiling.set(MAX_TXN_ID as i64);

    let stats = match engine.stats() {
        Ok(stats) => stats,
        Err(err) => {
            warn!(error = %err, "could not sample storage counters");
            return;
        }
    };
    metrics().txn_id.set(stats.next_txn_id as i64);
    metrics()
        .oldest_transaction_seconds
        .set(stats.oldest_txn.map_or(0, |(_, age)| age.as_secs() as i64));

    let used = stats.next_txn_id as f64 / MAX_TXN_ID as f64;
    if Alerts::edge(
        &alerts.txn_ceiling,
        (TXN_WARN_FRACTION..TXN_CRITICAL_FRACTION).contains(&used),
    ) {
        warn!(
            transactions = stats.next_txn_id,
            ceiling = MAX_TXN_ID,
            used_percent = format!("{:.1}", used * 100.0),
            "approaching the commit-bitmap ceiling; plan a dump and reload \
             before it is reached, since commits stop dead at that point"
        );
    }
    // Past the critical mark the warning repeats on every sample: at this
    // point it is no longer news to be filed away, it is time left.
    if used >= TXN_CRITICAL_FRACTION {
        warn!(
            transactions = stats.next_txn_id,
            ceiling = MAX_TXN_ID,
            remaining = MAX_TXN_ID.saturating_sub(stats.next_txn_id),
            used_percent = format!("{:.1}", used * 100.0),
            "close to the commit-bitmap ceiling: commits will start failing"
        );
    }

    let pages = stats.page_count as f64 / MAX_PAGES;
    if Alerts::edge(&alerts.data_size, pages >= PAGES_WARN_FRACTION) {
        warn!(
            pages = stats.page_count,
            max_pages = u32::MAX,
            used_percent = format!("{:.1}", pages * 100.0),
            "the data file is approaching the addressable page limit"
        );
    }

    match stats.oldest_txn {
        Some((txn, age)) if age >= LONG_TRANSACTION => {
            if Alerts::edge(&alerts.long_transaction, true) {
                warn!(
                    txn,
                    age_s = age.as_secs(),
                    active = stats.active_txns,
                    "a transaction has been open for a long time: it holds the MVCC \
                     pruning horizon back, so dead row versions accumulate until it ends"
                );
            }
        }
        _ => {
            Alerts::edge(&alerts.long_transaction, false);
        }
    }
}

/// A file that is not there yet reads as zero rather than as an error: the
/// journal is absent between a clean checkpoint and the next write.
fn file_size(path: &Path) -> i64 {
    std::fs::metadata(path).map(|m| m.len() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_edge_fires_once_per_episode() {
        let flag = AtomicBool::new(false);
        assert!(Alerts::edge(&flag, true), "the crossing is reported");
        assert!(!Alerts::edge(&flag, true), "and not repeated");
        assert!(!Alerts::edge(&flag, false), "clearing says nothing");
        assert!(
            Alerts::edge(&flag, true),
            "a second episode is reported again"
        );
    }
}
