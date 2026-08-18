#[path = "fixtures.rs"]
mod fixtures;

use std::path::Path;

use collide::collide::{
    analyse, apply_predictions, json_report, severity_name, verdict_name, Cycle, PairVerdicts,
    JSON_SCHEMA_VERSION,
};
use collide::config::Config;
use collide::model::{Checkout, FileVerdict, Severity, WorkTrees};
use serde_json::Value;

use fixtures::{change_set, checkout};

fn config() -> Config {
    Config {
        predict_conflicts: true,
        ..Config::default()
    }
}

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

fn sorted_keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .expect("schema level must be an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

fn assert_keys(value: &Value, expected: &[&str]) {
    assert_eq!(sorted_keys(value), expected);
}

#[test]
fn json_schema_keys_are_exact() {
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

    assert_eq!(json["schema"], JSON_SCHEMA_VERSION);
    assert_eq!(json["schema"], 2);
    assert_keys(
        &json,
        &["checkouts", "notes", "pairings", "schema", "statuses"],
    );

    let checkout_keys = &[
        "agent",
        "branch",
        "changed_files",
        "checkout_path",
        "degraded",
        "degraded_reason",
        "has_rename",
        "is_linked_worktree",
        "label",
        "lines_added",
        "lines_removed",
        "repo_key",
        "repo_root",
        "target_advisory",
        "target_approximate",
        "target_reason",
        "target_ref",
        "target_verdict",
        "workspace_id",
    ];
    for checkout in json["checkouts"].as_array().expect("checkouts array") {
        assert_keys(checkout, checkout_keys);
    }

    let pairing_keys = &[
        "approximate",
        "conflict_count",
        "left",
        "right",
        "shared",
        "unknown_count",
    ];
    let shared_keys = &["path", "verdict"];
    // Both of these levels are reached only by iterating, so an empty array
    // would pass the key checks while asserting nothing. `analyse` drops a
    // pairing through six separate `continue` arms and the default ignore
    // filter, so a future default or a fixture tweak could empty them
    // silently. Pin the population, not just the shape.
    let pairings = json["pairings"].as_array().expect("pairings array");
    assert!(!pairings.is_empty(), "the fixture must produce a pairing");
    for pairing in pairings {
        assert_keys(pairing, pairing_keys);
        let shared = pairing["shared"].as_array().expect("shared array");
        assert!(!shared.is_empty(), "the fixture must produce a shared file");
        for shared in shared {
            assert_keys(shared, shared_keys);
        }
    }

    let status_keys = &[
        "badge",
        "changed_files",
        "conflict_count",
        "lines_changed",
        "overlap_count",
        "runaway",
        "severity",
        "token",
        "unknown_count",
        "workspace_id",
    ];
    for status in json["statuses"].as_array().expect("statuses array") {
        assert_keys(status, status_keys);
    }

    let notes = json["notes"].as_array().expect("notes array");
    assert!(notes.iter().all(Value::is_string));
}

#[test]
fn json_enum_domains_are_exhaustive() {
    for severity in [
        Severity::Clean,
        Severity::Overlap,
        Severity::Runaway,
        Severity::Unknown,
        Severity::Conflict,
    ] {
        let expected = match severity {
            Severity::Clean => "clean",
            Severity::Overlap => "overlap",
            Severity::Runaway => "runaway",
            Severity::Unknown => "unknown",
            Severity::Conflict => "conflict",
        };
        assert_eq!(severity_name(severity), expected);
    }

    for verdict in [
        FileVerdict::Overlap,
        FileVerdict::Conflict,
        FileVerdict::Unknown,
    ] {
        let expected = match verdict {
            FileVerdict::Overlap => "overlap",
            FileVerdict::Conflict => "conflict",
            FileVerdict::Unknown => "unknown",
        };
        assert_eq!(verdict_name(verdict), expected);
    }
}
