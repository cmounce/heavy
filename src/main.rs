mod latency;

use std::convert::Infallible;
use std::env;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use latency::LatencyMonitor;

use http_body_util::{Either, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderMap, HeaderName};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode, Uri};
use hyper_util::rt::TokioExecutor;
use hyper_util::server::conn::auto::Builder as ServerBuilder;
use serde_json::{Map, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

struct Config {
    bind: String,
    target: Uri,
    target_authority: hyper::http::uri::Authority,
    log_path: String,
    latency_weight: f64,
    latency_high_ms: f64,
    latency_low_ms: f64,
}

#[tokio::main]
async fn main() {
    let config = load_config();

    // Open the log file in append mode and spawn a dedicated writer task
    let log_file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config.log_path)
        .await
        .unwrap_or_else(|e| panic!("failed to open log file {}: {e}", config.log_path));
    let (log_tx, mut log_rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut writer = tokio::io::BufWriter::new(log_file);
        while let Some(line) = log_rx.recv().await {
            if let Err(e) = async {
                writer.write_all(line.as_bytes()).await?;
                writer.write_all(b"\n").await?;
                writer.flush().await
            }
            .await
            {
                eprintln!("heavy: log write failed: {e}");
            }
        }
    });

    let listener = TcpListener::bind(&config.bind)
        .await
        .unwrap_or_else(|e| panic!("failed to bind to {}: {e}", config.bind));

    let monitor = Arc::new(LatencyMonitor::new(
        config.latency_weight,
        config.latency_high_ms,
        config.latency_low_ms,
    ));

    eprintln!(
        "heavy: listening on {}, proxying to {}, logging to {}",
        config.bind, config.target, config.log_path
    );

    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                eprintln!("heavy: failed to accept connection: {e}");
                continue;
            }
        };

        let target_authority = config.target_authority.clone();
        let log_tx = log_tx.clone();
        let monitor = monitor.clone();

        // tokio::spawn moves the connection onto its own async task, so the accept loop
        // immediately continues waiting for the next connection.
        tokio::spawn(async move {
            // TokioIo adapts a tokio TcpStream into the I/O traits hyper expects
            let io = hyper_util::rt::TokioIo::new(stream);

            // Preserve capitalization of existing headers and prefer Title-Case for new ones. This
            // addresses compatibility problems with Anubis (which expects "X-Real-Ip") and maybe
            // with other software as well.
            if let Err(e) = ServerBuilder::new(TokioExecutor::new())
                .preserve_header_case(true)
                .title_case_headers(true)
                .serve_connection(
                    io,
                    service_fn(|req| {
                        handle_request(
                            req,
                            target_authority.clone(),
                            log_tx.clone(),
                            monitor.clone(),
                        )
                    }),
                )
                .await
            {
                eprintln!("heavy: connection error: {e}");
            }
        });
    }
}

fn load_config() -> Config {
    let bind = env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8011".to_string());

    let default_target = "http://localhost:3011";
    let target_str = env::var("TARGET").unwrap_or_else(|_| default_target.to_string());
    let (target, target_authority) = target_str
        .parse::<Uri>()
        .ok()
        .and_then(|uri| uri.authority().cloned().map(|auth| (uri, auth)))
        .unwrap_or_else(|| panic!("TARGET must be a valid URI with host (e.g. {default_target})"));

    let log_path = env::var("LOG_FILE").unwrap_or_else(|_| "heavy.jsonl".to_string());

    let latency_weight: f64 = env::var("LATENCY_WEIGHT")
        .ok()
        .map(|v| {
            v.parse()
                .expect("LATENCY_WEIGHT must be a number between 0 and 1")
        })
        .unwrap_or(0.01);

    let latency_high_ms: f64 = env::var("LATENCY_HIGH_MS")
        .ok()
        .map(|v| v.parse().expect("LATENCY_HIGH_MS must be a number"))
        .unwrap_or(500.0);

    let latency_low_ms: f64 = env::var("LATENCY_LOW_MS")
        .ok()
        .map(|v| v.parse().expect("LATENCY_LOW_MS must be a number"))
        .unwrap_or(250.0);

    assert!(
        latency_low_ms < latency_high_ms,
        "LATENCY_LOW_MS ({latency_low_ms}) must be less than LATENCY_HIGH_MS ({latency_high_ms})"
    );

    Config {
        bind,
        target,
        target_authority,
        log_path,
        latency_weight,
        latency_high_ms,
        latency_low_ms,
    }
}

/// Proxy a single request to the target, log metadata, and return the response.
async fn handle_request(
    req: Request<Incoming>,
    target_authority: hyper::http::uri::Authority,
    log_tx: mpsc::UnboundedSender<String>,
    monitor: Arc<LatencyMonitor>,
) -> Result<Response<Either<Incoming, Full<Bytes>>>, Infallible> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let timestamp_secs = timestamp.as_secs() as f64 + timestamp.subsec_millis() as f64 / 1000.0;

    // Rewrite URI to point at the target, preserving the original path and query
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.to_string())
        .unwrap_or_else(|| "/".to_string());

    // Capture request metadata for logging before we consume the request
    let method = req.method().to_string();
    let path = path_and_query.clone();
    let (parts, body) = req.into_parts();

    // Build the outgoing request and collect headers for logging in one pass
    let mut outgoing = Request::builder().method(parts.method).uri(path_and_query);
    let mut logged_headers = Map::new();
    for (name, value) in filter_headers(&parts.headers) {
        outgoing = outgoing.header(name, value);
        if let Ok(v) = value.to_str() {
            logged_headers.insert(name.to_string(), Value::String(v.to_string()));
        }
    }
    let outgoing = outgoing.body(body).unwrap();

    // Send to the target and measure round-trip latency
    let start = Instant::now();
    let (status, resp_body) = match connect_and_send(target_authority.as_str(), outgoing).await {
        Ok(resp) => {
            let (resp_parts, resp_body) = resp.into_parts();
            let mut response = Response::builder().status(resp_parts.status);
            for (name, value) in filter_headers(&resp_parts.headers) {
                response = response.header(name, value);
            }
            let status = resp_parts.status.as_u16();
            (status, response.body(Either::Left(resp_body)).unwrap())
        }
        Err(e) => {
            eprintln!("heavy: upstream request failed: {e}");
            let resp = Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Either::Right(Full::new(Bytes::from("502 Bad Gateway\n"))))
                .unwrap();
            (502, resp)
        }
    };
    let latency_ms = start.elapsed().as_millis() as f64;

    // Update the latency monitor and log state transitions
    let changed = monitor.update(latency_ms);
    let high_load = monitor.is_high_load();
    if changed {
        if high_load {
            eprintln!(
                "heavy: entering high load (avg latency {:.1}ms)",
                monitor.average()
            );
        } else {
            eprintln!(
                "heavy: leaving high load (avg latency {:.1}ms)",
                monitor.average()
            );
        }
    }

    // Send the log entry to the writer task
    let entry = serde_json::json!({
        "timestamp": timestamp_secs,
        "method": method,
        "path": path,
        "headers": Value::Object(logged_headers),
        "status": status,
        "latency_ms": latency_ms as u32,
        "avg_latency_ms": (monitor.average() * 1000.0).round() / 1000.0,
        "high_load": high_load,
    });
    let _ = log_tx.send(entry.to_string());

    Ok(resp_body)
}

/// Open a TCP connection to the target and send a request with header case preservation.
async fn connect_and_send(
    authority: &str,
    req: Request<Incoming>,
) -> Result<Response<Incoming>, Box<dyn std::error::Error + Send + Sync>> {
    let stream = tokio::net::TcpStream::connect(authority).await?;
    let io = hyper_util::rt::TokioIo::new(stream);

    let (mut sender, conn) = hyper::client::conn::http1::Builder::new()
        .preserve_header_case(true)
        .title_case_headers(true)
        .handshake(io)
        .await?;

    // Drive the connection in a background task
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            eprintln!("heavy: upstream connection error: {e}");
        }
    });

    Ok(sender.send_request(req).await?)
}

/// Iterate over headers that are allowed to be forwarded through the proxy.
///
/// This filters out hop-by-hop headers (headers that only apply to a direct connection between two
/// clients), including those specified by the Connection header itself. See RFC 9110 or this page
/// for more info: https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Connection
fn filter_headers(
    headers: &HeaderMap,
) -> impl Iterator<Item = (&HeaderName, &header::HeaderValue)> {
    // Collect extra header names listed in `Connection: keep-alive, x-custom, ...`
    let connection_skip: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .into_iter()
        .filter_map(|val| val.to_str().ok())
        .flat_map(|val| val.split(','))
        .filter_map(|name| name.trim().parse::<HeaderName>().ok())
        .collect();

    // Filter out explicitly-named headers and any that were in `Connection`
    headers.iter().filter(move |(name, _)| match **name {
        header::CONNECTION
        | header::PROXY_AUTHENTICATE
        | header::PROXY_AUTHORIZATION
        | header::TE
        | header::TRAILER
        | header::TRANSFER_ENCODING
        | header::UPGRADE => false,
        _ if *name == "keep-alive" || connection_skip.contains(name) => false,
        _ => true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_names(headers: &HeaderMap) -> Vec<String> {
        let mut names: Vec<String> = filter_headers(headers)
            .map(|(name, _)| name.to_string())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn filters_all_hop_by_hop_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONNECTION, "close".parse().unwrap());
        headers.insert("keep-alive", "timeout=5".parse().unwrap());
        headers.insert(header::PROXY_AUTHENTICATE, "Basic".parse().unwrap());
        headers.insert(header::PROXY_AUTHORIZATION, "Basic abc".parse().unwrap());
        headers.insert(header::TE, "trailers".parse().unwrap());
        headers.insert(header::TRAILER, "Expires".parse().unwrap());
        headers.insert(header::TRANSFER_ENCODING, "chunked".parse().unwrap());
        headers.insert(header::UPGRADE, "websocket".parse().unwrap());

        assert!(header_names(&headers).is_empty());
    }

    #[test]
    fn mixed_hop_by_hop_and_regular() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "*/*".parse().unwrap());
        headers.insert(header::CONNECTION, "close".parse().unwrap());
        headers.insert(header::HOST, "localhost".parse().unwrap());
        headers.insert(header::TRANSFER_ENCODING, "chunked".parse().unwrap());
        headers.insert(header::USER_AGENT, "curl/8.0".parse().unwrap());
        headers.insert("x-real-ip", "127.0.0.1".parse().unwrap());

        assert_eq!(
            header_names(&headers),
            vec!["accept", "host", "user-agent", "x-real-ip"]
        );
    }

    #[test]
    fn filters_connection_nominated_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONNECTION, "x-foo, x-baz".parse().unwrap());
        headers.insert("x-foo", "1".parse().unwrap());
        headers.insert("x-bar", "2".parse().unwrap());
        headers.insert("x-baz", "3".parse().unwrap());

        assert_eq!(header_names(&headers), vec!["x-bar"]);
    }
}
