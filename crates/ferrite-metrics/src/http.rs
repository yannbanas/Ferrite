//! The observability listener: a minimal HTTP/1.1 server serving
//! `GET /metrics`.
//!
//! It answers scrapers and orchestrators, not browsers, so it implements
//! exactly the subset those send: one request per connection, no
//! keep-alive, no chunked bodies, no TLS. Request bytes come from an
//! untrusted peer and are bounded before anything is allocated, the same
//! rule `ferrite-protocol` follows.

use std::net::SocketAddr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tracing::{debug, warn};

use crate::metrics;

/// Largest request head accepted. A scrape sends a few hundred bytes; the
/// bound stops a peer from making the server buffer without limit.
const MAX_REQUEST_BYTES: usize = 8 * 1024;

/// The Prometheus text exposition content type, as scrapers expect it.
const METRICS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

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
    pub async fn run(self) -> std::io::Result<()> {
        loop {
            let (stream, peer) = match self.listener.accept().await {
                Ok(accepted) => accepted,
                Err(err) => {
                    warn!(error = %err, "observability listener accept failed");
                    return Err(err);
                }
            };
            tokio::spawn(async move {
                if let Err(err) = serve_one(stream).await {
                    debug!(%peer, error = %err, "observability request failed");
                }
            });
        }
    }
}

async fn serve_one(mut stream: TcpStream) -> std::io::Result<()> {
    let Some(request) = read_head(&mut stream).await? else {
        return respond(&mut stream, 400, "text/plain", "bad request").await;
    };
    let route = route_of(&request);
    match route {
        Some(Route::Metrics) => {
            respond(&mut stream, 200, METRICS_CONTENT_TYPE, &metrics().encode()).await
        }
        None => respond(&mut stream, 404, "text/plain", "not found").await,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Metrics,
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

    #[test]
    fn only_get_on_a_known_path_routes() {
        assert_eq!(route_of("GET /metrics HTTP/1.1"), Some(Route::Metrics));
        assert_eq!(route_of("HEAD /metrics HTTP/1.1"), Some(Route::Metrics));
        assert_eq!(route_of("GET /metrics?x=1 HTTP/1.1"), Some(Route::Metrics));
        assert_eq!(route_of("POST /metrics HTTP/1.1"), None);
        assert_eq!(route_of("GET / HTTP/1.1"), None);
        assert_eq!(route_of("GET /../etc/passwd HTTP/1.1"), None);
        assert_eq!(route_of("garbage"), None);
        assert_eq!(route_of(""), None);
    }

    #[tokio::test]
    async fn the_endpoint_serves_the_registry_over_http() {
        let endpoint = Endpoint::bind("127.0.0.1:0").await.expect("bind");
        let addr = endpoint.local_addr().expect("addr");
        tokio::spawn(endpoint.run());

        let mut stream = TcpStream::connect(addr).await.expect("connect");
        stream
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .expect("write");
        let mut response = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut stream, &mut response)
            .await
            .expect("read");

        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("text/plain; version=0.0.4"));
        assert!(response.contains("ferrite_connections_total"));
    }

    #[tokio::test]
    async fn an_unknown_path_is_a_404_and_never_hangs() {
        let endpoint = Endpoint::bind("127.0.0.1:0").await.expect("bind");
        let addr = endpoint.local_addr().expect("addr");
        tokio::spawn(endpoint.run());

        let mut stream = TcpStream::connect(addr).await.expect("connect");
        stream
            .write_all(b"GET /nope HTTP/1.1\r\n\r\n")
            .await
            .expect("write");
        let mut response = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut stream, &mut response)
            .await
            .expect("read");
        assert!(response.starts_with("HTTP/1.1 404"));
    }
}
