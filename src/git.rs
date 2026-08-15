//! Git layer: change sets and conflict prediction.
//!
//! Every invocation is read-only against the user's repository. The only writes
//! this module ever performs are to temporary index files under the plugin
//! state dir, via `GIT_INDEX_FILE`, and every invocation passes
//! `--no-optional-locks` so it cannot contend with an agent's own git commands.

use std::path::Path;
use std::time::Duration;

use crate::model::ChangeSet;
use crate::Result;

/// Everything `checkout` has changed relative to its merge base with `base`:
/// staged, unstaged, untracked, conflicted, and committed-since-merge-base.
///
/// Degrades rather than failing for detached HEAD, an unborn branch, or a
/// deleted branch — the returned `ChangeSet` carries `degraded` in those cases.
pub fn change_set(_checkout: &Path, _base: &str, _timeout: Duration) -> Result<ChangeSet> {
    unimplemented!("change set collection")
}

/// Whether the two checkouts' changes to `path` actually conflict, as opposed
/// to merely touching the same file.
pub fn predict_conflict(
    _left: &Path,
    _right: &Path,
    _paths: &[String],
    _timeout: Duration,
) -> Result<Vec<(String, bool)>> {
    unimplemented!("conflict prediction")
}

/// Branch name for a checkout, or `None` when HEAD is detached.
pub fn current_branch(_checkout: &Path, _timeout: Duration) -> Result<Option<String>> {
    unimplemented!("branch lookup")
}
