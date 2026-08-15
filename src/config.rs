//! Configuration, plugin identity, and the state/config directories herdr
//! hands us. Owned by the integrator; the other modules read it, none of them
//! change it.

use std::path::PathBuf;
use std::time::Duration;

use crate::Result;

pub const PLUGIN_ID: &str = "moneycaringcoder.collide";

/// git's own default-branch pointer. herdr's snapshot carries no integration
/// ref, so this is the starting guess for every repo.
pub const DEFAULT_BASE_REF: &str = "origin/HEAD";

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
    /// Ref every checkout's change set is measured against, as the `<base>` in
    /// `diff <base>...HEAD`. `git::change_set` degrades rather than failing when
    /// it does not resolve, which is the common case for a repo with no
    /// `origin` — the workspace still reports its dirty state, only its
    /// committed-since-base paths are missing.
    pub base_ref: String,
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
            base_ref: DEFAULT_BASE_REF.to_string(),
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
pub fn load_with_args(args: &[String]) -> Result<Config> {
    let mut config = load_file();
    if let Some(seconds) = value_arg(args, "--interval")? {
        config.interval = Duration::from_secs(
            seconds
                .trim()
                .parse::<u64>()
                .map_err(|err| format!("--interval {seconds}: {err}"))?,
        );
    }
    if let Some(base_ref) = value_arg(args, "--base-ref")? {
        config.base_ref = base_ref;
    }
    // Clamped last so neither source can push the derived TTL past herdr's
    // ceiling or below its floor.
    config.interval = Duration::from_secs(
        config
            .interval
            .as_secs()
            .clamp(MIN_INTERVAL_SECONDS, MAX_INTERVAL_SECONDS),
    );
    Ok(config)
}

/// The on-disk form. Every field is optional so a partial file overrides only
/// what it names, and unknown keys are ignored so a newer file does not break
/// an older binary.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct FileConfig {
    interval_seconds: Option<u64>,
    runaway_files: Option<usize>,
    runaway_lines: Option<u64>,
    ignore_suffixes: Option<Vec<String>>,
    predict_conflicts: Option<bool>,
    base_ref: Option<String>,
    git_timeout_seconds: Option<u64>,
}

fn config_file() -> PathBuf {
    config_dir().join("config.json")
}

/// Reads the config file over the defaults. A missing file is the normal case;
/// an unreadable or malformed one is a warning and the defaults, never a hard
/// failure — a typo in a config file must not stop the badge from rendering.
fn load_file() -> Config {
    let path = config_file();
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                eprintln!("collide: ignoring {}: {err}", path.display());
            }
            return Config::default();
        }
    };
    let file: FileConfig = match serde_json::from_str(&raw) {
        Ok(file) => file,
        Err(err) => {
            eprintln!("collide: ignoring malformed {}: {err}", path.display());
            return Config::default();
        }
    };

    let mut config = Config::default();
    if let Some(seconds) = file.interval_seconds {
        config.interval = Duration::from_secs(seconds);
    }
    if let Some(files) = file.runaway_files {
        config.runaway_files = files;
    }
    if let Some(lines) = file.runaway_lines {
        config.runaway_lines = lines;
    }
    if let Some(suffixes) = file.ignore_suffixes {
        config.ignore_suffixes = suffixes;
    }
    if let Some(predict) = file.predict_conflicts {
        config.predict_conflicts = predict;
    }
    if let Some(base_ref) = file.base_ref.filter(|r| !r.trim().is_empty()) {
        config.base_ref = base_ref;
    }
    if let Some(seconds) = file.git_timeout_seconds {
        config.git_timeout = Duration::from_secs(seconds);
    }
    config
}

/// Value of `--name <VALUE>` or `--name=<VALUE>`, last occurrence winning. A
/// missing or malformed value the user typed is a hard error, unlike a
/// malformed config file: they are looking right at it and silently ignoring it
/// would be worse.
///
/// `daemon::forwarded_args` recognises the same two spellings, so an argument
/// survives being handed to the detached child.
fn value_arg(args: &[String], name: &str) -> Result<Option<String>> {
    let flag = format!("{name}=");
    let mut found = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if let Some(value) = arg.strip_prefix(&flag) {
            found = Some(value.to_string());
        } else if arg == name {
            found = Some(rest.next().ok_or(format!("{name} needs a value"))?.clone());
        }
    }
    Ok(found)
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
