//! Git layer: change sets and conflict prediction.
//!
//! Every invocation is read-only against the user's repository. The only writes
//! this module ever performs are to temporary index files under the plugin
//! state dir, via `GIT_INDEX_FILE`.
//!
//! Lock avoidance is done with the `GIT_OPTIONAL_LOCKS=0` environment variable,
//! which [`run_git`] sets for *every* command. That is the mechanism that
//! actually holds: a plain `status` or `diff` would otherwise take
//! `<gitdir>/index.lock` to write back its stat cache and contend with the very
//! agent we are watching. The `--no-optional-locks` flag is passed to `status`
//! as well, but only as belt and braces — removing the environment variable
//! would silently reintroduce index writeback on every `diff`.
//!
//! Object writes are redirected too. `git add` (index snapshot) and
//! `merge-tree --write-tree` both create loose objects; both are run with
//! `GIT_OBJECT_DIRECTORY` pointed at a scratch directory and
//! `GIT_ALTERNATE_OBJECT_DIRECTORIES` pointed at the real object store, so the
//! user's ODB never grows and never needs a `gc` it did not ask for.
//!
//! Content filters are neutralised for the snapshot. `git add` would otherwise
//! run every configured `filter.<driver>.clean`/`.process` program — arbitrary
//! user code, which for git-lfs writes into the user's own `.git/lfs`. See
//! [`Predictor::filter_overrides`].
//!
//! Every child is put in its own process group and the *group* is killed on
//! expiry, because a git that leaves a descendant behind holding the stdout or
//! stderr pipe would otherwise park the refresh loop for as long as that
//! descendant lives — a timeout that only kills the direct child is not a
//! timeout.
//!
//! Paths come out of git as raw bytes. `model::ChangedPath` uses `String`, so
//! every path is converted at this boundary by [`lossy`], which replaces
//! non-UTF-8 bytes and control characters and then appends a digest of the raw
//! bytes so that two different files can never collapse onto one string.

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::fs;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::config;
use crate::model::{ChangeKind, ChangeSet, ChangedPath, RepoKey};
use crate::Result;

/// Machine-readable prefixes for `ChangeSet::degraded_reason`. The analysis
/// pass keys off these rather than the human text, and two of them
/// (`DEGRADED_UNBORN`, `DEGRADED_BROKEN_HEAD`) mean "this checkout has no
/// commit and must never be paired".
pub const DEGRADED_UNBORN: &str = "unborn-branch";
pub const DEGRADED_BROKEN_HEAD: &str = "broken-head";
pub const DEGRADED_MISSING_BASE_REF: &str = "missing-base-ref";
pub const DEGRADED_NO_MERGE_BASE: &str = "no-merge-base";
pub const DEGRADED_UNMERGED: &str = "merge-in-progress";
/// The git pass failed outright for this checkout. Distinct from every reason
/// above, which all describe a checkout we *did* read: this one means we did
/// not, so an empty change set carries no information at all.
pub const DEGRADED_UNREADABLE: &str = "unreadable";
/// Line counts could only be read in part, so the runaway thresholds are
/// measured against an understated total.
pub const DEGRADED_PARTIAL_VOLUME: &str = "partial-volume";
/// The overall refresh deadline expired before the cycle completed.
pub const DEGRADED_CYCLE_TIMEOUT: &str = "cycle-timeout";

/// Reason codes that exclude a checkout from pairing entirely.
pub const UNPAIRABLE_REASONS: [&str; 2] = [DEGRADED_UNBORN, DEGRADED_BROKEN_HEAD];

/// The `base` [`change_set`] is handed when [`integration_ref`] found nothing.
///
/// A repository whose trunk is not `main`, `master` or `trunk` and that has no
/// remote used to be measured against `HEAD`, which makes the committed half of
/// every change set empty *and reports no degradation at all* — two agents
/// about to collide head-on then read as two clean workspaces. There is no
/// honest ref to substitute, so the caller passes this instead and the checkout
/// is visibly degraded.
///
/// `<` and `>` are forbidden in ref names by `git check-ref-format`, so this can
/// never be mistaken for one a user configured.
pub const NO_INTEGRATION_REF: &str = "<no-integration-ref>";

/// How long to keep waiting for a child's pipes after the child itself is gone.
///
/// The child cannot exit until it has written everything, and it can only write
/// while we drain, so by the time we get here all but the last pipe-buffer's
/// worth is already read and EOF is imminent. Anything longer than this means a
/// descendant is holding the write end open, which is a different problem and is
/// solved by killing the process group rather than by waiting.
const PIPE_DRAIN_GRACE: Duration = Duration::from_millis(500);

// ---------------------------------------------------------------------------
// Process plumbing
thread_local! {
    static CYCLE_DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

struct CycleDeadlineGuard {
    previous: Option<Instant>,
}

impl Drop for CycleDeadlineGuard {
    fn drop(&mut self) {
        CYCLE_DEADLINE.set(self.previous);
    }
}

/// Runs `operation` with one absolute refresh deadline on the current thread.
/// Worker threads must explicitly inherit [`current_cycle_deadline`].
pub fn with_cycle_deadline<T>(deadline: Option<Instant>, operation: impl FnOnce() -> T) -> T {
    let previous = CYCLE_DEADLINE.replace(deadline);
    let _guard = CycleDeadlineGuard { previous };
    operation()
}

pub fn current_cycle_deadline() -> Option<Instant> {
    CYCLE_DEADLINE.get()
}

pub fn cycle_deadline_expired() -> bool {
    current_cycle_deadline().is_some_and(|deadline| Instant::now() >= deadline)
}

fn effective_timeout(requested: Duration) -> Duration {
    current_cycle_deadline().map_or(requested, |deadline| {
        requested.min(deadline.saturating_duration_since(Instant::now()))
    })
}

// ---------------------------------------------------------------------------

#[derive(Debug)]
struct GitOut {
    /// `None` when the child was killed by a signal, killed by our own deadline,
    /// or exited cleanly but left its output undrained. The last case is
    /// deliberately folded in here: a caller that sees `Some(0)` is entitled to
    /// treat `stdout` as the whole answer, and after a truncated read it is not.
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// The deadline fired, or the pipes could not be drained within
    /// [`PIPE_DRAIN_GRACE`] of the child exiting.
    timed_out: bool,
}

impl GitOut {
    fn ok(&self) -> bool {
        self.code == Some(0)
    }

    fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_string()
    }

    fn stdout_trimmed(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_string()
    }
}

/// Runs one git command with a deadline that actually holds.
///
/// A hung git (a stuck credential helper, an NFS stall, an fsmonitor that never
/// answers) must not stall the refresh loop, so the child is polled and killed
/// on expiry. The pipes are drained on separate threads because a child that
/// fills a 64 KiB pipe buffer blocks forever otherwise, and `status -uall` on a
/// big repo comfortably exceeds that.
///
/// Killing the child is not enough on its own, and this was a real bug rather
/// than a theoretical one. A pipe reaches EOF only when *every* holder of its
/// write end has closed it, so a process git leaves behind — a
/// `.gitattributes` clean filter that daemonises, a `core.fsmonitor` hook, a
/// credential helper — keeps `read_to_end` blocked long after git itself is
/// dead. Measured: a `git add` that finishes in 80 ms took over 40 s to return
/// through this function under a 2 s deadline. Two defences, both needed:
///
/// * the child gets its own process group (`setsid`), so the whole group can be
///   killed on expiry rather than just the process we spawned; and
/// * the drain threads are joined with a bounded wait, so even a group we
///   cannot kill can only cost [`PIPE_DRAIN_GRACE`] rather than forever.
///
/// A drain that does not finish is reported as `timed_out` with a `None` exit
/// code, never as a successful command with short output.
fn git_command(dir: &Path, envs: &[(&str, OsString)]) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(dir);
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_NAMESPACE",
    ] {
        command.env_remove(key);
    }
    command.env("GIT_TERMINAL_PROMPT", "0");
    command.env("GIT_OPTIONAL_LOCKS", "0");
    for (key, value) in envs {
        command.env(key, value);
    }
    command
}

fn run_git(
    dir: &Path,
    args: &[&str],
    envs: &[(&str, OsString)],
    timeout: Duration,
) -> Result<GitOut> {
    let mut command = git_command(dir, envs);
    command.args(args);
    run_command(command, timeout, format!("git in {}", dir.display()))
}

fn run_command(mut cmd: Command, timeout: Duration, description: String) -> Result<GitOut> {
    let timeout = effective_timeout(timeout);
    if timeout.is_zero() {
        return Ok(GitOut {
            code: None,
            stdout: Vec::new(),
            stderr: b"refresh cycle deadline exceeded".to_vec(),
            timed_out: true,
        });
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    // Own process group, so descendants can be killed with the direct child.
    // The bounded drain below remains the fallback if group creation fails.
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                libc::setpgid(0, 0);
            }
            Ok(())
        });
    }

    let mut child = cmd
        .spawn()
        .map_err(|err| format!("failed to run {description}: {err}"))?;
    let child_pid = child.id();

    let mut out_pipe = child.stdout.take().expect("stdout piped");
    let mut err_pipe = child.stderr.take().expect("stderr piped");
    let (out_tx, out_rx) = mpsc::channel();
    let (err_tx, err_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out_pipe.read_to_end(&mut buf);
        let _ = out_tx.send(buf);
    });
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err_pipe.read_to_end(&mut buf);
        let _ = err_tx.send(buf);
    });

    let deadline = Instant::now() + timeout;
    let mut backoff = Duration::from_micros(200);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    kill_process_group(child_pid);
                    let _ = child.kill();
                    break child.wait()?;
                }
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_millis(5));
            }
        }
    };

    let mut stdout = out_rx.recv_timeout(PIPE_DRAIN_GRACE).ok();
    let mut stderr = err_rx.recv_timeout(PIPE_DRAIN_GRACE).ok();
    if stdout.is_none() || stderr.is_none() {
        kill_process_group(child_pid);
        if stdout.is_none() {
            stdout = out_rx.recv_timeout(PIPE_DRAIN_GRACE).ok();
        }
        if stderr.is_none() {
            stderr = err_rx.recv_timeout(PIPE_DRAIN_GRACE).ok();
        }
    }

    let drained = stdout.is_some() && stderr.is_some();
    Ok(GitOut {
        code: if drained { status.code() } else { None },
        stdout: stdout.unwrap_or_default(),
        stderr: stderr.unwrap_or_default(),
        timed_out: timed_out || !drained,
    })
}

/// Kills the process group led by `pid`.
///
/// The child called `setsid`, so its process-group id is its own pid and a
/// negative pid addresses the whole group. The `pid > 1` guard matters: `kill`
/// with a pgid of 0 means "everyone in *my* group", which would include this
/// process.
#[cfg(unix)]
fn kill_process_group(pid: u32) {
    if pid > 1 {
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {}

fn git(dir: &Path, args: &[&str], timeout: Duration) -> Result<GitOut> {
    run_git(dir, args, &[], timeout)
}

/// Runs a command that is expected to succeed, turning any other outcome into
/// an error that names the command.
fn git_ok(dir: &Path, args: &[&str], timeout: Duration) -> Result<GitOut> {
    let out = git(dir, args, timeout)?;
    if out.timed_out {
        return Err(format!("git {} timed out in {}", args.join(" "), dir.display()).into());
    }
    if !out.ok() {
        return Err(format!(
            "git {} failed in {}: {}",
            args.join(" "),
            dir.display(),
            out.stderr_text()
        )
        .into());
    }
    Ok(out)
}

fn path_exists_bounded(path: &Path, timeout: Duration) -> Result<bool> {
    let mut command = Command::new("test");
    command.arg("-e").arg(path);
    let output = run_command(
        command,
        timeout,
        format!("bounded existence check for {}", path.display()),
    )?;
    if output.timed_out {
        return Err(format!("existence check timed out for {}", path.display()).into());
    }
    match output.code {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(format!(
            "could not check whether {} exists: {}",
            path.display(),
            output.stderr_text()
        )
        .into()),
    }
}

fn copy_file_bounded(source: &Path, destination: &Path, timeout: Duration) -> Result<bool> {
    if !path_exists_bounded(source, timeout)? {
        return Ok(false);
    }
    let mut command = Command::new("cp");
    command.arg(source).arg(destination);
    let output = run_command(
        command,
        timeout,
        format!("copy {} to {}", source.display(), destination.display()),
    )?;
    if output.timed_out || !output.ok() {
        return Err(format!(
            "could not copy {} to {}: {}",
            source.display(),
            destination.display(),
            output.stderr_text()
        )
        .into());
    }
    Ok(true)
}

fn probe(dir: &Path, args: &[&str], timeout: Duration) -> Result<Option<String>> {
    probe_with_env(dir, args, &[], timeout)
}

/// Runs a probe whose only meaningful answers are "yes" (exit 0, with output)
/// and "no" (exit 1).
///
/// This exists because folding every other outcome into "no" is how a healthy
/// checkout gets diagnosed as having no commit. `symbolic-ref` and `rev-parse
/// --verify -q` both exit 1 for "that does not resolve" and 128 for "I could
/// not look" — and a command we killed on our own deadline has no exit code at
/// all. Only the first of those is an answer; the rest have to be errors, or a
/// stalled git silently removes a workspace from every pairing while telling
/// the user its branch has no commits.
fn probe_with_env(
    dir: &Path,
    args: &[&str],
    envs: &[(&str, OsString)],
    timeout: Duration,
) -> Result<Option<String>> {
    let out = run_git(dir, args, envs, timeout)?;
    if out.timed_out {
        return Err(format!("git {} timed out in {}", args.join(" "), dir.display()).into());
    }
    match out.code {
        Some(0) => Ok(Some(out.stdout_trimmed())),
        Some(1) => Ok(None),
        _ => Err(format!(
            "git {} could not answer in {}: {}",
            args.join(" "),
            dir.display(),
            out.stderr_text()
        )
        .into()),
    }
}

/// Converts a raw git path to a `String`, replacing anything that is not valid
/// UTF-8 **and** anything that would take control of a terminal, then making
/// the result unique.
///
/// Paths arrive from `-z` output as raw bytes and are drawn verbatim into a
/// pane that redraws in place. A filename containing `ESC [ 2 J` would clear
/// that pane; one containing a newline would escape the line budget entirely
/// and corrupt every row below it. Both are legal on disk.
///
/// Replacement alone is not enough, because it is not injective: `\xff.txt` and
/// `\xfe.txt` both become `<?>.txt`, and change sets are intersected by string,
/// so two worktrees holding two *different* files were reported as sharing one.
/// Appending a digest of the raw bytes restores injectivity in the direction
/// that matters — identical raw bytes always produce an identical string, so
/// `status` output and `merge-tree` output still match each other, while
/// different bytes stop merging into one phantom shared path.
///
/// Raw bytes stay beside this display value for status-driven snapshots and
/// line counts. The surrogate itself still cannot be handed back to tree
/// plumbing, so `--why` refuses it rather than guessing.
pub(crate) fn lossy(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    // `from_utf8_lossy` borrows iff the input was already valid UTF-8.
    let invalid_utf8 = matches!(text, Cow::Owned(_));
    if !invalid_utf8 && !text.chars().any(char::is_control) {
        return text.into_owned();
    }
    let replaced: String = text
        .chars()
        .map(|ch| {
            if ch.is_control() {
                char::REPLACEMENT_CHARACTER
            } else {
                ch
            }
        })
        .collect();
    format!("{replaced}~{:08x}", fnv1a(bytes))
}

/// Whether a path is the safe display surrogate produced by [`lossy`] rather
/// than an addressable tree-plumbing path.
fn is_lossy_display_path(path: &str) -> bool {
    let Some((prefix, digest)) = path.rsplit_once('~') else {
        return false;
    };
    prefix.contains(char::REPLACEMENT_CHARACTER)
        && digest.len() == 8
        && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// FNV-1a, 32-bit. Not a hash with any security property — it only has to be
/// deterministic and stable, which `DefaultHasher` explicitly is not.
fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in bytes {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

// ---------------------------------------------------------------------------
// Repo identity and HEAD state
// ---------------------------------------------------------------------------

/// Resolves a worktree top level to its per-worktree Git directory through a
/// bounded Git probe.
///
/// This deliberately asks for `--absolute-git-dir`, not the common dir. A main
/// worktree returns the common store itself while a linked worktree returns its
/// `<store>/worktrees/<name>` directory; preserving that asymmetry is what makes
/// exact main-worktree identification possible.
pub fn worktree_git_dir(top: &Path, timeout: Duration) -> Result<PathBuf> {
    Ok(PathBuf::from(
        git_ok(
            top,
            &["rev-parse", "--path-format=absolute", "--git-dir"],
            timeout,
        )?
        .stdout_trimmed(),
    ))
}

/// Absolute `--git-common-dir`: the only safe answer to "are these the same
/// repository?". Every worktree of one repo shares it, while each has its own
/// `--git-dir`.
///
/// Git resolves gitfiles and common-dir indirection itself. Keeping that work
/// inside the timed child avoids an unbounded `canonicalize` in the daemon.
pub fn repo_key(checkout: &Path, timeout: Duration) -> Result<RepoKey> {
    let out = git_ok(
        checkout,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        timeout,
    )?;
    Ok(RepoKey(out.stdout_trimmed()))
}
/// Resolves the checkout's working-tree root through the same bounded process
/// boundary as every other Git query.
///
/// A filesystem walk using `canonicalize` and `Path::exists` has no deadline:
/// on a stalled mount it can freeze the refresh loop after every Git child has
/// otherwise been bounded correctly. Git already owns top-level discovery, so
/// keep the answer and the timeout in one place.
pub fn work_tree_root(checkout: &Path, timeout: Duration) -> Result<PathBuf> {
    Ok(PathBuf::from(
        git_ok(
            checkout,
            &["rev-parse", "--path-format=absolute", "--show-toplevel"],
            timeout,
        )?
        .stdout_trimmed(),
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadState {
    /// HEAD is a symref to a branch that resolves to a commit.
    Branch { name: String, oid: String },
    /// Detached HEAD. Perfectly usable for analysis; we just use the raw OID.
    Detached { oid: String },
    /// HEAD names a branch that does not exist in the ref store, so this
    /// checkout has no commit.
    ///
    /// "Never had one" and "had one until someone deleted the branch" are the
    /// same observable state — see [`head_state`] — so both land here.
    Unborn { name: String },
    /// HEAD names a ref that *does* exist but does not yield a commit: it points
    /// at a missing object, or at an object that is not a commit. A genuinely
    /// broken repository rather than an empty one.
    BrokenHead { name: String },
}

impl HeadState {
    pub fn oid(&self) -> Option<&str> {
        match self {
            HeadState::Branch { oid, .. } | HeadState::Detached { oid } => Some(oid),
            _ => None,
        }
    }

    pub fn branch(&self) -> Option<&str> {
        match self {
            HeadState::Branch { name, .. } | HeadState::Unborn { name } => Some(name),
            HeadState::Detached { .. } | HeadState::BrokenHead { .. } => None,
        }
    }
}

/// Classifies HEAD, or fails if git could not tell us.
pub fn head_state(checkout: &Path, timeout: Duration) -> Result<HeadState> {
    head_state_with_env(checkout, &[], timeout)
}

/// Classifies HEAD using an explicit git environment, or fails if git could not
/// tell us.
///
/// Two earlier discriminators were wrong and both are worth recording, because
/// the plumbing notes asserted each of them as verified fact:
///
/// 1. `symbolic-ref -q HEAD` was said to exit 1 for an unborn branch and 0 for a
///    deleted one. It does not: git 2.53.0 exits 0 and prints the same ref name
///    in both cases.
/// 2. The worktree's `logs/HEAD` was then said to be the discriminator that
///    works — a worktree that ever had a commit checked out has one. It is wrong
///    in *both* directions, with a reproducer each way:
///    `git checkout --orphan fresh` in an existing worktree is genuinely unborn
///    and has a reflog; a branch deleted under a worktree with
///    `core.logAllRefUpdates=false` has none.
///
/// The honest conclusion is that "never had a commit" and "had one until the
/// branch was deleted" are the *same observable state*: HEAD is a symref to a
/// ref that is not in the ref store. Nothing in the ref store, the index or the
/// worktree distinguishes them, so this function stops pretending and reports
/// both as [`HeadState::Unborn`]. What the ref store *can* prove is the
/// genuinely broken case — the ref is there and still yields no commit — which
/// is what [`HeadState::BrokenHead`] now means.
///
/// Every probe is routed through [`probe_with_env`], so a git that could not
/// answer raises an error instead of being read as "no commit here".
fn head_state_with_env(
    checkout: &Path,
    envs: &[(&str, OsString)],
    timeout: Duration,
) -> Result<HeadState> {
    // The full ref name, not `--short`: the ref-store lookup below needs it, and
    // stripping `refs/heads/` gives the same short name a pane wants.
    let symref = probe_with_env(checkout, &["symbolic-ref", "-q", "HEAD"], envs, timeout)?;
    let oid = probe_with_env(
        checkout,
        &["rev-parse", "--verify", "-q", "HEAD^{commit}"],
        envs,
        timeout,
    )?;

    let short = |full: &str| full.strip_prefix("refs/heads/").unwrap_or(full).to_string();

    match (symref, oid) {
        (Some(full), Some(oid)) => Ok(HeadState::Branch {
            name: short(&full),
            oid,
        }),
        (None, Some(oid)) => Ok(HeadState::Detached { oid }),
        (Some(full), None) => {
            // `show-ref --verify` answers straight from the ref store: 0 the ref
            // is there, 1 it is not, 128 it is there but its object is not.
            let exists = run_git(
                checkout,
                &["show-ref", "--verify", "--quiet", &full],
                envs,
                timeout,
            )?;
            if exists.timed_out || exists.code.is_none() {
                return Err(format!(
                    "git show-ref could not answer for {} in {}: {}",
                    full,
                    checkout.display(),
                    exists.stderr_text()
                )
                .into());
            }
            let name = short(&full);
            if exists.code == Some(1) {
                Ok(HeadState::Unborn { name })
            } else {
                Ok(HeadState::BrokenHead { name })
            }
        }
        // HEAD is not a symref and does not resolve: detached at something that
        // is not there.
        (None, None) => Ok(HeadState::BrokenHead {
            name: "HEAD".to_string(),
        }),
    }
}

/// Branch name for a checkout, or `None` when HEAD is detached.
pub fn current_branch(checkout: &Path, timeout: Duration) -> Result<Option<String>> {
    match head_state(checkout, timeout)? {
        HeadState::Branch { name, .. } => Ok(Some(name)),
        // An unborn or deleted branch still has a name, and reporting it is
        // more useful than reporting nothing.
        HeadState::Unborn { name } | HeadState::BrokenHead { name } => Ok(Some(name)),
        HeadState::Detached { .. } => Ok(None),
    }
}

/// Best guess at the integration ref to diff against, since `Config` carries no
/// explicit one. First match wins; `Ok(None)` means the chain found nothing.
///
/// This used to fall back to `HEAD`, which is not a guess but a fabrication:
/// `HEAD...HEAD` is empty by construction, and because both the base ref and the
/// merge base then resolve, [`change_set`] recorded no degradation either. A
/// repository whose trunk is `develop` reported every workspace as clean, with
/// two agents committing conflicting edits to the same line. Callers with no
/// answer should pass [`NO_INTEGRATION_REF`] instead, which degrades visibly.
///
/// The chain is deliberately wider than the original six. `origin` first,
/// because a fork's local `main` is usually staler than what it was forked from;
/// then the conventional local trunks; then the recorded HEAD of any other
/// remote, which is what a repository with an `upstream` but no `origin` has;
/// then whatever `init.defaultBranch` says this user names their trunks.
pub fn integration_ref(checkout: &Path, timeout: Duration) -> Result<Option<String>> {
    let mut candidates: Vec<String> = [
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/main",
        "refs/remotes/origin/master",
        "refs/heads/main",
        "refs/heads/master",
        "refs/heads/trunk",
    ]
    .iter()
    .map(|c| c.to_string())
    .collect();

    if let Some(remotes) = probe(checkout, &["remote"], timeout)? {
        for remote in remotes.lines().map(str::trim).filter(|r| !r.is_empty()) {
            if remote != "origin" {
                candidates.push(format!("refs/remotes/{remote}/HEAD"));
            }
        }
    }
    if let Some(default) = probe(
        checkout,
        &["config", "--get", "init.defaultBranch"],
        timeout,
    )? {
        let default = default.trim();
        if !default.is_empty() {
            candidates.push(format!("refs/heads/{default}"));
        }
    }

    for candidate in candidates {
        let resolved = probe(
            checkout,
            &[
                "rev-parse",
                "--verify",
                "-q",
                &format!("{candidate}^{{commit}}"),
            ],
            timeout,
        )?;
        if resolved.is_some() {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
/// Parses the `--branch` headers from porcelain-v2 status.
pub fn parse_status_head(bytes: &[u8]) -> Option<HeadState> {
    let mut oid = None;
    let mut branch = None;
    for field in bytes.split(|byte| *byte == 0) {
        if let Some(value) = field.strip_prefix(b"# branch.oid ") {
            oid = Some(String::from_utf8_lossy(value).into_owned());
        } else if let Some(value) = field.strip_prefix(b"# branch.head ") {
            branch = Some(String::from_utf8_lossy(value).into_owned());
        }
    }
    match (oid.as_deref(), branch.as_deref()) {
        (Some("(initial)"), Some(name)) => Some(HeadState::Unborn {
            name: name.to_string(),
        }),
        (Some(oid), Some("(detached)")) => Some(HeadState::Detached {
            oid: oid.to_string(),
        }),
        (Some(oid), Some(name)) => Some(HeadState::Branch {
            name: name.to_string(),
            oid: oid.to_string(),
        }),
        _ => None,
    }
}

// Change sets
// ---------------------------------------------------------------------------

/// The three flags carried by a submodule's `S<c><m><u>` status field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubmoduleState {
    pub commit_changed: bool,
    pub modified_content: bool,
    pub untracked_content: bool,
}

/// One parsed `status --porcelain=v2` record, before it is folded into a
/// change set. Exposed so the parser can be tested against real git output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    pub path: String,
    pub raw_path: Vec<u8>,
    /// Populated only for rename/copy (`2`) records: the original path.
    pub origin: Option<String>,
    pub raw_origin: Option<Vec<u8>>,
    /// Present for `S<c><m><u>` records and absent for ordinary `N...` paths.
    pub submodule: Option<SubmoduleState>,
    pub kind: ChangeKind,
    pub is_rename: bool,
    /// Whether status proves that this path currently has hashable worktree
    /// content. Deletions and type changes are left to the general `add -A`
    /// pass instead of being probed with unbounded filesystem metadata.
    pub worktree_content: bool,
}

/// Parses `status --porcelain=v2 -z --untracked-files=all --renames` output.
///
/// Two things bite here:
///
/// * `-z` disables path quoting, so paths are raw bytes and may contain spaces
///   and newlines. Every field is therefore located by counting spaces up to a
///   fixed position, never by splitting on all whitespace.
/// * A `2` (rename/copy) record consumes **two** NUL-terminated fields: the new
///   path, then the original path as the very next field. A parser that treats
///   every NUL field as one record desynchronises from here on and mistakes the
///   original path for a status line. Both paths belong to the change set.
///
/// `!` (ignored) records are dropped: ignored files must never enter a change
/// set, or every build directory would collide with every other.
pub fn parse_status_v2(bytes: &[u8]) -> Vec<StatusEntry> {
    let fields: Vec<&[u8]> = bytes.split(|b| *b == 0).filter(|f| !f.is_empty()).collect();
    let mut entries = Vec::new();
    let mut i = 0;
    while i < fields.len() {
        let line = fields[i];
        match line[0] {
            // Header, e.g. `# branch.oid (initial)`.
            b'#' => i += 1,
            // ordinary: `1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>`
            b'1' => {
                if let (Some(path), Some(xy)) = (field_after_space(line, 8), xy_of(line)) {
                    entries.push(StatusEntry {
                        path: lossy(path),
                        raw_path: path.to_vec(),
                        origin: None,
                        raw_origin: None,
                        kind: kind_from_xy(xy),
                        submodule: submodule_state_of(line),
                        is_rename: false,
                        worktree_content: worktree_content_from_xy(xy),
                    });
                }
                i += 1;
            }
            // rename/copy: `2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <Xscore> <path>`
            // NUL `<origPath>`
            b'2' => {
                if let (Some(path), Some(xy)) = (field_after_space(line, 9), xy_of(line)) {
                    let raw_origin = fields.get(i + 1).map(|field| field.to_vec());
                    let origin = raw_origin.as_deref().map(lossy);
                    entries.push(StatusEntry {
                        path: lossy(path),
                        raw_path: path.to_vec(),
                        origin,
                        raw_origin,
                        kind: kind_from_xy(xy),
                        submodule: submodule_state_of(line),
                        is_rename: true,
                        worktree_content: worktree_content_from_xy(xy),
                    });
                }
                // Consume the trailing origin-path field along with the record.
                i += 2;
            }
            // unmerged: `u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>`
            b'u' => {
                if let Some(path) = field_after_space(line, 10) {
                    entries.push(StatusEntry {
                        path: lossy(path),
                        raw_path: path.to_vec(),
                        origin: None,
                        raw_origin: None,
                        kind: ChangeKind::Conflicted,
                        submodule: None,
                        is_rename: false,
                        worktree_content: true,
                    });
                }
                i += 1;
            }
            // untracked: `? <path>`
            b'?' => {
                if let Some(path) = field_after_space(line, 1) {
                    entries.push(StatusEntry {
                        path: lossy(path),
                        raw_path: path.to_vec(),
                        origin: None,
                        raw_origin: None,
                        kind: ChangeKind::Untracked,
                        submodule: None,
                        is_rename: false,
                        worktree_content: false,
                    });
                }
                i += 1;
            }
            // ignored: `! <path>` — deliberately dropped.
            b'!' => i += 1,
            _ => i += 1,
        }
    }
    entries
}

fn changed_index_paths(entries: &[StatusEntry]) -> Vec<&[u8]> {
    let mut paths = BTreeSet::new();
    for entry in entries {
        // The forced content pass has nothing to do for an untracked path or a
        // gitlink. Deletions and type changes are recorded by the general
        // `add -A` pass; forcing them through `--renormalize` fails.
        if entry.kind == ChangeKind::Untracked
            || entry.submodule.is_some()
            || !entry.worktree_content
        {
            continue;
        }
        paths.insert(entry.raw_path.as_slice());
    }
    paths.into_iter().collect()
}

fn snapshot_path_still_hashable(
    checkout: &Path,
    raw_path: &[u8],
    timeout: Duration,
) -> Result<bool> {
    let mut command = git_command(checkout, &[]);
    command.args([
        "--no-optional-locks",
        "--literal-pathspecs",
        "status",
        "--porcelain=v2",
        "-z",
        "--untracked-files=no",
        "--",
    ]);
    command.arg(OsString::from_vec(raw_path.to_vec()));
    let status = run_command(
        command,
        timeout,
        format!("git status path probe in {}", checkout.display()),
    )?;
    if status.timed_out || !status.ok() {
        return Err(format!(
            "git status path probe failed in {}: {}",
            checkout.display(),
            status.stderr_text()
        )
        .into());
    }
    Ok(parse_status_v2(&status.stdout)
        .into_iter()
        .any(|entry| entry.raw_path == raw_path && entry.worktree_content))
}

/// Returns everything after the `n`th ASCII space, so a path containing spaces
/// survives intact.
fn field_after_space(line: &[u8], n: usize) -> Option<&[u8]> {
    let mut seen = 0;
    for (i, b) in line.iter().enumerate() {
        if *b == b' ' {
            seen += 1;
            if seen == n {
                return if i + 1 < line.len() {
                    Some(&line[i + 1..])
                } else {
                    None
                };
            }
        }
    }
    None
}

/// The `XY` pair sits at bytes 2..4 of every `1`, `2` and `u` record.
fn xy_of(line: &[u8]) -> Option<(u8, u8)> {
    if line.len() >= 4 && line[1] == b' ' {
        Some((line[2], line[3]))
    } else {
        None
    }
}

/// Parses the fixed-position `<sub>` field. `N...` means an ordinary path;
/// `S<c><m><u>` carries the recorded-commit, modified-content and
/// untracked-content flags in that order.
fn submodule_state_of(line: &[u8]) -> Option<SubmoduleState> {
    let field = line.split(|b| *b == b' ').nth(2)?;
    match field {
        [b'S', commit, modified, untracked] => Some(SubmoduleState {
            commit_changed: *commit == b'C',
            modified_content: *modified == b'M',
            untracked_content: *untracked == b'U',
        }),
        _ => None,
    }
}

fn worktree_content_from_xy((index, worktree): (u8, u8)) -> bool {
    !matches!(index, b'D' | b'T') && !matches!(worktree, b'D' | b'T')
}

/// `X` is the index status, `Y` the worktree status. An unstaged edit is the
/// more significant fact when a path is both staged and dirty, and
/// `ChangeKind`'s ordering already encodes that.
fn kind_from_xy(xy: (u8, u8)) -> ChangeKind {
    let (x, y) = xy;
    if x == b'U' || y == b'U' {
        ChangeKind::Conflicted
    } else if y != b'.' {
        ChangeKind::Unstaged
    } else {
        ChangeKind::Staged
    }
}

/// One `--numstat -z` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumStat {
    pub added: u64,
    pub removed: u64,
    pub paths: Vec<String>,
}

/// Parses `diff --numstat -z`.
///
/// Framing: `<added> TAB <removed> TAB <path> NUL` normally, but for a rename
/// the third tab-field is empty and the old and new paths follow as two
/// separate NUL fields. Binary files report `-` for both counts.
pub fn parse_numstat_z(bytes: &[u8]) -> Vec<NumStat> {
    let fields: Vec<&[u8]> = bytes.split(|b| *b == 0).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < fields.len() {
        let field = fields[i];
        if field.is_empty() {
            i += 1;
            continue;
        }
        let mut parts = field.splitn(3, |b| *b == b'\t');
        let added = parts.next().unwrap_or(b"");
        let removed = match parts.next() {
            Some(r) => r,
            None => {
                i += 1;
                continue;
            }
        };
        let rest = parts.next().unwrap_or(b"");
        let added = count_of(added);
        let removed = count_of(removed);
        if rest.is_empty() {
            // Rename or copy: the two paths are the next two NUL fields.
            let mut paths = Vec::new();
            if let Some(old) = fields.get(i + 1) {
                paths.push(lossy(old));
            }
            if let Some(new) = fields.get(i + 2) {
                paths.push(lossy(new));
            }
            out.push(NumStat {
                added,
                removed,
                paths,
            });
            i += 3;
        } else {
            out.push(NumStat {
                added,
                removed,
                paths: vec![lossy(rest)],
            });
            i += 1;
        }
    }
    out
}

fn count_of(field: &[u8]) -> u64 {
    // `-` marks a binary file; it contributes no line counts.
    std::str::from_utf8(field)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// `-c` overrides that stop git running the repository's content filters.
///
/// Two commands here would otherwise run them: the snapshot's `git add`, and
/// `git diff --numstat HEAD`, which cleans the working-tree side before
/// comparing it. Both were measured invoking a `filter.<driver>.clean` on git
/// 2.53.0; `status --porcelain=v2` was measured *not* to.
///
/// Those filters are arbitrary user programs, and the one everybody has is
/// git-lfs, whose clean filter writes into the user's own `.git/lfs/objects`
/// every time it runs. A tool whose whole claim is that it changes nothing in
/// the repository cannot execute them on every refresh cycle. With these
/// overrides the filter is not invoked at all — and `.required` has to be
/// overridden alongside `clean` and `process`, or git refuses with
/// `fatal: <path>: clean filter '<driver>' failed`. `required = true` is
/// git-lfs's default, so without that third override this fix would do nothing
/// for the case that motivates it.
///
/// Two consequences, both deliberate and both recorded in docs/git-plumbing.md:
///
/// * The snapshot tree holds the working tree's **raw bytes** for any filtered
///   path `add` had to re-hash — for LFS, the media rather than a pointer —
///   while anything the seeded index still considers clean keeps its existing
///   filtered blob. Conflict prediction only ever compares one snapshot tree
///   against another or against a commit tree, so a path both sides changed
///   differently still differs and a path only one side changed still merges.
/// * Line counts for a filtered path measure the unfiltered bytes. For LFS that
///   changes nothing (binary diffs report `-` and count zero either way); for a
///   text-transforming filter it overstates that path's volume, which the
///   runaway thresholds treat as an order-of-magnitude signal anyway.
#[derive(Default)]
struct RepositoryOverrides {
    filter_args: Vec<String>,
    custom_merge_drivers: Vec<String>,
}

fn repository_overrides_with_env(
    checkout: &Path,
    envs: &[(&str, OsString)],
    timeout: Duration,
) -> RepositoryOverrides {
    let Ok(output) = run_git(
        checkout,
        &[
            "config",
            "--name-only",
            "--get-regexp",
            "^(filter\\.|merge\\..*\\.driver$)",
        ],
        envs,
        timeout,
    ) else {
        return RepositoryOverrides::default();
    };
    if !output.ok() {
        return RepositoryOverrides::default();
    }
    let listing = String::from_utf8_lossy(&output.stdout);
    let mut filter_drivers = BTreeSet::new();
    let mut merge_drivers = BTreeSet::new();
    for key in listing.lines().map(str::trim) {
        if let Some(rest) = key.strip_prefix("filter.") {
            for suffix in [".clean", ".smudge", ".process", ".required"] {
                if let Some(name) = rest.strip_suffix(suffix) {
                    if !name.is_empty() {
                        filter_drivers.insert(name.to_string());
                    }
                    break;
                }
            }
        } else if let Some(name) = key
            .strip_prefix("merge.")
            .and_then(|rest| rest.strip_suffix(".driver"))
            .filter(|name| !name.is_empty())
        {
            merge_drivers.insert(name.to_string());
        }
    }
    let mut filter_args = Vec::new();
    for driver in filter_drivers {
        for key in ["clean", "process"] {
            filter_args.push("-c".to_string());
            filter_args.push(format!("filter.{driver}.{key}="));
        }
        filter_args.push("-c".to_string());
        filter_args.push(format!("filter.{driver}.required=false"));
    }
    RepositoryOverrides {
        filter_args,
        custom_merge_drivers: merge_drivers.into_iter().collect(),
    }
}

fn filter_overrides_with_env(
    checkout: &Path,
    envs: &[(&str, OsString)],
    timeout: Duration,
) -> Vec<String> {
    repository_overrides_with_env(checkout, envs, timeout).filter_args
}
/// Everything `checkout` has changed relative to its merge base with `base`:
/// staged, unstaged, untracked, conflicted, and committed-since-merge-base.
///
/// Degrades rather than failing for detached HEAD, an unborn branch, a deleted
/// branch, or a HEAD git could not read at all — the returned `ChangeSet`
/// carries `degraded` in those cases.
///
/// Pass [`NO_INTEGRATION_REF`] as `base` when there is no integration ref to
/// measure against; the result is a visibly degraded change set rather than a
/// silently empty one.
/// One checkout read, including the top level Git resolved while assembling
/// untracked-file volume. The gathering layer reuses this answer for same-tree
/// suppression instead of walking the filesystem a second time without a
/// deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeSetRead {
    pub change_set: ChangeSet,
    pub top_level: PathBuf,
    pub head: HeadState,
    pub status_entries: Vec<StatusEntry>,
    pub filter_overrides: Vec<String>,
    pub custom_merge_drivers: Vec<String>,
    pub target_oid: Option<String>,
}

/// Compatibility wrapper for callers that only need the change set.
pub fn change_set(checkout: &Path, base: &str, timeout: Duration) -> Result<ChangeSet> {
    Ok(read_change_set(checkout, base, timeout)?.change_set)
}

pub fn read_change_set(checkout: &Path, base: &str, timeout: Duration) -> Result<ChangeSetRead> {
    read_change_set_inner(checkout, base, timeout, true)
}

fn read_change_set_inner(
    checkout: &Path,
    base: &str,
    timeout: Duration,
    include_submodule_volume: bool,
) -> Result<ChangeSetRead> {
    let mut kinds: BTreeMap<String, ChangeKind> = BTreeMap::new();
    // Line volume is attributed per path so an ignored path takes its lines
    // with it rather than leaving them to trip the runaway threshold alone.
    let mut volume: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    // The *original* half of a rename. It belongs in the change set — a
    // sibling editing the old name really does collide — but one rename is one
    // changed file, so this half must not count twice toward `runaway_files`.
    let mut rename_origins: BTreeSet<String> = BTreeSet::new();
    let mut uncomparable_submodules: BTreeSet<String> = BTreeSet::new();
    let mut nested_changed_files: BTreeMap<String, usize> = BTreeMap::new();
    let mut set = ChangeSet::default();
    let mut reasons: Vec<String> = Vec::new();

    // `--no-optional-locks` is mandatory: a plain `status` takes
    // `<gitdir>/index.lock` to write back its stat cache, which would contend
    // with the very agent we are watching.
    let status = git_ok(
        checkout,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--renames",
            "--branch",
        ],
        timeout,
    )?;
    let entries = parse_status_v2(&status.stdout);
    let mut unmerged = false;
    for entry in &entries {
        if entry.kind == ChangeKind::Conflicted {
            unmerged = true;
        }
        if entry.is_rename {
            set.has_rename = true;
        }
        if entry
            .submodule
            .is_some_and(|sub| sub.modified_content || sub.untracked_content)
        {
            // The snapshot records only the committed gitlink, so content that
            // exists below it never reaches merge-tree and cannot be judged.
            uncomparable_submodules.insert(entry.path.clone());
        }
        note(&mut kinds, entry.path.clone(), entry.kind);
        if let Some(origin) = &entry.origin {
            // Both halves of a rename belong to the change set: another
            // worktree editing the original path collides with this one.
            note(&mut kinds, origin.clone(), entry.kind);
            rename_origins.insert(origin.clone());
        }
    }
    if unmerged {
        reasons.push(format!(
            "{DEGRADED_UNMERGED}: a merge is in progress, predictions are advisory"
        ));
    }

    let head = parse_status_head(&status.stdout).ok_or_else(|| {
        format!(
            "git status returned no branch headers in {}",
            checkout.display()
        )
    })?;
    match &head {
        HeadState::Unborn { name } => {
            reasons.push(format!(
                "{DEGRADED_UNBORN}: `{name}` does not exist, so this checkout has no commit"
            ));
        }
        HeadState::BrokenHead { name } => {
            reasons.push(format!(
                "{DEGRADED_BROKEN_HEAD}: `{name}` does not resolve to a commit"
            ));
        }
        _ => {}
    }
    // Every command below that hashes working-tree bytes receives the same
    // content-filter neutralization, including the no-index reader for
    // untracked files.
    let repository_overrides = repository_overrides_with_env(checkout, &[], timeout);
    let filter_overrides = repository_overrides.filter_args;
    let custom_merge_drivers = repository_overrides.custom_merge_drivers;

    let mut target_oid = None;
    if let Some(head_oid) = head.oid() {
        // Dirty-side line volume: everything between HEAD and the working tree.
        // This is the second command that would run the repository's content
        // filters, so it gets the same overrides as the snapshot.
        let mut dirty_args: Vec<&str> = filter_overrides.iter().map(String::as_str).collect();
        dirty_args.extend(["diff", "--numstat", "-z", "HEAD"]);
        let dirty = git(checkout, &dirty_args, timeout)?;
        if dirty.ok() {
            for stat in parse_numstat_z(&dirty.stdout) {
                add_volume(&mut volume, &stat);
            }
        } else {
            // Losing the dirty-side volume silently would understate a runaway
            // by exactly the work that is still uncommitted, which is the work
            // a runaway agent has most of.
            reasons.push(format!(
                "{DEGRADED_PARTIAL_VOLUME}: could not measure uncommitted line counts: {}",
                dirty.stderr_text()
            ));
        }

        // `NO_INTEGRATION_REF` is not a ref and must never be handed to git; it
        // is the caller saying "the probe chain found nothing", which is a
        // missing base ref by another route.
        let base_oid = if base == NO_INTEGRATION_REF {
            None
        } else {
            probe(
                checkout,
                &["rev-parse", "--verify", "-q", &format!("{base}^{{commit}}")],
                timeout,
            )?
        };
        if base_oid.is_none() {
            reasons.push(if base == NO_INTEGRATION_REF {
                format!(
                    "{DEGRADED_MISSING_BASE_REF}: no integration ref found, \
                     so only uncommitted work is counted"
                )
            } else {
                format!("{DEGRADED_MISSING_BASE_REF}: `{base}` does not resolve")
            });
        }
        target_oid = base_oid.clone();
        if let Some(base_oid) = base_oid {
            let merge_base = git(checkout, &["merge-base", &base_oid, head_oid], timeout)?;
            if !merge_base.ok() {
                reasons.push(format!(
                    "{DEGRADED_NO_MERGE_BASE}: no common ancestor with `{base}`"
                ));
            } else {
                // The three-dot form already means "from the merge base to
                // HEAD", so no second merge-base round trip is needed.
                let range = format!("{base_oid}...HEAD");
                let names = git_ok(checkout, &["diff", "--name-only", "-z", &range], timeout)?;
                for path in names.stdout.split(|b| *b == 0).filter(|f| !f.is_empty()) {
                    note(&mut kinds, lossy(path), ChangeKind::Committed);
                }
                let stats = git(checkout, &["diff", "--numstat", "-z", &range], timeout)?;
                if stats.ok() {
                    for stat in parse_numstat_z(&stats.stdout) {
                        // `--name-only` collapses a rename to the new path
                        // only; `--numstat` reports both, so this is where the
                        // pre-rename path enters the committed change set.
                        if stat.paths.len() > 1 {
                            set.has_rename = true;
                            // `parse_numstat_z` yields the old path first.
                            rename_origins.insert(stat.paths[0].clone());
                        }
                        for path in &stat.paths {
                            note(&mut kinds, path.clone(), ChangeKind::Committed);
                        }
                        add_volume(&mut volume, &stat);
                    }
                } else {
                    reasons.push(format!(
                        "{DEGRADED_PARTIAL_VOLUME}: could not measure committed line counts: {}",
                        stats.stderr_text()
                    ));
                }
            }
        }
    }

    // `status --porcelain` always reports paths relative to the repository
    // *root*, never to git's working directory, so a `checkout` that points at a
    // subdirectory would turn every disk read into `<root>/pkg/pkg/file` and
    // silently count zero lines. Resolve the top level once and join against it.
    let top_level = work_tree_root(checkout, timeout)?;

    // Untracked files are invisible to every repository diff. Measure their
    // line counts through a bounded `git diff --no-index` child rather than
    // opening repository files in the daemon process, where a slow mount has no
    // deadline.
    let mut count_path = |entry: &StatusEntry| match count_lines_with_git(
        &top_level,
        &entry.raw_path,
        &filter_overrides,
        timeout,
    ) {
        Ok(lines) => {
            volume.entry(entry.path.clone()).or_default().0 += lines;
        }
        Err(err) => reasons.push(format!(
            "{DEGRADED_PARTIAL_VOLUME}: could not measure `{}` line count: {err}",
            entry.path
        )),
    };
    for entry in &entries {
        if entry.kind == ChangeKind::Untracked {
            count_path(entry);
        }
    }
    // With no usable HEAD there is nothing to diff against, so staged and
    // unstaged additions use the same bounded path.
    if head.oid().is_none() {
        for entry in &entries {
            if entry.kind == ChangeKind::Staged || entry.kind == ChangeKind::Unstaged {
                count_path(entry);
            }
        }
    }

    if include_submodule_volume {
        for entry in &entries {
            let dirty_contents = entry
                .submodule
                .is_some_and(|submodule| submodule.modified_content || submodule.untracked_content);
            if !dirty_contents {
                continue;
            }
            let nested_checkout = top_level.join(OsString::from_vec(entry.raw_path.clone()));
            match read_change_set_inner(&nested_checkout, "HEAD", timeout, false) {
                Ok(nested) => {
                    let nested_set = nested.change_set;
                    let nested_files = nested_set
                        .paths
                        .iter()
                        .filter(|path| !path.is_rename_origin)
                        .count();
                    let nested_volume = volume.entry(entry.path.clone()).or_default();
                    nested_volume.0 = nested_volume.0.saturating_add(nested_set.lines_added);
                    nested_volume.1 = nested_volume.1.saturating_add(nested_set.lines_removed);
                    nested_changed_files.insert(entry.path.clone(), nested_files);
                }
                Err(err) => reasons.push(format!(
                    "{DEGRADED_PARTIAL_VOLUME}: could not measure submodule `{}` volume: {err}",
                    entry.path
                )),
            }
        }
    }

    set.paths = kinds
        .into_iter()
        .map(|(path, kind)| {
            let (added, removed) = volume.get(&path).copied().unwrap_or((0, 0));
            let nested_files = nested_changed_files.get(&path).copied().unwrap_or(0);
            ChangedPath {
                is_rename_origin: rename_origins.contains(&path),
                submodule_contents_uncomparable: uncomparable_submodules.contains(&path),
                path,
                kind,
                lines_added: added,
                lines_removed: removed,
                nested_changed_files: nested_files,
            }
        })
        .collect();
    // The totals are the sum of the parts, so a caller that filters paths and
    // one that reads the totals can never disagree.
    set.lines_added = set.paths.iter().map(|p| p.lines_added).sum();
    set.lines_removed = set.paths.iter().map(|p| p.lines_removed).sum();
    if !reasons.is_empty() {
        set.degraded = true;
        set.degraded_reason = Some(reasons.join("; "));
    }
    Ok(ChangeSetRead {
        change_set: set,
        top_level,
        head,
        status_entries: entries,
        custom_merge_drivers,
        filter_overrides,
        target_oid,
    })
}

/// Attributes one `--numstat` record's line counts to the path it describes.
///
/// A rename record carries two paths; the counts describe the single file that
/// moved, so they are attributed to the new path only. Charging both halves
/// would double a refactor's apparent volume.
fn add_volume(volume: &mut BTreeMap<String, (u64, u64)>, stat: &NumStat) {
    let Some(path) = stat.paths.last() else {
        return;
    };
    let entry = volume.entry(path.clone()).or_default();
    entry.0 = entry.0.saturating_add(stat.added);
    entry.1 = entry.1.saturating_add(stat.removed);
}

/// Keeps the most significant reason a path is in the change set.
fn note(kinds: &mut BTreeMap<String, ChangeKind>, path: String, kind: ChangeKind) {
    kinds
        .entry(path)
        .and_modify(|existing| {
            if kind > *existing {
                *existing = kind;
            }
        })
        .or_insert(kind);
}

fn count_lines_with_git(
    top_level: &Path,
    raw_path: &[u8],
    filter_overrides: &[String],
    timeout: Duration,
) -> std::result::Result<u64, String> {
    let mut command = git_command(top_level, &[]);
    command.args(filter_overrides);
    command.args(["diff", "--no-index", "--numstat", "-z", "--", "/dev/null"]);
    command.arg(OsString::from_vec(raw_path.to_vec()));
    let output = run_command(
        command,
        timeout,
        format!("bounded file read in {}", top_level.display()),
    )
    .map_err(|err| err.to_string())?;
    if output.timed_out || output.code.is_none() {
        return Err("bounded file read timed out".to_string());
    }
    if !matches!(output.code, Some(0 | 1)) {
        return Err(output.stderr_text());
    }
    Ok(parse_numstat_z(&output.stdout)
        .into_iter()
        .fold(0u64, |total, stat| total.saturating_add(stat.added)))
}

// ---------------------------------------------------------------------------
// Conflict prediction
// ---------------------------------------------------------------------------

/// Maximum conflicted blob size `--why` will read into memory.
///
/// Eight MiB is deliberately generous beside the 200-line, 160-column display
/// limit, while keeping one pathological generated file from consuming the
/// memory of the editor process this plugin runs alongside.
pub const WHY_BLOB_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// Outcome of predicting one checkout against its integration target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetMergeOutcome {
    /// The histories are unrelated, so no merge verdict exists.
    NoCommonAncestor,
    /// Git produced a verdict, together with any qualifications on that claim.
    Predicted {
        conflicts: bool,
        approximate: bool,
        advisory: bool,
    },
}

/// Verdict for one pair of checkouts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PairPrediction {
    /// One entry per requested path, plus any conflicted path git reported that
    /// was not requested (a rename can conflict on a path neither change set
    /// listed under the same name).
    pub verdicts: Vec<(String, bool)>,
    /// Paths git itself named as conflicted, before the pair-level fallback
    /// marks requested paths conflicted for a directory-rename conflict.
    pub conflicted_paths: Vec<String>,
    /// Tree written by the exact merge that produced these verdicts. Its
    /// objects live in the predictor's redirected object store.
    pub merged_tree: String,
    /// Machine-stable conflict-type tokens keyed by every path in the message
    /// record that reported them. The association matters: assigning a
    /// pair-level rename token to an unrelated conflicted path would claim a
    /// cause git did not name.
    pub conflict_types_by_path: BTreeMap<String, Vec<String>>,
    /// True when a single merge base had to be forced although more than one
    /// exists, so the answer is an approximation of what a real merge would do.
    pub approximate: bool,
    /// True when one side has a merge in progress: its snapshot contains
    /// conflict markers, so the prediction is advisory only.
    pub advisory: bool,
    /// Which side made [`Self::advisory`] true, retained so explanations can
    /// name the worktree whose own conflict markers are in the snapshot.
    pub left_advisory: bool,
    pub right_advisory: bool,
    /// merge-tree's exit status for the pair as a whole. Authoritative:
    /// git documents that a merge can conflict without any individual file
    /// appearing in the conflicted-file list.
    pub pair_conflict: bool,
    /// Machine-stable conflict-type tokens git reported, e.g.
    /// `CONFLICT (contents)`. Never parse the human prose instead.
    pub conflict_types: Vec<String>,
    /// Direct-submodule comparisons which refine otherwise uncomparable
    /// superproject gitlink paths.
    pub submodules: Vec<SubmodulePrediction>,
}

/// Result of comparing one direct submodule as a repository in its own right.
///
/// This is deliberately depth one: a nested checkout's own submodules remain
/// gitlinks in its snapshot and are never opened or recursively compared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmodulePrediction {
    /// Superproject-relative gitlink path.
    pub path: String,
    /// `Some(true)` is a nested conflict, `Some(false)` a clean nested merge,
    /// and `None` means the comparison could not be completed honestly.
    pub conflict: Option<bool>,
    /// Paths named by nested merge-tree, relative to the submodule checkout.
    pub conflicting_paths: Vec<String>,
    /// Why an unavailable comparison stayed unknown.
    pub reason: Option<String>,
    /// True when the nested merge had multiple merge bases and one had to be
    /// forced, so its verdict is an approximation of a real nested merge.
    pub approximate: bool,
}

fn nested_repository_dirs(checkout: &Path, timeout: Duration) -> Result<(PathBuf, PathBuf)> {
    let git_dir = PathBuf::from(
        git_ok(
            checkout,
            &["rev-parse", "--path-format=absolute", "--git-dir"],
            timeout,
        )?
        .stdout_trimmed(),
    );
    let common_dir = PathBuf::from(
        git_ok(
            checkout,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            timeout,
        )?
        .stdout_trimmed(),
    );
    Ok((git_dir, common_dir))
}

/// One checkout, reduced to what merge-tree needs.
#[derive(Debug, Clone)]
struct Side {
    common_dir: PathBuf,
    head: String,
    /// Snapshot tree of the working tree, present only when the checkout is
    /// dirty. A clean checkout is represented by its commit instead, which lets
    /// merge-tree recurse over multiple merge bases.
    tree: Option<String>,
    dirty: bool,
    unmerged: bool,
    custom_merge_drivers: Vec<String>,
}

#[derive(Debug, Clone)]
struct NestedSide {
    checkout: PathBuf,
    odb: PathBuf,
    side: Side,
}

#[derive(Debug, Clone)]
enum CachedNestedSide {
    Ready(NestedSide),
    Unavailable(String),
}

impl Side {
    /// The argument to hand merge-tree in tree mode.
    fn tree_ish(&self) -> String {
        match &self.tree {
            Some(tree) => tree.clone(),
            None => format!("{}^{{tree}}", self.head),
        }
    }
}

pub struct PreparedSideInput<'a> {
    pub checkout: &'a Path,
    pub common_dir: &'a Path,
    pub git_dir: &'a Path,
    pub read: &'a ChangeSetRead,
}

struct SnapshotSource<'a> {
    git_dir: &'a Path,
    odb: &'a Path,
    index_prefix: &'a str,
    base_env: &'a [(&'a str, OsString)],
    overrides: &'a [String],
}

static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

/// Holds the per-cycle state that makes prediction cheap: one outer scratch
/// object directory, one snapshot per worktree, and cached direct-submodule
/// repository state. Independent outer and nested snapshots are primed in
/// bounded workers; prediction only reads the completed maps.
pub struct Predictor {
    timeout: Duration,
    scratch: PathBuf,
    odb: PathBuf,
    sides: HashMap<PathBuf, Side>,
    targets: HashMap<(PathBuf, String), Side>,
    nested_sides: HashMap<(PathBuf, String), CachedNestedSide>,
}

impl Predictor {
    pub fn new(timeout: Duration) -> Result<Self> {
        let seq = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
        let name = format!("collide-{}-{seq}", std::process::id());
        let root = config::state_dir().join("scratch");
        let scratch = if fs::create_dir_all(&root).is_ok() {
            root.join(&name)
        } else {
            std::env::temp_dir().join(&name)
        };
        let odb = scratch.join("odb");
        // git creates fanout directories itself but expects these two.
        fs::create_dir_all(odb.join("pack"))?;
        fs::create_dir_all(odb.join("info"))?;
        Ok(Self {
            timeout,
            scratch,
            odb,
            sides: HashMap::new(),
            targets: HashMap::new(),
            nested_sides: HashMap::new(),
        })
    }

    /// Directory holding this run's temp indexes and redirected objects. It is
    /// removed when the `Predictor` is dropped.
    pub fn scratch_dir(&self) -> &Path {
        &self.scratch
    }

    /// Object-store redirection for any command that may write objects. Writes
    /// land in the scratch ODB; reads still see the real one through the
    /// alternates list, so the user's repository never grows.
    fn odb_env(&self, common_dir: &Path) -> Vec<(&'static str, OsString)> {
        vec![
            ("GIT_OBJECT_DIRECTORY", self.odb.clone().into_os_string()),
            (
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                common_dir.join("objects").into_os_string(),
            ),
        ]
    }

    /// Resolves and caches one checkout. Call this for every checkout before
    /// predicting, so the (single-threaded, index-copying) snapshot work
    /// happens once per worktree rather than once per pair.
    pub fn prime(&mut self, checkout: &Path) -> Result<()> {
        let key = canonical(checkout);
        if self.sides.contains_key(&key) {
            return Ok(());
        }
        let side = self.build_side(checkout)?;
        self.sides.insert(key, side);
        Ok(())
    }

    pub fn prime_prepared(&mut self, input: &PreparedSideInput<'_>) -> Result<()> {
        let key = canonical(input.checkout);
        if self.sides.contains_key(&key) {
            return Ok(());
        }
        let side = self.build_side_prepared(input)?;
        self.sides.insert(key, side);
        Ok(())
    }

    pub fn prime_target_oid(&mut self, common_dir: &Path, target_ref: &str, oid: &str) {
        let key = (common_dir.to_path_buf(), target_ref.to_string());
        self.targets.entry(key).or_insert_with(|| Side {
            common_dir: common_dir.to_path_buf(),
            head: oid.to_string(),
            tree: None,
            dirty: false,
            unmerged: false,
            custom_merge_drivers: Vec::new(),
        });
    }

    pub fn prime_prepared_all(
        &mut self,
        inputs: &[PreparedSideInput<'_>],
    ) -> BTreeMap<PathBuf, String> {
        let mut seen = BTreeSet::new();
        let pending: Vec<&PreparedSideInput<'_>> = inputs
            .iter()
            .filter(|input| {
                let key = canonical(input.checkout);
                !self.sides.contains_key(&key) && seen.insert(key)
            })
            .collect();
        if pending.is_empty() {
            return BTreeMap::new();
        }
        let workers = std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1)
            .clamp(1, 8)
            .min(pending.len());
        let chunk_size = pending.len().div_ceil(workers);
        let deadline = current_cycle_deadline();
        let predictor: &Predictor = self;
        let mut results = Vec::with_capacity(pending.len());
        std::thread::scope(|scope| {
            let handles: Vec<_> = pending
                .chunks(chunk_size)
                .map(|chunk| {
                    (
                        chunk,
                        scope.spawn(move || {
                            with_cycle_deadline(deadline, || {
                                chunk
                                    .iter()
                                    .map(|input| {
                                        (
                                            canonical(input.checkout),
                                            predictor
                                                .build_side_prepared(input)
                                                .map_err(|err| err.to_string()),
                                        )
                                    })
                                    .collect::<Vec<_>>()
                            })
                        }),
                    )
                })
                .collect();
            for (chunk, handle) in handles {
                match handle.join() {
                    Ok(chunk_results) => results.extend(chunk_results),
                    Err(_) => results.extend(chunk.iter().map(|input| {
                        (
                            canonical(input.checkout),
                            Err("checkout snapshot worker panicked".to_string()),
                        )
                    })),
                }
            }
        });
        let mut errors = BTreeMap::new();
        for (key, result) in results {
            match result {
                Ok(side) => {
                    self.sides.insert(key, side);
                }
                Err(err) => {
                    errors.insert(key, err);
                }
            }
        }
        errors
    }

    /// Resolves one local integration ref for the one-shot predictor API.
    /// The normal gather path reuses the OID already retained by change-set
    /// collection through [`Self::prime_target_oid`].
    pub fn prime_target(&mut self, checkout: &Path, target_ref: &str) -> Result<()> {
        let common_dir = self.side(checkout)?.common_dir.clone();
        let key = (common_dir.clone(), target_ref.to_string());
        if self.targets.contains_key(&key) {
            return Ok(());
        }

        let peeled = format!("{target_ref}^{{commit}}");
        let resolved = git(
            checkout,
            &["rev-parse", "--verify", "-q", &peeled],
            self.timeout,
        )?;
        if resolved.timed_out || resolved.code.is_none() {
            return Err(format!("resolving integration ref `{target_ref}` timed out").into());
        }
        if !resolved.ok() || resolved.stdout_trimmed().is_empty() {
            return Err(
                format!("integration ref `{target_ref}` does not resolve to a commit").into(),
            );
        }

        self.targets.insert(
            key,
            Side {
                common_dir,
                head: resolved.stdout_trimmed(),
                tree: None,
                dirty: false,
                unmerged: false,
                custom_merge_drivers: Vec::new(),
            },
        );
        Ok(())
    }

    /// Resolves and caches one direct submodule checkout before prediction
    /// fans out. Failures are cached too, because retrying from worker threads
    /// would both mutate this map and turn one bounded failure into many.
    ///
    /// Only `checkout/path` is opened. A submodule below that repository is
    /// intentionally left as a gitlink, so comparison depth is exactly one.
    pub fn prime_submodule(&mut self, checkout: &Path, path: &str) {
        let key = (canonical(checkout), path.to_string());
        if self.nested_sides.contains_key(&key) {
            return;
        }
        let nested_checkout = canonical(&checkout.join(path));
        let cached = match self.build_nested_side(&nested_checkout) {
            Ok(side) => CachedNestedSide::Ready(side),
            Err(err) => CachedNestedSide::Unavailable(err.to_string()),
        };
        self.nested_sides.insert(key, cached);
    }

    fn build_nested_side(&self, checkout: &Path) -> Result<NestedSide> {
        // Bootstrap the repository paths from `.git` so even the authoritative
        // rev-parse below starts with writes redirected. Direct submodule
        // checkouts always use a directory or gitfile at this location.
        let (git_dir, bootstrap_common_dir) = nested_repository_dirs(checkout, self.timeout)?;
        let seq = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
        let odb = self.scratch.join(format!("submodule-{seq}")).join("odb");
        fs::create_dir_all(odb.join("pack"))?;
        fs::create_dir_all(odb.join("info"))?;
        let bootstrap_env = vec![
            ("GIT_OBJECT_DIRECTORY", odb.clone().into_os_string()),
            (
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                bootstrap_common_dir.join("objects").into_os_string(),
            ),
        ];
        let common = run_git(
            checkout,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            &bootstrap_env,
            self.timeout,
        )?;
        if common.timed_out || !common.ok() {
            return Err(format!(
                "{}: could not resolve nested common dir: {}",
                checkout.display(),
                common.stderr_text()
            )
            .into());
        }
        let common_dir = PathBuf::from(common.stdout_trimmed());
        let env = vec![
            ("GIT_OBJECT_DIRECTORY", odb.clone().into_os_string()),
            (
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                common_dir.join("objects").into_os_string(),
            ),
        ];

        let head = match head_state_with_env(checkout, &env, self.timeout)? {
            HeadState::Branch { oid, .. } | HeadState::Detached { oid } => oid,
            HeadState::Unborn { name } => {
                return Err(
                    format!("{}: nested HEAD `{name}` is unborn", checkout.display()).into(),
                )
            }
            HeadState::BrokenHead { name } => {
                return Err(
                    format!("{}: nested HEAD `{name}` is broken", checkout.display()).into(),
                )
            }
        };
        let commit = run_git(
            checkout,
            &["cat-file", "-e", &format!("{head}^{{commit}}")],
            &env,
            self.timeout,
        )?;
        if commit.timed_out || !commit.ok() {
            return Err(format!(
                "{}: nested HEAD {head} is not a readable commit: {}",
                checkout.display(),
                commit.stderr_text()
            )
            .into());
        }

        let status = run_git(
            checkout,
            &[
                "--no-optional-locks",
                "status",
                "--porcelain=v2",
                "-z",
                "--untracked-files=all",
                "--renames",
            ],
            &env,
            self.timeout,
        )?;
        if status.timed_out || !status.ok() {
            return Err(format!(
                "{}: could not read nested status: {}",
                checkout.display(),
                status.stderr_text()
            )
            .into());
        }
        let entries = parse_status_v2(&status.stdout);
        let dirty = !entries.is_empty();
        let unmerged = entries
            .iter()
            .any(|entry| entry.kind == ChangeKind::Conflicted);
        let repository_overrides = repository_overrides_with_env(checkout, &env, self.timeout);
        let custom_merge_drivers = repository_overrides.custom_merge_drivers;
        let overrides = repository_overrides.filter_args;
        let tree = if dirty {
            let changed = changed_index_paths(&entries);
            Some(self.snapshot_tree_from_git_dir(
                checkout,
                SnapshotSource {
                    git_dir: &git_dir,
                    odb: &odb,
                    index_prefix: "submodule-index",
                    base_env: &env,
                    overrides: &overrides,
                },
                &changed,
            )?)
        } else {
            None
        };

        Ok(NestedSide {
            checkout: checkout.to_path_buf(),
            odb,
            side: Side {
                common_dir,
                head,
                tree,
                dirty,
                unmerged,
                custom_merge_drivers,
            },
        })
    }

    pub fn prime_submodules(&mut self, jobs: &[(PathBuf, String)]) {
        let mut seen = BTreeSet::new();
        let pending: Vec<(PathBuf, String)> = jobs
            .iter()
            .map(|(checkout, path)| (canonical(checkout), path.clone()))
            .filter(|key| !self.nested_sides.contains_key(key) && seen.insert(key.clone()))
            .collect();
        if pending.is_empty() {
            return;
        }
        let workers = std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1)
            .clamp(1, 8)
            .min(pending.len());
        let chunk_size = pending.len().div_ceil(workers);
        let deadline = current_cycle_deadline();
        let predictor: &Predictor = self;
        let mut results = Vec::with_capacity(pending.len());
        std::thread::scope(|scope| {
            let handles: Vec<_> = pending
                .chunks(chunk_size)
                .map(|chunk| {
                    (
                        chunk,
                        scope.spawn(move || {
                            with_cycle_deadline(deadline, || {
                                chunk
                                    .iter()
                                    .map(|(checkout, path)| {
                                        let nested_checkout = checkout.join(path);
                                        let cached =
                                            match predictor.build_nested_side(&nested_checkout) {
                                                Ok(side) => CachedNestedSide::Ready(side),
                                                Err(err) => {
                                                    CachedNestedSide::Unavailable(err.to_string())
                                                }
                                            };
                                        ((checkout.clone(), path.clone()), cached)
                                    })
                                    .collect::<Vec<_>>()
                            })
                        }),
                    )
                })
                .collect();
            for (chunk, handle) in handles {
                match handle.join() {
                    Ok(chunk_results) => results.extend(chunk_results),
                    Err(_) => results.extend(chunk.iter().map(|key| {
                        (
                            key.clone(),
                            CachedNestedSide::Unavailable(
                                "nested snapshot worker panicked".to_string(),
                            ),
                        )
                    })),
                }
            }
        });
        self.nested_sides.extend(results);
    }
    fn build_side_prepared(&self, input: &PreparedSideInput<'_>) -> Result<Side> {
        let head = input.read.head.oid().ok_or_else(|| {
            format!(
                "{} has no commit, nothing to compare",
                input.checkout.display()
            )
        })?;
        let dirty = !input.read.status_entries.is_empty();
        let unmerged = input
            .read
            .status_entries
            .iter()
            .any(|entry| entry.kind == ChangeKind::Conflicted);
        let tree = if dirty {
            let changed = changed_index_paths(&input.read.status_entries);
            let env = self.odb_env(input.common_dir);
            Some(self.snapshot_tree_from_git_dir(
                input.checkout,
                SnapshotSource {
                    git_dir: input.git_dir,
                    odb: &self.odb,
                    index_prefix: "index",
                    base_env: &env,
                    overrides: &input.read.filter_overrides,
                },
                &changed,
            )?)
        } else {
            None
        };
        Ok(Side {
            common_dir: input.common_dir.to_path_buf(),
            head: head.to_string(),
            tree,
            dirty,
            unmerged,
            custom_merge_drivers: input.read.custom_merge_drivers.clone(),
        })
    }

    fn build_side(&self, checkout: &Path) -> Result<Side> {
        let common_dir = PathBuf::from(
            git_ok(
                checkout,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                self.timeout,
            )?
            .stdout_trimmed(),
        );

        let head = match head_state(checkout, self.timeout)? {
            HeadState::Branch { oid, .. } | HeadState::Detached { oid } => oid,
            HeadState::Unborn { name } => {
                return Err(format!(
                    "{}: `{name}` has no commits, nothing to compare",
                    checkout.display()
                )
                .into())
            }
            HeadState::BrokenHead { name } => {
                return Err(format!(
                    "{}: `{name}` was deleted underneath the worktree",
                    checkout.display()
                )
                .into())
            }
        };

        // The exit-code trap: merge-tree exits 1 both for "conflict" and for a
        // bad argument, and `--quiet` erases the stdout signal that would tell
        // them apart. The only reliable defence is to hand merge-tree nothing
        // but 40-hex OIDs that we have already proved exist.
        let exists = git(
            checkout,
            &["cat-file", "-e", &format!("{head}^{{commit}}")],
            self.timeout,
        )?;
        if !exists.ok() {
            return Err(format!("{}: HEAD {head} is not a commit", checkout.display()).into());
        }

        let status = git_ok(
            checkout,
            &[
                "--no-optional-locks",
                "status",
                "--porcelain=v2",
                "-z",
                "--untracked-files=all",
                "--renames",
            ],
            self.timeout,
        )?;
        let entries = parse_status_v2(&status.stdout);
        let dirty = !entries.is_empty();
        let unmerged = entries.iter().any(|e| e.kind == ChangeKind::Conflicted);
        let custom_merge_drivers =
            repository_overrides_with_env(checkout, &[], self.timeout).custom_merge_drivers;

        let tree = if dirty {
            let changed = changed_index_paths(&entries);
            Some(self.snapshot_tree(checkout, &common_dir, &changed)?)
        } else {
            None
        };

        Ok(Side {
            common_dir,
            head,
            tree,
            dirty,
            unmerged,
            custom_merge_drivers,
        })
    }

    /// Turns the working tree (staged + unstaged + untracked) into a tree OID
    /// without ever touching the real index.
    ///
    /// Seeding the temp index by copying the real one is not an optimisation
    /// detail, it is the difference between 29 ms and 123 ms: `read-tree HEAD`
    /// into an empty index discards the stat cache, so `add -A` then rehashes
    /// every file in the worktree. The copy keeps the stat cache so untouched
    /// files are not rehashed. Status-reported files are forcibly re-read
    /// first, because the copied cache can otherwise hide a same-size edit
    /// whose mtime did not move and report a real conflict as a clean overlap.
    fn snapshot_tree(
        &self,
        checkout: &Path,
        common_dir: &Path,
        changed: &[&[u8]],
    ) -> Result<String> {
        let out = git_ok(
            checkout,
            &["rev-parse", "--path-format=absolute", "--git-dir"],
            self.timeout,
        )?;
        let git_dir = PathBuf::from(out.stdout_trimmed());
        let env = self.odb_env(common_dir);
        let overrides = filter_overrides_with_env(checkout, &env, self.timeout);
        self.snapshot_tree_from_git_dir(
            checkout,
            SnapshotSource {
                git_dir: &git_dir,
                odb: &self.odb,
                index_prefix: "index",
                base_env: &env,
                overrides: &overrides,
            },
            changed,
        )
    }

    fn snapshot_tree_from_git_dir(
        &self,
        checkout: &Path,
        source: SnapshotSource<'_>,
        changed: &[&[u8]],
    ) -> Result<String> {
        let git_dir = source.git_dir;
        let odb = source.odb;
        let index_prefix = source.index_prefix;
        let base_env = source.base_env;
        let overrides = source.overrides;
        let seq = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
        let index = TempIndex::new(self.scratch.join(format!("{index_prefix}-{seq}")))?;
        // A worktree with no index yet legitimately starts from empty. Every
        // *other* failure must not: seeding is what preserves the entries `add`
        // will not revisit, so an unreadable index silently drops every
        // sparse-checkout and skip-worktree path out of the snapshot tree, and
        // the pair then reports one-sided deletions for files nobody touched.
        let source_index = git_dir.join("index");
        let _seeded =
            copy_file_bounded(&source_index, &index.path, self.timeout).map_err(|err| {
                format!(
                    "{}: could not seed the snapshot index from {}: {err}",
                    checkout.display(),
                    source_index.display()
                )
            })?;

        let mut env = base_env.to_vec();
        debug_assert!(env
            .iter()
            .find(|(key, _)| *key == "GIT_OBJECT_DIRECTORY")
            .is_some_and(|(_, value)| value.as_os_str() == odb.as_os_str()));
        env.push(("GIT_INDEX_FILE", index.path.clone().into_os_string()));

        // `--renormalize` re-reads pathspec'd tracked files instead of trusting
        // their stat cache. Retaining each copied entry preserves its index
        // flags and keeps tracked-but-ignored paths tracked. The filter
        // overrides neutralise custom drivers only: `text` and `eol` attributes
        // still apply, so this can normalize a stat-clean staged blob that the
        // old snapshot kept byte-for-byte. Both sides are normalized through
        // the same prediction path.
        //
        // `changed` already contains only status records that prove hashable
        // worktree content. Missing files and type changes are left to the
        // general add below.
        let eligible: Vec<&[u8]> = changed.to_vec();

        // Status paths are user-controlled, so bound both their count and bytes
        // before one snapshot can grow past the operating system's exec limit.
        const MAX_PATH_BYTES: usize = 32 * 1024;
        const MAX_PATHS: usize = 256;
        let pathspec = TempIndex::new(self.scratch.join(format!("{index_prefix}-paths-{seq}")))?;
        let refresh_chunk = |paths: &[&[u8]]| -> Result<GitOut> {
            let mut pathspec_bytes = Vec::new();
            for path in paths {
                pathspec_bytes.extend_from_slice(path);
                pathspec_bytes.push(0);
            }
            fs::write(&pathspec.path, pathspec_bytes)?;
            let pathspec_arg = format!("--pathspec-from-file={}", pathspec.path.to_string_lossy());
            let mut args: Vec<&str> = overrides.iter().map(String::as_str).collect();
            args.extend([
                "--literal-pathspecs",
                "add",
                "-A",
                "--renormalize",
                "--pathspec-file-nul",
                &pathspec_arg,
            ]);
            run_git(checkout, &args, &env, self.timeout)
        };
        let mut first = 0;
        while first < eligible.len() {
            let mut end = first;
            let mut path_bytes = 0;
            while end < eligible.len() && end - first < MAX_PATHS {
                let next_bytes = eligible[end].len();
                if end > first && path_bytes + next_bytes > MAX_PATH_BYTES {
                    break;
                }
                path_bytes += next_bytes;
                end += 1;
            }

            let chunk = &eligible[first..end];
            let refresh = refresh_chunk(chunk)?;
            if !refresh.ok() {
                // A concurrent delete or type change can invalidate the status
                // record. Re-query those paths through bounded Git probes and
                // retry once only when the eligible set actually shrank.
                let mut narrowed = Vec::new();
                for path in chunk {
                    if snapshot_path_still_hashable(checkout, path, self.timeout)? {
                        narrowed.push(*path);
                    }
                }
                if narrowed.len() == chunk.len() {
                    return Err(format!(
                        "{}: could not prepare snapshot index: {}",
                        checkout.display(),
                        refresh.stderr_text()
                    )
                    .into());
                }
                if !narrowed.is_empty() {
                    let retry = refresh_chunk(&narrowed)?;
                    if !retry.ok() {
                        return Err(format!(
                            "{}: could not prepare snapshot index: {}",
                            checkout.display(),
                            retry.stderr_text()
                        )
                        .into());
                    }
                }
            }
            first = end;
        }
        let mut add_args: Vec<&str> = overrides.iter().map(String::as_str).collect();
        add_args.extend(["add", "-A", "--"]);
        let add = run_git(checkout, &add_args, &env, self.timeout)?;
        if !add.ok() {
            return Err(format!(
                "{}: could not snapshot working tree: {}",
                checkout.display(),
                add.stderr_text()
            )
            .into());
        }
        // `write-tree` needs the overrides too, and this was worth measuring
        // rather than assuming: it refreshes the index it is handed, so a
        // stat-dirty entry is re-hashed here — running the filter — even though
        // `add` above already visited it.
        let mut tree_args: Vec<&str> = overrides.iter().map(String::as_str).collect();
        tree_args.push("write-tree");
        let tree = run_git(checkout, &tree_args, &env, self.timeout)?;
        if !tree.ok() {
            return Err(format!(
                "{}: could not write snapshot tree: {}",
                checkout.display(),
                tree.stderr_text()
            )
            .into());
        }
        Ok(tree.stdout_trimmed())
    }

    fn side(&self, checkout: &Path) -> Result<&Side> {
        self.sides
            .get(&canonical(checkout))
            .ok_or_else(|| format!("{}: checkout was not primed", checkout.display()).into())
    }

    /// Whether the two checkouts' changes to `paths` actually conflict.
    ///
    /// Takes `&self` so a caller can fan the pairs out across threads once
    /// every checkout has been primed; everything below this point is lock-free.
    pub fn predict_pair(
        &self,
        left: &Path,
        right: &Path,
        paths: &[String],
    ) -> Result<PairPrediction> {
        let l = self.side(left)?;
        let r = self.side(right)?;

        // Different superproject common dirs are never comparable. Direct
        // submodules are separate clones in normal linked worktrees, so their
        // common dirs are intentionally handled by a different object view.
        if l.common_dir != r.common_dir {
            return Err(format!(
                "refusing to compare checkouts from different repositories: {} vs {}",
                l.common_dir.display(),
                r.common_dir.display()
            )
            .into());
        }

        let outer_env = self.odb_env(&l.common_dir);
        let mut prediction = self.predict_sides(left, right, l, r, paths, &outer_env)?;
        if self.nested_sides.is_empty() {
            return Ok(prediction);
        }

        let left_key = canonical(left);
        let right_key = canonical(right);
        for path in paths {
            let nested_left = self.nested_sides.get(&(left_key.clone(), path.clone()));
            let nested_right = self.nested_sides.get(&(right_key.clone(), path.clone()));
            if nested_left.is_none() && nested_right.is_none() {
                continue;
            }
            prediction
                .submodules
                .push(self.predict_submodule(path, nested_left, nested_right));
        }
        Ok(prediction)
    }

    fn predict_submodule(
        &self,
        path: &str,
        left: Option<&CachedNestedSide>,
        right: Option<&CachedNestedSide>,
    ) -> SubmodulePrediction {
        let unavailable = |reason: String| SubmodulePrediction {
            path: path.to_string(),
            conflict: None,
            conflicting_paths: Vec::new(),
            reason: Some(reason),
            approximate: false,
        };
        let (Some(left), Some(right)) = (left, right) else {
            return unavailable("one side was not primed as a direct submodule".to_string());
        };
        let l = match left {
            CachedNestedSide::Ready(side) => side,
            CachedNestedSide::Unavailable(reason) => {
                return unavailable(format!("left side unavailable: {reason}"))
            }
        };
        let r = match right {
            CachedNestedSide::Ready(side) => side,
            CachedNestedSide::Unavailable(reason) => {
                return unavailable(format!("right side unavailable: {reason}"))
            }
        };
        if l.side.unmerged || r.side.unmerged {
            return unavailable(
                "a nested checkout has an unresolved merge, so its snapshot is advisory"
                    .to_string(),
            );
        }

        let alternates = [
            l.side.common_dir.join("objects"),
            r.odb.clone(),
            r.side.common_dir.join("objects"),
        ];
        let alternate_env = match std::env::join_paths(alternates) {
            Ok(paths) => paths,
            Err(err) => {
                return unavailable(format!(
                    "nested object stores cannot be represented safely: {err}"
                ))
            }
        };
        let env = vec![
            ("GIT_OBJECT_DIRECTORY", l.odb.clone().into_os_string()),
            ("GIT_ALTERNATE_OBJECT_DIRECTORIES", alternate_env),
        ];
        match self.predict_sides(&l.checkout, &r.checkout, &l.side, &r.side, &[], &env) {
            Ok(prediction) => SubmodulePrediction {
                path: path.to_string(),
                conflict: Some(prediction.pair_conflict),
                conflicting_paths: prediction
                    .verdicts
                    .into_iter()
                    .filter_map(|(nested_path, hit)| hit.then_some(nested_path))
                    .collect(),
                reason: None,
                approximate: prediction.approximate,
            },
            Err(err) => unavailable(err.to_string()),
        }
    }

    fn predict_sides(
        &self,
        left: &Path,
        right: &Path,
        l: &Side,
        r: &Side,
        paths: &[String],
        env: &[(&str, OsString)],
    ) -> Result<PairPrediction> {
        let mut prediction = PairPrediction {
            advisory: l.unmerged || r.unmerged,
            left_advisory: l.unmerged,
            right_advisory: r.unmerged,
            ..Default::default()
        };
        let custom_drivers: BTreeSet<&str> = l
            .custom_merge_drivers
            .iter()
            .chain(&r.custom_merge_drivers)
            .map(String::as_str)
            .collect();
        if !custom_drivers.is_empty() {
            return Err(format!(
                "custom merge driver(s) configured ({}); prediction is unavailable because \
                 collide will not execute repository merge programs",
                custom_drivers.into_iter().collect::<Vec<_>>().join(", ")
            )
            .into());
        }

        // There is deliberately no prefilter here. There used to be one — skip
        // the pair when `paths` is empty, unless either side has a rename — and
        // it was unreachable for the case it existed to catch. `Side::has_rename`
        // was built from `status`, so it only ever saw *uncommitted* renames; a
        // worktree that had committed a directory rename and was otherwise clean
        // short-circuited to a conflict-free verdict while `merge-tree` on the
        // same pair exited 1 with `CONFLICT (directory rename suggested)`.
        //
        // Deciding which pairs are worth predicting belongs to `collide::analyse`,
        // which has both change sets. A second, differently-informed filter one
        // layer down can only disagree with it, and a clean pair costs 1.77 ms
        // to answer properly (docs/git-plumbing.md, "merge-tree cost").
        let (args_owned, approximate) = self.merge_tree_args(left, l, r, None, env)?;
        prediction.approximate = approximate;
        let base_args: Vec<&str> = args_owned.iter().map(String::as_str).collect();

        // One phase, not two. `--quiet` is not a sound conflict oracle; the
        // authoritative named form runs for outer and nested repositories.
        let mut args = vec!["merge-tree", "--write-tree", "-z", "--name-only"];
        args.extend(base_args.iter().copied());
        let named = run_git(left, &args, env, self.timeout)?;
        if named.code != Some(1) && named.code != Some(0) {
            return Err(format!(
                "merge-tree --name-only failed for {} vs {}: {}",
                left.display(),
                right.display(),
                named.stderr_text()
            )
            .into());
        }
        // Exit 1 also means a bad argument. A real merge prints at least its
        // result tree OID, so empty output is never accepted as a conflict.
        if named.stdout.is_empty() {
            return Err(format!(
                "merge-tree reported failure with no output for {} vs {}: {}",
                left.display(),
                right.display(),
                named.stderr_text()
            )
            .into());
        }

        let parsed = parse_merge_tree_z(&named.stdout);
        prediction.merged_tree = parsed.tree;
        prediction.conflict_types_by_path = parsed
            .conflicts
            .iter()
            .flat_map(|conflict| {
                conflict
                    .paths
                    .iter()
                    .map(|path| (path.clone(), conflict.conflict_type.clone()))
            })
            .fold(BTreeMap::new(), |mut by_path, (path, conflict_type)| {
                let types: &mut Vec<String> = by_path.entry(path).or_default();
                if !types.contains(&conflict_type) {
                    types.push(conflict_type);
                }
                by_path
            });
        prediction.conflict_types = parsed.conflict_types;
        prediction.conflicted_paths = parsed.conflicted.clone();
        prediction.pair_conflict = named.code == Some(1);
        let conflicted: BTreeSet<&String> = parsed.conflicted.iter().collect();
        let requested: BTreeSet<&String> = paths.iter().collect();
        prediction.verdicts = paths
            .iter()
            .map(|path| (path.clone(), conflicted.contains(path)))
            .collect();
        for extra in &parsed.conflicted {
            if !requested.contains(extra) {
                prediction.verdicts.push((extra.clone(), true));
            }
        }
        // Some directory-rename conflicts name no individual file. The pair
        // exit status remains authoritative in both repository scopes.
        if prediction.pair_conflict && parsed.conflicted.is_empty() {
            for verdict in &mut prediction.verdicts {
                verdict.1 = true;
            }
        }
        Ok(prediction)
    }

    /// Reads one path from the tree written by [`Self::predict_pair`].
    ///
    /// This deliberately addresses the retained tree instead of running the
    /// merge again: a second merge could disagree with the verdict being
    /// explained. The same redirected object store used by `merge-tree` keeps
    /// both the user's ODB read-only and the temporary merged blobs visible.
    ///
    /// Object kind and size are checked before content is requested. Unlike
    /// every other command in this module, blob content is arbitrary user data;
    /// letting `run_git` drain it first would make the display cap irrelevant
    /// to peak memory.
    pub fn merged_blob(&self, checkout: &Path, tree: &str, path: &str) -> Result<Vec<u8>> {
        let side = self.side(checkout)?;
        if tree.is_empty() {
            return Err("prediction produced no merged tree".into());
        }
        if is_lossy_display_path(path) {
            return Err(format!(
                "git reported `{path}` with bytes that are not representable as a tree path"
            )
            .into());
        }
        let object = format!("{tree}:{path}");
        let env = self.odb_env(&side.common_dir);

        let kind = run_git(
            checkout,
            &["cat-file", "-t", object.as_str()],
            &env,
            self.timeout,
        )?;
        if !kind.ok() {
            return Err(format!(
                "cat-file could not inspect `{}` in the predicted tree: {}",
                lossy(path.as_bytes()),
                kind.stderr_text()
            )
            .into());
        }
        let kind = kind.stdout_trimmed();
        if kind != "blob" {
            return Err(format!(
                "`{}` is a {kind}, not a blob, in the predicted tree",
                lossy(path.as_bytes())
            )
            .into());
        }

        let size = run_git(
            checkout,
            &["cat-file", "-s", object.as_str()],
            &env,
            self.timeout,
        )?;
        if !size.ok() {
            return Err(format!(
                "cat-file could not size `{}` in the predicted tree: {}",
                lossy(path.as_bytes()),
                size.stderr_text()
            )
            .into());
        }
        let size = size
            .stdout_trimmed()
            .parse::<u64>()
            .map_err(|err| format!("cat-file returned an invalid blob size: {err}"))?;
        if size > WHY_BLOB_MAX_BYTES {
            return Err(format!(
                "predicted blob is {size} bytes; --why will not read blobs above \
                 {WHY_BLOB_MAX_BYTES} bytes"
            )
            .into());
        }

        let out = run_git(
            checkout,
            &["cat-file", "blob", object.as_str()],
            &env,
            self.timeout,
        )?;
        if !out.ok() {
            return Err(format!(
                "cat-file could not read `{}` from predicted tree: {}",
                lossy(path.as_bytes()),
                out.stderr_text()
            )
            .into());
        }
        Ok(out.stdout)
    }

    /// Predicts whether one already-primed checkout conflicts with the cached
    /// local integration ref. The checkout's snapshot tree is reused verbatim;
    /// this path never reads status or creates another snapshot.
    pub fn predict_target(&self, checkout: &Path, target_ref: &str) -> Result<TargetMergeOutcome> {
        let side = self.side(checkout)?;
        if !side.custom_merge_drivers.is_empty() {
            return Err(format!(
                "custom merge driver(s) configured ({}); target prediction is unavailable",
                side.custom_merge_drivers.join(", ")
            )
            .into());
        }
        let key = (side.common_dir.clone(), target_ref.to_string());
        let target = self.targets.get(&key).ok_or_else(|| {
            format!(
                "integration ref `{target_ref}` was not primed for {}",
                checkout.display()
            )
        })?;

        // Establishing the target is a separate claim from running merge-tree:
        // unrelated histories have no meaningful "clean" or "conflict"
        // verdict, even though tree-mode plumbing can make them look like an
        // add/add conflict.
        let env = self.odb_env(&side.common_dir);
        let bases = self.merge_bases(checkout, side, target, &env)?;
        if bases.is_empty() {
            return Ok(TargetMergeOutcome::NoCommonAncestor);
        }
        let (args_owned, approximate) =
            self.merge_tree_args(checkout, side, target, Some(bases.as_slice()), &env)?;
        let base_args: Vec<&str> = args_owned.iter().map(String::as_str).collect();
        let mut args = vec!["merge-tree", "--write-tree", "-z", "--name-only"];
        args.extend(base_args.iter().copied());
        let named = run_git(checkout, &args, &env, self.timeout)?;
        if named.code != Some(1) && named.code != Some(0) {
            return Err(format!(
                "merge-tree --name-only failed against `{target_ref}`: {}",
                named.stderr_text()
            )
            .into());
        }
        if named.stdout.is_empty() {
            return Err(format!(
                "merge-tree reported failure with no output against `{target_ref}`: {}",
                named.stderr_text()
            )
            .into());
        }
        Ok(TargetMergeOutcome::Predicted {
            conflicts: named.code == Some(1),
            approximate,
            advisory: side.unmerged,
        })
    }

    fn merge_bases(
        &self,
        cwd: &Path,
        l: &Side,
        r: &Side,
        env: &[(&str, OsString)],
    ) -> Result<Vec<String>> {
        let bases = run_git(
            cwd,
            &["merge-base", "--all", &l.head, &r.head],
            env,
            self.timeout,
        )?;
        if bases.timed_out || bases.code.is_none() {
            return Err(format!(
                "merge-base could not answer in {}: {}",
                cwd.display(),
                bases.stderr_text()
            )
            .into());
        }
        let list = bases
            .stdout_trimmed()
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if bases.ok() || bases.code == Some(1) {
            Ok(list)
        } else {
            Err(format!(
                "merge-base failed in {}: {}",
                cwd.display(),
                bases.stderr_text()
            )
            .into())
        }
    }

    /// Builds the trailing merge-tree arguments and reports whether a single
    /// merge base had to be forced.
    ///
    /// Two checkouts with no common ancestor get one answer here regardless of
    /// how dirty they are: an error, which the caller turns into "prediction
    /// could not run". The dirty path used to substitute the empty tree as the
    /// base, which makes every shared path an add/add and reports a confident
    /// conflict on all of them, while the clean path let `merge-tree` refuse
    /// with `refusing to merge unrelated histories`. The same two orphan
    /// branches therefore flipped between "unknown" and "everything conflicts"
    /// depending on whether one of them happened to have a stray untracked file.
    /// With no common ancestor there is no merge to predict, and `Unknown` is
    /// the honest verdict.
    fn merge_tree_args(
        &self,
        cwd: &Path,
        l: &Side,
        r: &Side,
        bases: Option<&[String]>,
        env: &[(&str, OsString)],
    ) -> Result<(Vec<String>, bool)> {
        if !l.dirty && !r.dirty {
            // Both sides are commits, so no `--merge-base`: merge-tree then
            // resolves multiple bases recursively, which beats any single base
            // we could pick on a criss-cross history.
            return Ok((vec![l.head.clone(), r.head.clone()], false));
        }

        // A dirty side is a bare tree, and a tree carries no history, so the
        // base has to be supplied explicitly.
        let owned_bases;
        let list = match bases {
            Some(bases) => bases,
            None => {
                owned_bases = self.merge_bases(cwd, l, r, env)?;
                owned_bases.as_slice()
            }
        };
        if list.is_empty() {
            return Err(format!(
                "no common ancestor between {} and {}, so this pair cannot be predicted",
                l.head, r.head
            )
            .into());
        }
        // Passing `--merge-base` forces a single base; say so when there is more
        // than one, because the answer is then an approximation of the recursive
        // merge git itself would do.
        let approximate = list.len() > 1;
        let base = format!("{}^{{tree}}", list[0]);
        let base_tree = run_git(cwd, &["rev-parse", &base], env, self.timeout)?;
        if base_tree.timed_out || !base_tree.ok() {
            return Err(format!(
                "rev-parse could not resolve merge base in {}: {}",
                cwd.display(),
                base_tree.stderr_text()
            )
            .into());
        }
        let base_tree = base_tree.stdout_trimmed();

        Ok((
            vec![
                format!("--merge-base={base_tree}"),
                l.tree_ish(),
                r.tree_ish(),
            ],
            approximate,
        ))
    }
}

impl Drop for Predictor {
    fn drop(&mut self) {
        // Scratch objects and any stray index are ours alone; losing them on a
        // crash is the one leak we accept, so clean up eagerly here.
        let _ = fs::remove_dir_all(&self.scratch);
    }
}

/// Removes scratch directories left behind by collide processes that are no
/// longer running.
///
/// `Predictor::drop` is the normal cleanup path, but a SIGKILLed daemon never
/// runs it. Each scratch directory is named `collide-<pid>-<seq>`, so a
/// directory whose pid is gone is provably garbage. Directories belonging to a
/// live process — including this one, and including a concurrently running
/// second collide — are never touched.
///
/// Both locations are swept. `Predictor::new` falls back to
/// `std::env::temp_dir()` when the state dir cannot be created, and sweeping
/// only the state dir meant those fallbacks leaked forever.
pub fn sweep_scratch() {
    sweep_scratch_in(&config::state_dir().join("scratch"));
    sweep_scratch_in(&std::env::temp_dir());
}

fn sweep_scratch_in(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let self_pid = std::process::id();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix("collide-") else {
            continue;
        };
        // Only `collide-<pid>-<seq>`. Anything whose first segment is not a
        // number is somebody else's directory that happens to share the prefix.
        let Some(Ok(pid)) = rest.split('-').next().map(str::parse::<u32>) else {
            continue;
        };
        if pid == self_pid || process_is_alive(pid) {
            continue;
        }
        let _ = fs::remove_dir_all(entry.path());
    }
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    // Signal 0 performs the permission and existence checks without delivering
    // anything. EPERM means the process exists but belongs to someone else, so
    // only ESRCH proves it is gone.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(unix))]
fn process_is_alive(_pid: u32) -> bool {
    // Without a portable liveness check, never reclaim: leaking a scratch
    // directory is strictly better than deleting a live run's objects.
    true
}

/// A temp index that removes itself, `.lock` sibling included. git leaves an
/// `index.lock` behind if it dies mid-write, and a stale lock next to a temp
/// index would break the next snapshot that happened to reuse the name.
struct TempIndex {
    path: PathBuf,
}

impl TempIndex {
    fn new(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(Self { path })
    }
}

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let mut lock = self.path.clone().into_os_string();
        lock.push(".lock");
        let _ = fs::remove_file(PathBuf::from(lock));
    }
}

pub const CONFLICT_RENAME_RENAME: &str = "CONFLICT (rename/rename)";
pub const CONFLICT_RENAME_DELETE: &str = "CONFLICT (rename/delete)";
pub const CONFLICT_DIRECTORY_RENAME_SUGGESTED: &str = "CONFLICT (directory rename suggested)";

/// The compact pane annotation for a machine-stable merge-tree conflict token.
// These three tokens — rename/rename, rename/delete, and directory rename
// suggested — are exactly the conflict shapes that make a `(rename)` claim true.
pub fn conflict_type_annotation(conflict_type: &str) -> Option<&'static str> {
    match conflict_type {
        CONFLICT_RENAME_RENAME | CONFLICT_RENAME_DELETE | CONFLICT_DIRECTORY_RENAME_SUGGESTED => {
            Some("rename")
        }
        _ => None,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct MergeTreeConflict {
    pub paths: Vec<String>,
    pub conflict_type: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct MergeTreeOutput {
    pub tree: String,
    pub conflicted: Vec<String>,
    pub conflict_types: Vec<String>,
    /// Conflict message records with the path association git reported.
    pub conflicts: Vec<MergeTreeConflict>,
}

/// Parses `merge-tree --write-tree -z --name-only`.
///
/// Framing: `<tree-oid> NUL`, then one field per conflicted file, then an empty
/// field closing the file section, then message records of
/// `<n> NUL <path>×n NUL <conflict-type> NUL <human-message> NUL`. A clean
/// merge emits the tree OID and nothing else, so a single field means clean.
pub fn parse_merge_tree_z(bytes: &[u8]) -> MergeTreeOutput {
    let mut fields: Vec<&[u8]> = bytes.split(|b| *b == 0).collect();
    // `split` on trailing-NUL-terminated output leaves one empty tail field.
    if fields.last().map(|f| f.is_empty()).unwrap_or(false) {
        fields.pop();
    }
    let mut out = MergeTreeOutput::default();
    let mut i = 0;
    if let Some(tree) = fields.first() {
        out.tree = lossy(tree);
        i = 1;
    }
    while i < fields.len() && !fields[i].is_empty() {
        out.conflicted.push(lossy(fields[i]));
        i += 1;
    }
    i += 1; // the empty field closing the file section

    // Message records. Parsing is best-effort: the file section above is the
    // authoritative answer, while these preserve git's machine-stable reason
    // and exactly the paths to which git attached it.
    //
    // Not every message record is a conflict. git emits an `Auto-merging`
    // record for each file it merged successfully, in exactly the same shape.
    // Keep only conflict records, then derive the compatibility-oriented flat
    // token set from those retained records.
    let mut types: BTreeSet<String> = BTreeSet::new();
    while i < fields.len() {
        let Ok(count) = std::str::from_utf8(fields[i])
            .unwrap_or("")
            .trim()
            .parse::<usize>()
        else {
            break;
        };
        let type_at = i + 1 + count;
        if type_at >= fields.len() {
            break;
        }
        let token = lossy(fields[type_at]);
        if token.starts_with("CONFLICT (") {
            let paths = fields[i + 1..type_at]
                .iter()
                .map(|path| lossy(path))
                .collect();
            types.insert(token.clone());
            out.conflicts.push(MergeTreeConflict {
                paths,
                conflict_type: token,
            });
        }
        i = type_at + 2; // skip the human message too
    }
    out.conflict_types = types.into_iter().collect();
    out
}

/// Whether the two checkouts' changes to `paths` actually conflict, as opposed
/// to merely touching the same file.
///
/// One-shot convenience wrapper. Callers with more than one pair should build a
/// [`Predictor`], prime every checkout once, and reuse it — snapshotting a
/// worktree per pair instead of per cycle is the single most expensive mistake
/// available here.
pub fn predict_conflict(
    left: &Path,
    right: &Path,
    paths: &[String],
    timeout: Duration,
) -> Result<Vec<(String, bool)>> {
    let mut predictor = Predictor::new(timeout)?;
    predictor.prime(left)?;
    predictor.prime(right)?;
    Ok(predictor.predict_pair(left, right, paths)?.verdicts)
}

fn canonical(path: &Path) -> PathBuf {
    path.to_path_buf()
}
