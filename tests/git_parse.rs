//! Parsing of real git plumbing output, and the change-set assembly built on
//! it. Every byte string these tests parse comes out of an actual git process
//! running against a throwaway fixture, not from a hand-written literal, so a
//! change in git's framing fails the test instead of passing silently.

#[path = "fixtures.rs"]
mod fixtures;

use std::time::Duration;

use collide::git::{
    self, change_set, current_branch, parse_merge_tree_z, parse_numstat_z, parse_status_v2,
};
use collide::model::ChangeKind;

use fixtures::Fixture;

const TIMEOUT: Duration = Duration::from_secs(30);

/// Raw `status --porcelain=v2 -z` bytes for a worktree.
fn status_bytes(fixture: &Fixture, cwd: &std::path::Path, ignored: bool) -> Vec<u8> {
    let mut args = vec![
        "--no-optional-locks",
        "status",
        "--porcelain=v2",
        "-z",
        "--untracked-files=all",
        "--renames",
    ];
    if ignored {
        args.push("--ignored=matching");
    }
    // `try_git` trims, which would eat trailing NULs, so go through Command
    // directly for the byte-exact output.
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(&args)
        .env("HOME", fixture.root().join("home"))
        .env("GIT_CONFIG_GLOBAL", fixture.root().join("home/.gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("spawn git status");
    assert!(out.status.success(), "status failed");
    out.stdout
}

fn paths_of(entries: &[git::StatusEntry]) -> Vec<String> {
    entries.iter().map(|e| e.path.clone()).collect()
}

#[test]
fn ordinary_records_carry_staged_and_unstaged_kinds() {
    let fixture = Fixture::new("ordinary");
    let wt = fixture.worktree("wt", "wt");

    // Staged only.
    fixture.write(&wt, "conflict.txt", "staged\nbeta\ngamma\n");
    fixture.git(&wt, &["add", "conflict.txt"]);
    // Staged and then dirtied again.
    fixture.write(&wt, "shared.txt", "dirty\n");

    let entries = parse_status_v2(&status_bytes(&fixture, &wt, false));
    let conflict = entries.iter().find(|e| e.path == "conflict.txt").unwrap();
    assert_eq!(conflict.kind, ChangeKind::Staged);
    assert!(!conflict.is_rename);

    let shared = entries.iter().find(|e| e.path == "shared.txt").unwrap();
    assert_eq!(shared.kind, ChangeKind::Unstaged);
}

#[test]
fn rename_record_captures_both_paths_and_does_not_desync_the_stream() {
    let fixture = Fixture::new("rename");
    let wt = fixture.worktree("wt", "wt");

    fixture.git(&wt, &["mv", "renamed.txt", "moved.txt"]);
    // A second, ordinary record after the rename. If the parser forgets that a
    // `2` record consumes two NUL fields, it reads the origin path as a status
    // line and this entry goes missing.
    fixture.write(&wt, "conflict.txt", "touched\nbeta\ngamma\n");
    fixture.git(&wt, &["add", "conflict.txt"]);

    let entries = parse_status_v2(&status_bytes(&fixture, &wt, false));
    let rename = entries.iter().find(|e| e.path == "moved.txt").unwrap();
    assert!(rename.is_rename);
    assert_eq!(rename.origin.as_deref(), Some("renamed.txt"));

    assert!(
        entries.iter().any(|e| e.path == "conflict.txt"),
        "record after the rename was lost: {:?}",
        paths_of(&entries)
    );
    // The origin path must never be mistaken for a record of its own.
    assert!(!entries.iter().any(|e| e.path.starts_with("R.")));
}

#[test]
fn both_halves_of_a_rename_enter_the_change_set() {
    let fixture = Fixture::new("rename-changeset");
    let wt = fixture.worktree("wt", "wt");
    fixture.git(&wt, &["mv", "renamed.txt", "moved.txt"]);

    let set = change_set(&wt, "main", TIMEOUT).expect("change set");
    let paths: Vec<&str> = set.paths.iter().map(|p| p.path.as_str()).collect();
    assert!(paths.contains(&"moved.txt"), "{paths:?}");
    assert!(paths.contains(&"renamed.txt"), "{paths:?}");
}

#[test]
fn unmerged_records_are_reported_as_conflicted() {
    let fixture = Fixture::new("unmerged");
    let (a, _b) = fixture.committed_conflict_pair();

    let (code, _out, _err) = fixture.try_git(&a, &["merge", "--no-edit", "conflict-b"]);
    assert_ne!(code, 0, "the fixture merge was supposed to conflict");

    let entries = parse_status_v2(&status_bytes(&fixture, &a, false));
    let entry = entries.iter().find(|e| e.path == "conflict.txt").unwrap();
    assert_eq!(entry.kind, ChangeKind::Conflicted);

    // A worktree mid-merge is flagged so callers can mark predictions advisory.
    let set = change_set(&a, "main", TIMEOUT).expect("change set");
    assert!(set.degraded);
    assert!(set
        .degraded_reason
        .as_deref()
        .unwrap()
        .contains(git::DEGRADED_UNMERGED));
}

#[test]
fn untracked_files_are_reported_and_ignored_files_are_not() {
    let fixture = Fixture::new("untracked");
    let wt = fixture.worktree("wt", "wt");
    fixture.write(&wt, "fresh.txt", "new\n");
    fixture.ignored_files(&wt);

    // Even when git is explicitly asked for `!` records, the parser drops them.
    let entries = parse_status_v2(&status_bytes(&fixture, &wt, true));
    let paths = paths_of(&entries);
    assert!(paths.contains(&"fresh.txt".to_string()), "{paths:?}");
    assert!(
        !paths.iter().any(|p| p.contains("ignored/")),
        "ignored directory leaked in: {paths:?}"
    );
    assert!(
        !paths.iter().any(|p| p.ends_with("build.log")),
        "ignored file leaked in: {paths:?}"
    );

    let fresh = entries.iter().find(|e| e.path == "fresh.txt").unwrap();
    assert_eq!(fresh.kind, ChangeKind::Untracked);
}

#[test]
fn paths_with_a_space_and_a_newline_survive_z_framing() {
    let fixture = Fixture::new("weird-paths");
    let wt = fixture.worktree("wt", "wt");
    let (spaced, newline) = fixture.tricky_untracked(&wt);

    let entries = parse_status_v2(&status_bytes(&fixture, &wt, false));
    let paths = paths_of(&entries);
    assert!(
        paths.contains(&spaced),
        "path with a space was truncated: {paths:?}"
    );
    assert!(
        paths.contains(&newline),
        "path with a newline was split: {paths:?}"
    );

    // And they reach the change set intact.
    let set = change_set(&wt, "main", TIMEOUT).expect("change set");
    let set_paths: Vec<&str> = set.paths.iter().map(|p| p.path.as_str()).collect();
    assert!(set_paths.contains(&spaced.as_str()), "{set_paths:?}");
    assert!(set_paths.contains(&newline.as_str()), "{set_paths:?}");
}

#[test]
fn numstat_z_rename_records_consume_two_extra_fields() {
    let fixture = Fixture::new("numstat");
    let wt = fixture.worktree("wt", "wt");
    fixture.git(&wt, &["mv", "renamed.txt", "moved.txt"]);
    fixture.write(&wt, "conflict.txt", "one\ntwo\nthree\nfour\n");
    fixture.commit_all(&wt, "rename plus edit");

    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&wt)
        .args(["diff", "--numstat", "-z", "main...HEAD"])
        .env("HOME", fixture.root().join("home"))
        .env("GIT_CONFIG_GLOBAL", fixture.root().join("home/.gitconfig"))
        .output()
        .expect("spawn git diff");
    let stats = parse_numstat_z(&out.stdout);

    let rename = stats
        .iter()
        .find(|s| s.paths.iter().any(|p| p == "moved.txt"))
        .expect("rename record");
    assert_eq!(
        rename.paths,
        vec!["renamed.txt".to_string(), "moved.txt".to_string()]
    );

    let edit = stats
        .iter()
        .find(|s| s.paths == vec!["conflict.txt".to_string()])
        .expect("ordinary record after the rename");
    assert_eq!(edit.added, 4);
    assert_eq!(edit.removed, 3);
}

#[test]
fn merge_tree_z_clean_merge_is_a_single_field() {
    let fixture = Fixture::new("mt-clean");
    let (a, _b) = fixture.committed_clean_overlap_pair();

    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&a)
        .args([
            "merge-tree",
            "--write-tree",
            "-z",
            "--name-only",
            "clean-a",
            "clean-b",
        ])
        .env("HOME", fixture.root().join("home"))
        .env("GIT_CONFIG_GLOBAL", fixture.root().join("home/.gitconfig"))
        .output()
        .expect("spawn merge-tree");
    assert_eq!(out.status.code(), Some(0));

    let parsed = parse_merge_tree_z(&out.stdout);
    assert_eq!(parsed.tree.len(), 40, "expected a tree OID, got {parsed:?}");
    assert!(parsed.conflicted.is_empty(), "{parsed:?}");
}

#[test]
fn merge_tree_z_conflict_yields_paths_and_a_machine_stable_type() {
    let fixture = Fixture::new("mt-conflict");
    let (a, _b) = fixture.committed_conflict_pair();

    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&a)
        .args([
            "merge-tree",
            "--write-tree",
            "-z",
            "--name-only",
            "conflict-a",
            "conflict-b",
        ])
        .env("HOME", fixture.root().join("home"))
        .env("GIT_CONFIG_GLOBAL", fixture.root().join("home/.gitconfig"))
        .output()
        .expect("spawn merge-tree");
    assert_eq!(out.status.code(), Some(1));

    let parsed = parse_merge_tree_z(&out.stdout);
    assert_eq!(parsed.conflicted, vec!["conflict.txt".to_string()]);
    assert!(
        parsed
            .conflict_types
            .iter()
            .any(|t| t.starts_with("CONFLICT (")),
        "no machine-stable conflict token in {parsed:?}"
    );
}

#[test]
fn change_set_unions_dirty_and_committed_paths_with_line_counts() {
    let fixture = Fixture::new("union");
    let wt = fixture.worktree("wt", "wt");

    fixture.write(&wt, "conflict.txt", "one\ntwo\nthree\nfour\nfive\n");
    fixture.commit_all(&wt, "committed edit");
    fixture.write(&wt, "shared.txt", "dirty\n");
    fixture.write(&wt, "untracked.txt", "a\nb\nc\n");

    let set = change_set(&wt, "main", TIMEOUT).expect("change set");
    let by_path: std::collections::BTreeMap<&str, ChangeKind> = set
        .paths
        .iter()
        .map(|p| (p.path.as_str(), p.kind))
        .collect();

    assert_eq!(by_path.get("conflict.txt"), Some(&ChangeKind::Committed));
    assert_eq!(by_path.get("shared.txt"), Some(&ChangeKind::Unstaged));
    assert_eq!(by_path.get("untracked.txt"), Some(&ChangeKind::Untracked));
    assert!(!set.degraded, "{:?}", set.degraded_reason);

    // 5 committed additions, 1 dirty line, 3 untracked lines.
    assert!(
        set.lines_added >= 5 + 1 + 3,
        "line counts look empty: +{} -{}",
        set.lines_added,
        set.lines_removed
    );
    assert!(set.lines_removed >= 3, "-{}", set.lines_removed);
}

#[test]
fn detached_head_is_usable_and_has_no_branch() {
    let fixture = Fixture::new("detached");
    let wt = fixture.detached_worktree("detached");
    fixture.write(&wt, "conflict.txt", "detached edit\nbeta\ngamma\n");

    assert_eq!(current_branch(&wt, TIMEOUT).unwrap(), None);

    let set = change_set(&wt, "main", TIMEOUT).expect("change set");
    assert!(
        !set.degraded,
        "detached HEAD should be usable, got {:?}",
        set.degraded_reason
    );
    assert!(set.paths.iter().any(|p| p.path == "conflict.txt"));
}

#[test]
fn unborn_branch_degrades_and_is_unpairable() {
    let fixture = Fixture::new("unborn");
    let wt = fixture.unborn_worktree("unborn", "fresh");
    fixture.write(&wt, "brand.txt", "hello\nworld\n");
    fixture.git(&wt, &["add", "brand.txt"]);

    assert!(matches!(
        git::head_state(&wt, TIMEOUT).unwrap(),
        git::HeadState::Unborn { .. }
    ));

    let set = change_set(&wt, "main", TIMEOUT).expect("change set");
    assert!(set.degraded);
    let reason = set.degraded_reason.as_deref().unwrap();
    assert!(reason.contains(git::DEGRADED_UNBORN), "{reason}");
    assert!(!collide::collide::pairable(&set));
    // The change set is still the useful part: every staged addition.
    assert!(set.paths.iter().any(|p| p.path == "brand.txt"));
    assert_eq!(set.lines_added, 2);
}

#[test]
fn deleted_branch_degrades_distinctly_from_unborn() {
    let fixture = Fixture::new("deleted");
    let wt = fixture.deleted_branch_worktree("doomed", "doomed");

    assert!(
        matches!(
            git::head_state(&wt, TIMEOUT).unwrap(),
            git::HeadState::BrokenHead { .. }
        ),
        "a deleted branch must not be reported as unborn"
    );

    let set = change_set(&wt, "main", TIMEOUT).expect("change set");
    assert!(set.degraded);
    let reason = set.degraded_reason.as_deref().unwrap();
    assert!(reason.contains(git::DEGRADED_BROKEN_HEAD), "{reason}");
    assert!(!collide::collide::pairable(&set));
    // The branch name is still worth reporting even though it no longer exists.
    assert_eq!(
        current_branch(&wt, TIMEOUT).unwrap().as_deref(),
        Some("doomed")
    );
}

#[test]
fn a_missing_integration_ref_degrades_without_erroring() {
    let fixture = Fixture::new("no-base");
    let wt = fixture.worktree("wt", "wt");
    fixture.write(&wt, "conflict.txt", "edited\nbeta\ngamma\n");

    let set = change_set(&wt, "refs/heads/does-not-exist", TIMEOUT).expect("no Err for a bad base");
    assert!(set.degraded);
    assert!(set
        .degraded_reason
        .as_deref()
        .unwrap()
        .contains(git::DEGRADED_MISSING_BASE_REF));
    // The dirty half is still reported.
    assert!(set.paths.iter().any(|p| p.path == "conflict.txt"));
}

#[test]
fn repo_key_is_shared_by_worktrees_and_differs_across_repos() {
    let fixture = Fixture::new("repo-key");
    let wt = fixture.worktree("wt", "wt");
    let foreign = fixture.foreign_repo("foreign");

    let main_key = git::repo_key(&fixture.repo, TIMEOUT).unwrap();
    let wt_key = git::repo_key(&wt, TIMEOUT).unwrap();
    let foreign_key = git::repo_key(&foreign, TIMEOUT).unwrap();

    assert_eq!(main_key, wt_key, "worktrees of one repo share a key");
    assert_ne!(main_key, foreign_key);
}
