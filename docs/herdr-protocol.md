# herdr socket protocol notes (verified against herdr 0.8.0, protocol 19)

Working notes for this plugin's socket client. Everything here was verified
against a live herdr 0.8.0 server and the bundled schema (`herdr api schema
--json`), not inferred from documentation.

An adversarial review found five claims in an earlier version of this file that
were wrong, and each of them had a matching bug in the code. They are corrected
below and marked **"this document used to say"** where they appear, rather than
quietly edited, because a note that has been wrong once is worth re-checking:

- the retry that carries a call across a handoff needs a **pause**, and has to
  cover dialling as well as calling;
- `worktree.list` is not called by this plugin at all, and its
  `not_git_worktree` error is about the **cwd**, not about a workspace with no
  `worktree` object;
- an unwritable state dir must **fail** `--enable`, not warn;
- bounding the `--disable` wait is not enough without escalating to `SIGKILL`;
- `agents[].agent_session` is an object and can never serve as a display name.

## Transport

Both fall back to `$XDG_CONFIG_HOME` or `$HOME/.config` when nothing is
injected, and the two must resolve it the same way: a *relative*
`XDG_CONFIG_HOME` is ignored per the spec. They disagreed once — the socket path
read the variable directly and honoured a relative value, resolving it against
the process cwd, which for a plugin command is the plugin root. `--setup` then
edited the right `config.toml` and dialled a socket somewhere else, and because a
reload that does not succeed rolls the edit back, `--setup` could never succeed
at all.

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

   **The retry needs a pause, and this document used to imply otherwise.**
   Measured back to back, two attempts fired without one were 0.05 ms apart —
   one attempt, as far as a rebind is concerned. A handoff unlinks the socket
   and binds a new one; a retry that lands inside that window fails for the same
   reason the first attempt did. The client now waits 150 ms first.

   The retry also has to cover *dialling*, not just a call on an open path.
   `connect` used to dial exactly once, so `--disable`'s sweep, `--once` and the
   daemon's shutdown clear all failed outright if they started during a handoff.
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

`workspace.worktree` (absent entirely for non-git workspaces — confirmed on a
live session where seven of ten workspaces had no such key at all):

```
repo_key           the .git path — canonical same-repo identity
repo_name          the repo's directory name; we do not read it
repo_root          main checkout root
checkout_path      this workspace's checkout
is_linked_worktree bool
```

The schema types the field `anyOf [WorkspaceWorktreeInfo, null]`, so an explicit
`null` is legal even though the server does not currently send one. It means the
same thing as an absent key: not a repo. A `worktree` that is present but is
*neither* an object nor null, or one missing `repo_key`/`checkout_path`, is a
protocol break; the client counts those and the daemon reports the count, because
dropping them silently makes the session look smaller than it is.

`tokens` on a workspace/pane is a **readback of what plugins have set**, which
makes it useful for verifying our own writes in tests.

`workspaces`, `panes`, `agents`, `tabs` and `layouts` are all **required** by the
schema. An absent array is therefore a protocol break, not an idle session, and
the client fails loudly on one rather than returning an empty list — the
distinction that the original `result` vs `result.snapshot` bug turned on, one
level further down.

Note: `snapshot.protocol` and `snapshot.version` let us gate behaviour without
shelling out to `herdr --version`.

### `worktree.list` — params `{workspace_id?, cwd?}`

**This plugin does not call it.** An earlier version of this document said
branch names come from here and that "a later pass" fills them in; the code has
always asked git instead (`git::current_branch`, via `collide::gather_for`),
which is both cheaper and correct for a detached HEAD. Kept here only because
the shape is easy to get wrong if someone reaches for it later.

Returns `{source: WorktreeSourceInfo, worktrees: [WorktreeInfo]}`. Branch names
are absent from the workspace object, so this is where they would come from.
Match on `worktrees[].path == workspace.worktree.checkout_path`; never infer a
branch from the directory name (verified mismatched in a live session).

**The error case is about the cwd, not about the workspace.** This document used
to say the method "returns an error envelope for a non-git workspace", meaning a
workspace with no `worktree` object in the snapshot. That is wrong, and it is
wrong in the direction that would have made a caller trust an empty result.
Verified live against 0.8.0:

```
worktree.list {"workspace_id":"w1B"}   -> {"type":"worktree_list", ...}
    w1B has NO `worktree` object in the snapshot, but its cwd is inside a repo,
    so the call succeeds and lists that repo's worktrees.

worktree.list {"workspace_id":"wM"}    -> error
    {"code":"not_git_worktree",
     "message":"Herdr worktree actions require a workspace inside a Git work tree"}

worktree.list {"cwd":"/tmp"}           -> error
    {"code":"not_git_worktree",
     "message":"Herdr worktree actions require a path inside a Git work tree"}
```

So `not_git_worktree` means "this path is not in a work tree", and it is data
("not a repo"), never a transport failure — treating it as one would burn
strikes toward the failure-shutdown threshold.

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
collide_overlap  collide_conflict  collide_runaway
```

A clean workspace sets no token at all: `render::badge` returns an empty string
and the daemon treats that as "clear" rather than as an empty badge, so there is
no `collide_clean` row for the user to configure. The disable sweep still clears
that name defensively, which costs nothing.

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

Documented for completeness; **this plugin does not call it**. The client used
to carry a `notify` helper that only a test exercised, which is how a path rots.
It is four lines if a future version wants it back. The reply is not `ok`:

```
{"type":"notification_show","shown":true,"reason":"shown"}
```

`reason` is one of `shown | disabled | rate_limited | no_foreground_client |
busy`, so "the call succeeded" and "the toast appeared" are different questions.

### `server.reload_config` — params `{}`

What `--setup` calls after editing `config.toml`. **The reply is not
`{"type":"ok"}`,** and the difference matters: a reload can succeed as a request
while failing as a reload.

```
{"type":"config_reload","status":"applied","diagnostics":[]}
{"type":"config_reload","status":"partial","diagnostics":["invalid theme config: invalid type: integer `42`, expected a string\nin `name`\n; keeping current theme settings"]}
{"type":"config_reload","status":"failed","diagnostics":["config parse error: TOML parse error at line 1, column 19 …\n; keeping current config"]}
```

All three were captured live from a 0.8.0 server driven against a scratch config
file. `status` is `applied | partial | failed`; only `applied` means the file
took effect. Treat the other two as failures and print the `diagnostics`.

Two things this does **not** tell you:

- **herdr does not validate sidebar token names.** A row naming a token nobody
  sets reloads as `applied` with no diagnostics. So `applied` proves the file
  parsed, not that a badge will appear.
- Shelling out to `herdr server reload-config` cannot express any of this: the
  CLI prints the payload and its exit status is about the request. Call the
  method over the socket instead — which is also the only option that works from
  a plugin command, see *Plugin execution environment*.

## Nothing renders until the user edits config.toml

herdr's default sidebar rows name none of our tokens. The README must ship the
snippet, and `--setup` splices the same rows in.

```toml
[ui.sidebar.spaces]
rows = [
  ["state_icon", "workspace"],
  ["branch",
    { token = "$collide_overlap",  fg = "#FFC799" },
    { token = "$collide_runaway",  fg = "#FFB27F" },
    { token = "$collide_unknown",  fg = "#9399B2" },
    { token = "$collide_conflict", fg = "#FF8080" }],
]
```

The splice is line-oriented, and the shapes that break a line-oriented splice
are worth naming, because every one of them fails *silently* if it is not
handled:

- **Brackets in comments.** `# ["retired", "row"],` inside the array counts as a
  row if you only count `[` and `]`. The entries then land inside the comment.
  The file still parses, herdr still reloads it, and nothing renders.
- **Brackets in strings.** `["a ] literal"]` does the same thing.
- **A one-line array.** On `rows = [["a"], ["b"]]` the last `]` on the line closes
  the *array*, not the last row. Splicing there puts the token tables beside the
  rows rather than inside one, which is again valid TOML that renders nothing.
- **Nothing to splice into.** A `[ui.sidebar.spaces]` with no `rows` array, or an
  inline-table spelling of the section, cannot be handled by this approach at
  all. That has to be reported as a failure — "there was nothing to do" and "I
  could not do it" must never read the same.
- **The next table, of either spelling.** The walk for the `rows` array must stop
  at `[[array.of.tables]]` as well as at `[table]`. Stopping only at the latter
  meant that a `[ui.sidebar.spaces]` with no rows of its own sent the walk on
  into the following `[[keys.command]]` block, where it found *that* table's
  `rows` key and spliced four token tables into a keybinding. Valid TOML, reloads
  as `applied`, renders nothing — and reported as "added 4 sidebar rows". A row
  begins with `[` too, so the test is "`[` followed by a bare key", not "`[`".
- **Which section counts as configured.** "Is this token already here?" has to be
  asked of `[ui.sidebar.spaces]`, not of the whole file. A token named in
  `[ui.sidebar.agents]` cannot render in the spaces sidebar, and treating it as
  configured made the splice omit it from the section it was building — nothing
  rendered, the run reported success, and a second run said "already configured".
  With no section at all, nothing is configured yet, whatever the rest of the
  file says.
- **Where a new token goes.** On an upgrade, add it to the row that already holds
  one of this plugin's tokens, falling back to the last row only on a fresh
  install. Always targeting the last row split collide's badges across two
  sidebar rows the user never asked to have split.


## Daemon lifecycle

`[[startup]]` hooks run on **both** a fresh server start and a live handoff, so
one `--restore` verb covers both. A daemon herdr spawned as a child would die
with herdr; `--enable` re-execs the binary as `--daemon` detached via `setsid()`
in `pre_exec` (not a double fork) so it survives.

State lives in `HERDR_PLUGIN_STATE_DIR`:

- `updater.pid` — is a daemon live right now
- `enabled` — did the user ever ask for one
- `updater.lock` — held across the check-and-spawn
- `updater.log` — the detached daemon's stdout and stderr

The pid check must guard against **pid reuse** (the state dir outlives reboots)
by comparing `/proc/<pid>/comm` against our own on Linux, degrading to a
liveness probe elsewhere.

**An unwritable state dir is fatal to `--enable`, not a warning.** This document
used to say the marker writes were best-effort and must not fail the user's
action. That was wrong in a way that showed up immediately: with the state dir
read-only, every `--enable` found no pid file, concluded no daemon was running,
and spawned another one. Two invocations gave two daemons, none of them
recorded, and `--disable` could not stop any of them because there was nothing
to read. A daemon nobody can stop is worse than no daemon, and a permission
problem is something the user can fix once they are told.

**The lock is not optional either.** `--enable` is check-then-act, and a
keypress landing beside a `--restore` startup hook during a handoff is enough to
run two of them at once: both see no live pid, both spawn, and the second
`write_pid` overwrites the first, orphaning a daemon forever. `--enable`,
`--restore` and `--disable` all take an exclusive `flock` on `updater.lock`
around the part that decides whether a daemon exists.

Verbs:

| verb | behaviour |
|---|---|
| `--enable` | take the lock, mark enabled, no-op if a live pid exists, else spawn detached |
| `--disable` | take the lock, mark disabled **first**, request stop, **await exit, escalating to `SIGKILL`**, clear the marker, release the lock, then sweep every current workspace over a fresh connection |
| `--toggle` | disable if live, else enable |
| `--restore` | silent no-op unless the enabled marker is set and no daemon is live |

Awaiting exit on `--disable` is load-bearing: the stop request only *posts*, and
the pid file survives until the daemon finishes clearing. An `--enable` landing
in that window sees a live pid, spawns nothing, and the badge never returns.

Bounding that wait is **not** enough on its own, which is what this document
used to say. When the bound expired the old code warned and carried on — and
`clear_pid_file` then refused to delete a marker naming a live process, so the
verb exited 0 with the daemon still running and its pid still on file, and the
next `--enable` did nothing. A 3 s bound was also observed expiring on a loaded
machine with a perfectly healthy daemon. The bound is now 5 s and is a step on
the way to `SIGKILL` rather than a verdict; if even `SIGKILL` does not take, the
verb fails. Killing a slow daemon is safe because the sweep that follows clears
every token on every workspace anyway.

The signal thread must clear state over **its own connection**, so it never
waits on the main loop's sleep or in-flight round trip, and the main loop must
park rather than return so it cannot re-report into the race.

### The daemon's own diagnostics

`--enable` re-execs the binary, so **herdr never sees this process** and
`herdr plugin log list` will never show a line from it. Handing the child
`Stdio::null()` therefore does not mean "the output goes somewhere useful", it
means the output is destroyed: a refresh that fails every cycle, a push herdr
rejects, a shutdown that could not reach the socket — all of it, silently. A
badge that never appears with nothing in any log is the worst outcome this
plugin has.

The child's stdout and stderr go to `updater.log` in the state dir, truncated at
spawn and capped at 1 MiB (the daemon truncates its own stderr when it grows
past that, and does nothing at all when stderr is a terminal, so a foreground
`--daemon` still prints where the user pointed it).

### What is lit, and what a failed push means

The daemon tracks one token name per workspace. That record may only be dropped
when herdr **confirms** a clear, or when the workspace is gone
(`workspace_not_found`). Rebuilding it from the successful sets alone loses the
record of a token herdr is still rendering under its TTL, and the next severity
flip then emits no clear for it — two collide tokens lit on one workspace, which
is the exact failure the one-token-per-workspace design exists to prevent.

## Plugin execution environment

Commands are argv arrays run with **no shell**, cwd = plugin root, and a minimal
`PATH` — `git` must be resolved explicitly rather than assumed. Plugins run on
the **server** host, so any tool we shell out to must exist there.

That PATH is `/usr/local/bin:/bin:/usr/bin` (read out of the 0.8.0 binary), and
**herdr itself installs to `~/.local/bin`, which is not on it**:

```
$ env -i PATH=/usr/local/bin:/bin:/usr/bin sh -c 'command -v herdr'; echo $?
127
```

So `Command::new("herdr")` from a plugin command does not resolve. The setup
action used to reload the config that way and reported the resulting `ENOENT` as
"herdr rejected the updated config", which sends the reader to inspect a file
that was never the problem. Anything herdr can do over the socket should go over
the socket; `HERDR_SOCKET_PATH` is injected into every command herdr spawns.

The variables herdr injects into a plugin command are `HERDR_PLUGIN_ROOT`,
`HERDR_PLUGIN_CONFIG_DIR`, `HERDR_PLUGIN_STATE_DIR`, `HERDR_SOCKET_PATH`,
`HERDR_PLUGIN_ENTRYPOINT_ID` and `HERDR_PLUGIN_CONTEXT_JSON`, plus the
action/event ones. **`HERDR_PLUGIN_ID` is not among them** — the string does not
appear anywhere in the 0.8.0 binary — so the plugin id used to name the state
directory is always our own constant, and that constant has to keep matching the
directory herdr creates (`~/.local/state/herdr/plugins/moneycaringcoder.collide`).

`herdr plugin link .` does **not** run `[[build]]`; `herdr plugin install` does.
Build manually during local development.

Logs are in-server only (`herdr plugin log list`), with no log file on disk —
**and only for processes herdr spawned**. The badge updater re-execs itself, so
it is not one of them; see *The daemon's own diagnostics* above.

`plugin action invoke` resolves its context from the **focused workspace** and
has no workspace selector, so any per-workspace aiming happens inside the
plugin.

## Gaps this document does not answer

Found while implementing against it. Each is a decision we made rather than a
fact we verified, so revisit them if behaviour looks wrong:

- **Token batching.** The 16-keys-per-report limit implies a multi-token patch,
  but nothing states whether one report may clear several tokens at once, or mix
  a set and a clear — and if it could, it is unclear what `ttl_ms` would apply
  to. We send one token per call, so the disable sweep costs **five** round trips
  per workspace instead of one (`ALL_TOKENS` gained `collide_unknown`; this
  document said four).
- **A clear that carries a `ttl_ms`.** We assert above that it is rejected, and
  we omit the field. The schema does not encode the rule — `ttl_ms` is simply
  nullable — and confirming it needs a live write, which no test here performs.
  The client omits it either way, so nothing depends on the answer.
- **How a stop is requested.** The lifecycle contract says "request stop" without
  naming a mechanism. We use `SIGTERM`, then `SIGKILL` if that is ignored.
- **Empty versus missing.** herdr injects empty strings for absent environment
  context. We assume the same for snapshot string fields and treat empty as
  absent.
- **Agent join priority.** No precedence is documented. This document used to
  say we prefer `agents[].name`, then `agents[].agent_session`, then
  `panes[].agent` — but `agent_session` is an **object** on the wire
  (`{"agent":"claude","kind":"id","source":"herdr:claude","value":"…"}`) and can
  never serve as a display name. The real order is `agents[].name`, then
  `agents[].agent` (the program), then `panes[].agent`.

  Also undocumented: when a workspace has several agent rows, which one names
  it. We take the first in `agents[]` order, which is the server's order, not a
  choice of ours.
- **Response `id` echo.** Documented but not validated here — one request per
  connection makes a mismatch impossible to act on. Note that an
  `invalid_request` error comes back with `id: ""` rather than the id sent, so a
  client that did validate it would have to special-case that.

## Open risk

`repo_key` equality across linked worktrees is observed, not documented —
consistent across five worktrees of one repo in a live session. Confirm for
submodules and `--separate-git-dir` layouts, and fall back to
`git rev-parse --path-format=absolute --git-common-dir` when it looks wrong.
