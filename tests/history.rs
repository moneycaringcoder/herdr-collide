use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, Write};
use std::os::unix::fs::{symlink, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};

use collide::config::{self, Config};
use collide::history::{self, EpisodeRecord, EpisodeTracker, MAX_HISTORY_BYTES};
use collide::model::{Checkout, FileVerdict, Pairing, RepoKey, Report, SharedFile};

fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct TempDirs {
    root: PathBuf,
}

impl TempDirs {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "collide-history-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("state")).expect("state dir");
        fs::create_dir_all(root.join("config")).expect("config dir");
        std::env::set_var("HERDR_PLUGIN_STATE_DIR", root.join("state"));
        std::env::set_var("HERDR_PLUGIN_CONFIG_DIR", root.join("config"));
        Self { root }
    }

    fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }

    fn config_file(&self) -> PathBuf {
        self.root.join("config/config.json")
    }
}

impl Drop for TempDirs {
    fn drop(&mut self) {
        std::env::remove_var("HERDR_PLUGIN_STATE_DIR");
        std::env::remove_var("HERDR_PLUGIN_CONFIG_DIR");
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn enabled_config() -> Config {
    Config {
        conflict_history: true,
        ..Config::default()
    }
}

fn checkout(id: &str, label: &str, branch: &str) -> Checkout {
    Checkout {
        workspace_id: id.to_string(),
        workspace_label: label.to_string(),
        repo_key: RepoKey("/repos/project/.git".to_string()),
        repo_root: PathBuf::from("/repos/project"),
        checkout_path: PathBuf::from(format!("/worktrees/{id}")),
        is_linked_worktree: true,
        branch: Some(branch.to_string()),
        agent: Some("coding-agent".to_string()),
    }
}

fn report(
    verdict: Option<FileVerdict>,
    left_label: &str,
    right_label: &str,
    pair_order: (&str, &str),
) -> Report {
    let shared = verdict
        .map(|verdict| SharedFile {
            path: "src/lib.rs".to_string(),
            verdict,
            conflict_type: None,
        })
        .into_iter()
        .collect();
    Report {
        checkouts: vec![
            checkout("ws-a", left_label, "feature/a"),
            checkout("ws-b", right_label, "feature/b"),
        ],
        pairings: vec![Pairing {
            left_workspace_id: pair_order.0.to_string(),
            right_workspace_id: pair_order.1.to_string(),
            shared,
            approximate: false,
        }],
        ..Report::default()
    }
}

fn record(path: &str, timestamp: u64) -> EpisodeRecord {
    EpisodeRecord {
        repo_key: "/repos/project/.git".to_string(),
        path: path.to_string(),
        left_workspace_id: "ws-a".to_string(),
        right_workspace_id: "ws-b".to_string(),
        left_workspace_label: "api worktree".to_string(),
        right_workspace_label: "web worktree".to_string(),
        left_branch: Some("feature/api".to_string()),
        right_branch: Some("feature/web".to_string()),
        first_seen_unix_seconds: timestamp,
        last_seen_unix_seconds: None,
    }
}

fn closing_record(path: &str, first_seen: u64, last_seen: u64) -> EpisodeRecord {
    EpisodeRecord {
        last_seen_unix_seconds: Some(last_seen),
        ..record(path, first_seen)
    }
}

fn exists(path: &Path) -> bool {
    fs::metadata(path).is_ok()
}

#[test]
fn persisting_conflict_writes_one_episode_not_one_per_cycle() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("persistent");
    let mut tracker = EpisodeTracker::default();
    let conflict = report(
        Some(FileVerdict::Conflict),
        "left",
        "right",
        ("ws-a", "ws-b"),
    );

    history::record_cycle_at(&enabled_config(), &mut tracker, &conflict, 100).unwrap();
    history::record_cycle_at(&enabled_config(), &mut tracker, &conflict, 200).unwrap();
    history::record_cycle_at(
        &enabled_config(),
        &mut tracker,
        &report(None, "left", "right", ("ws-a", "ws-b")),
        300,
    )
    .unwrap();

    let records = history::load_records().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].first_seen_unix_seconds, 100);
    assert_eq!(records[0].last_seen_unix_seconds, None);
    assert_eq!(records[1].first_seen_unix_seconds, 100);
    assert_eq!(records[1].last_seen_unix_seconds, Some(200));
    assert!(history::render_records(&records).contains("last seen 200"));
}

#[test]
fn conflict_that_clears_and_returns_writes_a_second_episode() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("returns");
    let mut tracker = EpisodeTracker::default();
    let config = enabled_config();

    history::record_cycle_at(
        &config,
        &mut tracker,
        &report(
            Some(FileVerdict::Conflict),
            "left",
            "right",
            ("ws-a", "ws-b"),
        ),
        100,
    )
    .unwrap();
    history::record_cycle_at(
        &config,
        &mut tracker,
        &report(None, "left", "right", ("ws-a", "ws-b")),
        200,
    )
    .unwrap();
    history::record_cycle_at(
        &config,
        &mut tracker,
        &report(
            Some(FileVerdict::Conflict),
            "left",
            "right",
            ("ws-a", "ws-b"),
        ),
        300,
    )
    .unwrap();

    let records = history::load_records().unwrap();
    assert_eq!(records.len(), 3);
    assert_eq!(records[0].first_seen_unix_seconds, 100);
    assert_eq!(records[0].last_seen_unix_seconds, None);
    assert_eq!(records[1].first_seen_unix_seconds, 100);
    assert_eq!(records[1].last_seen_unix_seconds, Some(100));
    assert_eq!(records[2].first_seen_unix_seconds, 300);
    assert_eq!(records[2].last_seen_unix_seconds, None);
    let output = history::render_records(&records);
    assert!(output.contains("2 episodes"), "{output}");
    assert!(output.contains("episode still open"), "{output}");
}

#[test]
fn overlap_and_unknown_never_write_collision_history() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("non-conflicts");
    let mut tracker = EpisodeTracker::default();
    let config = enabled_config();

    history::record_cycle_at(
        &config,
        &mut tracker,
        &report(
            Some(FileVerdict::Overlap),
            "left",
            "right",
            ("ws-a", "ws-b"),
        ),
        100,
    )
    .unwrap();
    history::record_cycle_at(
        &config,
        &mut tracker,
        &report(
            Some(FileVerdict::Unknown),
            "left",
            "right",
            ("ws-a", "ws-b"),
        ),
        200,
    )
    .unwrap();

    assert!(!exists(&config::history_file()));
}

#[test]
fn unknown_does_not_end_an_active_episode() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("unknown-active");
    let mut tracker = EpisodeTracker::default();
    let config = enabled_config();

    for (verdict, seen_at) in [
        (FileVerdict::Conflict, 100),
        (FileVerdict::Unknown, 200),
        (FileVerdict::Conflict, 300),
    ] {
        history::record_cycle_at(
            &config,
            &mut tracker,
            &report(Some(verdict), "left", "right", ("ws-a", "ws-b")),
            seen_at,
        )
        .unwrap();
    }

    let records = history::load_records().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].first_seen_unix_seconds, 100);
    assert_eq!(records[0].last_seen_unix_seconds, None);
}

#[test]
fn workspace_pair_order_is_canonical_across_cycles() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("pair-order");
    let mut tracker = EpisodeTracker::default();
    let config = enabled_config();

    history::record_cycle_at(
        &config,
        &mut tracker,
        &report(
            Some(FileVerdict::Conflict),
            "left",
            "right",
            ("ws-a", "ws-b"),
        ),
        100,
    )
    .unwrap();
    history::record_cycle_at(
        &config,
        &mut tracker,
        &report(
            Some(FileVerdict::Conflict),
            "left",
            "right",
            ("ws-b", "ws-a"),
        ),
        200,
    )
    .unwrap();

    let records = history::load_records().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].left_workspace_id, "ws-a");
    assert_eq!(records[0].right_workspace_id, "ws-b");
}

#[test]
fn daemon_restart_restores_an_open_episode() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("restart");
    let config = enabled_config();
    let conflict = report(
        Some(FileVerdict::Conflict),
        "left",
        "right",
        ("ws-a", "ws-b"),
    );
    let mut tracker = EpisodeTracker::default();

    history::record_cycle_at(&config, &mut tracker, &conflict, 100).unwrap();
    let records = history::load_records().unwrap();
    let mut restarted = EpisodeTracker::from_records(&records);
    history::record_cycle_at(&config, &mut restarted, &conflict, 200).unwrap();
    history::record_cycle_at(
        &config,
        &mut restarted,
        &report(None, "left", "right", ("ws-a", "ws-b")),
        300,
    )
    .unwrap();

    let records = history::load_records().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].first_seen_unix_seconds, 100);
    assert_eq!(records[1].first_seen_unix_seconds, 100);
    assert_eq!(records[1].last_seen_unix_seconds, Some(200));
}

#[test]
fn workspace_rename_does_not_fragment_an_active_episode() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("rename");
    let mut tracker = EpisodeTracker::default();
    let config = enabled_config();

    history::record_cycle_at(
        &config,
        &mut tracker,
        &report(
            Some(FileVerdict::Conflict),
            "old left",
            "old right",
            ("ws-a", "ws-b"),
        ),
        100,
    )
    .unwrap();
    history::record_cycle_at(
        &config,
        &mut tracker,
        &report(
            Some(FileVerdict::Conflict),
            "renamed left",
            "renamed right",
            ("ws-a", "ws-b"),
        ),
        200,
    )
    .unwrap();

    let records = history::load_records().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].left_workspace_id, "ws-a");
    assert_eq!(records[0].left_workspace_label, "old left");
}

#[test]
fn history_is_opt_in_and_the_default_creates_no_file() {
    let _guard = env_lock();
    let dirs = TempDirs::new("off");
    let mut tracker = EpisodeTracker::default();

    history::record_cycle_at(
        &Config::default(),
        &mut tracker,
        &report(
            Some(FileVerdict::Conflict),
            "left",
            "right",
            ("ws-a", "ws-b"),
        ),
        100,
    )
    .unwrap();

    assert!(!Config::default().conflict_history);
    assert_eq!(
        config::history_file(),
        dirs.state_dir().join("conflict-history.jsonl")
    );
    assert!(!exists(&config::history_file()));
}

#[test]
fn conflict_history_config_key_is_optional_boolean_and_defaults_off() {
    let _guard = env_lock();
    let dirs = TempDirs::new("config");

    assert!(!config::load_with_args(&[]).unwrap().conflict_history);
    fs::write(dirs.config_file(), r#"{"conflict_history": true}"#).unwrap();
    assert!(config::load_with_args(&[]).unwrap().conflict_history);

    fs::write(dirs.config_file(), r#"{"conflict_history": "yes"}"#).unwrap();
    assert_eq!(config::load_with_args(&[]).unwrap(), Config::default());
}

#[test]
fn invalid_later_record_cannot_partially_append_a_batch() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("batch");
    let oversized_path = "x".repeat(MAX_HISTORY_BYTES as usize);

    let result = history::append_records(&[
        record("would-otherwise-land.rs", 10),
        record(&oversized_path, 20),
    ]);

    assert!(result.is_err());
    assert!(
        !exists(&config::history_file()),
        "the valid prefix of a rejected batch must not become durable"
    );
}

#[test]
fn cap_keeps_the_newest_lines_and_private_file_within_one_megabyte() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("cap");
    let large_path = "x".repeat(24_000);
    let mut records = (0..50)
        .map(|index| record(&format!("{large_path}/{index}"), index))
        .collect::<Vec<_>>();
    records.push(record("newest.rs", 999));

    history::append_records(&records).unwrap();

    let metadata = fs::metadata(config::history_file()).unwrap();
    assert!(metadata.len() <= MAX_HISTORY_BYTES, "{}", metadata.len());
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    let retained = history::load_records().unwrap();
    assert!(!retained.is_empty());
    assert_eq!(retained.last().unwrap().path, "newest.rs");
}

#[test]
fn history_file_symlink_cannot_redirect_an_append_outside_plugin_state() {
    let _guard = env_lock();
    let dirs = TempDirs::new("symlink");
    let repository_file = dirs.root.join("checkout/notes.txt");
    fs::create_dir_all(repository_file.parent().unwrap()).unwrap();
    fs::write(&repository_file, b"repository content\n").unwrap();
    symlink(&repository_file, config::history_file()).unwrap();

    assert!(history::append_records(&[record("src/lib.rs", 10)]).is_err());
    assert_eq!(
        fs::read(&repository_file).unwrap(),
        b"repository content\n",
        "following the state-file symlink would violate read-only operation"
    );
}

#[test]
fn history_file_symlink_cannot_redirect_a_read_outside_plugin_state() {
    let _guard = env_lock();
    let dirs = TempDirs::new("symlink-read");
    let repository_file = dirs.root.join("checkout/history.jsonl");
    fs::create_dir_all(repository_file.parent().unwrap()).unwrap();
    let contents = format!(
        "{}\n",
        serde_json::to_string(&record("secret.rs", 10)).unwrap()
    );
    fs::write(&repository_file, &contents).unwrap();
    symlink(&repository_file, config::history_file()).unwrap();

    assert!(history::load_records().is_err());
    assert_eq!(fs::read_to_string(&repository_file).unwrap(), contents);
}

#[test]
fn identity_check_refuses_to_trim_a_file_the_plugin_does_not_own() {
    let _guard = env_lock();
    let dirs = TempDirs::new("identity");
    let owned_path = config::history_file();
    let foreign_path = dirs.state_dir().join("foreign.log");
    fs::write(&owned_path, b"owned history\n").unwrap();
    fs::write(&foreign_path, b"do not truncate this file\n").unwrap();
    let before = fs::read(&foreign_path).unwrap();
    let mut foreign = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&foreign_path)
        .unwrap();

    assert!(!history::trim_if_owned(&mut foreign, &owned_path, 1).unwrap());
    let mut after = Vec::new();
    foreign.rewind().unwrap();
    foreign.read_to_end(&mut after).unwrap();
    assert_eq!(after, before);
}

#[test]
fn corrupt_and_truncated_lines_are_skipped_without_losing_valid_history() {
    let _guard = env_lock();
    let _dirs = TempDirs::new("corrupt");
    let first = serde_json::to_string(&record("first.rs", 10)).unwrap();
    let second = serde_json::to_string(&record("second.rs", 20)).unwrap();
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(config::history_file())
        .unwrap();
    writeln!(file, "{first}").unwrap();
    writeln!(file, "not json").unwrap();
    writeln!(file, "{second}").unwrap();
    write!(file, "{{\"repo_key\":").unwrap();
    drop(file);

    let records = history::load_records().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].path, "first.rs");
    assert_eq!(records[1].path, "second.rs");

    // The next append starts on a fresh line, so a killed writer does not also
    // consume the first valid record written after it.
    history::append_records(&[record("after-truncation.rs", 30)]).unwrap();
    let records = history::load_records().unwrap();
    assert_eq!(records.len(), 3);
    assert_eq!(records[2].path, "after-truncation.rs");
}

#[test]
fn history_verb_orders_repeat_offenders_and_clear_removes_the_record() {
    let _guard = env_lock();
    let dirs = TempDirs::new("verbs");
    history::append_records(&[
        record("frequent.rs", 10),
        closing_record("frequent.rs", 10, 20),
        record("single.rs", 30),
        record("frequent.rs", 40),
        closing_record("frequent.rs", 40, 60),
    ])
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_collide"))
        .arg("--history")
        .env("HERDR_PLUGIN_STATE_DIR", dirs.state_dir())
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let frequent = stdout
        .find("2 episodes | /repos/project/.git :: frequent.rs")
        .unwrap();
    let single = stdout
        .find("1 episode | /repos/project/.git :: single.rs")
        .unwrap();
    assert!(frequent < single, "{stdout}");
    assert!(stdout.contains("feature/api"), "{stdout}");
    assert!(stdout.contains("last seen 60"), "{stdout}");
    assert!(stdout.contains("episode still open"), "{stdout}");

    let clear = Command::new(env!("CARGO_BIN_EXE_collide"))
        .arg("--history-clear")
        .env("HERDR_PLUGIN_STATE_DIR", dirs.state_dir())
        .output()
        .unwrap();
    assert!(clear.status.success());
    assert!(String::from_utf8(clear.stdout)
        .unwrap()
        .contains("Conflict history cleared"));
    assert!(!exists(&config::history_file()));
}
