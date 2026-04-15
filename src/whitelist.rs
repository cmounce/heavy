use regex::RegexSet;
use serde::Deserialize;

/// A set of whitelist rules, compiled for quick querying.
///
/// This is the main interface for request handling. It's intended to be built once, at config load
/// time, and reused for each request to see if it matches any of the whitelist rules.
pub struct Whitelist {
    paths: Option<RegexSet>,
}

impl Whitelist {
    /// Build a Whitelist from the deserialized TOML data. Panics if any regex pattern is invalid.
    pub fn from_config(file: Option<FileWhitelist>) -> Whitelist {
        let paths = file
            .and_then(|w| w.path)
            .filter(|paths| !paths.is_empty())
            .map(|paths| {
                RegexSet::new(&paths)
                    .unwrap_or_else(|e| panic!("invalid whitelist path pattern: {e}"))
            });
        Whitelist { paths }
    }

    /// Whether the given request path is exempt from challenges.
    pub fn is_exempt(&self, path: &str) -> bool {
        self.paths.as_ref().is_some_and(|set| set.is_match(path))
    }
}

/// TOML representation of whitelist rules.
///
/// This is public so that the config system can use it to parse the `[whitelist]` section in
/// config.toml. In the future, it will also be used to parse any imported whitelist files.
#[derive(Deserialize, Default)]
#[serde(default)]
pub struct FileWhitelist {
    pub path: Option<Vec<String>>,
}
