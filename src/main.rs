mod latency;

use std::convert::Infallible;
use std::env;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use latency::LatencyMonitor;

use askama::Template;
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
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

const WORKER_JS: &str = include_str!("../web/worker.js");

#[derive(Template)]
#[template(path = "challenge.html")]
struct ChallengeTemplate {
    /// 64-char hex string (32 bytes)
    nonce: String,
    difficulty: u32,
}

/// A request to the access logger task to perform some action.
enum LogCmd {
    Append(String),
    Reopen,
}

struct Config {
    bind: String,
    target: Uri,
    target_authority: hyper::http::uri::Authority,
    access_log: Option<String>,
    latency_weight: f64,
    latency_high_ms: f64,
    latency_low_ms: f64,
    challenge_all: bool,
    difficulty: u32,
}

#[tokio::main]
async fn main() {
    let config = load_config();

    // If access logging is enabled: Open the log in append mode and spawn a dedicated writer task
    let log_tx = if let Some(ref log_path) = config.access_log {
        let log_file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .await
            .unwrap_or_else(|e| panic!("failed to open log file {log_path}: {e}"));
        let (tx, mut rx) = mpsc::unbounded_channel::<LogCmd>();
        let path = log_path.clone();
        tokio::spawn(async move {
            let mut writer = tokio::io::BufWriter::new(log_file);
            while let Some(msg) = rx.recv().await {
                match msg {
                    LogCmd::Append(line) => {
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
                    LogCmd::Reopen => {
                        let _ = writer.flush().await;
                        match tokio::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&path)
                            .await
                        {
                            Ok(new_file) => {
                                writer = tokio::io::BufWriter::new(new_file);
                                eprintln!("heavy: reopened log file {path}");
                            }
                            Err(e) => eprintln!("heavy: failed to reopen log file {path}: {e}"),
                        }
                    }
                }
            }
        });

        // Reopen the log file on SIGHUP. This is for logrotate compatibility; when the old log file
        // is renamed, our existing file handle continues to point to the old log. SIGHUP is how
        // logrotate tells us to start writing to a new one.
        let sighup_tx = tx.clone();
        tokio::spawn(async move {
            let mut sig = signal(SignalKind::hangup()).expect("failed to register SIGHUP handler");
            loop {
                sig.recv().await;
                let _ = sighup_tx.send(LogCmd::Reopen);
            }
        });

        Some(tx)
    } else {
        None
    };

    let listener = TcpListener::bind(&config.bind)
        .await
        .unwrap_or_else(|e| panic!("failed to bind to {}: {e}", config.bind));

    let monitor = Arc::new(LatencyMonitor::new(
        config.latency_weight,
        config.latency_high_ms,
        config.latency_low_ms,
    ));

    eprintln!(
        "heavy: listening on {}, proxying to {}",
        config.bind, config.target
    );
    if let Some(path) = &config.access_log {
        eprintln!("heavy: access logs enabled, writing to {path}")
    }
    if config.challenge_all {
        eprintln!("heavy: WARNING: challenge mode enabled for all requests");
    }

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
        let challenge_all = config.challenge_all;
        let difficulty = config.difficulty;

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
                            challenge_all,
                            difficulty,
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

    let access_log = env::var("ACCESS_LOG").ok();

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

    let challenge_all = env::var("CHALLENGE_ALL").is_ok();

    let difficulty: u32 = env::var("DIFFICULTY")
        .ok()
        .map(|v| v.parse().expect("DIFFICULTY must be a number"))
        .unwrap_or(20);

    Config {
        bind,
        target,
        target_authority,
        access_log,
        latency_weight,
        latency_high_ms,
        latency_low_ms,
        challenge_all,
        difficulty,
    }
}

/// Proxy a single request to the target, log metadata, and return the response.
async fn handle_request(
    req: Request<Incoming>,
    target_authority: hyper::http::uri::Authority,
    log_tx: Option<mpsc::UnboundedSender<LogCmd>>,
    monitor: Arc<LatencyMonitor>,
    challenge_all: bool,
    difficulty: u32,
) -> Result<Response<Either<Incoming, Full<Bytes>>>, Infallible> {
    // Intercept Heavy's own routes before anything else
    if req.uri().path().starts_with("/__heavy/") {
        return Ok(handle_heavy(&req));
    }

    // Decide whether this request needs to solve a challenge before we proxy it. Sub-resource
    // requests (images, scripts, etc) always bypass challenges, so pages don't break mid-load.
    //
    // TODO: We shouldn't rely solely on header values in the future because this makes it
    // straightforward to bypass Heavy if a scraper knows the "trick".
    let challenges_on = challenge_all || monitor.is_high_load();
    if challenges_on
        && !is_subresource_request(req.headers())
        && !cookie_values(&req, "_heavy-token").any(|v| v == "pass")
    {
        // Random nonce for now. Eventually this will be a hash of a timestamp and the client's
        // info, in order to prevent reusing PoW solutions.
        let mut nonce_bytes = [0u8; 32];
        getrandom::fill(&mut nonce_bytes).unwrap();
        let nonce: String = nonce_bytes.iter().map(|b| format!("{b:02x}")).collect();
        let html = ChallengeTemplate { nonce, difficulty }.render().unwrap();
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/html; charset=utf-8")
            .header("Cache-Control", "no-store")
            .body(Either::Right(Full::new(Bytes::from(html))))
            .unwrap());
    }

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

    // Send the log entry to the writer task (if logging is enabled)
    if let Some(ref log_tx) = log_tx {
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
        let _ = log_tx.send(LogCmd::Append(entry.to_string()));
    }

    Ok(resp_body)
}

/// Handle requests to Heavy's own `/__heavy/` namespace.
fn handle_heavy<B>(req: &Request<B>) -> Response<Either<Incoming, Full<Bytes>>> {
    match (req.method(), req.uri().path()) {
        (&hyper::Method::GET, "/__heavy/worker.js") => Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/javascript")
            .body(Either::Right(Full::new(Bytes::from(WORKER_JS))))
            .unwrap(),
        (&hyper::Method::POST, "/__heavy/pass") => {
            // Placeholder challenge verification: accept unconditionally, set the cookie, and
            // redirect back. The real PoW flow will validate a proof before setting the cookie.
            let location = req
                .headers()
                .get(header::REFERER)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("/");
            Response::builder()
                .status(StatusCode::SEE_OTHER)
                .header(
                    "Set-Cookie",
                    "_heavy-token=pass; Path=/; Max-Age=30; SameSite=Lax",
                )
                .header("Location", location)
                .body(Either::Right(Full::new(Bytes::new())))
                .unwrap()
        }
        _ => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Either::Right(Full::new(Bytes::from("404 Not Found\n"))))
            .unwrap(),
    }
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

/// Extract all values for a named cookie from the request's Cookie headers.
fn cookie_values<'a, B>(req: &'a Request<B>, name: &'a str) -> impl Iterator<Item = &'a str> {
    req.headers()
        .get_all(header::COOKIE)
        .into_iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(move |pair| {
            let (k, v) = pair.trim().split_once('=')?;
            (k == name).then_some(v)
        })
}

/// Whether the request is for a sub-resource (image, CSS, etc) as opposed to a top-level page load.
///
/// This is a heuristic based on the `Sec-Fetch-Mode` header. Older browsers (and non-browsers, such
/// as Curl) might not set it. And of course, a malicious client can always lie to us.
fn is_subresource_request(headers: &HeaderMap) -> bool {
    headers
        .get("sec-fetch-mode")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|mode| mode != "navigate")
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

    #[test]
    fn cookie_values_from_single_header() {
        let req = Request::builder()
            .header(header::COOKIE, "foo=bar; _heavy-token=pass; baz=qux")
            .body(())
            .unwrap();
        let vals: Vec<_> = cookie_values(&req, "_heavy-token").collect();
        assert_eq!(vals, vec!["pass"]);
    }

    #[test]
    fn cookie_values_from_multiple_headers() {
        let req = Request::builder()
            .header(header::COOKIE, "foo=bar")
            .header(header::COOKIE, "_heavy-token=abc")
            .body(())
            .unwrap();
        let vals: Vec<_> = cookie_values(&req, "_heavy-token").collect();
        assert_eq!(vals, vec!["abc"]);
    }

    #[test]
    fn cookie_values_missing() {
        let req = Request::builder()
            .header(header::COOKIE, "foo=bar; baz=qux; _heavy-token-extra=1")
            .body(())
            .unwrap();
        let vals: Vec<_> = cookie_values(&req, "_heavy-token").collect();
        assert!(vals.is_empty());
    }

    #[test]
    fn resource_only_on_navigate() {
        let sec_fetch_mode = |val: &str| -> HeaderMap {
            let mut result = HeaderMap::new();
            result.insert("sec-fetch-mode", val.parse().unwrap());
            result
        };
        for val in ["cors", "no-cors", "same-origin", "websocket"] {
            assert!(is_subresource_request(&sec_fetch_mode(val)));
        }
        assert!(!is_subresource_request(&sec_fetch_mode("navigate")));
    }

    #[test]
    fn assume_resource_without_fetch_mode() {
        assert!(!is_subresource_request(&HeaderMap::new()));
    }
}
