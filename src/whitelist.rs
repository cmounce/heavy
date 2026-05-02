use regex::RegexSet;
use serde::Deserialize;

/// A set of whitelist rules, compiled for quick querying.
///
/// This is the main interface for request handling. It's intended to be built once, at config load
/// time, and reused for each request to see if it matches any of the whitelist rules.
pub struct Whitelist {
    paths: RegexSet,
}

#[derive(Default)]
pub struct WhitelistParams {
    pub paths: Vec<String>,
}

impl Whitelist {
    pub fn new(params: &WhitelistParams) -> Self {
        Whitelist {
            paths: RegexSet::new(&params.paths)
                .unwrap_or_else(|e| panic!("invalid whitelist path pattern: {e}")),
        }
    }

    /// Whether the given request path is exempt from challenges.
    pub fn is_exempt(&self, path: &str) -> bool {
        self.paths.is_match(path)
    }
}

/// TOML representation of whitelist rules.
///
/// Used both for the central config's `[whitelist]` section and for any included whitelist files.
/// This might someday move into config.rs.
#[derive(Deserialize, Default)]
#[serde(default)]
pub struct FileWhitelist {
    pub path: Option<Vec<String>>,
    pub include: Option<Vec<String>>,
}
