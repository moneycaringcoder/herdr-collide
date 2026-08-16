# Roadmap

Ideas for future work, roughly in the order they would most improve the plugin.
None of this is committed to a release, and nothing here is a promise.

The rule everything below is measured against is the one the plugin exists for:
**overlap is not conflict**, and a prediction that cannot be made must say so
rather than be downgraded into a quieter answer that looks the same as good news.

## Closing the blind spots

### Compare submodule contents, or refuse to guess

A dirty submodule shows up as one changed path, but the snapshot records the
submodule's committed pointer rather than its contents. Two worktrees editing the
same submodule therefore read as a harmless overlap and never as a conflict, and
the work inside is invisible to the runaway thresholds.

That is the one remaining place where the plugin reports good news it has not
earned. Comparing submodule contents is the complete fix; marking the pairing
`? unknown` is the honest fallback and should land first, because it is small and
removes the false reassurance immediately.

### Detect renames and moves

If one worktree renames a file and another edits it, there is no shared path, so
the pair reports no overlap at all. That is a real conflict the tool currently
cannot see. Git's own rename detection is available; the work is deciding how a
rename pairing should be presented, since it is neither a plain overlap nor a
textual conflict.

### Harden unusual repository layouts

Repository identity across linked worktrees is observed rather than specified. It
holds for ordinary `git worktree` layouts; `--separate-git-dir` and submodule
setups are less well tested. Fixtures for both would turn an observation into a
guarantee, or find the case where it does not hold.

## Better answers

### Predict against the merge target, not only pairwise

Two worktrees can be mutually clean and both conflict with `main`. Predicting each
side against the integration ref, alongside the existing pairwise prediction,
catches the case where the collision is with the destination rather than with a
colleague.

### Rank pairings by severity

With several worktrees open, the useful question is not "is there a conflict" but
"which one do I deal with first". Ordering pairings by conflicting file count, and
surfacing the worst in the badge, answers it without opening the detail pane.

### `--why <path>`

An `x conflict` verdict is a summary of a real git merge that already ran in a
temporary index. Showing that output — the actual conflicting hunks — turns the
verdict into something a person can act on, and lets them disagree with it.

### Ignore rules beyond suffixes

`ignore_suffixes` handles lockfiles. It does not handle a generated directory, a
vendored tree, or a build output path. Path globs and directory rules would cover
the rest without users listing every extension a code generator emits.

## Noticing things

### Notify on transition, not on state

A conflict that has existed for ten minutes is not news; a pairing that *became*
conflicting thirty seconds ago is. Notifying on the edge rather than the level is
the difference between a useful alert and one people mute.

### Conflict history

Recording episodes would show that two agents keep colliding on the same file
week after week. That is rarely a git problem and usually a design one — the file
is doing too much, or the work was split along the wrong seam. The plugin is the
only thing positioned to notice it.

## Interfaces

### A versioned `--json` schema

The JSON output is useful for scripting, and scripting is only safe against a
shape that promises not to change silently. A schema version, and a changelog
entry whenever it moves, makes that promise explicit.

## Platforms

### Windows

The daemon relies on Unix process and signal behaviour, and the manifest declares
Linux and macOS accordingly. Windows support means replacing that lifecycle, not
just relaxing the manifest — worth doing only once someone actually asks.

## Blocked upstream

### Event-driven refresh

The plugin polls because herdr exposes no filesystem or git events. Nothing here
is worth working around with a shorter interval, which only spends more of the
budget to shrink the same window. If upstream ever exposes change events, this
becomes the single largest improvement available and everything above it in this
list stays true regardless.
