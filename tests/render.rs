//! Formatting tests for the badge string and the detail pane.
//!
//! These are pure: nothing here talks to herdr or to git. Every fixture is a
//! hand-built `Report`, and every assertion is about the text that comes out.
//!
//! Widths are checked in *display columns*, never in bytes. The badge is built
//! from multi-byte glyphs, so `str::len` would report 5 for a badge that
//! occupies 3 columns.

use std::path::PathBuf;

use collide::model::{
    ChangeKind, ChangeSet, ChangedPath, Checkout, FileVerdict, Pairing, RepoKey, Report, Severity,
    SharedFile, TargetPrediction, TargetVerdict, WorkspaceStatus,
};
use collide::render::{
    abbreviate, abbreviate_files, badge, detail, detail_at, detail_with_notes, BADGE_COLUMNS,
};
// Only the degradation reason codes: the tests build the strings git would
// write, so that a renamed code fails here rather than silently rendering raw.
use collide::git;

// ---------------------------------------------------------------------------
// Test-local display width
// ---------------------------------------------------------------------------
//
// This oracle is a *table*, not a second implementation of the renderer's rule
// set. That distinction is the whole point of it.
//
// The previous oracle was a range algorithm, written from the same reading of
// the same blocks as `render::char_columns` — and so it inherited exactly the
// same gaps. `🚀` measured one column in both, and a line that occupied 41
// columns in a real terminal sailed through an assertion that it fit in 40. Two
// implementations that share an assumption test the assumption once, not twice.
//
// A table cannot share a gap. Every non-ASCII scalar the fixtures produce has to
// be declared here with a hand-checked width, and a scalar nobody has declared
// panics the test rather than being silently guessed at. Adding a character to a
// fixture therefore forces somebody to look up what it actually measures.

/// Multi-scalar sequences whose width is not the sum of their parts, longest
/// first. A base scalar followed by U+FE0F takes *emoji* presentation, which is
/// two columns even where the bare scalar is one.
const SEQUENCE_WIDTHS: &[(&str, usize)] = &[
    ("\u{26a0}\u{fe0f}", 2), // ⚠️ warning sign, emoji presentation
    ("\u{2764}\u{fe0f}", 2), // ❤️ heavy black heart, emoji presentation
];

/// Width of every non-ASCII scalar these fixtures can produce, hand-checked
/// against Unicode 15 `EastAsianWidth.txt` (`W`/`F` are two columns, `N`/`Na`/`A`
/// one) and `emoji-data.txt` (`Emoji_Presentation` is two).
const WIDTHS: &[(char, usize)] = &[
    // --- punctuation and marks the renderer itself emits ---
    ('\u{b7}', 1),   // · MIDDLE DOT — Ambiguous, one column by default
    ('\u{2014}', 1), // — EM DASH — Ambiguous, one column by default
    ('\u{2026}', 1), // … HORIZONTAL ELLIPSIS — Ambiguous, one column by default
    ('\u{2718}', 1), // ✘ HEAVY BALLOT X — Neutral
    ('\u{29c9}', 1), // ⧉ TIE — Neutral
    ('\u{26a0}', 1), // ⚠ WARNING SIGN — Neutral, text presentation
    ('\u{fe0f}', 0), // VARIATION SELECTOR-16 — zero width on its own
    // --- Latin with diacritics, precomposed and combining ---
    ('\u{e9}', 1),  // é
    ('\u{ef}', 1),  // ï
    ('\u{301}', 0), // COMBINING ACUTE ACCENT
    ('\u{308}', 0), // COMBINING DIAERESIS
    // --- emoji that are wide with no selector ---
    ('\u{1f680}', 2), // 🚀 ROCKET — Wide
    ('\u{2b50}', 2),  // ⭐ WHITE MEDIUM STAR — Wide
    ('\u{231a}', 2),  // ⌚ WATCH — Wide
    ('\u{1fa79}', 2), // 🩹 ADHESIVE BANDAGE — Wide
    ('\u{a960}', 2),  // ꥠ HANGUL CHOSEONG TIKEUT-MIEUM — Wide
    // --- CJK used in the path fixtures ---
    ('\u{8a2d}', 2), // 設
    ('\u{8a08}', 2), // 計
    ('\u{8a73}', 2), // 詳
    ('\u{7d30}', 2), // 細
    ('\u{4ed5}', 2), // 仕
    ('\u{69d8}', 2), // 様
    ('\u{66f8}', 2), // 書
    ('\u{79fb}', 2), // 移
    ('\u{884c}', 2), // 行
    ('\u{30b5}', 2), // サ
    ('\u{30fc}', 2), // ー
    ('\u{30d3}', 2), // ビ
    ('\u{30b9}', 2), // ス
];

/// Width of `text` in terminal columns, from the declared table alone.
fn columns(text: &str) -> usize {
    let mut total = 0;
    let mut rest = text;
    'outer: while !rest.is_empty() {
        for (sequence, width) in SEQUENCE_WIDTHS {
            if let Some(tail) = rest.strip_prefix(*sequence) {
                total += width;
                rest = tail;
                continue 'outer;
            }
        }
        let ch = rest.chars().next().expect("rest is not empty");
        total += scalar_columns(ch);
        rest = &rest[ch.len_utf8()..];
    }
    total
}

fn scalar_columns(ch: char) -> usize {
    if ch == ' ' || ch.is_ascii_graphic() {
        return 1;
    }
    if ch.is_control() {
        return 0;
    }
    for (declared, width) in WIDTHS {
        if *declared == ch {
            return *width;
        }
    }
    panic!(
        "the width oracle has no entry for U+{:04X} ({ch:?}). Add it to WIDTHS with a width \
         looked up by hand — do not let the test guess, which is how the last width bug got \
         through.",
        ch as u32
    );
}

fn widest(text: &str) -> usize {
    text.lines().map(columns).max().unwrap_or(0)
}

/// The whole view as one whitespace-normalised line, for asserting on prose
/// that word-wraps differently at different widths.
fn flatten(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn the_oracle_is_pinned_to_hand_checked_ground_truth() {
    // If this ever has to change, somebody has looked the character up. That is
    // the point: the oracle is not allowed to drift toward the implementation.
    assert_eq!(columns("plain ascii"), 11);
    assert_eq!(columns("\u{1f680}"), 2, "🚀 is Wide");
    assert_eq!(columns("\u{26a0}"), 1, "bare ⚠ is Neutral");
    assert_eq!(
        columns("\u{26a0}\u{fe0f}"),
        2,
        "⚠️ takes emoji presentation"
    );
    assert_eq!(columns("\u{8a2d}\u{8a08}"), 4, "CJK is two columns each");
    assert_eq!(columns("e\u{301}"), 1, "a combining mark adds nothing");
    assert_eq!(
        columns("\u{1b}[31m"),
        4,
        "the oracle does not special-case CSI"
    );
}

#[test]
fn the_oracle_refuses_to_guess_at_an_undeclared_character() {
    // The failure mode this replaces was silence, so the replacement has to be
    // loud. U+0416 is not in the table and never will be.
    let undeclared = std::panic::catch_unwind(|| columns("\u{416}"));
    assert!(undeclared.is_err(), "an undeclared scalar must panic");
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
        unknown_count: 0,
        runaway: severity == Severity::Runaway,
        lines_changed: 0,
        changed_files: 0,
    }
}

/// A runaway is measured in changed lines, so it needs its own constructor.
fn runaway(id: &str, lines_changed: u64) -> WorkspaceStatus {
    WorkspaceStatus {
        lines_changed,
        ..status(id, Severity::Runaway, 0, 0)
    }
}

/// A runaway that crossed the *file* threshold and carries no counted lines —
/// hundreds of untracked binaries, say.
fn file_runaway(id: &str, changed_files: usize) -> WorkspaceStatus {
    WorkspaceStatus {
        changed_files,
        ..status(id, Severity::Runaway, 0, 0)
    }
}

fn unknown(id: &str, unknowns: usize) -> WorkspaceStatus {
    WorkspaceStatus {
        unknown_count: unknowns,
        ..status(id, Severity::Unknown, 0, 0)
    }
}

/// A change set that carries nothing but a degradation reason, in the
/// `code: detail` form `git::change_set` writes.
fn degraded(reason: &str) -> ChangeSet {
    ChangeSet {
        degraded: true,
        degraded_reason: Some(reason.to_string()),
        ..ChangeSet::default()
    }
}

fn shared(path: &str, verdict: FileVerdict) -> SharedFile {
    SharedFile {
        path: path.to_string(),
        verdict,
    }
}

fn pairing(left: &str, right: &str, files: Vec<SharedFile>) -> Pairing {
    Pairing {
        left_workspace_id: left.to_string(),
        right_workspace_id: right.to_string(),
        shared: files,
        approximate: false,
    }
}

const DEEP_PATH: &str =
    "crates/collide-core/src/analysis/pairing/heuristics/very_long_module_name.rs";

/// A path built from characters the renderer used to under-measure: an emoji
/// with a variation selector, an emoji that is wide with no selector at all, and
/// a combining mark. At 40 columns this line is what pushed the pane to 41.
const EMOJI_PATH: &str = "assets/\u{26a0}\u{fe0f}-warnings/\u{1f680}-launch/cafe\u{301}.svg";

/// All-CJK components, which the renderer has always measured correctly and
/// which must stay that way.
const CJK_PATH: &str = "\u{8a2d}\u{8a08}/\u{8a73}\u{7d30}\u{4ed5}\u{69d8}\u{66f8}.md";

/// One repo, three worktrees, one pairing carrying all three verdicts, and one
/// checkout with no branch at all.
fn report() -> Report {
    Report {
        checkouts: vec![
            checkout("w1", "api", Some("feature/api"), Some("claude")),
            checkout("w2", "ui", Some("feature/ui"), Some("codex")),
            checkout("w3", "salvage", None, None),
        ],
        pairings: vec![pairing(
            "w1",
            "w2",
            vec![
                shared("src/model.rs", FileVerdict::Overlap),
                shared(DEEP_PATH, FileVerdict::Unknown),
                shared("src/git.rs", FileVerdict::Conflict),
                shared("src/collide.rs", FileVerdict::Conflict),
                shared(EMOJI_PATH, FileVerdict::Conflict),
                shared(CJK_PATH, FileVerdict::Overlap),
            ],
        )],
        statuses: vec![
            status("w1", Severity::Conflict, 2, 3),
            status("w2", Severity::Conflict, 2, 3),
            status("w3", Severity::Clean, 0, 0),
        ],
        targets: Vec::new(),
        changes: Vec::new(),
    }
}

fn line_index(text: &str, needle: &str) -> usize {
    text.lines()
        .position(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no line containing {needle:?} in:\n{text}"))
}

fn line_with<'a>(text: &'a str, needle: &str) -> &'a str {
    text.lines()
        .find(|l| l.contains(needle))
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
    assert_eq!(badge(&unknown("w", 4)), "? 4");
    // A runaway counts changed lines, not shared files.
    assert_eq!(badge(&runaway("w", 4_100)), "\u{26a0} 4.1k");
}

#[test]
fn an_unknown_workspace_never_claims_a_clean_merge() {
    // This is the whole reason `Severity::Unknown` exists. A prediction that
    // could not run used to be folded into the overlap count, so the badge said
    // `⧉ 2` — and the legend spells `⧉` out as "same file, merges clean". A
    // failed prediction is not entitled to make that claim.
    let could_not_tell = unknown("w", 2);
    let genuinely_clean = status("w", Severity::Overlap, 2, 0);

    assert_eq!(badge(&could_not_tell), "? 2");
    assert_eq!(badge(&genuinely_clean), "\u{29c9} 2");
    assert_ne!(
        badge(&could_not_tell),
        badge(&genuinely_clean),
        "\"I could not tell\" and \"it merges clean\" must not render alike"
    );
    assert_ne!(
        could_not_tell.severity.token_name(),
        genuinely_clean.severity.token_name(),
        "...and they must not share a token either, since the token carries the colour"
    );
}

#[test]
fn an_unreadable_checkout_is_unknown_with_nothing_to_count() {
    // A checkout the git pass could not read at all is unknown without any
    // shared file to point at. The bare mark is the honest badge; an empty
    // string would clear the token and read as "clean".
    assert_eq!(badge(&unknown("w", 0)), "?");
}

#[test]
fn a_runaway_reports_change_set_size_not_shared_files() {
    // The severity exists to catch a workspace that has grown huge on its own,
    // which is usually one sharing nothing at all with its siblings. Reporting
    // its overlap count would print `⚠ 0` for exactly the case it is for.
    let alone = WorkspaceStatus {
        lines_changed: 4_100,
        ..status("w", Severity::Runaway, 0, 0)
    };
    assert_eq!(badge(&alone), "\u{26a0} 4.1k");

    // And where both numbers exist, the size is the one that shows.
    let sharing = WorkspaceStatus {
        lines_changed: 12_000,
        ..status("w", Severity::Runaway, 4, 0)
    };
    assert_eq!(badge(&sharing), "\u{26a0} 12k");
}

#[test]
fn a_file_count_runaway_reports_files_rather_than_a_bare_mark() {
    // Either threshold can trip a runaway. One tripped on files alone carries
    // no counted lines — hundreds of untracked binaries — and used to render as
    // `⚠` with no magnitude at all, which says only that something is wrong.
    assert_eq!(badge(&file_runaway("w", 60)), "\u{26a0} 60f");
    assert_eq!(badge(&file_runaway("w", 4_200)), "\u{26a0} 4kf");
    // The `f` is what stops 60 files reading as 60 lines.
    assert_ne!(badge(&file_runaway("w", 60)), badge(&runaway("w", 60)));
    assert_eq!(badge(&runaway("w", 60)), "\u{26a0} 60");
    // Lines win when there are any: they are the finer measure of volume.
    let both = WorkspaceStatus {
        lines_changed: 3_000,
        ..file_runaway("w", 60)
    };
    assert_eq!(badge(&both), "\u{26a0} 3.0k");
    // Nothing at all to report still beats an invented zero.
    assert_eq!(badge(&file_runaway("w", 0)), "\u{26a0}");
}

#[test]
fn the_badge_carries_no_colour() {
    // Severity rides the token *name*; the value herdr renders is flat text.
    for severity in [
        Severity::Overlap,
        Severity::Conflict,
        Severity::Runaway,
        Severity::Unknown,
    ] {
        let text = badge(&status("w", severity, 3, 1));
        assert!(
            !text.contains('\u{1b}'),
            "badge emitted an escape: {text:?}"
        );
    }
}

#[test]
fn a_severity_with_no_count_renders_the_mark_alone() {
    // Printing `⚠ 0` would read as "zero problems", which is the opposite of
    // what the mark is there to say.
    assert_eq!(badge(&runaway("w", 0)), "\u{26a0}");
    assert_eq!(badge(&status("w", Severity::Conflict, 0, 0)), "\u{2718}");
    assert_eq!(badge(&unknown("w", 0)), "?");
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
fn file_counts_abbreviate_inside_four_columns_including_the_unit() {
    // The badge budget is six columns: mark, space, and four for the magnitude.
    // The `f` has to come out of those four, not be added to them.
    for n in [0usize, 1, 999, 1_000, 99_999, 100_000, usize::MAX] {
        let text = abbreviate_files(n);
        assert!(
            columns(&text) <= 4,
            "abbreviate_files({n}) is {} columns: {text:?}",
            columns(&text)
        );
    }
    assert_eq!(abbreviate_files(0), "0f");
    assert_eq!(abbreviate_files(999), "999f");
    assert_eq!(abbreviate_files(1_000), "1kf");
    assert_eq!(abbreviate_files(99_999), "99kf");
    assert_eq!(abbreviate_files(100_000), "99k+");
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
    assert_eq!(badge(&runaway("w", 1_200_000)), "\u{26a0} 1.2M");
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
        99_999,
        100_000,
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
        Severity::Unknown,
        Severity::Conflict,
    ] {
        for count in counts {
            // Every numeric field at once, so no severity can read a field
            // that was left at zero and pass by accident.
            let status = WorkspaceStatus {
                lines_changed: count as u64,
                changed_files: count,
                unknown_count: count,
                ..status("w", severity, count, count)
            };
            let text = badge(&status);
            assert!(
                columns(&text) <= BADGE_COLUMNS,
                "badge {text:?} is {} columns for {severity:?}/{count}",
                columns(&text)
            );
        }
        // ...and the file-count fallback, which only shows when lines are zero.
        for count in counts {
            let status = WorkspaceStatus {
                lines_changed: 0,
                changed_files: count,
                ..status("w", severity, count, count)
            };
            let text = badge(&status);
            assert!(
                columns(&text) <= BADGE_COLUMNS,
                "badge {text:?} is {} columns for {severity:?}/{count} files",
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
fn detail_names_the_integration_ref_and_target_verdict() {
    let mut report = report();
    report.targets.push(TargetPrediction {
        workspace_id: "w1".to_string(),
        target_ref: Some("refs/remotes/origin/main".to_string()),
        verdict: TargetVerdict::Conflict,
        reason: None,
    });

    let text = detail(&report);
    assert!(
        text.contains("target refs/remotes/origin/main: conflict"),
        "{text}"
    );
}

#[test]
fn a_degraded_checkout_says_why() {
    let mut report = report();
    report.changes.push((
        "w3".to_string(),
        degraded(&format!(
            "{}: `wip/salvage` has no commits yet",
            git::DEGRADED_UNBORN
        )),
    ));

    let text = detail(&report);
    let worktree = line_index(&text, "salvage [no branch]");
    let note = line_index(&text, "degraded:");
    assert_eq!(
        note,
        worktree + 1,
        "the reason follows the worktree it explains"
    );

    let flat = flatten(&text);
    // git's own wording survives, because it names the branch involved...
    assert!(flat.contains("`wip/salvage` has no commits yet"), "{text}");
    // ...and the consequence is spelled out rather than left to the reader.
    // The consequence, not the cause: git's half already said "no commits yet",
    // and an explanation that repeats it back reads as a stutter in the pane.
    assert!(flat.contains("left out of pairing"), "{text}");
    assert!(flat.contains("nothing to merge against"), "{text}");
    // The machine-readable code itself is not user-facing text.
    assert!(!text.contains(git::DEGRADED_UNBORN), "{text}");
}

#[test]
fn every_degradation_code_gets_a_real_explanation() {
    // Each code must produce a distinct explanation. A missing arm would fall
    // through to git's raw text, which reads as an internal error string.
    let cases = [
        (git::DEGRADED_UNBORN, "left out of pairing"),
        (git::DEGRADED_BROKEN_HEAD, "left out of pairing"),
        (git::DEGRADED_MISSING_BASE_REF, "only uncommitted work"),
        (git::DEGRADED_NO_MERGE_BASE, "only uncommitted work"),
        (git::DEGRADED_UNMERGED, "advisory"),
    ];

    for (code, expected) in cases {
        let mut report = report();
        report
            .changes
            .push(("w3".to_string(), degraded(&format!("{code}: some detail"))));
        let flat = flatten(&detail(&report));
        assert!(
            flat.contains("some detail"),
            "{code}: git's detail was dropped"
        );
        assert!(flat.contains(expected), "{code}: no explanation in {flat}");
        assert!(!flat.contains(code), "{code}: raw code reached the view");
    }
}

#[test]
fn several_reasons_each_get_their_own_note() {
    let mut report = report();
    report.changes.push((
        "w3".to_string(),
        degraded(&format!(
            "{}: a merge is in progress; {}: `origin/HEAD` does not resolve",
            git::DEGRADED_UNMERGED,
            git::DEGRADED_MISSING_BASE_REF
        )),
    ));
    let text = detail(&report);
    assert_eq!(
        text.matches("degraded:").count(),
        2,
        "both reasons must be shown:\n{text}"
    );
}

#[test]
fn an_unrecognised_reason_is_shown_verbatim_rather_than_swallowed() {
    let mut report = report();
    report.changes.push((
        "w3".to_string(),
        degraded("some-future-code: a thing we do not know about yet"),
    ));
    let flat = flatten(&detail(&report));
    assert!(flat.contains("a thing we do not know about yet"), "{flat}");
}

#[test]
fn a_merge_in_progress_marks_the_pairing_advisory() {
    // A conflict warning computed from a tree full of conflict markers is not
    // the same claim as one computed from clean trees, and the user cannot
    // weigh it unless the view says so.
    let mut report = report();
    report.changes.push((
        "w1".to_string(),
        degraded(&format!(
            "{}: a merge is in progress",
            git::DEGRADED_UNMERGED
        )),
    ));

    let text = detail(&report);
    let flat = flatten(&text);
    assert!(flat.contains("advisory:"), "{text}");
    assert!(flat.contains("conflict markers"), "{text}");
    assert!(flat.contains("merge is in progress in api"), "{text}");

    // It belongs with the pairing it qualifies, above the verdicts it qualifies.
    let pair = line_index(&text, "api <-> ui");
    let advisory = line_index(&text, "advisory:");
    let first_verdict = line_index(&text, "src/collide.rs");
    assert!(pair < advisory && advisory < first_verdict, "{text}");
}

#[test]
fn a_clean_pairing_gets_no_advisory() {
    let mut report = report();
    report.changes.push((
        "w1".to_string(),
        degraded(&format!(
            "{}: `origin/HEAD` gone",
            git::DEGRADED_MISSING_BASE_REF
        )),
    ));
    assert!(!detail(&report).contains("advisory:"));
}

#[test]
fn an_approximate_pairing_says_the_merge_base_was_forced() {
    // `git merge-tree` needs a base, and a criss-cross history offers more than
    // one. Forcing a single base makes every verdict below an approximation, and
    // presenting it as final is a claim the prediction did not earn.
    let mut report = report();
    assert!(!detail(&report).contains("approximate:"), "not by default");

    report.pairings[0].approximate = true;
    let text = detail(&report);
    let flat = flatten(&text);
    assert!(flat.contains("approximate:"), "{text}");
    assert!(flat.contains("no single merge base"), "{text}");

    // Same placement rule as the advisory: above the verdicts it qualifies.
    let pair = line_index(&text, "api <-> ui");
    let note = line_index(&text, "approximate:");
    let first_verdict = line_index(&text, "src/collide.rs");
    assert!(pair < note && note < first_verdict, "{text}");
}

#[test]
fn a_checkout_with_no_change_set_falls_back_to_the_missing_branch() {
    // The base fixture carries no change sets at all, which is what the daemon
    // produces when the whole git pass failed.
    let text = detail(&report());
    let flat = flatten(&text);
    assert!(flat.contains("detached HEAD"), "{text}");
    assert!(flat.contains("branch lookup failed"), "{text}");
}

#[test]
fn a_healthy_checkout_gets_no_degraded_note() {
    let mut report = report();
    report.checkouts.retain(|c| c.branch.is_some());
    report.statuses.retain(|s| s.workspace_id != "w3");
    // A change set that is present and fine must not produce a note either.
    report
        .changes
        .push(("w1".to_string(), ChangeSet::default()));
    let text = detail(&report);
    assert!(!text.contains("degraded:"), "{text}");
    assert!(!text.contains("advisory:"), "{text}");
}

#[test]
fn long_paths_truncate_from_the_left() {
    let text = detail(&report());
    let line = line_with(&text, "very_long_module_name.rs");

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

// ---------------------------------------------------------------------------
// Width
// ---------------------------------------------------------------------------

#[test]
fn the_view_fits_eighty_columns() {
    let text = detail(&report());
    assert!(
        widest(&text) <= 80,
        "widest line is {}:\n{text}",
        widest(&text)
    );
    // ...and actually uses the room it has for the truncated path.
    let line = line_with(&text, "very_long_module_name.rs");
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
fn an_emoji_path_stays_inside_the_column_budget() {
    // `🚀` is East Asian Wide and `⚠️` takes emoji presentation because of the
    // variation selector after it; both are two columns. The renderer scored
    // each as one, so this exact line measured 41 columns in a 40-column pane —
    // and because the pane redraws in place without scrolling, one column of
    // overflow wraps the line and shunts every row below it down by one.
    for width in [30usize, 33, 40, 41, 60, 80, 120, 200] {
        let text = detail_at(&report(), width);
        let line = line_with(&text, ".svg");
        assert!(
            columns(line) <= width,
            "the emoji path is {} columns at width {width}: {line:?}",
            columns(line)
        );
    }
}

#[test]
fn a_variation_selector_is_measured_as_the_emoji_it_selects() {
    // `⚠` on its own is one column. `⚠` followed by U+FE0F is the emoji glyph,
    // which is two. Ignoring the selector under-measures by a column per emoji,
    // and since paths truncate from the LEFT it is the tail — the half that
    // always survives — where the error accumulates.
    let path = format!("assets/{}.svg", "\u{26a0}\u{fe0f}".repeat(12));
    let report = Report {
        checkouts: vec![
            checkout("w1", "api", Some("main"), None),
            checkout("w2", "ui", Some("main"), None),
        ],
        pairings: vec![pairing(
            "w1",
            "w2",
            vec![shared(&path, FileVerdict::Conflict)],
        )],
        statuses: Vec::new(),
        targets: Vec::new(),
        changes: Vec::new(),
    };

    // 24 real columns of emoji reads as 12 without the rule, so every width
    // where the renderer believes the path fits but it does not is a wrap.
    for width in [30usize, 33, 40, 50, 60, 80] {
        let text = detail_at(&report, width);
        let line = line_with(&text, ".svg");
        assert!(
            columns(line) <= width,
            "the selector path is {} columns at width {width}: {line:?}",
            columns(line)
        );
    }
}

#[test]
fn a_cjk_path_stays_inside_the_column_budget() {
    for width in [20usize, 24, 30, 40, 80, 200] {
        let text = detail_at(&report(), width);
        let line = line_with(&text, ".md");
        assert!(
            columns(line) <= width.max(20),
            "the CJK path is {} columns at width {width}: {line:?}",
            columns(line)
        );
    }
}

#[test]
fn no_line_anywhere_exceeds_the_width_it_was_given() {
    // The whole pane, at every width the plugin is ever asked for, measured by
    // the table oracle rather than by the renderer's own arithmetic.
    let mut report = report();
    report.pairings[0].approximate = true;
    report.statuses[2] = unknown("w3", 3);
    report.changes.push((
        "w1".to_string(),
        degraded(&format!(
            "{}: a merge is in progress",
            git::DEGRADED_UNMERGED
        )),
    ));

    for width in [1usize, 12, 20, 21, 24, 29, 30, 33, 40, 41, 60, 80, 120, 200] {
        let text = detail_at(&report, width);
        let cap = width.max(20);
        assert!(
            widest(&text) <= cap,
            "width {width} produced a {}-column line:\n{text}",
            widest(&text)
        );
    }
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
fn a_narrow_pane_keeps_the_tail_of_the_path_rather_than_eliding_both_ends() {
    // At twenty columns the eight-column verdict word left eight for the path,
    // out of a sixteen-column prefix — so the line was built at twenty-four
    // columns and then truncated from the right, eliding the tail that
    // `truncate_left` had just worked to preserve. `…ti…` identifies nothing.
    let text = detail_at(&report(), 20);
    let line = line_with(&text, "name.rs");

    assert!(
        line.ends_with("name.rs"),
        "the informative tail was cut: {line:?}"
    );
    assert_eq!(
        line.matches('\u{2026}').count(),
        1,
        "a path elided at both ends says nothing: {line:?}"
    );
    // Eight columns of path was what the old sixteen-column prefix left; the
    // narrow prefix has to do materially better than that or it is not worth
    // dropping the word for.
    let path = line.trim_start().trim_start_matches(|c| c != '\u{2026}');
    assert!(
        columns(path) >= 12,
        "only {} columns of path at 20: {line:?}",
        columns(path)
    );
    // The mark still carries the verdict, and the legend still spells it out.
    assert!(text.contains("legend"), "{text}");
}

#[test]
fn the_verdict_word_comes_back_once_there_is_room_for_it() {
    // The narrow layout is a concession, not the default. At any ordinary pane
    // width the word is there next to the mark.
    let wide = detail_at(&report(), 40);
    assert!(
        line_with(&wide, "src/git.rs").contains("\u{2718} conflict"),
        "{wide}"
    );

    let narrow = detail_at(&report(), 24);
    let line = line_with(&narrow, "src/git.rs");
    assert!(!line.contains("conflict"), "{line:?}");
    assert!(line.contains('\u{2718}'), "{line:?}");
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

// ---------------------------------------------------------------------------
// The worktree line
// ---------------------------------------------------------------------------

#[test]
fn a_workspace_badge_rides_along_in_the_detail_view() {
    let text = detail(&report());
    let line = line_with(&text, "api [");
    assert!(line.contains("\u{2718} 3"), "{line:?}");
}

#[test]
fn the_badge_survives_a_narrow_pane_and_a_long_label() {
    // The badge is the highest-value token on the line and it sits at the far
    // right, so blind right-truncation dropped exactly the thing the pane exists
    // to show: a conflicting worktree rendered with no mark on it at all, and
    // nothing but an ellipsis to say anything was missing.
    let mut report = report();
    report.checkouts[0].workspace_label =
        "refactor-the-entire-analysis-and-rendering-pipeline".to_string();
    report.checkouts[0].branch = Some("feature/refactor-the-entire-analysis-pipeline".to_string());
    // Only the worktree lines, so the label cannot be found on a pairing head
    // instead. The badge comes from the status either way.
    report.pairings.clear();

    for width in [20usize, 24, 40, 60, 80, 120] {
        let text = detail_at(&report, width);
        let line = line_with(&text, "refactor-the");
        assert!(
            line.contains('\u{2718}'),
            "the badge was truncated away at width {width}: {line:?}"
        );
        assert!(
            line.ends_with("\u{2718} 3"),
            "the badge must be the last thing on the line: {line:?}"
        );
        assert!(
            columns(line) <= width.max(20),
            "width {width} produced a {}-column worktree line: {line:?}",
            columns(line)
        );
    }
}

#[test]
fn a_runaway_worktree_keeps_its_badge_and_drops_the_word_instead() {
    // When the line will not fit, the `runaway` word is the slack. The mark is
    // not: it is the only thing on screen saying this workspace is the problem.
    let mut report = report();
    report.statuses[2] = runaway("w3", 4_100);

    let wide = detail_at(&report, 80);
    let roomy = line_with(&wide, "salvage [");
    assert!(roomy.contains("runaway"), "{roomy:?}");
    assert!(roomy.ends_with("\u{26a0} 4.1k"), "{roomy:?}");

    let cramped = detail_at(&report, 20);
    let line = line_with(&cramped, "salvage");
    assert!(
        line.ends_with("\u{26a0} 4.1k"),
        "the badge went before the word did: {line:?}"
    );
    assert!(columns(line) <= 20, "{} columns: {line:?}", columns(line));
}

#[test]
fn a_worktree_with_no_status_still_renders() {
    let mut report = report();
    report.statuses.clear();
    let text = detail_at(&report, 40);
    assert!(text.contains("api ["), "{text}");
    assert!(widest(&text) <= 40, "{text}");
}

// ---------------------------------------------------------------------------
// Ordering, notes, and the degenerate reports
// ---------------------------------------------------------------------------

#[test]
fn the_pairing_that_matters_sorts_to_the_top() {
    // The pane does not scroll and `draw` cuts the tail, so with eight worktrees
    // an alphabetical order put the one conflicting pair on line 95 of 99 —
    // behind six screens of clean overlaps that nobody needs to read.
    let labels = ["alpha", "bravo", "charlie", "yankee", "zulu"];
    let checkouts: Vec<Checkout> = labels
        .iter()
        .enumerate()
        .map(|(i, l)| checkout(&format!("w{i}"), l, Some("main"), None))
        .collect();

    let mut pairings = Vec::new();
    for i in 0..labels.len() {
        for j in (i + 1)..labels.len() {
            // yankee (w3) vs zulu (w4) is the only genuine conflict, and the
            // last pair alphabetically.
            let verdict = if i == 3 && j == 4 {
                FileVerdict::Conflict
            } else {
                FileVerdict::Overlap
            };
            pairings.push(pairing(
                &format!("w{i}"),
                &format!("w{j}"),
                vec![shared("src/shared.rs", verdict)],
            ));
        }
    }

    let report = Report {
        checkouts,
        pairings,
        statuses: Vec::new(),
        targets: Vec::new(),
        changes: Vec::new(),
    };
    let text = detail_at(&report, 80);

    assert_eq!(
        line_index(&text, "yankee <-> zulu") + 1,
        line_index(&text, "conflict"),
        "the conflicting pair must be the first one shown:\n{text}"
    );
    assert!(
        line_index(&text, "yankee <-> zulu") < line_index(&text, "alpha <-> bravo"),
        "worst first, not alphabetically first:\n{text}"
    );
}

#[test]
fn an_undecided_pairing_outranks_a_clean_one() {
    let report = Report {
        checkouts: vec![
            checkout("w1", "alpha", Some("main"), None),
            checkout("w2", "bravo", Some("main"), None),
            checkout("w3", "yankee", Some("main"), None),
        ],
        pairings: vec![
            pairing("w1", "w2", vec![shared("a.rs", FileVerdict::Overlap)]),
            pairing("w1", "w3", vec![shared("b.rs", FileVerdict::Unknown)]),
        ],
        statuses: Vec::new(),
        targets: Vec::new(),
        changes: Vec::new(),
    };
    let text = detail_at(&report, 80);
    assert!(
        line_index(&text, "alpha <-> yankee") < line_index(&text, "alpha <-> bravo"),
        "an unknown verdict outranks a known-clean one:\n{text}"
    );
}

#[test]
fn the_notes_sit_under_the_title_where_a_short_pane_cannot_cut_them() {
    // `draw` truncates from the bottom. A note saying a checkout could not be
    // read is the only signal that the clean-looking report below it is
    // incomplete, so putting it last made it the first casualty.
    let notes = vec!["/repos/app/.worktrees/ui: git rev-parse failed".to_string()];
    let text = detail_with_notes(&report(), &notes, 80);

    let title = line_index(&text, "collide");
    let note = line_index(&text, "git rev-parse failed");
    let first_repo = line_index(&text, "repo /repos/app");
    assert!(title < note && note < first_repo, "{text}");
    assert!(text.contains("notes"), "{text}");

    // And with no notes the pane is exactly what it always was.
    assert_eq!(detail_with_notes(&report(), &[], 80), detail(&report()));
}

#[test]
fn notes_reach_an_otherwise_empty_pane() {
    // "No git-backed workspaces are open" and "every checkout failed to read"
    // produce the same report. The notes are the only thing that tells them
    // apart, so they must survive the empty-report early return.
    let notes = vec!["skipping /repos/app: not a git repository".to_string()];
    let text = detail_with_notes(&Report::default(), &notes, 80);
    assert!(text.contains("not a git repository"), "{text}");
    assert!(
        text.contains("No git-backed workspaces are open."),
        "{text}"
    );
    assert!(widest(&text) <= 80, "{text}");
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
fn a_lone_worktree_says_there_was_nothing_to_compare() {
    // "Nothing shared" is a comparison result. A repo with one worktree has no
    // comparison to report, and reporting the negative result of a comparison
    // that never happened is a small lie the user has no way to detect.
    let report = Report {
        checkouts: vec![checkout("w1", "solo", Some("main"), None)],
        statuses: vec![status("w1", Severity::Clean, 0, 0)],
        ..Report::default()
    };
    let flat = flatten(&detail(&report));
    assert!(flat.contains("only one worktree open"), "{flat}");
    assert!(flat.contains("nothing to compare"), "{flat}");
    assert!(
        !flat.contains("no files shared with a sibling worktree"),
        "{flat}"
    );
}

#[test]
fn a_repo_whose_only_sibling_is_unpairable_says_that_instead() {
    // Two worktrees, but one has no commit, so it was excluded from pairing
    // rather than compared. Neither of the other two sentences is true here.
    let mut report = report();
    report.checkouts.truncate(2);
    report.pairings.clear();
    report.statuses.truncate(2);
    report.changes.push((
        "w2".to_string(),
        degraded(&format!(
            "{}: `feature/ui` has no commits yet",
            git::DEGRADED_UNBORN
        )),
    ));

    let flat = flatten(&detail(&report));
    assert!(
        flat.contains("no sibling worktree here can be compared"),
        "{flat}"
    );
    assert!(!flat.contains("only one worktree open"), "{flat}");
    assert!(
        !flat.contains("no files shared with a sibling worktree"),
        "{flat}"
    );
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
fn the_unknown_legend_keys_off_the_severity_as_well_as_the_verdicts() {
    // A checkout the git pass could not read at all is `Severity::Unknown`
    // without any single shared file being unknown, so its `?` badge would
    // otherwise appear in the pane with nothing explaining it.
    let mut report = report();
    for pairing in &mut report.pairings {
        pairing.shared.retain(|f| f.verdict != FileVerdict::Unknown);
    }
    report.statuses[2] = unknown("w3", 0);

    let text = detail(&report);
    assert!(line_with(&text, "salvage [").contains('?'), "{text}");
    assert!(text.contains("prediction unavailable"), "{text}");
}

#[test]
fn a_lines_based_runaway_badge_and_legend_name_lines_as_the_default_unit() {
    assert_eq!(badge(&runaway("w", 4_100)), "\u{26a0} 4.1k");

    let mut with_runaway = report();
    with_runaway.statuses[2] = runaway("w3", 4_100);
    let text = detail(&with_runaway);
    assert_eq!(
        line_with(&text, "runaway change set"),
        "  \u{26a0}  runaway change set (lines, or f = files)"
    );
}

#[test]
fn a_file_count_runaway_badge_and_legend_explain_the_f_suffix() {
    assert_eq!(badge(&file_runaway("w", 60)), "\u{26a0} 60f");

    let mut with_runaway = report();
    with_runaway.statuses[2] = file_runaway("w3", 60);
    let text = detail(&with_runaway);
    assert_eq!(
        line_with(&text, "runaway change set"),
        "  \u{26a0}  runaway change set (lines, or f = files)"
    );
    assert!(
        line_with(&text, "salvage [").contains("\u{26a0} 60f"),
        "{text}"
    );
}

#[test]
fn a_narrow_pane_preserves_the_complete_runaway_explanation() {
    let mut with_runaway = report();
    with_runaway.statuses[2] = runaway("w3", 4_100);

    for width in [20, 24, 40] {
        let text = detail_at(&with_runaway, width);
        assert!(
            flatten(&text).contains("runaway change set (lines, or f = files)"),
            "the explanation was cut at width {width}:\n{text}"
        );
        assert!(
            widest(&text) <= width,
            "width {width} produced a {}-column line:\n{text}",
            widest(&text)
        );
    }
}

#[test]
fn the_runaway_legend_appears_only_when_a_workspace_is_runaway() {
    let explanation = "runaway change set (lines, or f = files)";
    assert!(!detail(&report()).contains(explanation));

    let mut with_runaway = report();
    with_runaway.statuses[2] = file_runaway("w3", 60);
    let text = detail(&with_runaway);
    assert!(text.contains(explanation), "{text}");
}

// ---------------------------------------------------------------------------
// Model contract these renderings rely on
// ---------------------------------------------------------------------------

#[test]
fn severity_still_maps_onto_the_five_documented_tokens() {
    // The renderer deliberately encodes no colour, on the strength of this.
    assert_eq!(Severity::Clean.token_name(), "collide_clean");
    assert_eq!(Severity::Overlap.token_name(), "collide_overlap");
    assert_eq!(Severity::Runaway.token_name(), "collide_runaway");
    assert_eq!(Severity::Unknown.token_name(), "collide_unknown");
    assert_eq!(Severity::Conflict.token_name(), "collide_conflict");
    assert_eq!(Severity::ALL_TOKENS.len(), 5);
}

#[test]
fn a_change_set_summarises_its_own_volume() {
    // Not a rendering test as such, but the runaway badge is only meaningful
    // if this stays additive.
    let change_set = ChangeSet {
        paths: vec![ChangedPath::new("src/render.rs", ChangeKind::Unstaged)],
        lines_added: 4_000,
        lines_removed: 100,
        ..ChangeSet::default()
    };
    assert_eq!(change_set.lines_changed(), 4_100);
    assert_eq!(abbreviate(change_set.lines_changed()), "4.1k");
}
