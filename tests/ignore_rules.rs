//! Regression corpus for path ignore rules. False positives here hide real
//! collisions, so every accepted shape is paired with nearby paths that must stay.

#[path = "fixtures.rs"]
mod fixtures;

use std::path::Path;

use collide::collide::{analyse, apply_predictions, is_ignored, PairVerdicts};
use collide::config::Config;
use collide::model::{Report, Severity, WorkTrees};

use fixtures::{change_set, change_set_with_lines, checkout};

fn matches(pattern: &str, path: &str) -> bool {
    is_ignored(path, &config(&[pattern]))
}

fn config(globs: &[&str]) -> Config {
    Config {
        ignore_globs: globs.iter().map(|glob| glob.to_string()).collect(),
        ..Config::default()
    }
}

fn distinct_trees(checkouts: &[collide::model::Checkout]) -> WorkTrees {
    let mut trees = WorkTrees::new();
    for checkout in checkouts {
        trees.insert(
            checkout.workspace_id.clone(),
            checkout.checkout_path.clone(),
        );
    }
    trees
}

fn status_of<'a>(report: &'a Report, id: &str) -> &'a collide::model::WorkspaceStatus {
    report
        .statuses
        .iter()
        .find(|status| status.workspace_id == id)
        .expect("status")
}

#[test]
fn vendor_glob_is_root_anchored_and_globstar_crosses_directories() {
    for path in ["vendor/x", "vendor/a/b/c"] {
        assert!(matches("vendor/**", path), "{path} should match");
    }
    for path in ["my-vendor/x", "x/vendor/y", "vendored.rs"] {
        assert!(!matches("vendor/**", path), "{path} must stay visible");
    }
    assert!(
        !matches("vendor/**", "/vendor/x"),
        "absolute paths must never be matched"
    );
}

#[test]
fn trailing_slash_matches_only_that_directory_tree() {
    for path in ["build", "build/x", "build/x/y"] {
        assert!(matches("build/", path), "{path} should match");
    }
    for path in ["rebuild/x", "build.rs", "a/build/x"] {
        assert!(!matches("build/", path), "{path} must stay visible");
    }
}

#[test]
fn wildcard_directory_rules_do_not_hide_root_files() {
    for (pattern, ignored, visible) in [
        ("target*/", "targetx/output.o", "targetx"),
        ("*/", "docs/README.md", "README.md"),
    ] {
        assert!(
            matches(pattern, ignored),
            "{ignored} should match {pattern}"
        );
        assert!(
            !matches(pattern, visible),
            "{visible} must stay visible to {pattern}"
        );
    }
    assert!(
        !matches("*/", "Cargo.toml"),
        "root-level files must stay visible"
    );
}

#[test]
fn root_single_star_does_not_cross_directories() {
    assert!(matches("*.gen.rs", "a.gen.rs"));
    assert!(
        !matches("*.gen.rs", "src/a.gen.rs"),
        "whole-path anchoring keeps a root pattern at the repository root"
    );
    assert!(!matches("*.gen.rs", "a.gen.rs/kept"));
}

#[test]
fn globstar_suffix_matches_generated_files_at_every_depth() {
    for path in ["a.gen.rs", "src/a.gen.rs", "src/deep/a.gen.rs"] {
        assert!(matches("**.gen.rs", path), "{path} should match");
    }
    for path in ["a.rs", "src/a.gen.rs.bak"] {
        assert!(!matches("**.gen.rs", path), "{path} must stay visible");
    }
}

#[test]
fn component_single_star_does_not_cross_directories() {
    assert!(matches("src/*/mod.rs", "src/a/mod.rs"));
    assert!(!matches("src/*/mod.rs", "src/a/b/mod.rs"));
}

#[test]
fn empty_glob_inputs_never_match() {
    assert!(!is_ignored("vendor/x", &config(&[])));
    assert!(matches("vendor/**", "vendor/x"));
    assert!(!matches("", "vendor/x"));
    assert!(!matches("", ""));
}

#[test]
fn unsupported_glob_syntax_is_literal_not_magic() {
    for (pattern, path) in [
        ("src/?.rs", "src/a.rs"),
        ("src/[ab].rs", "src/a.rs"),
        ("src/{a,b}.rs", "src/a.rs"),
        ("!vendor/**", "vendor/x"),
    ] {
        assert!(
            !matches(pattern, path),
            "unsupported syntax in {pattern:?} must not broaden an ignore rule"
        );
    }
    assert!(matches("src/?.rs", "src/?.rs"));
    assert!(matches("!vendor/**", "!vendor/x"));
}

#[test]
fn a_glob_ignored_path_takes_its_file_and_line_counts_with_it() {
    let checkouts = vec![checkout("one", Path::new("/tmp/one"), "/repo/.git")];
    let changes = vec![(
        "one".to_string(),
        change_set_with_lines(&[("generated/client/output.rs", 90_000)]),
    )];
    let config = Config {
        runaway_files: 0,
        runaway_lines: 0,
        ..config(&["generated/**"])
    };

    let report = analyse(&checkouts, &changes, &distinct_trees(&checkouts), &config);
    let status = status_of(&report, "one");
    assert!(!status.runaway);
    assert_eq!(status.severity, Severity::Clean);
    assert_eq!(status.changed_files, 0);
    assert_eq!(status.lines_changed, 0);
}

#[test]
fn a_glob_ignored_path_is_removed_before_pairing() {
    let checkouts = vec![
        checkout("one", Path::new("/tmp/one"), "/repo/.git"),
        checkout("two", Path::new("/tmp/two"), "/repo/.git"),
    ];
    let changes = vec![
        (
            "one".to_string(),
            change_set(&["vendor/generated/output.rs"]),
        ),
        (
            "two".to_string(),
            change_set(&["vendor/generated/output.rs"]),
        ),
    ];

    let report = analyse(
        &checkouts,
        &changes,
        &distinct_trees(&checkouts),
        &config(&["vendor/**"]),
    );
    assert!(report.pairings.is_empty(), "ignored overlap was paired");
    for id in ["one", "two"] {
        assert_eq!(status_of(&report, id).severity, Severity::Clean);
    }
}

#[test]
fn a_glob_ignored_path_cannot_return_as_an_unlisted_conflict() {
    let checkouts = vec![
        checkout("one", Path::new("/tmp/one"), "/repo/.git"),
        checkout("two", Path::new("/tmp/two"), "/repo/.git"),
    ];
    let changes = vec![
        (
            "one".to_string(),
            change_set(&["src/a.rs", "generated/client.rs"]),
        ),
        (
            "two".to_string(),
            change_set(&["src/a.rs", "generated/client.rs"]),
        ),
    ];
    let config = config(&["generated/**"]);
    let prediction = vec![PairVerdicts {
        left_workspace_id: "one".to_string(),
        right_workspace_id: "two".to_string(),
        verdicts: vec![
            ("src/a.rs".to_string(), false),
            ("generated/client.rs".to_string(), true),
        ],
        submodules: Default::default(),
        conflict_types_by_path: Default::default(),
        failed: false,
        approximate: false,
    }];

    let mut report = analyse(&checkouts, &changes, &distinct_trees(&checkouts), &config);
    apply_predictions(&mut report, &prediction, &changes, &config);

    let paths: Vec<&str> = report.pairings[0]
        .shared
        .iter()
        .map(|shared| shared.path.as_str())
        .collect();
    assert_eq!(paths, vec!["src/a.rs"]);
    for id in ["one", "two"] {
        let status = status_of(&report, id);
        assert_eq!(status.conflict_count, 0);
        assert_eq!(status.severity, Severity::Overlap);
    }
}

#[test]
fn suffix_and_glob_rules_compose_by_union() {
    let config = Config {
        ignore_suffixes: vec![".snap".to_string()],
        ..config(&["generated/**"])
    };

    assert!(collide::collide::is_ignored("tests/result.snap", &config));
    assert!(collide::collide::is_ignored("generated/client.rs", &config));
    assert!(!collide::collide::is_ignored("src/result.snap.rs", &config));
    assert!(!collide::collide::is_ignored(
        "my-generated/client.rs",
        &config
    ));
}
