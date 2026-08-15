//! Rendering: the badge string that rides a workspace token, and the live
//! detail pane.

use crate::config::Config;
use crate::model::{Report, WorkspaceStatus};
use crate::Result;

/// Badge text for one workspace, e.g. `✘ 2` or `⧉ 3`. Severity itself is
/// carried by the token *name*, not this string.
pub fn badge(_status: &WorkspaceStatus) -> String {
    unimplemented!("badge text")
}

/// Full-screen detail view of one report.
pub fn detail(_report: &Report) -> String {
    unimplemented!("detail view")
}

/// `--watch`: render the detail view on an interval until interrupted.
pub fn run_watch(_config: &Config) -> Result<()> {
    unimplemented!("watch loop")
}
