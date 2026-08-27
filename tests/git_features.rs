//! Observed-behavior coverage for Git and filesystem features used by snapshots.

#[path = "fixtures.rs"]
mod fixtures;

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use collide::collide::gather_for;
use collide::config::Config;
use collide::model::{FileVerdict, Severity};
use fixtures::{checkout, Fixture};

const TIMEOUT: Duration = Duration::from_secs(30);

fn config() -> Config {
    Config {
        base_ref: "main".to_string(),
        git_timeout: TIMEOUT,
        ..Config::default()
    }
}

#[test]
fn split_index_untracked_cache_and_fsmonitor_preserve_prediction() {
    let fixture = Fixture::new("index-extensions");
    let (left, right) = fixture.uncommitted_conflict_pair();
    fixture.git(&fixture.repo, &["config", "core.untrackedCache", "true"]);
    let hook = fixture.root().join("fsmonitor-ok.sh");
    std::fs::write(&hook, "#!/bin/sh\nprintf '/\\0'\n").expect("write fsmonitor");
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))
        .expect("chmod fsmonitor");
    fixture.git(
        &fixture.repo,
        &["config", "core.fsmonitor", hook.to_str().unwrap()],
    );
    for worktree in [&left, &right] {
        fixture.git(worktree, &["update-index", "--split-index"]);
        fixture.git(worktree, &["update-index", "--untracked-cache"]);
    }

    let cycle = gather_for(
        vec![
            checkout("left", &left, "unused"),
            checkout("right", &right, "unused"),
        ],
        &config(),
    )
    .expect("gather");
    assert!(cycle.notes.is_empty(), "{:?}", cycle.notes);
    assert_eq!(
        cycle.report.pairings[0].shared[0].verdict,
        FileVerdict::Conflict
    );
}

#[test]
fn a_git_managed_worktree_relocation_preserves_prediction() {
    let fixture = Fixture::new("relocated-worktree");
    let (left, right) = fixture.uncommitted_conflict_pair();
    let relocated = fixture.root().join("relocated-left");
    fixture.git(
        &fixture.repo,
        &[
            "worktree",
            "move",
            left.to_str().unwrap(),
            relocated.to_str().unwrap(),
        ],
    );

    let cycle = gather_for(
        vec![
            checkout("left", &relocated, "stale-herdr-key"),
            checkout("right", &right, "stale-herdr-key"),
        ],
        &config(),
    )
    .expect("gather");
    assert_eq!(cycle.report.pairings[0].conflicts(), 1);
    assert_eq!(
        cycle
            .report
            .checkouts
            .iter()
            .find(|checkout| checkout.workspace_id == "left")
            .expect("left checkout")
            .checkout_path,
        relocated
    );
}

#[test]
fn canonically_equivalent_unicode_filename_conflicts_are_not_lost() {
    let fixture = Fixture::new("unicode-normalization");
    fixture.write(&fixture.repo, "caf\u{e9}.txt", "base\n");
    fixture.commit_all(&fixture.repo, "unicode path");
    let left = fixture.worktree("unicode-left", "unicode-left");
    let right = fixture.worktree("unicode-right", "unicode-right");
    fixture.write(&left, "caf\u{e9}.txt", "left\n");
    fixture.write(&right, "caf\u{e9}.txt", "right\n");

    let cycle = gather_for(
        vec![
            checkout("left", &left, "unused"),
            checkout("right", &right, "unused"),
        ],
        &config(),
    )
    .expect("gather");
    assert_eq!(cycle.report.pairings[0].conflicts(), 1);
}

#[test]
fn case_insensitive_filesystems_keep_one_changed_path() {
    let fixture = Fixture::new("case-folding");
    fixture.write(&fixture.repo, "CaseProbe.txt", "base\n");
    if !fixture.repo.join("caseprobe.txt").exists() {
        eprintln!("skipped: this filesystem is case-sensitive");
        return;
    }
    fixture.commit_all(&fixture.repo, "case path");
    let left = fixture.worktree("case-left", "case-left");
    let right = fixture.worktree("case-right", "case-right");
    fixture.write(&left, "CaseProbe.txt", "left\n");
    fixture.write(&right, "caseprobe.txt", "right\n");

    let cycle = gather_for(
        vec![
            checkout("left", &left, "unused"),
            checkout("right", &right, "unused"),
        ],
        &config(),
    )
    .expect("gather");
    assert_eq!(cycle.report.pairings.len(), 1);
    assert_eq!(cycle.report.pairings[0].conflicts(), 1);
    assert_eq!(cycle.report.statuses[0].severity, Severity::Conflict);
}
