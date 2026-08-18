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

/// Pause before the single retry.
///
/// The retry exists to carry the client across a `herdr update --handoff`,
/// where the old server unlinks the socket and the new one binds it. A retry
/// fired immediately does not do that: measured back to back, the two attempts
/// were 0.05 ms apart, which is one attempt as far as the handoff window is
/// concerned. This is long enough to land on the other side of a rebind and
/// short enough that a refresh cycle never notices.
const RETRY_BACKOFF: Duration = Duration::from_millis(150);

/// How many times `connect` dials before giving up. `connect` is a call like
/// any other — `--disable`'s sweep, `--once` and the daemon's shutdown clear
/// all go through it — so it gets the same one retry the protocol notes require
/// of every call.
const CONNECT_ATTEMPTS: usize = 2;

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
/// Whether a successful `notification.show` request put a toast on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationDelivery {
    Shown,
    Disabled,
    Transient(NotificationTransient),
}

/// Reasons that did not display a toast but may succeed on a later cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationTransient {
    RateLimited,
    NoForegroundClient,
    Busy,
}

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
    skipped_worktrees: usize,
}

impl Herdr {
    pub fn connect() -> Result<Self> {
        let socket_path = socket_path();
        // Dial so a missing server is reported here, with the path, rather than
        // as a confusing failure inside the first call. The connection is
        // dropped immediately: one request per connection means there is
        // nothing worth holding open.
        //
        // Retried with the same backoff `call` uses, because a handoff can just
        // as easily land here as in the middle of a call.
        let mut last = None;
        for attempt in 0..CONNECT_ATTEMPTS {
            match dial(&socket_path) {
                Ok(_) => {
                    return Ok(Self {
                        socket_path,
                        next_id: 0,
                        skipped_worktrees: 0,
                    })
                }
                Err(err) => {
                    last = Some(err);
                    if attempt + 1 < CONNECT_ATTEMPTS {
                        std::thread::sleep(RETRY_BACKOFF);
                    }
                }
            }
        }
        Err(last.unwrap_or_else(|| "could not reach herdr".into()))
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
        // The same argument one level down. `workspaces` is a required field of
        // `SessionSnapshot`, so its absence is a protocol break, not an idle
        // session — and treating it as an empty array would hide the break
        // exactly the way reading the arrays off `result` used to.
        if snapshot.get("workspaces").is_none() {
            return Err(format!(
                "session.snapshot carried no `workspaces` array (snapshot keys: {})",
                key_list(snapshot)
            )
            .into());
        }
        if !snapshot
            .get("workspaces")
            .is_some_and(serde_json::Value::is_array)
        {
            return Err("session.snapshot `workspaces` was not an array".into());
        }
        let (checkouts, skipped) = reduce_snapshot(snapshot);
        self.skipped_worktrees = skipped;
        Ok(checkouts)
    }

    /// Workspaces the last `checkouts` call dropped because their `worktree`
    /// object was there but unreadable — a `worktree` that is not an object at
    /// all, or a workspace missing its `workspace_id`, `repo_key` or
    /// `checkout_path`. Zero is the normal case; anything else means the plugin
    /// is quietly seeing fewer repos than the session has, which is worth a
    /// note rather than silence.
    pub fn skipped_worktrees(&self) -> usize {
        self.skipped_worktrees
    }

    /// Asks herdr to re-read `config.toml`.
    ///
    /// The reply is **not** `{"type":"ok"}`. Captured from a live 0.8.0 server:
    ///
    /// ```text
    /// {"type":"config_reload","status":"applied","diagnostics":[]}
    /// {"type":"config_reload","status":"partial","diagnostics":["invalid theme config: …"]}
    /// {"type":"config_reload","status":"failed","diagnostics":["config parse error: …"]}
    /// ```
    ///
    /// Only `applied` means the file took effect, so anything else is an error
    /// carrying the diagnostics. Note that herdr does not validate sidebar
    /// token *names*: a row naming a token nobody sets still reloads as
    /// `applied`, so this proves the file parsed, not that the badge will show.
    pub fn reload_config(&mut self) -> Result<()> {
        let result = self.call("server.reload_config", json!({}))?;
        let status = text(&result, "status").unwrap_or("missing");
        if status == "applied" {
            return Ok(());
        }
        let diagnostics: Vec<String> = array(&result, "diagnostics")
            .iter()
            .map(|d| d.as_str().unwrap_or("").trim().to_string())
            .filter(|d| !d.is_empty())
            .collect();
        let detail = if diagnostics.is_empty() {
            "no diagnostics were reported".to_string()
        } else {
            diagnostics.join("; ")
        };
        Err(format!("herdr reported config reload status `{status}`: {detail}").into())
    }

    /// Shows a desktop notification and distinguishes an accepted request from
    /// a toast the user actually saw.
    ///
    /// Herdr 0.8.0 returns `notification_show`, not `ok`, and its required
    /// `reason` is the delivery verdict. The `shown` flag is checked too so a
    /// contradictory response cannot silently lose an alert.
    pub fn show_notification(&mut self, title: &str, body: &str) -> Result<NotificationDelivery> {
        let result = self.call(
            "notification.show",
            json!({
                "title": title,
                "body": body,
            }),
        )?;
        if text(&result, "type") != Some("notification_show") {
            return Err(format!(
                "notification.show returned result type `{}`",
                text(&result, "type").unwrap_or("missing")
            )
            .into());
        }
        let shown = result
            .get("shown")
            .and_then(Value::as_bool)
            .ok_or("notification.show returned no boolean `shown`")?;
        let reason = text(&result, "reason").ok_or("notification.show returned no `reason`")?;
        match (shown, reason) {
            (true, "shown") => Ok(NotificationDelivery::Shown),
            (false, "disabled") => Ok(NotificationDelivery::Disabled),
            (false, "rate_limited") => Ok(NotificationDelivery::Transient(
                NotificationTransient::RateLimited,
            )),
            (false, "no_foreground_client") => Ok(NotificationDelivery::Transient(
                NotificationTransient::NoForegroundClient,
            )),
            (false, "busy") => Ok(NotificationDelivery::Transient(NotificationTransient::Busy)),
            _ => Err(format!(
                "notification.show returned contradictory or unknown delivery \
                 (`shown`: {shown}, `reason`: `{reason}`)"
            )
            .into()),
        }
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

    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = format!("collide:{}", self.next_id);
        match self.call_once(&id, method, &params) {
            Ok(result) => Ok(result),
            Err(Failure::Protocol(err)) => Err(Box::new(err)),
            // Every attempt dials its own connection — the server answers one
            // request and closes, so there is nothing to reuse and nothing to
            // keep open. A transport failure is therefore a real failure rather
            // than the protocol's normal end-of-message, and the one thing worth
            // retrying it for is a `herdr update --handoff`, where the first
            // attempt lands on a socket the old server has just unlinked. That
            // only works with the pause below: the new server needs a moment to
            // bind, and two attempts fired back to back were measured 0.05 ms
            // apart.
            Err(Failure::Transport(first)) => {
                std::thread::sleep(RETRY_BACKOFF);
                match self.call_once(&id, method, &params) {
                    Ok(result) => Ok(result),
                    Err(Failure::Protocol(err)) => Err(Box::new(err)),
                    Err(Failure::Transport(second)) => {
                        Err(format!("{method} failed twice: {first}; on retry: {second}").into())
                    }
                }
            }
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

/// Where herdr's socket lives.
///
/// The fallback goes through `config::xdg_dir` rather than reading
/// `XDG_CONFIG_HOME` itself. It used to do the latter, and the two then disagreed
/// about the same variable: `xdg_dir` ignores a *relative* `XDG_CONFIG_HOME`, as
/// the spec requires, while this read it and resolved it against the process
/// cwd — which for a plugin command is the plugin root. `--setup` would edit the
/// right `config.toml` and then dial a socket somewhere else entirely, and since
/// a reload that does not succeed rolls the edit back, `--setup` could never
/// succeed at all.
fn socket_path() -> PathBuf {
    // herdr injects this into everything it spawns; the fallback exists only
    // for hand invocation from a shell.
    if let Some(path) = config::non_empty_env("HERDR_SOCKET_PATH") {
        return PathBuf::from(path);
    }
    config::xdg_dir("XDG_CONFIG_HOME", ".config")
        .join("herdr")
        .join("herdr.sock")
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

/// The keys an object carries, for an error that says what arrived instead of
/// what was expected.
fn key_list(value: &Value) -> String {
    match value.as_object() {
        Some(object) if !object.is_empty() => object.keys().cloned().collect::<Vec<_>>().join(", "),
        Some(_) => "none".to_string(),
        None => "not an object".to_string(),
    }
}

/// Reduces a `session.snapshot` result to the git-backed workspaces. The flat
/// sibling arrays are joined on `workspace_id`.
/// Drops `.` components from a path herdr reports.
///
/// herdr echoes back whatever path a worktree was created with, so a workspace
/// made with `--cwd .` arrives as `/home/you/repos/app/.` and would be rendered
/// that way in the detail pane. Purely cosmetic — the path still resolves — but
/// the pane is something people look at.
fn tidy_path(raw: &str) -> PathBuf {
    let path = Path::new(raw);
    let tidied: PathBuf = path
        .components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .collect();
    if tidied.as_os_str().is_empty() {
        path.to_path_buf()
    } else {
        tidied
    }
}

/// Reduces the snapshot to git-backed checkouts, and counts the workspaces that
/// carried a `worktree` object we could not read. A workspace with no
/// `worktree` at all is data, not a problem, and is not counted.
fn reduce_snapshot(snapshot: &Value) -> (Vec<Checkout>, usize) {
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
    let mut skipped = 0usize;
    for workspace in array(snapshot, "workspaces") {
        // No `worktree` key means the workspace is not a repo. That is data,
        // not an error: most sessions have at least one such workspace. A
        // `worktree` that is present but not an object is a protocol break, and
        // is counted. (The schema permits an explicit `null` here, which the
        // live server does not currently send; `null` is "not a repo", not a
        // break.)
        let Some(worktree) = workspace.get("worktree") else {
            continue;
        };
        if worktree.is_null() {
            continue;
        }
        let Some(worktree) = Some(worktree).filter(|w| w.is_object()) else {
            skipped += 1;
            continue;
        };
        let (Some(workspace_id), Some(repo_key), Some(checkout_path)) = (
            text(workspace, "workspace_id"),
            text(worktree, "repo_key"),
            text(worktree, "checkout_path"),
        ) else {
            // A repo we can see but cannot address — no `workspace_id` on the
            // workspace, or no `repo_key`/`checkout_path` on the worktree.
            // Silently dropping it makes the session look smaller than it is,
            // which is the failure this count exists to surface.
            skipped += 1;
            continue;
        };
        let repo_root = text(worktree, "repo_root").unwrap_or(checkout_path);
        checkouts.push(Checkout {
            workspace_id: workspace_id.to_string(),
            workspace_label: text(workspace, "label").unwrap_or(workspace_id).to_string(),
            repo_key: RepoKey(repo_key.to_string()),
            repo_root: tidy_path(repo_root),
            checkout_path: tidy_path(checkout_path),
            is_linked_worktree: worktree
                .get("is_linked_worktree")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            // Branch names are absent from the workspace object. `collide`
            // fills them in later by asking git directly
            // (`git::current_branch`), not by calling `worktree.list` — this
            // client never calls that method at all.
            branch: None,
            agent: agents
                .iter()
                .find(|(id, _)| id == workspace_id)
                .map(|(_, name)| name.clone()),
        });
    }
    (checkouts, skipped)
}
