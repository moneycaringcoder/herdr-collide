# herdr-collide — handoff

You are taking over a finished, published plugin. This document is the state of
play: what it does, how it is put together, what was decided and why, and what
is left. Read it before touching anything.

Repository: https://github.com/moneycaringcoder/herdr-collide — public, CI green
on Linux and macOS, **no `herdr-plugin` topic and no tag**. Adding either is the
human's decision, not yours.

## What it does

Watches every herdr workspace backed by a git checkout, groups them by
repository, and compares **every pair** of worktrees within a repository. For
each pair it reports the files both sides changed, and whether those changes
merely overlap or will genuinely conflict on merge. It also flags a *runaway*
worktree whose change set has grown past a threshold.

Output goes to a per-workspace sidebar badge and a detail pane. It is strictly
read-only against user repositories.

## Structure

```
src/
  main.rs      verb dispatch; options may precede or follow the verb
  lib.rs       the crate is lib+bin so integration tests can reach real modules
  model.rs     shared types — the contract every other module works against
  herdr.rs     socket client: newline-delimited JSON, one request per connection
  git.rs       change sets and conflict prediction; the read-only guarantee lives here
  collide.rs   analysis: grouping, pairing, severity; pure core plus impure gathering
  daemon.rs    detached badge updater, pid/enabled markers, TTL pushes, cleanup
  render.rs    badge text and the detail pane; sole author of badge strings
  config.rs    config file, CLI overrides, plugin directory resolution
  setup.rs     splices sidebar tokens into the user's config.toml, with rollback
tests/         8 suites, 130 tests; read_only.rs proves the no-write guarantee
docs/          herdr-protocol.md and git-plumbing.md — verified behaviour, not docs
```

Verbs: `--once`, `--json`, `--watch`, `--enable`, `--disable`, `--toggle`,
`--restore`, `--daemon`, `--setup`, `--setup-rollback`.

## Decisions worth knowing

- **Badges ride workspace metadata tokens, not a pseudo-agent row.** The
  pseudo-agent trick gives a space a fake agent-status mark everywhere in the UI
  and leaves a row behind if the daemon is killed, because reported agents have
  no TTL. Tokens expire on their own.
- **Severity rides the token *name*.** herdr renders a token's value as flat
  text and cannot colour by content, so exactly one of `collide_overlap`,
  `collide_runaway`, `collide_conflict` is lit at a time. There is deliberately
  no `collide_clean` token: a clean workspace clears its badge.
- **Nothing renders until the user's `config.toml` names the tokens**, which is
  why the setup action exists.
- **`render::badge` is the single author of badge text.** Two builders once
  wrote competing versions that disagreed about the clean case.
- **Analysis is split pure/impure** so the interesting logic is testable without
  herdr or git.

## Four bugs that a green test suite did not catch

Each was an invisible failure — a wrong answer with no error. This is the
failure mode to expect here:

1. `merge-tree --write-tree --quiet` reports clean for merges that genuinely
   conflict; it stops at the first directory both sides modified. Every conflict
   silently degraded to an overlap. Not used anywhere now — see
   `docs/git-plumbing.md`.
2. The socket client read snapshot arrays one level too high and reported "no
   git-backed workspaces" with two open. Its test fake had encoded the same
   wrong shape, so the suite agreed with the bug.
3. The setup action spliced tokens *beside* the sidebar rows instead of inside
   one. Valid TOML, accepted by herdr, rendered nothing.
4. State and config directories resolved to a temp dir when run by hand and to
   herdr's directory when run by an action, so a hand-run `--disable` could not
   stop a daemon an action had started.

## Current state

- 130 tests, `cargo clippy --all-targets --locked -- -D warnings` clean,
  `cargo fmt` clean, release builds. Toolchain 1.97.1, matching CI.
- Verified live: correctly separated conflict from overlap across four
  worktrees, and correctly reported clean worktrees as clean.
- Governance docs, issue forms, CODEOWNERS, dependabot, logo, two diagrams
  (`docs/diagrams/*.d2` rendered to `docs/img/`, plus inline Mermaid).
- The user's live herdr config contains collide's three sidebar tokens with a
  backup at `~/.config/herdr/config.toml.collide-backup`. Leave both alone
  unless you are testing the setup action, and restore exactly if you do.
- The plugin is linked locally (`herdr plugin link`), daemon currently stopped.

## What is left, in priority order

1. **Adversarial review.** Assume more invisible bugs exist. Useful lenses:
   correctness of the git layer, silent-failure paths where a fallback resembles
   a normal empty result, and tests whose fakes encode assumptions rather than
   captured behaviour. Judge findings before fixing — a confident wrong finding
   is worse than none.
2. **Untested real-world states.** Submodules, `--separate-git-dir`, non-UTF-8
   filenames, a workspace closing mid-cycle, herdr restarting under the daemon,
   `herdr update --handoff`, a config file that is not JSON, two daemons racing.
3. **Scale.** Measure a full cycle on a repository with thousands of files and
   tens of worktrees, and put the real number in the README instead of a guess.
4. **The pane is the product.** Verify the detail view at 40, 80, 120 and 200
   columns with long paths and several repositories. Replace the README's
   hand-written pane example with real captured output from a run you set up and
   then tear down.
5. **Install from scratch.** `herdr plugin install` from the public repo into a
   clean state, not just `herdr plugin link`.

## Rules

- Everything in `~/repos/ORCHESTRATION-BRIEF.md` applies.
- Tear down every worktree, workspace and daemon you create.
- No topic, no tag, no force-push, no `git branch -D`.
- Other sessions are working in `~/repos/herdr-shear` and `~/repos/herdr-redact`.
  Leave them alone.
