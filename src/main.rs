mod access_log;
mod breaker;
mod challenge;
mod config;
mod latency;
mod whitelist;

use std::convert::Infallible;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use latency::LatencyMonitor;

use askama::Template;
use http_body_util::{BodyExt, Either, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderMap, HeaderName};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioExecutor;
use hyper_util::server::conn::auto::Builder as ServerBuilder;
use serde_json::{Map, Value};
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};

use crate::access_log::AccessLog;
use crate::whitelist::Whitelist;

const WORKER_JS: &str = include_str!("../web/worker.js");

#[derive(Template)]
#[template(path = "challenge.html")]
struct ChallengeTemplate {
    puzzle: String,
}

/// Challenge-related configuration shared across request handlers.
#[derive(Clone)]
struct ChallengeConfig {
    auth: challenge::Authenticator,
    challenge_all: bool,
    token_lifetime: u64,
    whitelist: Arc<Whitelist>,
}

#[tokio::main]
async fn main() {
    let config = config::load();

    // If access logging is enabled, open the log in append mode and spawn a dedicated writer task
    let access_log = if let Some(ref log_path) = config.access_log {
        Some(AccessLog::open(log_path).await)
    } else {
        None
    };

    // Reopen the access log on SIGHUP. This is for logrotate compatibility; when the old log file
    // is renamed, our existing file handle continues to point to the old log. SIGHUP is how
    // logrotate tells us to start writing to a new one.
    if let Some(ref access_log) = access_log {
        let access_log = access_log.clone();
        tokio::spawn(async move {
            let mut sig = signal(SignalKind::hangup()).expect("failed to register SIGHUP handler");
            loop {
                sig.recv().await;
                access_log.reopen();
            }
        });
    }

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

    let challenge_config = ChallengeConfig {
        auth: challenge::Authenticator::new(
            &config.token_secret,
            config.difficulty,
            config.token_lifetime,
        ),
        challenge_all: config.challenge_all,
        token_lifetime: config.token_lifetime,
        whitelist: Arc::new(config.whitelist),
    };

    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                eprintln!("heavy: failed to accept connection: {e}");
                continue;
            }
        };

        let target_authority = config.target_authority.clone();
        let challenge_config = challenge_config.clone();
        let access_log = access_log.clone();
        let monitor = monitor.clone();
        // IP of the directly-connected peer (usually the reverse proxy, not the end user)
        let peer_ip = addr.ip();

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
                            access_log.clone(),
                            monitor.clone(),
                            challenge_config.clone(),
                            peer_ip,
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

/// Proxy a single request to the target, log metadata, and return the response.
async fn handle_request(
    req: Request<Incoming>,
    target_authority: hyper::http::uri::Authority,
    access_log: Option<AccessLog>,
    monitor: Arc<LatencyMonitor>,
    cc: ChallengeConfig,
    peer_ip: IpAddr,
) -> Result<Response<Either<Incoming, Full<Bytes>>>, Infallible> {
    let client_ip = client_ip(req.headers(), peer_ip);
    let user_agent = req
        .headers()
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // Intercept Heavy's own routes before anything else
    if req.uri().path().starts_with("/__heavy/") {
        return Ok(handle_heavy(req, &cc, client_ip, &user_agent).await);
    }

    // Decide whether this request needs to solve a challenge before we proxy it. Sub-resource
    // requests (images, scripts, etc) always bypass challenges, so pages don't break mid-load.
    //
    // TODO: We shouldn't rely solely on header values in the future because this makes it
    // straightforward to bypass Heavy if a scraper knows the "trick".
    let challenges_on = cc.challenge_all || monitor.is_high_load();
    if challenges_on
        && !cc.whitelist.is_exempt(req.uri().path())
        && !is_subresource_request(req.headers())
        && !cookie_values(&req, "_heavy-token")
            .any(|v| cc.auth.verify_token(client_ip, &user_agent, v))
    {
        let puzzle = cc.auth.make_puzzle(client_ip, &user_agent);
        let html = ChallengeTemplate { puzzle }.render().unwrap();
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
    if let Some(ref access_log) = access_log {
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
        access_log.append(entry.to_string());
    }

    Ok(resp_body)
}

/// Handle requests to Heavy's own `/__heavy/` namespace.
async fn handle_heavy(
    req: Request<Incoming>,
    cc: &ChallengeConfig,
    client_ip: IpAddr,
    user_agent: &str,
) -> Response<Either<Incoming, Full<Bytes>>> {
    match (req.method().clone(), req.uri().path()) {
        (hyper::Method::GET, "/__heavy/worker.js") => Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/javascript")
            .body(Either::Right(Full::new(Bytes::from(WORKER_JS))))
            .unwrap(),
        (hyper::Method::POST, "/__heavy/submit") => {
            verify_and_issue_token(req, cc, client_ip, user_agent).await
        }
        _ => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Either::Right(Full::new(Bytes::from("404 Not Found\n"))))
            .unwrap(),
    }
}

/// Verify a proof-of-work solution and mint a token cookie on success.
async fn verify_and_issue_token(
    req: Request<Incoming>,
    cc: &ChallengeConfig,
    client_ip: IpAddr,
    user_agent: &str,
) -> Response<Either<Incoming, Full<Bytes>>> {
    let bad_request = || {
        Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Either::Right(Full::new(Bytes::from("400 Bad Request\n"))))
            .unwrap()
    };

    let body = match req.collect().await {
        Ok(b) => b.to_bytes(),
        Err(_) => return bad_request(),
    };

    let Some(token) = cc.auth.redeem_solution(client_ip, user_agent, &body) else {
        return bad_request();
    };

    let cookie = format!(
        "_heavy-token={token}; Path=/; Max-Age={}; SameSite=Lax",
        cc.token_lifetime
    );
    Response::builder()
        .status(StatusCode::OK)
        .header("Set-Cookie", cookie)
        .body(Either::Right(Full::new(Bytes::new())))
        .unwrap()
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

/// Extract the client IP from request headers, checking standard proxy headers.
///
/// Checks X-Real-IP, X-Forwarded-For (leftmost entry), and the Forwarded header (first `for=`
/// directive) in that order, falling back to the peer socket address.
///
/// TODO: Think about how we want to configure and secure IP extraction. Concerns include:
/// - A malicious client can forge proxy headers if the reverse proxy doesn't strip/overwrite them
/// - Silently falling back to peer_ip when headers are missing could mask a misconfigured proxy
///   (e.g., everything looks like localhost), and the admin should find out so they can fix it
/// - We may want to let the admin choose which header(s) to trust
fn client_ip(headers: &HeaderMap, peer_ip: IpAddr) -> IpAddr {
    // X-Real-IP: a single IP set by the reverse proxy
    if let Some(ip) = headers
        .get("X-Real-IP")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse().ok())
    {
        return ip;
    }

    // X-Forwarded-For: comma-separated list, leftmost is the original client
    if let Some(ip) = headers
        .get("X-Forwarded-For")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .and_then(|s| s.trim().parse().ok())
    {
        return ip;
    }

    // Forwarded: RFC 7239, look for the first `for=` directive
    if let Some(ip) = headers
        .get("Forwarded")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_forwarded_for)
    {
        return ip;
    }

    peer_ip
}

/// Parse the first `for=` directive from an RFC 7239 Forwarded header value.
fn parse_forwarded_for(header: &str) -> Option<IpAddr> {
    for param in header.split([',', ';']) {
        let param = param.trim();
        let (key, value) = param.split_once('=')?;
        if !key.eq_ignore_ascii_case("for") {
            continue;
        }
        let value = value.trim_matches('"');
        // Direct parse (bare IPv4 or IPv6)
        if let Ok(ip) = value.parse() {
            return Some(ip);
        }
        // Bracketed IPv6 with optional port: [2001:db8::1]:8080
        if let Some(rest) = value.strip_prefix('[') {
            if let Some((addr, _)) = rest.split_once(']') {
                return addr.parse().ok();
            }
        }
        // IPv4 with port: 192.0.2.1:8080
        if let Some((addr, _)) = value.rsplit_once(':') {
            return addr.parse().ok();
        }
    }
    None
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

    const LOCALHOST: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1));

    #[test]
    fn client_ip_from_x_real_ip() {
        let mut headers = HeaderMap::new();
        headers.insert("X-Real-IP", "10.0.0.1".parse().unwrap());
        assert_eq!(client_ip(&headers, LOCALHOST).to_string(), "10.0.0.1");
    }

    #[test]
    fn client_ip_from_x_forwarded_for() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "X-Forwarded-For",
            "10.0.0.1, 12.34.56.78, 98.76.54.32".parse().unwrap(),
        );
        assert_eq!(client_ip(&headers, LOCALHOST).to_string(), "10.0.0.1");
    }

    #[test]
    fn client_ip_from_forwarded() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "Forwarded",
            "for=10.0.0.1;proto=http;by=12.34.56.78".parse().unwrap(),
        );
        assert_eq!(client_ip(&headers, LOCALHOST).to_string(), "10.0.0.1");
    }

    #[test]
    fn client_ip_forwarded_ipv6_bracketed() {
        let mut headers = HeaderMap::new();
        headers.insert("Forwarded", "for=\"[fd00::1]\"".parse().unwrap());
        assert_eq!(client_ip(&headers, LOCALHOST).to_string(), "fd00::1");
    }

    #[test]
    fn client_ip_falls_back_to_peer() {
        let peer: IpAddr = "10.0.0.1".parse().unwrap();
        assert_eq!(client_ip(&HeaderMap::new(), peer), peer);
    }
}
