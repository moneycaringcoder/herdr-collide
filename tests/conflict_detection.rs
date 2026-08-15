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
use std::path::Path;
use std::time::Duration;

use collide::collide::{analyse, apply_predictions, json_report, Cycle, PairVerdicts};
use collide::config::Config;
use collide::git::{self, predict_conflict, Predictor};
use collide::model::{FileVerdict, Report, Severity};

use fixtures::{change_set, checkout, Fixture};

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

    let report = analyse(&checkouts, &changes, &config());
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

    let report = analyse(&checkouts, &changes, &config());
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

    let report = analyse(&checkouts, &changes, &config());
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

    let report = analyse(&checkouts, &changes, &config());
    assert!(report.pairings.is_empty());
}

#[test]
fn runaway_thresholds_raise_severity_without_an_overlap() {
    let checkouts = vec![checkout("one", Path::new("/tmp/one"), "/repo/.git")];
    let mut set = change_set(&["a.txt"]);
    set.lines_added = 10_000;
    let changes = vec![("one".to_string(), set)];

    let report = analyse(&checkouts, &changes, &config());
    assert_eq!(report.statuses[0].severity, Severity::Runaway);
    assert!(report.statuses[0].runaway);
}

#[test]
fn conflict_outranks_runaway_and_overlap() {
    let checkouts = vec![
        checkout("one", Path::new("/tmp/one"), "/repo/.git"),
        checkout("two", Path::new("/tmp/two"), "/repo/.git"),
    ];
    let mut noisy = change_set(&["shared.txt", "other.txt"]);
    noisy.lines_added = 10_000;
    let changes = vec![
        ("one".to_string(), noisy),
        ("two".to_string(), change_set(&["shared.txt"])),
    ];

    let mut report = analyse(&checkouts, &changes, &config());
    apply_predictions(
        &mut report,
        &[PairVerdicts {
            left_workspace_id: "one".to_string(),
            right_workspace_id: "two".to_string(),
            verdicts: vec![("shared.txt".to_string(), true)],
            failed: false,
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

    let mut report = analyse(&checkouts, &changes, &config());
    apply_predictions(
        &mut report,
        &[PairVerdicts {
            left_workspace_id: "one".to_string(),
            right_workspace_id: "two".to_string(),
            verdicts: vec![("shared.txt".to_string(), false)],
            failed: false,
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

    let mut report = analyse(&checkouts, &changes, &config());
    apply_predictions(
        &mut report,
        &[PairVerdicts {
            left_workspace_id: "one".to_string(),
            right_workspace_id: "two".to_string(),
            verdicts: Vec::new(),
            failed: true,
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
    let mut report = analyse(&checkouts, &changes, &config());
    apply_predictions(
        &mut report,
        &[PairVerdicts {
            left_workspace_id: "one".to_string(),
            right_workspace_id: "two".to_string(),
            verdicts: vec![("shared.txt".to_string(), true)],
            failed: false,
        }],
        &changes,
        &config(),
    );

    let json = json_report(&Cycle {
        report,
        changes,
        notes: vec!["a note".to_string()],
    });

    assert_eq!(json["schema"], 1);
    assert_eq!(json["checkouts"].as_array().unwrap().len(), 2);
    assert_eq!(json["checkouts"][0]["workspace_id"], "one");
    assert_eq!(json["checkouts"][0]["changed_files"], 1);
    assert_eq!(json["pairings"][0]["left"], "one");
    assert_eq!(json["pairings"][0]["conflict_count"], 1);
    assert_eq!(json["pairings"][0]["shared"][0]["verdict"], "conflict");
    assert_eq!(json["statuses"][0]["severity"], "conflict");
    assert_eq!(json["statuses"][0]["token"], "collide_conflict");
    assert_eq!(json["notes"][0], "a note");
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
