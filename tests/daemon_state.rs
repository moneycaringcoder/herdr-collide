//! Marker-file and config tests for the badge updater.
//!
//! These run against a temp state dir, never the user's real one, and never
//! spawn a daemon: every case either short-circuits before the spawn or records
//! a pid that is already live (our own test process).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use collide::config::{self, Config, DEFAULT_BASE_REF, MAX_INTERVAL_SECONDS, MIN_INTERVAL_SECONDS};
use collide::daemon::{self, BadgeOp};
use collide::model::{Severity, WorkspaceStatus};

fn owned(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| arg.to_string()).collect()
}

fn status(
    workspace_id: &str,
    severity: Severity,
    conflicts: usize,
    overlaps: usize,
) -> WorkspaceStatus {
    WorkspaceStatus {
        workspace_id: workspace_id.to_string(),
        severity,
        overlap_count: overlaps,
        conflict_count: conflicts,
        runaway: severity == Severity::Runaway,
        lines_changed: 0,
    }
}

fn lit(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(id, token)| (id.to_string(), token.to_string()))
        .collect()
}

/// The state and config dirs come from process-global env vars, so these tests
/// have to run one at a time.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct TempDirs {
    root: PathBuf,
}

impl TempDirs {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "collide-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("state")).expect("state dir");
        std::fs::create_dir_all(root.join("config")).expect("config dir");
        std::env::set_var("HERDR_PLUGIN_STATE_DIR", root.join("state"));
        std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", root.join("config"));
        Self { root }
    }

    fn config_file(&self) -> PathBuf {
        self.root.join("config").join("config.json")
    }
}

impl Drop for TempDirs {
    fn drop(&mut self) {
        std::env::remove_var("HERDR_PLUGIN_STATE_DIR");
        std::env::remove_var("HERDR_PLUGIN_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Saves and restores process-global variables, so a test that rewrites `HOME`
/// cannot leak that into the next one.
struct EnvGuard {
    saved: Vec<(String, Option<String>)>,
}

impl EnvGuard {
    fn new(variables: &[&str]) -> Self {
        let saved = variables
            .iter()
            .map(|name| (name.to_string(), std::env::var(name).ok()))
            .collect();
        for name in variables {
            std::env::remove_var(name);
        }
        Self { saved }
    }

    fn set(&self, name: &str, value: &str) {
        std::env::set_var(name, value);
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.saved {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

/// The four variables that decide where state and config land.
fn dir_env() -> EnvGuard {
    EnvGuard::new(&[
        "HERDR_PLUGIN_STATE_DIR",
        "HERDR_PLUGIN_CONFIG_DIR",
        "XDG_STATE_HOME",
        "XDG_CONFIG_HOME",
        "HOME",
        "HERDR_PLUGIN_ID",
    ])
}

fn write_pid_file(contents: &str) {
    std::fs::write(config::pid_file(), contents).expect("write pid file");
}

fn exists(path: &Path) -> bool {
    path.exists()
}

/// A pid that is guaranteed dead: a child we have already reaped. Immediate
/// reuse by another process is vanishingly unlikely within one test.
fn reaped_pid() -> u32 {
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg("exit 0")
        .spawn()
        .expect("spawn");
    let pid = child.id();
    child.wait().expect("wait");
    pid
}

#[test]
fn enabled_flag_round_trips() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("enabled");

    assert!(
        !daemon::is_enabled(),
        "a fresh state dir means never enabled"
    );

    daemon::mark_enabled(true);
    assert!(daemon::is_enabled());
    assert!(exists(&config::enabled_flag()));

    daemon::mark_enabled(false);
    assert!(!daemon::is_enabled());
    assert!(!exists(&config::enabled_flag()));

    // Disabling twice is a no-op, not an error: the marker is already gone.
    daemon::mark_enabled(false);
    assert!(!daemon::is_enabled());
}

#[test]
fn no_pid_file_means_no_daemon() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("nopid");

    assert_eq!(daemon::live_pid(), None);
}

#[test]
fn a_stale_pid_file_is_not_a_live_daemon() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("stale");

    write_pid_file(&reaped_pid().to_string());

    assert_eq!(daemon::live_pid(), None, "the recorded process is gone");
    assert!(
        !exists(&config::pid_file()),
        "a stale marker is swept, so the next --enable can spawn"
    );
}

#[test]
fn a_malformed_pid_file_is_not_a_live_daemon() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("garbage");

    for contents in ["", "   ", "not-a-pid", "0", "-1"] {
        write_pid_file(contents);
        assert_eq!(daemon::live_pid(), None, "pid file contents {contents:?}");
    }
}

#[test]
fn our_own_live_pid_counts_as_a_daemon() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("live");

    let pid = std::process::id();
    daemon::write_pid(pid);

    assert_eq!(daemon::live_pid(), Some(pid as i32));
    assert_eq!(daemon::read_pid(), Some(pid as i32));

    daemon::clear_pid_file();
    assert_eq!(daemon::live_pid(), None);
}

/// The state dir outlives reboots, so a recorded pid can be alive and belong to
/// something else entirely. pid 1 is always alive and is never us.
#[cfg(target_os = "linux")]
#[test]
fn a_reused_pid_belonging_to_another_program_is_not_a_daemon() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("reuse");

    write_pid_file("1");

    assert_eq!(
        daemon::live_pid(),
        None,
        "/proc/1/comm is not our binary, so this pid was reused"
    );
}

#[test]
fn restore_is_a_no_op_when_never_enabled() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("restore-off");

    daemon::restore().expect("restore must stay silent, not fail");

    assert!(
        !exists(&config::pid_file()),
        "restore must not spawn a daemon the user never asked for"
    );
}

#[test]
fn restore_is_a_no_op_when_a_daemon_is_already_live() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("restore-live");

    daemon::mark_enabled(true);
    // Our own pid stands in for a live daemon, so restore has nothing to do.
    daemon::write_pid(std::process::id());

    daemon::restore().expect("restore");

    assert_eq!(daemon::read_pid(), Some(std::process::id() as i32));
    daemon::clear_pid_file();
}

#[test]
fn ttl_is_three_refresh_cycles_and_stays_inside_herdrs_range() {
    let with_interval = |seconds: u64| Config {
        interval: Duration::from_secs(seconds),
        ..Config::default()
    };

    assert_eq!(
        with_interval(5).ttl_ms(),
        15_000,
        "three cycles, so one miss is fine"
    );
    assert_eq!(with_interval(MIN_INTERVAL_SECONDS).ttl_ms(), 3_000);
    assert_eq!(
        with_interval(MAX_INTERVAL_SECONDS).ttl_ms(),
        MAX_INTERVAL_SECONDS * 3_000
    );
    assert!(
        with_interval(MAX_INTERVAL_SECONDS).ttl_ms() <= 86_400_000,
        "herdr's ceiling"
    );
    // A zero interval would derive a zero TTL, which herdr rejects.
    assert_eq!(with_interval(0).ttl_ms(), 1);
}

#[test]
fn interval_argument_overrides_the_default_and_is_clamped() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("interval");

    let args = |args: &[&str]| -> Config { config::load_with_args(&owned(args)).expect("load") };

    assert_eq!(args(&[]).interval, Config::default().interval);
    assert_eq!(
        args(&["--daemon", "--interval", "12"]).interval,
        Duration::from_secs(12)
    );
    assert_eq!(args(&["--interval=9"]).interval, Duration::from_secs(9));
    assert_eq!(
        args(&["--interval", "0"]).interval,
        Duration::from_secs(MIN_INTERVAL_SECONDS)
    );
    assert_eq!(
        args(&["--interval", "999999"]).interval,
        Duration::from_secs(MAX_INTERVAL_SECONDS),
        "clamped so the derived TTL cannot exceed herdr's ceiling"
    );

    // A value the user typed themselves fails loudly, unlike a config file.
    assert!(config::load_with_args(&["--interval".to_string()]).is_err());
    assert!(config::load_with_args(&["--interval".to_string(), "soon".to_string()]).is_err());
}

#[test]
fn the_injected_env_var_wins_over_every_fallback() {
    let _guard = env_lock();
    let env = dir_env();
    // herdr is authoritative for the commands it spawns.
    env.set("HOME", "/home/ignored");
    env.set("XDG_STATE_HOME", "/xdg/ignored");
    env.set("HERDR_PLUGIN_STATE_DIR", "/injected/state");
    env.set("HERDR_PLUGIN_CONFIG_DIR", "/injected/config");

    assert_eq!(config::state_dir(), PathBuf::from("/injected/state"));
    assert_eq!(config::config_dir(), PathBuf::from("/injected/config"));
    assert_eq!(
        config::pid_file(),
        PathBuf::from("/injected/state/updater.pid")
    );
}

#[test]
fn the_xdg_variables_are_honoured_when_herdr_injects_nothing() {
    let _guard = env_lock();
    let env = dir_env();
    env.set("HOME", "/home/test");
    env.set("XDG_STATE_HOME", "/state-root");
    env.set("XDG_CONFIG_HOME", "/config-root");

    let id = config::plugin_id();
    assert_eq!(
        config::state_dir(),
        PathBuf::from(format!("/state-root/herdr/plugins/{id}"))
    );
    assert_eq!(
        config::config_dir(),
        PathBuf::from(format!("/config-root/herdr/plugins/config/{id}"))
    );

    // The spec says a relative XDG path must be ignored, and honouring one here
    // would put the state dir inside somebody's repository.
    env.set("XDG_STATE_HOME", "relative/state");
    assert_eq!(
        config::state_dir(),
        PathBuf::from(format!("/home/test/.local/state/herdr/plugins/{id}"))
    );
}

#[test]
fn the_default_is_the_herdr_path_and_never_a_temp_dir() {
    let _guard = env_lock();
    let env = dir_env();
    env.set("HOME", "/home/test");

    let id = config::plugin_id();
    assert_eq!(
        config::state_dir(),
        PathBuf::from(format!("/home/test/.local/state/herdr/plugins/{id}"))
    );
    assert_eq!(
        config::config_dir(),
        PathBuf::from(format!("/home/test/.config/herdr/plugins/config/{id}"))
    );
    // The temp-dir fallback is what split `--enable` from `--disable`.
    let temp = std::env::temp_dir();
    assert!(
        !config::state_dir().starts_with(&temp),
        "state dir in {temp:?}"
    );
    assert!(
        !config::config_dir().starts_with(&temp),
        "config dir in {temp:?}"
    );

    // The plugin id is part of the path, so two plugins never share a state dir.
    env.set("HERDR_PLUGIN_ID", "someone.else");
    assert_eq!(
        config::state_dir(),
        PathBuf::from("/home/test/.local/state/herdr/plugins/someone.else")
    );
}

/// The invariant that actually broke: the directory the binary computes for
/// itself must be the directory herdr injects. When they disagree, `--enable`
/// through a plugin action and `--disable` from a shell address different
/// daemons, and the one that is running cannot be stopped.
#[test]
fn hand_invocation_and_herdr_invocation_resolve_to_one_directory() {
    let _guard = env_lock();
    let env = dir_env();
    env.set("HOME", "/home/test");

    let id = config::plugin_id();
    // Run by hand: nothing injected.
    let by_hand_state = config::state_dir();
    let by_hand_config = config::config_dir();

    // Run by herdr: it injects the paths it actually uses on this machine.
    env.set(
        "HERDR_PLUGIN_STATE_DIR",
        &format!("/home/test/.local/state/herdr/plugins/{id}"),
    );
    env.set(
        "HERDR_PLUGIN_CONFIG_DIR",
        &format!("/home/test/.config/herdr/plugins/config/{id}"),
    );

    assert_eq!(by_hand_state, config::state_dir());
    assert_eq!(by_hand_config, config::config_dir());
}

#[test]
fn a_process_with_no_home_still_gets_a_usable_state_dir() {
    let _guard = env_lock();
    let _env = dir_env();
    // No HOME, no XDG: writing to the working directory would mean writing into
    // the user's repository, so a temp path is the least bad answer left.
    let state = config::state_dir();

    assert!(state.is_absolute());
    assert!(state.starts_with(std::env::temp_dir()));
    assert!(state.ends_with(config::plugin_id()));
}

#[test]
fn a_severity_flip_clears_the_old_token_before_setting_the_new_one() {
    // Tokens are a merge patch, so a name we do not mention stays lit. Without
    // the clear, herdr renders two badges for one workspace.
    let plan = daemon::badge_plan(
        &lit(&[("w6", "collide_overlap")]),
        &[status("w6", Severity::Conflict, 2, 0)],
    );

    match plan.as_slice() {
        [BadgeOp::Clear {
            workspace_id,
            token,
        }, BadgeOp::Set {
            workspace_id: set_id,
            token: set_token,
            text,
        }] => {
            assert_eq!(workspace_id, "w6");
            assert_eq!(token, "collide_overlap");
            assert_eq!(set_id, "w6");
            assert_eq!(*set_token, "collide_conflict");
            // Text is render::badge's to author; the daemon only carries it.
            assert_eq!(text, "\u{2718} 2");
        }
        other => panic!("expected clear-then-set, got {other:?}"),
    }
}

#[test]
fn an_unchanged_severity_is_re_set_so_the_ttl_never_lapses() {
    let plan = daemon::badge_plan(
        &lit(&[("w6", "collide_conflict")]),
        &[status("w6", Severity::Conflict, 2, 0)],
    );

    assert_eq!(
        plan,
        vec![BadgeOp::Set {
            workspace_id: "w6".to_string(),
            token: "collide_conflict",
            text: "\u{2718} 2".to_string(),
        }],
        "no redundant clear, but the write still refreshes the TTL"
    );
}

#[test]
fn a_clean_workspace_is_cleared_rather_than_given_an_empty_badge() {
    // render::badge renders clean as the empty string, and an empty token value
    // would occupy the sidebar row with nothing at all.
    let plan = daemon::badge_plan(
        &lit(&[("w6", "collide_conflict")]),
        &[status("w6", Severity::Clean, 0, 0)],
    );

    assert_eq!(
        plan,
        vec![BadgeOp::Clear {
            workspace_id: "w6".to_string(),
            token: "collide_conflict".to_string(),
        }]
    );

    // And a workspace that was never lit costs no calls at all.
    assert!(daemon::badge_plan(&HashMap::new(), &[status("w6", Severity::Clean, 0, 0)]).is_empty());
}

#[test]
fn a_workspace_that_left_the_report_is_cleared() {
    let plan = daemon::badge_plan(
        &lit(&[("w6", "collide_conflict"), ("w7", "collide_overlap")]),
        &[status("w6", Severity::Conflict, 1, 0)],
    );

    assert!(
        plan.contains(&BadgeOp::Clear {
            workspace_id: "w7".to_string(),
            token: "collide_overlap".to_string(),
        }),
        "a closed workspace must not keep its badge until the TTL expires: {plan:?}"
    );
    // w6 is still reported, so it is refreshed rather than cleared.
    assert!(!plan.contains(&BadgeOp::Clear {
        workspace_id: "w6".to_string(),
        token: "collide_conflict".to_string(),
    }));
}

#[test]
fn one_workspaces_severity_does_not_disturb_another() {
    let plan = daemon::badge_plan(
        &lit(&[("w6", "collide_overlap")]),
        &[
            status("w6", Severity::Conflict, 1, 0),
            status("w7", Severity::Overlap, 0, 3),
            status("w8", Severity::Clean, 0, 0),
        ],
    );

    let sets: Vec<&BadgeOp> = plan
        .iter()
        .filter(|op| matches!(op, BadgeOp::Set { .. }))
        .collect();
    assert_eq!(sets.len(), 2, "clean draws nothing, the other two draw");
    assert!(!plan.iter().any(|op| matches!(
        op,
        BadgeOp::Clear { workspace_id, .. } if workspace_id == "w7" || workspace_id == "w8"
    )));
}

#[test]
fn only_notes_that_are_new_since_the_last_cycle_are_reported() {
    // A note repeats every cycle for as long as its cause lasts, and at a 5s
    // interval that is a wall of identical lines.
    let previous = owned(&["w1: not a repo", "w2: timed out"]);
    let current = owned(&["w1: not a repo", "w3: prediction failed"]);

    assert_eq!(
        daemon::new_notes(&previous, &current),
        owned(&["w3: prediction failed"])
    );
    assert!(daemon::new_notes(&current, &current).is_empty());
    assert_eq!(daemon::new_notes(&[], &current), current);
}

#[test]
fn base_ref_precedence_is_default_then_file_then_command_line() {
    let _guard = env_lock();
    let dirs = TempDirs::new("baseref");

    // Default: git's own default-branch pointer.
    let defaulted = config::load_with_args(&[]).expect("load");
    assert_eq!(defaulted.base_ref, DEFAULT_BASE_REF);
    assert_eq!(defaulted.base_ref, "origin/HEAD");

    // The file beats the default.
    std::fs::write(dirs.config_file(), r#"{"base_ref": "upstream/main"}"#).expect("write config");
    assert_eq!(
        config::load_with_args(&[]).expect("load").base_ref,
        "upstream/main"
    );

    // The command line beats the file, in either spelling.
    assert_eq!(
        config::load_with_args(&owned(&["--daemon", "--base-ref", "main"]))
            .expect("load")
            .base_ref,
        "main"
    );
    assert_eq!(
        config::load_with_args(&owned(&["--base-ref=release/2.0"]))
            .expect("load")
            .base_ref,
        "release/2.0"
    );

    // An empty value in the file is not a ref; fall back rather than handing
    // git an empty string.
    std::fs::write(dirs.config_file(), r#"{"base_ref": "  "}"#).expect("write config");
    assert_eq!(
        config::load_with_args(&[]).expect("load").base_ref,
        DEFAULT_BASE_REF
    );

    // A flag with no value is the user's typo, so it is fatal.
    assert!(config::load_with_args(&owned(&["--base-ref"])).is_err());
}

#[test]
fn recognised_arguments_are_forwarded_to_the_detached_child() {
    // Both spellings normalise to `--name value`, and the verb itself and any
    // flag the daemon does not read are dropped.
    assert_eq!(
        daemon::forwarded_args(&owned(&["--enable", "--interval", "30"])).expect("forward"),
        owned(&["--interval", "30"])
    );
    assert_eq!(
        daemon::forwarded_args(&owned(&["--toggle", "--interval=30", "--base-ref=main"]))
            .expect("forward"),
        owned(&["--interval", "30", "--base-ref", "main"])
    );
    assert_eq!(
        daemon::forwarded_args(&owned(&["--enable"])).expect("forward"),
        Vec::<String>::new()
    );
    assert_eq!(
        daemon::forwarded_args(&owned(&["--enable", "--quiet"])).expect("forward"),
        Vec::<String>::new(),
        "an unrecognised flag must not reach the child"
    );
    // A value that happens to start with the flag name is still a value.
    assert_eq!(
        daemon::forwarded_args(&owned(&["--base-ref", "--interval"])).expect("forward"),
        owned(&["--base-ref", "--interval"])
    );

    assert!(daemon::forwarded_args(&owned(&["--enable", "--interval"])).is_err());
}

#[test]
fn enable_rejects_a_bad_value_before_changing_any_state() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("enable-bad");

    let err = daemon::enable(&owned(&["--enable", "--interval", "soon"]))
        .expect_err("a typo'd interval must be fatal");

    assert!(err.to_string().contains("--interval"));
    assert!(
        !daemon::is_enabled(),
        "nothing is marked until the arguments parse"
    );
    assert!(
        !exists(&config::pid_file()),
        "and nothing is spawned either"
    );
}

#[test]
fn a_config_file_overrides_only_the_fields_it_names() {
    let _guard = env_lock();
    let dirs = TempDirs::new("cfgfile");

    std::fs::write(
        dirs.config_file(),
        r#"{"interval_seconds": 30, "runaway_files": 7, "ignore_suffixes": [".snap"]}"#,
    )
    .expect("write config");

    let config = config::load_with_args(&[]).expect("load");

    assert_eq!(config.interval, Duration::from_secs(30));
    assert_eq!(config.runaway_files, 7);
    assert_eq!(config.ignore_suffixes, vec![".snap".to_string()]);
    assert_eq!(config.runaway_lines, Config::default().runaway_lines);
    assert_eq!(
        config.predict_conflicts,
        Config::default().predict_conflicts
    );

    // The command line still wins over the file.
    let overridden =
        config::load_with_args(&["--interval".to_string(), "3".to_string()]).expect("load");
    assert_eq!(overridden.interval, Duration::from_secs(3));
}

#[test]
fn a_malformed_config_file_warns_and_falls_back_to_defaults() {
    let _guard = env_lock();
    let dirs = TempDirs::new("cfgbad");

    std::fs::write(dirs.config_file(), "{ this is not json").expect("write config");

    let config = config::load_with_args(&[]).expect("a bad config file is never fatal");

    assert_eq!(config, Config::default());
}

#[test]
fn a_missing_config_file_is_the_normal_case() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("cfgnone");

    assert_eq!(
        config::load_with_args(&[]).expect("load"),
        Config::default()
    );
}
