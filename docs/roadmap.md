# Roadmap

Ideas for future work, roughly in the order they would most improve the plugin.
None of this is committed to a release, and nothing here is a promise.

The rule everything below is measured against is the one the plugin exists for:
**overlap is not conflict**, and a prediction that cannot be made must say so
rather than be downgraded into a quieter answer that looks the same as good news.

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
becomes the single largest improvement available. Nothing already shipped depends
on it: every verdict the plugin reports is correct for the moment it was taken,
and events would only shorten the window before it is taken again.
