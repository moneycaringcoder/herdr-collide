//! herdr socket client.
//!
//! Newline-delimited JSON over the socket at `HERDR_SOCKET_PATH`. The server
//! answers exactly one request per connection and then closes, so every call
//! must be able to reconnect and retry once — see docs/herdr-protocol.md.

use std::fmt;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::config;
use crate::model::{Checkout, RepoKey};
use crate::Result;

/// Reference value from the protocol notes. Long enough that a busy server is
/// not mistaken for a dead one, short enough that the refresh loop can never
/// wedge behind one call.
const IO_TIMEOUT: Duration = Duration::from_secs(15);

/// herdr rejects a `ttl_ms` outside this range with `invalid_metadata_ttl`.
/// Clamping is better than losing the push: the protocol notes say to clamp the
/// cadence, and a badge with a slightly wrong TTL still renders.
const MIN_TTL_MS: u64 = 1;
const MAX_TTL_MS: u64 = 86_400_000;

/// A herdr error envelope, carried as a real error type so callers can tell
/// `workspace_not_found` (a workspace closed under us — benign) from a
/// transport failure (we are blind and should say so).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HerdrError {
    pub code: String,
    pub message: String,
}

impl fmt::Display for HerdrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "herdr {}: {}", self.code, self.message)
    }
}

impl std::error::Error for HerdrError {}

/// Error code from a herdr error envelope, or `None` for a transport failure.
pub fn error_code<'a>(err: &'a (dyn std::error::Error + 'static)) -> Option<&'a str> {
    err.downcast_ref::<HerdrError>().map(|e| e.code.as_str())
}

/// Split so that only transport failures are retried. Retrying a rejected
/// request would just be rejected again, and would double-count against
/// herdr's own error accounting.
enum Failure {
    Transport(String),
    Protocol(HerdrError),
}

#[derive(Debug)]
pub struct Herdr {
    socket_path: PathBuf,
    next_id: u64,
}

impl Herdr {
    pub fn connect() -> Result<Self> {
        let socket_path = socket_path()?;
        // Dial once so a missing server is reported here, with the path, rather
        // than as a confusing failure inside the first call. The connection is
        // dropped immediately: one request per connection means there is
        // nothing worth holding open.
        dial(&socket_path)?;
        Ok(Self {
            socket_path,
            next_id: 0,
        })
    }

    /// One `session.snapshot` call, reduced to the git-backed workspaces.
    /// Workspaces with no `worktree` key are not repos and are skipped.
    pub fn checkouts(&mut self) -> Result<Vec<Checkout>> {
        let result = self.call("session.snapshot", json!({}))?;
        // The payload is `{"type":"session_snapshot","snapshot":{...}}`; the
        // arrays live one level down, under `snapshot`. Verified against a live
        // 0.8.0 server — reading them off the result object silently yields no
        // workspaces at all, which looks exactly like an idle session.
        //
        // Absent `snapshot` is an error rather than a fallback: an empty
        // checkout list is indistinguishable from an idle session, so a
        // protocol change here would make the plugin quietly report nothing at
        // all instead of failing.
        let snapshot = result.get("snapshot").ok_or_else(|| {
            format!(
                "session.snapshot returned no `snapshot` object (result type `{}`)",
                text(&result, "type").unwrap_or("missing")
            )
        })?;
        Ok(reduce_snapshot(snapshot))
    }

    /// Sets one badge token on a workspace, with a TTL so it self-clears if
    /// this process dies.
    pub fn set_badge(
        &mut self,
        workspace_id: &str,
        token: &str,
        value: &str,
        ttl_ms: u64,
    ) -> Result<()> {
        // `tokens` is a merge patch: only the named token is touched, which is
        // why a severity flip has to clear the previous name explicitly.
        self.call(
            "workspace.report_metadata",
            json!({
                "workspace_id": workspace_id,
                "source": config::plugin_id(),
                "tokens": { token: value },
                "ttl_ms": ttl_ms.clamp(MIN_TTL_MS, MAX_TTL_MS),
            }),
        )?;
        Ok(())
    }

    /// Clears one badge token. Sends a null value and no TTL.
    pub fn clear_badge(&mut self, workspace_id: &str, token: &str) -> Result<()> {
        // A null value is the delete in the merge patch, and `ttl_ms` must be
        // omitted entirely — sending one alongside a delete is rejected.
        self.call(
            "workspace.report_metadata",
            json!({
                "workspace_id": workspace_id,
                "source": config::plugin_id(),
                "tokens": { token: Value::Null },
            }),
        )?;
        Ok(())
    }

    pub fn notify(&mut self, title: &str, body: &str) -> Result<()> {
        self.call("notification.show", json!({ "title": title, "body": body }))?;
        Ok(())
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = format!("collide:{}", self.next_id);
        match self.call_once(&id, method, &params) {
            Ok(result) => Ok(result),
            Err(Failure::Protocol(err)) => Err(Box::new(err)),
            // One request per connection is the normal path, not an error path:
            // the server EOFs after answering, so the connection we would reuse
            // is already gone. The same retry carries the client across a
            // `herdr update --handoff`, where the first attempt lands on a
            // socket the old server has just unlinked.
            Err(Failure::Transport(first)) => match self.call_once(&id, method, &params) {
                Ok(result) => Ok(result),
                Err(Failure::Protocol(err)) => Err(Box::new(err)),
                Err(Failure::Transport(second)) => {
                    Err(format!("{method} failed twice: {first}; on retry: {second}").into())
                }
            },
        }
    }

    fn call_once(
        &self,
        id: &str,
        method: &str,
        params: &Value,
    ) -> std::result::Result<Value, Failure> {
        let stream = dial(&self.socket_path).map_err(|e| Failure::Transport(e.to_string()))?;

        // `params` is mandatory and must be an object — never null, `{}` when
        // empty.
        let params = if params.is_object() {
            params.clone()
        } else {
            Value::Object(Map::new())
        };
        let mut line = serde_json::to_string(&json!({
            "id": id,
            "method": method,
            "params": params,
        }))
        .map_err(|e| Failure::Transport(format!("could not encode request: {e}")))?;
        line.push('\n');

        (&stream)
            .write_all(line.as_bytes())
            .and_then(|()| (&stream).flush())
            .map_err(|e| Failure::Transport(format!("write to {method} failed: {e}")))?;

        let mut response = String::new();
        BufReader::new(&stream)
            .read_line(&mut response)
            .map_err(|e| Failure::Transport(format!("read of {method} response failed: {e}")))?;
        if response.trim().is_empty() {
            return Err(Failure::Transport(
                "server closed the connection without answering".into(),
            ));
        }

        let value: Value = serde_json::from_str(response.trim_end())
            .map_err(|e| Failure::Transport(format!("malformed response to {method}: {e}")))?;

        if let Some(err) = value.get("error") {
            return Err(Failure::Protocol(HerdrError {
                code: text(err, "code").unwrap_or("unknown_error").to_string(),
                message: text(err, "message").unwrap_or("no message").to_string(),
            }));
        }
        match value.get("result") {
            Some(result) => Ok(result.clone()),
            None => Err(Failure::Transport(format!(
                "response to {method} carried neither result nor error"
            ))),
        }
    }
}

fn dial(socket_path: &Path) -> Result<UnixStream> {
    let stream = UnixStream::connect(socket_path)
        .map_err(|e| format!("cannot reach herdr at {}: {e}", socket_path.display()))?;
    // Without these a half-open socket parks the refresh loop forever.
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok(stream)
}

fn socket_path() -> Result<PathBuf> {
    // herdr injects this into everything it spawns; the fallback exists only
    // for hand invocation from a shell.
    if let Some(path) = config::non_empty_env("HERDR_SOCKET_PATH") {
        return Ok(PathBuf::from(path));
    }
    let config_home = config::non_empty_env("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| config::non_empty_env("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or("HERDR_SOCKET_PATH is unset and neither XDG_CONFIG_HOME nor HOME is set")?;
    Ok(config_home.join("herdr").join("herdr.sock"))
}

/// Non-empty string field, since herdr reports absent context as an empty
/// string rather than as a missing key.
fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value.get(key).and_then(Value::as_array).map_or(&[], |a| a)
}

/// Reduces a `session.snapshot` result to the git-backed workspaces. The flat
/// sibling arrays are joined on `workspace_id`.
fn reduce_snapshot(snapshot: &Value) -> Vec<Checkout> {
    let mut agents: Vec<(String, String)> = Vec::new();
    let mut record_agent = |workspace_id: Option<&str>, name: Option<&str>| {
        if let (Some(workspace_id), Some(name)) = (workspace_id, name) {
            if !agents.iter().any(|(id, _)| id == workspace_id) {
                agents.push((workspace_id.to_string(), name.to_string()));
            }
        }
    };
    for agent in array(snapshot, "agents") {
        // `name` is the user's own label for the agent ("gitsmith"); `agent` is
        // the program ("claude"). `agent_session` is an object on the wire, not
        // a string, so it is no use as a display name.
        let name = text(agent, "name").or_else(|| text(agent, "agent"));
        record_agent(text(agent, "workspace_id"), name);
    }
    for pane in array(snapshot, "panes") {
        record_agent(text(pane, "workspace_id"), text(pane, "agent"));
    }

    let mut checkouts = Vec::new();
    for workspace in array(snapshot, "workspaces") {
        // No `worktree` key means the workspace is not a repo. That is data,
        // not an error: most sessions have at least one such workspace.
        let Some(worktree) = workspace.get("worktree").filter(|w| w.is_object()) else {
            continue;
        };
        let (Some(workspace_id), Some(repo_key), Some(checkout_path)) = (
            text(workspace, "workspace_id"),
            text(worktree, "repo_key"),
            text(worktree, "checkout_path"),
        ) else {
            continue;
        };
        let repo_root = text(worktree, "repo_root").unwrap_or(checkout_path);
        checkouts.push(Checkout {
            workspace_id: workspace_id.to_string(),
            workspace_label: text(workspace, "label").unwrap_or(workspace_id).to_string(),
            repo_key: RepoKey(repo_key.to_string()),
            repo_root: PathBuf::from(repo_root),
            checkout_path: PathBuf::from(checkout_path),
            is_linked_worktree: worktree
                .get("is_linked_worktree")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            // Branch names are absent from the workspace object; a later pass
            // fills them from `worktree.list`.
            branch: None,
            agent: agents
                .iter()
                .find(|(id, _)| id == workspace_id)
                .map(|(_, name)| name.clone()),
        });
    }
    checkouts
}
