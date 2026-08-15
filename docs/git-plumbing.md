# git plumbing notes (verified on git 2.53.0, Linux)

Working notes for `src/git.rs`. Every command here was verified against a
purpose-built fixture repo containing worktrees that genuinely conflict,
worktrees that touch the same file without conflicting, and every degenerate
worktree state. Timings are warm-cache on a 5000-file, 60 MB repo.

## Hard rules

1. Always pass `--no-optional-locks` to `status`. Plain `status` takes
   `<gitdir>/index.lock` to write back its stat cache — verified by watching the
   index mtime advance. With the flag it does not.
2. Never touch a worktree's real index. All staging goes through a temporary
   `GIT_INDEX_FILE`.
3. Resolve both sides to 40-hex OIDs before calling `merge-tree`. See the
   exit-code trap below.
4. Only compare worktrees whose canonicalized
   `git rev-parse --path-format=absolute --git-common-dir` is identical.

Everything except `status` is lock-free and safely parallel. A 120-process
stress run produced zero errors and no leftover `index.lock`.

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
`CONFLICT (contents)`, `CONFLICT (rename/rename)`, `CONFLICT (modify/delete)`.
Note that an add/add conflict reports the token `CONFLICT (contents)`; only the
human message says "add/add", so there is no `CONFLICT (add/add)` token to match.

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
cp "$GD/index" "$IDX" 2>/dev/null || :        # seeding keeps the stat cache
GIT_INDEX_FILE="$IDX" git -C "$WT" add -A --
GIT_INDEX_FILE="$IDX" git -C "$WT" write-tree
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

## Degenerate cases

| case | detection | action |
|---|---|---|
| detached HEAD | `worktree list` → `detached`; `symbolic-ref -q HEAD` exits 1 | usable, use the raw OID |
| unborn branch | `HEAD 0000…`; `rev-parse --verify -q 'HEAD^{commit}'` exits 1; **no** `logs/HEAD` | exclude from pairing; change set is every `A.` entry |
| branch deleted underneath | as unborn, but the worktree **has** a non-empty `logs/HEAD` | exclude from pairing, report as broken |
| foreign repo | differing common-dir | never compare |

Correction to an earlier note here: `symbolic-ref -q HEAD` does **not**
distinguish unborn from deleted-branch. On git 2.53.0 it exits 0 and prints the
same ref name in both cases. The discriminator that works is the worktree's own
HEAD reflog: a worktree that ever had a commit checked out has `logs/HEAD`, a
freshly initialised one does not.

## Cost, and the resulting pipeline

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

1. **Prefilter, free.** Intersect change sets. Skip pairs with an empty
   intersection — they cannot conflict. Exception: do not skip when either side
   has a rename record.
2. **Predict, ~2 ms/pair.** `merge-tree --write-tree -z --name-only` on the
   survivors, in parallel, with objects redirected. One phase. Do not add a
   `--quiet` pre-check: see "The --quiet trap".

Snapshot each worktree once and reuse its tree OID across all of its pairs.

Budget check at the default 5 s interval: 8 worktrees give 28 pairs, so ~55 ms
of prediction plus ~230 ms of snapshotting — about 6 % of one interval, before
the parallel fan-out. Prediction is not the term that matters; the per-worktree
snapshot is.

## Unverified

Cold-cache timings; `core.fsmonitor`, `feature.manyFiles`, split index and
untracked-cache interactions with the seeded temp index; Windows and
case-insensitive or NFD filesystems; submodules and `.gitattributes` merge
drivers; worktrees relocated across mounts.
