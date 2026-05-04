use std::net::IpAddr;
use std::str::FromStr;

use ipnet::IpNet;
use prefix_trie::joint::JointPrefixSet;
use regex::RegexSet;
use serde::Deserialize;

/// A set of whitelist rules, compiled for quick querying.
///
/// This is the main interface for request handling. It's intended to be built once, at config load
/// time, and reused for each request to see if it matches any of the whitelist rules.
pub struct Whitelist {
    paths: RegexSet,
    ips: JointPrefixSet<IpNet>,
}

#[derive(Default)]
pub struct WhitelistParams {
    pub paths: Vec<String>,
    pub ips: Vec<String>,
}

impl Whitelist {
    pub fn new(params: &WhitelistParams) -> Self {
        let paths = RegexSet::new(&params.paths)
            .unwrap_or_else(|e| panic!("invalid whitelist path pattern: {e}"));
        let mut ips = JointPrefixSet::new();
        for s in &params.ips {
            let net =
                IpNet::from_str(s).unwrap_or_else(|e| panic!("invalid whitelist CIDR {s:?}: {e}"));
            ips.insert(net);
        }
        Whitelist { paths, ips }
    }

    /// Whether the given request matches a whitelist rule.
    pub fn is_exempt(&self, path: &str, ip: IpAddr) -> bool {
        self.paths.is_match(path) || self.ips.get_lpm(&IpNet::from(ip)).is_some()
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
    pub ip: Option<Vec<String>>,
    pub include: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn whitelist_empty() {
        let wl = Whitelist::new(&WhitelistParams::default());
        assert!(!wl.is_exempt("/", ip("127.0.0.1")));
        assert!(!wl.is_exempt("/robots.txt", ip("::1")));
    }

    #[test]
    fn whitelist_cidr_blocks() {
        let wl = Whitelist::new(&WhitelistParams {
            paths: vec![],
            ips: vec![
                "10.0.0.1/32".into(),    // single address
                "192.168.0.0/16".into(), // IPv4 range
                "fd00::/8".into(),       // IPv6 range
            ],
        });

        // Whitelisted
        assert!(wl.is_exempt("/x", ip("10.0.0.1")));
        assert!(wl.is_exempt("/x", ip("192.168.12.34")));
        assert!(wl.is_exempt("/x", ip("fd01:2345::1")));

        // Not whitelisted
        assert!(!wl.is_exempt("/x", ip("10.0.0.2")));
        assert!(!wl.is_exempt("/x", ip("192.169.0.1")));
        assert!(!wl.is_exempt("/x", ip("fe80::1")));
    }

    #[test]
    fn whitelist_paths() {
        let wl = Whitelist::new(&WhitelistParams {
            paths: vec![r"^/robots\.txt$".into(), r"^/favicon\.ico$".into()],
            ips: vec![],
        });
        let ip = ip("127.0.0.1");
        assert!(wl.is_exempt("/robots.txt", ip));
        assert!(wl.is_exempt("/favicon.ico", ip));
        assert!(!wl.is_exempt("/", ip));
        assert!(!wl.is_exempt("/robots.txt?foo=bar", ip));
    }
}
