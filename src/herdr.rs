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

    /// Reduces the live session to readable Git-backed workspaces.
    ///
    /// Herdr 0.8.2 removed the deprecated `worktree` metadata from
    /// `session.snapshot` workspace summaries. For that version and newer,
    /// `worktree.list` is the public source for repository identity and checkout
    /// paths. Older snapshots are still understood so Collide remains usable
    /// during a Herdr upgrade.
    pub fn checkouts(&mut self) -> Result<Vec<Checkout>> {
        let result = self.call("session.snapshot", json!({}), RetrySafety::Idempotent)?;
        let snapshot = result.get("snapshot").ok_or_else(|| {
            format!(
                "session.snapshot returned no `snapshot` object (result type `{}`)",
                text(&result, "type").unwrap_or("missing")
            )
        })?;
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

        if !worktree_list_is_public_source(snapshot) {
            let (checkouts, skipped) = reduce_snapshot(snapshot);
            self.skipped_worktrees = skipped;
            return Ok(checkouts);
        }

        let agents = workspace_agents(snapshot);
        let mut pending: Vec<(usize, &Value)> =
            array(snapshot, "workspaces").iter().enumerate().collect();
        let mut checkouts = Vec::new();
        let mut skipped = 0usize;
        while let Some((_, workspace)) = pending.first().copied() {
            let Some(workspace_id) = text(workspace, "workspace_id") else {
                pending.remove(0);
                skipped += 1;
                continue;
            };
            let listed = match self.call(
                "worktree.list",
                json!({"workspace_id": workspace_id}),
                RetrySafety::Idempotent,
            ) {
                Ok(listed) => listed,
                Err(err)
                    if matches!(
                        error_code(&*err),
                        Some("not_git_worktree" | "workspace_not_found")
                    ) =>
                {
                    pending.remove(0);
                    continue;
                }
                Err(err) => return Err(err),
            };
            if text(&listed, "type") != Some("worktree_list") {
                return Err(format!(
                    "worktree.list for workspace `{workspace_id}` returned result type `{}`",
                    text(&listed, "type").unwrap_or("missing")
                )
                .into());
            }

            let mut mapped = Vec::new();
            for (position, (snapshot_index, candidate)) in pending.iter().enumerate() {
                if let Some(checkout) = reduce_worktree_list(snapshot, candidate, &agents, &listed)
                {
                    checkouts.push((*snapshot_index, checkout));
                    mapped.push(position);
                }
            }
            if mapped.is_empty() {
                pending.remove(0);
                skipped += 1;
                continue;
            }
            for position in mapped.into_iter().rev() {
                pending.remove(position);
            }
        }
        checkouts.sort_by_key(|(snapshot_index, _)| *snapshot_index);
        self.skipped_worktrees = skipped;
        Ok(checkouts
            .into_iter()
            .map(|(_, checkout)| checkout)
            .collect())
    }

    /// Workspaces the last [`checkouts`](Self::checkouts) call could not reduce
    /// from Herdr's checkout metadata. Zero is the normal case; anything else
    /// means the plugin is seeing fewer repositories than the session exposes.
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

/// Herdr 0.8.2 made `worktree.list` the public checkout source. Missing or
/// unparseable versions take the current path rather than trusting deprecated
/// workspace-summary fields.
fn worktree_list_is_public_source(snapshot: &Value) -> bool {
    text(snapshot, "version")
        .and_then(parse_version)
        .is_none_or(|version| version >= (0, 8, 2))
}

fn parse_version(raw: &str) -> Option<(u64, u64, u64)> {
    let core = raw.split_once('-').map_or(raw, |(core, _)| core);
    let mut parts = core.split('.');
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ))
}

/// Agent display names from the flat snapshot arrays, joined on workspace id.
fn workspace_agents(snapshot: &Value) -> Vec<(String, String)> {
    let mut agents = Vec::new();
    let mut record = |workspace_id: Option<&str>, name: Option<&str>| {
        if let (Some(workspace_id), Some(name)) = (workspace_id, name) {
            if !agents.iter().any(|(id, _)| id == workspace_id) {
                agents.push((workspace_id.to_string(), name.to_string()));
            }
        }
    };
    for agent in array(snapshot, "agents") {
        // `name` is the user's label ("gitsmith"); `agent` is the program
        // ("claude"). `agent_session` is an object, not a display name.
        record(
            text(agent, "workspace_id"),
            text(agent, "name").or_else(|| text(agent, "agent")),
        );
    }
    for pane in array(snapshot, "panes") {
        record(text(pane, "workspace_id"), text(pane, "agent"));
    }
    agents
}

/// Drops `.` components from a path Herdr reports.
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

/// Joins one 0.8.2 workspace summary to a `worktree.list` response. Public
/// `open_workspace_id` mappings win, followed by pane cwd containment. The
/// response's source checkout belongs only to its explicit source workspace.
fn reduce_worktree_list(
    snapshot: &Value,
    workspace: &Value,
    agents: &[(String, String)],
    listed: &Value,
) -> Option<Checkout> {
    let workspace_id = text(workspace, "workspace_id")?;
    let source = listed.get("source")?.as_object()?;
    let repo_key = source.get("repo_key")?.as_str()?.trim();
    let repo_root = source.get("repo_root")?.as_str()?.trim();
    if repo_key.is_empty() || repo_root.is_empty() {
        return None;
    }
    let worktrees = listed.get("worktrees")?.as_array()?;
    let mut panes: Vec<&Value> = array(snapshot, "panes")
        .iter()
        .filter(|pane| text(pane, "workspace_id") == Some(workspace_id))
        .collect();
    let active_tab_id = text(workspace, "active_tab_id");
    panes.sort_by_key(|pane| {
        if pane.get("focused").and_then(Value::as_bool) == Some(true) {
            0
        } else if text(pane, "tab_id") == active_tab_id {
            1
        } else {
            2
        }
    });
    let pane_cwds = panes.iter().flat_map(|pane| {
        [text(pane, "foreground_cwd"), text(pane, "cwd")]
            .into_iter()
            .flatten()
    });
    let fallback = if source
        .get("source_workspace_id")
        .and_then(Value::as_str)
        .map(str::trim)
        == Some(workspace_id)
    {
        source
            .get("source_checkout_path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|path| !path.is_empty())
    } else {
        None
    };
    let listed_workspace = worktrees
        .iter()
        .find(|worktree| text(worktree, "open_workspace_id") == Some(workspace_id))
        .and_then(|worktree| text(worktree, "path").map(|path| (tidy_path(path), worktree)));
    let (checkout_path, worktree) = listed_workspace.or_else(|| {
        pane_cwds
            .chain(fallback)
            .find_map(|cwd| containing_worktree(cwd, worktrees))
    })?;

    Some(Checkout {
        workspace_id: workspace_id.to_string(),
        workspace_label: text(workspace, "label").unwrap_or(workspace_id).to_string(),
        repo_key: RepoKey(repo_key.to_string()),
        repo_root: tidy_path(repo_root),
        checkout_path,
        is_linked_worktree: worktree
            .get("is_linked_worktree")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        branch: text(worktree, "branch").map(str::to_string),
        agent: agents
            .iter()
            .find(|(id, _)| id == workspace_id)
            .map(|(_, name)| name.clone()),
    })
}

fn containing_worktree<'a>(cwd: &str, worktrees: &'a [Value]) -> Option<(PathBuf, &'a Value)> {
    let cwd = tidy_path(cwd);
    worktrees
        .iter()
        .filter_map(|worktree| {
            let path = tidy_path(text(worktree, "path")?);
            cwd.starts_with(&path).then_some((path, worktree))
        })
        .max_by_key(|(path, _)| path.components().count())
}
/// Legacy reduction for Herdr versions before 0.8.2.
fn reduce_snapshot(snapshot: &Value) -> (Vec<Checkout>, usize) {
    let agents = workspace_agents(snapshot);
    let mut checkouts = Vec::new();
    let mut skipped = 0usize;
    for workspace in array(snapshot, "workspaces") {
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
            branch: None,
            agent: agents
                .iter()
                .find(|(id, _)| id == workspace_id)
                .map(|(_, name)| name.clone()),
        });
    }
    (checkouts, skipped)
}
