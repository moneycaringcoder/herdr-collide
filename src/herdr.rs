//! Thin, plugin-local wrapper around Crook's herdr client.
//!
//! This module keeps Collide's response validation and domain reduction while
//! Crook owns socket resolution and NDJSON transport.

use std::fmt;
use std::path::{Path, PathBuf};

use crook::client::{Client, Error as ClientError, RetrySafety};
use crook::env::PluginEnv;
use serde_json::{json, Value};

use crate::config;
use crate::model::{Checkout, RepoKey};
use crate::Result;

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

/// Crook-backed Herdr client with Collide's last snapshot-reduction state.
#[derive(Debug)]
pub struct Herdr {
    client: Client,
    skipped_worktrees: usize,
}

impl Herdr {
    pub fn connect() -> Result<Self> {
        let plugin_env = PluginEnv::resolve(config::PLUGIN_ID);
        let client = Client::connect(plugin_env.socket_path().to_path_buf(), "collide")
            .map_err(local_error)?;
        Ok(Self {
            client,
            skipped_worktrees: 0,
        })
    }

    /// One `session.snapshot` call, reduced to the git-backed workspaces.
    /// Workspaces with no `worktree` key are not repos and are skipped.
    pub fn checkouts(&mut self) -> Result<Vec<Checkout>> {
        let result = self.call("session.snapshot", json!({}), RetrySafety::Idempotent)?;
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
        let result = self.call("server.reload_config", json!({}), RetrySafety::Never)?;
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
            RetrySafety::Never,
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

    /// Atomically replaces this plugin's badge state for one workspace.
    ///
    /// `tokens` is one merge patch: `Some` sets a value and `None` clears it.
    /// A severity transition therefore clears every inactive collide token and
    /// lights the selected one in one server mutation, so a partial two-call
    /// failure can never render two badges. Herdr 0.8.0 applies `ttl_ms` only to
    /// values set by the patch; cleared names disappear immediately.
    pub fn patch_badges(
        &mut self,
        workspace_id: &str,
        tokens: &std::collections::BTreeMap<String, Option<String>>,
        ttl_ms: Option<u64>,
    ) -> Result<()> {
        let mut params = json!({
            "workspace_id": workspace_id,
            "source": config::plugin_id(),
            "tokens": tokens,
        });
        if let Some(ttl_ms) = ttl_ms {
            params["ttl_ms"] = Value::from(ttl_ms.clamp(MIN_TTL_MS, MAX_TTL_MS));
        }
        self.call("workspace.report_metadata", params, RetrySafety::Never)?;
        Ok(())
    }

    fn call(&self, method: &str, params: Value, retry_safety: RetrySafety) -> Result<Value> {
        self.client
            .request(method, params, retry_safety)
            .map_err(local_error)
    }
}

fn local_error(error: ClientError) -> Box<dyn std::error::Error> {
    match error {
        ClientError::Protocol { code, message } => Box::new(HerdrError { code, message }),
        error => Box::new(error),
    }
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
