use std::env;
use std::path::Path;

use hyper::Uri;
use serde::Deserialize;

pub struct Config {
    pub bind: String,
    pub target: Uri,
    pub target_authority: hyper::http::uri::Authority,
    pub access_log: Option<String>,
    pub latency_weight: f64,
    pub latency_high_ms: f64,
    pub latency_low_ms: f64,
    pub challenge_all: bool,
    pub difficulty: u32,
    /// Secret string used to derive puzzle and token keys.
    pub token_secret: String,
    /// How long a token (and its cookie) remain valid, in seconds.
    pub token_lifetime: u64,
}

/// TOML file representation. Every field is optional; missing keys are treated as unset.
#[derive(Deserialize, Default)]
#[serde(default)]
struct FileConfig {
    bind: Option<String>,
    target: Option<String>,
    access_log: Option<String>,
    latency_weight: Option<f64>,
    latency_high_ms: Option<f64>,
    latency_low_ms: Option<f64>,
    challenge_all: Option<bool>,
    difficulty: Option<u32>,
    token_secret: Option<String>,
    token_lifetime: Option<u64>,
}

/// Load configuration from the config file and environment variables.
///
/// Environment variables take precedence over TOML values, which take precedence over built-in
/// defaults. Panics on invalid values or malformed TOML (a missing config file is fine and treated
/// as empty).
pub fn load() -> Config {
    // Parse the TOML file if it exists
    let config_path = env::var("HEAVY_CONFIG").unwrap_or_else(|_| "/etc/heavy/config.toml".into());
    let file = load_file(&config_path);

    // Helper: Resolve a config field with env var > TOML value > default
    macro_rules! resolve {
        ($field:ident, $env:literal, || $default:expr) => {{
            env::var($env)
                .ok()
                .map(|v| {
                    v.parse()
                        .unwrap_or_else(|_| panic!("invalid value for {}: {}", $env, v))
                })
                .or(file.$field)
                .unwrap_or_else(|| $default)
        }};
        ($field:ident, $env:literal, $default:expr) => {{ resolve!($field, $env, || $default) }};
    }

    // TODO: Remove `target_authority` from the config and just have `target`? We have to process
    // these fields separately from the rest of them because we're splitting one field into two.
    let default_target = "http://localhost:3011";
    let target_str = resolve!(target, "TARGET", || default_target.into());
    let (target, target_authority) = target_str
        .parse::<Uri>()
        .ok()
        .and_then(|uri| uri.authority().cloned().map(|auth| (uri, auth)))
        .unwrap_or_else(|| panic!("target must be a valid URI with host (e.g. {default_target})"));

    // Helper: Generate a random secret string for when one isn't configured
    let gen_secret = || {
        eprintln!(
            "heavy: no token_secret configured, using random key (tokens won't survive restarts)"
        );
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).unwrap();
        hex::encode(bytes)
    };

    // Resolve and check remaining config fields
    let result = Config {
        bind: resolve!(bind, "BIND", || "0.0.0.0:8011".into()),
        target,
        target_authority,
        access_log: env::var("ACCESS_LOG").ok().or(file.access_log),
        latency_weight: resolve!(latency_weight, "LATENCY_WEIGHT", 0.01),
        latency_high_ms: resolve!(latency_high_ms, "LATENCY_HIGH_MS", 500.0),
        latency_low_ms: resolve!(latency_low_ms, "LATENCY_LOW_MS", 250.0),
        challenge_all: resolve!(challenge_all, "CHALLENGE_ALL", false),
        difficulty: resolve!(difficulty, "DIFFICULTY", 20),
        token_secret: resolve!(token_secret, "TOKEN_SECRET", || gen_secret()),
        token_lifetime: resolve!(token_lifetime, "TOKEN_LIFETIME", 60 * 60 * 24 * 7),
    };

    assert!(
        result.latency_low_ms < result.latency_high_ms,
        "latency_low_ms ({}) must be less than latency_high_ms ({})",
        result.latency_low_ms,
        result.latency_high_ms,
    );

    result
}

/// Read and parse the TOML config file at the given path.
///
/// Returns an empty FileConfig if the file doesn't exist. Panics if the file exists but can't be
/// parsed or contains invalid TOML.
fn load_file(path: &str) -> FileConfig {
    if !Path::new(path).exists() {
        return FileConfig::default();
    }
    let contents = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read config file {path}: {e}"));
    toml::from_str(&contents).unwrap_or_else(|e| panic!("failed to parse config file {path}: {e}"))
}
