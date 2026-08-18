# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Tag-triggered release automation. Pushing `vX.Y.Z` runs the full suite on
  Linux and macOS and publishes the GitHub release with notes taken from that
  version's changelog section — but only after an identity gate has confirmed
  that the tag, `Cargo.toml`, `Cargo.lock` and `herdr-plugin.toml` all name the
  same version and that the changelog section for it exists and is not empty.
  The manifest version is the one the marketplace displays and the one easiest
  to forget, so it is checked explicitly.
- An advisory upstream canary. Once a day it resolves one exact herdr `master`
  commit, fetches the API schema herdr generates from its own types at that
  revision, and checks that the three methods collide calls, the parameters it
  sends, and the workspace and worktree fields it reads are all still there. It
  is scheduled and manual only, it is not a required check, and a red canary is
  a signal to read herdr's recent changes rather than a reason to hold a pull
  request.
- Each checkout is now predicted against the integration ref as well as against its
  siblings. Two worktrees can be mutually clean and both conflict with `main`, and that
  case was invisible: every prediction was pairwise. The detail pane names the ref and
  reports `clean`, `conflict` or `unknown` per worktree, and `--json` carries
  `target_ref`, `target_verdict`, `target_reason`, `target_approximate` and
  `target_advisory`. Adding keys is compatible, so the schema version does not move.
- A target verdict qualifies itself the way a pairwise one does. If the histories offer
  more than one merge base, one is forced and the pane says the verdict approximates
  what a real merge would do; if the worktree has a merge in progress, its snapshot
  contains conflict markers and the verdict is advisory. Both were computed and then
  discarded in the first pass, so a criss-cross history read as a firm answer.
- The target verdict deliberately does not touch the badge. The badge has one slot and
  four severities already competing for it, and a fifth source of `✘` that fires on
  every stale branch would mute the signal that matters. The pane and `--json` carry it
  until there is real-session evidence for doing more.
- Nothing is fetched. The ref is read from the local ref store, exactly as the existing
  probe chain does, so a stale `origin/main` gives an answer about where `main` *was* —
  which is why the pane names the ref rather than refreshing it. A fetch would also
  write to the object store and refs, which this plugin may not do.
- `ignore_globs`, a second and additive path-ignore list beside `ignore_suffixes`, for
  the generated directory, vendored tree or build output path that no extension covers.
  `*` matches within one path component, `**` may cross `/` without absorbing the
  separator beside it, and a trailing `/` selects a directory tree. Patterns are
  anchored at the repository root, so `vendor/**` does not match `my-vendor/a`, and
  `**.gen.rs` is the spelling that reaches every depth. The default is empty, so
  nothing changes for an existing installation, and setting the key replaces the whole
  list as `ignore_suffixes` already does.
- A matching path is dropped from all four places a path can count: the pairing
  intersection, the runaway file count, the runaway line count, and the admission of a
  conflicting path the predictor named but no change set listed. Filtering only the
  first is how an ignored lockfile previously came back as a runaway badge, so each of
  the four has its own test.
- The `--json` schema is now a documented promise rather than an implementation
  detail. `README.md` records the `schema` key, its current value, the full key
  inventory, both enum domains, and the rule for when the number moves: adding a
  key or an array element is compatible, while removing or renaming a key,
  changing a value's type, or adding a `severity` or `verdict` value is not.
  Array order is explicitly outside the contract.
- A schema test that fails when the shape moves without the version. The existing
  tests would notice a renamed or removed key by way of the value they assert; an
  *added* key passed silently, and the enum domains were unpinned entirely. The
  new test compares the exact key set at every level and maps every `Severity` and
  `FileVerdict` variant through an exhaustive match with no wildcard, so a new
  variant stops compiling rather than quietly widening the output.

### Changed

- Pairings are ranked worst first in `--json` and in the one-shot text report, not
  only in the detail pane. The ranking rule now lives on `Pairing` and is used by
  every consumer, so a script and a person are told the same thing about which
  collision to deal with first — previously the pane sorted and the report vector
  did not, which left `--json` in checkout-arrival order. Ranking is by conflicting
  file count, then unknown count, then overlap count, all descending; unknowns rank
  above overlaps for the same reason the `unknown` severity outranks `overlap`.
  Array order was already outside the `--json` contract, so the schema version does
  not move.

### Fixed

- A dirty submodule is no longer reported as a harmless overlap. `status`
  reports it as one changed path, but the snapshot records the submodule's
  committed gitlink rather than its contents, so `merge-tree` compared two
  identical gitlinks and found nothing — and the pair was reported `⧉ overlap`,
  whose legend reads *"same file, merges clean"*. It now reports `? unknown`,
  and the detail pane says the contents were never compared. Contents are still
  not compared; that remains a separate, larger change.
- A submodule whose *recorded commit* changed keeps its real verdict. Those two
  gitlinks are genuinely comparable, so answering "I do not know" there would
  discard a fact the plugin already has. Only modified or untracked content
  inside the submodule is uncomparable, and a path git flags as conflicting is
  still a conflict whatever its contents are doing.
- The runaway badge carries two different units and the legend named only one of
  them. `⚠ 4.2k` is a changed-line count and `⚠ 60f` is a file count, but the
  legend read `runaway change set (f = files)`, which explains the suffix and
  leaves the bare number — the common case — unexplained. It now reads
  `runaway change set (lines, or f = files)`.
- Legend lines are wrapped rather than truncated, so a narrow pane no longer cuts
  the end off an explanation. Truncating an explanation removes the explanation,
  which is the rule the notes already followed and the legend did not.
- The README's sidebar sample explained an overlap badge it never showed. The
  sample now includes the overlap row its own prose describes.

### Tests

- Fixtures for the two repository layouts that were previously only assumed to
  work: `--separate-git-dir`, and a superproject with a submodule. Repository
  identity across linked worktrees was an observation rather than a guarantee,
  and these pin it: every worktree of one repository resolves to the same
  canonicalized common directory, a submodule keeps a repository identity of its
  own and is never paired with its superproject, and the resolved working-tree
  roots agree with `git rev-parse --show-toplevel` in both layouts.
- The read-only fingerprint now covers a submodule's own repository — its index
  bytes, refs, reflogs and object paths — and not just the superproject's. A
  submodule is a second place a write could land, and it was outside the
  assertion that the plugin changes nothing.

### Known issues

- The fixtures found the case the layout work existed to look for: rule 1 of the
  repository-root agreement joins `.git` to a worktree's top level and compares
  the canonicalized result against the repository key, but under
  `--separate-git-dir` that `.git` is a gitfile naming the store rather than the
  store itself, so the exact rule never fires and the answer falls through to the
  deterministic member fallback. The reported root is still correct for the
  layouts tested, and it is correct by fallback rather than by the rule that was
  written to decide it. The fix is deliberately not in this change.

## [0.1.0] - 2026-08-16

First release.

### Added

- Per-workspace collision detection across every git worktree herdr has open,
  grouped by repository. Two checkouts are only ever compared when their
  canonicalized `--git-common-dir` matches, so unrelated repositories are never
  paired.
- Real conflict prediction, not just shared-path detection. A file both sides
  touched is reported as an overlap when it merges cleanly and as a conflict
  only when git says it will actually conflict, including for uncommitted work
  on both sides.
- Runaway detection for a worktree whose change set has grown past a
  configurable threshold, which is usually the first visible sign that an agent
  has wandered off.
- A sidebar badge per workspace, pushed with a TTL so it clears itself if the
  updater dies, and a **Collide: shared files** overlay pane with the full
  picture.
- A setup action that adds the required tokens to `config.toml`, backs the file
  up first, reloads herdr, and restores the backup automatically if that reload
  fails. An undo action restores it on demand.
- Command-line access to everything the actions do, for when the plugin is
  misbehaving.

### Notes

The plugin is strictly read-only against your repositories. Every git
invocation passes `--no-optional-locks`, nothing is ever staged through a real
index, and object writes are redirected away from your object store. A test
asserts all of that by hashing the index, working tree, refs and object count
before and after a full run, including while another process holds
`index.lock`.

Two behaviours of git and herdr are worth knowing about, and both are recorded
in `docs/`:

- `git merge-tree --write-tree --quiet` reports clean for merges that genuinely
  conflict, so this plugin does not use it. It stops at the first directory both
  sides modified and, because merge-ort walks paths in reverse-sorted order,
  loses any conflict on a path sorting before that directory.
- herdr's socket answers exactly one request per connection, so every call
  reconnects; and nothing renders in the sidebar until the user's `config.toml`
  names the plugin's tokens.

### Hardening pass before the first release

An adversarial pass over the whole plugin, looking for one kind of bug: a wrong
answer with no error, indistinguishable from a right one. Everything below was
found by looking for that shape deliberately, and every fix carries a test that
was checked to fail without it.

#### Added

- **A fourth severity, `unknown`, with its own sidebar token.** When conflict
  prediction cannot run, the affected files are reported as `? unknown` and the
  workspace badges `?`. Previously they were folded into the overlap badge,
  whose legend reads *"same file, merges clean"* — so a prediction that failed
  looked exactly like a prediction that succeeded and found nothing wrong.
  **Run the setup action again to add the new row**; it adds only what your
  config is missing and tells you what it added.
- `unknown_count` and `changed_files` on each status in `--json`, plus
  `has_rename` and `approximate` on the objects that gained a meaning for them.
  The JSON schema version is now `2`, because `severity` gained a value.

#### Fixed

- A checkout whose git pass failed was given an empty change set and reported as
  clean. It is now visibly degraded.
- When no conventional trunk could be found, the base ref silently became `HEAD`,
  which empties the committed half of every change set and reports no
  degradation — two agents colliding head-on read as two clean workspaces. The
  probe chain is wider, and when it still finds nothing it says so.
- A failed HEAD probe was reported as "unborn branch" or "branch deleted",
  excluding a healthy checkout from every comparison and telling the user its
  branch was gone.
- A git invocation that left a descendant holding its output pipe could park the
  refresh loop indefinitely, despite the timeout: a command finishing in 80 ms
  took over 40 seconds to return under a 2 second deadline. Children now run in
  their own process group and the output drain is bounded.
- A committed directory rename could conflict for real and be reported clean,
  because the pair was dropped before prediction ran.
- A pair of histories with no common ancestor answered "everything conflicts" or
  "cannot tell" depending on whether one side happened to have an untracked file.
  It is now refused consistently.
- Line volume ignored `ignore_suffixes`, so a single `npm install` could paint a
  runaway badge on a workspace whose whole diff the plugin had decided to ignore.
- Both halves of a rename counted toward `runaway_files`, halving the threshold.
- Two herdr workspaces open on one checkout — or one nested inside another — were
  compared against each other and reported a permanent overlap.
- `ignore_suffixes` matched anywhere in a path, so `go.sum` swallowed
  `tools/cargo.sum`.
- The badge updater sent every diagnostic to `/dev/null`; it now writes to a
  size-capped `updater.log` in its state directory.
- A rejected badge push made the updater forget which token was lit, leaving two
  badges on one workspace after the next severity change.
- `--enable` could spawn an unstoppable updater when the state directory was not
  writable, and two concurrent invocations produced two updaters with one
  recorded. Both are refused, and the check and spawn happen under a lock.
- `--disable` reported success while the updater was still running.
- Sidebar setup could splice its rows into a comment, or beside the rows rather
  than inside one — both valid TOML, both rendering nothing — and reported
  "nothing to do" when it had failed to place them at all.
- Setup's idempotency check was all-or-nothing, so a config written before a new
  token existed never gained it.
- The config file was accepted as a JSON array and applied positionally;
  `git_timeout_seconds` was not clamped; unknown keys vanished silently.
- Emoji and several wide characters were measured as one column, so pane lines
  could exceed their width budget and wrap.
- The badge was the last thing on a worktree line and so the first thing
  truncated — it disappeared from exactly the narrow panes where it matters most.
- A mistyped option was ignored rather than reported, and verbs were recognised
  by elimination, so the first boolean flag added to the binary would silently
  have become the verb.

#### Changed

- **The snapshot no longer runs your repository's content filters.** `git add`
  was executing whatever `filter.*.clean` or `filter.*.process` programs a
  repository configures, on every refresh — and for git-lfs that writes into your
  own `.git/lfs`, which a read-only tool may not do. Filtered paths are now
  compared as their raw bytes.
- Paths that are not valid UTF-8, or that contain control characters, are still
  replaced for display but now carry a short digest of the original bytes, so two
  different files can no longer render as one shared path.
- Pairings in the detail view sort worst first rather than alphabetically, and
  the notes section moved to the top of the pane, where a short pane cannot cut
  it off.

#### Documentation

- Both notes files are corrected where they were found to be wrong, with the
  wrong claims called out rather than quietly edited. Chief among them: the rule
  for telling an unborn branch from a deleted one by its reflog fails in *both*
  directions, and no discriminator exists, because the two are the same
  observable state.

[Unreleased]: https://github.com/moneycaringcoder/herdr-collide/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/moneycaringcoder/herdr-collide/releases/tag/v0.1.0
