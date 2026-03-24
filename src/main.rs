use std::convert::Infallible;
use std::env;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use http_body_util::{Either, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderMap, HeaderName};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode, Uri};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use hyper_util::server::conn::auto::Builder as ServerBuilder;
use serde_json::{Map, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() {
    let bind = env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8011".to_string());

    let target: Uri = env::var("TARGET")
        .unwrap_or_else(|_| "http://localhost:3011".to_string())
        .parse()
        .expect("TARGET must be a valid URI (e.g. http://localhost:3011)");

    let target_authority = target
        .authority()
        .expect("TARGET must include a host (e.g. http://localhost:3011)")
        .clone();

    let log_path = env::var("LOG_FILE").unwrap_or_else(|_| "heavy.jsonl".to_string());

    // Open the log file in append mode and spawn a dedicated writer task
    let log_file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .await
        .unwrap_or_else(|e| panic!("failed to open log file {log_path}: {e}"));
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

    // Using the legacy Client for connection pooling.
    // The Incoming type parameter streams request bodies without buffering.
    let client: Client<_, Incoming> = Client::builder(TokioExecutor::new()).build_http();

    let listener = TcpListener::bind(&bind)
        .await
        .unwrap_or_else(|e| panic!("failed to bind to {bind}: {e}"));

    eprintln!("heavy: listening on {bind}, proxying to {target}, logging to {log_path}");

    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                eprintln!("heavy: failed to accept connection: {e}");
                continue;
            }
        };

        let client = client.clone();
        let target_authority = target_authority.clone();
        let log_tx = log_tx.clone();

        // tokio::spawn moves the connection onto its own async task, so the accept loop
        // immediately continues waiting for the next connection.
        tokio::spawn(async move {
            // TokioIo adapts a tokio TcpStream into the I/O traits hyper expects
            let io = hyper_util::rt::TokioIo::new(stream);
            if let Err(e) = ServerBuilder::new(TokioExecutor::new())
                .serve_connection(
                    io,
                    service_fn(|req| {
                        handle_request(
                            req,
                            client.clone(),
                            target_authority.clone(),
                            log_tx.clone(),
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
    client: Client<hyper_util::client::legacy::connect::HttpConnector, Incoming>,
    target_authority: hyper::http::uri::Authority,
    log_tx: mpsc::UnboundedSender<String>,
) -> Result<Response<Either<Incoming, Full<Bytes>>>, Infallible> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let timestamp_secs = timestamp.as_secs() as f64 + timestamp.subsec_millis() as f64 / 1000.0;

    // Rewrite URI to point at the target, preserving the original path and query
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let target_uri: Uri = Uri::builder()
        .scheme("http")
        .authority(target_authority.as_str())
        .path_and_query(path_and_query)
        .build()
        .unwrap();

    // Capture request metadata for logging before we consume the request
    let method = req.method().to_string();
    let path = path_and_query.to_string();
    let (parts, body) = req.into_parts();
    let request_headers = collect_header_map(&parts.headers);

    // Build the outgoing request with filtered headers
    let mut outgoing = Request::builder().method(parts.method).uri(target_uri);
    for (name, value) in forward_headers(&parts.headers) {
        outgoing = outgoing.header(name, value);
    }
    outgoing = outgoing.header(header::HOST, target_authority.as_str());
    let outgoing = outgoing.body(body).unwrap();

    // Send to the target and measure round-trip latency
    let start = Instant::now();
    let (status, resp_body) = match client.request(outgoing).await {
        Ok(resp) => {
            let (resp_parts, resp_body) = resp.into_parts();
            let mut response = Response::builder().status(resp_parts.status);
            for (name, value) in forward_headers(&resp_parts.headers) {
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
    let latency_ms = start.elapsed().as_millis() as u64;

    // Send the log entry to the writer task
    let entry = serde_json::json!({
        "timestamp": timestamp_secs,
        "method": method,
        "path": path,
        "headers": Value::Object(request_headers),
        "status": status,
        "latency_ms": latency_ms,
    });
    let _ = log_tx.send(entry.to_string());

    Ok(resp_body)
}

/// Collect forwarded request headers into a JSON map for logging.
fn collect_header_map(headers: &HeaderMap) -> Map<String, Value> {
    let mut map = Map::new();
    for (name, value) in forward_headers(headers) {
        if let Ok(v) = value.to_str() {
            map.insert(name.to_string(), Value::String(v.to_string()));
        }
    }
    map
}

/// Iterate over headers that are allowed to be forwarded through the proxy.
///
/// This filters out hop-by-hop headers (headers that only apply to a direct connection between two
/// clients), including those specified by the Connection header itself. See RFC 9110 or this page
/// for more info: https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Connection
fn forward_headers(
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
        | header::UPGRADE
        | _ if *name == "keep-alive" || connection_skip.contains(name) => false,
        _ => true,
    })
}
