# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0]

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

[Unreleased]: https://github.com/moneycaringcoder/herdr-collide/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/moneycaringcoder/herdr-collide/releases/tag/v0.1.0
