# Contributing to collide

Contributions are genuinely welcome — bug reports, questions, documentation
fixes, and code. This document exists so you know what to expect before you
spend time on something, not to put obstacles in front of you.

The project is maintained by one person. That means review is attentive but not
instant, and it means every change is read carefully before it lands. Please
don't take questions on a pull request as resistance; they are how the
maintainer stays confident in code that runs against other people's
repositories.

## The one rule that matters

**collide is read-only against user repositories.** It runs on machines full of
in-flight, uncommitted, unpushed agent work. A bug that loses someone's work is
categorically worse than a bug that shows a wrong badge.

So any change touching `src/git.rs` must keep these true:

- every git invocation passes `--no-optional-locks`
- nothing is ever staged through a real index — snapshots go through a
  temporary `GIT_INDEX_FILE`
- object writes are redirected via `GIT_OBJECT_DIRECTORY` so the user's object
  store never grows
- no command mutates refs, the working tree, or the index

`tests/read_only.rs` enforces all of it by fingerprinting the index bytes,
working tree, refs, reflogs and object count before and after a full run,
including while another process holds `index.lock`. If your change makes that
test fail, the test is right and the change is wrong.

## Getting set up

```sh
git clone https://github.com/moneycaringcoder/herdr-collide
cd herdr-collide
cargo build --release
herdr plugin link .          # note: `link` does NOT run the build step
```

Rebuild by hand after every change, since `herdr plugin link` deliberately
skips the `[[build]]` hook.

Before opening a pull request:

```sh
cargo fmt --all
cargo clippy --all-targets --locked -- -D warnings
cargo test --all
```

CI runs exactly these on Linux and macOS with the current stable toolchain. If
your local Rust is older than CI's, clippy will pass locally and fail there —
`rustup update stable` first if in doubt.

No test requires a running herdr. The fixtures build throwaway git repositories
in a temp directory and clean up after themselves.

## What makes a change easy to merge

**A test that fails before your fix and passes after it.** This matters more
here than in most projects, because the bugs this plugin attracts are
*invisible* ones: a wrong answer with no error, which looks exactly like a
correct answer. Four such bugs shipped past a green test suite during initial
development, and each one is now pinned by a regression test.

**Tests built from observed behaviour, not assumed behaviour.** The socket
client once passed its whole suite while being wrong, because the test's fake
server replied with the shape the client expected rather than the shape herdr
actually sends. If you are testing against herdr or git, capture real output
first — `herdr api snapshot`, a real fixture repo — and encode that.

**Verification against something real.** If a change affects what a user sees,
run it against a live herdr session with at least two worktrees of one
repository and say what you observed. A passing suite is necessary and not
sufficient; the README's screenshots came from real runs, not mock-ups.

**Comments that say why, not what.** The code is full of small, load-bearing
decisions that look arbitrary until explained — why the disable path waits for
the daemon to exit, why severity rides the token *name*, why `--quiet` is
avoided. If your change encodes a decision like that, leave the reason behind.

**A breaking `--json` shape change bumps `JSON_SCHEMA_VERSION` and adds a
changelog entry.** Removing or renaming a key, changing a value's type, or
adding a value to the `severity` or `verdict` enum is breaking. Adding a key or
an element to an array is compatible and does not bump the version; array order
is not part of the contract. Keep the README inventory and the schema tests in
sync so consumers can tell when an exhaustive match needs to change.

## What to expect from review

- Small fixes — a typo, a clear bug with a test, a documentation correction —
  are usually merged quickly and without ceremony.
- Behavioural changes get discussed. If the change alters what the badge shows,
  what counts as a conflict, or what the plugin writes anywhere, expect
  questions about edge cases before it lands.
- Larger features are best raised as an issue first, so you don't build
  something the project then declines. "Would you take a PR that does X?" is
  always a fine question, and a fast answer is more useful to you than a
  thorough review of work that was never going to land.

The maintainer reviews every pull request personally and may make small edits
on merge rather than sending a change back for a one-line fix.

## Scope

collide deliberately does a narrow thing well. Things that are in scope:
detection quality, correctness against unusual repository states, performance,
clearer output, better documentation, platform support for Linux and macOS.

Things that are out of scope, and why:

- **Resolving conflicts, or writing to repositories in any way.** The read-only
  guarantee is the reason the plugin is safe to run unattended.
- **Anything requiring a network call.** No GitHub API, no telemetry, no update
  checks. Everything is local git and the herdr socket.
- **Windows.** Not refused on principle, but the socket layer, daemon
  detachment and path handling are Unix-shaped, and there is no way to test it
  here. A well-tested contribution would be considered.

## Reporting bugs

Please include the output of `collide --json`, your `herdr --version`, your
`git --version`, and what you expected to see instead. If it involves the
sidebar, the relevant part of your `config.toml` helps too.

Redact freely — paths and branch names can be sensitive, and a report with
`/home/you/repos/app` in it is just as useful.

## Security

Please don't open a public issue for a security problem. See
[SECURITY.md](SECURITY.md).

## Licence

By contributing, you agree that your contributions are licensed under the MIT
Licence, the same terms that cover the project.
