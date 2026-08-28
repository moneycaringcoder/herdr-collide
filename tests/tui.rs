#[path = "fixtures.rs"]
mod fixtures;

use std::path::PathBuf;
use std::time::Duration;

use collide::collide::{gather_for, why_for, Cycle};
use collide::config::Config;
use collide::git;
use collide::model::{
    ChangeSet, Checkout, FileVerdict, Pairing, RepoKey, Report, Severity, SharedFile,
    WorkspaceStatus,
};
use collide::tui::view::{self, MouseMap};
use collide::tui::{adopt, apply, map_key_event, show_hunks, Detail, Key, Mode, RowId};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;

use fixtures::{checkout, Fixture};

struct DrawnDetail {
    text: String,
    buffer: Buffer,
    mouse: MouseMap,
}

fn draw(detail: &Detail, cycle: Option<&Cycle>, width: u16, height: u16) -> DrawnDetail {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut mouse = MouseMap::default();
    terminal
        .draw(|frame| {
            mouse = view::render(frame, detail, cycle, Duration::from_secs(5));
        })
        .expect("detail frame");
    let buffer = terminal.backend().buffer().clone();
    let mut text = String::new();
    for row in 0..buffer.area.height {
        for column in 0..buffer.area.width {
            text.push_str(
                buffer
                    .cell((column, row))
                    .expect("cell inside buffer")
                    .symbol(),
            );
        }
        text.push('\n');
    }
    DrawnDetail {
        text,
        buffer,
        mouse,
    }
}

fn text_position(buffer: &Buffer, needle: &str) -> (u16, u16) {
    for row in 0..buffer.area.height {
        let mut text = String::new();
        for column in 0..buffer.area.width {
            text.push_str(
                buffer
                    .cell((column, row))
                    .expect("cell inside buffer")
                    .symbol(),
            );
        }
        if let Some(column) = text.find(needle) {
            return (column as u16, row);
        }
    }
    panic!("text {needle:?} was not rendered");
}

fn synthetic_cycle() -> Cycle {
    let key = RepoKey("/tmp/example/.git".into());
    let checkout = |id: &str, label: &str| Checkout {
        workspace_id: id.into(),
        workspace_label: label.into(),
        repo_key: key.clone(),
        repo_root: PathBuf::from("/tmp/example"),
        checkout_path: PathBuf::from(format!("/tmp/{id}")),
        is_linked_worktree: true,
        branch: Some("main".into()),
        agent: None,
    };
    let status = |id: &str, severity: Severity, runaway: bool| WorkspaceStatus {
        workspace_id: id.into(),
        severity,
        overlap_count: 1,
        conflict_count: usize::from(severity == Severity::Conflict),
        unknown_count: usize::from(severity == Severity::Unknown),
        runaway,
        lines_changed: if runaway { 4_200 } else { 12 },
        changed_files: 3,
    };
    Cycle {
        report: Report {
            checkouts: vec![checkout("left", "left"), checkout("right", "right")],
            pairings: vec![Pairing {
                left_workspace_id: "left".into(),
                right_workspace_id: "right".into(),
                shared: vec![
                    SharedFile {
                        path: "src/conflict.rs".into(),
                        verdict: FileVerdict::Conflict,
                        conflict_type: None,
                    },
                    SharedFile {
                        path: "src/overlap.rs".into(),
                        verdict: FileVerdict::Overlap,
                        conflict_type: None,
                    },
                    SharedFile {
                        path: "src/unknown.rs".into(),
                        verdict: FileVerdict::Unknown,
                        conflict_type: None,
                    },
                ],
                approximate: false,
            }],
            statuses: vec![
                status("left", Severity::Conflict, false),
                status("right", Severity::Runaway, true),
            ],
            targets: Vec::new(),
            changes: vec![
                ("left".into(), ChangeSet::default()),
                ("right".into(), ChangeSet::default()),
            ],
        },
        notes: Vec::new(),
    }
}

#[test]
fn verdict_tags_use_only_the_intended_colors_and_cursor_reverses_the_row() {
    let cycle = synthetic_cycle();
    let detail = adopt(Detail::empty(), &cycle.report);
    let drawn = draw(&detail, Some(&cycle), 100, 36);

    for (text, expected) in [
        ("conflict", Color::Red),
        ("overlap", Color::Yellow),
        ("unknown", Color::Reset),
        ("runaway", Color::Magenta),
    ] {
        let position = text_position(&drawn.buffer, text);
        let cell = drawn.buffer.cell(position).expect("tag cell");
        assert_eq!(cell.fg, expected, "{text} tag color");
        assert!(
            cell.modifier.contains(Modifier::BOLD),
            "{text} tag should be bold"
        );
    }

    let (_, cursor_row) = text_position(&drawn.buffer, "left [main]");
    for column in 1..drawn.buffer.area.width - 1 {
        assert!(
            drawn
                .buffer
                .cell((column, cursor_row))
                .expect("cursor row cell")
                .modifier
                .contains(Modifier::REVERSED),
            "column {column} was not reversed"
        );
    }
    let repo = text_position(&drawn.buffer, "repo /tmp/example");
    assert_eq!(
        drawn.buffer.cell(repo).expect("normal text").fg,
        Color::Reset
    );
}

#[test]
fn narrow_view_keeps_the_mark_and_path_tail_but_drops_the_verdict_word() {
    let cycle = synthetic_cycle();
    let mut detail = adopt(Detail::empty(), &cycle.report);
    detail.cursor = detail
        .rows
        .iter()
        .position(|row| row.shared_path() == Some("src/conflict.rs"))
        .expect("conflict row");

    let rendered = draw(&detail, Some(&cycle), 28, 18).text;
    assert!(rendered.contains('✘'), "{rendered}");
    assert!(rendered.contains("conflict.rs"), "{rendered}");
    assert!(!rendered.contains("✘ conflict"), "{rendered}");
}

#[test]
fn mouse_hit_testing_returns_focus_without_triggering_an_action() {
    let cycle = synthetic_cycle();
    let detail = adopt(Detail::empty(), &cycle.report);
    let drawn = draw(&detail, Some(&cycle), 100, 30);
    let (_, row) = text_position(&drawn.buffer, "left [main]");

    assert_eq!(drawn.mouse.focus_at(1, row), Some(detail.cursor));
    assert_eq!(drawn.mouse.focus_at(0, 0), None);
    assert_eq!(detail.mode, Mode::List, "hit testing is presentation only");
}

#[test]
fn refresh_preserves_cursor_and_open_hunk_path_then_reports_disappearance() {
    let cycle = synthetic_cycle();
    let mut detail = adopt(Detail::empty(), &cycle.report);
    detail.cursor = detail
        .rows
        .iter()
        .position(|row| row.shared_path() == Some("src/conflict.rs"))
        .expect("conflict row");
    let selected = detail.selected().cloned().expect("selected row");
    detail = show_hunks(
        detail,
        "src/conflict.rs".into(),
        "<<<<<<< left\n=======\n>>>>>>> right\n".into(),
        false,
    );

    detail = adopt(detail, &cycle.report);
    assert_eq!(detail.selected(), Some(&selected));
    assert!(matches!(
        detail.mode,
        Mode::Hunks { ref path, .. } if path == "src/conflict.rs"
    ));

    let mut vanished = cycle.report.clone();
    vanished.pairings.clear();
    detail = adopt(detail, &vanished);
    assert_eq!(detail.mode, Mode::List);
    assert!(
        detail
            .message
            .as_deref()
            .is_some_and(|message| message.contains("no longer shared")),
        "{:?}",
        detail.message
    );
}

#[test]
fn hunks_view_renders_real_conflict_markers_from_the_fixture() {
    let fixture = Fixture::new("tui-hunks");
    let pair = fixture.committed_conflict_pair();
    let config = Config {
        base_ref: "main".into(),
        predict_conflicts: true,
        ..Config::default()
    };
    let key = git::repo_key(&fixture.repo, config.git_timeout).expect("repo key");
    let checkouts = vec![
        checkout("left-worktree", &pair.0, &key.0),
        checkout("right-worktree", &pair.1, &key.0),
    ];
    let why = why_for(checkouts.clone(), &config, "conflict.txt").expect("why report");
    // Git versions differ on whether merge-tree includes the base (`|||||||`)
    // section. The TUI contract is to preserve the existing why output, not
    // invent a marker that Git did not produce.
    let has_diff3_base = why.text.contains("|||||||");
    let cycle = gather_for(checkouts, &config).expect("collision report");
    let detail = show_hunks(
        adopt(Detail::empty(), &cycle.report),
        "conflict.txt".into(),
        why.text,
        why.prediction_failed,
    );

    let rendered = draw(&detail, Some(&cycle), 100, 24).text;
    for marker in ["<<<<<<<", "=======", ">>>>>>>"] {
        assert!(rendered.contains(marker), "missing {marker}:\n{rendered}");
    }
    if has_diff3_base {
        assert!(rendered.contains("|||||||"), "{rendered}");
    }
    assert!(rendered.contains("ALPHA-A"), "{rendered}");
    assert!(rendered.contains("ALPHA-B"), "{rendered}");
}

#[test]
fn crossterm_keys_map_to_the_pure_state_machine() {
    for (code, expected) in [
        (KeyCode::Up, Key::Up),
        (KeyCode::Down, Key::Down),
        (KeyCode::Char('k'), Key::Up),
        (KeyCode::Char('j'), Key::Down),
        (KeyCode::Enter, Key::Enter),
        (KeyCode::Char('R'), Key::Rescan),
        (KeyCode::Char('q'), Key::Back),
        (KeyCode::Esc, Key::Back),
    ] {
        assert_eq!(
            map_key_event(KeyEvent::new(code, KeyModifiers::NONE)),
            Some(expected)
        );
    }
    assert_eq!(
        map_key_event(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Some(Key::Quit)
    );

    let mut released = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
    released.kind = KeyEventKind::Release;
    assert_eq!(map_key_event(released), None);
}

#[test]
fn state_machine_drills_down_and_backs_out_without_terminal_io() {
    let cycle = synthetic_cycle();
    let mut detail = adopt(Detail::empty(), &cycle.report);
    detail.cursor = detail
        .rows
        .iter()
        .position(|row| matches!(row, RowId::SharedFile { .. }))
        .expect("shared row");
    detail = apply(detail, Key::Enter);
    assert!(matches!(detail.mode, Mode::OpeningHunks { .. }));
    detail = show_hunks(
        detail,
        "src/conflict.rs".into(),
        "<<<<<<<\n=======\n>>>>>>>\n".into(),
        false,
    );
    detail = apply(detail, Key::Back);
    assert_eq!(detail.mode, Mode::List);
    detail = apply(detail, Key::Back);
    assert_eq!(detail.mode, Mode::Done);
}
