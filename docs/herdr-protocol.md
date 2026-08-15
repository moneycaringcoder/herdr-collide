# herdr socket protocol notes (verified against herdr 0.8.0, protocol 19)

Working notes for this plugin's socket client. Everything here was verified
against a live herdr 0.8.0 server and the bundled schema (`herdr api schema
--json`), not inferred from documentation.

## Transport

`HERDR_SOCKET_PATH` is injected into every command herdr spawns (build hooks,
startup hooks, actions, panes). Fall back to `$XDG_CONFIG_HOME/herdr/herdr.sock`
(macOS/Linux) only for hand invocation. Treat an empty-string env var as unset.

Framing is **newline-delimited JSON**. Not length-prefixed. There is no
`jsonrpc` field.

```
request : {"id":"<string>","method":"<name>","params":{...}}\n
success : {"id":"<string>","result":{"type":"<snake_case>",...}}\n
failure : {"id":"<string>","error":{"code":"<string>","message":"<string>"}}\n
```

- `id` must be a **string**.
- `params` is **mandatory and must be an object** — send `{}` for methods that
  take no parameters, never `null`.
- Every mutation returns `{"type":"ok"}`.

### The socket is one request per connection

Verified live: after a single response the server sends EOF and closes.
Pipelining two requests into one write answers the first, then `ECONNRESET`; a
second write on a connection that still looks open fails with `EPIPE`.

Consequences:

1. Every call must be able to reconnect and retry once. This is the hot path,
   not an edge case, and it is also what carries the client across a
   `herdr update --handoff` (the server's socket is re-created, the first call
   after the handoff fails, the retry lands on the new server).
2. `session.snapshot` is strictly better than `workspace.list` + N ×
   `pane.list`: one connection instead of N+1, and no tearing when a workspace
   closes mid-enumeration.

The one exception is `events.subscribe`, which **does** hold the connection
open and streams `{"event": <kind>, "data": {...}}` envelopes with no `id`.

## Methods this plugin uses

### `session.snapshot` — params `{}`

Returns flat sibling arrays joined by ID, plus `version` and `protocol`:

```
snapshot.workspaces[]  workspace_id, number, label, focused, pane_count,
                       tab_count, active_tab_id, agent_status,
                       tokens?, worktree?
snapshot.panes[]       pane_id, terminal_id, workspace_id, tab_id, focused,
                       agent_status, revision, cwd?, agent?, tokens?
snapshot.agents[]      pane_id, tab_id, workspace_id, agent_session, name?
snapshot.tabs[] snapshot.layouts[]
```

`workspace.worktree` (absent entirely for non-git workspaces):

```
repo_key           the .git path — canonical same-repo identity
repo_root          main checkout root
checkout_path      this workspace's checkout
is_linked_worktree bool
```

`tokens` on a workspace/pane is a **readback of what plugins have set**, which
makes it useful for verifying our own writes in tests.

Note: `snapshot.protocol` and `snapshot.version` let us gate behaviour without
shelling out to `herdr --version`.

### `worktree.list` — params `{workspace_id?, cwd?}`

Returns `{source: WorktreeSourceInfo, worktrees: [WorktreeInfo]}`. Needed only
for **branch names**, which are absent from the workspace object. Match on
`worktrees[].path == workspace.worktree.checkout_path`; never infer a branch
from the directory name (verified mismatched in a live session).

**This method returns an error envelope for a non-git workspace.** Treat that as
data ("not a repo"), never as a transport failure — otherwise it burns strikes
toward the failure-shutdown threshold.

### `workspace.report_metadata` — the badge (default surface)

Required: `workspace_id`, `source`, `tokens`. Tokens-only — no title, no
state labels, no display agent.

```json
{"id":"collide:7","method":"workspace.report_metadata","params":{
  "workspace_id":"w6","source":"moneycaringcoder.collide",
  "tokens":{"collide_conflict":"✘ 2"},"ttl_ms":15000}}
```

Clearing: send the token name with a `null` value, and **omit `ttl_ms`**.

Semantics:

- `tokens` is a **merge patch**. Omitted names are untouched, `null` deletes.
  Max 16 keys per report; herdr stores at most 32 per target. Names match
  `^[A-Za-z0-9_-]{1,32}$` — **no `$` on the wire**. The `$` prefix exists only
  in herdr's `config.toml` row syntax.
- `ttl_ms` is 1..86_400_000 and is what makes the badge self-heal when the
  daemon is killed. Derive it as ~3× the refresh interval so one missed cycle
  does not blink the badge out, and clamp the *cadence* rather than the TTL.
- `source` is our plugin id, namespacing ownership so we never clobber another
  plugin's tokens.
- `seq` is optional and costs a tracked "sequenced source" slot per target.
  Omit it — we are a single-writer daemon. If it is ever added, it must be
  monotonic forever after.

Errors: `invalid_metadata_request`, `invalid_metadata_token`,
`invalid_metadata_ttl`, `invalid_metadata_source`, `workspace_not_found`.
**Push errors are easy to swallow silently**, which renders as a blank badge
with nothing to debug — log them.

### Colour by token name

herdr renders a token's value as flat text and cannot colour by content. So
severity is encoded in the **token name**: light exactly one, clear the others.

```
collide_clean  collide_overlap  collide_conflict  collide_runaway
```

Each gets its own `fg` in the user's config. Track which name is currently
active per workspace so a severity flip clears the previous name first —
otherwise two badges light at once.

### `pane.report_agent` / `pane.release_agent` — opt-in mode only

Required: `pane_id`, `source`, `agent`, `state` where state is one of
`idle|working|blocked|unknown`. Plugins cannot write `done`.

This claims a *spare* pane (one whose `agent` is null/empty) as a pseudo-agent
row. Never claim a pane running a real agent. Two costs make this non-default:

1. A reported agent gives its space an **agent-status mark** in the sidebar,
   session navigator, and mobile view — the space reads as a live idle agent.
2. **A reported agent has no TTL.** Only tokens expire, so a SIGKILLed daemon
   leaves an empty row behind until the next enable/disable/restart sweep.

### `notification.show` — params `{title, body?, sound?, position?}`

## Nothing renders until the user edits config.toml

herdr's default sidebar rows name none of our tokens. The README must ship the
snippet and tell the user to run `herdr server reload-config` (rows reload live;
no restart needed).

```toml
[ui.sidebar.spaces]
rows = [
  ["state_icon", "workspace"],
  ["branch",
    { token = "$collide_clean",    fg = "#a6e3a1" },
    { token = "$collide_overlap",  fg = "#f9e2af" },
    { token = "$collide_conflict", fg = "#f38ba8" },
    { token = "$collide_runaway",  fg = "#fab387" }],
]
```

## Daemon lifecycle

`[[startup]]` hooks run on **both** a fresh server start and a live handoff, so
one `--restore` verb covers both. A daemon herdr spawned as a child would die
with herdr; `--enable` re-execs the binary as `--daemon` detached via `setsid()`
in `pre_exec` (not a double fork) so it survives.

State lives in `HERDR_PLUGIN_STATE_DIR`:

- `updater.pid` — is a daemon live right now
- `enabled` — did the user ever ask for one

Both are needed, and both writes are best-effort: an unwritable state dir must
not fail the user's action. The pid check must also guard against **pid reuse**
(the state dir outlives reboots) by comparing `/proc/<pid>/comm` against our own
on Linux, degrading to a liveness probe elsewhere.

Verbs:

| verb | behaviour |
|---|---|
| `--enable` | mark enabled **first**, no-op if a live pid exists, else spawn detached |
| `--disable` | mark disabled **first**, request stop, **await exit**, then sweep every current workspace over a fresh connection |
| `--toggle` | disable if live, else enable |
| `--restore` | silent no-op unless the enabled marker is set and no daemon is live |

Awaiting exit on `--disable` is load-bearing: the stop request only *posts*, and
the pid file survives until the daemon finishes clearing. An `--enable` landing
in that window sees a live pid, spawns nothing, and the badge never returns.
Bound the wait (~3s) so disable can never hang.

The signal thread must clear state over **its own connection**, so it never
waits on the main loop's sleep or in-flight round trip, and the main loop must
park rather than return so it cannot re-report into the race.

## Plugin execution environment

Commands are argv arrays run with **no shell**, cwd = plugin root, and a minimal
`PATH` — `git` must be resolved explicitly rather than assumed. Plugins run on
the **server** host, so any tool we shell out to must exist there.

`herdr plugin link .` does **not** run `[[build]]`; `herdr plugin install` does.
Build manually during local development.

Logs are in-server only (`herdr plugin log list`), with no log file on disk.

`plugin action invoke` resolves its context from the **focused workspace** and
has no workspace selector, so any per-workspace aiming happens inside the
plugin.

## Gaps this document does not answer

Found while implementing against it. Each is a decision we made rather than a
fact we verified, so revisit them if behaviour looks wrong:

- **Token batching.** The 16-keys-per-report limit implies a multi-token patch,
  but nothing states whether one report may clear several tokens at once, or mix
  a set and a clear — and if it could, it is unclear what `ttl_ms` would apply
  to. We send one token per call, so the disable sweep costs four round trips
  per workspace instead of one.
- **How a stop is requested.** The lifecycle contract says "request stop" without
  naming a mechanism. We use `SIGTERM`, matching the signal-thread language.
- **Empty versus missing.** herdr injects empty strings for absent environment
  context. We assume the same for snapshot string fields and treat empty as
  absent.
- **Agent join priority.** When `agents[].name`, `agents[].agent_session` and
  `panes[].agent` disagree, no precedence is documented. We prefer them in that
  order.
- **Response `id` echo.** Documented but not validated here — one request per
  connection makes a mismatch impossible to act on.

## Open risk

`repo_key` equality across linked worktrees is observed, not documented —
consistent across five worktrees of one repo in a live session. Confirm for
submodules and `--separate-git-dir` layouts, and fall back to
`git rev-parse --path-format=absolute --git-common-dir` when it looks wrong.
