//! Porcelain-v2 status and numstat parsing.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::time::Duration;

use crate::model::ChangeKind;
use crate::Result;

use super::{git_command, lossy, run_command, HeadState};

/// Parses the `--branch` headers from porcelain-v2 status.
pub fn parse_status_head(bytes: &[u8]) -> Option<HeadState> {
    let mut oid = None;
    let mut branch = None;
    for field in bytes.split(|byte| *byte == 0) {
        if let Some(value) = field.strip_prefix(b"# branch.oid ") {
            oid = Some(String::from_utf8_lossy(value).into_owned());
        } else if let Some(value) = field.strip_prefix(b"# branch.head ") {
            branch = Some(String::from_utf8_lossy(value).into_owned());
        }
    }
    match (oid.as_deref(), branch.as_deref()) {
        (Some("(initial)"), Some(name)) => Some(HeadState::Unborn {
            name: name.to_string(),
        }),
        (Some(oid), Some("(detached)")) => Some(HeadState::Detached {
            oid: oid.to_string(),
        }),
        (Some(oid), Some(name)) => Some(HeadState::Branch {
            name: name.to_string(),
            oid: oid.to_string(),
        }),
        _ => None,
    }
}

/// The three flags carried by a submodule's `S<c><m><u>` status field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubmoduleState {
    pub commit_changed: bool,
    pub modified_content: bool,
    pub untracked_content: bool,
}

/// One parsed `status --porcelain=v2` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    pub path: String,
    pub raw_path: Vec<u8>,
    pub origin: Option<String>,
    pub raw_origin: Option<Vec<u8>>,
    pub submodule: Option<SubmoduleState>,
    pub kind: ChangeKind,
    pub is_rename: bool,
    pub worktree_content: bool,
}

/// Parses `status --porcelain=v2 -z --untracked-files=all --renames` output.
pub fn parse_status_v2(bytes: &[u8]) -> Vec<StatusEntry> {
    let fields: Vec<&[u8]> = bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect();
    let mut entries = Vec::new();
    let mut index = 0;
    while index < fields.len() {
        let line = fields[index];
        match line[0] {
            b'#' => index += 1,
            b'1' => {
                if let (Some(path), Some(xy)) = (field_after_space(line, 8), xy_of(line)) {
                    entries.push(StatusEntry {
                        path: lossy(path),
                        raw_path: path.to_vec(),
                        origin: None,
                        raw_origin: None,
                        kind: kind_from_xy(xy),
                        submodule: submodule_state_of(line),
                        is_rename: false,
                        worktree_content: worktree_content_from_xy(xy),
                    });
                }
                index += 1;
            }
            b'2' => {
                if let (Some(path), Some(xy)) = (field_after_space(line, 9), xy_of(line)) {
                    let raw_origin = fields.get(index + 1).map(|field| field.to_vec());
                    let origin = raw_origin.as_deref().map(lossy);
                    entries.push(StatusEntry {
                        path: lossy(path),
                        raw_path: path.to_vec(),
                        origin,
                        raw_origin,
                        kind: kind_from_xy(xy),
                        submodule: submodule_state_of(line),
                        is_rename: true,
                        worktree_content: worktree_content_from_xy(xy),
                    });
                }
                index += 2;
            }
            b'u' => {
                if let Some(path) = field_after_space(line, 10) {
                    entries.push(StatusEntry {
                        path: lossy(path),
                        raw_path: path.to_vec(),
                        origin: None,
                        raw_origin: None,
                        kind: ChangeKind::Conflicted,
                        submodule: None,
                        is_rename: false,
                        worktree_content: true,
                    });
                }
                index += 1;
            }
            b'?' => {
                if let Some(path) = field_after_space(line, 1) {
                    entries.push(StatusEntry {
                        path: lossy(path),
                        raw_path: path.to_vec(),
                        origin: None,
                        raw_origin: None,
                        kind: ChangeKind::Untracked,
                        submodule: None,
                        is_rename: false,
                        worktree_content: false,
                    });
                }
                index += 1;
            }
            b'!' => index += 1,
            _ => index += 1,
        }
    }
    entries
}

pub(super) fn changed_index_paths(entries: &[StatusEntry]) -> Vec<&[u8]> {
    let mut paths = BTreeSet::new();
    for entry in entries {
        if entry.kind == ChangeKind::Untracked
            || entry.submodule.is_some()
            || !entry.worktree_content
        {
            continue;
        }
        paths.insert(entry.raw_path.as_slice());
    }
    paths.into_iter().collect()
}

pub(super) fn snapshot_path_still_hashable(
    checkout: &Path,
    raw_path: &[u8],
    timeout: Duration,
) -> Result<bool> {
    let mut command = git_command(checkout, &[]);
    command.args([
        "--no-optional-locks",
        "--literal-pathspecs",
        "status",
        "--porcelain=v2",
        "-z",
        "--untracked-files=no",
        "--",
    ]);
    command.arg(OsString::from_vec(raw_path.to_vec()));
    let status = run_command(
        command,
        timeout,
        format!("git status path probe in {}", checkout.display()),
    )?;
    if status.timed_out || !status.ok() {
        return Err(format!(
            "git status path probe failed in {}: {}",
            checkout.display(),
            status.stderr_text()
        )
        .into());
    }
    Ok(parse_status_v2(&status.stdout)
        .into_iter()
        .any(|entry| entry.raw_path == raw_path && entry.worktree_content))
}

fn field_after_space(line: &[u8], count: usize) -> Option<&[u8]> {
    let mut seen = 0;
    for (index, byte) in line.iter().enumerate() {
        if *byte == b' ' {
            seen += 1;
            if seen == count {
                return (index + 1 < line.len()).then_some(&line[index + 1..]);
            }
        }
    }
    None
}

fn xy_of(line: &[u8]) -> Option<(u8, u8)> {
    (line.len() >= 4 && line[1] == b' ').then_some((line[2], line[3]))
}

fn submodule_state_of(line: &[u8]) -> Option<SubmoduleState> {
    let field = line.split(|byte| *byte == b' ').nth(2)?;
    match field {
        [b'S', commit, modified, untracked] => Some(SubmoduleState {
            commit_changed: *commit == b'C',
            modified_content: *modified == b'M',
            untracked_content: *untracked == b'U',
        }),
        _ => None,
    }
}

fn worktree_content_from_xy((index, worktree): (u8, u8)) -> bool {
    !matches!(index, b'D' | b'T') && !matches!(worktree, b'D' | b'T')
}

fn kind_from_xy((index, worktree): (u8, u8)) -> ChangeKind {
    if index == b'U' || worktree == b'U' {
        ChangeKind::Conflicted
    } else if worktree != b'.' {
        ChangeKind::Unstaged
    } else {
        ChangeKind::Staged
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumStat {
    pub added: u64,
    pub removed: u64,
    pub paths: Vec<String>,
}

pub fn parse_numstat_z(bytes: &[u8]) -> Vec<NumStat> {
    let fields: Vec<&[u8]> = bytes.split(|byte| *byte == 0).collect();
    let mut output = Vec::new();
    let mut index = 0;
    while index < fields.len() {
        let field = fields[index];
        if field.is_empty() {
            index += 1;
            continue;
        }
        let mut parts = field.splitn(3, |byte| *byte == b'\t');
        let added = parts.next().unwrap_or(b"");
        let Some(removed) = parts.next() else {
            index += 1;
            continue;
        };
        let rest = parts.next().unwrap_or(b"");
        let added = count_of(added);
        let removed = count_of(removed);
        if rest.is_empty() {
            let mut paths = Vec::new();
            if let Some(old) = fields.get(index + 1) {
                paths.push(lossy(old));
            }
            if let Some(new) = fields.get(index + 2) {
                paths.push(lossy(new));
            }
            output.push(NumStat {
                added,
                removed,
                paths,
            });
            index += 3;
        } else {
            output.push(NumStat {
                added,
                removed,
                paths: vec![lossy(rest)],
            });
            index += 1;
        }
    }
    output
}

fn count_of(field: &[u8]) -> u64 {
    std::str::from_utf8(field)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(0)
}
