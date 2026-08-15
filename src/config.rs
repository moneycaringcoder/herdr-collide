//! Configuration, plugin identity, and the state/config directories herdr
//! hands us. Owned by the integrator; the other modules read it, none of them
//! change it.

use std::path::PathBuf;
use std::time::Duration;

use crate::Result;

pub const PLUGIN_ID: &str = "moneycaringcoder.collide";

pub const DEFAULT_INTERVAL_SECONDS: u64 = 5;
pub const MIN_INTERVAL_SECONDS: u64 = 1;
/// Bounded so the derived TTL can never exceed herdr's 24h ceiling. The
/// compile-time assertion below keeps the two in step.
pub const MAX_INTERVAL_SECONDS: u64 = 3_600;

const MAX_TTL_MS: u64 = 86_400_000;
const _: () = assert!(MAX_INTERVAL_SECONDS.saturating_mul(3_000) <= MAX_TTL_MS);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub interval: Duration,
    /// Change-set size past which a workspace is flagged as a runaway agent.
    pub runaway_files: usize,
    pub runaway_lines: u64,
    /// Paths matching these suffixes never count as changes. Lockfiles and
    /// build output overlap constantly and mean nothing.
    pub ignore_suffixes: Vec<String>,
    /// Predict real conflicts rather than only reporting shared paths.
    pub predict_conflicts: bool,
    /// Timeout for any single git invocation, so one slow repo cannot stall
    /// the refresh loop.
    pub git_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(DEFAULT_INTERVAL_SECONDS),
            runaway_files: 40,
            runaway_lines: 2_000,
            ignore_suffixes: vec![
                "Cargo.lock".into(),
                "package-lock.json".into(),
                "pnpm-lock.yaml".into(),
                "yarn.lock".into(),
                "poetry.lock".into(),
                "go.sum".into(),
            ],
            predict_conflicts: true,
            git_timeout: Duration::from_secs(10),
        }
    }
}

impl Config {
    /// TTL for a badge push: three refresh cycles, so one missed cycle does not
    /// blink the badge out, clamped to herdr's ceiling.
    pub fn ttl_ms(&self) -> u64 {
        self.interval
            .as_secs()
            .saturating_mul(3_000)
            .clamp(1, MAX_TTL_MS)
    }
}

pub fn load() -> Result<Config> {
    load_with_args(&[])
}

/// Loads the config file, then applies command-line overrides.
pub fn load_with_args(_args: &[String]) -> Result<Config> {
    unimplemented!("config loading")
}

pub fn plugin_id() -> String {
    non_empty_env("HERDR_PLUGIN_ID").unwrap_or_else(|| PLUGIN_ID.to_string())
}

pub fn state_dir() -> PathBuf {
    non_empty_env("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join(plugin_id()))
}

pub fn config_dir() -> PathBuf {
    non_empty_env("HERDR_PLUGIN_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join(format!("{}-config", plugin_id())))
}

/// Marker: a daemon is live right now.
pub fn pid_file() -> PathBuf {
    state_dir().join("updater.pid")
}

/// Marker: the user asked for a daemon at some point. Survives restarts, and is
/// what `--restore` consults.
pub fn enabled_flag() -> PathBuf {
    state_dir().join("enabled")
}

/// herdr injects empty strings for absent context, so empty means unset.
pub fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}
