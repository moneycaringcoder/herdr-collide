//! Git layer: change sets and conflict prediction.
//!
//! Every invocation is read-only against the user's repository. The only writes
//! this module ever performs are to temporary index files under the plugin
//! state dir, via `GIT_INDEX_FILE`, and every invocation passes
//! `--no-optional-locks` so it cannot contend with an agent's own git commands.
//!
//! Object writes are redirected too. `git add` (index snapshot) and
//! `merge-tree --write-tree` (phase 2) both create loose objects; both are run
//! with `GIT_OBJECT_DIRECTORY` pointed at a scratch directory and
//! `GIT_ALTERNATE_OBJECT_DIRECTORIES` pointed at the real object store, so the
//! user's ODB never grows and never needs a `gc` it did not ask for.
//!
//! Paths come out of git as raw bytes. `model::ChangedPath` uses `String`, so
//! every path is converted with `String::from_utf8_lossy` at this boundary;
//! non-UTF-8 path bytes are replaced rather than preserved.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
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

/// Reason codes that exclude a checkout from pairing entirely.
pub const UNPAIRABLE_REASONS: [&str; 2] = [DEGRADED_UNBORN, DEGRADED_BROKEN_HEAD];

/// The empty tree, used as a stand-in base when there is no merge base.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Untracked files are line-counted by reading them, since no `diff` covers
/// them. Anything larger than this is counted as zero lines rather than paying
/// to read it; runaway detection only needs an order of magnitude.
const MAX_UNTRACKED_READ: u64 = 8 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Process plumbing
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct GitOut {
    /// `None` when the child was killed by a signal or by our own deadline.
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
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

/// Runs one git command with a hard deadline.
///
/// A hung git (a stuck credential helper, an NFS stall, an fsmonitor that never
/// answers) must not stall the refresh loop, so the child is polled and killed
/// on expiry. The pipes are drained on separate threads because a child that
/// fills a 64 KiB pipe buffer blocks forever otherwise, and `status -uall` on a
/// big repo comfortably exceeds that.
fn run_git(
    dir: &Path,
    args: &[&str],
    envs: &[(&str, OsString)],
    timeout: Duration,
) -> Result<GitOut> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir);
    cmd.args(args);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    // Never inherit a caller's git environment: collide can be launched from a
    // git hook, where GIT_DIR/GIT_INDEX_FILE would silently retarget every
    // command at the wrong repository.
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_NAMESPACE",
    ] {
        cmd.env_remove(key);
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
    for (key, value) in envs {
        cmd.env(key, value);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to run git in {}: {e}", dir.display()))?;

    let mut out_pipe = child.stdout.take().expect("stdout piped");
    let mut err_pipe = child.stderr.take().expect("stderr piped");
    let out_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out_pipe.read_to_end(&mut buf);
        buf
    });
    let err_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err_pipe.read_to_end(&mut buf);
        buf
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
                    let _ = child.kill();
                    break child.wait()?;
                }
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_millis(5));
            }
        }
    };

    let stdout = out_thread.join().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default();
    Ok(GitOut {
        code: status.code(),
        stdout,
        stderr,
        timed_out,
    })
}

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

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

// ---------------------------------------------------------------------------
// Repo identity and HEAD state
// ---------------------------------------------------------------------------

/// Canonicalized `--git-common-dir`: the only safe answer to "are these the
/// same repository?". Every worktree of one repo shares it, while each has its
/// own `--git-dir`. Canonicalizing matters because a symlinked or bind-mounted
/// checkout otherwise yields two keys for one repo.
pub fn repo_key(checkout: &Path, timeout: Duration) -> Result<RepoKey> {
    let out = git_ok(
        checkout,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        timeout,
    )?;
    let raw = PathBuf::from(out.stdout_trimmed());
    let canonical = fs::canonicalize(&raw).unwrap_or(raw);
    Ok(RepoKey(canonical.to_string_lossy().into_owned()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadState {
    /// HEAD is a symref to a branch that resolves to a commit.
    Branch { name: String, oid: String },
    /// Detached HEAD. Perfectly usable for analysis; we just use the raw OID.
    Detached { oid: String },
    /// HEAD names a branch that has never had a commit.
    Unborn { name: String },
    /// HEAD names a branch that was deleted underneath this worktree.
    BrokenHead { name: String },
}

impl HeadState {
    pub fn oid(&self) -> Option<&str> {
        match self {
            HeadState::Branch { oid, .. } | HeadState::Detached { oid } => Some(oid),
            _ => None,
        }
    }
}

/// Classifies HEAD.
///
/// docs/git-plumbing.md claims `symbolic-ref -q HEAD` exits 1 for an unborn
/// branch and 0 for a deleted one. It does not: git 2.53.0 exits 0 in both
/// cases and prints the same ref name, so the two states really are
/// indistinguishable by that command. The discriminator that does work is the
/// worktree's own HEAD reflog: a worktree that ever had a commit checked out
/// has `logs/HEAD`, a freshly initialised one does not.
pub fn head_state(checkout: &Path, timeout: Duration) -> Result<HeadState> {
    let symref = git(
        checkout,
        &["symbolic-ref", "-q", "--short", "HEAD"],
        timeout,
    )?;
    let resolved = git(
        checkout,
        &["rev-parse", "--verify", "-q", "HEAD^{commit}"],
        timeout,
    )?;

    let oid = if resolved.ok() {
        Some(resolved.stdout_trimmed())
    } else {
        None
    };

    match (symref.ok(), oid) {
        (true, Some(oid)) => Ok(HeadState::Branch {
            name: symref.stdout_trimmed(),
            oid,
        }),
        (false, Some(oid)) => Ok(HeadState::Detached { oid }),
        (symbolic, None) => {
            let name = if symbolic {
                symref.stdout_trimmed()
            } else {
                "HEAD".to_string()
            };
            let reflog = git(checkout, &["rev-parse", "--git-path", "logs/HEAD"], timeout)?;
            let has_reflog = reflog.ok()
                && checkout
                    .join(reflog.stdout_trimmed())
                    .metadata()
                    .map(|m| m.len() > 0)
                    .unwrap_or(false);
            if has_reflog {
                Ok(HeadState::BrokenHead { name })
            } else {
                Ok(HeadState::Unborn { name })
            }
        }
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
/// explicit one. First match wins.
pub fn integration_ref(checkout: &Path, timeout: Duration) -> Result<String> {
    for candidate in [
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/main",
        "refs/remotes/origin/master",
        "refs/heads/main",
        "refs/heads/master",
        "refs/heads/trunk",
    ] {
        let out = git(
            checkout,
            &[
                "rev-parse",
                "--verify",
                "-q",
                &format!("{candidate}^{{commit}}"),
            ],
            timeout,
        )?;
        if out.ok() {
            return Ok(candidate.to_string());
        }
    }
    // No integration ref: fall back to HEAD, which makes the committed half of
    // the change set empty and leaves the dirty half intact.
    Ok("HEAD".to_string())
}

// ---------------------------------------------------------------------------
// Change sets
// ---------------------------------------------------------------------------

/// One parsed `status --porcelain=v2` record, before it is folded into a
/// change set. Exposed so the parser can be tested against real git output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    pub path: String,
    /// Populated only for rename/copy (`2`) records: the original path.
    pub origin: Option<String>,
    pub kind: ChangeKind,
    pub is_rename: bool,
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
                        origin: None,
                        kind: kind_from_xy(xy),
                        is_rename: false,
                    });
                }
                i += 1;
            }
            // rename/copy: `2 <XY> ... <Xscore> <path>` NUL `<origPath>`
            b'2' => {
                if let (Some(path), Some(xy)) = (field_after_space(line, 9), xy_of(line)) {
                    let origin = fields.get(i + 1).map(|f| lossy(f));
                    entries.push(StatusEntry {
                        path: lossy(path),
                        origin,
                        kind: kind_from_xy(xy),
                        is_rename: true,
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
                        origin: None,
                        kind: ChangeKind::Conflicted,
                        is_rename: false,
                    });
                }
                i += 1;
            }
            // untracked: `? <path>`
            b'?' => {
                if let Some(path) = field_after_space(line, 1) {
                    entries.push(StatusEntry {
                        path: lossy(path),
                        origin: None,
                        kind: ChangeKind::Untracked,
                        is_rename: false,
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

/// Everything `checkout` has changed relative to its merge base with `base`:
/// staged, unstaged, untracked, conflicted, and committed-since-merge-base.
///
/// Degrades rather than failing for detached HEAD, an unborn branch, or a
/// deleted branch — the returned `ChangeSet` carries `degraded` in those cases.
pub fn change_set(checkout: &Path, base: &str, timeout: Duration) -> Result<ChangeSet> {
    let mut kinds: BTreeMap<String, ChangeKind> = BTreeMap::new();
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
        ],
        timeout,
    )?;
    let entries = parse_status_v2(&status.stdout);
    let mut unmerged = false;
    for entry in &entries {
        if entry.kind == ChangeKind::Conflicted {
            unmerged = true;
        }
        note(&mut kinds, entry.path.clone(), entry.kind);
        if let Some(origin) = &entry.origin {
            // Both halves of a rename belong to the change set: another
            // worktree editing the original path collides with this one.
            note(&mut kinds, origin.clone(), entry.kind);
        }
    }
    if unmerged {
        reasons.push(format!(
            "{DEGRADED_UNMERGED}: a merge is in progress, predictions are advisory"
        ));
    }

    let head = head_state(checkout, timeout)?;
    match &head {
        HeadState::Unborn { name } => {
            reasons.push(format!("{DEGRADED_UNBORN}: `{name}` has no commits yet"));
        }
        HeadState::BrokenHead { name } => {
            reasons.push(format!(
                "{DEGRADED_BROKEN_HEAD}: `{name}` was deleted underneath this worktree"
            ));
        }
        _ => {}
    }

    if let Some(head_oid) = head.oid() {
        // Dirty-side line volume: everything between HEAD and the working tree.
        let dirty = git(checkout, &["diff", "--numstat", "-z", "HEAD"], timeout)?;
        if dirty.ok() {
            for stat in parse_numstat_z(&dirty.stdout) {
                set.lines_added += stat.added;
                set.lines_removed += stat.removed;
            }
        }

        let base_oid = git(
            checkout,
            &["rev-parse", "--verify", "-q", &format!("{base}^{{commit}}")],
            timeout,
        )?;
        if !base_oid.ok() {
            reasons.push(format!(
                "{DEGRADED_MISSING_BASE_REF}: `{base}` does not resolve"
            ));
        } else {
            let base_oid = base_oid.stdout_trimmed();
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
                        set.lines_added += stat.added;
                        set.lines_removed += stat.removed;
                        // `--name-only` collapses a rename to the new path
                        // only; `--numstat` reports both, so this is where the
                        // pre-rename path enters the committed change set.
                        for path in stat.paths {
                            note(&mut kinds, path, ChangeKind::Committed);
                        }
                    }
                }
            }
        }
    }

    // Untracked files are invisible to every `diff`, so count them from disk.
    for entry in &entries {
        if entry.kind == ChangeKind::Untracked {
            set.lines_added += count_lines_on_disk(&checkout.join(&entry.path));
        }
    }
    // On an unborn branch there is no HEAD to diff against, so the staged
    // additions are counted the same way.
    if head.oid().is_none() {
        for entry in &entries {
            if entry.kind == ChangeKind::Staged || entry.kind == ChangeKind::Unstaged {
                set.lines_added += count_lines_on_disk(&checkout.join(&entry.path));
            }
        }
    }

    set.paths = kinds
        .into_iter()
        .map(|(path, kind)| ChangedPath { path, kind })
        .collect();
    if !reasons.is_empty() {
        set.degraded = true;
        set.degraded_reason = Some(reasons.join("; "));
    }
    Ok(set)
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

fn count_lines_on_disk(path: &Path) -> u64 {
    let Ok(meta) = fs::metadata(path) else {
        return 0;
    };
    if !meta.is_file() || meta.len() > MAX_UNTRACKED_READ {
        return 0;
    }
    let Ok(bytes) = fs::read(path) else {
        return 0;
    };
    // A NUL in the first 8 KiB is git's own binary heuristic; binary files
    // report no line counts, matching `--numstat`.
    if bytes.iter().take(8192).any(|b| *b == 0) {
        return 0;
    }
    if bytes.is_empty() {
        return 0;
    }
    let newlines = bytes.iter().filter(|b| **b == b'\n').count() as u64;
    if bytes.last() == Some(&b'\n') {
        newlines
    } else {
        newlines + 1
    }
}

// ---------------------------------------------------------------------------
// Conflict prediction
// ---------------------------------------------------------------------------

/// Verdict for one pair of checkouts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PairPrediction {
    /// One entry per requested path, plus any conflicted path git reported that
    /// was not requested (a rename can conflict on a path neither change set
    /// listed under the same name).
    pub verdicts: Vec<(String, bool)>,
    /// True when a single merge base had to be forced although more than one
    /// exists, so the answer is an approximation of what a real merge would do.
    pub approximate: bool,
    /// True when one side has a merge in progress: its snapshot contains
    /// conflict markers, so the prediction is advisory only.
    pub advisory: bool,
    /// merge-tree's exit status for the pair as a whole. Authoritative:
    /// git documents that a merge can conflict without any individual file
    /// appearing in the conflicted-file list.
    pub pair_conflict: bool,
    /// Machine-stable conflict-type tokens git reported, e.g.
    /// `CONFLICT (contents)`. Never parse the human prose instead.
    pub conflict_types: Vec<String>,
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
    has_rename: bool,
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

static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

/// Holds the per-cycle state that makes prediction cheap: one scratch object
/// directory for the whole run, and one snapshot per worktree reused across all
/// of that worktree's pairs.
pub struct Predictor {
    timeout: Duration,
    scratch: PathBuf,
    odb: PathBuf,
    sides: HashMap<PathBuf, Side>,
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

    fn build_side(&self, checkout: &Path) -> Result<Side> {
        let common_dir = {
            let out = git_ok(
                checkout,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                self.timeout,
            )?;
            let raw = PathBuf::from(out.stdout_trimmed());
            fs::canonicalize(&raw).unwrap_or(raw)
        };

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
        let has_rename = entries.iter().any(|e| e.is_rename);

        let tree = if dirty {
            Some(self.snapshot_tree(checkout, &common_dir)?)
        } else {
            None
        };

        Ok(Side {
            common_dir,
            head,
            tree,
            dirty,
            unmerged,
            has_rename,
        })
    }

    /// Turns the working tree (staged + unstaged + untracked) into a tree OID
    /// without ever touching the real index.
    ///
    /// Seeding the temp index by copying the real one is not an optimisation
    /// detail, it is the difference between 29 ms and 123 ms: `read-tree HEAD`
    /// into an empty index discards the stat cache, so `add -A` then rehashes
    /// every file in the worktree. The copy keeps the stat cache, so `add -A`
    /// only hashes what actually changed.
    fn snapshot_tree(&self, checkout: &Path, common_dir: &Path) -> Result<String> {
        let git_dir = {
            let out = git_ok(
                checkout,
                &["rev-parse", "--path-format=absolute", "--git-dir"],
                self.timeout,
            )?;
            PathBuf::from(out.stdout_trimmed())
        };

        let seq = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
        let index = TempIndex::new(self.scratch.join(format!("index-{seq}")))?;
        // Best effort: a worktree with no index yet just starts from empty.
        let _ = fs::copy(git_dir.join("index"), &index.path);

        let mut env = self.odb_env(common_dir);
        env.push(("GIT_INDEX_FILE", index.path.clone().into_os_string()));

        let add = run_git(checkout, &["add", "-A", "--"], &env, self.timeout)?;
        if !add.ok() {
            return Err(format!(
                "{}: could not snapshot working tree: {}",
                checkout.display(),
                add.stderr_text()
            )
            .into());
        }
        let tree = run_git(checkout, &["write-tree"], &env, self.timeout)?;
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

        // Different repositories are never comparable, and merge-tree would
        // happily produce nonsense for two unrelated histories rather than
        // refuse.
        if l.common_dir != r.common_dir {
            return Err(format!(
                "refusing to compare checkouts from different repositories: {} vs {}",
                l.common_dir.display(),
                r.common_dir.display()
            )
            .into());
        }

        let mut prediction = PairPrediction {
            advisory: l.unmerged || r.unmerged,
            ..Default::default()
        };

        // Prefilter, free: with no shared path there is nothing to conflict
        // over. The exception is a rename on either side, where the merge can
        // conflict on a path that appears under different names in the two
        // change sets.
        if paths.is_empty() && !l.has_rename && !r.has_rename {
            return Ok(prediction);
        }

        let (args_owned, approximate) = self.merge_tree_args(left, l, r)?;
        prediction.approximate = approximate;
        let base_args: Vec<&str> = args_owned.iter().map(String::as_str).collect();
        let env = self.odb_env(&l.common_dir);

        // One phase, not two. `--quiet` used to gate this call because it is
        // ~15x cheaper, but it is not a sound oracle: on git 2.53.0 it reports
        // a clean merge for merges that genuinely conflict. See
        // docs/git-plumbing.md, "The --quiet trap". Losing a conflict is the
        // one failure this plugin cannot have, so the cheap gate is gone and
        // the authoritative form runs on every pair that survives the
        // path-intersection prefilter.
        let mut args = vec!["merge-tree", "--write-tree", "-z", "--name-only"];
        args.extend(base_args.iter().copied());
        let named = run_git(left, &args, &env, self.timeout)?;
        if named.code != Some(1) && named.code != Some(0) {
            return Err(format!(
                "merge-tree --name-only failed for {} vs {}: {}",
                left.display(),
                right.display(),
                named.stderr_text()
            )
            .into());
        }
        // The exit-code trap: a bad argument also exits 1, with empty stdout
        // and a message on stderr. A real merge always prints at least the
        // merged tree OID, so empty stdout means the arguments were rejected,
        // not that the merge conflicted.
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
        prediction.conflict_types = parsed.conflict_types;
        prediction.pair_conflict = named.code == Some(1);
        let conflicted: BTreeSet<&String> = parsed.conflicted.iter().collect();

        let requested: BTreeSet<&String> = paths.iter().collect();
        prediction.verdicts = paths
            .iter()
            .map(|p| (p.clone(), conflicted.contains(p)))
            .collect();
        for extra in &parsed.conflicted {
            if !requested.contains(extra) {
                prediction.verdicts.push((extra.clone(), true));
            }
        }
        // git's own documentation warns that an empty conflicted-file list is
        // not a clean merge: some directory-rename conflicts have no individual
        // conflicted file. The exit status is the authority, so surface the
        // pair-level verdict rather than silently reporting every path clean.
        if prediction.pair_conflict && parsed.conflicted.is_empty() {
            for verdict in &mut prediction.verdicts {
                verdict.1 = true;
            }
        }
        Ok(prediction)
    }

    /// Builds the trailing merge-tree arguments and reports whether a single
    /// merge base had to be forced.
    fn merge_tree_args(&self, cwd: &Path, l: &Side, r: &Side) -> Result<(Vec<String>, bool)> {
        if !l.dirty && !r.dirty {
            // Both sides are commits, so no `--merge-base`: merge-tree then
            // resolves multiple bases recursively, which beats any single base
            // we could pick on a criss-cross history.
            return Ok((vec![l.head.clone(), r.head.clone()], false));
        }

        // A dirty side is a bare tree, and a tree carries no history, so the
        // base has to be supplied explicitly.
        let bases = git(
            cwd,
            &["merge-base", "--all", &l.head, &r.head],
            self.timeout,
        )?;
        let (base_tree, approximate) = if bases.ok() {
            let list: Vec<String> = bases
                .stdout_trimmed()
                .lines()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            let first = list
                .first()
                .cloned()
                .unwrap_or_else(|| EMPTY_TREE.to_string());
            let tree = git_ok(
                cwd,
                &["rev-parse", &format!("{first}^{{tree}}")],
                self.timeout,
            )?
            .stdout_trimmed();
            (tree, list.len() > 1)
        } else {
            // Unrelated histories: the empty tree is the honest base, and every
            // shared path then shows up as add/add.
            (EMPTY_TREE.to_string(), true)
        };

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
pub fn sweep_scratch() {
    let root = config::state_dir().join("scratch");
    let Ok(entries) = fs::read_dir(&root) else {
        return;
    };
    let self_pid = std::process::id();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix("collide-") else {
            continue;
        };
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

#[derive(Debug, Default, PartialEq, Eq)]
pub struct MergeTreeOutput {
    pub tree: String,
    pub conflicted: Vec<String>,
    pub conflict_types: Vec<String>,
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
    // authoritative answer, these only add the machine-stable type token.
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
        out.conflict_types.push(lossy(fields[type_at]));
        i = type_at + 2; // skip the human message too
    }
    out.conflict_types.dedup();
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
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
