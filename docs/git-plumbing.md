# git plumbing notes (verified on git 2.53.0, Linux)

Working notes for `src/git.rs`. Every command here was verified against a
purpose-built fixture repo containing worktrees that genuinely conflict,
worktrees that touch the same file without conflicting, and every degenerate
worktree state. Timings are warm-cache on a 5000-file, 60 MB repo.

## Hard rules

1. Set `GIT_OPTIONAL_LOCKS=0` in the environment of **every** invocation. Plain
   `status` takes `<gitdir>/index.lock` to write back its stat cache — verified
   by watching the index mtime advance — and so does `diff` against the working
   tree. The `--no-optional-locks` flag is passed to `status` as well, but the
   environment variable is the mechanism that actually covers every command; an
   earlier version of this file credited the flag alone, which would have made
   dropping the variable look safe.
2. Never touch a worktree's real index. All staging goes through a temporary
   `GIT_INDEX_FILE`.
3. Resolve both sides to 40-hex OIDs before calling `merge-tree`. See the
   exit-code trap below.
4. Only compare worktrees whose canonicalized
   `git rev-parse --path-format=absolute --git-common-dir` is identical.
5. Give every child its own process group and kill the *group* on expiry. See
   "The deadline that was not one".
6. Neutralise content filters for anything that hashes working-tree bytes. See
   "Content filters".

Everything except `status` is lock-free and safely parallel. A 120-process
stress run produced zero errors and no leftover `index.lock`.

## The deadline that was not one

Killing a child on expiry does not bound the call. A pipe reaches EOF only when
**every** holder of its write end has closed it, so any process git leaves
behind that inherited git's stdout or stderr keeps `read_to_end` blocked long
after git itself is dead. Reproduced with a `core.fsmonitor` hook and again with
a `filter.<driver>.clean` that backgrounds a child:

```sh
# hook.sh — answers git immediately, leaves a holder behind on git's stderr
#!/bin/sh
sleep 90 >/dev/null &
printf '/\0'
```

Measured: `git add` completes in 80 ms; the same command through a naive
"kill the child, then join the reader threads" wrapper with a **2 s** deadline
was still blocked at 40 s and had to be killed from outside. In a daemon that is
not a wrong answer but a silent stop — the refresh loop parks, every badge
freezes at its last value, and nothing is written to `notes`.

Two defences, both required:

* `setsid` in `pre_exec`, so the child leads its own process group and
  `kill(-pid, SIGKILL)` reaches everything it spawned; and
* a bounded wait on the drain threads, so even a group that escaped (a holder
  that called `setsid` itself) costs one grace period rather than forever.

The group is killed **only** when the child overran or a pipe failed to drain,
never on the happy path: git legitimately starts background helpers
(`fsmonitor--daemon`, `maintenance run --auto`) and killing those would be
damage of our own.

A drain that does not complete must be reported as a failure, not as a short
answer. Undrained output is surfaced with no exit code at all, so no caller can
mistake a truncated read for a successful command.

## Content filters

`git add` and `git diff --numstat HEAD` both run the repository's
`filter.<driver>.clean` — or `.process`, which takes precedence over it — on the
working-tree bytes. `git write-tree` runs it too, because it refreshes the index
it is handed and re-hashes any stat-dirty entry. Measured on git 2.53.0:

| command | clean-filter invocations |
|---|---|
| `status --porcelain=v2 -z -uall --renames` | 0 |
| `diff --numstat -z HEAD` | 2 per changed file |
| `diff --name-only -z <base>...HEAD` | 0 |
| `diff --numstat -z <base>...HEAD` | 0 |
| `add -A --` | 2 per changed file |
| `write-tree` | 1 per stat-dirty entry |

Those filters are arbitrary user programs. The one everybody has is git-lfs,
whose clean filter writes into the user's own `.git/lfs/objects` every time it
runs, so a plugin that promises to change nothing cannot execute them once per
refresh cycle. Neutralise them per invocation:

```sh
git -c filter.<driver>.clean= \
    -c filter.<driver>.process= \
    -c filter.<driver>.required=false \
    add -A --
```

All three overrides are needed. Emptying `clean` alone leaves `.process` in
charge; emptying both while `required = true` — git-lfs's default — makes git
refuse outright with `fatal: <path>: clean filter '<driver>' failed`. Verified
each combination: with all three, the filter is not invoked and the blob is the
raw working-tree bytes.

Enumerate the drivers from the repository itself, since the set is per repo:

```sh
git config --name-only --get-regexp '^filter\.'   # exit 1 when there are none
```

Strip `filter.` and the trailing `.clean`/`.smudge`/`.process`/`.required`; a
driver name may itself contain dots, so strip from the right.

### What that costs, and how the false-conflict shape is contained

The snapshot tree then holds raw bytes for any filtered path `add` had to
re-hash — for LFS the media rather than a pointer — while anything the seeded
index still considers clean keeps its existing filtered blob. Prediction only
ever compares one snapshot tree against another or against a commit tree, so a
path both sides changed differently still differs and a path only one side
changed still merges cleanly.

Line counts for a filtered path then measure unfiltered bytes. For LFS that
changes nothing: the diff is binary either way and `--numstat` reports `-`, which
counts zero. For a text-transforming filter it overstates that path's volume,
which the runaway thresholds treat as an order-of-magnitude signal anyway.

A filtered path that is stat-dirty but content-identical is still re-hashed to
raw bytes and therefore looks modified inside the scratch tree against a base
holding the filtered blob. Demonstrated with an LFS-shaped filter and a `touch`:
`status` reports the worktree clean while the scratch tree differs from HEAD.

That artifact is contained at the report boundary. A conflict path absent from
the initial intersection is admitted only when **both** change sets list it, or
when a rename explains why the names differ. If only the sibling lists the
filtered path, the other side is unchanged by the authoritative status pass and
Git could take the sibling directly; the scratch-only conflict is discarded.
This closes the false alarm without retaining raw path bytes in the public
change-set model or running repository content filters.

## Repo identity

```sh
git -C <path> rev-parse --path-format=absolute --git-common-dir
```

All worktrees of one repo share this; each has its own `--git-dir`. Do not use
`--git-dir`, `--show-toplevel`, or the directory name. Canonicalize before
comparing, since symlinked or bind-mounted roots can yield two keys for one
repo. A bare repo and a clone of it correctly compare as different.

## Enumerating worktrees

```sh
git -C <worktree> worktree list --porcelain -z
```

Records separated by an empty NUL field. `worktree <abs-path>` is always first;
after that do not assume ordering. A `bare` record has no `HEAD` and no
`branch`. `HEAD 0000…0000` means unborn or dangling. `detached` replaces
`branch`. `locked`/`prunable` may carry an optional reason.

Skip `bare`, `prunable`, and all-zero `HEAD` records. `locked` and `detached`
are fine for read-only analysis.

## Change set for one worktree

Dirty paths:

```sh
git -C <wt> --no-optional-locks status --porcelain=v2 -z --untracked-files=all --renames
```

`-z` disables path quoting, so paths are **raw bytes** — verified with a
filename containing a literal newline.

Those bytes become `String` at this boundary, and the conversion has to be
injective or change sets stop meaning what they say. Replacement alone is not:
`\xff.txt` and `\xfe.txt` both render as `<?>.txt`, and because change sets are
intersected by string, two worktrees holding two *different* files were reported
as sharing one — with a verdict attached. Control characters have to go too, or
a filename containing `ESC [ 2 J` clears the pane it is drawn into. So: replace
invalid bytes and control characters, then append a digest of the raw bytes.
Identical bytes always render identically, which is what keeps `status` output
and `merge-tree` output matching each other; different bytes never collide. The
cost is that such a path no longer addresses its file on disk, so an untracked
file named that way is line-counted as zero.

Record grammar, split on NUL then on ASCII space with a bounded `splitn`:

| kind | layout | fields before path |
|---|---|---|
| header | `# <key> <value…>` | — |
| ordinary | `1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>` | 8 |
| rename/copy | `2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <Xscore> <path>` NUL `<origPath>` | 9 |
| unmerged | `u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>` | 10 |
| untracked | `? <path>` | 1 |
| ignored | `! <path>` | 1 |

**The framing rule naive parsers get wrong:** in `-z` mode a `2` record consumes
**two** NUL-terminated fields — the new path, then the original path as the very
next field. Both paths belong to the change set.

`X` is the index status, `Y` the worktree status: `.` unmodified, `M` modified,
`T` type change, `A` added, `D` deleted, `R` renamed, `C` copied, `U` unmerged.

Committed-since-merge-base paths, in one process:

```sh
git -C <wt> diff --name-only -z <integration-ref>...HEAD
```

The three-dot form already means "from the merge base to HEAD", so no separate
`merge-base` call is needed.

The worktree's change set is the union of the two.

Untracked line counts are read through bounded
`git diff --no-index --numstat -z -- /dev/null <path>` children, with content
filters neutralised exactly as they are for snapshotting. The path is relative
to `rev-parse --show-toplevel`, because status paths are repository-relative
even when the caller handed Git a subdirectory.

### Choosing the integration ref

There is no configured one, so it is probed. First match wins:

1. `refs/remotes/origin/HEAD`, `refs/remotes/origin/main`,
   `refs/remotes/origin/master`
2. `refs/heads/main`, `refs/heads/master`, `refs/heads/trunk`
3. `refs/remotes/<remote>/HEAD` for every other configured remote — a fork with
   an `upstream` and no `origin` has nothing above this
4. `refs/heads/<init.defaultBranch>`

**Never fall back to `HEAD`.** `HEAD...HEAD` is empty by construction, and
because both the base ref and the merge base then resolve, nothing is marked
degraded either — a repository whose trunk is `develop` reported every workspace
as clean while two agents committed conflicting edits to the same line of the
same file. When the chain finds nothing, hand the change set a sentinel that is
not a legal ref name (`<` and `>` are forbidden by `git check-ref-format`) and
degrade with `missing-base-ref`. An unmeasurable checkout must never be
indistinguishable from a clean one.

Every probe must also distinguish "git said no" from "git could not answer".
`rev-parse --verify -q` exits 1 for "does not resolve" and 128 for a real error,
and a command killed on our own deadline has no exit code at all; only the first
is an answer.

## Conflict prediction

### Committed state

```sh
git -C <repo> merge-tree --write-tree -z --name-only <oidA> <oidB>   # paths
```

This is the only form that may be used. `--quiet` looks like a cheap boolean
oracle for the same question and is not one — see "The --quiet trap" below.

Do **not** pass `--merge-base` here. Without it, merge-tree resolves multiple
bases recursively, which is more accurate on criss-cross histories than any
single base we could compute.

Exit codes:

| exit | meaning |
|---|---|
| 0 | merged cleanly |
| 1 | conflicts **or** a fatal argument error |
| 128 | unrelated histories, invalid object name |

**The argument trap:** a bad ref also exits 1, with empty stdout and a message on
stderr — indistinguishable from "conflict" by exit code. Mitigation, mandatory:
pre-resolve with `git rev-parse --verify -q '<rev>^{commit}'` and
`git cat-file -e '<oid>^{commit}'`, pass only 40-hex OIDs, and on exit 1 treat it
as a conflict **only if stdout is non-empty**. A real merge always prints at
least the merged tree OID, so empty stdout means the arguments were rejected.

An empty conflicted-file list is **not** a clean merge. git's own documentation
warns that some directory-rename conflicts produce no individual conflicted
file. The exit status is the authority; the file list only attributes the
conflict to paths.

### The --quiet trap

`git merge-tree --write-tree --quiet` **reports clean for merges that genuinely
conflict**. Verified on git 2.53.0. It must not be used as a mergeability
oracle, and collide no longer runs it.

The rule, established by bisecting a real failing pair down to a minimal case:

> `--quiet` stops as soon as it has processed a directory that **both** sides
> modified, and reports the whole merge clean if it has not seen a conflict by
> then. merge-ort walks paths in reverse-sorted order, so "by then" means
> "paths sorting after that directory". Any conflict on a path sorting *before*
> a both-sides-modified directory is silently lost.

Minimal reproducer — base has `README.md` and `docs/{a,b}.md`; both sides edit
`README.md` incompatibly, and each side additionally edits a *different* file
inside `docs/`:

```sh
git merge-tree --write-tree --quiet     --merge-base=$BASE $T1 $T2  # exit 0  WRONG
git merge-tree --write-tree -z --name-only --merge-base=$BASE $T1 $T2  # exit 1, CONFLICT in README.md
```

Confirmed behaviour, each direction tested:

| shape | agrees with `--name-only`? |
|---|---|
| conflict only, no other changes | yes |
| one-sided extra edit on **one** side only | yes |
| one-sided extra edits on both sides, but in **different** directories | yes |
| one-sided extra edits on both sides at the **top level** | yes |
| both-sides-touched directory sorts **before** the conflicting path | yes |
| both-sides-touched directory sorts **after** the conflicting path | **no — conflict lost** |
| as above, but a second conflict sorts after that directory | yes (the later conflict is seen) |

This is why flat, hand-built fixtures all pass while real worktrees fail: every
real repository has subdirectories, and two agents working in one repo routinely
touch the same directory. Independent of `merge.conflictstyle`, and reproducible
with `GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null`.

`tests/conflict_detection.rs` pins both halves: that collide reports the
conflict, and that git still exhibits the disagreement, so the regression test
cannot quietly go vacuous.

`-z` output framing: `<tree-oid> NUL`, then conflicted-file fields, then an
empty field ending the file section, then message records of
`<n> NUL <path>×n NUL <conflict-type> NUL <human-message>\n NUL`. A clean merge
in `-z` mode emits exactly one field, so `fields.len() == 1` means clean. The
human message carries a trailing newline — trim it.

Key off the machine-stable conflict-type tokens, never the prose:
`CONFLICT (contents)`, `CONFLICT (rename/rename)`, `CONFLICT (modify/delete)`,
`CONFLICT (directory rename suggested)`. Note that an add/add conflict reports
the token `CONFLICT (contents)`; only the human message says "add/add", so there
is no `CONFLICT (add/add)` token to match.

**Not every message record is a conflict.** git emits an `Auto-merging` record
for each file it merged *successfully*, in exactly the same framing, and the
records interleave. A two-file conflict yields
`Auto-merging, CONFLICT (contents), Auto-merging, CONFLICT (contents)`. Keep only
the fields beginning `CONFLICT (`, and deduplicate with a set — `Vec::dedup`
removes only *adjacent* duplicates and leaves that sequence untouched. An
assertion of the form `any(|t| t.starts_with("CONFLICT ("))` passes happily on
the noisy list, so pin the whole list instead.

`--write-tree` writes loose objects into the real ODB. Redirect them, always:

```sh
GIT_OBJECT_DIRECTORY=$TMP/odb \
GIT_ALTERNATE_OBJECT_DIRECTORIES=<repo>/.git/objects \
git -C <repo> merge-tree --write-tree -z --name-only <a> <b>
```

The same redirection is required for the temp-index snapshot below: `git add`
writes blobs too, so without it the plugin grows the user's object store on
every refresh.

### Uncommitted state

merge-tree only merges commits and trees, so dirty state must first become a
tree. Snapshot it through a throwaway index:

```sh
GD=$(git -C "$WT" rev-parse --path-format=absolute --git-dir)
IDX=$(mktemp)
cp "$GD/index" "$IDX"                         # seeding keeps the stat cache
GIT_INDEX_FILE="$IDX" git -C "$WT" $FILTER_OVERRIDES --literal-pathspecs \
  add -A --renormalize -- $CHANGED_FILES      # the stat cache lies; see below
GIT_INDEX_FILE="$IDX" git -C "$WT" $FILTER_OVERRIDES add -A --
GIT_INDEX_FILE="$IDX" git -C "$WT" $FILTER_OVERRIDES write-tree
rm -f "$IDX" "$IDX.lock"
```

Verified properties: the real index is byte-identical afterwards, the working
tree is untouched, it succeeds even while the worktree's real `index.lock` is
held, two concurrent snapshots of one worktree both succeed, untracked files are
staged (so add/add conflicts are predicted) while `.gitignore` is respected, and
cone-mode sparse checkouts are handled correctly rather than deleting
out-of-cone paths.

Seeding with `cp` costs 29 ms; `read-tree HEAD` instead costs 123 ms because it
discards the stat cache and rehashes every file.

**The seeded stat cache can hide an edit, and that is the dangerous direction.**
`add` skips re-hashing a path whose cached size and mtime still match, so a
same-size rewrite that lands inside one filesystem timestamp tick is invisible
and the prediction compares content the snapshot never saw — a real conflict
reported as a clean overlap. Reproduce it deterministically with
`core.trustctime=false` (an in-place rewrite always moves ctime and nothing can
set it back, which is what the tick race hides): stage a change to a distant
line so `status` names the path at all, push the file's mtime into the past and
`update-index --refresh` so the entry is not racily clean, then rewrite one line
to the same byte length and restore that mtime. The plain recipe yields the
stale blob; the `--renormalize` pass yields the new one.

`$CHANGED_FILES` is what `status` already reported, and the pass has four
bounds, each measured:

- **Only paths `status` named.** A path it did not name cannot have moved, and
  re-reading everything is the 123 ms the `cp` exists to avoid. A rewrite with
  *nothing* staged is invisible to `status` itself, so no consumer of this
  recipe can see it either; that is a limit of the approach, not a bug in it.
- **`--literal-pathspecs`.** These paths come from the user's worktree and reach
  `add` as pathspecs otherwise: `star*.txt` re-reads unrelated siblings, and a
  name beginning `:` is fatal.
- **Regular files and symlinks only.** A tracked file replaced by a directory
  still passes an `lstat` guard and then fails the pass outright — `error: 'f'
  does not have a commit checked out`, exit 255 — and a deleted path fails with
  `fatal: unable to stat`, exit 128. Neither can be hidden by a stat cache
  anyway; the general `add -A` below records the deletion or type change.
- **Retry only on a shrinking set.** A worktree an agent is editing can delete a
  path between the guard and the pass. Re-stat and retry once when the eligible
  set actually shrank; a set that did not shrink is a systematic failure and has
  to stay loud, because swallowing it restores the stale-content bug silently.

Do not close the hole by evicting the entries instead
(`update-index --force-remove` then `add -A`): a *tracked but ignored* file then
leaves the index, `add -A` refuses to put an ignored path back, and the snapshot
tree loses it — measured, `['.gitignore','app.log','shared.txt']` becomes
`['.gitignore','shared.txt']`, which is the one-sided deletion the seeding
exists to prevent. Eviction also drops each entry's index flags. Re-reading in
place keeps both, at the cost of one honest caveat: the filter overrides
neutralise custom drivers only, so `text` and `eol` attributes still apply and a
stat-clean *staged* entry can come back normalized (measured, a staged CRLF blob
`d5a6cc6` becomes LF `7be73ce`). Both sides of a pair go through the same path.

The seeding `cp` must not be best-effort. A missing index is the one benign
failure — a worktree that has none legitimately starts from empty — but every
other failure has to be an error, because seeding is exactly what preserves the
entries `add` will not revisit. Demonstrated on a cone-mode sparse checkout: the
seeded snapshot tree contains the out-of-cone `out/o.txt`, the unseeded one does
not, and merge-tree then reports one-sided deletions for files nobody touched.
(A *truncated* copy fails loudly on its own — `fatal: <path>: index file smaller
than expected` — so only a source that cannot be opened at all takes the silent
route.)

Then merge with an explicit base tree:

```sh
BT=$(git -C "$REPO" rev-parse "$(git -C "$REPO" merge-base "$H1" "$H2")^{tree}")
git -C "$REPO" merge-tree --write-tree -z --name-only --merge-base="$BT" "$T1" "$T2"
```

Caveats to encode:

- An **unmerged worktree** (merge in progress) cannot `write-tree` from the raw
  copied index. `add -A` collapses the stages by staging the on-disk file
  *including conflict markers*, so the snapshot succeeds but the tree contains
  `<<<<<<<`. Detect via a `u ` record in status and mark those predictions
  advisory.
- Passing `--merge-base` forces a single base. If `git merge-base --all` returns
  more than one, flag the result as approximate.
- **No common ancestor is not a prediction.** Substituting the empty tree for the
  base turns every shared path into an add/add and reports a confident conflict
  on all of them, while the commit form of the same pair lets merge-tree refuse
  with `refusing to merge unrelated histories` (exit 128). The same two orphan
  branches then flip between "unknown" and "everything conflicts" depending on
  whether one of them happens to have a stray untracked file. Detect the empty
  `merge-base --all` explicitly and give both shapes the same answer: unknown.

### Do not prefilter twice

The pairing pass owns the decision about which pairs are worth predicting,
because it is the only place that holds both change sets. A second prefilter
inside the predictor — "skip a pair with no shared path unless a side renamed
something" — sounds equivalent and is not: the predictor derives "renamed" from
`status`, which shows only *uncommitted* renames. A worktree that had committed
a directory rename and was otherwise clean therefore short-circuited to a
conflict-free verdict, while merge-tree on the very same pair exits 1:

```sh
# A renames docs/ to guide/ and commits; B adds docs/notes-c.md and commits.
# Change sets: {docs/*, guide/*} and {docs/notes-c.md} — intersection empty.
git -C repo merge-tree --write-tree -z --name-only wa wb
# exit 1, conflicted file `guide/notes-c.md`,
# token `CONFLICT (directory rename suggested)`
```

A clean pair costs 1.77 ms to answer properly. That is the whole saving the
prefilter was buying, against losing a conflict.

## Degenerate cases

| case | detection | action |
|---|---|---|
| detached HEAD | `worktree list` → `detached`; `symbolic-ref -q HEAD` exits 1 | usable, use the raw OID |
| no commit on this branch | `symbolic-ref -q HEAD` exits 0; `rev-parse --verify -q 'HEAD^{commit}'` exits 1; `show-ref --verify --quiet <ref>` exits 1 | exclude from pairing; change set is every `A.` entry |
| broken HEAD | as above, but `show-ref --verify --quiet <ref>` exits 0 or 128 — the ref is there and still yields no commit | exclude from pairing, report as broken |
| HEAD git cannot read at all | any probe exits 128, times out, or has no exit code | error, and degrade with `unreadable` — never "no commit" |
| foreign repo | differing common-dir | never compare |

### Unborn versus deleted: two wrong answers, recorded

This file has twice asserted a discriminator between "this branch never had a
commit" and "this branch was deleted underneath the worktree". Both were tested
and both are wrong.

1. `symbolic-ref -q HEAD` was said to exit 1 for unborn and 0 for deleted. On
   git 2.53.0 it exits 0 and prints the same ref name in both cases.
2. The worktree's `logs/HEAD` was then said to be the discriminator that works —
   a worktree that ever had a commit checked out has one. It is wrong in **both**
   directions:

   ```sh
   # genuinely unborn, and it has a reflog: the old rule says "deleted"
   git checkout --orphan fresh          # in a worktree that already had commits

   # genuinely deleted, and it has no reflog: the old rule says "unborn"
   git config core.logAllRefUpdates false
   git worktree add -b doomed ../doomed main
   git update-ref -d refs/heads/doomed
   ```

The honest conclusion is that the two are the **same observable state**: HEAD is
a symref to a ref that is not in the ref store. Nothing in the ref store, the
index or the worktree tells them apart, and reflogging is a configuration choice
rather than evidence about commits. Report both as "no commit on this branch",
which is the part that is true and the part that decides what happens next —
either way the checkout cannot be merged against anything.

What the ref store *can* prove is the genuinely broken case: the ref exists and
still yields no commit, because it points at a missing object or at a non-commit.
`show-ref --verify --quiet` answers that in one command — 0 the ref resolves,
1 there is no such ref, 128 the ref is there but its object is not. Note that a
worktree in that state cannot even be `status`ed (`fatal: bad object HEAD`), so
it fails loudly one step earlier.

## Cost, and the resulting pipeline

Run `cargo bench --bench gather_cost` to rebuild the representative outer
worktree, predicted-conflict, and dirty-submodule fixtures and measure complete
`gather_for` cycles. Every case asserts its verdict before timing, reports the
worktree/pair/sample counts, and uses the shipped size-optimised profile. The
numbers are diagnostic rather than a CI gate: shared runners and filesystem
caches are too noisy for a stable threshold.

| command | ms |
|---|---|
| `worktree list --porcelain` | 1 |
| `merge-base` / `rev-parse` | 2 |
| `diff --name-only -z <base>...HEAD` | 2 |
| `status --porcelain=v2 -z -uall` | 6 |
| temp-index snapshot, seeded | 29 |

### `merge-tree` cost, re-measured

The earlier claim that `--quiet` is ~15× cheaper than `--name-only` was measured
on one pathological pair and generalised. Re-measured across pair shapes, median
of 30 runs, warm cache:

| pair | `--quiet` | `-z --name-only` |
|---|---|---|
| clean, 200 changed files per side, 5000-file repo | 1.72 ms | 1.77 ms |
| conflicting, 12 conflicted files | 1.76 ms | 1.97 ms |
| conflicting, 500 conflicted files | 2.37 ms | 35.37 ms |
| the real failing pair from this repo | 1.48 ms | 1.87 ms |

`--name-only` costs what it costs because it writes the merged tree and one blob
per conflicted file, so its price scales with the **number of conflicted files**,
not with repo size. The 15× gap therefore only appears on pairs with hundreds of
conflicts — pairs the old pipeline had to run `--name-only` on anyway.

The gate only ever saved work on **clean** pairs, and a clean pair is exactly
where `--name-only` has nothing to write: 1.77 ms against 1.72 ms. The two-phase
design bought 0.05 ms per clean pair and paid for it in lost conflicts.

`--stdin` batching saves nothing (31 ms vs 34 ms for three pairs) and is not
worth the parsing complexity.

Pipeline for N worktrees (N(N−1)/2 pairs):

1. **Prefilter, free, and in one place only.** Intersect change sets in the
   pairing pass. Skip pairs with an empty intersection — they cannot conflict.
   Exception: do not skip when either side has a rename, *committed or
   uncommitted*. See "Do not prefilter twice" for what happens when the predictor
   second-guesses this with a narrower notion of "rename".
2. **Predict, ~2 ms/pair.** `merge-tree --write-tree -z --name-only` on every
   pair it hands over, in parallel, with objects redirected. One phase. Do not
   add a `--quiet` pre-check: see "The --quiet trap".

Snapshot each worktree once and reuse its tree OID across all of its pairs.
Change-set collection retains the porcelain-v2 branch/HEAD facts, top level,
filter overrides, status entries, and integration OID that prediction needs.
The predictor therefore does not repeat branch, HEAD, status, filter, git-dir,
or target-ref probes. Independent outer snapshots and deduplicated direct
submodule sides fan out across at most eight workers.

Measured by `gather_cost` after that cutover, warm cache on the development
machine:

| case | before | prepared/parallel |
|---|---:|---:|
| 16 dirty worktrees, 120 overlap pairs | 372.88 ms | 287.74 ms |
| one predicted conflict | 88.01 ms | 50.84 ms |
| one dirty direct-submodule conflict | 213.23 ms | 121.71 ms |

The 16-worktree cycle fell 22.8%; the dirty-submodule case fell 42.9%. These
numbers include every unchanged gathering and analysis phase, so they are the
user-visible wall-time improvement rather than an isolated microbenchmark.

Nested commands use a repository-specific object view. While snapshotting one
nested checkout the exact write-related environment is:

- `GIT_OBJECT_DIRECTORY=<predictor scratch>/submodule-<seq>/odb`
- `GIT_ALTERNATE_OBJECT_DIRECTORIES=<nested common dir>/objects`
- `GIT_INDEX_FILE=<predictor scratch>/submodule-index-<seq>`

For the nested `merge-base`, `rev-parse`, and `merge-tree`, the object directory
is the left nested side's scratch ODB and the alternates list is the left
nested common-dir object store, the right nested scratch ODB, and the right
nested common-dir object store, joined with the platform path separator. Thus
the command can read both independent clones and both snapshots while every
object it writes still lands under predictor scratch. `run_git` also sets
`GIT_OPTIONAL_LOCKS=0` and clears inherited repository-targeting variables for
every one of these commands.

## Known limitations

- **Dirty direct submodules are compared to depth one.** `status` still reports
  the submodule as one superproject-relative changed path
  (`1 .M S.MU … sub`), but the bounded nested-prime workers open each required
  direct checkout as a repository, resolve its common dir and HEAD, record
  dirty/unmerged state, and snapshot dirty contents through a scratch index.
  The immutable prediction phase then runs a nested merge. A clean nested merge
  earns `overlap`; a nested conflict makes the superproject gitlink path
  `conflict`, with nested conflicting paths carried only in the detail note so
  path scopes never mix. A missing or uninitialised checkout, unborn or broken
  nested HEAD, timeout, unrelated history, or any other failed nested command
  leaves the verdict `unknown`. A changed recorded gitlink is still compared by
  the outer merge and any outer conflict remains authoritative. Submodules
  inside the direct submodule remain gitlinks and are not recursively opened.
  Work below a submodule still line-counts as zero, so it remains invisible to
  the runaway thresholds.

## Filename encoding differs by platform

macOS (APFS and HFS+) enforces valid UTF-8 in filenames and answers `EILSEQ`;
Linux filesystems accept any byte except `/` and NUL. So the case where two
files' names differ only in an invalid byte — the one the path digest exists to
keep apart — **cannot be constructed on macOS at all**. Observed on CI: the
fixture that creates `\xff.txt` fails there with
`Os { code: 92, message: "Illegal byte sequence" }`.

The on-disk test therefore skips itself, loudly, where the filesystem refuses
the names, and the parser half is covered everywhere from captured bytes
instead. Do not "fix" that skip by dropping the case: it is a real difference
between the two supported platforms, and on macOS the bug it guards against
cannot happen.

## Overall refresh deadline

`git_timeout_seconds` bounds one child; `cycle_timeout_seconds` bounds the
repository-analysis cycle. An absolute deadline is inherited by checkout and
pair-prediction worker threads, and every `run_git` call receives the smaller
of its ordinary timeout and the cycle's remaining time. Once it expires, the
cycle discards partial clean-looking output and reports all observed checkouts
as `cycle-timeout` unknowns.

Repository identity, top-level discovery, per-worktree git-dir discovery,
nested git-dir/common-dir discovery, and untracked line reads all go through
that process boundary. Scratch/state directory housekeeping remains ordinary
filesystem I/O because it is plugin-owned rather than repository data.

## Unverified

Cold-cache timings; `feature.manyFiles`, split index and untracked-cache
interactions with the seeded temp index; Windows and case-insensitive or NFD
filesystems; `.gitattributes` **merge** drivers (clean/smudge filters are
covered above); worktrees relocated across mounts. `core.fsmonitor` is now
exercised as a pipe-leak fixture in `tests/read_only.rs`, but its effect on the
seeded temp index is still untested.
