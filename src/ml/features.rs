use hyper::header;
use hyper::{Method, Request};

pub const FEATURE_COUNT: usize = 3;

/// A fixed-length vector of numeric features for a single request.
///
/// This represents info that can be gleaned from an incoming request before any work is done to
/// serve it; stuff like latency or HTTP response code is out of scope. This is so we can use the
/// features for making serve vs. challenge classifications.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestFeatures([i32; FEATURE_COUNT]);

impl RequestFeatures {
    /// Extracts the feature vector from a request.
    pub fn from_request<B>(req: &Request<B>) -> Self {
        // HTTP method to enum value
        let verb = HttpVerb::from(req.method()) as i32;

        // Lengths are counted in bytes: header values and the path are already
        // plain byte slices, so this avoids having to count UTF-8 characters.
        let user_agent_len = req
            .headers()
            .get(header::USER_AGENT)
            .map_or(0, |v| v.as_bytes().len()) as i32;
        let path_len = req.uri().path().len() as i32;

        RequestFeatures([verb, user_agent_len, path_len])
    }
}

impl Default for RequestFeatures {
    fn default() -> Self {
        RequestFeatures([0; FEATURE_COUNT])
    }
}

/// Enum for mapping HTTP methods to feature values.
///
/// In an attempt to encode some meaning in the order of these values, we assign them
/// chronologically with alphabetical order as the tiebreaker. This has the advantage of making the
/// feature an approximate measure of rarity: larger numbers tend to be uncommon requests, whereas
/// the most common method (GET) is assigned the minimum value of 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum HttpVerb {
    // The original method from HTTP/0.9 (1991-ish)
    Get = 0,

    // RFC 1945 (1996)
    Head,
    Post,

    // RFC 2068 (1997)
    Connect,
    Delete,
    Options,
    Put,
    Trace,

    // RFC 5789 (2010)
    Patch,

    // Catchall for nonstandard methods (or potential new ones)
    Unknown,
}

impl From<&Method> for HttpVerb {
    fn from(method: &Method) -> Self {
        match method.as_str() {
            "CONNECT" => HttpVerb::Connect,
            "DELETE" => HttpVerb::Delete,
            "GET" => HttpVerb::Get,
            "HEAD" => HttpVerb::Head,
            "OPTIONS" => HttpVerb::Options,
            "PATCH" => HttpVerb::Patch,
            "POST" => HttpVerb::Post,
            "PUT" => HttpVerb::Put,
            "TRACE" => HttpVerb::Trace,
            _ => HttpVerb::Unknown,
        }
    }
}
