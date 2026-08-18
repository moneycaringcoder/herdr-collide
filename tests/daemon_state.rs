//! Marker-file and config tests for the badge updater.
//!
//! These run against a temp state dir, never the user's real one, and never
//! spawn a daemon: every case either short-circuits before the spawn or records
//! a pid that is already live (our own test process).
//!
//! What that leaves uncovered, written down so the next reader does not have to
//! infer it from the runtime:
//!
//!   * `stop_daemon`'s escalation — the `SIGTERM` wait, the `SIGKILL` that
//!     follows it, and the "survived `SIGKILL`" error;
//!   * `spawn_detached`, including the branch that kills a child whose pid could
//!     not be recorded;
//!   * `cap_log` end to end (its decision is covered by
//!     `should_truncate_log`, against real files, but nothing exercises the
//!     `ftruncate` itself);
//!   * `push`, the glue that threads one `previous` map through `badge_plan` and
//!     `next_active`. Both halves are covered here; the wiring is not, and the
//!     second bug in `next_active` lived in exactly that seam.
//!
//! `SpawnLock` under contention *is* covered — see
//! `enable_waits_for_a_held_spawn_lock` — because `flock` is per open file
//! description, so two acquisitions in one process contend the way two processes
//! would.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Barrier, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use collide::config::{
    self, Config, DEFAULT_BASE_REF, MAX_GIT_TIMEOUT_SECONDS, MAX_INTERVAL_SECONDS,
    MIN_GIT_TIMEOUT_SECONDS, MIN_INTERVAL_SECONDS,
};
use collide::daemon::{self, BadgeOp, LitTokens, PushOutcome, SpawnLock};
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
        unknown_count: usize::from(severity == Severity::Unknown),
        runaway: severity == Severity::Runaway,
        lines_changed: 0,
        changed_files: 0,
    }
}

/// One token believed lit per workspace, the ordinary case.
fn lit(pairs: &[(&str, &str)]) -> LitTokens {
    pairs
        .iter()
        .map(|(id, token)| {
            (
                id.to_string(),
                std::iter::once(token.to_string()).collect::<BTreeSet<String>>(),
            )
        })
        .collect()
}

/// Several tokens believed lit on one workspace, which is what an unconfirmed
/// clear leaves behind.
fn lit_many(workspace_id: &str, tokens: &[&str]) -> LitTokens {
    std::iter::once((
        workspace_id.to_string(),
        tokens.iter().map(|t| t.to_string()).collect(),
    ))
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

    fn state_dir(&self) -> PathBuf {
        self.root.join("state")
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
///
/// The paths below are **literals, taken from the machine this was written
/// against**, not values computed from `config::state_dir()`. An earlier
/// version of this test injected whatever the code had just produced and then
/// asserted the two were equal, which is true of any implementation and so
/// could never fail. These are the real directories herdr created there:
///
/// ```text
/// $ ls ~/.local/state/herdr/plugins/
/// ez-corp.git-status  herdr.agent-icons  moneycaringcoder.collide  nicosuave.memex …
/// $ ls ~/.config/herdr/plugins/config/
/// ez-corp.git-status  herdr.agent-icons  moneycaringcoder.collide  usagebar …
/// ```
#[test]
fn hand_invocation_and_herdr_invocation_resolve_to_one_directory() {
    let _guard = env_lock();
    let env = dir_env();
    env.set("HOME", "/home/test");

    // The layout herdr uses, spelled out rather than derived.
    let herdr_state = "/home/test/.local/state/herdr/plugins/moneycaringcoder.collide";
    let herdr_config = "/home/test/.config/herdr/plugins/config/moneycaringcoder.collide";

    // Run by hand: nothing injected. This must already land where herdr puts
    // it, because there is no second chance to agree later.
    assert_eq!(config::state_dir(), PathBuf::from(herdr_state));
    assert_eq!(config::config_dir(), PathBuf::from(herdr_config));

    // Run by herdr: it injects those same paths, and they must be honoured.
    env.set("HERDR_PLUGIN_STATE_DIR", herdr_state);
    env.set("HERDR_PLUGIN_CONFIG_DIR", herdr_config);
    assert_eq!(config::state_dir(), PathBuf::from(herdr_state));
    assert_eq!(config::config_dir(), PathBuf::from(herdr_config));

    // `HERDR_PLUGIN_ID` is not something herdr sets — it appears nowhere in the
    // 0.8.0 binary — so the constant is what has to match the directory name
    // above. If the constant ever changes, the hand-run path silently stops
    // being herdr's path, which is exactly the bug this test exists for.
    assert_eq!(config::PLUGIN_ID, "moneycaringcoder.collide");
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
    assert!(
        daemon::badge_plan(&LitTokens::new(), &[status("w6", Severity::Clean, 0, 0)]).is_empty()
    );
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

fn severities(pairs: &[(&str, Severity)]) -> BTreeMap<String, Severity> {
    pairs
        .iter()
        .map(|(workspace_id, severity)| (workspace_id.to_string(), *severity))
        .collect()
}

#[test]
fn the_first_notification_cycle_establishes_a_baseline_without_alerting() {
    let current = [status("w6", Severity::Conflict, 1, 0)];

    assert!(
        daemon::notification_plan(None, &current).is_empty(),
        "a restart must not turn an old conflict into new news"
    );
}

#[test]
fn every_lower_severity_escalating_to_conflict_is_an_edge() {
    for previous in [
        Severity::Clean,
        Severity::Overlap,
        Severity::Runaway,
        Severity::Unknown,
    ] {
        let plan = daemon::notification_plan(
            Some(&severities(&[("w6", previous)])),
            &[status("w6", Severity::Conflict, 1, 0)],
        );

        assert_eq!(plan.len(), 1, "{previous:?} -> conflict must notify");
        assert_eq!(plan[0].workspace_id, "w6");
        assert_eq!(plan[0].previous, previous);
        assert_eq!(plan[0].current, Severity::Conflict);
    }
}

#[test]
fn clean_to_runaway_is_not_a_notification_edge() {
    assert!(daemon::notification_plan(
        Some(&severities(&[("w6", Severity::Clean)])),
        &[status("w6", Severity::Runaway, 0, 0)],
    )
    .is_empty());
}

#[test]
fn overlap_to_unknown_is_not_a_notification_edge() {
    assert!(daemon::notification_plan(
        Some(&severities(&[("w6", Severity::Overlap)])),
        &[status("w6", Severity::Unknown, 0, 0)],
    )
    .is_empty());
}

#[test]
fn no_severity_transition_to_runaway_is_notification_news() {
    for previous in [
        Severity::Clean,
        Severity::Overlap,
        Severity::Runaway,
        Severity::Unknown,
        Severity::Conflict,
    ] {
        assert!(
            daemon::notification_plan(
                Some(&severities(&[("w6", previous)])),
                &[status("w6", Severity::Runaway, 0, 0)],
            )
            .is_empty(),
            "{previous:?} -> runaway must not notify"
        );
    }
}

#[test]
fn no_severity_transition_to_unknown_is_notification_news() {
    for previous in [
        Severity::Clean,
        Severity::Overlap,
        Severity::Runaway,
        Severity::Unknown,
        Severity::Conflict,
    ] {
        assert!(
            daemon::notification_plan(
                Some(&severities(&[("w6", previous)])),
                &[status("w6", Severity::Unknown, 0, 0)],
            )
            .is_empty(),
            "{previous:?} -> unknown must not notify"
        );
    }
}

#[test]
fn no_severity_transition_to_overlap_is_notification_news() {
    for previous in [
        Severity::Clean,
        Severity::Overlap,
        Severity::Runaway,
        Severity::Unknown,
        Severity::Conflict,
    ] {
        assert!(
            daemon::notification_plan(
                Some(&severities(&[("w6", previous)])),
                &[status("w6", Severity::Overlap, 0, 1)],
            )
            .is_empty(),
            "{previous:?} -> overlap must not notify"
        );
    }
}

#[test]
fn a_conflict_becoming_overlap_does_not_interrupt_the_user() {
    assert!(daemon::notification_plan(
        Some(&severities(&[("w6", Severity::Conflict)])),
        &[status("w6", Severity::Overlap, 0, 1)],
    )
    .is_empty());
}

#[test]
fn a_new_workspace_has_no_notification_baseline() {
    assert!(daemon::notification_plan(
        Some(&severities(&[("w6", Severity::Clean)])),
        &[
            status("w6", Severity::Clean, 0, 0),
            status("w7", Severity::Conflict, 1, 0),
        ],
    )
    .is_empty());
}

#[test]
fn the_first_notification_cycle_never_has_edges_for_any_severity() {
    let current = [
        status("clean", Severity::Clean, 0, 0),
        status("overlap", Severity::Overlap, 0, 1),
        status("runaway", Severity::Runaway, 0, 0),
        status("unknown", Severity::Unknown, 0, 0),
        status("conflict", Severity::Conflict, 1, 0),
    ];

    assert!(daemon::notification_plan(None, &current).is_empty());
}

#[test]
fn an_existing_conflict_is_a_level_not_an_edge() {
    assert!(daemon::notification_plan(
        Some(&severities(&[("w6", Severity::Conflict)])),
        &[status("w6", Severity::Conflict, 1, 0)],
    )
    .is_empty());
}

#[test]
fn losing_a_conflict_answer_is_not_a_resolution_or_a_notification() {
    assert!(daemon::notification_plan(
        Some(&severities(&[("w6", Severity::Conflict)])),
        &[status("w6", Severity::Unknown, 0, 0)],
    )
    .is_empty());
}

#[test]
fn a_conflict_becoming_clean_does_not_interrupt_the_user() {
    assert!(daemon::notification_plan(
        Some(&severities(&[("w6", Severity::Conflict)])),
        &[status("w6", Severity::Clean, 0, 0)],
    )
    .is_empty());
}

#[test]
fn a_workspace_disappearing_is_not_a_notification_edge() {
    assert!(
        daemon::notification_plan(Some(&severities(&[("w6", Severity::Conflict)])), &[],)
            .is_empty()
    );
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
        r#"{"interval_seconds": 30, "runaway_files": 7, "ignore_suffixes": [".snap"], "notifications_enabled": true}"#,
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
    assert!(config.notifications_enabled);
    assert!(
        !Config::default().notifications_enabled,
        "desktop notifications are opt-in"
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

/// serde will build a struct out of a JSON *sequence* by position, so
/// `[1, 2, 3]` used to deserialize cleanly into
/// `interval_seconds = 1, runaway_files = 2, runaway_lines = 3` — with no
/// warning at all. A garbage file silently reconfiguring the plugin is worse
/// than a garbage file being ignored.
#[test]
fn a_config_file_that_is_not_an_object_is_rejected() {
    let _guard = env_lock();
    let dirs = TempDirs::new("cfgarray");

    for contents in ["[1, 2, 3]", "\"interval_seconds\"", "42", "true", "null"] {
        std::fs::write(dirs.config_file(), contents).expect("write config");
        assert_eq!(
            config::load_with_args(&[]).expect("never fatal"),
            Config::default(),
            "config file {contents:?} must not reconfigure anything"
        );
    }
}

/// A zero git timeout is not "no timeout", it is "every git call fails", which
/// leaves every workspace degraded while the plugin looks like it is working.
#[test]
fn the_git_timeout_is_clamped_like_the_interval() {
    let _guard = env_lock();
    let dirs = TempDirs::new("cfggit");

    std::fs::write(dirs.config_file(), r#"{"git_timeout_seconds": 0}"#).expect("write");
    assert_eq!(
        config::load_with_args(&[]).expect("load").git_timeout,
        Duration::from_secs(MIN_GIT_TIMEOUT_SECONDS)
    );

    std::fs::write(
        dirs.config_file(),
        r#"{"git_timeout_seconds": 18446744073709551615}"#,
    )
    .expect("write");
    assert_eq!(
        config::load_with_args(&[]).expect("load").git_timeout,
        Duration::from_secs(MAX_GIT_TIMEOUT_SECONDS)
    );

    // A sane value survives untouched.
    std::fs::write(dirs.config_file(), r#"{"git_timeout_seconds": 30}"#).expect("write");
    assert_eq!(
        config::load_with_args(&[]).expect("load").git_timeout,
        Duration::from_secs(30)
    );
}

/// An unknown key is still applied around rather than fatal — a newer config
/// file must not break an older binary — but the keys beside it still take
/// effect, so a typo cannot look like a setting that worked.
#[test]
fn an_unrecognised_key_does_not_stop_the_rest_of_the_file() {
    let _guard = env_lock();
    let dirs = TempDirs::new("cfgtypo");

    std::fs::write(
        dirs.config_file(),
        r#"{"interval_secondz": 300, "runaway_files": 9}"#,
    )
    .expect("write config");

    let config = config::load_with_args(&[]).expect("load");
    assert_eq!(config.runaway_files, 9);
    assert_eq!(
        config.interval,
        Config::default().interval,
        "the misspelled key must not have taken effect"
    );
}

/// The file path already refused an empty ref; the command line did not, and an
/// empty `--base-ref` reaches git as an empty string.
#[test]
fn an_empty_base_ref_on_the_command_line_is_fatal() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("cfgbaseref");

    assert!(config::load_with_args(&owned(&["--base-ref", ""])).is_err());
    assert!(config::load_with_args(&owned(&["--base-ref=  "])).is_err());
    assert_eq!(
        config::load_with_args(&owned(&["--base-ref", "main"]))
            .expect("load")
            .base_ref,
        "main"
    );
}

// ---------------------------------------------------------------------------
// What stays lit after a cycle
// ---------------------------------------------------------------------------

fn set(workspace_id: &str, token: &'static str) -> BadgeOp {
    BadgeOp::Set {
        workspace_id: workspace_id.to_string(),
        token,
        text: "x".to_string(),
    }
}

fn clear(workspace_id: &str, token: &str) -> BadgeOp {
    BadgeOp::Clear {
        workspace_id: workspace_id.to_string(),
        token: token.to_string(),
    }
}

/// The bug this exists for, captured off the wire: a set that herdr rejects
/// used to erase the daemon's record of the token herdr was still rendering, so
/// the next severity flip emitted no clear for it and two collide tokens lit at
/// once on one workspace.
#[test]
fn a_rejected_set_does_not_make_the_daemon_forget_what_is_lit() {
    let before = lit(&[("w6", "collide_overlap")]);

    let after = daemon::next_active(
        &before,
        &[(set("w6", "collide_overlap"), PushOutcome::Failed)],
    );

    assert_eq!(
        after, before,
        "herdr is still rendering collide_overlap under its TTL"
    );

    // …so the next cycle, with the severity flipped, still clears it first.
    let plan = daemon::badge_plan(&after, &[status("w6", Severity::Runaway, 0, 0)]);
    assert!(
        plan.contains(&BadgeOp::Clear {
            workspace_id: "w6".to_string(),
            token: "collide_overlap".to_string(),
        }),
        "two badges would light at once: {plan:?}"
    );
}

#[test]
fn a_successful_set_replaces_what_was_lit() {
    let after = daemon::next_active(
        &lit(&[("w6", "collide_overlap")]),
        &[
            (clear("w6", "collide_overlap"), PushOutcome::Done),
            (set("w6", "collide_conflict"), PushOutcome::Done),
        ],
    );
    assert_eq!(after, lit(&[("w6", "collide_conflict")]));
}

#[test]
fn a_failed_clear_is_remembered_so_it_can_be_reissued() {
    let after = daemon::next_active(
        &lit(&[("w6", "collide_conflict")]),
        &[(clear("w6", "collide_conflict"), PushOutcome::Failed)],
    );
    assert_eq!(
        after,
        lit(&[("w6", "collide_conflict")]),
        "the token is still lit, so forgetting it strands the badge"
    );
}

/// The same bug from the other side, and the reason a workspace holds a *set* of
/// names rather than one.
///
/// On a severity flip the plan is `[Clear(old), Set(new)]`. When the clear fails
/// and the set succeeds, herdr is rendering both: it never got the clear, and it
/// did get the set. With one name per workspace the successful set overwrote the
/// old one, the daemon forgot a token it was responsible for, and no later cycle
/// ever cleared it — two collide badges on one workspace until the old one's TTL
/// ran out.
#[test]
fn a_failed_clear_survives_a_successful_set_on_the_same_workspace() {
    let previous = lit(&[("w6", "collide_overlap")]);
    let plan = daemon::badge_plan(&previous, &[status("w6", Severity::Conflict, 1, 0)]);
    assert_eq!(plan.len(), 2, "a flip clears before it sets: {plan:?}");

    // Exactly the wire outcome: the clear did not take, the set did.
    let results: Vec<(BadgeOp, PushOutcome)> = plan
        .into_iter()
        .map(|op| {
            let outcome = match &op {
                BadgeOp::Clear { .. } => PushOutcome::Failed,
                BadgeOp::Set { .. } => PushOutcome::Done,
            };
            (op, outcome)
        })
        .collect();

    let after = daemon::next_active(&previous, &results);
    assert_eq!(
        after,
        lit_many("w6", &["collide_conflict", "collide_overlap"]),
        "both are lit on herdr's side, so both have to be on ours"
    );

    // …and the next cycle, with the severity unchanged, still clears the one
    // that never went out.
    let next = daemon::badge_plan(&after, &[status("w6", Severity::Conflict, 1, 0)]);
    assert!(
        next.contains(&BadgeOp::Clear {
            workspace_id: "w6".to_string(),
            token: "collide_overlap".to_string(),
        }),
        "the stranded token is never cleared again: {next:?}"
    );
    assert!(
        next.contains(&BadgeOp::Set {
            workspace_id: "w6".to_string(),
            token: "collide_conflict",
            text: "✘ 1".to_string(),
        }),
        "and the badge the workspace should show is still refreshed: {next:?}"
    );
}

/// Once the clear does take, the extra name goes and the plan settles.
#[test]
fn a_reissued_clear_that_succeeds_settles_the_workspace() {
    let stranded = lit_many("w6", &["collide_conflict", "collide_overlap"]);
    let plan = daemon::badge_plan(&stranded, &[status("w6", Severity::Conflict, 1, 0)]);
    let results: Vec<(BadgeOp, PushOutcome)> =
        plan.into_iter().map(|op| (op, PushOutcome::Done)).collect();

    let after = daemon::next_active(&stranded, &results);
    assert_eq!(after, lit(&[("w6", "collide_conflict")]));

    let settled = daemon::badge_plan(&after, &[status("w6", Severity::Conflict, 1, 0)]);
    assert!(
        !settled.iter().any(|op| matches!(op, BadgeOp::Clear { .. })),
        "nothing left to clear: {settled:?}"
    );
}

/// A workspace that drops out of the report has *all* of its names cleared, not
/// just the last one recorded.
#[test]
fn a_workspace_that_left_the_report_clears_every_name_it_had() {
    let plan = daemon::badge_plan(
        &lit_many("w9", &["collide_conflict", "collide_overlap"]),
        &[],
    );
    assert_eq!(
        plan,
        vec![
            BadgeOp::Clear {
                workspace_id: "w9".to_string(),
                token: "collide_conflict".to_string(),
            },
            BadgeOp::Clear {
                workspace_id: "w9".to_string(),
                token: "collide_overlap".to_string(),
            },
        ]
    );
}

/// A workspace that closed under us took its badge with it, so there is nothing
/// left to clear and nothing to remember — otherwise the daemon would reissue a
/// doomed clear on every cycle for the rest of its life.
#[test]
fn a_workspace_that_went_away_is_forgotten() {
    assert!(daemon::next_active(
        &lit(&[("w6", "collide_conflict")]),
        &[(clear("w6", "collide_conflict"), PushOutcome::Gone)],
    )
    .is_empty());

    assert!(daemon::next_active(
        &lit(&[("w6", "collide_conflict")]),
        &[(set("w6", "collide_overlap"), PushOutcome::Gone)],
    )
    .is_empty());
}

#[test]
fn one_workspaces_failure_does_not_disturb_another() {
    let after = daemon::next_active(
        &lit(&[("w6", "collide_overlap"), ("w7", "collide_conflict")]),
        &[
            (set("w6", "collide_conflict"), PushOutcome::Failed),
            (clear("w7", "collide_conflict"), PushOutcome::Done),
            (set("w7", "collide_overlap"), PushOutcome::Done),
        ],
    );
    assert_eq!(
        after,
        lit(&[("w6", "collide_overlap"), ("w7", "collide_overlap")])
    );
}

// ---------------------------------------------------------------------------
// Refusing to start a daemon nobody could stop
// ---------------------------------------------------------------------------

/// An unwritable state dir used to be a warning: `--enable` exited 0, spawned a
/// daemon, failed to record its pid, and did the same again on every subsequent
/// invocation. The daemons piled up and `--disable` could not stop any of them,
/// because there was no pid file to read.
#[cfg(unix)]
#[test]
fn enable_refuses_to_spawn_when_the_state_dir_cannot_be_written() {
    use std::os::unix::fs::PermissionsExt;

    let _guard = env_lock();
    let dirs = TempDirs::new("readonly");
    let state = dirs.state_dir();

    let original = std::fs::metadata(&state).expect("stat").permissions();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o500)).expect("chmod");

    let result = daemon::enable(&owned(&["--enable"]));

    // Restore before asserting, so a failure cannot leave an unwritable dir
    // behind for the Drop impl.
    std::fs::set_permissions(&state, original).expect("chmod back");

    let err = result.expect_err("an unstoppable daemon is worse than no daemon");
    let message = err.to_string();
    assert!(
        message.contains("lock") || message.contains("state directory"),
        "the message must name the problem: {message}"
    );
    assert!(
        !exists(&config::pid_file()),
        "nothing may have been spawned"
    );
    assert!(
        !exists(&config::enabled_flag()),
        "and nothing may have been marked either"
    );
}

/// `--enable` is check-then-act, so two of them — a keypress and a `--restore`
/// startup hook during a handoff — must not both conclude that no daemon is
/// running. The lock is the only thing that makes that true.
///
/// This holds the lock on another thread and times how long `--enable` takes to
/// get past it. The previous version of this test held nothing and only
/// exercised the live-pid short-circuit, which behaved identically before the
/// lock existed — a test named for a property it did not test.
///
/// A pid file is written first so the `--enable` under test short-circuits after
/// taking the lock rather than spawning a real daemon.
#[test]
fn enable_waits_for_a_held_spawn_lock() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("lock");
    daemon::write_pid(std::process::id());

    let hold = Duration::from_millis(400);
    let taken = Arc::new(Barrier::new(2));
    let holder = {
        let taken = Arc::clone(&taken);
        std::thread::spawn(move || {
            let lock = SpawnLock::acquire().expect("hold the lock");
            taken.wait();
            std::thread::sleep(hold);
            drop(lock);
        })
    };
    taken.wait();

    let started = Instant::now();
    daemon::enable(&owned(&["--enable"])).expect("enable");
    let waited = started.elapsed();
    holder.join().expect("holder thread");

    assert!(
        waited >= hold / 2,
        "--enable did not wait for the lock (returned after {waited:?}, \
         while it was held for {hold:?})"
    );
    assert_eq!(daemon::read_pid(), Some(std::process::id() as i32));
    assert!(exists(&config::enabled_flag()));
    daemon::clear_pid_file();
}

/// The lock file lives in the state dir beside the markers, and must not be
/// mistaken for one: its presence says nothing about whether a daemon is wanted
/// or running.
#[test]
fn the_lock_file_is_not_a_marker() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("lockfile");

    // A bad value fails before any state is touched, so nothing here is marked…
    daemon::enable(&owned(&["--enable", "--interval", "soon"])).expect_err("bad interval");
    assert!(!daemon::is_enabled());
    assert_eq!(daemon::live_pid(), None);

    // …and a lock file on its own still means neither.
    drop(SpawnLock::acquire().expect("lock"));
    assert!(
        exists(&config::lock_file()),
        "the lock file should have been created"
    );
    assert!(!daemon::is_enabled(), "a lock is not the enabled marker");
    assert_eq!(daemon::live_pid(), None, "a lock is not a live daemon");
}

/// `--disable` exists to make the badges go away. An earlier version returned on
/// the first problem, so the cases where a badge was most likely to be stranded
/// — an unwritable state dir, a daemon that would not die — were exactly the
/// cases where nothing tried to clear it.
#[cfg(unix)]
#[test]
fn disable_still_tries_to_clear_the_badges_when_the_lock_cannot_be_taken() {
    use std::os::unix::fs::PermissionsExt;

    let _guard = env_lock();
    let dirs = TempDirs::new("disable-readonly");
    let state = dirs.state_dir();
    // No server here, so the sweep will fail too — which is the point: the
    // error has to mention both problems, not just the first.
    std::env::set_var("HERDR_SOCKET_PATH", state.join("absent.sock"));

    let original = std::fs::metadata(&state).expect("stat").permissions();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o500)).expect("chmod");

    let result = daemon::disable();

    std::fs::set_permissions(&state, original).expect("chmod back");
    std::env::remove_var("HERDR_SOCKET_PATH");

    let message = result.expect_err("both halves failed").to_string();
    assert!(
        message.contains("lock"),
        "the lock problem must be reported: {message}"
    );
    assert!(
        message.contains("badges"),
        "the badges were not even attempted, or the attempt was not reported: {message}"
    );
}

// ---------------------------------------------------------------------------
// Whose log is it
// ---------------------------------------------------------------------------

/// The daemon caps its own log so a cycle that fails all week cannot fill the
/// disk. It must cap *only* its own: `collide --daemon 2>>~/notes.log` points
/// stderr at a regular file too, and truncating that destroys a file the plugin
/// never opened. Testing "is it a regular file?" passed for both.
#[test]
fn only_the_daemons_own_log_is_ever_truncated() {
    let _guard = env_lock();
    let dirs = TempDirs::new("logcap");
    let ours = dirs.state_dir().join("updater.log");
    let theirs = dirs.state_dir().join("somebody-elses.log");
    let big = vec![b'x'; 2048];
    std::fs::write(&ours, &big).expect("write log");
    std::fs::write(&theirs, &big).expect("write other file");

    let ours_meta = std::fs::metadata(&ours).expect("stat");
    let theirs_meta = std::fs::metadata(&theirs).expect("stat");

    assert!(
        daemon::should_truncate_log(&ours_meta, Some(&ours_meta), 1024),
        "our own oversized log is exactly what the cap is for"
    );
    assert!(
        !daemon::should_truncate_log(&theirs_meta, Some(&ours_meta), 1024),
        "a file the daemon did not open must never be truncated"
    );
    assert!(
        !daemon::should_truncate_log(&ours_meta, Some(&ours_meta), 8192),
        "under the cap there is nothing to do"
    );
    assert!(
        !daemon::should_truncate_log(&ours_meta, None, 1024),
        "with no log of our own on disk, stderr is not ours to truncate"
    );

    // A directory stands in for "not a regular file" — a terminal or a pipe
    // cannot be conjured from a path, and the branch is the same one.
    let dir_meta = std::fs::metadata(dirs.state_dir()).expect("stat");
    assert!(!daemon::should_truncate_log(&dir_meta, Some(&ours_meta), 0));
}

// ---------------------------------------------------------------------------
// A severity with nowhere to render
// ---------------------------------------------------------------------------

/// herdr renders a plugin token only if `config.toml` names it, so an install
/// that ran `--setup` before `collide_unknown` existed shows *nothing* for that
/// severity. And because `Unknown` outranks `Overlap` and `Runaway`, a workspace
/// that used to show a badge goes blank after the upgrade — which reads as
/// clean. Nothing on the wire reveals it: `report_metadata` accepts any token
/// name, and `server.reload_config` answers `applied` for a file naming none of
/// ours. So the daemon reads the file and says so.
#[test]
fn the_daemon_notices_when_herdrs_sidebar_cannot_render_a_badge() {
    let _guard = env_lock();
    let dirs = TempDirs::new("tokens");
    let herdr_config = dirs.state_dir().join("herdr-config.toml");
    std::env::set_var("HERDR_CONFIG_PATH", &herdr_config);

    let note_for = |body: &str| -> Option<String> {
        std::fs::write(&herdr_config, body).expect("write herdr config");
        collide::setup::sidebar_token_note()
    };

    // The upgrade case: three named, the newest not.
    let older = "[ui.sidebar.spaces]\nrows = [\n  [\"branch\",\n\
        { token = \"$collide_overlap\",  fg = \"#FFC799\" },\n\
        { token = \"$collide_runaway\",  fg = \"#FFB27F\" },\n\
        { token = \"$collide_conflict\", fg = \"#FF8080\" }],\n]\n";
    let note = note_for(older).expect("a severity that cannot render must be reported");
    assert!(note.contains("$collide_unknown"), "{note}");
    assert!(
        note.contains("set up sidebar"),
        "the note must name the action that fixes it: {note}"
    );

    // A file naming all four says nothing at all.
    let current = "[ui.sidebar.spaces]\nrows = [\n  [\"branch\",\n\
        { token = \"$collide_overlap\",  fg = \"#FFC799\" },\n\
        { token = \"$collide_runaway\",  fg = \"#FFB27F\" },\n\
        { token = \"$collide_unknown\",  fg = \"#9399B2\" },\n\
        { token = \"$collide_conflict\", fg = \"#FF8080\" }],\n]\n";
    assert_eq!(
        note_for(current),
        None,
        "a correctly configured install must stay quiet, or the note teaches \
         people to ignore it — `collide_clean` is deliberately not in the set"
    );

    // A file that does not exist at all is the fresh-install case, and is worth
    // exactly the same sentence.
    std::fs::remove_file(&herdr_config).expect("remove");
    let note = collide::setup::sidebar_token_note().expect("no config, no badges");
    assert!(note.contains("set up sidebar"), "{note}");

    std::env::remove_var("HERDR_CONFIG_PATH");
}
