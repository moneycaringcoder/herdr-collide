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

pub const DEFAULT_GIT_TIMEOUT_SECONDS: u64 = 10;
/// A zero git timeout is not "no timeout", it is "every git call fails", which
/// degrades every workspace while looking like a working plugin. Clamped for
/// the same reason the interval is: a number nobody could have meant should not
/// silently become the plugin's behaviour.
pub const MIN_GIT_TIMEOUT_SECONDS: u64 = 1;
pub const MAX_GIT_TIMEOUT_SECONDS: u64 = 600;

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
    /// Repository-relative paths matching these globs never count as changes.
    pub ignore_globs: Vec<String>,
    /// Predict real conflicts rather than only reporting shared paths.
    pub predict_conflicts: bool,
    /// Show desktop notifications when a workspace becomes conflicting.
    /// Intrusive output is opt-in even though badge reporting is always on.
    pub notifications_enabled: bool,
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
            ignore_globs: Vec::new(),
            predict_conflicts: true,
            notifications_enabled: false,
            base_ref: DEFAULT_BASE_REF.to_string(),
            git_timeout: Duration::from_secs(DEFAULT_GIT_TIMEOUT_SECONDS),
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
        // The same emptiness rule the file path uses. `--base-ref ""` is not a
        // ref, and handing git an empty string makes every change set degrade
        // for a reason nobody can see.
        if base_ref.trim().is_empty() {
            return Err("--base-ref needs a ref name, not an empty string".into());
        }
        config.base_ref = base_ref;
    }
    // Clamped last so neither source can push the derived TTL past herdr's
    // ceiling or below its floor, and so a zero git timeout can never mean
    // "fail every git call".
    config.interval = Duration::from_secs(
        config
            .interval
            .as_secs()
            .clamp(MIN_INTERVAL_SECONDS, MAX_INTERVAL_SECONDS),
    );
    config.git_timeout = Duration::from_secs(
        config
            .git_timeout
            .as_secs()
            .clamp(MIN_GIT_TIMEOUT_SECONDS, MAX_GIT_TIMEOUT_SECONDS),
    );
    Ok(config)
}

/// The on-disk form. Every field is optional so a partial file overrides only
/// what it names, and an unknown key is applied-around rather than fatal so a
/// newer file does not break an older binary — but it is named in a warning, so
/// a typo cannot look like a setting that took effect.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct FileConfig {
    interval_seconds: Option<u64>,
    runaway_files: Option<usize>,
    runaway_lines: Option<u64>,
    ignore_suffixes: Option<Vec<String>>,
    ignore_globs: Option<Vec<String>>,
    predict_conflicts: Option<bool>,
    notifications_enabled: Option<bool>,
    base_ref: Option<String>,
    git_timeout_seconds: Option<u64>,
}

/// Every key `FileConfig` understands. Kept beside the struct because the
/// unknown-key warning is only useful while the two agree.
const KNOWN_KEYS: [&str; 9] = [
    "interval_seconds",
    "runaway_files",
    "runaway_lines",
    "ignore_suffixes",
    "ignore_globs",
    "predict_conflicts",
    "notifications_enabled",
    "base_ref",
    "git_timeout_seconds",
];

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
    // Parsed as a `Value` first, and required to be an object. serde will
    // happily build a struct out of a JSON *sequence* by position, so a file
    // containing `[1, 2, 3]` used to deserialize cleanly into
    // `interval_seconds = 1, runaway_files = 2, runaway_lines = 3` — a garbage
    // file silently reconfiguring the plugin, which is worse than a garbage
    // file being ignored.
    let value: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("collide: ignoring malformed {}: {err}", path.display());
            return Config::default();
        }
    };
    let Some(object) = value.as_object() else {
        eprintln!(
            "collide: ignoring {}: the config file must be a JSON object, found {}",
            path.display(),
            json_kind(&value)
        );
        return Config::default();
    };
    for key in object.keys() {
        if !KNOWN_KEYS.contains(&key.as_str()) {
            eprintln!(
                "collide: ignoring unknown key `{key}` in {} (known keys: {})",
                path.display(),
                KNOWN_KEYS.join(", ")
            );
        }
    }
    let file: FileConfig = match serde_json::from_value(value.clone()) {
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
    if let Some(globs) = file.ignore_globs {
        config.ignore_globs = globs;
    }
    if let Some(predict) = file.predict_conflicts {
        config.predict_conflicts = predict;
    }
    if let Some(enabled) = file.notifications_enabled {
        config.notifications_enabled = enabled;
    }
    if let Some(base_ref) = file.base_ref.filter(|r| !r.trim().is_empty()) {
        config.base_ref = base_ref;
    }
    if let Some(seconds) = file.git_timeout_seconds {
        config.git_timeout = Duration::from_secs(seconds);
    }
    config
}

/// What a JSON value is, for a message that tells the user what they wrote.
fn json_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

/// Value of `--name <VALUE>` or `--name=<VALUE>`, last occurrence winning. A
/// missing or malformed value the user typed is a hard error, unlike a
/// malformed config file: they are looking right at it and silently ignoring it
/// would be worse.
///
/// `daemon::forwarded_args` recognises the same two spellings, so an argument
/// survives being handed to the detached child.
pub fn value_arg(args: &[String], name: &str) -> Result<Option<String>> {
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

/// Where the daemon's markers live: `~/.local/state/herdr/plugins/<id>/`.
///
/// herdr injects `HERDR_PLUGIN_STATE_DIR` into the commands it spawns and is
/// authoritative when it does, but the fallback has to resolve to the *same*
/// directory. The README encourages running the binary by hand during
/// development, and a fallback that pointed somewhere else — a temp dir — gave
/// `--enable` from a plugin action and `--disable` from a shell two different
/// state dirs: the hand-run disable found no pid file, silently did nothing,
/// and left a daemon running that the user had no way to stop.
pub fn state_dir() -> PathBuf {
    non_empty_env("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            xdg_dir("XDG_STATE_HOME", ".local/state")
                .join("herdr")
                .join("plugins")
                .join(plugin_id())
        })
}

/// Where the config file lives:
/// `~/.config/herdr/plugins/config/<id>/`. Same split-brain rule as
/// [`state_dir`] — a config read by hand must be the config herdr reads.
pub fn config_dir() -> PathBuf {
    non_empty_env("HERDR_PLUGIN_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            xdg_dir("XDG_CONFIG_HOME", ".config")
                .join("herdr")
                .join("plugins")
                .join("config")
                .join(plugin_id())
        })
}

/// An XDG base directory. The variable wins when it is set to an absolute path
/// — the spec says a relative one must be ignored — otherwise `$HOME/<relative>`.
///
/// The temp path is a last resort for a process with no home directory at all
/// (an empty-environment service manager). It is the wrong place for state, but
/// it is better than writing to the working directory, which for this plugin is
/// somebody's repository.
///
/// `setup` resolves herdr's own config directory through this too, so the two
/// modules cannot disagree about what `XDG_CONFIG_HOME` means.
pub(crate) fn xdg_dir(variable: &str, relative: &str) -> PathBuf {
    if let Some(base) = non_empty_env(variable)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        return base;
    }
    match non_empty_env("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        Some(home) => home.join(relative),
        None => std::env::temp_dir().join("herdr-no-home"),
    }
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

/// Lock held across the check-and-spawn in `--enable`/`--restore`, so two of
/// them cannot both conclude that no daemon is running.
pub fn lock_file() -> PathBuf {
    state_dir().join("updater.lock")
}

/// Where the detached daemon's stderr goes. Without this every diagnostic the
/// daemon writes is lost: herdr only logs commands *it* spawned, and the daemon
/// re-execs itself, so a badge that never appears leaves nothing to read.
pub fn log_file() -> PathBuf {
    state_dir().join("updater.log")
}

/// herdr injects empty strings for absent context, so empty means unset.
pub fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}
