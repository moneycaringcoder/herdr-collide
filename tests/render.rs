//! Formatting tests for the badge string and the detail pane.
//!
//! These are pure: nothing here talks to herdr or to git. Every fixture is a
//! hand-built `Report`, and every assertion is about the text that comes out.
//!
//! Widths are checked in *display columns*, never in bytes. The badge is built
//! from multi-byte glyphs, so `str::len` would report 5 for a badge that
//! occupies 3 columns. The `columns` helper below is a deliberately independent
//! second implementation of the width rule, so a bug in the renderer's own
//! width code cannot hide behind a matching bug in the test.

use std::path::PathBuf;

use collide::model::{
    ChangeKind, ChangeSet, ChangedPath, Checkout, FileVerdict, Pairing, RepoKey, Report, Severity,
    SharedFile, WorkspaceStatus,
};
use collide::render::{abbreviate, badge, detail, detail_at, BADGE_COLUMNS};

// ---------------------------------------------------------------------------
// Test-local display width
// ---------------------------------------------------------------------------

/// Width of `text` in terminal columns. Written from scratch rather than
/// reusing `render::display_width`, and rather than pulling in `unicode-width`,
/// which is not a dependency of this crate.
fn columns(text: &str) -> usize {
    let mut total = 0;
    let mut in_escape = false;
    for ch in text.chars() {
        if in_escape {
            // A CSI sequence ends at its final byte, which is always in @-~.
            if ('\u{40}'..='\u{7e}').contains(&ch) {
                in_escape = false;
            }
            continue;
        }
        if ch == '\u{1b}' {
            in_escape = true;
            continue;
        }
        if ch.is_control() {
            continue;
        }
        total += match ch as u32 {
            // Combining marks and variation selectors take no space.
            0x0300..=0x036f | 0x200b..=0x200f | 0xfe00..=0xfe0f | 0xfeff => 0,
            // The common East Asian wide and fullwidth blocks take two.
            0x1100..=0x115f
            | 0x2e80..=0xa4cf
            | 0xac00..=0xd7a3
            | 0xf900..=0xfaff
            | 0xfe30..=0xfe6f
            | 0xff00..=0xff60
            | 0xffe0..=0xffe6
            | 0x1f300..=0x1f64f
            | 0x1f900..=0x1f9ff => 2,
            _ => 1,
        };
    }
    total
}

fn widest(text: &str) -> usize {
    text.lines().map(columns).max().unwrap_or(0)
}

/// The whole view as one whitespace-normalised line, for asserting on prose
/// that word-wraps differently at different widths.
fn flatten(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const REPO: &str = "/repos/app/.git";

fn checkout(id: &str, label: &str, branch: Option<&str>, agent: Option<&str>) -> Checkout {
    Checkout {
        workspace_id: id.to_string(),
        workspace_label: label.to_string(),
        repo_key: RepoKey(REPO.to_string()),
        repo_root: PathBuf::from("/repos/app"),
        checkout_path: PathBuf::from(format!("/repos/app/.worktrees/{label}")),
        is_linked_worktree: label != "api",
        branch: branch.map(str::to_string),
        agent: agent.map(str::to_string),
    }
}

fn status(id: &str, severity: Severity, overlaps: usize, conflicts: usize) -> WorkspaceStatus {
    WorkspaceStatus {
        workspace_id: id.to_string(),
        severity,
        overlap_count: overlaps,
        conflict_count: conflicts,
        runaway: severity == Severity::Runaway,
        lines_changed: 0,
    }
}

fn shared(path: &str, verdict: FileVerdict) -> SharedFile {
    SharedFile {
        path: path.to_string(),
        verdict,
    }
}

const DEEP_PATH: &str =
    "crates/collide-core/src/analysis/pairing/heuristics/very_long_module_name.rs";

/// One repo, three worktrees, one pairing carrying all three verdicts, and one
/// checkout with no branch at all.
fn report() -> Report {
    Report {
        checkouts: vec![
            checkout("w1", "api", Some("feature/api"), Some("claude")),
            checkout("w2", "ui", Some("feature/ui"), Some("codex")),
            checkout("w3", "salvage", None, None),
        ],
        pairings: vec![Pairing {
            left_workspace_id: "w1".to_string(),
            right_workspace_id: "w2".to_string(),
            shared: vec![
                shared("src/model.rs", FileVerdict::Overlap),
                shared(DEEP_PATH, FileVerdict::Unknown),
                shared("src/git.rs", FileVerdict::Conflict),
                shared("src/collide.rs", FileVerdict::Conflict),
            ],
        }],
        statuses: vec![
            status("w1", Severity::Conflict, 2, 2),
            status("w2", Severity::Conflict, 2, 2),
            status("w3", Severity::Clean, 0, 0),
        ],
        changes: Vec::new(),
    }
}

fn line_index(text: &str, needle: &str) -> usize {
    text.lines()
        .position(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no line containing {needle:?} in:\n{text}"))
}

// ---------------------------------------------------------------------------
// Badge
// ---------------------------------------------------------------------------

#[test]
fn a_clean_workspace_renders_no_badge() {
    // The empty string is the daemon's signal to clear the token, so it must
    // stay empty rather than becoming a placeholder.
    assert_eq!(badge(&status("w1", Severity::Clean, 0, 0)), "");
    // Stale counters must not resurrect a badge on a clean workspace.
    assert_eq!(badge(&status("w1", Severity::Clean, 7, 3)), "");
}

#[test]
fn each_severity_gets_its_own_mark() {
    assert_eq!(badge(&status("w", Severity::Overlap, 3, 0)), "\u{29c9} 3");
    assert_eq!(badge(&status("w", Severity::Conflict, 5, 2)), "\u{2718} 2");
    assert_eq!(badge(&status("w", Severity::Runaway, 4, 0)), "\u{26a0} 4");
}

#[test]
fn the_badge_carries_no_colour() {
    // Severity rides the token *name*; the value herdr renders is flat text.
    for severity in [Severity::Overlap, Severity::Conflict, Severity::Runaway] {
        let text = badge(&status("w", severity, 3, 1));
        assert!(
            !text.contains('\u{1b}'),
            "badge emitted an escape: {text:?}"
        );
    }
}

#[test]
fn a_severity_with_no_count_renders_the_mark_alone() {
    // A runaway is about change-set size, so it can be flagged with nothing
    // shared. Printing `⚠ 0` would read as "zero problems".
    assert_eq!(badge(&status("w", Severity::Runaway, 0, 0)), "\u{26a0}");
    assert_eq!(badge(&status("w", Severity::Conflict, 0, 0)), "\u{2718}");
}

#[test]
fn numbers_abbreviate_at_the_boundaries() {
    assert_eq!(abbreviate(0), "0");
    assert_eq!(abbreviate(999), "999");
    assert_eq!(abbreviate(1_000), "1.0k");
    assert_eq!(abbreviate(1_200), "1.2k");
    // Truncation, not rounding: the badge must never overstate.
    assert_eq!(abbreviate(1_299), "1.2k");
    assert_eq!(abbreviate(9_999), "9.9k");
    assert_eq!(abbreviate(10_000), "10k");
    assert_eq!(abbreviate(999_999), "999k");
    assert_eq!(abbreviate(1_000_000), "1.0M");
    assert_eq!(abbreviate(9_999_999), "9.9M");
    assert_eq!(abbreviate(10_000_000), "10M");
    assert_eq!(abbreviate(999_999_999), "999M");
    assert_eq!(abbreviate(1_000_000_000), "1G+");
}

#[test]
fn abbreviation_reaches_the_badge() {
    assert_eq!(
        badge(&status("w", Severity::Overlap, 1_200, 0)),
        "\u{29c9} 1.2k"
    );
    assert_eq!(
        badge(&status("w", Severity::Conflict, 0, 4_100)),
        "\u{2718} 4.1k"
    );
}

#[test]
fn the_badge_never_exceeds_its_column_budget() {
    assert_eq!(BADGE_COLUMNS, 6);
    let counts = [
        0usize,
        1,
        9,
        10,
        99,
        100,
        999,
        1_000,
        1_200,
        9_999,
        10_000,
        999_999,
        1_000_000,
        9_999_999,
        10_000_000,
        999_999_999,
        1_000_000_000,
        usize::MAX,
    ];
    for severity in [
        Severity::Clean,
        Severity::Overlap,
        Severity::Runaway,
        Severity::Conflict,
    ] {
        for count in counts {
            let text = badge(&status("w", severity, count, count));
            assert!(
                columns(&text) <= BADGE_COLUMNS,
                "badge {text:?} is {} columns for {severity:?}/{count}",
                columns(&text)
            );
        }
    }
}

#[test]
fn badge_width_is_columns_not_bytes() {
    let text = badge(&status("w", Severity::Overlap, 3, 0));
    // The point of the helper: `len()` lies about multi-byte glyphs.
    assert_eq!(text.len(), 5, "expected a multi-byte badge, got {text:?}");
    assert_eq!(columns(&text), 3);
}

// ---------------------------------------------------------------------------
// Detail view
// ---------------------------------------------------------------------------

#[test]
fn conflicts_sort_above_overlaps() {
    let text = detail(&report());
    let first_conflict = line_index(&text, "src/collide.rs");
    let second_conflict = line_index(&text, "src/git.rs");
    let unknown = line_index(&text, "unknown");
    let overlap = line_index(&text, "src/model.rs");

    assert!(first_conflict < second_conflict, "conflicts sort by path");
    assert!(second_conflict < unknown, "conflicts precede unknowns");
    assert!(unknown < overlap, "unknowns precede plain overlaps");
}

#[test]
fn every_verdict_is_named_in_words() {
    let text = detail(&report());
    assert!(text.contains("\u{2718} conflict"));
    assert!(text.contains("\u{29c9} overlap"));
    assert!(text.contains("? unknown"));
    // And the legend spells out what each mark means.
    assert!(text.contains("conflict predicted on merge"));
    assert!(text.contains("same file, merges clean"));
    assert!(text.contains("conflict prediction unavailable"));
}

#[test]
fn each_worktree_shows_its_branch_and_agent() {
    let text = detail(&report());
    assert!(text.contains("api [feature/api] @claude"), "{text}");
    assert!(text.contains("ui [feature/ui] @codex"), "{text}");
    // An unoccupied worktree says so rather than leaving a blank column.
    assert!(text.contains("(no agent)"), "{text}");
    // The repo they belong to is named once, above them.
    assert!(text.contains("repo /repos/app"));
    assert_eq!(
        line_index(&text, "repo /repos/app") + 1,
        line_index(&text, "api [")
    );
}

#[test]
fn a_degraded_checkout_says_why() {
    let text = detail(&report());
    let worktree = line_index(&text, "salvage [no branch]");
    let note = line_index(&text, "degraded:");
    assert_eq!(
        note,
        worktree + 1,
        "the reason follows the worktree it explains"
    );
    assert!(flatten(&text).contains("detached HEAD"), "{text}");
    assert!(flatten(&text).contains("branch lookup failed"), "{text}");
}

#[test]
fn a_healthy_checkout_gets_no_degraded_note() {
    let mut report = report();
    report.checkouts.retain(|c| c.branch.is_some());
    report.statuses.retain(|s| s.workspace_id != "w3");
    let text = detail(&report);
    assert!(!text.contains("degraded:"), "{text}");
}

#[test]
fn long_paths_truncate_from_the_left() {
    let text = detail(&report());
    let line = text
        .lines()
        .find(|l| l.contains("very_long_module_name.rs"))
        .unwrap_or_else(|| panic!("the long path vanished:\n{text}"));

    // The tail survives, the head is replaced by a single ellipsis, and the
    // original head is gone.
    assert!(line.ends_with("very_long_module_name.rs"), "{line:?}");
    assert!(line.contains('\u{2026}'), "no ellipsis in {line:?}");
    assert!(!line.contains("crates/collide-core"), "{line:?}");
}

#[test]
fn short_paths_are_left_intact() {
    let text = detail(&report());
    assert!(text.contains("  src/model.rs"), "{text}");
    assert!(!text.contains("\u{2026}src/model.rs"), "{text}");
}

#[test]
fn the_view_fits_eighty_columns() {
    let text = detail(&report());
    assert!(
        widest(&text) <= 80,
        "widest line is {}:\n{text}",
        widest(&text)
    );
    // ...and actually uses the room it has for the truncated path.
    let line = text
        .lines()
        .find(|l| l.contains("very_long_module_name.rs"))
        .unwrap();
    assert!(columns(line) > 40, "80 columns wasted on {line:?}");
}

#[test]
fn the_view_fits_forty_columns() {
    let text = detail_at(&report(), 40);
    assert!(
        widest(&text) <= 40,
        "widest line is {}:\n{text}",
        widest(&text)
    );

    // Narrow does not mean lossy: the structure survives.
    assert!(text.contains("conflict"), "{text}");
    assert!(text.contains("overlap"), "{text}");
    assert!(text.contains("degraded:"), "{text}");
    // The note wraps at this width rather than being cut off, so look for the
    // phrase across the line breaks: truncating an explanation removes it.
    assert!(flatten(&text).contains("detached HEAD"), "{text}");
    assert!(flatten(&text).contains("branch lookup failed"), "{text}");
    // 40 columns leaves 24 for the path, so the tail is cut shorter than at 80
    // — but it is still the tail, and still the half that identifies the file.
    let path_line = text
        .lines()
        .find(|l| l.contains("_module_name.rs"))
        .unwrap_or_else(|| panic!("the informative tail was lost at 40 columns:\n{text}"));
    assert!(path_line.ends_with("_module_name.rs"), "{path_line:?}");
    assert!(path_line.contains('\u{2026}'), "{path_line:?}");
}

#[test]
fn an_absurdly_narrow_terminal_still_produces_bounded_lines() {
    for width in [1usize, 5, 12, 20, 21, 33] {
        let text = detail_at(&report(), width);
        let cap = width.max(20);
        assert!(
            widest(&text) <= cap,
            "width {width} produced a {}-column line:\n{text}",
            widest(&text)
        );
    }
}

#[test]
fn the_view_emits_no_ansi_escapes() {
    // There is no colour library here, and severity is a token name, not a
    // colour in the pane.
    for width in [40usize, 80, 200] {
        let text = detail_at(&report(), width);
        assert!(!text.contains('\u{1b}'), "escape sequence at width {width}");
    }
}

#[test]
fn an_empty_session_says_so() {
    let text = detail(&Report::default());
    assert!(
        text.contains("No git-backed workspaces are open."),
        "{text}"
    );
    assert!(widest(&text) <= 80);
}

#[test]
fn a_repo_with_no_shared_files_says_so() {
    let mut report = report();
    report.pairings.clear();
    report.statuses = vec![status("w1", Severity::Clean, 0, 0)];
    let text = detail(&report);
    assert!(
        text.contains("no files shared with a sibling worktree"),
        "{text}"
    );
    // No legend when there is nothing to explain.
    assert!(!text.contains("legend"), "{text}");
}

#[test]
fn the_unknown_legend_appears_only_when_something_is_unknown() {
    let mut report = report();
    for pairing in &mut report.pairings {
        pairing.shared.retain(|f| f.verdict != FileVerdict::Unknown);
    }
    let text = detail(&report);
    assert!(text.contains("legend"), "{text}");
    assert!(!text.contains("prediction unavailable"), "{text}");
}

#[test]
fn a_workspace_badge_rides_along_in_the_detail_view() {
    let text = detail(&report());
    let line = text.lines().find(|l| l.contains("api [")).unwrap();
    assert!(line.contains("\u{2718} 2"), "{line:?}");
}

// ---------------------------------------------------------------------------
// Model contract these renderings rely on
// ---------------------------------------------------------------------------

#[test]
fn severity_still_maps_onto_the_four_documented_tokens() {
    // The renderer deliberately encodes no colour, on the strength of this.
    assert_eq!(Severity::Clean.token_name(), "collide_clean");
    assert_eq!(Severity::Overlap.token_name(), "collide_overlap");
    assert_eq!(Severity::Runaway.token_name(), "collide_runaway");
    assert_eq!(Severity::Conflict.token_name(), "collide_conflict");
    assert_eq!(Severity::ALL_TOKENS.len(), 4);
}

#[test]
fn a_change_set_summarises_its_own_volume() {
    // Not a rendering test as such, but the runaway badge is only meaningful
    // if this stays additive.
    let change_set = ChangeSet {
        paths: vec![ChangedPath {
            path: "src/render.rs".to_string(),
            kind: ChangeKind::Unstaged,
        }],
        lines_added: 4_000,
        lines_removed: 100,
        degraded: false,
        degraded_reason: None,
    };
    assert_eq!(change_set.lines_changed(), 4_100);
    assert_eq!(abbreviate(change_set.lines_changed()), "4.1k");
}
