//! Parsing of real git plumbing output, the change-set assembly built on it,
//! and the decisions the git layer makes before it will ask git anything at all.
//! Every byte string these tests parse comes out of an actual git process
//! running against a throwaway fixture, not from a hand-written literal, so a
//! change in git's framing fails the test instead of passing silently.

#[path = "fixtures.rs"]
mod fixtures;

use std::time::Duration;

use collide::git::{
    self, change_set, current_branch, parse_merge_tree_z, parse_numstat_z, parse_status_v2,
    Predictor,
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
fn dirty_submodule_flags_survive_status_framing() {
    let fixture = Fixture::new("dirty-submodule-status");
    let (_superproject, first, _second, submodule) =
        fixture.superproject_with_submodule("embedded");
    fixture.write(&submodule, "payload.txt", "modified payload\n");
    fixture.write(&submodule, "untracked.txt", "untracked payload\n");
    // This tracked ordinary record must still parse after the fixed-position
    // `<sub>` field has been read from the preceding record.
    fixture.write(&first, "shared.txt", "following\n");

    let captured = status_bytes(&fixture, &first, false);
    assert!(
        captured
            .windows(b" S.MU ".len())
            .any(|part| part == b" S.MU "),
        "git did not produce the dirty-submodule flags this parser test needs: {captured:?}"
    );
    assert!(
        captured
            .windows(b"1 .M N...".len())
            .any(|part| part == b"1 .M N..."),
        "git did not produce the ordinary tracked record this parser test needs: {captured:?}"
    );
    let entries = parse_status_v2(&captured);
    assert_eq!(
        paths_of(&entries),
        vec!["embedded".to_string(), "shared.txt".to_string()],
        "reading `<sub>` desynchronised a later NUL-framed record"
    );

    let submodule = entries
        .iter()
        .find(|entry| entry.path == "embedded")
        .unwrap();
    let state = submodule.submodule.expect("S<c><m><u> state");
    assert!(!state.commit_changed);
    assert!(state.modified_content);
    assert!(state.untracked_content);
    let ordinary = entries
        .iter()
        .find(|entry| entry.path == "shared.txt")
        .unwrap();
    assert!(ordinary.submodule.is_none(), "ordinary paths use N...");
}

#[test]
fn commit_only_submodule_is_comparable_by_gitlink() {
    let fixture = Fixture::new("commit-only-submodule");
    let (_superproject, first, _second, submodule) =
        fixture.superproject_with_submodule("embedded");
    fixture.write(&submodule, "payload.txt", "new committed payload\n");
    fixture.commit_all(&submodule, "advance submodule pointer");

    let entries = parse_status_v2(&status_bytes(&fixture, &first, false));
    let state = entries
        .iter()
        .find(|entry| entry.path == "embedded")
        .and_then(|entry| entry.submodule)
        .expect("submodule state");
    assert!(state.commit_changed);
    assert!(!state.modified_content);
    assert!(!state.untracked_content);

    let set = change_set(&first, "refs/heads/main", TIMEOUT).expect("change set");
    let changed = set
        .paths
        .iter()
        .find(|path| path.path == "embedded")
        .expect("changed gitlink");
    assert!(
        !changed.submodule_contents_uncomparable,
        "a clean C-only submodule has a real gitlink for merge-tree to compare"
    );
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
fn a_path_with_a_space_survives_z_framing_intact() {
    let fixture = Fixture::new("weird-paths");
    let wt = fixture.worktree("wt", "wt");
    let (spaced, _newline) = fixture.tricky_untracked(&wt);

    let entries = parse_status_v2(&status_bytes(&fixture, &wt, false));
    let paths = paths_of(&entries);
    assert!(
        paths.contains(&spaced),
        "path with a space was truncated: {paths:?}"
    );

    // And it reaches the change set intact.
    let set = change_set(&wt, "main", TIMEOUT).expect("change set");
    let set_paths: Vec<&str> = set.paths.iter().map(|p| p.path.as_str()).collect();
    assert!(set_paths.contains(&spaced.as_str()), "{set_paths:?}");
}

/// A path containing a control character is deliberately *not* passed through:
/// it is drawn into a pane that redraws in place, and a newline in a filename
/// would corrupt every row below it. What must survive is the framing — the
/// record is one record, not two — and the identity of the file.
#[test]
fn a_path_with_a_newline_is_neutralised_but_not_split() {
    let fixture = Fixture::new("newline-path");
    let wt = fixture.worktree("wt", "wt");
    let (_spaced, newline) = fixture.tricky_untracked(&wt);
    assert!(newline.contains('\n'), "fixture stopped using a newline");

    let entries = parse_status_v2(&status_bytes(&fixture, &wt, false));
    let paths = paths_of(&entries);
    let rendered = paths
        .iter()
        .find(|p| p.starts_with("weird"))
        .unwrap_or_else(|| panic!("the record was split or lost: {paths:?}"));
    assert!(
        !rendered.contains('\n'),
        "a control character reached the model: {rendered:?}"
    );
    assert!(
        rendered.starts_with("weird\u{FFFD}name.txt~"),
        "unexpected rendering: {rendered:?}"
    );

    // One file on disk, one entry: the newline did not become a record boundary.
    assert_eq!(
        paths.iter().filter(|p| p.starts_with("weird")).count(),
        1,
        "{paths:?}"
    );
    let set = change_set(&wt, "main", TIMEOUT).expect("change set");
    assert!(
        set.paths.iter().any(|p| &p.path == rendered),
        "the change set disagrees with the parser: {:?}",
        set.paths
    );
}

/// Replacement on its own is not injective, and change sets are intersected by
/// string: `\xff-one.txt` and `\xfe-two.txt` both render as `<?>-one.txt`-shaped
/// text, so two worktrees holding two *different* files were reported as sharing
/// one. The rendering has to stay stable for identical bytes — `status` and
/// `merge-tree` output must still match — and differ for different bytes.
#[cfg(unix)]
#[test]
fn two_different_non_utf8_paths_do_not_collapse_into_one() {
    let fixture = Fixture::new("non-utf8");
    let wt = fixture.worktree("wt", "wt");
    let Some((first, second)) = fixture.distinct_invalid_utf8_untracked(&wt) else {
        // APFS and HFS+ enforce valid UTF-8 in filenames, so on macOS these two
        // files cannot exist and the collision this guards against cannot
        // arise. The byte-level half of the same guarantee is covered
        // everywhere by `the_digest_distinguishes_two_paths_from_captured_bytes`.
        eprintln!(
            "skipped: this filesystem refuses invalid UTF-8 filenames, \
             so the on-disk half of this case cannot be built here"
        );
        return;
    };
    assert_ne!(first, second);

    let bytes = status_bytes(&fixture, &wt, false);
    let entries = parse_status_v2(&bytes);
    let rendered: Vec<String> = paths_of(&entries)
        .into_iter()
        .filter(|p| p.contains('\u{FFFD}'))
        .collect();

    assert_eq!(
        rendered.len(),
        2,
        "two distinct files must render as two distinct paths: {rendered:?}"
    );
    assert_ne!(rendered[0], rendered[1], "{rendered:?}");

    // Stability: parsing the same bytes twice must give the same strings, or
    // status output and merge-tree output would stop matching each other.
    assert_eq!(paths_of(&parse_status_v2(&bytes)), paths_of(&entries));

    // And the change set carries both, still distinct.
    let set = change_set(&wt, "main", TIMEOUT).expect("change set");
    let in_set: Vec<&str> = set
        .paths
        .iter()
        .map(|p| p.path.as_str())
        .filter(|p| p.contains('\u{FFFD}'))
        .collect();
    assert_eq!(in_set.len(), 2, "{in_set:?}");
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
    assert_eq!(
        parsed.conflict_types,
        vec!["CONFLICT (contents)".to_string()],
        "conflict_types must be exactly the machine-stable tokens"
    );
}

/// git emits an `Auto-merging` message record for every file it merged
/// *successfully*, in the same framing as a real conflict record. Taking every
/// type field verbatim filled this list with noise, and `Vec::dedup` could not
/// collapse the repeats because they are not adjacent — for this pair the raw
/// sequence is `Auto-merging, CONFLICT (contents), Auto-merging,
/// CONFLICT (contents)`. Asserting `any(starts_with("CONFLICT ("))` passed
/// happily on that, which is why this asserts the whole list.
#[test]
fn merge_tree_conflict_types_exclude_auto_merging_and_never_repeat() {
    let fixture = Fixture::new("mt-types");
    let (a, _b) = fixture.two_file_conflict_pair();

    let (code, stdout) = fixture.merge_tree(
        &a,
        &[
            "--write-tree",
            "-z",
            "--name-only",
            "twofile-a",
            "twofile-b",
        ],
    );
    assert_eq!(code, 1, "the fixture pair was supposed to conflict");
    // Guard against the fixture going vacuous: git really does emit the noise.
    let raw = String::from_utf8_lossy(&stdout);
    assert!(
        raw.contains("Auto-merging"),
        "git no longer emits Auto-merging records; this test proves nothing now"
    );

    let parsed = parse_merge_tree_z(&stdout);
    assert_eq!(
        parsed.conflicted,
        vec!["conflict.txt".to_string(), "renamed.txt".to_string()]
    );
    assert_eq!(
        parsed.conflict_types,
        vec!["CONFLICT (contents)".to_string()],
        "two conflicting files must not yield four tokens, two of them noise"
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

/// A branch deleted underneath its worktree leaves exactly what an unborn one
/// leaves: HEAD pointing at a ref that is not in the ref store. It is reported
/// as having no commit, which is the part that is true and the part that
/// matters — it is degraded, unpairable, and still names its branch.
#[test]
fn a_deleted_branch_is_reported_as_having_no_commit() {
    let fixture = Fixture::new("deleted");
    let wt = fixture.deleted_branch_worktree("doomed", "doomed");

    assert!(
        matches!(
            git::head_state(&wt, TIMEOUT).unwrap(),
            git::HeadState::Unborn { .. }
        ),
        "HEAD names a ref that is not in the ref store"
    );

    let set = change_set(&wt, "main", TIMEOUT).expect("change set");
    assert!(set.degraded);
    let reason = set.degraded_reason.as_deref().unwrap();
    assert!(reason.contains(git::DEGRADED_UNBORN), "{reason}");
    assert!(!collide::collide::pairable(&set));
    // The branch name is still worth reporting even though it no longer exists.
    assert_eq!(
        current_branch(&wt, TIMEOUT).unwrap().as_deref(),
        Some("doomed")
    );
}

/// `docs/git-plumbing.md` used to assert that the worktree's `logs/HEAD` tells
/// an unborn branch from a deleted one. It does not, in either direction, and
/// both counter-examples are pinned here so the claim cannot come back.
///
/// Direction one: `git checkout --orphan` in a worktree that already had
/// commits is *genuinely unborn* and has a reflog, so the old rule called it a
/// deleted branch.
#[test]
fn an_orphan_branch_with_a_reflog_is_not_a_broken_head() {
    let fixture = Fixture::new("orphan-reflog");
    let wt = fixture.orphaned_in_place_worktree("orphan", "fresh");

    let git_dir = fixture.git(&wt, &["rev-parse", "--path-format=absolute", "--git-dir"]);
    let reflog = std::path::Path::new(&git_dir).join("logs/HEAD");
    assert!(
        reflog.metadata().map(|m| m.len() > 0).unwrap_or(false),
        "fixture no longer has the reflog that made the old rule wrong"
    );

    assert!(
        matches!(
            git::head_state(&wt, TIMEOUT).unwrap(),
            git::HeadState::Unborn { .. }
        ),
        "an orphan branch is unborn however much reflog the worktree carries"
    );
}

/// Direction two: with `core.logAllRefUpdates=false` a branch really was deleted
/// and there is no reflog to prove it, so the old rule called it unborn. Both
/// deleted-branch fixtures must now agree with each other.
#[test]
fn a_deleted_branch_without_a_reflog_is_classified_the_same_way() {
    let fixture = Fixture::new("deleted-no-reflog");
    let wt = fixture.deleted_branch_worktree_without_reflog("quiet", "quiet");

    let git_dir = fixture.git(&wt, &["rev-parse", "--path-format=absolute", "--git-dir"]);
    let reflog = std::path::Path::new(&git_dir).join("logs/HEAD");
    assert!(
        !reflog.metadata().map(|m| m.len() > 0).unwrap_or(false),
        "fixture no longer has the missing reflog that made the old rule wrong"
    );

    assert!(
        matches!(
            git::head_state(&wt, TIMEOUT).unwrap(),
            git::HeadState::Unborn { .. }
        ),
        "reflogging is a configuration choice, not evidence about commits"
    );
}

/// What the ref store *can* prove: the ref is there and still yields no commit.
/// That is the only state `BrokenHead` now claims.
#[test]
fn a_ref_pointing_at_a_missing_object_is_a_broken_head() {
    let fixture = Fixture::new("dangling");
    let wt = fixture.dangling_head_worktree("dangling", "dangling");

    assert!(
        matches!(
            git::head_state(&wt, TIMEOUT).unwrap(),
            git::HeadState::BrokenHead { .. }
        ),
        "a ref whose object is missing is broken, not empty"
    );

    // `change_set` cannot be asserted on here: `status` itself refuses a HEAD
    // whose object is gone (`fatal: bad object HEAD`), so the checkout fails
    // loudly one step earlier. That is the right outcome — the point of the
    // classification is that `BrokenHead` now names a state git can prove.
    assert!(matches!(
        git::current_branch(&wt, TIMEOUT).unwrap().as_deref(),
        Some("dangling")
    ));
}

/// A git that cannot answer must not be read as an answer. Every HEAD probe
/// exits 128 here, and the old code folded that into the same arm as "no
/// commit" — which silently removes a healthy checkout from every pairing while
/// telling the user its branch is unborn.
#[test]
fn a_head_git_cannot_read_is_an_error_not_an_unborn_branch() {
    let fixture = Fixture::new("garbage-head");
    let wt = fixture.garbage_head_worktree("garbage");

    let err = git::head_state(&wt, TIMEOUT)
        .expect_err("an unreadable HEAD must not be reported as a state");
    let text = err.to_string();
    assert!(
        text.contains("could not answer") || text.contains("timed out"),
        "unhelpful error: {text}"
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

// ---------------------------------------------------------------------------
// Choosing a base to measure against
// ---------------------------------------------------------------------------

/// The probe chain used to end in `Ok("HEAD")`, which is not a guess but a
/// fabrication: `HEAD...HEAD` is empty by construction, and because both the
/// base ref and the merge base then resolve, nothing was marked degraded either.
/// A repository whose trunk is `develop` reported every workspace as clean while
/// two agents committed conflicting edits to the same line of the same file.
#[test]
fn a_repo_with_no_conventional_trunk_reports_no_integration_ref() {
    let fixture = Fixture::new("trunkless");
    let repo = fixture.trunkless_repo("dev-trunk", "develop");

    assert_eq!(
        git::integration_ref(&repo, TIMEOUT).expect("probe"),
        None,
        "there is no honest ref to measure against here"
    );
}

/// And the sentinel it is replaced by has to *degrade*, not silently produce an
/// empty change set. Committed work is what disappears, so the fixture commits
/// something the old code lost entirely.
#[test]
fn the_no_integration_ref_sentinel_degrades_instead_of_reporting_clean() {
    let fixture = Fixture::new("trunkless-degrade");
    let repo = fixture.trunkless_repo("dev-trunk", "develop");
    fixture.write(&repo, "conflict.txt", "COMMITTED\nbeta\ngamma\n");
    fixture.commit_all(&repo, "work nobody can measure");

    let set = change_set(&repo, git::NO_INTEGRATION_REF, TIMEOUT).expect("change set");
    assert!(
        set.degraded,
        "an unmeasurable checkout must not read as a clean one"
    );
    let reason = set.degraded_reason.as_deref().unwrap();
    assert!(reason.contains(git::DEGRADED_MISSING_BASE_REF), "{reason}");
    // The sentinel is never handed to git, so nothing complains about a bad ref.
    assert!(!reason.contains("does not resolve"), "{reason}");
}

/// The chain is also wider than the six conventional names, so fewer
/// repositories reach the sentinel at all: the recorded HEAD of a remote that is
/// not `origin`, and whatever this user names their trunks.
#[test]
fn the_probe_chain_finds_a_non_origin_remote_head() {
    let fixture = Fixture::new("upstream-remote");
    let repo = fixture.trunkless_repo("forked", "develop");
    fixture.git(
        &repo,
        &["remote", "add", "upstream", fixture.repo.to_str().unwrap()],
    );
    fixture.git(&repo, &["fetch", "-q", "upstream"]);
    fixture.git(
        &repo,
        &[
            "symbolic-ref",
            "refs/remotes/upstream/HEAD",
            "refs/remotes/upstream/main",
        ],
    );

    assert_eq!(
        git::integration_ref(&repo, TIMEOUT).expect("probe"),
        Some("refs/remotes/upstream/HEAD".to_string())
    );
}

#[test]
fn the_probe_chain_falls_back_to_the_configured_default_branch() {
    let fixture = Fixture::new("default-branch");
    let repo = fixture.trunkless_repo("named", "develop");
    fixture.git(&repo, &["config", "init.defaultBranch", "develop"]);

    assert_eq!(
        git::integration_ref(&repo, TIMEOUT).expect("probe"),
        Some("refs/heads/develop".to_string())
    );
}

/// `status --porcelain` reports paths relative to the repository root whatever
/// directory git was run in, so joining them onto a checkout path that is a
/// subdirectory addressed nothing and every untracked file counted zero lines.
#[test]
fn line_counts_are_the_same_from_a_subdirectory_as_from_the_root() {
    let fixture = Fixture::new("subdir");
    let wt = fixture.worktree("wt", "wt");
    fixture.write(&wt, "pkg/untracked.txt", "one\ntwo\nthree\n");

    let from_root = change_set(&wt, "main", TIMEOUT).expect("change set");
    let from_subdir = change_set(&wt.join("pkg"), "main", TIMEOUT).expect("change set");

    assert_eq!(from_root.lines_added, 3, "{:?}", from_root.paths);
    assert_eq!(
        from_subdir.lines_added, from_root.lines_added,
        "the same worktree measured from a subdirectory: {:?}",
        from_subdir.paths
    );
    assert_eq!(from_subdir.paths, from_root.paths);
}

// ---------------------------------------------------------------------------
// What the predictor refuses to answer without asking git
// ---------------------------------------------------------------------------

/// The one that got away. `predict_pair` used to short-circuit a pair with no
/// shared path unless a side had a rename — and it decided that from `status`,
/// which only ever shows *uncommitted* renames. A worktree that had committed a
/// directory rename and was otherwise clean therefore short-circuited to a
/// conflict-free verdict, while `merge-tree` on the very same pair exits 1 with
/// `CONFLICT (directory rename suggested)`.
///
/// The pair is asked with an empty path list on purpose: that is exactly the
/// shape the intersection produces here, because one side changed `docs/*` and
/// `guide/*` and the other changed `docs/notes-c.md`.
#[test]
fn a_committed_directory_rename_is_predicted_even_with_no_shared_path() {
    let fixture = Fixture::new("dir-rename");
    let (a, b) = fixture.committed_directory_rename_pair();

    // Guard against the fixture going vacuous: the change sets really do share
    // nothing, so nothing but the predictor can catch this.
    let left = change_set(&a, "main", TIMEOUT).expect("change set");
    let right = change_set(&b, "main", TIMEOUT).expect("change set");
    let shared: Vec<&str> = left
        .path_set()
        .intersection(&right.path_set())
        .copied()
        .collect();
    assert!(shared.is_empty(), "fixture now shares paths: {shared:?}");

    let mut predictor = Predictor::new(TIMEOUT).expect("predictor");
    predictor.prime(&a).unwrap();
    predictor.prime(&b).unwrap();
    let prediction = predictor.predict_pair(&a, &b, &[]).expect("prediction");

    assert!(
        prediction.pair_conflict,
        "merge-tree's exit status is the authority and it says conflict: {prediction:?}"
    );
    assert!(
        prediction
            .verdicts
            .iter()
            .any(|(path, hit)| *hit && path == "guide/notes-c.md"),
        "the conflicting path was not reported: {prediction:?}"
    );
    assert!(
        prediction
            .conflict_types
            .iter()
            .any(|t| t.contains("directory rename")),
        "{prediction:?}"
    );
}

/// Two checkouts with no common ancestor get one answer regardless of how dirty
/// they are. The dirty path used to substitute the empty tree for the base,
/// which turns every shared path into an add/add and reports a confident
/// conflict on all of them, while the clean path let merge-tree refuse. The same
/// two branches therefore flipped between "unknown" and "everything conflicts"
/// on the strength of one stray untracked file.
#[test]
fn a_pair_with_no_common_ancestor_is_refused_however_dirty_it_is() {
    let fixture = Fixture::new("unrelated");
    let (a, b) = fixture.unrelated_history_pair();

    let refuse = |left: &std::path::Path, right: &std::path::Path| -> String {
        let mut predictor = Predictor::new(TIMEOUT).expect("predictor");
        predictor.prime(left).unwrap();
        predictor.prime(right).unwrap();
        predictor
            .predict_pair(left, right, &["conflict.txt".to_string()])
            .expect_err("a pair with no common ancestor cannot be predicted")
            .to_string()
    };

    let clean = refuse(&a, &b);
    // One untracked file is all it took to change the verdict before.
    fixture.write(&a, "stray.txt", "stray\n");
    let dirty = refuse(&a, &b);

    for err in [&clean, &dirty] {
        assert!(
            err.contains("no common ancestor") || err.contains("unrelated histories"),
            "unhelpful error: {err}"
        );
    }
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

/// The platform-independent half of the case above.
///
/// macOS refuses to create a file whose name is not valid UTF-8, so the on-disk
/// test cannot run there — but the parser is the part that has to get this
/// right, and it can be driven from bytes directly. These are real
/// `status --porcelain=v2 -z -uall` bytes, captured on ext4 from a worktree
/// holding `\xff.txt` and `\xfe.txt`: two files whose names differ only in the
/// byte that has to be replaced.
#[test]
fn the_digest_distinguishes_two_paths_from_captured_bytes() {
    let captured: &[u8] = b"? \xfe.txt\0? \xff.txt\0";

    let rendered = paths_of(&parse_status_v2(captured));
    assert_eq!(rendered.len(), 2, "{rendered:?}");
    assert!(
        rendered.iter().all(|p| p.contains('\u{FFFD}')),
        "the invalid byte must be replaced for display: {rendered:?}"
    );
    assert_ne!(
        rendered[0], rendered[1],
        "two different files rendered as one path, so they would be reported as \
         shared when neither worktree has the other's file: {rendered:?}"
    );

    // Stable across calls, or `status` output and `merge-tree` output would
    // stop matching each other for the same file.
    assert_eq!(paths_of(&parse_status_v2(captured)), rendered);

    // And a path that needs no replacement is left completely alone — the
    // disambiguating suffix must not appear on ordinary names.
    let plain = paths_of(&parse_status_v2(b"? src/main.rs\0"));
    assert_eq!(plain, vec!["src/main.rs".to_string()]);
}
