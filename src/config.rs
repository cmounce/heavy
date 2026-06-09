use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use hyper::Uri;
use serde::Deserialize;

use crate::breaker::CircuitBreakerConfig;
use crate::whitelist::{FileWhitelist, Whitelist, WhitelistParams};

pub struct Config {
    pub bind: String,
    pub target: Uri,
    pub target_authority: hyper::http::uri::Authority,
    pub access_log: Option<String>,
    pub socket_file: Option<String>,
    pub circuit_breaker: Arc<ArcSwap<CircuitBreakerConfig>>, // `ArcSwap` for future hot reloading
    pub challenge_all: bool,
    pub difficulty: u32,
    /// Secret string used to derive puzzle and token keys.
    pub token_secret: String,
    /// How long a token (and its cookie) remain valid, in seconds.
    pub token_lifetime: u64,
    pub whitelist: Whitelist,
}

/// TOML file representation. Every field is optional; missing keys are treated as unset.
#[derive(Deserialize, Default)]
#[serde(default)]
struct FileConfig {
    bind: Option<String>,
    target: Option<String>,
    access_log: Option<String>,
    socket_file: Option<String>,
    challenge_all: Option<bool>,
    difficulty: Option<u32>,
    token_secret: Option<String>,
    token_lifetime: Option<u64>,
    circuit_breaker: Option<FileCircuitBreaker>,
    whitelist: Option<FileWhitelist>,
}

/// TOML representation of the `[circuit_breaker]` section. All values are in seconds, except
/// `smoothing` which is the window size for 95% of the latency moving average.
#[derive(Deserialize, Default)]
#[serde(default)]
struct FileCircuitBreaker {
    trip_above: Option<f64>,
    reset_below: Option<f64>,
    smoothing: Option<f64>,
    min_open_duration: Option<f64>,
    backoff_half_life: Option<f64>,
}

/// Load configuration from the config file and environment variables.
///
/// Environment variables take precedence over TOML values, which take precedence over built-in
/// defaults. Panics on invalid values or malformed TOML (a missing config file is fine and treated
/// as empty).
pub fn load() -> Config {
    // Parse the TOML file if it exists
    let config_path: PathBuf = env::var_os("HEAVY_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/heavy/config.toml"));
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

    // Build the circuit breaker config. No env-var overrides for these keys.
    let cb_file = file.circuit_breaker.unwrap_or_default();
    let breaker_config = CircuitBreakerConfig::new(
        cb_file.trip_above.unwrap_or(0.500),
        cb_file.reset_below.unwrap_or(0.250),
        cb_file.smoothing.unwrap_or(50.0),
        cb_file.min_open_duration.unwrap_or(60.0),
        cb_file.backoff_half_life.unwrap_or(60.0),
    );
    assert!(
        breaker_config.reset_below < breaker_config.trip_above,
        "circuit_breaker.reset_below ({}s) must be less than circuit_breaker.trip_above ({}s)",
        breaker_config.reset_below,
        breaker_config.trip_above,
    );

    // Read any whitelist include files and combine them into a single WhitelistParams
    let config_dir = config_path.parent().unwrap_or(Path::new("/"));
    let whitelist_params = if let Some(file_whitelist) = file.whitelist {
        load_whitelist_params(config_dir, &file_whitelist)
    } else {
        WhitelistParams::default()
    };

    Config {
        bind: resolve!(bind, "BIND", || "0.0.0.0:8011".into()),
        target,
        target_authority,
        access_log: env::var("ACCESS_LOG").ok().or(file.access_log),
        socket_file: env::var("SOCKET_FILE").ok().or(file.socket_file),
        circuit_breaker: Arc::new(ArcSwap::new(Arc::new(breaker_config))),
        challenge_all: resolve!(challenge_all, "CHALLENGE_ALL", false),
        difficulty: resolve!(difficulty, "DIFFICULTY", 20),
        token_secret: resolve!(token_secret, "TOKEN_SECRET", || gen_secret()),
        token_lifetime: resolve!(token_lifetime, "TOKEN_LIFETIME", 60 * 60 * 24 * 7),
        whitelist: Whitelist::new(&whitelist_params),
    }
}

// Resolve the whitelist rules, following all includes from the main config's `[whitelist]` section.
fn load_whitelist_params(config_dir: &Path, base: &FileWhitelist) -> WhitelistParams {
    let mut params = WhitelistParams::default();
    if let Some(paths) = &base.paths {
        params.paths.extend_from_slice(paths);
    }
    if let Some(cidrs) = &base.cidrs {
        params.ips.extend_from_slice(cidrs);
    }
    for include in base.includes.as_deref().unwrap_or(&[]) {
        let path = if Path::new(include).is_absolute() {
            PathBuf::from(include)
        } else {
            config_dir.join(include)
        };
        let contents = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("couldn't read whitelist file {include}: {e}"));
        let parsed: FileWhitelist = toml::from_str(&contents)
            .unwrap_or_else(|e| panic!("couldn't parse whitelist file {include}: {e}"));
        if parsed.includes.is_some() {
            panic!("nested includes in {include} are not allowed");
        }
        if let Some(paths) = parsed.paths {
            params.paths.extend(paths);
        }
        if let Some(cidrs) = parsed.cidrs {
            params.ips.extend(cidrs);
        }
    }
    params
}

/// Read and parse the TOML config file at the given path.
///
/// Returns an empty FileConfig if the file doesn't exist. Panics if the file exists but can't be
/// parsed or contains invalid TOML.
fn load_file(path: &Path) -> FileConfig {
    if !path.exists() {
        return FileConfig::default();
    }
    let display = path.display();
    let contents = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read config file {display}: {e}"));
    toml::from_str(&contents)
        .unwrap_or_else(|e| panic!("failed to parse config file {display}: {e}"))
}
