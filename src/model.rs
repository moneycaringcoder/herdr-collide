//! Shared types. This module is the contract between the git layer, the herdr
//! socket client, the analysis pass, and the renderers, so that each can be
//! developed and tested independently.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

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

/// Where each checkout's working tree actually starts, keyed by workspace id.
///
/// This is *not* a field on [`Checkout`], because herdr does not report it and
/// cannot: it is what `git rev-parse --show-toplevel` answers, and it has to be
/// resolved from disk. `collide::gather_for` resolves it once per checkout and
/// hands the result to the pure analysis pass, which keeps the filesystem out of
/// [`collide::analyse`].
///
/// It exists because a path prefix is not a working tree. Two checkouts share a
/// working tree exactly when their top levels are equal — a workspace opened on
/// `<root>/src` resolves to `<root>` and really is the same tree, while a linked
/// worktree at `<root>/.worktrees/api` resolves to itself and is a different one
/// even though its path sits underneath. Deciding that by prefix silently
/// stopped every worktree in a `.worktrees/` layout from being compared with the
/// repository it lives in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkTrees {
    roots: BTreeMap<String, PathBuf>,
}

impl WorkTrees {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, workspace_id: impl Into<String>, root: impl Into<PathBuf>) {
        self.roots.insert(workspace_id.into(), root.into());
    }

    pub fn get(&self, workspace_id: &str) -> Option<&Path> {
        self.roots.get(workspace_id).map(PathBuf::as_path)
    }

    /// Whether two workspaces are checked out on the same working tree.
    ///
    /// An unresolved top level answers `false`: "I do not know" must not become
    /// "these are the same tree", because the consequence of that claim is a
    /// pair silently dropped from the comparison. Pairing two checkouts that
    /// turn out to share a tree costs a visible, explicable overlap; refusing to
    /// pair two that do not costs a conflict nobody is ever shown.
    pub fn same_tree(&self, left_workspace_id: &str, right_workspace_id: &str) -> bool {
        match (self.get(left_workspace_id), self.get(right_workspace_id)) {
            (Some(left), Some(right)) => left == right,
            _ => false,
        }
    }
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
    /// Lines this path contributes to the change set. Carried per path so that
    /// `ignore_suffixes` can drop a path's volume along with the path itself —
    /// a `package-lock.json` that the plugin has decided to ignore must not
    /// still trip the runaway threshold.
    pub lines_added: u64,
    pub lines_removed: u64,
    /// True for the *original* path of a rename. Both halves belong to the
    /// change set, because a sibling worktree editing the old name really does
    /// collide — but one rename is one changed file, so the origin half must not
    /// count twice toward `runaway_files`.
    pub is_rename_origin: bool,
    /// The submodule's working tree differs from its committed pointer, so the
    /// superproject snapshot cannot predict whether those contents conflict.
    pub submodule_contents_uncomparable: bool,
}

impl ChangedPath {
    /// A plain changed path with no volume attached, for callers that only care
    /// about the set of paths.
    pub fn new(path: impl Into<String>, kind: ChangeKind) -> Self {
        Self {
            path: path.into(),
            kind,
            lines_added: 0,
            lines_removed: 0,
            is_rename_origin: false,
            submodule_contents_uncomparable: false,
        }
    }

    pub fn lines_changed(&self) -> u64 {
        self.lines_added.saturating_add(self.lines_removed)
    }
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
    /// True when git reported a rename in this checkout. A rename can conflict
    /// on a path that appears under a different name in each change set, so a
    /// pair with no literal path in common still has to be predicted when
    /// either side renamed something.
    pub has_rename: bool,
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
///
/// `Unknown` is not a weaker `Overlap`. It means prediction could not run, so
/// the honest answer is "I do not know", and the severity ladder ranks it above
/// a known-clean overlap for exactly that reason.
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
    /// True when the prediction for this pair had to force a single merge base
    /// although the histories offer more than one. The verdicts are then an
    /// approximation of what a real merge would do, and the detail view says so
    /// rather than presenting them as final.
    ///
    /// A pair with *no* common ancestor is not approximate — it is refused
    /// outright and its files stay [`FileVerdict::Unknown`], because there is no
    /// merge to approximate.
    pub approximate: bool,
}

impl Pairing {
    pub fn conflicts(&self) -> usize {
        self.shared
            .iter()
            .filter(|f| f.verdict == FileVerdict::Conflict)
            .count()
    }

    pub fn unknowns(&self) -> usize {
        self.shared
            .iter()
            .filter(|f| f.verdict == FileVerdict::Unknown)
            .count()
    }

    /// Returns the three-rung severity key used to rank pairings worst first:
    /// conflict count, then unknown count, then overlap count.
    ///
    /// Unknowns precede overlaps because an unknown is a missing answer, while
    /// an overlap is known to merge cleanly. Burying the missing answer under
    /// the harmless one would repeat the mistake the `Unknown` severity exists
    /// to prevent.
    pub fn severity_rank_key(
        &self,
    ) -> (
        std::cmp::Reverse<usize>,
        std::cmp::Reverse<usize>,
        std::cmp::Reverse<usize>,
    ) {
        let (mut conflicts, mut unknowns, mut overlaps) = (0, 0, 0);
        for file in &self.shared {
            match file.verdict {
                FileVerdict::Conflict => conflicts += 1,
                FileVerdict::Unknown => unknowns += 1,
                FileVerdict::Overlap => overlaps += 1,
            }
        }
        (
            std::cmp::Reverse(conflicts),
            std::cmp::Reverse(unknowns),
            std::cmp::Reverse(overlaps),
        )
    }
}

/// Worst-case state for a single workspace, which is what the badge shows.
///
/// The order is the precedence order. Only one severity is ever live, so every
/// rung is a decision about what the badge gives up.
///
/// `Unknown` above `Overlap`: an overlap badge means "both of you touched this
/// file and it merges clean", which is a claim about a merge, and a prediction
/// that could not run has not earned it.
///
/// `Unknown` above `Runaway` needs its own argument, because a runaway is not a
/// claim about a merge and is known with certainty while an unknown verdict is
/// the absence of knowledge. Three things decide it:
///
/// * **Nothing is lost.** A runaway that loses the badge still says so
///   everywhere else — `WorkspaceStatus::runaway` stays true, `--json` reports
///   it, and the detail pane prints the word `runaway` on the worktree line next
///   to the badge. An unknown verdict that loses the badge has nowhere else to
///   go in the sidebar. Demoting a runaway costs a decoration; demoting an
///   unknown costs the only signal.
/// * **Rarity should win.** A runaway is a slow-burn heuristic about one
///   workspace's own size, and on a busy branch it is on almost permanently. A
///   failed prediction is rare and transient. Ranking the common signal above
///   the rare one means the rare one is never seen; ranking it the other way
///   round costs the common one a badge it will get back on the next cycle.
/// * **Shape.** This plugin exists to answer one question — are these two agents
///   about to collide? A runaway is a side observation. `Unknown` is that
///   question going unanswered, which is closer to `Conflict` than to anything
///   measuring volume, and it belongs next to it.
///
/// The cost is real and worth stating: a workspace that is certainly a runaway
/// and has one unpredictable shared file badges `?` rather than `⚠`, so the
/// volume signal waits for the pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Severity {
    #[default]
    Clean,
    Overlap,
    Runaway,
    Unknown,
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
            Severity::Unknown => "collide_unknown",
            Severity::Conflict => "collide_conflict",
        }
    }

    pub const ALL_TOKENS: [&'static str; 5] = [
        "collide_clean",
        "collide_overlap",
        "collide_runaway",
        "collide_unknown",
        "collide_conflict",
    ];
}

/// What the daemon pushes for one workspace on one cycle.
///
/// Deliberately carries no rendered text. `render::badge` is the single author
/// of the badge string: two independent builders disagreed about what a clean
/// workspace should emit, and the protocol needs an empty string there to clear
/// the token rather than a tick to draw one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceStatus {
    pub workspace_id: String,
    pub severity: Severity,
    pub overlap_count: usize,
    pub conflict_count: usize,
    /// Shared files whose verdict could not be established.
    pub unknown_count: usize,
    pub runaway: bool,
    /// Size of this checkout's change set, which is what a runaway badge
    /// reports. Counts alone cannot express it: a runaway is usually a
    /// workspace sharing nothing at all with its siblings.
    pub lines_changed: u64,
    /// Distinct changed files after `ignore_suffixes`, with the origin half of
    /// a rename counted once. A runaway tripped on the file threshold alone
    /// carries no lines, so the badge falls back to this rather than rendering
    /// a bare mark with no magnitude.
    pub changed_files: usize,
}

/// Full analysis for one refresh cycle, shared by the badge daemon, the detail
/// pane, and the JSON action.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub checkouts: Vec<Checkout>,
    pub pairings: Vec<Pairing>,
    pub statuses: Vec<WorkspaceStatus>,
    /// Change set per workspace id, kept so the detail view can explain *why* a
    /// checkout is degraded instead of inferring it from a missing branch.
    pub changes: Vec<(String, ChangeSet)>,
}

impl Report {
    pub fn change_set(&self, workspace_id: &str) -> Option<&ChangeSet> {
        self.changes
            .iter()
            .find(|(id, _)| id == workspace_id)
            .map(|(_, set)| set)
    }
}
