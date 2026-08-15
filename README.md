# collide

A [herdr](https://github.com/moneycaringcoder/herdr) plugin that warns you when two agents are about to
step on each other.

Running several coding agents at once usually means several git worktrees of the same repository, one
per herdr workspace. That works right up until two of them start editing the same file, and you find
out at merge time. `collide` watches every workspace that is backed by a git checkout, groups them by
repository, and tells you — while the work is still in flight — which sibling worktrees are touching
the same files, and whether those edits will merely overlap or will genuinely conflict on merge. It
also flags a *runaway* worktree whose change set has grown past a threshold you set, which is usually
the first visible sign that an agent has wandered off.

It never writes to your repositories. Every git invocation is read-only, passes `--no-optional-locks`
so it cannot contend with an agent's own git commands, and stages nothing through your real index.

## What it looks like

In the sidebar, each workspace picks up a short badge next to its branch name:

```
  api      feature/api    ✘ 2
  ui       feature/ui     ✘ 2
  docs     docs/readme    ⧉ 1
  spike    spike/parser   ⚠ 4.1k
  notes    notes/inbox
```

`✘ 2` means two files are predicted to conflict and `⧉ 1` means one file is shared but merges cleanly.
`⚠ 4.1k` is a runaway: 4100 changed lines in one worktree, which is a count of the change set rather
than of shared files — a runaway agent is usually one that shares nothing with anybody. A clean
workspace shows nothing at all. Numbers abbreviate once they get long — `1.2k`, `12k`, `1.2M` — so a
badge never grows wide enough to push the branch name off the row.

The full picture lives in the **Collide: shared files** overlay pane, which redraws on an interval:

```
collide · shared files

repo /home/you/repos/app
  api [feature/api] @claude  ✘ 2
      degraded: a merge is in progress — this side is snapshotted with its
      conflict markers still in place, so any prediction involving it is
      advisory.
  salvage [no branch] (no agent)
      degraded: `wip/salvage` has no commits yet — unborn branch, so this
      checkout has no commit and is not paired with its siblings.
  ui [feature/ui] @codex  ✘ 2

  api <-> ui
    advisory: a merge is in progress in api, so these verdicts were computed
    from a tree that still contains conflict markers.
    ✘ conflict  src/collide.rs
    ✘ conflict  src/git.rs
    ? unknown   …e-core/src/analysis/pairing/heuristics/very_long_module_name.rs
    ⧉ overlap   src/model.rs

legend
  ✘  conflict predicted on merge
  ⧉  same file, merges clean
  ?  conflict prediction unavailable
```

Conflicts sort above overlaps, long paths are trimmed from the left so the informative tail survives,
a checkout that could only be read in part says which part and why, and the view reflows down to very
narrow panes.

## Install

```sh
herdr plugin install moneycaringcoder/herdr-collide
```

Installing runs the plugin's build step for you, so you end up with a compiled
`target/release/collide` and nothing further to do.

To develop against a local checkout instead:

```sh
git clone https://github.com/moneycaringcoder/herdr-collide
cd herdr-collide
cargo build --release          # required: `link` does NOT run the build step
herdr plugin link .
```

`herdr plugin link` deliberately skips the `[[build]]` hook, so the binary every command in
`herdr-plugin.toml` points at will not exist until you build it yourself. Rebuild by hand after every
change.

Removal:

```sh
herdr plugin unlink moneycaringcoder.collide
```

Logs are kept in the server rather than on disk:

```sh
herdr plugin log list --plugin moneycaringcoder.collide
```

## Required: add the tokens to your herdr config

**Nothing renders in the sidebar until you do this.** herdr's default sidebar rows do not name any of
this plugin's tokens, so a freshly installed `collide` will happily compute everything and display
none of it.

The quickest route is the bundled action — run **Collide: set up sidebar (start here)**. It splices
the rows below into your `config.toml`, takes a `config.toml.collide-backup` alongside it first, and
reloads herdr; if the reload fails it puts the backup back byte for byte. **Collide: undo sidebar
setup** restores that backup.

To do it by hand, add the three tokens to `[ui.sidebar.spaces]` in `~/.config/herdr/config.toml`:

```toml
[ui.sidebar.spaces]
rows = [
  ["state_icon", "workspace"],
  ["branch",
    { token = "$collide_overlap",  fg = "#FFC799" },
    { token = "$collide_runaway",  fg = "#FFB27F" },
    { token = "$collide_conflict", fg = "#FF8080" }],
]
```

Then reload:

```sh
herdr server reload-config
```

Sidebar rows reload live — no restart, and no losing your panes.

### Why there are three tokens instead of one

herdr renders a token's *value* as flat text and cannot colour it by content. A single
`$collide_status` token could say `✘ 2`, but it could never say it in red. So severity is encoded in
the token *name*: the plugin lights exactly one of `collide_overlap`, `collide_runaway`, or
`collide_conflict` at a time and clears the other two, and each name carries its own `fg` in your
config. The `$` prefix belongs to herdr's config row syntax only; the names sent over the wire have
no `$`.

There is deliberately no token for a clean workspace. A workspace with nothing to report clears its
badge instead of writing one, so its sidebar cell is empty by design — an empty cell means "no
collisions", not "the plugin is broken".

Change the colours to taste. The names must stay exactly as written, and all three should be present —
if you leave one out, workspaces at that severity simply show nothing.

## Actions and panes

| Action | What it does |
| --- | --- |
| **Collide: set up sidebar (start here)** | Adds the tokens above to `config.toml`, backs it up, reloads herdr |
| **Collide: undo sidebar setup** | Restores the backup that setup took |
| **Collide: report** | One-shot collision report for the focused repo |
| **Collide: JSON snapshot** | The same data, machine-readable, for scripting |
| **Collide: enable badge updater** | Starts the background updater that pushes badges |
| **Collide: disable badge updater** | Stops it and clears every badge this plugin set |
| **Collide: toggle badge updater** | Whichever of the two applies |

There is one pane, **Collide: shared files**, placed as an overlay. It runs the live detail view shown
above and refreshes on the configured interval. Close it the way you close any herdr overlay; it exits
cleanly on `SIGINT` and `SIGTERM` and restores the cursor on the way out.

The badge updater is off until you enable it. Once enabled it survives a herdr restart and a
`herdr update --handoff`: a startup hook re-spawns it, but only if you had it enabled when herdr went
away. Disabling it stops the updater, waits for it to finish, and then sweeps every current workspace
so no stale badge is left behind.

Everything is also available from the command line, which is handy when the plugin is misbehaving:

```
collide --once      # one-shot report
collide --json      # the same report as JSON
collide --watch     # the live detail view
collide --enable | --disable | --toggle
collide --setup | --setup-rollback
collide --interval 10 --watch
collide --base-ref origin/main --once
collide --help
```

Options may come before or after the verb, so `collide --base-ref main --once` and
`collide --once --base-ref main` are the same command.

## Configuration

Configuration is a JSON file at `$HERDR_PLUGIN_CONFIG_DIR/config.json`. herdr injects that directory
when it runs the plugin; when you run the binary yourself it resolves to the same place herdr would
use, `~/.config/herdr/plugins/config/moneycaringcoder.collide/config.json`, so both routes read one
file. The daemon's own state lives alongside it under `~/.local/state/herdr/plugins/`, which is why
`collide --disable` typed at a shell stops the updater a plugin action started. Every key is optional and overrides just that default, and unknown keys are
ignored, so a config written for a newer version will not break an older binary. A missing file is the
normal case; a malformed one prints a warning and falls back to the defaults rather than taking the
badge down.

```json
{
  "interval_seconds": 5,
  "runaway_files": 40,
  "runaway_lines": 2000,
  "ignore_suffixes": [
    "Cargo.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "poetry.lock",
    "go.sum"
  ],
  "predict_conflicts": true,
  "base_ref": "origin/HEAD",
  "git_timeout_seconds": 10
}
```

- **`interval_seconds`** — how often the badge updater and the detail pane refresh. Default 5, clamped
  to 1–3600. `--interval <SECS>` overrides it for a single run.
- **`runaway_files`** / **`runaway_lines`** — a workspace is flagged as a runaway once its change set
  passes either threshold. Defaults 40 files and 2000 changed lines. The badge reports the changed-line
  count, so `runaway_lines` is also the number the `⚠` badge is measured against.
- **`ignore_suffixes`** — paths ending in any of these never count as a change. Lockfiles overlap
  constantly and mean nothing, so they are excluded by default. Setting the key replaces the whole
  list rather than adding to it.
- **`predict_conflicts`** — set to `false` to report shared paths only. Cheaper, and it stops
  distinguishing a real conflict from a plain overlap.
- **`base_ref`** — the ref each checkout's change set is measured against, as the `<base>` in
  `git diff <base>...HEAD`. Default `origin/HEAD`; where that does not resolve, `collide` falls back to
  the first of `origin/main`, `origin/master`, `main`, `master`, `trunk` that exists, and finally to
  `HEAD` — which still reports the checkout's dirty state, and only loses the committed-since-base
  half. `--base-ref <REF>` overrides it for a single run.
- **`git_timeout_seconds`** — cap on any single git invocation, so one slow repository cannot stall
  the refresh loop.

## How it works

Each cycle takes a single `session.snapshot` over herdr's socket — one round trip for every workspace,
pane, and agent at once, so nothing tears while a workspace is closing. Workspaces with no worktree
information are not repositories and are dropped. For the rest, repository identity is re-derived from
git itself rather than trusted from the snapshot — the canonicalized common git directory — and
checkouts are grouped by that, so two of them are only ever compared when they are genuinely the same
repository.

For each checkout, `collide` reads a change set — staged, unstaged, untracked, conflicted, and
committed since the merge base — with read-only git plumbing. Pairs of checkouts within a repository
are then intersected: a pair with no files in common cannot conflict and is dropped for free. Survivors
go through conflict prediction in two phases, a cheap boolean pass over every remaining pair and an
expensive pass that recovers paths only for the pairs the first pass flagged.

The result is one badge per workspace, pushed with a TTL of roughly three refresh intervals. That TTL
is what makes the display self-healing: if the updater is killed, herdr expires the badges on its own
within a cycle or two rather than leaving a stale warning on screen forever.

## Limitations

Worth knowing before you trust it:

- **It polls.** herdr exposes no filesystem events, so changes are noticed on the refresh interval and
  not before. A five-second badge is a five-second-old badge.
- **Conflict prediction is a prediction.** It merges the two sides' current state in a temporary index
  and reports what git says *now*. Commit, rebase, or keep typing and the answer can change. A worktree
  with a merge already in progress is trickier still: its snapshot is staged from files that still
  contain conflict markers. The detail pane labels those pairings `advisory:` and names the side
  responsible, rather than presenting the verdict as if the trees were clean.
- **Lockfiles are ignored by default.** `Cargo.lock`, `package-lock.json`, and friends overlap in
  almost every pair of worktrees and almost never mean anything. If you want them counted, override
  `ignore_suffixes`.
- **Non-UTF-8 paths are rendered lossily.** Git reports raw bytes; anything that is not valid UTF-8 is
  replaced before display, so such a path may render differently from how it appears on disk.
- **A checkout can be readable only in part.** An unborn branch, a branch deleted underneath a
  worktree, a base ref that does not resolve, or two histories with no common ancestor all limit what
  can be compared. Rather than quietly reporting such a checkout as clean, the detail pane marks it
  `degraded:` and states which of those it was and what the consequence is — excluded from pairing, or
  counted on uncommitted work only. A pair whose prediction could not run is shown as `? unknown`
  instead of being downgraded to a plain overlap.
- **Linux and macOS only.** The daemon relies on Unix process and signal behaviour, and the plugin
  declares those two platforms.
- **Repository identity across linked worktrees is observed rather than specified.** It holds for
  ordinary `git worktree` layouts; submodules and `--separate-git-dir` setups are less well tested.

## Contributing

Bug reports, questions, documentation fixes and code are all welcome — see
[CONTRIBUTING.md](CONTRIBUTING.md) for how to build it, what makes a change
easy to merge, and what is deliberately out of scope. The project is maintained
by one person, so review is careful rather than instant.

The one rule worth knowing up front: collide is strictly read-only against your
repositories, and `tests/read_only.rs` enforces that by fingerprinting every
index, ref and object before and after a run.

Security issues go through [private reporting](SECURITY.md) rather than public
issues.

## Licence

MIT. See [LICENSE](LICENSE).
