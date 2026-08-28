#[path = "fixtures.rs"]
mod fixtures;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::Command;

use collide::collide::{gather_for, text_report, why_for};
use collide::config::Config;
use collide::git::{self, WHY_BLOB_MAX_BYTES};
use collide::model::{Checkout, FileVerdict};
use serde_json::json;

use fixtures::{checkout, Fixture};

fn config() -> Config {
    Config {
        base_ref: "main".to_string(),
        predict_conflicts: true,
        ..Config::default()
    }
}

fn checkouts(fixture: &Fixture, pair: &(PathBuf, PathBuf)) -> Vec<Checkout> {
    let key = git::repo_key(&fixture.repo, config().git_timeout).expect("repo key");
    vec![
        checkout("left-worktree", &pair.0, &key.0),
        checkout("right-worktree", &pair.1, &key.0),
    ]
}

fn run_cli_why(fixture: &Fixture, pair: &(PathBuf, PathBuf), path: &str) -> std::process::Output {
    let socket = fixture
        .repo
        .parent()
        .expect("fixture root")
        .join("why.sock");
    let listener = UnixListener::bind(&socket).expect("bind fake herdr");
    let workspaces: Vec<_> = [("left-worktree", &pair.0), ("right-worktree", &pair.1)]
        .into_iter()
        .enumerate()
        .map(|(index, (label, checkout_path))| {
            json!({
                "workspace_id": label,
                "number": index + 1,
                "label": label,
                "focused": false,
                "pane_count": 0,
                "tab_count": 0,
                "agent_status": "idle",
                "worktree": {
                    "repo_key": fixture.repo.join(".git"),
                    "repo_name": "repo",
                    "repo_root": fixture.repo,
                    "checkout_path": checkout_path,
                    "is_linked_worktree": true
                }
            })
        })
        .collect();
    let mut reply = json!({
        "id": null,
        "result": {
            "type": "session_snapshot",
            "snapshot": {
                "protocol": 19,
                "version": "0.8.0",
                "layouts": [],
                "tabs": [],
                "panes": [],
                "agents": [],
                "workspaces": workspaces
            }
        }
    });
    let server = std::thread::spawn(move || loop {
        let (stream, _) = listener.accept().expect("accept collide");
        let mut request = String::new();
        BufReader::new(&stream)
            .read_line(&mut request)
            .expect("read request");
        if request.is_empty() {
            continue;
        }
        assert!(request.contains("session.snapshot"), "{request}");
        let request: serde_json::Value =
            serde_json::from_str(&request).expect("parse fake Herdr request");
        reply["id"] = request["id"].clone();
        let reply = reply.to_string();
        let mut stream = &stream;
        stream.write_all(reply.as_bytes()).expect("write reply");
        stream.write_all(b"\n").expect("finish reply");
        break;
    });

    let output = Command::new(env!("CARGO_BIN_EXE_collide"))
        .args(["--why", path, "--base-ref", "main"])
        .env("HERDR_SOCKET_PATH", &socket)
        .env("HERDR_PLUGIN_ID", "herdr.collide")
        .env("HERDR_WORKSPACE_ID", "left-worktree")
        .env(
            "HERDR_PLUGIN_STATE_DIR",
            fixture.repo.parent().expect("fixture root").join("state"),
        )
        .env(
            "HERDR_PLUGIN_CONFIG_DIR",
            fixture.repo.parent().expect("fixture root").join("config"),
        )
        .env(
            "HOME",
            fixture.repo.parent().expect("fixture root").join("home"),
        )
        .output()
        .expect("run collide --why");
    server.join().expect("fake herdr server");
    let _ = std::fs::remove_file(&socket);
    output
}

#[test]
fn why_conflict_prints_real_marker_hunks_and_names_both_worktrees() {
    let fixture = Fixture::new("why-conflict");
    let pair = fixture.committed_conflict_pair();
    let report = why_for(checkouts(&fixture, &pair), &config(), "conflict.txt").expect("why");

    assert!(!report.prediction_failed, "{}", report.text);
    assert!(report.text.contains("left-worktree"), "{}", report.text);
    assert!(report.text.contains("right-worktree"), "{}", report.text);
    assert!(report.text.contains("<<<<<<<"), "{}", report.text);
    assert!(report.text.contains("======="), "{}", report.text);
    assert!(report.text.contains(">>>>>>>"), "{}", report.text);
    assert!(report.text.contains("ALPHA-A"), "{}", report.text);
    assert!(report.text.contains("ALPHA-B"), "{}", report.text);
}

#[test]
fn why_clean_overlap_says_it_merges_cleanly_without_a_diff() {
    let fixture = Fixture::new("why-overlap");
    let pair = fixture.committed_clean_overlap_pair();
    let report = why_for(checkouts(&fixture, &pair), &config(), "shared.txt").expect("why");

    assert!(report.text.contains("left-worktree"), "{}", report.text);
    assert!(report.text.contains("right-worktree"), "{}", report.text);
    assert!(report.text.contains("merge cleanly"), "{}", report.text);
    assert!(!report.text.contains("unknown:"), "{}", report.text);
    assert!(
        !report.text.contains("did not produce an answer"),
        "{}",
        report.text
    );
    assert!(!report.text.contains("<<<<<<<"), "{}", report.text);
    assert!(!report.text.contains("diff --git"), "{}", report.text);
}

#[test]
fn why_unknown_says_prediction_did_not_run_and_requests_failure_status() {
    let fixture = Fixture::new("why-unknown");
    let pair = fixture.unrelated_history_pair();
    fixture.write(&pair.0, "unknown.txt", "left\n");
    fixture.write(&pair.1, "unknown.txt", "right\n");
    let report = why_for(checkouts(&fixture, &pair), &config(), "unknown.txt").expect("why");

    assert!(report.prediction_failed, "{}", report.text);
    assert!(
        report.text.contains("prediction did not run"),
        "{}",
        report.text
    );
    assert!(!report.text.contains("merge cleanly"), "{}", report.text);
}

#[test]
fn why_unshared_path_says_no_pair_shares_it() {
    let fixture = Fixture::new("why-unshared");
    let pair = fixture.committed_conflict_pair();
    let report = why_for(checkouts(&fixture, &pair), &config(), "not-shared.txt").expect("why");

    assert!(!report.prediction_failed, "{}", report.text);
    assert!(
        report.text.contains("not shared by any worktree pair"),
        "{}",
        report.text
    );
}

#[test]
fn why_discloses_an_in_progress_merge_before_its_verdict() {
    let fixture = Fixture::new("why-advisory");
    let mid_merge = fixture.merge_in_progress_worktree("mid-merge");
    let (other, _) = fixture.uncommitted_conflict_pair();
    let pair = (mid_merge, other);
    let report = why_for(checkouts(&fixture, &pair), &config(), "conflict.txt").expect("why");

    let advisory = report.text.find("advisory:").expect("advisory qualifier");
    let verdict = report
        .text
        .find("conflict:")
        .or_else(|| report.text.find("unknown:"))
        .expect("verdict");
    assert!(advisory < verdict, "{}", report.text);
    assert!(
        report
            .text
            .contains("tree that still contains conflict markers"),
        "{}",
        report.text
    );
}

#[test]
fn unreadable_checkout_is_unknown_instead_of_unshared() {
    let fixture = Fixture::new("why-unreadable-checkout");
    let healthy = fixture.worktree("healthy", "healthy");
    let unreadable = fixture.garbage_head_worktree("unreadable");
    let pair = (healthy, unreadable);
    let report = why_for(checkouts(&fixture, &pair), &config(), "conflict.txt").expect("why");

    assert!(report.prediction_failed, "{}", report.text);
    assert!(report.text.starts_with("unknown:"), "{}", report.text);
    assert!(report.text.contains("unreadable"), "{}", report.text);
    assert!(
        !report.text.contains("not shared by any worktree pair"),
        "{}",
        report.text
    );
}

#[test]
fn rename_conflict_on_an_unlisted_path_is_explained_and_matches_once() {
    let fixture = Fixture::new("why-rename-extra");
    let pair = fixture.committed_directory_rename_pair();
    let checkout_list = checkouts(&fixture, &pair);
    let cycle = gather_for(checkout_list.clone(), &config()).expect("once gather");
    let renamed_path =
        cycle
            .report
            .pairings
            .iter()
            .flat_map(|pairing| &pairing.shared)
            .find(|shared| {
                shared.verdict == FileVerdict::Conflict
                    && cycle.report.changes.iter().all(|(_, changes)| {
                        changes.paths.iter().all(|path| path.path != shared.path)
                    })
            })
            .map(|shared| shared.path.clone())
            .expect("rename-induced conflict path absent from both change sets");

    let once = text_report(&cycle);
    let why = why_for(checkout_list, &config(), &renamed_path).expect("why");
    assert!(once.contains(&renamed_path), "{once}");
    assert!(once.contains("conflict"), "{once}");
    assert!(why.text.contains(&renamed_path), "{}", why.text);
    assert!(why.text.contains("conflict"), "{}", why.text);
    assert!(
        why.text
            .starts_with("approximate: git reported this conflicting path"),
        "{}",
        why.text
    );
    assert!(
        !why.text.contains("not shared by any worktree pair"),
        "{}",
        why.text
    );
}

#[test]
fn oversized_conflict_blob_is_unknown_without_being_read() {
    let fixture = Fixture::new("why-blob-ceiling");
    let pair = fixture.committed_conflict_pair();
    let side_len = usize::try_from(WHY_BLOB_MAX_BYTES).expect("ceiling fits usize") + 1;
    fixture.write(
        &pair.0,
        "conflict.txt",
        &format!("{}\n", "A".repeat(side_len)),
    );
    fixture.commit_all(&pair.0, "large left");
    fixture.write(
        &pair.1,
        "conflict.txt",
        &format!("{}\n", "B".repeat(side_len)),
    );
    fixture.commit_all(&pair.1, "large right");

    let report = why_for(checkouts(&fixture, &pair), &config(), "conflict.txt").expect("why");
    assert!(report.prediction_failed, "{}", report.text);
    assert!(report.text.starts_with("unknown:"), "{}", report.text);
    assert!(
        report
            .text
            .contains(&format!("above {WHY_BLOB_MAX_BYTES} bytes")),
        "{}",
        report.text
    );
}

#[test]
fn cli_returns_failure_when_why_prediction_is_unknown() {
    let fixture = Fixture::new("why-cli-unknown");
    let pair = fixture.unrelated_history_pair();
    fixture.write(&pair.0, "unknown.txt", "left\n");
    fixture.write(&pair.1, "unknown.txt", "right\n");

    let output = run_cli_why(&fixture, &pair, "unknown.txt");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    assert_eq!(
        stdout.lines().next(),
        Some(
            "unknown: `unknown.txt` between left-worktree and right-worktree: prediction did not run"
        ),
        "{stdout}"
    );
    assert!(
        !stdout.contains("not shared by any worktree pair"),
        "{stdout}"
    );
}

#[test]
fn cli_conflict_overlap_and_unshared_outcomes_have_exact_first_lines() {
    let conflict_fixture = Fixture::new("why-cli-conflict");
    let conflict_pair = conflict_fixture.committed_conflict_pair();
    let conflict = run_cli_why(&conflict_fixture, &conflict_pair, "conflict.txt");
    assert_eq!(conflict.status.code(), Some(0), "{conflict:?}");
    let stdout = String::from_utf8(conflict.stdout).expect("utf8 conflict stdout");
    assert_eq!(
        stdout.lines().next(),
        Some("conflict: `conflict.txt` between left-worktree and right-worktree"),
        "{stdout}"
    );

    let overlap_fixture = Fixture::new("why-cli-overlap");
    let overlap_pair = overlap_fixture.committed_clean_overlap_pair();
    let overlap = run_cli_why(&overlap_fixture, &overlap_pair, "shared.txt");
    assert_eq!(overlap.status.code(), Some(0), "{overlap:?}");
    let stdout = String::from_utf8(overlap.stdout).expect("utf8 overlap stdout");
    assert_eq!(
        stdout.lines().next(),
        Some(
            "overlap: `shared.txt` was touched by left-worktree and right-worktree, but their changes merge cleanly"
        ),
        "{stdout}"
    );

    let unshared_fixture = Fixture::new("why-cli-unshared");
    let unshared_pair = unshared_fixture.committed_conflict_pair();
    let unshared = run_cli_why(&unshared_fixture, &unshared_pair, "not-shared.txt");
    assert_eq!(unshared.status.code(), Some(0), "{unshared:?}");
    let stdout = String::from_utf8(unshared.stdout).expect("utf8 unshared stdout");
    assert_eq!(
        stdout.lines().next(),
        Some("`not-shared.txt` is not shared by any worktree pair"),
        "{stdout}"
    );
}
