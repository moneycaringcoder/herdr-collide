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

const CONFLICT_MARK: &str = "\u{2718}"; // ✘
const OVERLAP_MARK: &str = "\u{29c9}"; // ⧉
const RUNAWAY_MARK: &str = "\u{26a0}"; // ⚠
const UNKNOWN_MARK: &str = "?";
const ELLIPSIS: char = '\u{2026}'; // …

const TITLE: &str = "collide \u{b7} shared files";
const NO_BRANCH: &str = "no branch";
const NO_AGENT: &str = "(no agent)";

/// `"    "` + mark + `" "` + an eight-column verdict word + `"  "`.
const FILE_PREFIX_COLUMNS: usize = 16;

/// Last resort, used only when the report carries no change set at all for a
/// checkout, so there is no reason code to explain.
const NO_BRANCH_NOTE: &str = "degraded: no branch reported for this checkout \u{2014} it is a \
     detached HEAD, or the worktree branch lookup failed.";

// ---------------------------------------------------------------------------
// Badge
// ---------------------------------------------------------------------------

/// Badge text for one workspace, e.g. `✘ 2` or `⧉ 3`. Severity itself is
/// carried by the token *name*, not this string.
///
/// A clean workspace renders the empty string, which the daemon treats as
/// "clear the badge" rather than "write an empty badge".
pub fn badge(status: &WorkspaceStatus) -> String {
    // A runaway is measured in change-set size, not in shared files: the whole
    // point of the severity is a workspace that has grown huge on its own,
    // usually sharing nothing at all with its siblings.
    let (mark, magnitude) = match status.severity {
        Severity::Clean => return String::new(),
        Severity::Conflict => (CONFLICT_MARK, status.conflict_count as u64),
        Severity::Runaway => (RUNAWAY_MARK, status.lines_changed),
        Severity::Overlap => (OVERLAP_MARK, status.overlap_count as u64),
    };

    // Zero has nothing to say; the mark alone is the whole message.
    let text = if magnitude == 0 {
        mark.to_string()
    } else {
        format!("{mark} {}", abbreviate(magnitude))
    };

    // Belt and braces: `abbreviate` is bounded at four columns, so this only
    // ever fires if the marks change.
    truncate_right(&text, BADGE_COLUMNS)
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
    let width = columns.max(MIN_COLUMNS);
    let mut out = String::new();
    push_line(&mut out, TITLE, width);

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
    let mut saw_unknown = false;

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
            push_line(&mut out, &worktree_line(checkout, status), width);
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
        pairings.sort_by_key(|p| {
            (
                display_label(&p.left_workspace_id, &checkout_by_id),
                display_label(&p.right_workspace_id, &checkout_by_id),
            )
        });

        if pairings.is_empty() {
            push_line(&mut out, "  no files shared with a sibling worktree", width);
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

            let mut files: Vec<&crate::model::SharedFile> = pairing.shared.iter().collect();
            // Conflicts first, then the ones we could not decide, then plain
            // overlaps; alphabetical within each band.
            files.sort_by(|a, b| {
                verdict_rank(a.verdict)
                    .cmp(&verdict_rank(b.verdict))
                    .then_with(|| a.path.cmp(&b.path))
            });

            let path_budget = width.saturating_sub(FILE_PREFIX_COLUMNS).max(8);
            for file in files {
                if file.verdict == FileVerdict::Unknown {
                    saw_unknown = true;
                }
                let (mark, word) = verdict_marks(file.verdict);
                // Paths truncate from the LEFT: the tail is the informative half.
                let path = truncate_left(&file.path, path_budget);
                push_line(&mut out, &format!("    {mark} {word:<8}  {path}"), width);
            }
        }
    }

    if saw_shared {
        out.push('\n');
        push_line(&mut out, "legend", width);
        push_line(
            &mut out,
            &format!("  {CONFLICT_MARK}  conflict predicted on merge"),
            width,
        );
        push_line(
            &mut out,
            &format!("  {OVERLAP_MARK}  same file, merges clean"),
            width,
        );
        if saw_unknown {
            push_line(
                &mut out,
                &format!("  {UNKNOWN_MARK}  conflict prediction unavailable"),
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

fn worktree_line(checkout: &Checkout, status: Option<&WorkspaceStatus>) -> String {
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

    let mut line = format!("  {} [{branch}] {agent}", label_of(checkout));
    if let Some(status) = status {
        if status.runaway {
            line.push_str("  runaway");
        }
        let badge_text = badge(status);
        if !badge_text.is_empty() {
            line.push_str("  ");
            line.push_str(&badge_text);
        }
    }
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
        git::DEGRADED_UNBORN => {
            "unborn branch, so this checkout has no commit and is not paired with its siblings"
        }
        git::DEGRADED_BROKEN_HEAD => {
            "broken HEAD, so this checkout has no commit and is not paired with its siblings"
        }
        git::DEGRADED_MISSING_BASE_REF => {
            "the base ref does not resolve here, so only uncommitted work is counted"
        }
        git::DEGRADED_NO_MERGE_BASE => {
            "no common ancestor with the base ref, so only uncommitted work is counted"
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

/// Width of `text` in terminal display columns. Hand-rolled because the crate
/// takes no width dependency: control characters and CSI escape sequences count
/// zero, combining marks count zero, and the common East Asian wide blocks
/// count two.
pub fn display_width(text: &str) -> usize {
    let mut width = 0;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for tail in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&tail) {
                        break;
                    }
                }
            }
            continue;
        }
        width += char_columns(ch);
    }
    width
}

fn char_columns(ch: char) -> usize {
    if ch.is_control() {
        return 0;
    }
    let code = ch as u32;
    let zero_width = matches!(code,
        0x0300..=0x036f      // combining diacriticals
        | 0x1ab0..=0x1aff    // combining diacriticals extended
        | 0x20d0..=0x20ff    // combining marks for symbols
        | 0x200b..=0x200f    // zero width space .. RLM
        | 0xfe00..=0xfe0f    // variation selectors
        | 0xfe20..=0xfe2f    // combining half marks
        | 0xfeff);
    if zero_width {
        return 0;
    }
    let wide = matches!(code,
        0x1100..=0x115f
        | 0x2e80..=0x303e
        | 0x3041..=0x33ff
        | 0x3400..=0x4dbf
        | 0x4e00..=0x9fff
        | 0xa000..=0xa4cf
        | 0xac00..=0xd7a3
        | 0xf900..=0xfaff
        | 0xfe10..=0xfe19
        | 0xfe30..=0xfe6f
        | 0xff00..=0xff60
        | 0xffe0..=0xffe6
        | 0x1f300..=0x1f64f
        | 0x1f900..=0x1f9ff
        | 0x20000..=0x2fffd
        | 0x30000..=0x3fffd);
    if wide {
        2
    } else {
        1
    }
}

/// Trims `text` to `max` display columns, dropping characters from the LEFT and
/// marking the cut with `…`. Used for paths, whose tail is the informative half.
pub fn truncate_left(text: &str, max: usize) -> String {
    if display_width(text) <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    if max == 1 {
        return ELLIPSIS.to_string();
    }

    let budget = max - 1;
    let mut kept: Vec<char> = Vec::new();
    let mut used = 0;
    for ch in text.chars().rev() {
        let columns = char_columns(ch);
        if used + columns > budget {
            break;
        }
        used += columns;
        kept.push(ch);
    }
    kept.reverse();

    let mut out = String::from(ELLIPSIS);
    out.extend(kept);
    out
}

/// Trims `text` to `max` display columns from the right, marking the cut with
/// `…`. Used for labels and headings, whose head is the informative half.
pub fn truncate_right(text: &str, max: usize) -> String {
    if display_width(text) <= max {
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
    for ch in text.chars() {
        let columns = char_columns(ch);
        if used + columns > budget {
            break;
        }
        used += columns;
        out.push(ch);
    }
    out.push(ELLIPSIS);
    out
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
            Ok(cycle) => {
                let mut frame = detail_at(&cycle.report, columns);
                frame.push_str(&notes_section(&cycle.notes, columns.max(MIN_COLUMNS)));
                frame
            }
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
