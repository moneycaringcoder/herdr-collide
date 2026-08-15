//! Analysis: group checkouts by repo, pair them, and turn shared paths into
//! severities. Pure over its inputs so it can be tested without herdr or git.

use crate::config::Config;
use crate::model::{Checkout, ChangeSet, Report};
use crate::Result;

/// Groups checkouts by repo key, pairs every distinct checkout within a repo,
/// and derives a per-workspace severity. Checkouts from different repos are
/// never compared.
pub fn analyse(
    _checkouts: &[Checkout],
    _changes: &[(String, ChangeSet)],
    _config: &Config,
) -> Report {
    unimplemented!("pairing and severity")
}

pub fn run_once(_config: &Config) -> Result<()> {
    unimplemented!("one-shot report")
}

pub fn run_json(_config: &Config) -> Result<()> {
    unimplemented!("json report")
}
