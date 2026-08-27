# Roadmap

Ideas for future work, roughly in the order they would most improve the plugin.
None of this is committed to a release, and nothing here is a promise.

Anything that lands here is measured against the rule the plugin exists for:
**overlap is not conflict**, and a prediction that cannot be made must say so
rather than be downgraded into a quieter answer that looks the same as good news.

## Platforms

### Windows

The daemon relies on Unix process and signal behaviour, and the manifest declares
Linux and macOS accordingly. Windows support means replacing that lifecycle, not
just relaxing the manifest — worth doing only once someone actually asks.

## Blocked upstream

### Filesystem-driven refresh

The plugin still polls because herdr exposes no filesystem or Git edit events.
Workspace and worktree lifecycle events now wake the daemon immediately, but
nothing can announce an ordinary file write. A shorter interval only spends
more of the budget to shrink the same window. If upstream exposes repository
change events, replacing edit polling remains the largest latency improvement.

For the pairwise verdicts, polling costs latency rather than correctness. The
edge-triggered features pay more than latency for it: notifications and conflict
history are both differences between consecutive cycles, so a collision that
appears and clears inside one interval is never notified and never recorded, and
episode timestamps land on poll boundaries rather than on the edit.
