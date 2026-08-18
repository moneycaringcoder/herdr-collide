# Roadmap

Ideas for future work, roughly in the order they would most improve the plugin.
None of this is committed to a release, and nothing here is a promise.

Anything that lands here is measured against the rule the plugin exists for:
**overlap is not conflict**, and a prediction that cannot be made must say so
rather than be downgraded into a quieter answer that looks the same as good news.

## Closing the blind spots

### Decide the repository root by rule rather than by fallback

Rule 1 of the repository-root agreement joins `.git` to a worktree's top level and
compares the result against the repository key. Under `--separate-git-dir` that
path is a gitfile naming the store rather than the store itself, so the rule cannot
fire and the root is settled by the deterministic fallback instead. The fixtures in
`tests/repo_layout.rs` pin the current answer, which is right for every layout
tested — but right by fallback, not by the rule written to decide it. The doc
comment on `agree_on_repo_root` still claims rule 1 covers that layout, and is
wrong in the same place.

## Platforms

### Windows

The daemon relies on Unix process and signal behaviour, and the manifest declares
Linux and macOS accordingly. Windows support means replacing that lifecycle, not
just relaxing the manifest — worth doing only once someone actually asks.

## Blocked upstream

### Event-driven refresh

The plugin polls because herdr exposes no filesystem or git events. Nothing here is
worth working around with a shorter interval, which only spends more of the budget
to shrink the same window. If upstream ever exposes change events, this becomes the
single largest improvement available.

For the pairwise verdicts, polling costs latency rather than correctness. The
edge-triggered features pay more than latency for it: notifications and conflict
history are both differences between consecutive cycles, so a collision that
appears and clears inside one interval is never notified and never recorded, and
episode timestamps land on poll boundaries rather than on the edit.
