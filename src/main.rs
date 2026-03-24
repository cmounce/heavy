use std::convert::Infallible;
use std::env;

use http_body_util::{Either, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderMap, HeaderName};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode, Uri};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use hyper_util::server::conn::auto::Builder as ServerBuilder;
use tokio::net::TcpListener;

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

    // Using the legacy `Client` here for connection pooling.
    // `Incoming` streams request bodies without buffering.
    let client: Client<_, Incoming> = Client::builder(TokioExecutor::new()).build_http();

    let listener = TcpListener::bind(&bind)
        .await
        .unwrap_or_else(|e| panic!("failed to bind to {bind}: {e}"));

    eprintln!("heavy: listening on {bind}, proxying to {target}");

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

        // tokio::spawn moves the connection onto its own async task, so the accept loop immediately
        // continues waiting for the next connection.
        tokio::spawn(async move {
            // TokioIo adapts a tokio TcpStream into the I/O traits hyper expects.
            let io = hyper_util::rt::TokioIo::new(stream);
            if let Err(e) = ServerBuilder::new(TokioExecutor::new())
                .serve_connection(
                    io,
                    service_fn(|req| handle_request(req, client.clone(), target_authority.clone())),
                )
                .await
            {
                eprintln!("heavy: connection error: {e}");
            }
        });
    }
}

/// Proxy a single request to the target and return its response.
async fn handle_request(
    req: Request<Incoming>,
    client: Client<hyper_util::client::legacy::connect::HttpConnector, Incoming>,
    target_authority: hyper::http::uri::Authority,
) -> Result<Response<Either<Incoming, Full<Bytes>>>, Infallible> {
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

    // Build the outgoing request with filtered headers
    let (parts, body) = req.into_parts();
    let mut outgoing = Request::builder().method(parts.method).uri(target_uri);
    for (name, value) in forward_headers(&parts.headers) {
        outgoing = outgoing.header(name, value);
    }
    outgoing = outgoing.header(header::HOST, target_authority.as_str());
    let outgoing = outgoing.body(body).unwrap();

    // Send to the target
    let upstream_resp = match client.request(outgoing).await {
        Ok(resp) => resp,
        Err(e) => {
            eprintln!("heavy: upstream request failed: {e}");
            return Ok(Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Either::Right(Full::new(Bytes::from("502 Bad Gateway\n"))))
                .unwrap());
        }
    };

    // Build the response back to the client, again with filtered headers.
    let (resp_parts, resp_body) = upstream_resp.into_parts();
    let mut response = Response::builder().status(resp_parts.status);
    for (name, value) in forward_headers(&resp_parts.headers) {
        response = response.header(name, value);
    }

    Ok(response.body(Either::Left(resp_body)).unwrap())
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
        | header::UPGRADE | _ if *name == "keep-alive" || connection_skip.contains(name) => false,
        _ => true,
    })
}
