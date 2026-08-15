//! Shared types. This module is the contract between the git layer, the herdr
//! socket client, the analysis pass, and the renderers, so that each can be
//! developed and tested independently.

use std::collections::BTreeSet;
use std::path::PathBuf;

/// Canonical identity for "the same repository", taken from herdr's
/// `workspace.worktree.repo_key` (the `.git` path). Two checkouts are only ever
/// compared when their repo keys match.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoKey(pub String);

/// One herdr workspace that is backed by a git checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkout {
    pub workspace_id: String,
    pub workspace_label: String,
    pub repo_key: RepoKey,
    pub repo_root: PathBuf,
    pub checkout_path: PathBuf,
    pub is_linked_worktree: bool,
    /// From `worktree.list`; absent for detached HEAD or when the lookup failed.
    pub branch: Option<String>,
    /// Agent occupying this workspace, if herdr reports one.
    pub agent: Option<String>,
}

/// How a path came to be in a change set. Ordering is significance order: a
/// path present for several reasons keeps the most significant one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChangeKind {
    Untracked,
    Committed,
    Staged,
    Unstaged,
    Conflicted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedPath {
    pub path: String,
    pub kind: ChangeKind,
}

/// Everything one checkout has changed relative to its merge base, plus enough
/// volume detail to judge a runaway agent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeSet {
    pub paths: Vec<ChangedPath>,
    pub lines_added: u64,
    pub lines_removed: u64,
    /// True when the checkout is in a state we could only partially read
    /// (unborn branch, deleted branch, detached HEAD with no merge base).
    pub degraded: bool,
    pub degraded_reason: Option<String>,
}

impl ChangeSet {
    pub fn path_set(&self) -> BTreeSet<&str> {
        self.paths.iter().map(|p| p.path.as_str()).collect()
    }

    pub fn lines_changed(&self) -> u64 {
        self.lines_added.saturating_add(self.lines_removed)
    }
}

/// Per-file verdict for one pair of checkouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FileVerdict {
    /// Both sides touched the file; the changes merge cleanly.
    Overlap,
    /// Both sides touched the file and a real textual conflict is predicted.
    Conflict,
    /// Both sides touched the file, but conflict prediction could not run.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedFile {
    pub path: String,
    pub verdict: FileVerdict,
}

/// One ordered pair of checkouts within a repo, and what they share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pairing {
    pub left_workspace_id: String,
    pub right_workspace_id: String,
    pub shared: Vec<SharedFile>,
}

impl Pairing {
    pub fn conflicts(&self) -> usize {
        self.shared.iter().filter(|f| f.verdict == FileVerdict::Conflict).count()
    }
}

/// Worst-case state for a single workspace, which is what the badge shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Severity {
    #[default]
    Clean,
    Overlap,
    Runaway,
    Conflict,
}

impl Severity {
    /// Token name carrying this severity. Severity is encoded in the token
    /// *name* because herdr renders a token value as flat text and cannot
    /// colour by content.
    pub fn token_name(self) -> &'static str {
        match self {
            Severity::Clean => "collide_clean",
            Severity::Overlap => "collide_overlap",
            Severity::Runaway => "collide_runaway",
            Severity::Conflict => "collide_conflict",
        }
    }

    pub const ALL_TOKENS: [&'static str; 4] =
        ["collide_clean", "collide_overlap", "collide_runaway", "collide_conflict"];
}

/// What the daemon pushes for one workspace on one cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceStatus {
    pub workspace_id: String,
    pub severity: Severity,
    /// Rendered badge text, e.g. `✘ 2` or `⧉ 3`.
    pub badge: String,
    pub overlap_count: usize,
    pub conflict_count: usize,
    pub runaway: bool,
}

/// Full analysis for one refresh cycle, shared by the badge daemon, the detail
/// pane, and the JSON action.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub checkouts: Vec<Checkout>,
    pub pairings: Vec<Pairing>,
    pub statuses: Vec<WorkspaceStatus>,
}
