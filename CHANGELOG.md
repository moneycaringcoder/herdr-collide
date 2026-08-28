# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- Installed report, JSON, `--why`, and detail-pane actions now consume
  `HERDR_PLUGIN_CONTEXT_JSON` instead of the plugin process cwd. The focused
  pane cwd is tried before the workspace cwd; each candidate must resolve
  through Git and match a readable Herdr checkout. The installed plugin root is
  never selected. Direct CLI invocations retain one process-cwd candidate, and
  the badge daemon remains session-wide.
- Herdr 0.8.2 checkout discovery no longer requires deprecated
  `session.snapshot.workspaces[*].worktree` metadata. Collide consumes
  explicitly named open siblings before querying another unresolved workspace.
  Pane-cwd and `source_checkout_path` fallbacks apply only to the queried
  workspace, preventing a parent response from absorbing a distinct nested
  repository.
- A scoped cycle timeout now fails clearly instead of widening the report to
  unrelated repositories or comparing Herdr-provided repository keys with
  Git-derived identity.

## [0.2.0] - 2026-08-28

### Added

- `cycle_timeout_seconds`, a 30-second default wall-clock budget for one
  repository-analysis refresh. The absolute deadline is inherited by checkout
  and prediction workers; an overrun produces visible `cycle-timeout` unknown
  statuses for every observed checkout instead of publishing partial clean
  output.
- Worktree/workspace lifecycle event hooks now wake an enabled updater through
  an internal `--refresh`/`SIGUSR1` path. Ordinary edits continue polling
  because herdr exposes no filesystem event.
- The `--watch`/`detail` surface is now an interactive ratatui pane. Keyboard
  and mouse navigation focus checkout and shared-file rows; Enter opens
  degraded checkout detail or the existing `--why` explanation from the exact
  retained prediction tree. Refreshes preserve cursor and open-hunk identity,
  and the new `open-detail` action provides a bindable pane front door.

### Changed

- Change-set collection now retains porcelain-v2 branch/HEAD facts, status
  entries, filter overrides, top levels, per-worktree Git directories, and
  integration OIDs for prediction. Outer snapshots and deduplicated direct
  submodule sides prime in bounded parallel workers instead of repeating probes
  and snapshotting sequentially.
- Refresh data now has one owner: `Report::changes`; `Cycle` no longer clones
  the same change sets. Filtered change data and compiled ignore patterns are
  likewise built once and reused when predictions are folded back into the
  report. The glob matcher compiles its deliberately small grammar once and
  matches without per-pattern dynamic-programming allocations.
- Configuration docs no longer duplicate `predict_conflicts`, the Herdr
  environment notes match the current `HERDR_BIN_PATH`/plugin context contract,
  and obsolete sequential-prime and ignored-`base_ref` claims are removed.
- Porcelain-v2 status and numstat parsing now live in `git::status`, separating
  byte-framing/path parsing from process, snapshot, and prediction plumbing.
  Public parser APIs remain re-exported from `git`.

### Fixed

- Repository identity, per-worktree and nested Git-directory discovery, and
  untracked-file line reads now stay behind bounded Git subprocesses rather
  than performing unbounded repository filesystem reads in the daemon process.
  Content filters remain neutralised for the new no-index line-count path.
- Raw Git status path bytes now remain attached to their safe display
  surrogates. NUL-delimited pathspec files and raw `OsString` arguments let
  snapshots and line counts address non-UTF-8/control-character filenames
  without exposing them to the terminal.
- Dirty direct-submodule file and line volume now contributes to runaway
  thresholds at depth one. A nested volume failure marks the outer change set
  partial rather than silently understating it.
- Repository-configured custom merge drivers are never executed. Pair and
  target predictions remain `unknown` when such a driver is configured, and
  fixtures now cover split index, untracked cache, fsmonitor, Git-managed
  relocation, case folding, and canonically equivalent Unicode filenames.
- An enabled daemon now detects when plugin installation replaces its
  executable, starts the binary at the new path with the forwarded overrides,
  and exits. Reinstallation no longer leaves old code running until a herdr
  restart. The obsolete warning that configured `base_ref` was ignored is also
  removed; that setting is already honored by change-set and target gathering.


## [0.1.3] - 2026-08-26

### Added

- `cargo bench --bench gather_cost`, a dependency-free manual benchmark for
  complete collision cycles over 2, 4, 8, and 16 dirty worktrees, a predicted
  conflict, and dirty direct-submodule contents. Every case asserts its verdict
  before timing and reports its worktree, pair, and sample counts; timings stay
  advisory rather than becoming a noisy shared-runner gate.

### Changed

- Rust 1.80 is now the declared and tested minimum toolchain while the existing
  Linux/macOS jobs continue to test current stable; `unicode-segmentation` is
  pinned to its Rust-1.80-compatible 1.12 release. Every third-party GitHub
  Action is pinned to the exact commit behind its reviewed major tag, checkout
  credentials are never persisted, and ordinary CI now declares read-only
  contents permission explicitly.
- Repository-identity and branch probes now fan out across at most eight
  checkout-verification workers, then restore Herdr snapshot order before
  analysis. The later `status` pass remains sequential; only the lock-free
  probes run concurrently, so sessions with many worktrees spend less wall time
  before change-set collection without adding index contention.
- Badge refreshes now send one atomic `workspace.report_metadata` patch per
  workspace, clearing every inactive collide token and setting the selected
  token together. Severity flips can no longer fail between a clear and a set
  and briefly render two badges; unchanged badges still refresh their TTL, and
  disable/shutdown sweeps now clear all owned names in one call per workspace.

### Fixed

- A scratch-only content-filter artifact can no longer become a conflict on a
  path only one agent changed. Conflict paths outside the initial intersection
  are now admitted only when both authoritative change sets list them or a
  rename explains the name mismatch. This contains the documented
  stat-dirty/content-identical filtered-file false positive while retaining
  exact rename conflicts and the no-filter read-only guarantee.
- Closing the live detail overlay with `SIGHUP` now follows the same
  signal-flag shutdown path as `SIGINT` and `SIGTERM`, so the process restores
  the hidden cursor before exiting. Cursor restoration is guarded on every
  ordinary return and by a panic hook because the release profile aborts
  without running destructors; a process-level regression sends a real SIGHUP
  and asserts the emitted terminal cleanup bytes.
- The daemon diagnostic integration test now waits for the failure text in its
  dedicated stderr file instead of killing the process as soon as the fake
  server records the second request. The server records before closing the
  socket, so the old synchronization could kill the daemon between receiving
  EOF and writing the diagnostic, intermittently failing slower macOS CI.
- Terminal width and truncation now use maintained Unicode width tables and
  extended grapheme boundaries instead of a hand-written scalar-range table.
  Skin-tone emoji, ZWJ families, regional-indicator flags, Hebrew points, Thai
  marks, and future Unicode table updates can no longer be split or
  under-counted into a line that wraps the redraw-in-place detail pane.
- Herdr-invoked reports, JSON snapshots, `--why`, and the live detail pane were
  scoped to the invoking workspace's repository. Current releases obtain that
  invocation scope from `HERDR_PLUGIN_CONTEXT_JSON`; direct shell invocations
  use their process cwd.
- Worktree-root discovery now reuses Git's timed `--show-toplevel` result from
  change-set collection instead of performing a second, unbounded
  `canonicalize` and ancestor walk in the daemon thread. A slow filesystem can
  therefore fail through the configured Git deadline and become a visible
  unreadable checkout rather than freezing every badge refresh.

## [0.1.2] - 2026-08-25

### Fixed

- The exact rule that identifies a repository's main worktree now resolves a
  `.git` gitfile instead of only canonicalizing the path, so it fires for
  `--separate-git-dir` and for a submodule whose git directory lives under its
  superproject. It could not fire for those layouts before, because a gitfile is
  a regular file naming the store rather than the store itself, and the root was
  decided by fallback — right by luck for the layouts under test, and wrong when
  the external store happens to be named `.git`: the parent-of-the-key rule then
  claimed the store's parent, a directory that is not the working tree. A linked
  worktree still cannot satisfy the rule, because its gitfile names
  `worktrees/<name>` and the rule deliberately does not follow `commondir`. The
  0.1.1 "Known issues" entry for this is now closed.
- A snapshot no longer trusts the copied index's stat cache for a path git
  itself reported as changed. The temporary index is still seeded from the real
  one — that is what keeps sparse-checkout and skip-worktree entries out of a
  one-sided deletion — but a same-size edit whose mtime did not move was
  invisible to `add -A`, so the prediction compared content the snapshot never
  saw and could report a real conflict as a clean overlap: the one failure
  direction this plugin treats as unacceptable. A status-reported path whose
  worktree entry is still a file is now re-read by a path-limited renormalizing
  add before the general one, which re-hashes exactly the paths the prediction
  depends on and leaves every other entry, and its index flags, untouched. A
  path status did not name cannot be affected, and neither a file that is gone
  nor one replaced by a directory can be hidden by a stat cache at all.

## [0.1.1] - 2026-08-18

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
- Optional conflict history, off by default (`conflict_history`), and two verbs to read and
  delete it: `collide --history` lists the paths that collide repeatedly, between which
  worktrees and how often, and `collide --history-clear` removes the record. Two agents
  colliding on the same file week after week is rarely a git problem and usually a design
  one, and this plugin is the only thing positioned to notice it.
- History records episodes rather than cycles: one record when a collision appears and one
  when it stops, because a five-second refresh would otherwise write some seventeen thousand
  lines a day for a single unresolved conflict. Only a real conflict opens an episode. An
  overlap is a known-clean merge and closes one; an *unknown* is an absent answer and does
  neither, so a predictor that fails for one cycle cannot split a single continuing collision
  into two. A daemon restart continues an open episode rather than starting another.
- An episode is identified by repository, path and the two workspace ids, never by branch name
  or workspace label. Both of those are stored as well, because a person reading the history
  needs them, but they are display text that changes when a workspace is renamed and identity
  must not move with them.
- Each record holds the repository key, the conflicting path, both workspace ids and labels,
  both branch names, when the collision was first seen, and — on the closing record — when it
  was last confirmed. `collide --history` counts episodes per path and pair, reports the last
  sighting, and says when an episode is still open.
- The record lives only in the plugin's own state directory, is created mode 0600, refuses to
  follow a symlink on read or write, and is capped at 1 MiB — trimmed to the newest complete
  records, and only after confirming by device and inode that the file being trimmed is the
  one the plugin owns. It contains paths and branch names, which is why it is opt-in and why
  deleting it takes one command.
- Optional desktop notifications when a workspace *becomes* conflicting, off by default
  (`notifications_enabled`). A conflict that has existed for ten minutes is not news; the
  edge is. Only a transition into `conflict` from a non-`conflict` severity notifies:
  transitions into `runaway`, `unknown` or `overlap` do not, because a runaway is
  near-permanent on a busy branch and an unknown is the absence of an answer, and neither
  is worth training someone to mute.
- Losing a conflict is never announced as resolving one. `conflict` → `unknown` means the
  answer was lost, so it neither notifies nor overwrites the last real answer — otherwise a
  single git timeout would re-announce an unchanged conflict as new news every minute for as
  long as the timeouts continued. `conflict` → `clean` does not interrupt anyone either.
  Neither does the first cycle after the daemon starts, which has no baseline at all.
- A notification distinguishes being sent from being seen. herdr answers with a reason, so
  `rate_limited`, `busy` and `no_foreground_client` leave the alert pending for the next
  cycle rather than recording a toast that never appeared, and a reply the client cannot
  trust is rejected rather than believed. A prediction that rests on a forced merge base
  says so in the notification, as it already does everywhere else.
- `collide --why <PATH>` shows the conflicting hunks behind an `✘ conflict` verdict. The
  verdict is a summary of a real merge that already ran in a temporary index, so the
  command reads the merged blob out of that prediction's own object store rather than
  merging again — a second merge would cost twice and could disagree with the verdict
  being asked about. It refuses honestly rather than printing an empty diff: an overlap
  says the two sides merge cleanly, a path no pair shares says so, and a prediction that
  could not run says that and exits non-zero.
- `--why` states what limits a verdict before showing it. A forced merge base says the
  hunks approximate what a real merge would do, and a merge in progress on either side
  says the snapshot already contains that worktree's own conflict markers — without which
  `--why` would attribute a half-finished merge to the other agent.
- File content shown by `--why` is sanitised before it reaches a terminal. Line and tab
  structure survives, every other control character is replaced, and the output is capped
  in lines and in display columns with the truncation announced. A filename could already
  have cleared a redraw-in-place pane, and a file's contents can carry the same payload.
  A blob above 8 MiB is refused as unknown rather than read into memory.
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
- A submodule's contents are now compared, so two agents editing the same file inside one
  submodule get a real verdict instead of `? unknown`. Each direct submodule is treated as
  the repository it is: its own common directory, its own index copied into scratch, its own
  HEAD and snapshot, and its own scratch object store whose alternate is the *submodule's*
  object store rather than the superproject's. Depth is one — a submodule's own submodules
  are not entered.
- Every failure still falls back to `? unknown` with the note explaining why: a submodule
  that is absent or uninitialised on either side, an unborn or unreadable nested HEAD, a
  nested merge already in progress, no common ancestor, or any timeout. The fallback is the
  point: a nested comparison that could not run must not report a clean merge. A nested
  merge that had to force one of several merge bases says so too, in its own wording,
  because the existing caveat describes the outer histories rather than the nested ones.
- A shared file's path stays superproject-relative. Nested conflicting paths are named in the
  pane note instead, because a path in that field is superproject-relative everywhere else
  and mixing the two scopes would corrupt every consumer of it.
- The read-only guarantee now covers the second repository a submodule introduces: the
  nested index, refs, reflogs and complete object path set are fingerprinted before and after
  a run that exercises nested prediction, for both worktrees. One dirty submodule shared by
  two worktrees — one pair — measured 616 ms per cycle. The nested snapshot is per worktree
  and the nested merge is per pair, so that figure does not scale by submodule count alone.
- A conflict git attributes to a rename now says so. `merge-tree` reports a
  machine-stable conflict type per message record, and the parser discarded which
  paths each type applied to, so the only evidence available was a flat per-pair set
  of tokens. The association is kept now, and a conflicting file that git named in a
  `rename/rename`, `rename/delete` or directory-rename record is marked `(rename)` in
  the detail pane. Narrow panes drop the annotation before the path, as they already
  drop the verdict word.

### Fixed

- A rename conflict git named exactly is no longer reported as approximate. An
  unlisted conflicting path used to be admitted on pair-level rename evidence and the
  whole pairing marked approximate, because nothing said which conflict a rename
  explained. With per-path attribution that is now known, so only an admission that
  rename evidence alone cannot attribute stays a guess. A forced merge base still
  marks the pairing approximate, as before.
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

- A dirty submodule is no longer reported as a harmless overlap merely because
  the snapshot records its committed gitlink rather than its contents.
  `merge-tree` used to compare two identical pointers, find nothing, and report
  `⧉ overlap`, whose legend reads *"same file, merges clean"*. The safety fix
  changed that unsupported conclusion to `? unknown`, preventing committed
  pointer equality from standing in for a contents verdict.
- A submodule whose *recorded commit* changed kept its real gitlink verdict in
  that safety fix. The guard was limited to dirty contents the gitlink snapshot
  could not represent; a path git flagged as conflicting remained a conflict
  regardless of what those unrepresented contents were doing.
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

[Unreleased]: https://github.com/moneycaringcoder/herdr-collide/compare/v0.1.3...HEAD
[0.1.3]: https://github.com/moneycaringcoder/herdr-collide/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/moneycaringcoder/herdr-collide/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/moneycaringcoder/herdr-collide/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/moneycaringcoder/herdr-collide/releases/tag/v0.1.0
