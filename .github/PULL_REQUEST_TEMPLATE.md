<!--
Thanks for contributing. Nothing here is meant to be a hurdle — delete any
section that does not apply. A one-line typo fix needs a one-line description.
-->

## What this changes

<!-- What the change does, and why. If it fixes an issue, link it. -->

## How it was verified

<!--
Which of these you did. The suite passing is necessary but often not
sufficient: several bugs in this project were wrong answers with no error,
which look identical to correct ones until you run them for real.
-->

- [ ] `cargo fmt --all` and `cargo clippy --all-targets -- -D warnings` are clean
- [ ] `cargo test --all` passes
- [ ] There is a test that fails without this change
- [ ] Ran against a live herdr session, with what I observed described below

<!-- If it changes what a user sees, paste the before and after. -->

## Read-only guarantee

<!--
Only relevant if you touched src/git.rs or anything that shells out to git.
Delete this section otherwise.
-->

- [ ] `tests/read_only.rs` still passes
- [ ] No new git invocation writes to the index, working tree, refs, or object
      store, and every one passes `--no-optional-locks`
