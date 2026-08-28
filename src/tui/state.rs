//! Pure state machine for the interactive collision detail pane.

use std::collections::BTreeMap;

use crate::model::{Checkout, Pairing, RepoKey, Report, SharedFile};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowId {
    Checkout(String),
    SharedFile {
        left_workspace_id: String,
        right_workspace_id: String,
        path: String,
    },
}

impl RowId {
    pub fn shared_path(&self) -> Option<&str> {
        match self {
            Self::SharedFile { path, .. } => Some(path),
            Self::Checkout(_) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Enter,
    Back,
    Quit,
    Rescan,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    List,
    CheckoutDetail { workspace_id: String },
    OpeningHunks { path: String },
    Hunks {
        path: String,
        text: String,
        prediction_failed: bool,
        scroll: usize,
    },
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detail {
    pub rows: Vec<RowId>,
    pub cursor: usize,
    pub mode: Mode,
    pub message: Option<String>,
    pub refresh_requested: bool,
}

impl Detail {
    pub fn empty() -> Self {
        Self {
            rows: Vec::new(),
            cursor: 0,
            mode: Mode::List,
            message: None,
            refresh_requested: false,
        }
    }

    pub fn selected(&self) -> Option<&RowId> {
        self.rows.get(self.cursor)
    }

    pub fn is_finished(&self) -> bool {
        self.mode == Mode::Done
    }

    pub fn open_hunk_path(&self) -> Option<&str> {
        match &self.mode {
            Mode::OpeningHunks { path } | Mode::Hunks { path, .. } => Some(path),
            _ => None,
        }
    }
}

pub fn apply(mut detail: Detail, key: Key) -> Detail {
    if key == Key::Quit {
        detail.mode = Mode::Done;
        return detail;
    }
    if key == Key::Rescan {
        detail.refresh_requested = true;
        return detail;
    }

    match detail.mode.clone() {
        Mode::List => match key {
            Key::Up => detail.cursor = detail.cursor.saturating_sub(1),
            Key::Down => {
                detail.cursor = detail
                    .cursor
                    .saturating_add(1)
                    .min(detail.rows.len().saturating_sub(1));
            }
            Key::Enter => match detail.selected().cloned() {
                Some(RowId::Checkout(workspace_id)) => {
                    detail.mode = Mode::CheckoutDetail { workspace_id };
                }
                Some(RowId::SharedFile { path, .. }) => {
                    detail.mode = Mode::OpeningHunks { path };
                }
                None => {}
            },
            Key::Back => detail.mode = Mode::Done,
            Key::Quit | Key::Rescan | Key::Other => {}
        },
        Mode::CheckoutDetail { .. } => {
            if key == Key::Back {
                detail.mode = Mode::List;
            }
        }
        Mode::OpeningHunks { .. } => {}
        Mode::Hunks {
            path,
            text,
            prediction_failed,
            mut scroll,
        } => {
            match key {
                Key::Up => scroll = scroll.saturating_sub(1),
                Key::Down => {
                    scroll = scroll
                        .saturating_add(1)
                        .min(text.lines().count().saturating_sub(1));
                }
                Key::Back => {
                    detail.mode = Mode::List;
                    return detail;
                }
                Key::Enter | Key::Quit | Key::Rescan | Key::Other => {}
            }
            detail.mode = Mode::Hunks {
                path,
                text,
                prediction_failed,
                scroll,
            };
        }
        Mode::Done => {}
    }
    detail
}

pub fn show_hunks(
    mut detail: Detail,
    path: String,
    text: String,
    prediction_failed: bool,
) -> Detail {
    let previous_scroll = match &detail.mode {
        Mode::Hunks {
            path: current,
            scroll,
            ..
        } if current == &path => *scroll,
        _ => 0,
    };
    let scroll = previous_scroll.min(text.lines().count().saturating_sub(1));
    detail.mode = Mode::Hunks {
        path,
        text,
        prediction_failed,
        scroll,
    };
    detail
}

pub fn refresh_failed(mut detail: Detail, message: String) -> Detail {
    detail.refresh_requested = false;
    detail.message = Some(format!("Refresh failed: {message}"));
    detail
}

/// Adopts a new report while carrying cursor and drill-down identity by stable
/// row key. Indices are frame-local and must never be carried across refreshes.
pub fn adopt(mut detail: Detail, report: &Report) -> Detail {
    let old_cursor = detail.cursor;
    let selected = detail.selected().cloned();
    let open_path = detail.open_hunk_path().map(str::to_string);
    let rows = display_order(report);

    detail.cursor = selected
        .as_ref()
        .and_then(|selected| rows.iter().position(|row| row == selected))
        .or_else(|| {
            open_path.as_deref().and_then(|path| {
                rows.iter()
                    .position(|row| row.shared_path() == Some(path))
            })
        })
        .unwrap_or_else(|| old_cursor.min(rows.len().saturating_sub(1)));
    detail.rows = rows;
    detail.refresh_requested = false;
    detail.message = None;

    match &detail.mode {
        Mode::CheckoutDetail { workspace_id }
            if !detail
                .rows
                .contains(&RowId::Checkout(workspace_id.clone())) =>
        {
            detail.mode = Mode::List;
            detail.message = Some(
                "The focused checkout vanished during refresh; returned to the list.".into(),
            );
        }
        Mode::OpeningHunks { path } | Mode::Hunks { path, .. }
            if !detail
                .rows
                .iter()
                .any(|row| row.shared_path() == Some(path)) =>
        {
            let path = path.clone();
            detail.mode = Mode::List;
            detail.message = Some(format!(
                "`{path}` is no longer shared; returned to the list."
            ));
        }
        _ => {}
    }
    detail
}

pub(crate) struct RepoGroup<'a> {
    pub key: &'a RepoKey,
    pub checkouts: Vec<&'a Checkout>,
}

pub(crate) fn repo_groups(report: &Report) -> Vec<RepoGroup<'_>> {
    let mut grouped: BTreeMap<&RepoKey, Vec<&Checkout>> = BTreeMap::new();
    for checkout in &report.checkouts {
        grouped.entry(&checkout.repo_key).or_default().push(checkout);
    }
    grouped
        .into_iter()
        .map(|(key, mut checkouts)| {
            checkouts.sort_by(|left, right| {
                crate::render::label_of(left)
                    .cmp(crate::render::label_of(right))
                    .then_with(|| left.workspace_id.cmp(&right.workspace_id))
            });
            RepoGroup { key, checkouts }
        })
        .collect()
}

pub(crate) fn pairings_for_repo<'a>(report: &'a Report, key: &RepoKey) -> Vec<&'a Pairing> {
    let by_id: BTreeMap<&str, &Checkout> = report
        .checkouts
        .iter()
        .map(|checkout| (checkout.workspace_id.as_str(), checkout))
        .collect();
    let label = |workspace_id: &str| {
        by_id
            .get(workspace_id)
            .map(|checkout| crate::render::label_of(checkout).to_string())
            .unwrap_or_else(|| workspace_id.to_string())
    };
    let mut pairings: Vec<_> = report
        .pairings
        .iter()
        .filter(|pairing| {
            by_id
                .get(pairing.left_workspace_id.as_str())
                .or_else(|| by_id.get(pairing.right_workspace_id.as_str()))
                .is_some_and(|checkout| &checkout.repo_key == key)
                && !pairing.shared.is_empty()
        })
        .collect();
    pairings.sort_by_key(|pairing| {
        (
            pairing.severity_rank_key(),
            label(&pairing.left_workspace_id),
            label(&pairing.right_workspace_id),
        )
    });
    pairings
}

pub(crate) fn files_for_pair(pairing: &Pairing) -> Vec<&SharedFile> {
    let mut files: Vec<_> = pairing.shared.iter().collect();
    files.sort_by(|left, right| {
        crate::render::verdict_rank(left.verdict)
            .cmp(&crate::render::verdict_rank(right.verdict))
            .then_with(|| left.path.cmp(&right.path))
    });
    files
}

pub fn display_order(report: &Report) -> Vec<RowId> {
    let mut rows = Vec::new();
    for group in repo_groups(report) {
        rows.extend(
            group
                .checkouts
                .iter()
                .map(|checkout| RowId::Checkout(checkout.workspace_id.clone())),
        );
        for pairing in pairings_for_repo(report, group.key) {
            rows.extend(files_for_pair(pairing).into_iter().map(|file| {
                RowId::SharedFile {
                    left_workspace_id: pairing.left_workspace_id.clone(),
                    right_workspace_id: pairing.right_workspace_id.clone(),
                    path: file.path.clone(),
                }
            }));
        }
    }
    rows
}
