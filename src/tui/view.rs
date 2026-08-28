//! Ratatui rendering for the interactive collision detail pane.

use std::collections::BTreeMap;
use std::time::Duration;

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::collide::Cycle;
use crate::model::{Checkout, FileVerdict, Pairing, Report, Severity, TargetVerdict};
use crate::render;

use super::state::{self, Detail, Mode, RowId};

const TITLE: &str = "Collide: shared files";
const HELP_LIST: &str =
    "↑/k ↓/j move  wheel scroll  click focus  Enter details/hunks  R refresh  q/Esc quit";
const HELP_HUNKS: &str = "↑/k ↓/j scroll  wheel scroll  Esc/q back  R refresh  Ctrl-C quit";
const HELP_MODAL: &str = "Esc/q back  R refresh  Ctrl-C quit";

fn normal() -> Style {
    Style::default().fg(Color::Reset)
}

fn tag(color: Color) -> Style {
    normal().fg(color).add_modifier(Modifier::BOLD)
}

fn verdict_style(verdict: FileVerdict) -> Style {
    match verdict {
        FileVerdict::Conflict => tag(Color::Red),
        FileVerdict::Overlap => tag(Color::Yellow),
        FileVerdict::Unknown => normal().add_modifier(Modifier::BOLD),
    }
}

fn severity_style(severity: Severity) -> Style {
    match severity {
        Severity::Conflict => tag(Color::Red),
        Severity::Overlap => tag(Color::Yellow),
        Severity::Runaway => tag(Color::Magenta),
        Severity::Unknown => normal().add_modifier(Modifier::BOLD),
        Severity::Clean => normal(),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MouseMap {
    rows: Vec<HitRow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HitRow {
    y: u16,
    left: u16,
    right: u16,
    focus: usize,
}

impl MouseMap {
    pub fn focus_at(&self, column: u16, row: u16) -> Option<usize> {
        self.rows
            .iter()
            .find(|hit| hit.y == row && column >= hit.left && column < hit.right)
            .map(|hit| hit.focus)
    }
}

#[derive(Debug, Clone)]
struct Segment {
    text: String,
    style: Style,
}

impl Segment {
    fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: normal(),
        }
    }

    fn styled(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

#[derive(Debug, Clone)]
struct BodyLine {
    segments: Vec<Segment>,
    focus: Option<usize>,
}

impl BodyLine {
    fn plain(text: impl Into<String>) -> Self {
        Self {
            segments: vec![Segment::plain(text)],
            focus: None,
        }
    }

    fn focus(segments: Vec<Segment>, focus: usize) -> Self {
        Self {
            segments,
            focus: Some(focus),
        }
    }
}

pub fn render(
    frame: &mut Frame<'_>,
    detail: &Detail,
    cycle: Option<&Cycle>,
    interval: Duration,
) -> MouseMap {
    let area = frame.area();
    if area.is_empty() {
        return MouseMap::default();
    }

    if matches!(detail.mode, Mode::Hunks { .. }) {
        render_hunks(frame, detail, area);
        return MouseMap::default();
    }

    let regions = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(3),
    ])
    .split(area);
    render_header(frame, detail, cycle, interval, regions[0]);
    let mouse = render_body(frame, detail, cycle, regions[1]);
    render_footer(frame, detail, regions[2]);
    render_checkout_modal(frame, detail, cycle, area);
    mouse
}

fn render_header(
    frame: &mut Frame<'_>,
    detail: &Detail,
    cycle: Option<&Cycle>,
    interval: Duration,
    area: Rect,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(normal())
        .title(Span::styled(format!(" {TITLE} "), normal()));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let summary = match cycle {
        Some(cycle) => format!(
            "{} worktrees · {} shared rows · refresh every {}s{}",
            cycle.report.checkouts.len(),
            detail
                .rows
                .iter()
                .filter(|row| row.shared_path().is_some())
                .count(),
            interval.as_secs(),
            if detail.refresh_requested {
                " · refreshing"
            } else {
                ""
            }
        ),
        None => format!(
            "waiting for session · refresh every {}s",
            interval.as_secs()
        ),
    };
    frame.render_widget(Paragraph::new(summary).style(normal()), inner);
}

fn render_body(
    frame: &mut Frame<'_>,
    detail: &Detail,
    cycle: Option<&Cycle>,
    area: Rect,
) -> MouseMap {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(normal())
        .title(Span::styled(" collisions ", normal()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return MouseMap::default();
    }

    let lines = body_lines(detail, cycle, inner.width as usize);
    let capacity = inner.height as usize;
    if capacity == 0 {
        return MouseMap::default();
    }
    let cursor_line = lines
        .iter()
        .position(|line| line.focus == Some(detail.cursor))
        .unwrap_or(0);
    let top = if lines.len() <= capacity {
        0
    } else {
        cursor_line
            .saturating_add(1)
            .saturating_sub(capacity)
            .min(lines.len() - capacity)
    };

    let mut mouse = MouseMap::default();
    for (offset, line) in lines.iter().skip(top).take(capacity).enumerate() {
        let row = Rect::new(
            inner.x,
            inner.y.saturating_add(offset as u16),
            inner.width,
            1,
        );
        let selected = line.focus == Some(detail.cursor);
        let row_style = if selected {
            normal().add_modifier(Modifier::REVERSED)
        } else {
            normal()
        };
        frame.render_widget(Block::default().style(row_style), row);
        let spans = line
            .segments
            .iter()
            .map(|segment| Span::styled(segment.text.clone(), row_style.patch(segment.style)))
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(Line::from(spans)).style(row_style), row);
        if let Some(focus) = line.focus {
            mouse.rows.push(HitRow {
                y: row.y,
                left: row.x,
                right: row.right(),
                focus,
            });
        }
    }
    mouse
}

fn body_lines(detail: &Detail, cycle: Option<&Cycle>, width: usize) -> Vec<BodyLine> {
    let mut lines = Vec::new();
    if let Some(message) = detail.message.as_deref() {
        lines.push(BodyLine::plain("notes"));
        push_wrapped(&mut lines, "  ", message, width);
        lines.push(BodyLine::plain(""));
    }
    let Some(cycle) = cycle else {
        lines.push(BodyLine::plain("Could not read the session."));
        lines.push(BodyLine::plain("The pane will retry on the next refresh."));
        return lines;
    };
    if !cycle.notes.is_empty() {
        lines.push(BodyLine::plain("notes"));
        for note in &cycle.notes {
            push_wrapped(&mut lines, "  ", note, width);
        }
        lines.push(BodyLine::plain(""));
    }
    let report = &cycle.report;
    if report.checkouts.is_empty() {
        lines.push(BodyLine::plain("No git-backed workspaces are open."));
        return lines;
    }

    let status_by_id: BTreeMap<&str, _> = report
        .statuses
        .iter()
        .map(|status| (status.workspace_id.as_str(), status))
        .collect();
    let checkout_by_id: BTreeMap<&str, _> = report
        .checkouts
        .iter()
        .map(|checkout| (checkout.workspace_id.as_str(), checkout))
        .collect();
    let mut saw_shared = false;
    let mut saw_unknown = report
        .statuses
        .iter()
        .any(|status| status.severity == Severity::Unknown);
    let mut saw_runaway = false;

    for group in state::repo_groups(report) {
        if !lines.is_empty()
            && !lines
                .last()
                .is_some_and(|line| line.segments.len() == 1 && line.segments[0].text.is_empty())
        {
            lines.push(BodyLine::plain(""));
        }
        let root = group
            .checkouts
            .first()
            .map(|checkout| checkout.repo_root.to_string_lossy())
            .unwrap_or_default();
        lines.push(BodyLine::plain(format!(
            "repo {}",
            render::truncate_left(&root, width.saturating_sub(5).max(8))
        )));

        for checkout in &group.checkouts {
            let row_id = RowId::Checkout(checkout.workspace_id.clone());
            let Some(focus) = detail.rows.iter().position(|row| row == &row_id) else {
                continue;
            };
            let status = status_by_id.get(checkout.workspace_id.as_str()).copied();
            saw_runaway |= status.is_some_and(|status| status.runaway);
            lines.push(BodyLine::focus(
                checkout_segments(checkout, status, width),
                focus,
            ));
            for note in render::degraded_notes(report.change_set(&checkout.workspace_id), checkout)
            {
                push_wrapped(&mut lines, "      ", &note, width);
            }
            for note in target_lines(report, checkout) {
                push_wrapped(&mut lines, "      ", &note, width);
            }
        }

        let pairings = state::pairings_for_repo(report, group.key);
        if pairings.is_empty() {
            let pairable = group
                .checkouts
                .iter()
                .filter(|checkout| {
                    report
                        .change_set(&checkout.workspace_id)
                        .map(crate::collide::pairable)
                        .unwrap_or(true)
                })
                .count();
            let message = if group.checkouts.len() < 2 {
                "only one worktree open for this repository — nothing to compare"
            } else if pairable < 2 {
                "no sibling worktree here can be compared — see the notes above"
            } else {
                "no files shared with a sibling worktree"
            };
            push_wrapped(&mut lines, "  ", message, width);
            continue;
        }

        for pairing in pairings {
            saw_shared = true;
            lines.push(BodyLine::plain(""));
            lines.push(BodyLine::plain(format!(
                "  {} <-> {}",
                display_label(&pairing.left_workspace_id, &checkout_by_id),
                display_label(&pairing.right_workspace_id, &checkout_by_id),
            )));
            append_pair_notes(&mut lines, report, pairing, &checkout_by_id, width);

            for file in state::files_for_pair(pairing) {
                saw_unknown |= file.verdict == FileVerdict::Unknown;
                let row_id = RowId::SharedFile {
                    left_workspace_id: pairing.left_workspace_id.clone(),
                    right_workspace_id: pairing.right_workspace_id.clone(),
                    path: file.path.clone(),
                };
                let Some(focus) = detail.rows.iter().position(|row| row == &row_id) else {
                    continue;
                };
                lines.push(BodyLine::focus(shared_segments(file, width), focus));
            }
        }
    }

    if saw_shared {
        lines.push(BodyLine::plain(""));
        lines.push(BodyLine::plain("legend"));
        lines.push(BodyLine {
            segments: vec![
                Segment::styled("  ✘  ", verdict_style(FileVerdict::Conflict)),
                Segment::plain("conflict predicted on merge"),
            ],
            focus: None,
        });
        lines.push(BodyLine {
            segments: vec![
                Segment::styled("  ⧉  ", verdict_style(FileVerdict::Overlap)),
                Segment::plain("same file, merges clean"),
            ],
            focus: None,
        });
        if saw_unknown {
            lines.push(BodyLine {
                segments: vec![
                    Segment::styled("  ?  ", verdict_style(FileVerdict::Unknown)),
                    Segment::plain("conflict prediction unavailable"),
                ],
                focus: None,
            });
        }
        if saw_runaway {
            lines.push(BodyLine {
                segments: vec![
                    Segment::styled("  ⚠  ", tag(Color::Magenta)),
                    Segment::plain("runaway change set (lines, or f = files)"),
                ],
                focus: None,
            });
        }
    }
    lines
}

fn checkout_segments(
    checkout: &Checkout,
    status: Option<&crate::model::WorkspaceStatus>,
    width: usize,
) -> Vec<Segment> {
    let line = render::worktree_line(checkout, status, width);
    let Some(status) = status else {
        return vec![Segment::plain(line)];
    };
    let badge = render::badge(status);
    let badge_at = (!badge.is_empty() && line.ends_with(&badge)).then(|| line.len() - badge.len());
    let (prefix, badge_text) = match badge_at {
        Some(at) => (&line[..at], Some(&line[at..])),
        None => (line.as_str(), None),
    };
    let mut segments = Vec::new();
    if status.runaway {
        if let Some(at) = prefix.rfind("runaway") {
            segments.push(Segment::plain(&prefix[..at]));
            segments.push(Segment::styled("runaway", tag(Color::Magenta)));
            segments.push(Segment::plain(&prefix[at + "runaway".len()..]));
        } else {
            segments.push(Segment::plain(prefix));
        }
    } else {
        segments.push(Segment::plain(prefix));
    }
    if let Some(badge_text) = badge_text {
        segments.push(Segment::styled(
            badge_text.to_string(),
            severity_style(status.severity),
        ));
    }
    segments
}

fn shared_segments(file: &crate::model::SharedFile, width: usize) -> Vec<Segment> {
    let narrow = width < 30;
    let (mark, word) = render::verdict_marks(file.verdict);
    let annotation = (!narrow)
        .then(|| {
            file.conflict_type
                .as_deref()
                .and_then(crate::git::conflict_type_annotation)
        })
        .flatten();
    let prefix_columns = if narrow { 7 } else { 16 };
    let annotation_columns = annotation
        .map(|annotation| render::display_width(annotation) + 4)
        .unwrap_or(0);
    let path = render::truncate_left(
        &file.path,
        width
            .saturating_sub(prefix_columns + annotation_columns)
            .max(4),
    );
    let tag_text = if narrow {
        format!("{mark}  ")
    } else {
        format!("{mark} {word:<8}  ")
    };
    let mut segments = vec![
        Segment::plain("    "),
        Segment::styled(tag_text, verdict_style(file.verdict)),
        Segment::plain(path),
    ];
    if let Some(annotation) = annotation {
        segments.push(Segment::plain(format!("  ({annotation})")));
    }
    segments
}

fn append_pair_notes(
    lines: &mut Vec<BodyLine>,
    report: &Report,
    pairing: &Pairing,
    checkout_by_id: &BTreeMap<&str, &Checkout>,
    width: usize,
) {
    for side in [&pairing.left_workspace_id, &pairing.right_workspace_id] {
        if render::is_unmerged(report.change_set(side)) {
            let label = display_label(side, checkout_by_id);
            push_wrapped(
                lines,
                "    ",
                &format!(
                    "advisory: a merge is in progress in {label}, so these verdicts were computed from a tree that still contains conflict markers."
                ),
                width,
            );
        }
    }
    let paths = render::uncomparable_submodule_paths(report, pairing);
    if !paths.is_empty() {
        let listed = paths
            .iter()
            .map(|path| format!("`{path}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let scope = if paths.len() == 1 {
            "this path"
        } else {
            "these paths"
        };
        push_wrapped(
            lines,
            "    ",
            &format!(
                "unknown: submodule contents differ at {listed}; the gitlink snapshot alone cannot represent those contents, and no successful depth-one nested comparison is available, so a clean merge for {scope} was not verified."
            ),
            width,
        );
    }
    if pairing.approximate {
        push_wrapped(
            lines,
            "    ",
            "approximate: these two histories offer no single merge base, so one was forced and the verdicts below approximate what a real merge would do.",
            width,
        );
    }
}

fn target_lines(report: &Report, checkout: &Checkout) -> Vec<String> {
    let Some(target) = report.target_prediction(&checkout.workspace_id) else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    if target.advisory {
        lines.push(format!(
            "advisory: a merge is in progress in {}, so this target verdict was computed from a tree that still contains conflict markers.",
            render::label_of(checkout)
        ));
    }
    if target.approximate {
        lines.push(
            "approximate: this history and its integration target offer no single merge base, so one was forced and the target verdict approximates what a real merge would do."
                .into(),
        );
    }
    let named = target
        .target_ref
        .as_deref()
        .map(|target_ref| format!("target {target_ref}"))
        .unwrap_or_else(|| "target integration ref".into());
    let verdict = match target.verdict {
        TargetVerdict::Clean => "clean",
        TargetVerdict::Conflict => "conflict",
        TargetVerdict::Unknown => "unknown",
    };
    lines.push(match &target.reason {
        Some(reason) => format!("{named}: {verdict} — {reason}"),
        None => format!("{named}: {verdict}"),
    });
    lines
}

fn display_label(workspace_id: &str, by_id: &BTreeMap<&str, &Checkout>) -> String {
    by_id
        .get(workspace_id)
        .map(|checkout| render::label_of(checkout).to_string())
        .unwrap_or_else(|| workspace_id.to_string())
}

fn push_wrapped(lines: &mut Vec<BodyLine>, prefix: &str, text: &str, width: usize) {
    let width = width.max(1);
    let mut current = prefix.to_string();
    for word in text.split_whitespace() {
        let separator = if current == prefix { "" } else { " " };
        if render::display_width(&current)
            .saturating_add(render::display_width(separator))
            .saturating_add(render::display_width(word))
            > width
            && current != prefix
        {
            lines.push(BodyLine::plain(current));
            current = prefix.to_string();
        }
        if current != prefix {
            current.push(' ');
        }
        current.push_str(word);
    }
    if current != prefix || text.is_empty() {
        lines.push(BodyLine::plain(render::truncate_right(&current, width)));
    }
}

fn render_footer(frame: &mut Frame<'_>, detail: &Detail, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(normal())
        .title(Span::styled(" keys ", normal()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let help = match detail.mode {
        Mode::CheckoutDetail { .. } => HELP_MODAL,
        Mode::OpeningHunks { .. } | Mode::Hunks { .. } => HELP_HUNKS,
        Mode::List | Mode::Done => HELP_LIST,
    };
    frame.render_widget(Paragraph::new(help).style(normal()), inner);
}

fn render_checkout_modal(
    frame: &mut Frame<'_>,
    detail: &Detail,
    cycle: Option<&Cycle>,
    screen: Rect,
) {
    let Mode::CheckoutDetail { workspace_id } = &detail.mode else {
        return;
    };
    let Some(report) = cycle.map(|cycle| &cycle.report) else {
        return;
    };
    let Some(checkout) = report
        .checkouts
        .iter()
        .find(|checkout| &checkout.workspace_id == workspace_id)
    else {
        return;
    };
    let mut lines = vec![
        format!("workspace: {}", render::label_of(checkout)),
        format!("checkout: {}", checkout.checkout_path.display()),
        format!(
            "branch: {}",
            checkout.branch.as_deref().unwrap_or("no branch")
        ),
    ];
    let mut details = render::degraded_notes(report.change_set(workspace_id), checkout);
    details.extend(target_lines(report, checkout));
    if details.is_empty() {
        details.push("change set read completely; no degraded detail.".into());
    }
    lines.push(String::new());
    lines.extend(details);
    lines.push(String::new());
    lines.push("Esc or q returns to the list.".into());

    let height = (lines.len() as u16 + 2).clamp(7, screen.height.saturating_sub(2).max(1));
    let area = centered(screen, 82, height);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(normal())
        .title(Span::styled(" Checkout detail ", normal()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines.join("\n")).style(normal()), inner);
}

fn render_hunks(frame: &mut Frame<'_>, detail: &Detail, area: Rect) {
    let Mode::Hunks {
        path,
        text,
        prediction_failed,
        scroll,
    } = &detail.mode
    else {
        return;
    };
    frame.render_widget(Clear, area);
    let available = area.width.saturating_sub(4) as usize;
    let title_path = render::truncate_left(path, available.max(4));
    let title = if *prediction_failed {
        format!(" Why: {title_path} · unavailable ")
    } else {
        format!(" Why: {title_path} ")
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(normal())
        .title(Span::styled(title, normal()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }
    let help_y = inner.bottom().saturating_sub(1);
    let content = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(1),
    );
    frame.render_widget(
        Paragraph::new(text.clone())
            .style(normal())
            .scroll(((*scroll).min(u16::MAX as usize) as u16, 0)),
        content,
    );
    frame.render_widget(
        Paragraph::new(HELP_HUNKS).style(normal()),
        Rect::new(inner.x, help_y, inner.width, 1),
    );
}

fn centered(screen: Rect, max_width: u16, height: u16) -> Rect {
    let width = max_width.min(screen.width.saturating_sub(2).max(1));
    let height = height.min(screen.height.saturating_sub(2).max(1));
    Rect::new(
        screen.x + screen.width.saturating_sub(width) / 2,
        screen.y + screen.height.saturating_sub(height) / 2,
        width,
        height,
    )
}
