//! The discrimination this plugin exists for: two agents touching the same
//! file is normal and usually harmless; two agents about to produce a merge
//! conflict is not. A tool that cannot tell those apart is a nuisance, so these
//! tests assert both directions on real repositories.
//!
//! They also pin the read-only guarantee: after a snapshot, the worktree's real
//! index must be byte-identical and the repository's object store must not have
//! grown.

#[path = "fixtures.rs"]
mod fixtures;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use collide::collide::{
    analyse, apply_predictions, json_report, work_tree_root, Cycle, PairVerdicts,
};
use collide::config::Config;
use collide::git::{self, predict_conflict, Predictor};
use collide::model::{Checkout, FileVerdict, Report, Severity, WorkTrees};

use fixtures::{
    change_set, change_set_degraded, change_set_renamed, change_set_with_lines, checkout, Fixture,
};

const TIMEOUT: Duration = Duration::from_secs(30);

fn verdicts(left: &Path, right: &Path, paths: &[&str]) -> Vec<(String, bool)> {
    let owned: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
    predict_conflict(left, right, &owned, TIMEOUT).expect("prediction")
}

fn conflicting(verdicts: &[(String, bool)]) -> BTreeSet<&str> {
    verdicts
        .iter()
        .filter(|(_, hit)| *hit)
        .map(|(path, _)| path.as_str())
        .collect()
}

/// Every loose and packed object file under a repository's object store.
fn odb_files(repo: &Path) -> usize {
    fn walk(dir: &Path) -> usize {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        entries
            .flatten()
            .map(|entry| match entry.file_type() {
                Ok(t) if t.is_dir() => walk(&entry.path()),
                Ok(_) => 1,
                Err(_) => 0,
            })
            .sum()
    }
    walk(&repo.join(".git/objects"))
}

// ---------------------------------------------------------------------------
// The core discrimination
// ---------------------------------------------------------------------------

#[test]
fn same_file_edited_at_opposite_ends_is_an_overlap_not_a_conflict() {
    let fixture = Fixture::new("clean-overlap");
    let (a, b) = fixture.committed_clean_overlap_pair();

    let result = verdicts(&a, &b, &["shared.txt"]);
    assert_eq!(
        result,
        vec![("shared.txt".to_string(), false)],
        "a clean merge of a shared file must not be reported as a conflict"
    );
}

#[test]
fn committed_edits_to_the_same_line_are_a_conflict() {
    let fixture = Fixture::new("committed-conflict");
    let (a, b) = fixture.committed_conflict_pair();

    let result = verdicts(&a, &b, &["conflict.txt"]);
    assert_eq!(result, vec![("conflict.txt".to_string(), true)]);
}

#[test]
fn uncommitted_edits_to_the_same_line_are_a_conflict() {
    let fixture = Fixture::new("dirty-conflict");
    let (a, b) = fixture.uncommitted_conflict_pair();

    // Nothing is committed on either side, so this can only work through the
    // temp-index snapshot.
    let result = verdicts(&a, &b, &["conflict.txt"]);
    assert_eq!(result, vec![("conflict.txt".to_string(), true)]);
}

/// Regression: `merge-tree --write-tree --quiet` used to gate the expensive
/// `--name-only` run. It reports a *clean* merge for this pair, so every real
/// conflict of this shape silently degraded to a plain overlap. Built from the
/// temp-index snapshot of two real worktrees, because trees assembled by hand
/// do not reproduce it.
#[test]
fn a_conflict_survives_even_when_quiet_would_call_the_merge_clean() {
    let fixture = Fixture::new("quiet-trap");
    let (a, b) = fixture.quiet_trap_pair();

    let result = verdicts(&a, &b, &["conflict.txt"]);
    assert_eq!(
        result,
        vec![("conflict.txt".to_string(), true)],
        "the conflict was lost; see docs/git-plumbing.md, \"The --quiet trap\""
    );
}

/// Guards the fixture above against going vacuous. If this fails, git has
/// changed its behaviour — collide is fine either way, but the regression test
/// above would no longer be testing anything and the plumbing notes are stale.
#[test]
fn the_quiet_oracle_still_disagrees_with_name_only_on_real_snapshot_trees() {
    let fixture = Fixture::new("quiet-trap-guard");
    let (a, b) = fixture.quiet_trap_pair();

    let tree_a = fixture.snapshot_tree(&a);
    let tree_b = fixture.snapshot_tree(&b);
    let base = fixture.git(&a, &["rev-parse", "main^{tree}"]);
    let merge_base = format!("--merge-base={base}");

    let (quiet, _) = fixture.merge_tree(
        &a,
        &["--write-tree", "--quiet", &merge_base, &tree_a, &tree_b],
    );
    let (named, stdout) = fixture.merge_tree(
        &a,
        &[
            "--write-tree",
            "-z",
            "--name-only",
            &merge_base,
            &tree_a,
            &tree_b,
        ],
    );

    assert_eq!(named, 1, "--name-only must see the conflict");
    assert!(
        String::from_utf8_lossy(&stdout).contains("conflict.txt"),
        "--name-only did not name the conflicting file"
    );
    assert_eq!(
        quiet, 0,
        "--quiet now agrees with --name-only. git's behaviour changed; \
         re-check docs/git-plumbing.md, \"The --quiet trap\", before trusting it again"
    );
}

#[test]
fn uncommitted_edits_at_opposite_ends_are_not_a_conflict() {
    let fixture = Fixture::new("dirty-clean");
    let (a, b) = fixture.uncommitted_clean_pair();

    let result = verdicts(&a, &b, &["shared.txt"]);
    assert_eq!(result, vec![("shared.txt".to_string(), false)]);
}

#[test]
fn add_add_of_the_same_new_path_is_a_conflict() {
    let fixture = Fixture::new("add-add");
    let (a, b) = fixture.add_add_pair();

    let result = verdicts(&a, &b, &["brand-new.txt"]);
    assert_eq!(result, vec![("brand-new.txt".to_string(), true)]);
}

#[test]
fn untracked_add_add_is_predicted_too() {
    // The temp-index snapshot stages untracked files, which is the only reason
    // an add/add between two never-committed files can be predicted at all.
    let fixture = Fixture::new("add-add-dirty");
    let a = fixture.worktree("ua", "ua");
    let b = fixture.worktree("ub", "ub");
    fixture.write(&a, "untracked-both.txt", "from a\n");
    fixture.write(&b, "untracked-both.txt", "from b\n");

    let result = verdicts(&a, &b, &["untracked-both.txt"]);
    assert_eq!(result, vec![("untracked-both.txt".to_string(), true)]);
}

#[test]
fn rename_rename_of_the_same_file_is_a_conflict() {
    let fixture = Fixture::new("rename-rename");
    let (a, b) = fixture.rename_rename_pair();

    let result = verdicts(&a, &b, &["renamed.txt"]);
    assert!(
        !conflicting(&result).is_empty(),
        "rename/rename went unreported: {result:?}"
    );
}

#[test]
fn a_pair_with_no_shared_paths_is_still_checked_when_either_side_renamed() {
    let fixture = Fixture::new("rename-prefilter");
    let (a, b) = fixture.uncommitted_rename_pair();

    let mut predictor = Predictor::new(TIMEOUT).expect("predictor");
    predictor.prime(&a).unwrap();
    predictor.prime(&b).unwrap();

    // Empty path list: the free prefilter would normally short-circuit here.
    let prediction = predictor.predict_pair(&a, &b, &[]).expect("prediction");
    assert!(
        !prediction.verdicts.is_empty(),
        "the rename exception to the prefilter did not fire"
    );
    assert!(prediction.verdicts.iter().any(|(_, hit)| *hit));
}

#[test]
fn checkouts_from_different_repositories_are_refused() {
    let fixture = Fixture::new("foreign");
    let wt = fixture.worktree("wt", "wt");
    let foreign = fixture.foreign_repo("foreign");
    // Both have a `conflict.txt`, so a repo-blind implementation would happily
    // compare them.
    fixture.write(&wt, "conflict.txt", "local edit\nbeta\ngamma\n");
    fixture.write(&foreign, "conflict.txt", "foreign edit\nbeta\ngamma\n");

    let mut predictor = Predictor::new(TIMEOUT).expect("predictor");
    predictor.prime(&wt).unwrap();
    predictor.prime(&foreign).unwrap();

    let err = predictor
        .predict_pair(&wt, &foreign, &["conflict.txt".to_string()])
        .expect_err("comparing two repositories must be refused");
    assert!(err.to_string().contains("different repositories"), "{err}");
}

// ---------------------------------------------------------------------------
// Read-only guarantees
// ---------------------------------------------------------------------------

#[test]
fn snapshotting_leaves_the_real_index_byte_identical() {
    let fixture = Fixture::new("index-untouched");
    let (a, b) = fixture.uncommitted_conflict_pair();
    // Give the index something to lose: a staged change plus an untracked file.
    fixture.write(&a, "staged.txt", "staged\n");
    fixture.git(&a, &["add", "staged.txt"]);
    fixture.write(&a, "untracked.txt", "loose\n");

    let before = fixture.index_bytes(&a);
    assert!(!before.is_empty(), "fixture has no index to compare");

    let mut predictor = Predictor::new(TIMEOUT).expect("predictor");
    predictor.prime(&a).unwrap();
    predictor.prime(&b).unwrap();
    predictor
        .predict_pair(&a, &b, &["conflict.txt".to_string()])
        .expect("prediction");

    let after = fixture.index_bytes(&a);
    assert_eq!(before, after, "the worktree's real index was modified");

    // And nothing was staged or unstaged behind the user's back.
    let status = fixture.git(&a, &["status", "--porcelain"]);
    assert!(status.contains("A  staged.txt"), "{status}");
    assert!(status.contains("?? untracked.txt"), "{status}");
}

#[test]
fn prediction_does_not_grow_the_users_object_store() {
    let fixture = Fixture::new("odb-untouched");
    let (a, b) = fixture.uncommitted_conflict_pair();
    fixture.write(&a, "bulky.txt", &"payload\n".repeat(200));

    let before = odb_files(&fixture.repo);
    let mut predictor = Predictor::new(TIMEOUT).expect("predictor");
    predictor.prime(&a).unwrap();
    predictor.prime(&b).unwrap();
    // Force phase 2, which is the half that writes a merged tree and blobs.
    let prediction = predictor
        .predict_pair(&a, &b, &["conflict.txt".to_string()])
        .expect("prediction");
    assert!(prediction.verdicts.iter().any(|(_, hit)| *hit));

    assert_eq!(
        before,
        odb_files(&fixture.repo),
        "objects leaked into the user's repository"
    );
}

#[test]
fn temp_index_files_are_cleaned_up() {
    let fixture = Fixture::new("temp-cleanup");
    let (a, b) = fixture.uncommitted_conflict_pair();

    let scratch = {
        let mut predictor = Predictor::new(TIMEOUT).expect("predictor");
        let scratch = predictor.scratch_dir().to_path_buf();
        predictor.prime(&a).unwrap();
        predictor.prime(&b).unwrap();
        predictor
            .predict_pair(&a, &b, &["conflict.txt".to_string()])
            .expect("prediction");

        // While it is alive, the snapshot's temp index must already be gone —
        // and so must its `.lock` sibling, which git leaves behind on a crash.
        let leftovers: Vec<String> = std::fs::read_dir(&scratch)
            .expect("scratch exists during the run")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("index-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp index left behind: {leftovers:?}"
        );
        scratch
    };
    assert!(
        !scratch.exists(),
        "scratch directory {} outlived the predictor",
        scratch.display()
    );
}

#[test]
fn ignored_files_never_enter_a_change_set() {
    let fixture = Fixture::new("ignored");
    let wt = fixture.worktree("wt", "wt");
    fixture.ignored_files(&wt);
    fixture.write(&wt, "visible.txt", "kept\n");

    let set = git::change_set(&wt, "main", TIMEOUT).expect("change set");
    let paths: Vec<&str> = set.paths.iter().map(|p| p.path.as_str()).collect();
    assert!(paths.contains(&"visible.txt"), "{paths:?}");
    assert!(
        !paths
            .iter()
            .any(|p| p.ends_with(".log") || p.starts_with("ignored/")),
        "ignored paths leaked into the change set: {paths:?}"
    );
}

// ---------------------------------------------------------------------------
// The pure analysis pass
// ---------------------------------------------------------------------------

fn config() -> Config {
    Config {
        predict_conflicts: true,
        ..Config::default()
    }
}

/// One working tree per checkout, taken from the checkout path verbatim.
///
/// The synthetic fixtures use paths that do not exist (`/tmp/one`, `/tmp/two`),
/// and what they mean by that is "two different working trees". Saying so
/// directly keeps these tests independent of whatever happens to be on disk;
/// [`resolved_trees`] is for the fixtures where the answer has to come from git.
fn distinct_trees(checkouts: &[Checkout]) -> WorkTrees {
    let mut trees = WorkTrees::new();
    for checkout in checkouts {
        trees.insert(
            checkout.workspace_id.clone(),
            checkout.checkout_path.clone(),
        );
    }
    trees
}

/// Exactly what `gather_for` builds: every checkout's top level resolved from
/// disk.
fn resolved_trees(checkouts: &[Checkout]) -> WorkTrees {
    let mut trees = WorkTrees::new();
    for checkout in checkouts {
        trees.insert(
            checkout.workspace_id.clone(),
            work_tree_root(&checkout.checkout_path),
        );
    }
    trees
}

/// `git rev-parse --show-toplevel`, the answer this plugin's own resolution has
/// to agree with.
fn git_toplevel(path: &Path) -> PathBuf {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--path-format=absolute", "--show-toplevel"])
        .output()
        .expect("git rev-parse");
    assert!(
        out.status.success(),
        "git rev-parse --show-toplevel failed in {}: {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    let raw = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
    std::fs::canonicalize(&raw).unwrap_or(raw)
}

#[test]
fn analyse_never_pairs_checkouts_from_different_repos() {
    let checkouts = vec![
        checkout("one", Path::new("/tmp/one"), "/repo-a/.git"),
        checkout("two", Path::new("/tmp/two"), "/repo-b/.git"),
    ];
    let changes = vec![
        ("one".to_string(), change_set(&["shared.txt"])),
        ("two".to_string(), change_set(&["shared.txt"])),
    ];

    let report = analyse(&checkouts, &changes, &distinct_trees(&checkouts), &config());
    assert!(
        report.pairings.is_empty(),
        "two repos were paired: {:?}",
        report.pairings
    );
    assert!(report
        .statuses
        .iter()
        .all(|s| s.severity == Severity::Clean));
}

#[test]
fn analyse_pairs_within_a_repo_and_intersects_change_sets() {
    let checkouts = vec![
        checkout("one", Path::new("/tmp/one"), "/repo/.git"),
        checkout("two", Path::new("/tmp/two"), "/repo/.git"),
    ];
    let changes = vec![
        ("one".to_string(), change_set(&["a.txt", "shared.txt"])),
        ("two".to_string(), change_set(&["b.txt", "shared.txt"])),
    ];

    let report = analyse(&checkouts, &changes, &distinct_trees(&checkouts), &config());
    assert_eq!(report.pairings.len(), 1);
    let paths: Vec<&str> = report.pairings[0]
        .shared
        .iter()
        .map(|s| s.path.as_str())
        .collect();
    assert_eq!(paths, vec!["shared.txt"]);
    // Prediction has not run yet, so the verdict is honestly unknown.
    assert_eq!(report.pairings[0].shared[0].verdict, FileVerdict::Unknown);
}

#[test]
fn ignore_suffixes_drop_lockfiles_before_anything_counts_them() {
    let checkouts = vec![
        checkout("one", Path::new("/tmp/one"), "/repo/.git"),
        checkout("two", Path::new("/tmp/two"), "/repo/.git"),
    ];
    let changes = vec![
        ("one".to_string(), change_set(&["Cargo.lock"])),
        ("two".to_string(), change_set(&["Cargo.lock"])),
    ];

    let report = analyse(&checkouts, &changes, &distinct_trees(&checkouts), &config());
    assert!(
        report.pairings.is_empty(),
        "a lockfile overlap was reported: {:?}",
        report.pairings
    );
}

#[test]
fn unpairable_checkouts_are_excluded() {
    let checkouts = vec![
        checkout("one", Path::new("/tmp/one"), "/repo/.git"),
        checkout("two", Path::new("/tmp/two"), "/repo/.git"),
    ];
    let mut unborn = change_set(&["shared.txt"]);
    unborn.degraded = true;
    unborn.degraded_reason = Some(format!("{}: `x` has no commits yet", git::DEGRADED_UNBORN));
    let changes = vec![
        ("one".to_string(), change_set(&["shared.txt"])),
        ("two".to_string(), unborn),
    ];

    let report = analyse(&checkouts, &changes, &distinct_trees(&checkouts), &config());
    assert!(report.pairings.is_empty());
}

#[test]
fn runaway_thresholds_raise_severity_without_an_overlap() {
    let checkouts = vec![checkout("one", Path::new("/tmp/one"), "/repo/.git")];
    // Volume lives on the paths, so that filtering a path also removes the
    // lines it contributed.
    let changes = vec![(
        "one".to_string(),
        change_set_with_lines(&[("a.txt", 10_000)]),
    )];

    let report = analyse(&checkouts, &changes, &distinct_trees(&checkouts), &config());
    assert_eq!(report.statuses[0].severity, Severity::Runaway);
    assert!(report.statuses[0].runaway);
}

#[test]
fn conflict_outranks_runaway_and_overlap() {
    let checkouts = vec![
        checkout("one", Path::new("/tmp/one"), "/repo/.git"),
        checkout("two", Path::new("/tmp/two"), "/repo/.git"),
    ];
    let noisy = change_set_with_lines(&[("shared.txt", 10_000), ("other.txt", 0)]);
    let changes = vec![
        ("one".to_string(), noisy),
        ("two".to_string(), change_set(&["shared.txt"])),
    ];

    let mut report = analyse(&checkouts, &changes, &distinct_trees(&checkouts), &config());
    apply_predictions(
        &mut report,
        &[PairVerdicts {
            left_workspace_id: "one".to_string(),
            right_workspace_id: "two".to_string(),
            verdicts: vec![("shared.txt".to_string(), true)],
            failed: false,
            approximate: false,
        }],
        &changes,
        &config(),
    );

    let one = status_of(&report, "one");
    assert_eq!(one.severity, Severity::Conflict);
    assert_eq!(one.conflict_count, 1);
    assert_eq!(one.overlap_count, 0);
    assert!(one.runaway, "the runaway fact is still reported");
    assert_eq!(collide::render::badge(one), "✘ 1");
    assert_eq!(one.severity.token_name(), "collide_conflict");
}

#[test]
fn a_clean_prediction_downgrades_unknown_to_overlap() {
    let checkouts = vec![
        checkout("one", Path::new("/tmp/one"), "/repo/.git"),
        checkout("two", Path::new("/tmp/two"), "/repo/.git"),
    ];
    let changes = vec![
        ("one".to_string(), change_set(&["shared.txt"])),
        ("two".to_string(), change_set(&["shared.txt"])),
    ];

    let mut report = analyse(&checkouts, &changes, &distinct_trees(&checkouts), &config());
    apply_predictions(
        &mut report,
        &[PairVerdicts {
            left_workspace_id: "one".to_string(),
            right_workspace_id: "two".to_string(),
            verdicts: vec![("shared.txt".to_string(), false)],
            failed: false,
            approximate: false,
        }],
        &changes,
        &config(),
    );

    assert_eq!(report.pairings[0].shared[0].verdict, FileVerdict::Overlap);
    assert_eq!(status_of(&report, "one").severity, Severity::Overlap);
    assert_eq!(collide::render::badge(status_of(&report, "one")), "⧉ 1");
}

#[test]
fn a_failed_prediction_leaves_the_verdict_unknown() {
    let checkouts = vec![
        checkout("one", Path::new("/tmp/one"), "/repo/.git"),
        checkout("two", Path::new("/tmp/two"), "/repo/.git"),
    ];
    let changes = vec![
        ("one".to_string(), change_set(&["shared.txt"])),
        ("two".to_string(), change_set(&["shared.txt"])),
    ];

    let mut report = analyse(&checkouts, &changes, &distinct_trees(&checkouts), &config());
    apply_predictions(
        &mut report,
        &[PairVerdicts {
            left_workspace_id: "one".to_string(),
            right_workspace_id: "two".to_string(),
            verdicts: Vec::new(),
            failed: true,
            approximate: false,
        }],
        &changes,
        &config(),
    );

    assert_eq!(report.pairings[0].shared[0].verdict, FileVerdict::Unknown);
}

#[test]
fn json_report_is_stable_and_documented() {
    let checkouts = vec![
        checkout("one", Path::new("/tmp/one"), "/repo/.git"),
        checkout("two", Path::new("/tmp/two"), "/repo/.git"),
    ];
    let changes = vec![
        ("one".to_string(), change_set(&["shared.txt"])),
        ("two".to_string(), change_set(&["shared.txt"])),
    ];
    let mut report = analyse(&checkouts, &changes, &distinct_trees(&checkouts), &config());
    apply_predictions(
        &mut report,
        &[PairVerdicts {
            left_workspace_id: "one".to_string(),
            right_workspace_id: "two".to_string(),
            verdicts: vec![("shared.txt".to_string(), true)],
            failed: false,
            approximate: false,
        }],
        &changes,
        &config(),
    );

    let json = json_report(&Cycle {
        report,
        changes,
        notes: vec!["a note".to_string()],
    });

    // Every field in the schema block on `collide::json_report`, asserted once.
    // The block documents the contract a `--json` consumer reads; a field that
    // is documented and not asserted can disappear without a test noticing, and
    // that is precisely what happened to the fields the version was bumped for.
    assert_eq!(json["schema"], 2);

    assert_eq!(json["checkouts"].as_array().unwrap().len(), 2);
    let one = &json["checkouts"][0];
    assert_eq!(one["workspace_id"], "one");
    assert_eq!(one["label"], "one");
    assert_eq!(one["repo_key"], "/repo/.git");
    assert_eq!(one["repo_root"], "/tmp/one");
    assert_eq!(one["checkout_path"], "/tmp/one");
    assert_eq!(one["branch"], "one");
    assert!(one["agent"].is_null());
    assert_eq!(one["is_linked_worktree"], true);
    assert_eq!(one["changed_files"], 1);
    assert_eq!(one["lines_added"], 0);
    assert_eq!(one["lines_removed"], 0);
    assert_eq!(one["has_rename"], false);
    assert_eq!(one["degraded"], false);
    assert!(one["degraded_reason"].is_null());

    let pairing = &json["pairings"][0];
    assert_eq!(pairing["left"], "one");
    assert_eq!(pairing["right"], "two");
    assert_eq!(pairing["conflict_count"], 1);
    assert_eq!(pairing["unknown_count"], 0);
    assert_eq!(pairing["approximate"], false);
    assert_eq!(pairing["shared"][0]["path"], "shared.txt");
    assert_eq!(pairing["shared"][0]["verdict"], "conflict");

    let status = &json["statuses"][0];
    assert_eq!(status["workspace_id"], "one");
    assert_eq!(status["severity"], "conflict");
    assert_eq!(status["token"], "collide_conflict");
    assert_eq!(status["badge"], "\u{2718} 1");
    assert_eq!(status["conflict_count"], 1);
    assert_eq!(status["overlap_count"], 0);
    assert_eq!(status["unknown_count"], 0);
    assert_eq!(status["runaway"], false);
    assert_eq!(status["lines_changed"], 0);
    assert_eq!(status["changed_files"], 1);

    assert_eq!(json["notes"][0], "a note");
}

/// The value the schema bump was for. `unknown` is a severity a consumer that
/// matched exhaustively on the old four has never seen, so it belongs in the
/// documented output and in a test.
#[test]
fn json_reports_the_unknown_severity_the_schema_was_bumped_for() {
    let checkouts = vec![
        checkout("one", Path::new("/tmp/one"), "/repo/.git"),
        checkout("two", Path::new("/tmp/two"), "/repo/.git"),
    ];
    let changes = vec![
        ("one".to_string(), change_set(&["shared.txt"])),
        ("two".to_string(), change_set(&["shared.txt"])),
    ];
    let mut report = analyse(&checkouts, &changes, &distinct_trees(&checkouts), &config());
    apply_predictions(
        &mut report,
        &[PairVerdicts {
            left_workspace_id: "one".to_string(),
            right_workspace_id: "two".to_string(),
            verdicts: Vec::new(),
            failed: true,
            approximate: true,
        }],
        &changes,
        &config(),
    );

    let json = json_report(&Cycle {
        report,
        changes,
        notes: Vec::new(),
    });

    assert_eq!(json["statuses"][0]["severity"], "unknown");
    assert_eq!(json["statuses"][0]["token"], "collide_unknown");
    assert_eq!(json["statuses"][0]["badge"], "? 1");
    assert_eq!(json["statuses"][0]["unknown_count"], 1);
    assert_eq!(json["pairings"][0]["unknown_count"], 1);
    assert_eq!(json["pairings"][0]["shared"][0]["verdict"], "unknown");
    // A failed prediction says nothing about the merge base either.
    assert_eq!(json["pairings"][0]["approximate"], false);
}

fn status_of<'a>(report: &'a Report, id: &str) -> &'a collide::model::WorkspaceStatus {
    report
        .statuses
        .iter()
        .find(|s| s.workspace_id == id)
        .expect("status")
}

// ---------------------------------------------------------------------------
// End to end, on real repositories
// ---------------------------------------------------------------------------

#[test]
fn end_to_end_separates_the_conflicting_pair_from_the_clean_one() {
    let fixture = Fixture::new("end-to-end");
    let (ca, cb) = fixture.committed_conflict_pair();
    let (oa, ob) = fixture.committed_clean_overlap_pair();
    let foreign = fixture.foreign_repo("foreign");
    fixture.write(&foreign, "conflict.txt", "foreign\nbeta\ngamma\n");

    let key = git::repo_key(&fixture.repo, TIMEOUT).unwrap();
    let foreign_key = git::repo_key(&foreign, TIMEOUT).unwrap();
    let checkouts = vec![
        checkout("ca", &ca, &key.0),
        checkout("cb", &cb, &key.0),
        checkout("oa", &oa, &key.0),
        checkout("ob", &ob, &key.0),
        checkout("foreign", &foreign, &foreign_key.0),
    ];

    let cycle = collide::collide::gather_for(checkouts, &config()).expect("gather");
    assert!(
        cycle.notes.is_empty(),
        "unexpected notes: {:?}",
        cycle.notes
    );

    let conflict_pair = cycle
        .report
        .pairings
        .iter()
        .find(|p| [p.left_workspace_id.as_str(), p.right_workspace_id.as_str()] == ["ca", "cb"])
        .expect("the conflicting pair was not reported at all");
    assert_eq!(conflict_pair.conflicts(), 1);
    assert_eq!(conflict_pair.shared[0].path, "conflict.txt");

    let overlap_pair = cycle
        .report
        .pairings
        .iter()
        .find(|p| [p.left_workspace_id.as_str(), p.right_workspace_id.as_str()] == ["oa", "ob"])
        .expect("the clean overlap was not reported");
    assert_eq!(
        overlap_pair.conflicts(),
        0,
        "a clean merge was reported as a conflict"
    );
    assert_eq!(overlap_pair.shared[0].verdict, FileVerdict::Overlap);

    // The foreign checkout shares a file name with everything and must still be
    // paired with nothing.
    assert!(
        !cycle
            .report
            .pairings
            .iter()
            .any(|p| p.left_workspace_id == "foreign" || p.right_workspace_id == "foreign"),
        "the foreign repo was paired"
    );

    assert_eq!(status_of(&cycle.report, "ca").severity, Severity::Conflict);
    assert_eq!(status_of(&cycle.report, "oa").severity, Severity::Overlap);
    assert_eq!(
        status_of(&cycle.report, "foreign").severity,
        Severity::Clean
    );
}

#[test]
fn end_to_end_tolerates_degenerate_worktrees() {
    let fixture = Fixture::new("degenerate");
    let (ca, cb) = fixture.committed_conflict_pair();
    let detached = fixture.detached_worktree("detached");
    let unborn = fixture.unborn_worktree("unborn", "fresh");
    let deleted = fixture.deleted_branch_worktree("doomed", "doomed");
    // The detached worktree touches the same file as the conflicting pair.
    fixture.write(&detached, "conflict.txt", "DETACHED\nbeta\ngamma\n");
    fixture.write(&unborn, "conflict.txt", "unborn\n");
    fixture.write(&deleted, "conflict.txt", "deleted\n");

    let key = git::repo_key(&fixture.repo, TIMEOUT).unwrap();
    let checkouts = vec![
        checkout("ca", &ca, &key.0),
        checkout("cb", &cb, &key.0),
        checkout("detached", &detached, &key.0),
        checkout("unborn", &unborn, &key.0),
        checkout("deleted", &deleted, &key.0),
    ];

    let cycle = collide::collide::gather_for(checkouts, &config()).expect("gather");

    // A detached HEAD is a first-class citizen.
    assert!(
        cycle
            .report
            .pairings
            .iter()
            .any(|p| p.left_workspace_id == "detached" || p.right_workspace_id == "detached"),
        "the detached worktree was dropped"
    );
    assert_eq!(
        status_of(&cycle.report, "detached").severity,
        Severity::Conflict
    );

    // Commitless worktrees are reported but never paired.
    for id in ["unborn", "deleted"] {
        assert!(
            !cycle
                .report
                .pairings
                .iter()
                .any(|p| p.left_workspace_id == id || p.right_workspace_id == id),
            "{id} was paired despite having no commit"
        );
        assert_eq!(status_of(&cycle.report, id).severity, Severity::Clean);
    }

    // And the whole run still says nothing about the user's repo being touched.
    let status = fixture.git(&ca, &["status", "--porcelain"]);
    assert!(status.is_empty(), "worktree was modified: {status}");
}

// ---------------------------------------------------------------------------
// Checkout enrichment: branch and base ref
// ---------------------------------------------------------------------------

#[test]
fn gather_fills_in_the_branch_and_reports_none_only_when_detached() {
    let fixture = Fixture::new("branch-fill");
    let attached = fixture.worktree("feature", "fix/tier-promotion-scope");
    let detached = fixture.detached_worktree("detached");

    let key = git::repo_key(&fixture.repo, TIMEOUT).unwrap();
    // Deliberately hand in the wrong branch, as herdr does when it has none:
    // the git lookup is authoritative for the path we were given.
    let mut checkouts = vec![
        checkout("main", &fixture.repo, &key.0),
        checkout("feature", &attached, &key.0),
        checkout("detached", &detached, &key.0),
    ];
    for c in &mut checkouts {
        c.branch = None;
    }

    let cycle = collide::collide::gather_for(checkouts, &config()).expect("gather");
    let branch_of = |id: &str| {
        cycle
            .report
            .checkouts
            .iter()
            .find(|c| c.workspace_id == id)
            .and_then(|c| c.branch.clone())
    };

    assert_eq!(branch_of("main").as_deref(), Some("main"));
    assert_eq!(
        branch_of("feature").as_deref(),
        Some("fix/tier-promotion-scope"),
        "an attached HEAD must report its branch, not `(detached)`"
    );
    assert_eq!(
        branch_of("detached"),
        None,
        "a genuinely detached HEAD must stay None"
    );
}

#[test]
fn a_configured_base_ref_wins_over_the_probing_chain() {
    let fixture = Fixture::new("base-ref");
    let wt = fixture.worktree("wt", "wt");
    fixture.write(&wt, "conflict.txt", "committed\nbeta\ngamma\n");
    fixture.commit_all(&wt, "committed since main");

    let key = git::repo_key(&fixture.repo, TIMEOUT).unwrap();
    let checkouts = || vec![checkout("wt", &wt, &key.0)];

    // Default: the probing chain finds `refs/heads/main`, so the commit above
    // shows up as a committed-since-base change.
    let default_run = collide::collide::gather_for(checkouts(), &config()).expect("gather");
    let paths: Vec<&str> = default_run
        .report
        .change_set("wt")
        .expect("change set")
        .paths
        .iter()
        .map(|p| p.path.as_str())
        .collect();
    assert_eq!(
        paths,
        vec!["conflict.txt"],
        "probing chain did not use main"
    );

    // Configured: measured against the checkout's own branch, so nothing is
    // committed-since-base. Only an honoured base_ref can produce this.
    let configured = Config {
        base_ref: "refs/heads/wt".to_string(),
        ..config()
    };
    let pinned = collide::collide::gather_for(checkouts(), &configured).expect("gather");
    let set = pinned.report.change_set("wt").expect("change set");
    assert!(
        set.paths.is_empty(),
        "base_ref was ignored; still measuring against the probed ref: {:?}",
        set.paths
    );
    assert!(!set.degraded, "{:?}", set.degraded_reason);
}

#[test]
fn a_configured_base_ref_that_does_not_resolve_degrades_rather_than_falling_back() {
    let fixture = Fixture::new("base-ref-missing");
    let wt = fixture.worktree("wt", "wt");
    fixture.write(&wt, "conflict.txt", "committed\nbeta\ngamma\n");
    fixture.commit_all(&wt, "committed since main");

    let key = git::repo_key(&fixture.repo, TIMEOUT).unwrap();
    let configured = Config {
        base_ref: "refs/heads/no-such-ref".to_string(),
        ..config()
    };
    let cycle = collide::collide::gather_for(vec![checkout("wt", &wt, &key.0)], &configured)
        .expect("gather");

    let set = cycle.report.change_set("wt").expect("change set");
    assert!(
        set.degraded,
        "a bad base_ref must be reported, not silently swapped for a probed one"
    );
    assert!(set
        .degraded_reason
        .as_deref()
        .unwrap()
        .contains(git::DEGRADED_MISSING_BASE_REF));
}

#[test]
fn base_ref_for_only_probes_at_the_default() {
    let fixture = Fixture::new("base-ref-unit");
    let wt = fixture.worktree("wt", "wt");

    // At the default the chain probes and lands on a ref that exists here.
    let probed = collide::collide::base_ref_for(&wt, &config());
    assert_eq!(probed, "refs/heads/main");

    // Anything else is passed through untouched.
    let pinned = Config {
        base_ref: "refs/heads/anything".to_string(),
        ..config()
    };
    assert_eq!(
        collide::collide::base_ref_for(&wt, &pinned),
        "refs/heads/anything"
    );
}

// ---------------------------------------------------------------------------
// Wrong answers that used to look like right ones
//
// Each test below pins a case where the analysis produced a confident, ordinary
// looking result that was not true. They are grouped because they share a
// shape: a failure, an exclusion, or a double count that reached the badge
// wearing the costume of a normal reading.
// ---------------------------------------------------------------------------

/// A prediction that could not run must not be reported as "merges cleanly".
/// The overlap badge is a claim about the merge; a failed prediction has not
/// earned it, and the two used to be indistinguishable in the sidebar.
#[test]
fn a_pair_whose_prediction_failed_is_unknown_and_never_an_overlap() {
    let checkouts = vec![
        checkout("one", Path::new("/tmp/one"), "/repo/.git"),
        checkout("two", Path::new("/tmp/two"), "/repo/.git"),
    ];
    let changes = vec![
        ("one".to_string(), change_set(&["shared.txt"])),
        ("two".to_string(), change_set(&["shared.txt"])),
    ];

    let mut report = analyse(&checkouts, &changes, &distinct_trees(&checkouts), &config());
    // `failed` is exactly what `predict_all` builds when the git call errors.
    let failed = vec![PairVerdicts {
        left_workspace_id: "one".to_string(),
        right_workspace_id: "two".to_string(),
        verdicts: Vec::new(),
        failed: true,
        approximate: false,
    }];
    apply_predictions(&mut report, &failed, &changes, &config());

    assert_eq!(report.pairings[0].shared[0].verdict, FileVerdict::Unknown);
    for id in ["one", "two"] {
        let status = status_of(&report, id);
        assert_eq!(status.severity, Severity::Unknown);
        assert_eq!(status.severity.token_name(), "collide_unknown");
        assert_eq!(status.unknown_count, 1);
        assert_eq!(
            status.overlap_count, 0,
            "an unknown verdict must not be counted as an overlap"
        );
    }
}

/// A successful prediction that finds the file clean is a genuine overlap, so
/// the test above cannot pass by accident.
#[test]
fn a_pair_whose_prediction_succeeded_and_found_nothing_is_a_real_overlap() {
    let checkouts = vec![
        checkout("one", Path::new("/tmp/one"), "/repo/.git"),
        checkout("two", Path::new("/tmp/two"), "/repo/.git"),
    ];
    let changes = vec![
        ("one".to_string(), change_set(&["shared.txt"])),
        ("two".to_string(), change_set(&["shared.txt"])),
    ];

    let mut report = analyse(&checkouts, &changes, &distinct_trees(&checkouts), &config());
    let clean = vec![PairVerdicts {
        left_workspace_id: "one".to_string(),
        right_workspace_id: "two".to_string(),
        verdicts: vec![("shared.txt".to_string(), false)],
        failed: false,
        approximate: false,
    }];
    apply_predictions(&mut report, &clean, &changes, &config());

    assert_eq!(report.pairings[0].shared[0].verdict, FileVerdict::Overlap);
    assert_eq!(status_of(&report, "one").severity, Severity::Overlap);
    assert_eq!(status_of(&report, "one").unknown_count, 0);
}

/// A checkout whose git pass failed carries no paths, which used to make it
/// indistinguishable from a checkout with nothing to report.
#[test]
fn a_checkout_that_could_not_be_read_is_not_reported_as_clean() {
    let checkouts = vec![
        checkout("healthy", Path::new("/tmp/healthy"), "/repo/.git"),
        checkout("unreadable", Path::new("/tmp/unreadable"), "/repo/.git"),
    ];
    let changes = vec![
        ("healthy".to_string(), change_set(&[])),
        (
            "unreadable".to_string(),
            change_set_degraded(&format!("{}: permission denied", git::DEGRADED_UNREADABLE)),
        ),
    ];

    let report = analyse(&checkouts, &changes, &distinct_trees(&checkouts), &config());
    assert_eq!(status_of(&report, "healthy").severity, Severity::Clean);
    assert_eq!(status_of(&report, "unreadable").severity, Severity::Unknown);
}

/// A workspace with no entry in `changes` at all is the same failure by another
/// route, and must not default to clean either.
#[test]
fn a_checkout_with_no_change_set_at_all_is_unknown() {
    let checkouts = vec![checkout("orphan", Path::new("/tmp/orphan"), "/repo/.git")];
    let report = analyse(&checkouts, &[], &distinct_trees(&checkouts), &config());
    assert_eq!(status_of(&report, "orphan").severity, Severity::Unknown);
}

/// Ignored paths must take their line volume with them. A `package-lock.json`
/// the plugin has decided not to look at cannot be allowed to paint a runaway
/// badge on its own.
#[test]
fn an_ignored_path_takes_its_line_count_with_it() {
    let checkouts = vec![checkout("one", Path::new("/tmp/one"), "/repo/.git")];
    let changes = vec![(
        "one".to_string(),
        change_set_with_lines(&[("package-lock.json", 90_000)]),
    )];

    let report = analyse(&checkouts, &changes, &distinct_trees(&checkouts), &config());
    let status = status_of(&report, "one");
    assert!(
        !status.runaway,
        "a change set consisting only of ignored paths cannot be a runaway"
    );
    assert_eq!(status.severity, Severity::Clean);
    assert_eq!(status.lines_changed, 0);
}

/// The same volume on a path that is *not* ignored still trips the threshold,
/// so the test above is measuring the filter rather than a broken threshold.
#[test]
fn the_same_volume_on_a_counted_path_is_still_a_runaway() {
    let checkouts = vec![checkout("one", Path::new("/tmp/one"), "/repo/.git")];
    let changes = vec![(
        "one".to_string(),
        change_set_with_lines(&[("src/generated.rs", 90_000)]),
    )];

    let report = analyse(&checkouts, &changes, &distinct_trees(&checkouts), &config());
    assert!(status_of(&report, "one").runaway);
    assert_eq!(status_of(&report, "one").severity, Severity::Runaway);
}

/// Both halves of a rename belong in the change set for pairing, and one rename
/// is still one changed file. Counting both halved the runaway threshold.
#[test]
fn a_rename_counts_as_one_changed_file_not_two() {
    let checkouts = vec![checkout("one", Path::new("/tmp/one"), "/repo/.git")];
    // 21 renames: 42 paths, but only 21 files. The default threshold is 40.
    let changes = vec![("one".to_string(), change_set_renamed(21))];

    let report = analyse(&checkouts, &changes, &distinct_trees(&checkouts), &config());
    let status = status_of(&report, "one");
    assert_eq!(status.changed_files, 21);
    assert!(
        !status.runaway,
        "21 renamed files must not trip a 40-file threshold"
    );

    // And the threshold still works: 41 renamed files is 41 changed files.
    let many = vec![("one".to_string(), change_set_renamed(41))];
    let report = analyse(&checkouts, &many, &distinct_trees(&checkouts), &config());
    assert!(status_of(&report, "one").runaway);
}

/// Two herdr workspaces can be opened on one directory, and one checkout can
/// sit inside another. Either way git reports one change set twice, so every
/// changed file looks shared and the pair badges a collision that is not real.
#[test]
fn two_workspaces_on_the_same_tree_are_never_paired() {
    let fixture = Fixture::new("same-tree");
    let wt = fixture.worktree("wt", "wt");
    let nested = wt.join("src");
    std::fs::create_dir_all(&nested).expect("nested dir");

    let same = vec![
        checkout("outer", &wt, "/repo/.git"),
        checkout("again", &wt, "/repo/.git"),
    ];
    let changes = vec![
        ("outer".to_string(), change_set(&["src/a.rs", "src/b.rs"])),
        ("again".to_string(), change_set(&["src/a.rs", "src/b.rs"])),
    ];
    let report = analyse(&same, &changes, &resolved_trees(&same), &config());
    assert!(
        report.pairings.is_empty(),
        "one working tree cannot collide with itself"
    );

    let inside = vec![
        checkout("outer", &wt, "/repo/.git"),
        checkout("inner", &nested, "/repo/.git"),
    ];
    let changes = vec![
        ("outer".to_string(), change_set(&["src/a.rs"])),
        ("inner".to_string(), change_set(&["src/a.rs"])),
    ];
    let report = analyse(&inside, &changes, &resolved_trees(&inside), &config());
    assert!(
        report.pairings.is_empty(),
        "a checkout nested inside another reports the same change set"
    );
}

/// The other half, and the one a sibling-worktree fixture cannot reach: a linked
/// worktree *inside* the main worktree's directory, which is the `.worktrees/`
/// layout most agent-per-worktree setups use.
///
/// Its path sits under the main worktree's, so a same-tree test written as a
/// path-prefix comparison called them one tree and stopped comparing them. They
/// are not one tree — separate HEAD, separate index, separate change set — and
/// the main worktree then badged clean while conflicting head-on with every
/// agent living inside it. Nothing about that was visible: no error, no note,
/// and an empty badge, which the daemon reads as "clear the token".
#[test]
fn a_worktree_nested_inside_another_is_still_compared_with_it() {
    let fixture = Fixture::new("nested-worktree");
    let api = fixture.nested_worktree("api", "feature/api");
    let ui = fixture.nested_worktree("ui", "feature/ui");
    let main = fixture.repo.clone();

    // All three change the same line of the same file: three real conflicts.
    for (tree, text) in [(&api, "api\n"), (&ui, "ui\n"), (&main, "main\n")] {
        std::fs::write(tree.join("conflict.txt"), text).expect("write");
    }
    fixture.commit_all(&api, "api");
    fixture.commit_all(&ui, "ui");

    let checkouts = vec![
        checkout("main", &main, "unused"),
        checkout("api", &api, "unused"),
        checkout("ui", &ui, "unused"),
    ];
    let cycle = collide::collide::gather_for(checkouts, &config()).expect("cycle");

    // The nesting is real: `api` lives underneath `main`'s directory.
    assert!(api.starts_with(&main), "the fixture is not nested");

    // Normalised, because which side is "left" is an artefact of the input
    // order and not what this test is about.
    let mut pairs: Vec<String> = cycle
        .report
        .pairings
        .iter()
        .map(|p| {
            let mut ends = [p.left_workspace_id.as_str(), p.right_workspace_id.as_str()];
            ends.sort();
            format!("{} <-> {}", ends[0], ends[1])
        })
        .collect();
    pairs.sort();
    assert_eq!(
        pairs,
        vec!["api <-> main", "api <-> ui", "main <-> ui"],
        "every pair must be compared, notes: {:?}",
        cycle.notes
    );

    // And the main worktree is not reported clean.
    let main_status = status_of(&cycle.report, "main");
    assert_ne!(
        main_status.severity,
        Severity::Clean,
        "the main worktree conflicts with both of its nested worktrees"
    );
    assert_eq!(main_status.severity, Severity::Conflict);
    assert!(!collide::render::badge(main_status).is_empty());
}

/// The resolution the guard above depends on, checked against git rather than
/// against itself. `work_tree_root` is a filesystem walk, not a `git rev-parse`
/// call, so this is what stops it drifting away from what git actually says.
#[test]
fn resolved_work_trees_match_git_rev_parse() {
    let fixture = Fixture::new("toplevels");
    let nested = fixture.nested_worktree("api", "feature/api");
    let sibling = fixture.worktree("beside", "beside");
    let subdir_of_main = fixture.repo.join("pkg");
    let subdir_of_nested = nested.join("pkg");
    std::fs::create_dir_all(&subdir_of_main).expect("dir");
    std::fs::create_dir_all(&subdir_of_nested).expect("dir");

    for path in [
        fixture.repo.clone(),
        nested.clone(),
        sibling.clone(),
        subdir_of_main.clone(),
        subdir_of_nested.clone(),
    ] {
        assert_eq!(
            work_tree_root(&path),
            git_toplevel(&path),
            "work_tree_root disagrees with git for {}",
            path.display()
        );
    }

    // The two facts the pairing pass actually needs out of that.
    assert_eq!(
        work_tree_root(&subdir_of_main),
        work_tree_root(&fixture.repo),
        "a subdirectory is the same working tree"
    );
    assert_ne!(
        work_tree_root(&nested),
        work_tree_root(&fixture.repo),
        "a nested linked worktree is a different working tree"
    );
}

/// A rename can conflict on a path that appears under a different name in each
/// change set, so an empty path intersection is not proof that a pair is safe.
/// The prefilter used to drop those pairs before prediction ever saw them,
/// which made the escape hatch in `git::Predictor::predict_pair` unreachable.
#[test]
fn a_pair_with_no_shared_path_is_still_predicted_when_either_side_renamed() {
    let checkouts = vec![
        checkout("one", Path::new("/tmp/one"), "/repo/.git"),
        checkout("two", Path::new("/tmp/two"), "/repo/.git"),
    ];
    let mut renamed = change_set(&["only-here.rs"]);
    renamed.has_rename = true;
    let changes = vec![
        ("one".to_string(), renamed),
        ("two".to_string(), change_set(&["only-there.rs"])),
    ];

    let report = analyse(&checkouts, &changes, &distinct_trees(&checkouts), &config());
    assert_eq!(
        report.pairings.len(),
        1,
        "a renaming side must still be handed to prediction"
    );
    assert!(report.pairings[0].shared.is_empty());

    // With neither side renaming, the pair is dropped for free as before.
    let plain = vec![
        ("one".to_string(), change_set(&["only-here.rs"])),
        ("two".to_string(), change_set(&["only-there.rs"])),
    ];
    assert!(
        analyse(&checkouts, &plain, &distinct_trees(&checkouts), &config())
            .pairings
            .is_empty()
    );
}

/// The rename probe must stay invisible when it finds nothing: a pairing with
/// no shared files is noise in the pane.
#[test]
fn a_rename_probe_that_finds_nothing_leaves_no_pairing_behind() {
    let checkouts = vec![
        checkout("one", Path::new("/tmp/one"), "/repo/.git"),
        checkout("two", Path::new("/tmp/two"), "/repo/.git"),
    ];
    let mut renamed = change_set(&["only-here.rs"]);
    renamed.has_rename = true;
    let changes = vec![
        ("one".to_string(), renamed),
        ("two".to_string(), change_set(&["only-there.rs"])),
    ];

    let mut report = analyse(&checkouts, &changes, &distinct_trees(&checkouts), &config());
    let nothing = vec![PairVerdicts {
        left_workspace_id: "one".to_string(),
        right_workspace_id: "two".to_string(),
        verdicts: Vec::new(),
        failed: false,
        approximate: false,
    }];
    apply_predictions(&mut report, &nothing, &changes, &config());
    assert!(report.pairings.is_empty());
}

/// A forced merge base makes the verdict an approximation, and the user is
/// entitled to know that rather than being handed it as final.
#[test]
fn an_approximate_prediction_is_carried_through_to_the_pairing() {
    let checkouts = vec![
        checkout("one", Path::new("/tmp/one"), "/repo/.git"),
        checkout("two", Path::new("/tmp/two"), "/repo/.git"),
    ];
    let changes = vec![
        ("one".to_string(), change_set(&["shared.txt"])),
        ("two".to_string(), change_set(&["shared.txt"])),
    ];

    let mut report = analyse(&checkouts, &changes, &distinct_trees(&checkouts), &config());
    assert!(!report.pairings[0].approximate);
    let forced = vec![PairVerdicts {
        left_workspace_id: "one".to_string(),
        right_workspace_id: "two".to_string(),
        verdicts: vec![("shared.txt".to_string(), true)],
        failed: false,
        approximate: true,
    }];
    apply_predictions(&mut report, &forced, &changes, &config());
    assert!(report.pairings[0].approximate);
}

/// `ignore_suffixes` is a suffix list, not a substring list. Swallowing
/// `tools/cargo.sum` because the list contains `go.sum` drops a real change
/// with nothing to show for it.
#[test]
fn an_ignore_suffix_only_matches_at_a_path_boundary() {
    let config = Config {
        ignore_suffixes: vec![
            "Cargo.lock".to_string(),
            "go.sum".to_string(),
            ".tmp".to_string(),
        ],
        ..config()
    };

    for ignored in [
        "Cargo.lock",
        "crates/core/Cargo.lock",
        "go.sum",
        "vendor/go.sum",
        // A suffix beginning with `.` is an extension and may match mid-name.
        "build/output.tmp",
    ] {
        assert!(
            collide::collide::is_ignored(ignored, &config),
            "{ignored} should be ignored"
        );
    }

    for kept in [
        "vendor/NotReallyCargo.lock",
        "tools/cargo.sum",
        "docs/mango.sum",
        "src/yarn.lock.rs",
    ] {
        assert!(
            !collide::collide::is_ignored(kept, &config),
            "{kept} is a real path and must not be ignored"
        );
    }
}

/// The detail view prints one header per repository. It used to take the
/// `repo_root` of whichever member sorted first by label, so the header named a
/// worktree rather than the repository — and changed when a workspace was
/// renamed.
#[test]
fn every_checkout_of_one_repo_reports_the_same_root() {
    let fixture = Fixture::new("repo-root-agreement");
    let main = fixture.worktree("wt", "wt");
    let sibling = fixture.worktree("other", "other");

    let cycle = collide::collide::gather_for(
        vec![
            checkout("one", &main, "ignored-herdr-key"),
            checkout("two", &sibling, "ignored-herdr-key"),
        ],
        &config(),
    )
    .expect("cycle");

    let roots: BTreeSet<&std::path::Path> = cycle
        .report
        .checkouts
        .iter()
        .map(|c| c.repo_root.as_path())
        .collect();
    assert_eq!(
        roots.len(),
        1,
        "two worktrees of one repository must agree on its root, got {roots:?}"
    );
    // And the root they agree on is the repository, not either worktree.
    let root = roots.into_iter().next().unwrap();
    assert_ne!(root, main.as_path());
    assert_ne!(root, sibling.as_path());
}

/// git can name a conflicting path that appears in neither change set. A rename
/// explains that honestly. A content filter the snapshot deliberately does not
/// run explains it dishonestly: a filtered path that is stat-dirty but
/// content-identical re-hashes to its raw bytes and differs from a base holding
/// the filtered blob, even though `status` correctly calls the worktree clean.
/// Believing that would raise a conflict on a file neither agent touched.
#[test]
fn a_conflicting_path_in_neither_change_set_is_only_believed_when_a_rename_explains_it() {
    let checkouts = vec![
        checkout("one", Path::new("/tmp/one"), "/repo/.git"),
        checkout("two", Path::new("/tmp/two"), "/repo/.git"),
    ];
    let changes = vec![
        ("one".to_string(), change_set(&["shared.txt"])),
        ("two".to_string(), change_set(&["shared.txt"])),
    ];
    // Neither side renamed anything, and neither change set mentions media.bin.
    let phantom = vec![PairVerdicts {
        left_workspace_id: "one".to_string(),
        right_workspace_id: "two".to_string(),
        verdicts: vec![
            ("shared.txt".to_string(), false),
            ("media.bin".to_string(), true),
        ],
        failed: false,
        approximate: false,
    }];

    let mut report = analyse(&checkouts, &changes, &distinct_trees(&checkouts), &config());
    apply_predictions(&mut report, &phantom, &changes, &config());
    let paths: Vec<&str> = report.pairings[0]
        .shared
        .iter()
        .map(|s| s.path.as_str())
        .collect();
    assert_eq!(
        paths,
        vec!["shared.txt"],
        "a conflict on a file neither change set lists, with no rename to explain it, is a false alarm"
    );

    // The same prediction, with a rename on one side, is believed: that is the
    // case the pair was predicted for.
    let mut renamed = change_set(&["shared.txt"]);
    renamed.has_rename = true;
    let changes = vec![
        ("one".to_string(), renamed),
        ("two".to_string(), change_set(&["shared.txt"])),
    ];
    let mut report = analyse(&checkouts, &changes, &distinct_trees(&checkouts), &config());
    apply_predictions(&mut report, &phantom, &changes, &config());
    let paths: Vec<&str> = report.pairings[0]
        .shared
        .iter()
        .map(|s| s.path.as_str())
        .collect();
    assert_eq!(paths, vec!["media.bin", "shared.txt"]);
}

/// A path the plugin was told to ignore must not come back through the
/// unlisted-conflict door.
///
/// `merge-tree` merges whole trees and names every conflicted path regardless of
/// the paths it was asked about, so a `Cargo.lock` both agents regenerated is
/// reported as conflicting even though it was filtered out of the pairing. It
/// was then re-added as a `Conflict` and drove the badge — the single commonest
/// false alarm there is, and precisely what `ignore_suffixes` exists to stop.
#[test]
fn an_ignored_path_cannot_return_as_an_unlisted_conflict() {
    let checkouts = vec![
        checkout("one", Path::new("/tmp/one"), "/repo/.git"),
        checkout("two", Path::new("/tmp/two"), "/repo/.git"),
    ];
    // Both sides changed a real file and the lockfile. Only the real file
    // survives the filter into the pairing.
    let changes = vec![
        ("one".to_string(), change_set(&["src/a.rs", "Cargo.lock"])),
        ("two".to_string(), change_set(&["src/a.rs", "Cargo.lock"])),
    ];
    assert!(collide::collide::is_ignored("Cargo.lock", &config()));

    let prediction = vec![PairVerdicts {
        left_workspace_id: "one".to_string(),
        right_workspace_id: "two".to_string(),
        verdicts: vec![
            ("src/a.rs".to_string(), false),
            ("Cargo.lock".to_string(), true),
        ],
        failed: false,
        approximate: false,
    }];

    let mut report = analyse(&checkouts, &changes, &distinct_trees(&checkouts), &config());
    apply_predictions(&mut report, &prediction, &changes, &config());

    let paths: Vec<&str> = report.pairings[0]
        .shared
        .iter()
        .map(|s| s.path.as_str())
        .collect();
    assert_eq!(
        paths,
        vec!["src/a.rs"],
        "an ignored path came back as a conflict"
    );
    for id in ["one", "two"] {
        let status = status_of(&report, id);
        assert_eq!(status.conflict_count, 0);
        assert_eq!(status.severity, Severity::Overlap);
    }
}

/// The other direction, so the test above is measuring the ignore filter and not
/// a broken unlisted-path rule: a path that is *not* ignored and that a change
/// set does list is believed.
#[test]
fn a_listed_unignored_path_still_returns_as_a_conflict() {
    let checkouts = vec![
        checkout("one", Path::new("/tmp/one"), "/repo/.git"),
        checkout("two", Path::new("/tmp/two"), "/repo/.git"),
    ];
    // `vendor/notes.md` is listed by one side only, so it is not in the
    // intersection and not in the pairing — but it is a real path, not an
    // ignored one.
    let changes = vec![
        (
            "one".to_string(),
            change_set(&["src/a.rs", "vendor/notes.md"]),
        ),
        ("two".to_string(), change_set(&["src/a.rs"])),
    ];
    let prediction = vec![PairVerdicts {
        left_workspace_id: "one".to_string(),
        right_workspace_id: "two".to_string(),
        verdicts: vec![
            ("src/a.rs".to_string(), false),
            ("vendor/notes.md".to_string(), true),
        ],
        failed: false,
        approximate: false,
    }];

    let mut report = analyse(&checkouts, &changes, &distinct_trees(&checkouts), &config());
    apply_predictions(&mut report, &prediction, &changes, &config());

    let paths: Vec<&str> = report.pairings[0]
        .shared
        .iter()
        .map(|s| s.path.as_str())
        .collect();
    assert_eq!(paths, vec!["src/a.rs", "vendor/notes.md"]);
    assert_eq!(status_of(&report, "one").severity, Severity::Conflict);
    // Listed is a fact, not a guess, so nothing about this is approximate.
    assert!(!report.pairings[0].approximate);
}

/// An unlisted path admitted only because *some* rename happened somewhere in
/// the pair is a guess: nothing in the prediction says which conflict the rename
/// explains. It is a guess worth making — the alternative is losing the rename
/// conflicts the pair was predicted for — but the pairing says so.
#[test]
fn a_path_admitted_only_by_a_rename_marks_the_pairing_approximate() {
    let checkouts = vec![
        checkout("one", Path::new("/tmp/one"), "/repo/.git"),
        checkout("two", Path::new("/tmp/two"), "/repo/.git"),
    ];
    let mut renamed = change_set(&["shared.txt"]);
    renamed.has_rename = true;
    let changes = vec![
        ("one".to_string(), renamed),
        ("two".to_string(), change_set(&["shared.txt"])),
    ];
    // `moved.rs` is in neither change set; only the rename can explain it.
    let prediction = vec![PairVerdicts {
        left_workspace_id: "one".to_string(),
        right_workspace_id: "two".to_string(),
        verdicts: vec![
            ("shared.txt".to_string(), false),
            ("moved.rs".to_string(), true),
        ],
        failed: false,
        approximate: false,
    }];

    let mut report = analyse(&checkouts, &changes, &distinct_trees(&checkouts), &config());
    apply_predictions(&mut report, &prediction, &changes, &config());

    let paths: Vec<&str> = report.pairings[0]
        .shared
        .iter()
        .map(|s| s.path.as_str())
        .collect();
    assert_eq!(paths, vec!["moved.rs", "shared.txt"], "the guess is kept");
    assert!(
        report.pairings[0].approximate,
        "a path admitted on the strength of an unrelated rename is not a firm verdict"
    );

    // With every conflicting path listed, nothing was guessed and the pairing
    // stays firm even though a rename happened.
    let mut renamed = change_set(&["shared.txt"]);
    renamed.has_rename = true;
    let changes = vec![
        ("one".to_string(), renamed),
        ("two".to_string(), change_set(&["shared.txt"])),
    ];
    let firm = vec![PairVerdicts {
        left_workspace_id: "one".to_string(),
        right_workspace_id: "two".to_string(),
        verdicts: vec![("shared.txt".to_string(), true)],
        failed: false,
        approximate: false,
    }];
    let mut report = analyse(&checkouts, &changes, &distinct_trees(&checkouts), &config());
    apply_predictions(&mut report, &firm, &changes, &config());
    assert!(!report.pairings[0].approximate);
}

/// Reason codes are the stable half of a `degraded_reason`; the human half
/// interpolates branch and ref names the user chose. Matching a code as a
/// substring of the whole string therefore fired on a *branch* named
/// `unborn-branch`, which silently excluded its checkout from every comparison.
#[test]
fn a_branch_named_after_a_reason_code_is_still_paired() {
    for name in ["unborn-branch", "feature/broken-head-rework"] {
        let mut set = change_set(&["shared.txt"]);
        set.degraded = true;
        set.degraded_reason = Some(format!(
            "{}: `{name}` does not resolve",
            git::DEGRADED_MISSING_BASE_REF
        ));
        assert!(
            collide::collide::pairable(&set),
            "a missing base ref named {name} is not an unborn branch"
        );
    }

    // The real codes still work, alone and joined.
    let mut unborn = change_set(&[]);
    unborn.degraded = true;
    unborn.degraded_reason = Some(format!(
        "{}: `wip` has no commits yet",
        git::DEGRADED_UNBORN
    ));
    assert!(!collide::collide::pairable(&unborn));

    let mut joined = change_set(&[]);
    joined.degraded = true;
    joined.degraded_reason = Some(format!(
        "{}: a merge is in progress; {}: `wip` was deleted",
        git::DEGRADED_UNMERGED,
        git::DEGRADED_BROKEN_HEAD
    ));
    assert!(!collide::collide::pairable(&joined));
}

/// The same trap on the severity ladder: a checkout is `Unknown` because the git
/// pass could not read it, not because a ref happens to be named `unreadable`.
#[test]
fn a_ref_named_after_the_unreadable_code_does_not_force_unknown() {
    let checkouts = vec![checkout("one", Path::new("/tmp/one"), "/repo/.git")];
    let mut set = change_set(&["src/a.rs"]);
    set.degraded = true;
    set.degraded_reason = Some(format!(
        "{}: `refs/heads/unreadable` does not resolve",
        git::DEGRADED_MISSING_BASE_REF
    ));
    let changes = vec![("one".to_string(), set)];

    let report = analyse(&checkouts, &changes, &distinct_trees(&checkouts), &config());
    assert_eq!(status_of(&report, "one").severity, Severity::Clean);

    // And a genuinely unreadable checkout is still unknown.
    let changes = vec![(
        "one".to_string(),
        change_set_degraded(&format!("{}: permission denied", git::DEGRADED_UNREADABLE)),
    )];
    let report = analyse(&checkouts, &changes, &distinct_trees(&checkouts), &config());
    assert_eq!(status_of(&report, "one").severity, Severity::Unknown);
}

/// Exactly one severity is live for every combination of inputs, and it is the
/// one the documented ladder names.
///
/// The rung this pins was added without a test: swapping `Unknown` and
/// `Runaway` in the ladder broke nothing in the suite, so the position of the
/// severity the whole change was about could be moved by accident.
///
/// Three workspaces, because a failed prediction is per *pair*: `one` is paired
/// with `two`, whose prediction succeeds and can produce a conflict or an
/// overlap, and with `three`, whose prediction fails and produces the unknown.
/// With only two workspaces the four inputs cannot be varied independently — a
/// failed prediction swallows the whole pair.
#[test]
fn the_severity_ladder_is_total_and_ordered_as_documented() {
    // `one` carries at most three shared files plus its private ones, so the
    // threshold only trips when the case asks for it.
    let config = Config {
        runaway_files: 5,
        ..config()
    };
    let checkouts = vec![
        checkout("one", Path::new("/tmp/one"), "/repo/.git"),
        checkout("two", Path::new("/tmp/two"), "/repo/.git"),
        checkout("three", Path::new("/tmp/three"), "/repo/.git"),
    ];

    for conflict in [false, true] {
        for unknown in [false, true] {
            for runaway in [false, true] {
                for overlap in [false, true] {
                    let mut mine: Vec<&str> = Vec::new();
                    let mut predicted: Vec<&str> = Vec::new();
                    let mut undecidable: Vec<&str> = Vec::new();
                    if conflict {
                        mine.push("c.rs");
                        predicted.push("c.rs");
                    }
                    if overlap {
                        mine.push("o.rs");
                        predicted.push("o.rs");
                    }
                    if unknown {
                        mine.push("u.rs");
                        undecidable.push("u.rs");
                    }
                    if runaway {
                        mine.extend(["r1.rs", "r2.rs", "r3.rs", "r4.rs", "r5.rs", "r6.rs"]);
                    }
                    let changes = vec![
                        ("one".to_string(), change_set(&mine)),
                        ("two".to_string(), change_set(&predicted)),
                        ("three".to_string(), change_set(&undecidable)),
                    ];

                    let mut report =
                        analyse(&checkouts, &changes, &distinct_trees(&checkouts), &config);

                    let mut verdicts = Vec::new();
                    if conflict {
                        verdicts.push(("c.rs".to_string(), true));
                    }
                    if overlap {
                        verdicts.push(("o.rs".to_string(), false));
                    }
                    apply_predictions(
                        &mut report,
                        &[
                            PairVerdicts {
                                left_workspace_id: "one".to_string(),
                                right_workspace_id: "two".to_string(),
                                verdicts,
                                failed: false,
                                approximate: false,
                            },
                            // The pair that could not be decided.
                            PairVerdicts {
                                left_workspace_id: "one".to_string(),
                                right_workspace_id: "three".to_string(),
                                verdicts: Vec::new(),
                                failed: true,
                                approximate: false,
                            },
                        ],
                        &changes,
                        &config,
                    );

                    let status = status_of(&report, "one");
                    let label = format!(
                        "conflict={conflict} unknown={unknown} runaway={runaway} overlap={overlap}"
                    );

                    // The ladder, written out here rather than derived from the
                    // implementation, so a change to either has to be made twice.
                    let expected = if conflict {
                        Severity::Conflict
                    } else if unknown {
                        Severity::Unknown
                    } else if runaway {
                        Severity::Runaway
                    } else if overlap {
                        Severity::Overlap
                    } else {
                        Severity::Clean
                    };
                    assert_eq!(status.severity, expected, "{label}");

                    // Exactly one severity is live, and the counts underneath it
                    // agree rather than drifting.
                    assert_eq!(status.conflict_count, usize::from(conflict), "{label}");
                    assert_eq!(status.unknown_count, usize::from(unknown), "{label}");
                    assert_eq!(status.overlap_count, usize::from(overlap), "{label}");
                    assert_eq!(status.runaway, runaway, "{label}");

                    // ...and every fact survives its own demotion, which is what
                    // makes the ordering a presentation choice rather than a
                    // loss of information.
                    if runaway {
                        assert!(status.runaway, "{label}");
                    }
                    if conflict && unknown {
                        assert!(status.unknown_count > 0, "{label}");
                    }
                }
            }
        }
    }
}

/// The rung itself, stated once and directly: an unknown verdict outranks a
/// runaway. See the argument on `model::Severity` — the runaway fact survives in
/// `runaway`, in `--json` and in the pane, and the unknown has nowhere else to
/// go in the sidebar.
#[test]
fn an_unknown_verdict_outranks_a_runaway() {
    let config = Config {
        runaway_files: 1,
        runaway_lines: 10,
        ..config()
    };
    let checkouts = vec![
        checkout("one", Path::new("/tmp/one"), "/repo/.git"),
        checkout("two", Path::new("/tmp/two"), "/repo/.git"),
    ];
    let changes = vec![
        (
            "one".to_string(),
            change_set(&["shared.rs", "big1.rs", "big2.rs", "big3.rs"]),
        ),
        ("two".to_string(), change_set(&["shared.rs"])),
    ];

    let mut report = analyse(&checkouts, &changes, &distinct_trees(&checkouts), &config);
    apply_predictions(
        &mut report,
        &[PairVerdicts {
            left_workspace_id: "one".to_string(),
            right_workspace_id: "two".to_string(),
            verdicts: Vec::new(),
            failed: true,
            approximate: false,
        }],
        &changes,
        &config,
    );

    let status = status_of(&report, "one");
    assert_eq!(status.severity, Severity::Unknown);
    assert_eq!(status.severity.token_name(), "collide_unknown");
    // The runaway is not lost, it just does not own the badge.
    assert!(status.runaway, "the runaway fact must survive the demotion");
    assert!(status.changed_files >= 4);
}

/// The producer half of "a checkout we could not read is not clean".
///
/// The interpretation of a `DEGRADED_UNREADABLE` change set is tested
/// elsewhere, from a hand-built fixture. This pins the place the fix actually
/// lives: `gather_for` turning a `git::change_set` failure into a degraded
/// change set rather than an empty one. Reverting that line used to leave the
/// whole suite green.
#[test]
fn gather_for_marks_a_checkout_it_could_not_read_rather_than_clean() {
    let fixture = Fixture::new("unreadable-checkout");
    let healthy = fixture.worktree("healthy", "healthy");
    let doomed = fixture.worktree("doomed", "doomed");

    // Snapshot `doomed` while it is readable, then take the repository away
    // underneath it: `repo_key` and the branch lookup have already run by the
    // time `change_set` needs the object store, which is exactly the shape of a
    // worktree pruned or unmounted mid-cycle.
    let checkouts = vec![
        checkout("healthy", &healthy, "unused"),
        checkout("doomed", &doomed, "unused"),
    ];

    let cycle = collide::collide::gather_for(checkouts.clone(), &config()).expect("cycle");
    assert_eq!(
        status_of(&cycle.report, "doomed").severity,
        Severity::Clean,
        "a readable, unchanged worktree is clean"
    );

    // Now break it. Removing the worktree's own git dir leaves the directory in
    // place but makes every object lookup fail.
    let git_dir = fixture.repo.join(".git/worktrees/doomed");
    std::fs::remove_dir_all(&git_dir).expect("remove worktree git dir");

    let cycle = collide::collide::gather_for(checkouts, &config()).expect("cycle");
    let doomed_status = cycle
        .report
        .statuses
        .iter()
        .find(|s| s.workspace_id == "doomed");

    match doomed_status {
        // Either it was dropped before analysis, in which case a note says so...
        None => assert!(
            cycle.notes.iter().any(|n| n.contains("doomed")),
            "a checkout that vanished from the report must leave a note: {:?}",
            cycle.notes
        ),
        // ...or it reached the report, and must not read as clean.
        Some(status) => {
            assert_ne!(
                status.severity,
                Severity::Clean,
                "a checkout we could not read must not badge as clean; notes: {:?}",
                cycle.notes
            );
            assert_eq!(status.severity, Severity::Unknown);
            assert_eq!(collide::render::badge(status), "?");
        }
    }

    // The healthy one is unaffected either way.
    assert_eq!(
        status_of(&cycle.report, "healthy").severity,
        Severity::Clean
    );
}

/// The repo header names the repository, in the nested layout too.
///
/// `agree_on_repo_root` used to derive the root from the repo key alone. With
/// the main worktree open its top level is the exact answer — it is the tree
/// that owns the `--git-common-dir` — so nothing has to be guessed.
#[test]
fn the_repo_root_is_the_main_worktree_not_a_nested_one() {
    let fixture = Fixture::new("nested-repo-root");
    let api = fixture.nested_worktree("api", "feature/api");
    let main = fixture.repo.clone();

    let cycle = collide::collide::gather_for(
        vec![
            checkout("api", &api, "unused"),
            checkout("main", &main, "unused"),
        ],
        &config(),
    )
    .expect("cycle");

    let roots: BTreeSet<&Path> = cycle
        .report
        .checkouts
        .iter()
        .map(|c| c.repo_root.as_path())
        .collect();
    assert_eq!(roots.len(), 1, "one repository, one root: {roots:?}");

    let root = std::fs::canonicalize(roots.into_iter().next().unwrap()).expect("canonical");
    assert_eq!(
        root,
        std::fs::canonicalize(&main).expect("canonical main"),
        "the root is the main worktree, not the nested one"
    );
    assert_ne!(root, std::fs::canonicalize(&api).expect("canonical api"));
}
