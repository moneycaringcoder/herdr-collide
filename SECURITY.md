# Security policy

## Reporting a vulnerability

Please report security issues privately, through GitHub's
[private vulnerability reporting](https://github.com/moneycaringcoder/herdr-collide/security/advisories/new)
rather than as a public issue.

You can expect an acknowledgement within a few days. Since this is a
single-maintainer project, please don't read silence as dismissal — follow up
if you have heard nothing after a week.

If you would rather not use GitHub's reporting flow, open a public issue saying
only that you have found a security problem and would like a private channel,
with no details, and one will be arranged.

## What counts as a security issue here

collide reads repository state and talks to a local herdr socket. The things
worth reporting urgently:

- **Any write to a user's repository.** The plugin is meant to be strictly
  read-only. A path that mutates the index, working tree, refs, or object store
  is a serious bug even if it looks harmless, because the plugin runs
  unattended against in-flight agent work.
- **Anything that could destroy or corrupt uncommitted work**, including
  leftover lock files that block a user's own git commands.
- **Leaking repository contents** — file contents, branch names, or paths —
  anywhere they should not go. collide makes no network calls at all, so any
  outbound traffic is a bug by definition.
- **Editing a user's `config.toml` incorrectly.** The setup action modifies a
  file the plugin does not own; corrupting it, or losing the backup that makes
  the change reversible, is in scope.
- **Command injection through a branch name, path, or config value.** Git
  invocations pass arguments as argv arrays rather than through a shell, so
  this should not be reachable — a way around that is worth reporting.

## What is out of scope

- Wrong badge counts, missed collisions, or false conflicts. These are ordinary
  bugs; please open a normal issue.
- The plugin executing code you gave it, such as a config file you wrote
  yourself.
- Issues in herdr itself, or in git. Those belong upstream, though a report here
  is welcome if collide could work around one.

## Supported versions

The most recent release is supported. Given the size of the project, fixes are
made on `main` and released rather than backported.
