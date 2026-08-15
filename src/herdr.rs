//! herdr socket client.
//!
//! Newline-delimited JSON over the socket at `HERDR_SOCKET_PATH`. The server
//! answers exactly one request per connection and then closes, so every call
//! must be able to reconnect and retry once — see docs/herdr-protocol.md.

use crate::model::Checkout;
use crate::Result;

pub struct Herdr {
    _private: (),
}

impl Herdr {
    pub fn connect() -> Result<Self> {
        unimplemented!("socket connect")
    }

    /// One `session.snapshot` call, reduced to the git-backed workspaces.
    /// Workspaces with no `worktree` key are not repos and are skipped.
    pub fn checkouts(&mut self) -> Result<Vec<Checkout>> {
        unimplemented!("session.snapshot")
    }

    /// Sets one badge token on a workspace, with a TTL so it self-clears if
    /// this process dies.
    pub fn set_badge(
        &mut self,
        _workspace_id: &str,
        _token: &str,
        _value: &str,
        _ttl_ms: u64,
    ) -> Result<()> {
        unimplemented!("workspace.report_metadata")
    }

    /// Clears one badge token. Sends a null value and no TTL.
    pub fn clear_badge(&mut self, _workspace_id: &str, _token: &str) -> Result<()> {
        unimplemented!("workspace.report_metadata clear")
    }

    pub fn notify(&mut self, _title: &str, _body: &str) -> Result<()> {
        unimplemented!("notification.show")
    }
}
