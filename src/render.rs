//! Rendering: the badge string that rides a workspace token, and the live
//! detail pane.
//!
//! Nothing here emits colour. herdr renders a token's *value* as flat text and
//! cannot colour by content, so severity travels in the token *name*
//! (`Severity::token_name`) and the strings below stay plain — see
//! `docs/herdr-protocol.md`. The detail pane is likewise plain text: there is no
//! colour library in this crate's dependency set.
//!
//! The formatting half of the module is pure and is what `tests/render.rs`
//! exercises. Only `run_watch` talks to herdr or git.
//!
//! # Width model
//!
//! Everything here is measured in terminal display columns, never in bytes or
//! `chars()`. [`char_columns`] scores an East-Asian-*Ambiguous* character — `…`
//! and `·` among them — as one column. That is right for the common default and
//! one column short in a terminal explicitly configured ambiguous-wide, which is
//! a setting some CJK users enable; a truncated line would then be one column
//! over its budget. Taking the other side would cost a column on every line for
//! every other user, so the narrow reading stands.

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;
use crate::git;
use crate::model::{
    ChangeSet, Checkout, FileVerdict, Pairing, RepoKey, Report, Severity, WorkspaceStatus,
};
use crate::Result;

/// A badge sits in the sidebar next to a branch name. Six display columns is
/// the budget; anything longer starts pushing the branch out of view.
pub const BADGE_COLUMNS: usize = 6;

/// Width assumed when the real terminal width is unknown.
pub const DEFAULT_COLUMNS: usize = 80;

/// Below this the detail view stops trying to stay pretty, but it still never
/// emits a line wider than the width it was given.
pub const MIN_COLUMNS: usize = 20;

/// Maximum amount of a conflicted blob `--why` will put on a terminal.
pub const CONTENT_MAX_LINES: usize = 200;
/// Maximum display width of each line emitted by `--why`.
pub const CONTENT_MAX_COLUMNS: usize = 160;

const CONFLICT_MARK: &str = "\u{2718}"; // ✘
const OVERLAP_MARK: &str = "\u{29c9}"; // ⧉
const RUNAWAY_MARK: &str = "\u{26a0}"; // ⚠
const UNKNOWN_MARK: &str = "?";
const ELLIPSIS: char = '\u{2026}'; // …
/// Variation selector 16. A scalar followed by this one takes emoji
/// presentation, which is two columns wide even when the scalar alone is one.
const VS16: char = '\u{fe0f}';

const TITLE: &str = "collide \u{b7} shared files";
const NO_BRANCH: &str = "no branch";
const NO_AGENT: &str = "(no agent)";
const RUNAWAY_WORD: &str = "  runaway";

/// `"    "` + mark + `" "` + an eight-column verdict word + `"  "`.
const FILE_PREFIX_COLUMNS: usize = 16;

/// `"    "` + mark + `"  "`. Below [`NARROW_COLUMNS`] the verdict word costs
/// more than it is worth: spending nine of twenty columns naming what the mark
/// already says leaves too little for the path, and both ends of it end up
/// elided.
const NARROW_FILE_PREFIX_COLUMNS: usize = 7;

/// Width below which the verdict word is dropped from a file line.
const NARROW_COLUMNS: usize = 30;

/// Smallest label stub a worktree line will settle for before it starts
/// dropping lower-value tokens from the tail instead.
const LABEL_MIN_COLUMNS: usize = 12;

/// Last resort, used only when the report carries no change set at all for a
/// checkout, so there is no reason code to explain.
const NO_BRANCH_NOTE: &str = "degraded: no branch reported for this checkout \u{2014} it is a \
     detached HEAD, or the worktree branch lookup failed.";

// ---------------------------------------------------------------------------
// Badge
// ---------------------------------------------------------------------------

/// Badge text for one workspace, e.g. `✘ 2`, `⧉ 3`, `? 1` or `⚠ 60f`. Severity
/// itself is carried by the token *name*, not this string.
///
/// A clean workspace renders the empty string, which the daemon treats as
/// "clear the badge" rather than "write an empty badge".
pub fn badge(status: &WorkspaceStatus) -> String {
    let (mark, magnitude) = match status.severity {
        Severity::Clean => return String::new(),
        Severity::Conflict => (CONFLICT_MARK, count(status.conflict_count)),
        // A failed prediction is its own severity, not a quiet overlap. The
        // count can legitimately be zero: a checkout the git pass could not
        // read at all is unknown without any shared file to point at, and the
        // bare mark is then the honest badge.
        Severity::Unknown => (UNKNOWN_MARK, count(status.unknown_count)),
        Severity::Runaway => (RUNAWAY_MARK, runaway_magnitude(status)),
        Severity::Overlap => (OVERLAP_MARK, count(status.overlap_count)),
    };

    // Zero has nothing to say; the mark alone is the whole message.
    let text = match magnitude {
        Some(magnitude) => format!("{mark} {magnitude}"),
        None => mark.to_string(),
    };

    // Belt and braces: every magnitude above is bounded at four columns, so
    // this only ever fires if the marks change.
    truncate_right(&text, BADGE_COLUMNS)
}

/// A shared-file count, or `None` when there is nothing worth printing.
fn count(n: usize) -> Option<String> {
    (n > 0).then(|| abbreviate(n as u64))
}

/// What a runaway badge reports.
///
/// A runaway is measured in change-set size, not in shared files: the whole
/// point of the severity is a workspace that has grown huge on its own, usually
/// sharing nothing at all with its siblings. Either threshold can trip it
/// though, and a workspace that crossed the *file* threshold can carry no
/// counted lines at all — hundreds of untracked binaries, say. Rendering a bare
/// `⚠` there tells the user only that something is wrong, so the file count
/// stands in, with a `f` so the two units cannot be confused.
fn runaway_magnitude(status: &WorkspaceStatus) -> Option<String> {
    if status.lines_changed > 0 {
        return Some(abbreviate(status.lines_changed));
    }
    (status.changed_files > 0).then(|| abbreviate_files(status.changed_files))
}

/// Compact magnitude, never wider than four display columns: `999`, `1.2k`,
/// `12k`, `999k`, `1.2M`, `999M`, `1G+`. Rounding is truncation, so the badge
/// never overstates.
pub fn abbreviate(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=9_999 => format!("{}.{}k", n / 1_000, (n % 1_000) / 100),
        10_000..=999_999 => format!("{}k", n / 1_000),
        1_000_000..=9_999_999 => format!("{}.{}M", n / 1_000_000, (n % 1_000_000) / 100_000),
        10_000_000..=999_999_999 => format!("{}M", n / 1_000_000),
        _ => "1G+".to_string(),
    }
}

/// Compact *file* count, never wider than four display columns **including** the
/// `f` that distinguishes it from a line count: `999f`, `99kf`, `99k+`. Same
/// truncating arithmetic as [`abbreviate`], so it never overstates either.
pub fn abbreviate_files(n: usize) -> String {
    match n {
        0..=999 => format!("{n}f"),
        1_000..=99_999 => format!("{}kf", n / 1_000),
        _ => "99k+".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Detail view
// ---------------------------------------------------------------------------

/// Full-screen detail view of one report, laid out for an 80-column pane.
pub fn detail(report: &Report) -> String {
    detail_at(report, DEFAULT_COLUMNS)
}

/// `detail`, at an explicit width. No line in the result exceeds `columns`
/// display columns (or `MIN_COLUMNS`, whichever is larger).
pub fn detail_at(report: &Report, columns: usize) -> String {
    detail_with_notes(report, &[], columns)
}

/// [`detail_at`], with the gathering pass's non-fatal notes placed directly
/// under the title.
///
/// They go first because [`draw`] truncates from the *bottom*: a note saying a
/// checkout could not be read is the only signal that the clean-looking report
/// below it is incomplete, and putting it last made it the first thing a short
/// pane threw away.
pub fn detail_with_notes(report: &Report, notes: &[String], columns: usize) -> String {
    let width = columns.max(MIN_COLUMNS);
    let mut out = String::new();
    push_line(&mut out, TITLE, width);
    out.push_str(&notes_section(notes, width));

    if report.checkouts.is_empty() {
        out.push('\n');
        push_wrapped(
            &mut out,
            "",
            "",
            "No git-backed workspaces are open.",
            width,
        );
        return out;
    }

    let status_by_id: BTreeMap<&str, &WorkspaceStatus> = report
        .statuses
        .iter()
        .map(|s| (s.workspace_id.as_str(), s))
        .collect();
    let checkout_by_id: BTreeMap<&str, &Checkout> = report
        .checkouts
        .iter()
        .map(|c| (c.workspace_id.as_str(), c))
        .collect();

    let mut repos: BTreeMap<&RepoKey, Vec<&Checkout>> = BTreeMap::new();
    for checkout in &report.checkouts {
        repos.entry(&checkout.repo_key).or_default().push(checkout);
    }

    let mut saw_shared = false;
    let mut saw_runaway = false;
    // A workspace can be unknown without any single file being unknown — a
    // checkout the git pass could not read at all — so the legend keys off the
    // severity as well as the per-file verdicts.
    let mut saw_unknown = report
        .statuses
        .iter()
        .any(|s| s.severity == Severity::Unknown);

    for (repo_key, mut group) in repos {
        group.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));

        out.push('\n');
        let root = group[0].repo_root.to_string_lossy();
        let head = format!(
            "repo {}",
            truncate_left(&root, width.saturating_sub(5).max(8))
        );
        push_line(&mut out, &head, width);

        for checkout in &group {
            let status = status_by_id.get(checkout.workspace_id.as_str()).copied();
            push_line(&mut out, &worktree_line(checkout, status, width), width);
            if status.map(|s| s.runaway).unwrap_or(false) {
                saw_runaway = true;
            }
            for note in degraded_notes(report.change_set(&checkout.workspace_id), checkout) {
                push_wrapped(&mut out, "      ", "      ", &note, width);
            }
        }

        let mut pairings: Vec<&Pairing> = report
            .pairings
            .iter()
            .filter(|p| pairing_repo(p, &checkout_by_id) == Some(repo_key))
            .filter(|p| !p.shared.is_empty())
            .collect();
        // Worst first. The pane does not scroll and `draw` cuts the tail, so
        // with twenty worktrees — a hundred and ninety pairings — an
        // alphabetical order put the one conflicting pair off the bottom of the
        // screen behind six screens of clean overlaps.
        pairings.sort_by_key(|p| {
            (
                std::cmp::Reverse(p.conflicts()),
                std::cmp::Reverse(p.unknowns()),
                display_label(&p.left_workspace_id, &checkout_by_id),
                display_label(&p.right_workspace_id, &checkout_by_id),
            )
        });

        if pairings.is_empty() {
            // "Nothing shared" is a comparison result. A repo with only one
            // worktree that can be paired has no comparison to report, and the
            // two must not read the same.
            let pairable = group
                .iter()
                .filter(|c| {
                    report
                        .change_set(&c.workspace_id)
                        .map(crate::collide::pairable)
                        // A checkout with no change set at all was not judged
                        // unpairable; it was simply not read.
                        .unwrap_or(true)
                })
                .count();
            let message = if group.len() < 2 {
                "only one worktree open for this repository \u{2014} nothing to compare"
            } else if pairable < 2 {
                "no sibling worktree here can be compared \u{2014} see the notes above"
            } else {
                "no files shared with a sibling worktree"
            };
            push_wrapped(&mut out, "  ", "  ", message, width);
            continue;
        }

        for pairing in pairings {
            saw_shared = true;
            out.push('\n');
            let pair_head = format!(
                "  {} <-> {}",
                display_label(&pairing.left_workspace_id, &checkout_by_id),
                display_label(&pairing.right_workspace_id, &checkout_by_id),
            );
            push_line(&mut out, &pair_head, width);

            // A side with a merge in progress is snapshotted from files that
            // still contain conflict markers, so every verdict below was
            // computed against that. Saying "conflict" without saying that
            // would be a warning the user cannot weigh.
            for side in [&pairing.left_workspace_id, &pairing.right_workspace_id] {
                if !is_unmerged(report.change_set(side)) {
                    continue;
                }
                let label = display_label(side, &checkout_by_id);
                push_wrapped(
                    &mut out,
                    "    ",
                    "    ",
                    &format!(
                        "advisory: a merge is in progress in {label}, so these verdicts were \
                         computed from a tree that still contains conflict markers."
                    ),
                    width,
                );
            }

            let uncomparable_submodules = uncomparable_submodule_paths(report, pairing);
            if !uncomparable_submodules.is_empty() {
                let paths = uncomparable_submodules
                    .iter()
                    .map(|path| format!("`{path}`"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let scope = if uncomparable_submodules.len() == 1 {
                    "this path"
                } else {
                    "these paths"
                };
                push_wrapped(
                    &mut out,
                    "    ",
                    "    ",
                    &format!(
                        "unknown: submodule contents differ at {paths}; the snapshot records the \
                         submodule's committed pointer rather than its contents, so a clean merge \
                         for {scope} was never checked."
                    ),
                    width,
                );
            }

            // The same reasoning as the advisory above: a verdict computed
            // against a merge base that had to be guessed at is a weaker claim
            // than one computed against the real base, and only the pane can
            // say so.
            if pairing.approximate {
                push_wrapped(
                    &mut out,
                    "    ",
                    "    ",
                    "approximate: these two histories offer no single merge base, so one was \
                     forced and the verdicts below approximate what a real merge would do.",
                    width,
                );
            }

            let mut files: Vec<&crate::model::SharedFile> = pairing.shared.iter().collect();
            // Conflicts first, then the ones we could not decide, then plain
            // overlaps; alphabetical within each band.
            files.sort_by(|a, b| {
                verdict_rank(a.verdict)
                    .cmp(&verdict_rank(b.verdict))
                    .then_with(|| a.path.cmp(&b.path))
            });

            // Narrow panes drop the verdict word; the mark says the same thing
            // in one column instead of nine, and the legend spells it out.
            let narrow = width < NARROW_COLUMNS;
            let prefix_columns = if narrow {
                NARROW_FILE_PREFIX_COLUMNS
            } else {
                FILE_PREFIX_COLUMNS
            };
            let path_budget = width.saturating_sub(prefix_columns).max(4);
            for file in files {
                if file.verdict == FileVerdict::Unknown {
                    saw_unknown = true;
                }
                let (mark, word) = verdict_marks(file.verdict);
                // Paths truncate from the LEFT: the tail is the informative half.
                let path = truncate_left(&file.path, path_budget);
                let line = if narrow {
                    format!("    {mark}  {path}")
                } else {
                    format!("    {mark} {word:<8}  {path}")
                };
                push_line(&mut out, &line, width);
            }
        }
    }

    if saw_shared {
        out.push('\n');
        push_line(&mut out, "legend", width);
        // Explanations are the point of the legend, so wrap them rather than
        // let `push_line` truncate their meaning in a narrow pane.
        push_wrapped(
            &mut out,
            &format!("  {CONFLICT_MARK}  "),
            "     ",
            "conflict predicted on merge",
            width,
        );
        push_wrapped(
            &mut out,
            &format!("  {OVERLAP_MARK}  "),
            "     ",
            "same file, merges clean",
            width,
        );
        if saw_unknown {
            push_wrapped(
                &mut out,
                &format!("  {UNKNOWN_MARK}  "),
                "     ",
                "conflict prediction unavailable",
                width,
            );
        }
        // The runaway mark reaches the worktree lines through `badge`, so it
        // needs explaining wherever it can appear. `⚠ 4.1k` counts lines and
        // `⚠ 60f` counts files, and nothing else on screen says which.
        if saw_runaway {
            push_wrapped(
                &mut out,
                &format!("  {RUNAWAY_MARK}  "),
                "     ",
                "runaway change set (lines, or f = files)",
                width,
            );
        }
    }

    out
}

fn sort_key(checkout: &Checkout) -> (&str, &str) {
    (label_of(checkout), checkout.workspace_id.as_str())
}

fn label_of(checkout: &Checkout) -> &str {
    let label = checkout.workspace_label.trim();
    if label.is_empty() {
        &checkout.workspace_id
    } else {
        label
    }
}

fn display_label(workspace_id: &str, by_id: &BTreeMap<&str, &Checkout>) -> String {
    by_id
        .get(workspace_id)
        .map(|c| label_of(c).to_string())
        .unwrap_or_else(|| workspace_id.to_string())
}

fn pairing_repo<'a>(
    pairing: &Pairing,
    by_id: &BTreeMap<&str, &'a Checkout>,
) -> Option<&'a RepoKey> {
    by_id
        .get(pairing.left_workspace_id.as_str())
        .or_else(|| by_id.get(pairing.right_workspace_id.as_str()))
        .map(|c| &c.repo_key)
}

/// One worktree's line, composed against `width` rather than truncated into it.
///
/// The badge is the highest-value token on the line and it lives at the far
/// right, so blind right-truncation dropped exactly the thing the pane exists to
/// show — a 40-column pane would render a conflicting worktree with no mark on
/// it at all. Here the badge is reserved first, the `runaway` word is the slack
/// that gets dropped when the line will not fit, and the identity on the left is
/// truncated around what is left.
fn worktree_line(checkout: &Checkout, status: Option<&WorkspaceStatus>, width: usize) -> String {
    let branch = checkout
        .branch
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .unwrap_or(NO_BRANCH);
    let agent = match checkout
        .agent
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
    {
        Some(agent) => format!("@{agent}"),
        None => NO_AGENT.to_string(),
    };

    let head = format!("  {} [{branch}] {agent}", label_of(checkout));
    let Some(status) = status else {
        return truncate_right(&head, width);
    };

    let badge_text = badge(status);
    let badge_tail = if badge_text.is_empty() {
        String::new()
    } else {
        format!("  {badge_text}")
    };
    let badge_columns = display_width(&badge_tail);

    // Everything the badge does not claim is available to the identity and, if
    // it still fits afterwards, the `runaway` word.
    let rest = width.saturating_sub(badge_columns);
    let runaway_columns = if status.runaway {
        display_width(RUNAWAY_WORD)
    } else {
        0
    };
    let floor = LABEL_MIN_COLUMNS.min(rest);
    let keep_runaway = status.runaway && rest.saturating_sub(runaway_columns) >= floor;

    let head_budget = if keep_runaway {
        rest - runaway_columns
    } else {
        rest
    };

    let mut line = truncate_right(&head, head_budget);
    if keep_runaway {
        line.push_str(RUNAWAY_WORD);
    }
    line.push_str(&badge_tail);
    line
}

/// Why this checkout could only be read in part, one note per reason.
///
/// `git` writes `degraded_reason` as `code: human text`, joined with `"; "`
/// when there is more than one. The codes are the stable half, so each is
/// matched and turned into a full explanation; git's own text is kept because
/// it names the branch or ref involved. An unrecognised code falls through to
/// git's text verbatim rather than being swallowed.
fn degraded_notes(change_set: Option<&ChangeSet>, checkout: &Checkout) -> Vec<String> {
    let Some(change_set) = change_set else {
        // No change set at all: the only signal left is the missing branch.
        return if checkout.branch.is_none() {
            vec![NO_BRANCH_NOTE.to_string()]
        } else {
            Vec::new()
        };
    };

    if !change_set.degraded {
        return Vec::new();
    }

    let Some(reason) = change_set.degraded_reason.as_deref() else {
        return vec!["degraded: this checkout could only be read in part.".to_string()];
    };

    reason
        .split("; ")
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| format!("degraded: {}.", explain_reason(part)))
        .collect()
}

/// Splits one `code: detail` reason and expands the code into a consequence the
/// reader can act on.
fn explain_reason(reason: &str) -> String {
    let (code, detail) = match reason.split_once(": ") {
        Some((code, detail)) => (code, detail.trim()),
        None => (reason, ""),
    };

    let consequence = match code {
        // Both of these follow a git message that already says the checkout
        // has no commit, so the explanation states the consequence rather than
        // repeating the cause back at the reader.
        git::DEGRADED_UNBORN => "left out of pairing: there is nothing to merge against",
        git::DEGRADED_BROKEN_HEAD => {
            "left out of pairing: its HEAD is broken, so there is nothing to merge against"
        }
        git::DEGRADED_MISSING_BASE_REF => {
            "so the committed half of this change set could not be measured, and \
             only uncommitted work is counted"
        }
        git::DEGRADED_NO_MERGE_BASE => {
            "so there is no range to measure against, and only uncommitted work \
             is counted"
        }
        git::DEGRADED_UNMERGED => {
            "this side is snapshotted with its conflict markers still in place, so any \
             prediction involving it is advisory"
        }
        // An unfamiliar code is still worth showing; it is git's own wording.
        _ => return reason.to_string(),
    };

    if detail.is_empty() {
        consequence.to_string()
    } else {
        format!("{detail} \u{2014} {consequence}")
    }
}

fn is_unmerged(change_set: Option<&ChangeSet>) -> bool {
    change_set
        .and_then(|set| set.degraded_reason.as_deref())
        .map(|reason| {
            reason
                .split("; ")
                .any(|part| part.trim_start().starts_with(git::DEGRADED_UNMERGED))
        })
        .unwrap_or(false)
}

fn uncomparable_submodule_paths<'a>(report: &Report, pairing: &'a Pairing) -> Vec<&'a str> {
    pairing
        .shared
        .iter()
        .filter(|shared| {
            // A divergent gitlink is a real comparison even when its checkout is dirty,
            // so this note must follow the verdict rather than the status flag alone.
            shared.verdict == FileVerdict::Unknown
                && [
                    pairing.left_workspace_id.as_str(),
                    pairing.right_workspace_id.as_str(),
                ]
                .iter()
                .any(|workspace_id| {
                    report.change_set(workspace_id).is_some_and(|set| {
                        set.paths.iter().any(|changed| {
                            changed.path == shared.path && changed.submodule_contents_uncomparable
                        })
                    })
                })
        })
        .map(|shared| shared.path.as_str())
        .collect()
}

fn verdict_rank(verdict: FileVerdict) -> u8 {
    match verdict {
        FileVerdict::Conflict => 0,
        FileVerdict::Unknown => 1,
        FileVerdict::Overlap => 2,
    }
}

fn verdict_marks(verdict: FileVerdict) -> (&'static str, &'static str) {
    match verdict {
        FileVerdict::Conflict => (CONFLICT_MARK, "conflict"),
        FileVerdict::Unknown => (UNKNOWN_MARK, "unknown"),
        FileVerdict::Overlap => (OVERLAP_MARK, "overlap"),
    }
}

// ---------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------

/// One indivisible piece of a string, and the display columns it occupies.
///
/// A unit is a base scalar plus every zero-width scalar that follows it —
/// combining marks, variation selectors, zero-width joiners — or a whole CSI
/// escape sequence, which occupies nothing. Measuring and cutting in units
/// rather than in `char`s is what makes the two agree: a `char`-wise cut can
/// strand a combining mark on the ellipsis or split an escape sequence, and a
/// `char`-wise measure cannot see that `⚠` followed by U+FE0F is two columns
/// wide and not one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Unit<'a> {
    text: &'a str,
    columns: usize,
}

/// Splits `text` into measurable units, left to right.
fn units(text: &str) -> Vec<Unit<'_>> {
    let mut out = Vec::new();
    let mut rest = text;

    while !rest.is_empty() {
        let mut chars = rest.char_indices();
        let (_, first) = chars.next().expect("rest is not empty");

        // A CSI sequence draws nothing, and cutting inside one would leave the
        // terminal reading the tail of it as text.
        if first == '\u{1b}' {
            let mut end = first.len_utf8();
            if let Some((_, '[')) = chars.next() {
                end += 1;
                for (i, ch) in chars {
                    end = i + ch.len_utf8();
                    if ('\u{40}'..='\u{7e}').contains(&ch) {
                        break;
                    }
                }
            }
            out.push(Unit {
                text: &rest[..end],
                columns: 0,
            });
            rest = &rest[end..];
            continue;
        }

        // Absorb the trailing zero-width scalars, noting whether one of them
        // switches the base scalar into emoji presentation.
        let mut end = first.len_utf8();
        let mut emoji = false;
        for (i, ch) in rest.char_indices().skip(1) {
            if ch != VS16 && !is_zero_width(ch) {
                break;
            }
            emoji |= ch == VS16;
            end = i + ch.len_utf8();
        }

        out.push(Unit {
            text: &rest[..end],
            columns: char_columns(first, emoji),
        });
        rest = &rest[end..];
    }
    out
}

/// Width of `text` in terminal display columns. Hand-rolled because the crate
/// takes no width dependency: control characters and CSI escape sequences count
/// zero, combining marks count zero, and the East Asian wide blocks — plus
/// anything wearing an emoji presentation selector — count two.
pub fn display_width(text: &str) -> usize {
    units(text).iter().map(|unit| unit.columns).sum()
}

fn is_zero_width(ch: char) -> bool {
    if ch.is_control() {
        return true;
    }
    matches!(ch as u32,
        0x0300..=0x036f      // combining diacriticals
        | 0x1ab0..=0x1aff    // combining diacriticals extended
        | 0x20d0..=0x20ff    // combining marks for symbols
        | 0x200b..=0x200f    // zero width space .. RLM
        | 0xfe00..=0xfe0f    // variation selectors
        | 0xfe20..=0xfe2f    // combining half marks
        | 0xfeff)
}

/// Columns for one base scalar. `emoji_presentation` is set when a variation
/// selector U+FE0F followed it, which promotes an otherwise one-column scalar
/// such as `⚠` to the two columns its emoji glyph actually takes.
fn char_columns(ch: char, emoji_presentation: bool) -> usize {
    if is_zero_width(ch) {
        return 0;
    }
    if emoji_presentation || is_wide(ch as u32) {
        2
    } else {
        1
    }
}

/// East Asian Wide and Fullwidth, plus the emoji blocks that render two columns
/// wide without any selector.
///
/// The emoji ranges are deliberately over-inclusive: `0x1f300..=0x1faff` sweeps
/// in a handful of genuinely narrow scalars (ornamental dingbats, chess
/// symbols) along with everything wide. Over-counting costs a column of unused
/// room at the end of a line; under-counting overflows the pane and wraps the
/// frame, so the error is taken in the safe direction. Two earlier versions of
/// this table had gaps at `0x1f650..=0x1f8ff` and `0x1fa00..=0x1faff`, which is
/// how `🚀` came to measure one column and push a 40-column pane to 41.
fn is_wide(code: u32) -> bool {
    matches!(code,
        0x1100..=0x115f
        | 0x231a..=0x231b    // ⌚⌛
        | 0x23e9..=0x23ec
        | 0x23f0
        | 0x23f3
        | 0x25fd..=0x25fe
        | 0x2614..=0x2615
        | 0x2648..=0x2653
        | 0x267f
        | 0x2693
        | 0x26a1
        | 0x26aa..=0x26ab
        | 0x26bd..=0x26be
        | 0x26c4..=0x26c5
        | 0x26ce
        | 0x26d4
        | 0x26ea
        | 0x26f2..=0x26f3
        | 0x26f5
        | 0x26fa
        | 0x26fd
        | 0x2705
        | 0x270a..=0x270b
        | 0x2728
        | 0x274c
        | 0x274e
        | 0x2753..=0x2755
        | 0x2757
        | 0x2795..=0x2797
        | 0x27b0
        | 0x27bf
        | 0x2b1b..=0x2b1c
        | 0x2b50
        | 0x2b55
        | 0x2e80..=0x303e
        | 0x3041..=0x33ff
        | 0x3400..=0x4dbf
        | 0x4e00..=0x9fff
        | 0xa000..=0xa4cf
        | 0xa960..=0xa97c    // Hangul Jamo Extended-A
        | 0xac00..=0xd7a3
        | 0xf900..=0xfaff
        | 0xfe10..=0xfe19
        | 0xfe30..=0xfe6f
        | 0xff00..=0xff60
        | 0xffe0..=0xffe6
        | 0x1f300..=0x1faff
        | 0x20000..=0x2fffd
        | 0x30000..=0x3fffd)
}

/// Trims `text` to `max` display columns, dropping characters from the LEFT and
/// marking the cut with `…`. Used for paths, whose tail is the informative half.
pub fn truncate_left(text: &str, max: usize) -> String {
    let units = units(text);
    if units.iter().map(|unit| unit.columns).sum::<usize>() <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    if max == 1 {
        return ELLIPSIS.to_string();
    }

    let budget = max - 1;
    let mut used = 0;
    let mut first_kept = units.len();
    for (i, unit) in units.iter().enumerate().rev() {
        if used + unit.columns > budget {
            break;
        }
        used += unit.columns;
        first_kept = i;
    }

    let mut out = String::from(ELLIPSIS);
    for unit in &units[first_kept..] {
        out.push_str(unit.text);
    }
    out
}

/// Trims `text` to `max` display columns from the right, marking the cut with
/// `…`. Used for labels and headings, whose head is the informative half.
pub fn truncate_right(text: &str, max: usize) -> String {
    let units = units(text);
    if units.iter().map(|unit| unit.columns).sum::<usize>() <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    if max == 1 {
        return ELLIPSIS.to_string();
    }

    let budget = max - 1;
    let mut out = String::new();
    let mut used = 0;
    for unit in &units {
        if used + unit.columns > budget {
            break;
        }
        used += unit.columns;
        out.push_str(unit.text);
    }
    out.push(ELLIPSIS);
    out
}

/// Makes arbitrary blob content safe and bounded for terminal output.
///
/// Newlines and tabs are structure and remain intact. Every other C0/C1
/// control scalar is replaced, invalid UTF-8 becomes the replacement
/// character, and both dimensions are capped with an explicit notice.
pub fn sanitize_content(bytes: &[u8]) -> String {
    let decoded = String::from_utf8_lossy(bytes);
    let mut safe = String::with_capacity(decoded.len());
    for ch in decoded.chars() {
        let code = ch as u32;
        if ch == '\n' || ch == '\t' || !(code <= 0x1f || (0x7f..=0x9f).contains(&code)) {
            safe.push(ch);
        } else {
            safe.push('\u{fffd}');
        }
    }

    let mut out = String::new();
    let mut lines = safe.split_inclusive('\n');
    let mut width_cuts = 0usize;
    for _ in 0..CONTENT_MAX_LINES {
        let Some(segment) = lines.next() else {
            break;
        };
        let (line, newline) = match segment.strip_suffix('\n') {
            Some(line) => (line, true),
            None => (segment, false),
        };
        let (line, cut) = cap_content_line(line, CONTENT_MAX_COLUMNS);
        width_cuts += usize::from(cut);
        out.push_str(&line);
        if newline {
            out.push('\n');
        }
    }
    let omitted = lines.count();

    if omitted > 0 || width_cuts > 0 {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("[collide: output truncated");
        if omitted > 0 {
            out.push_str(&format!(": {omitted} more line(s)"));
        }
        if width_cuts > 0 {
            let separator = if omitted > 0 { "; " } else { ": " };
            out.push_str(&format!(
                "{separator}{width_cuts} line(s) exceeded {CONTENT_MAX_COLUMNS} columns"
            ));
        }
        out.push_str("]\n");
    }
    out
}

fn cap_content_line(line: &str, max: usize) -> (String, bool) {
    let line_units = units(line);
    let mut width = 0usize;
    for unit in &line_units {
        width += content_unit_columns(unit, width);
    }
    if width <= max {
        return (line.to_string(), false);
    }
    if max == 0 {
        return (String::new(), true);
    }
    if max == 1 {
        return (ELLIPSIS.to_string(), true);
    }

    let budget = max - 1;
    let mut out = String::new();
    let mut used = 0usize;
    for unit in line_units {
        let columns = content_unit_columns(&unit, used);
        if used + columns > budget {
            break;
        }
        out.push_str(unit.text);
        used += columns;
    }
    out.push(ELLIPSIS);
    (out, true)
}

/// Display width of one sanitizer unit at a given starting column.
///
/// `units` deliberately absorbs trailing zero-width scalars, and tabs are
/// control scalars. Counting tabs in the whole unit keeps adjacent runs honest
/// without changing the general-purpose width rules used by the pane.
fn content_unit_columns(unit: &Unit<'_>, start: usize) -> usize {
    let mut end = start + unit.columns;
    for _ in unit.text.bytes().filter(|byte| *byte == b'\t') {
        end += 8 - (end % 8);
    }
    end - start
}

#[cfg(test)]
fn content_line_columns(line: &str) -> usize {
    units(line)
        .iter()
        .fold(0, |width, unit| width + content_unit_columns(unit, width))
}

fn push_line(out: &mut String, line: &str, width: usize) {
    let trimmed = line.trim_end();
    out.push_str(&truncate_right(trimmed, width));
    out.push('\n');
}

/// Greedy word wrap. Notes and legends wrap rather than truncate, because
/// truncating an explanation removes the explanation.
fn push_wrapped(out: &mut String, first: &str, rest: &str, text: &str, width: usize) {
    let mut prefix = first;
    let mut line = String::new();

    for word in text.split_whitespace() {
        let budget = width.saturating_sub(display_width(prefix)).max(1);
        let candidate = if line.is_empty() {
            word.to_string()
        } else {
            format!("{line} {word}")
        };
        if display_width(&candidate) <= budget || line.is_empty() {
            line = candidate;
        } else {
            push_line(out, &format!("{prefix}{line}"), width);
            prefix = rest;
            line = word.to_string();
        }
    }
    if !line.is_empty() {
        push_line(out, &format!("{prefix}{line}"), width);
    }
}

// ---------------------------------------------------------------------------
// Watch loop
// ---------------------------------------------------------------------------

const CLEAR_SCREEN: &str = "\u{1b}[H\u{1b}[2J";
const HIDE_CURSOR: &str = "\u{1b}[?25l";
const SHOW_CURSOR: &str = "\u{1b}[?25h";
const RESET_ATTRS: &str = "\u{1b}[0m";

/// `--watch`: render the detail view on an interval until interrupted.
///
/// This runs inside a herdr overlay pane, so it clears and redraws in place
/// rather than scrolling, sizes itself from the real terminal every frame, and
/// restores the cursor on the way out so SIGINT/SIGTERM never leave the pane
/// mangled.
pub fn run_watch(config: &Config) -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    register_stop_signals(&stop)?;

    let mut out = std::io::stdout();
    let _ = write!(out, "{HIDE_CURSOR}");
    let _ = out.flush();

    let result = watch_loop(config, &stop, &mut out);

    // Best effort, and deliberately unconditional: whatever went wrong, the
    // terminal goes back the way we found it.
    let _ = write!(out, "{SHOW_CURSOR}{RESET_ATTRS}");
    let _ = out.flush();
    result
}

fn watch_loop(config: &Config, stop: &AtomicBool, out: &mut impl Write) -> Result<()> {
    while !stop.load(Ordering::Relaxed) {
        let (columns, rows) = terminal_size();
        // `collide::gather` is the one gathering pipeline; the pane renders what
        // it produces rather than assembling a second, subtly different one.
        let frame = match crate::collide::gather(config) {
            Ok(cycle) => detail_with_notes(&cycle.report, &cycle.notes, columns),
            Err(err) => error_frame(&err.to_string(), columns),
        };
        draw(out, &frame, rows)?;
        if !sleep_interruptibly(config.interval, stop) {
            break;
        }
    }
    Ok(())
}

fn draw(out: &mut impl Write, frame: &str, rows: usize) -> Result<()> {
    // One line is left free so the last row cannot scroll the screen up.
    let budget = rows.saturating_sub(1).max(1);
    let total = frame.lines().count();

    let mut buffer = String::with_capacity(frame.len() + 64);
    buffer.push_str(CLEAR_SCREEN);
    if total <= budget {
        buffer.push_str(frame);
    } else {
        for line in frame.lines().take(budget.saturating_sub(1)) {
            buffer.push_str(line);
            buffer.push('\n');
        }
        buffer.push_str(&format!("... {} more lines\n", total - (budget - 1)));
    }

    out.write_all(buffer.as_bytes())?;
    out.flush()?;
    Ok(())
}

fn error_frame(message: &str, columns: usize) -> String {
    let width = columns.max(MIN_COLUMNS);
    let mut out = String::new();
    push_line(&mut out, TITLE, width);
    out.push('\n');
    push_wrapped(&mut out, "", "", "Could not read the session:", width);
    push_wrapped(&mut out, "  ", "  ", message, width);
    out.push('\n');
    push_wrapped(&mut out, "", "", "Retrying on the next refresh.", width);
    out
}

/// Non-fatal problems the gathering pass collected — a checkout that vanished,
/// a pair whose prediction failed. They belong on screen: silently dropping
/// them renders as a suspiciously clean report.
fn notes_section(notes: &[String], width: usize) -> String {
    let mut out = String::new();
    if notes.is_empty() {
        return out;
    }
    out.push('\n');
    push_line(&mut out, "notes", width);
    for note in notes {
        push_wrapped(&mut out, "  ", "    ", note, width);
    }
    out
}

fn sleep_interruptibly(interval: Duration, stop: &AtomicBool) -> bool {
    let slice = Duration::from_millis(100);
    let mut remaining = interval;
    while !remaining.is_zero() {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        let step = slice.min(remaining);
        std::thread::sleep(step);
        remaining -= step;
    }
    !stop.load(Ordering::Relaxed)
}

#[cfg(unix)]
fn register_stop_signals(stop: &Arc<AtomicBool>) -> Result<()> {
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(stop))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn register_stop_signals(_stop: &Arc<AtomicBool>) -> Result<()> {
    Ok(())
}

/// Terminal size in (columns, rows). The pane may be narrower than we would
/// like, so this is read every frame rather than cached.
fn terminal_size() -> (usize, usize) {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;

        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        let fd = std::io::stdout().as_raw_fd();
        // SAFETY: `size` is a correctly sized, owned `winsize`.
        let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) };
        if rc == 0 && size.ws_col > 0 {
            let rows = if size.ws_row > 0 {
                size.ws_row as usize
            } else {
                24
            };
            return (size.ws_col as usize, rows);
        }
    }
    env_terminal_size()
}

fn env_terminal_size() -> (usize, usize) {
    let columns = crate::config::non_empty_env("COLUMNS")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|c| *c > 0)
        .unwrap_or(DEFAULT_COLUMNS);
    let rows = crate::config::non_empty_env("LINES")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|r| *r > 0)
        .unwrap_or(24);
    (columns, rows)
}

#[cfg(test)]
mod content_tests {
    use super::{
        cap_content_line, content_line_columns, display_width, sanitize_content,
        CONTENT_MAX_COLUMNS, CONTENT_MAX_LINES,
    };

    #[test]
    fn content_sanitizer_neutralises_controls_but_preserves_structure() {
        let input = "one\tcell\nescape:\u{1b}[2J nul:\0 del:\u{7f} c1:\u{85} end\n";
        let rendered = sanitize_content(input.as_bytes());
        assert_eq!(
            rendered,
            "one\tcell\nescape:\u{fffd}[2J nul:\u{fffd} del:\u{fffd} c1:\u{fffd} end\n"
        );
    }

    #[test]
    fn content_sanitizer_survives_invalid_utf8() {
        assert_eq!(sanitize_content(b"left\xffright\n"), "left\u{fffd}right\n");
    }

    #[test]
    fn content_sanitizer_caps_lines_and_announces_the_cut() {
        let input = "line\n".repeat(CONTENT_MAX_LINES + 2);
        let rendered = sanitize_content(input.as_bytes());
        assert_eq!(rendered.matches("line\n").count(), CONTENT_MAX_LINES);
        assert!(rendered.contains("output truncated: 2 more line(s)"));
    }

    #[test]
    fn content_sanitizer_caps_width_and_announces_the_cut() {
        let input = "x".repeat(CONTENT_MAX_COLUMNS + 20);
        let rendered = sanitize_content(input.as_bytes());
        let first = rendered.lines().next().expect("content line");
        assert_eq!(display_width(first), CONTENT_MAX_COLUMNS);
        assert!(first.ends_with('\u{2026}'));
        assert!(rendered.contains(&format!("1 line(s) exceeded {CONTENT_MAX_COLUMNS} columns")));
    }

    #[test]
    fn adjacent_tabs_are_measured_at_each_tab_stop() {
        let (rendered, cut) = cap_content_line("\t\tcode", 16);
        assert!(cut, "two tabs plus text occupy 20 columns");
        assert!(content_line_columns(&rendered) <= 16, "{rendered:?}");
    }

    #[test]
    fn content_sanitizer_caps_a_line_made_only_of_tabs() {
        let rendered = sanitize_content("\t".repeat(40).as_bytes());
        let first = rendered.lines().next().expect("content line");
        assert!(content_line_columns(first) <= CONTENT_MAX_COLUMNS);
        assert!(rendered.contains(&format!("1 line(s) exceeded {CONTENT_MAX_COLUMNS} columns")));
    }
}
