//! The observability listener: a minimal HTTP/1.1 server serving
//! `GET /metrics` and `GET /health`.
//!
//! It answers scrapers and orchestrators, not browsers, so it implements
//! exactly the subset those send: one request per connection, no
//! keep-alive, no chunked bodies, no TLS. Request bytes come from an
//! untrusted peer and are bounded before anything is allocated, the same
//! rule `ferrite-protocol` follows.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tracing::{debug, warn};

use crate::metrics;

/// Largest request head accepted. A scrape sends a few hundred bytes; the
/// bound stops a peer from making the server buffer without limit.
const MAX_REQUEST_BYTES: usize = 8 * 1024;

/// The Prometheus text exposition content type, as scrapers expect it.
const METRICS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// How long a health probe may take before the endpoint calls it a
/// failure. Comfortably above any healthy round trip and comfortably below
/// the `HEALTHCHECK --timeout` in the `Dockerfile`, so a wedged engine
/// produces a 503 with a reason rather than a client-side timeout with
/// none.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(3);

/// Proves the thing behind the endpoint is actually working.
///
/// A liveness check that only tests the listener is worse than none: a
/// server whose engine is wedged still accepts TCP and still answers this
/// port, because neither goes through the engine. An implementation must
/// therefore do a real round trip through whatever could be stuck.
#[async_trait]
pub trait HealthProbe: Send + Sync + 'static {
    /// `Ok(())` when the engine answered; `Err(reason)` otherwise, where
    /// `reason` is shown to the operator by `docker inspect` and friends.
    async fn check(&self) -> Result<(), String>;
}

/// A probe for a deployment with nothing to ask — the endpoint running
/// without an engine behind it. Reports healthy as soon as the process is
/// up, which is exactly the weak check [`HealthProbe`] exists to replace,
/// so it is only appropriate when there is genuinely nothing to probe.
pub struct AlwaysHealthy;

#[async_trait]
impl HealthProbe for AlwaysHealthy {
    async fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

/// A bound observability listener.
pub struct Endpoint {
    listener: TcpListener,
}

impl Endpoint {
    pub async fn bind(addr: impl ToSocketAddrs) -> std::io::Result<Self> {
        Ok(Self {
            listener: TcpListener::bind(addr).await?,
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accepts requests until the listener fails.
    ///
    /// One slow or hostile scraper must not stall the next one, so every
    /// connection is served on its own task, and a connection that goes
    /// quiet is dropped rather than held.
    pub async fn run(self, probe: Arc<dyn HealthProbe>) -> std::io::Result<()> {
        loop {
            let (stream, peer) = match self.listener.accept().await {
                Ok(accepted) => accepted,
                Err(err) => {
                    warn!(error = %err, "observability listener accept failed");
                    return Err(err);
                }
            };
            let probe = Arc::clone(&probe);
            tokio::spawn(async move {
                if let Err(err) = serve_one(stream, probe).await {
                    debug!(%peer, error = %err, "observability request failed");
                }
            });
        }
    }
}

async fn serve_one(mut stream: TcpStream, probe: Arc<dyn HealthProbe>) -> std::io::Result<()> {
    let Some(request) = read_head(&mut stream).await? else {
        return respond(&mut stream, 400, "text/plain", "bad request").await;
    };
    match route_of(&request) {
        Some(Route::Metrics) => {
            respond(&mut stream, 200, METRICS_CONTENT_TYPE, &metrics().encode()).await
        }
        Some(Route::Health) => {
            let (status, body) = health(probe.as_ref()).await;
            respond(&mut stream, status, "text/plain", &body).await
        }
        None => respond(&mut stream, 404, "text/plain", "not found").await,
    }
}

/// Runs the probe under a deadline and records the outcome.
///
/// The deadline is the point of the whole route: an engine that has stopped
/// answering does not return an error, it returns nothing, and a probe
/// without a timeout would hang exactly as hard as the thing it is meant to
/// detect.
async fn health(probe: &dyn HealthProbe) -> (u16, String) {
    let started = Instant::now();
    let outcome = tokio::time::timeout(HEALTH_TIMEOUT, probe.check()).await;
    let elapsed = started.elapsed();

    metrics().health_checks_total.inc();
    metrics().record_health_probe(elapsed.as_secs_f64());

    match outcome {
        Ok(Ok(())) => (200, format!("ok\nprobe={}us\n", elapsed.as_micros())),
        Ok(Err(reason)) => {
            metrics().health_failures_total.inc();
            warn!(reason = %reason, "health probe failed");
            (503, format!("unhealthy\n{reason}\n"))
        }
        Err(_) => {
            metrics().health_failures_total.inc();
            warn!(
                timeout_s = HEALTH_TIMEOUT.as_secs(),
                "health probe timed out: the engine is not answering"
            );
            (
                503,
                format!(
                    "unhealthy\nthe engine did not answer within {}s\n",
                    HEALTH_TIMEOUT.as_secs()
                ),
            )
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Metrics,
    Health,
}

/// Maps a request line to a route. Only `GET` is answered; a query string
/// is ignored, since neither route takes parameters.
fn route_of(request_line: &str) -> Option<Route> {
    let mut parts = request_line.split(' ');
    let method = parts.next()?;
    let target = parts.next()?;
    if method != "GET" && method != "HEAD" {
        return None;
    }
    match target.split('?').next()? {
        "/metrics" => Some(Route::Metrics),
        "/health" => Some(Route::Health),
        _ => None,
    }
}

/// Reads up to the end of the request head and returns its first line.
///
/// The body, if any, is not read: neither route takes one, and the
/// connection is closed straight after the response.
async fn read_head(stream: &mut TcpStream) -> std::io::Result<Option<String>> {
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 512];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..read]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.windows(2).any(|w| w == b"\n\n") {
            break;
        }
        if buf.len() > MAX_REQUEST_BYTES {
            return Ok(None);
        }
    }
    let head = String::from_utf8_lossy(&buf);
    Ok(head.lines().next().map(|line| line.trim_end().to_owned()))
}

async fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        503 => "Service Unavailable",
        _ => "Unknown",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    let _ = stream.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A probe that answers however the test asks it to, including by never
    /// answering at all.
    struct Scripted(Result<(), String>, bool);

    #[async_trait]
    impl HealthProbe for Scripted {
        async fn check(&self) -> Result<(), String> {
            if self.1 {
                std::future::pending::<()>().await;
            }
            self.0.clone()
        }
    }

    async fn get(probe: Arc<dyn HealthProbe>, target: &str) -> String {
        let endpoint = Endpoint::bind("127.0.0.1:0").await.expect("bind");
        let addr = endpoint.local_addr().expect("addr");
        tokio::spawn(endpoint.run(probe));

        let mut stream = TcpStream::connect(addr).await.expect("connect");
        stream
            .write_all(format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
            .await
            .expect("write");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .await
            .expect("read the response");
        response
    }

    fn healthy() -> Arc<dyn HealthProbe> {
        Arc::new(AlwaysHealthy)
    }

    #[test]
    fn only_get_on_a_known_path_routes() {
        assert_eq!(route_of("GET /metrics HTTP/1.1"), Some(Route::Metrics));
        assert_eq!(route_of("HEAD /metrics HTTP/1.1"), Some(Route::Metrics));
        assert_eq!(route_of("GET /metrics?x=1 HTTP/1.1"), Some(Route::Metrics));
        assert_eq!(route_of("GET /health HTTP/1.1"), Some(Route::Health));
        assert_eq!(route_of("POST /metrics HTTP/1.1"), None);
        assert_eq!(route_of("GET / HTTP/1.1"), None);
        assert_eq!(route_of("GET /../etc/passwd HTTP/1.1"), None);
        assert_eq!(route_of("garbage"), None);
        assert_eq!(route_of(""), None);
    }

    #[tokio::test]
    async fn the_endpoint_serves_the_registry_over_http() {
        let response = get(healthy(), "/metrics").await;
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("text/plain; version=0.0.4"));
        assert!(response.contains("ferrite_connections_total"));
    }

    #[tokio::test]
    async fn an_unknown_path_is_a_404_and_never_hangs() {
        let response = get(healthy(), "/nope").await;
        assert!(response.starts_with("HTTP/1.1 404"));
    }

    #[tokio::test]
    async fn health_is_200_when_the_probe_answers() {
        let response = get(healthy(), "/health").await;
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("ok"));
    }

    #[tokio::test]
    async fn health_is_503_with_the_reason_when_the_probe_fails() {
        let probe = Arc::new(Scripted(Err("disk is read-only".into()), false));
        let response = get(probe, "/health").await;
        assert!(response.starts_with("HTTP/1.1 503"));
        assert!(response.contains("disk is read-only"));
    }

    /// The case that motivates the whole route: the engine never answers.
    /// The endpoint must still respond, with a 503, rather than hang along
    /// with it.
    #[tokio::test(start_paused = true)]
    async fn health_is_503_when_the_probe_never_answers() {
        let probe = Arc::new(Scripted(Ok(()), true));
        let response = get(probe, "/health").await;
        assert!(response.starts_with("HTTP/1.1 503"));
        assert!(response.contains("did not answer"));
    }
}
