//! Badge updater lifecycle: detached daemon, pid/enabled markers, TTL badge
//! pushes, and cleanup that survives being killed. See docs/herdr-protocol.md
//! for the lifecycle contract these verbs implement.

use crate::config::Config;
use crate::Result;

pub fn enable() -> Result<()> {
    unimplemented!("enable")
}

pub fn disable() -> Result<()> {
    unimplemented!("disable")
}

pub fn toggle() -> Result<()> {
    unimplemented!("toggle")
}

/// herdr startup hook. Silent no-op unless the enabled marker is set and no
/// daemon is currently live.
pub fn restore() -> Result<()> {
    unimplemented!("restore")
}

/// The refresh loop itself, running in the foreground.
pub fn run(_config: &Config) -> Result<()> {
    unimplemented!("daemon loop")
}
