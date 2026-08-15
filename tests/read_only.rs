//! Proof of the plugin's central safety claim: running the full git pipeline
//! against a repository changes nothing in it.
//!
//! Every other test here checks that collide computes the right answer. These
//! check that computing it costs the user nothing — no index writeback, no
//! stray object, no touched file, no leftover lock. The claim used to live only
//! in prose, and prose does not fail CI.
//!
//! The fingerprint deliberately covers more than the assertions strictly need:
//! the whole common git directory (minus the object store, which is compared by
//! name set), every index, every working-tree file including untracked and
//! ignored ones, and every ref and reflog. Anything git writes shows up.

#[path = "fixtures.rs"]
mod fixtures;

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use collide::collide::gather_for;
use collide::config::Config;
use collide::git::{self, Predictor};
use collide::model::Checkout;

use fixtures::{checkout, Fixture};

const TIMEOUT: Duration = Duration::from_secs(60);

fn config() -> Config {
    Config {
        predict_conflicts: true,
        ..Config::default()
    }
}

// ---------------------------------------------------------------------------
// Fingerprinting
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    hash: u64,
    len: u64,
}

fn hash_of(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn stamp(path: &Path) -> FileStamp {
    let bytes = std::fs::read(path).unwrap_or_default();
    FileStamp {
        hash: hash_of(&bytes),
        len: bytes.len() as u64,
    }
}

/// Everything about a repository that a read-only operation must leave alone.
#[derive(Debug)]
struct Fingerprint {
    /// Full bytes of every index file, so a failure can say what changed rather
    /// than only that something did.
    index_bytes: BTreeMap<PathBuf, Vec<u8>>,
    /// mtime of every index file. A stat-cache writeback moves this even when
    /// the contents happen to round-trip identically, so it is asserted
    /// separately rather than trusted as the only signal.
    index_mtimes: BTreeMap<PathBuf, SystemTime>,
    /// Every file in the common git dir (excluding the object store) and in
    /// every working tree, untracked and ignored files included.
    files: BTreeMap<PathBuf, FileStamp>,
    /// Every file in the real object store, by path.
    odb: BTreeSet<PathBuf>,
    /// Refs and reflogs as git itself reports them.
    refs: String,
    reflogs: BTreeMap<PathBuf, FileStamp>,
    /// Any `*.lock` present. Excluded from `files` because a lock is transient
    /// by nature; tracked here so leftovers are still caught.
    locks: BTreeSet<PathBuf>,
}

fn walk(
    root: &Path,
    exclude: &[PathBuf],
    files: &mut BTreeMap<PathBuf, FileStamp>,
    locks: &mut BTreeSet<PathBuf>,
) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if exclude.contains(&path) {
            continue;
        }
        match entry.file_type() {
            Ok(t) if t.is_dir() => walk(&path, exclude, files, locks),
            Ok(_) => {
                if path.extension().map(|e| e == "lock").unwrap_or(false) {
                    locks.insert(path);
                } else {
                    let s = stamp(&path);
                    files.insert(path, s);
                }
            }
            Err(_) => {}
        }
    }
}

fn collect_paths(root: &Path, out: &mut BTreeSet<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => collect_paths(&path, out),
            Ok(_) => {
                out.insert(path);
            }
            Err(_) => {}
        }
    }
}

fn fingerprint(fixture: &Fixture, worktrees: &[PathBuf]) -> Fingerprint {
    let common_dir = PathBuf::from(fixture.git(
        &fixture.repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ));
    let objects = common_dir.join("objects");

    let mut files = BTreeMap::new();
    let mut locks = BTreeSet::new();
    // The object store is compared as a name set instead of file-by-file, so it
    // is excluded from the byte walk.
    walk(
        &common_dir,
        std::slice::from_ref(&objects),
        &mut files,
        &mut locks,
    );
    for wt in worktrees {
        // `.git` inside a linked worktree is a gitlink file, and inside the main
        // worktree it is the git dir itself; either way it is repository state,
        // already covered by the common-dir walk.
        walk(wt, &[wt.join(".git")], &mut files, &mut locks);
    }

    let mut odb = BTreeSet::new();
    collect_paths(&objects, &mut odb);

    let mut index_bytes = BTreeMap::new();
    let mut index_mtimes = BTreeMap::new();
    for wt in std::iter::once(&fixture.repo).chain(worktrees.iter()) {
        let git_dir =
            PathBuf::from(fixture.git(wt, &["rev-parse", "--path-format=absolute", "--git-dir"]));
        let index = git_dir.join("index");
        if let Ok(bytes) = std::fs::read(&index) {
            index_bytes.insert(index.clone(), bytes);
            if let Ok(mtime) = std::fs::metadata(&index).and_then(|m| m.modified()) {
                index_mtimes.insert(index, mtime);
            }
        }
    }

    let mut refs = fixture.git(
        &fixture.repo,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname) %(objecttype)",
        ],
    );
    refs.push('\n');
    refs.push_str(&fixture.git(&fixture.repo, &["worktree", "list", "--porcelain"]));

    let mut reflogs = BTreeMap::new();
    let mut reflog_locks = BTreeSet::new();
    walk(
        &common_dir.join("logs"),
        &[],
        &mut reflogs,
        &mut reflog_locks,
    );
    for wt in worktrees {
        let git_dir =
            PathBuf::from(fixture.git(wt, &["rev-parse", "--path-format=absolute", "--git-dir"]));
        walk(&git_dir.join("logs"), &[], &mut reflogs, &mut reflog_locks);
    }

    Fingerprint {
        index_bytes,
        index_mtimes,
        files,
        odb,
        refs,
        reflogs,
        locks,
    }
}

/// Reports every difference, so one run names all the damage instead of only
/// the first byte that moved.
fn assert_unchanged(before: &Fingerprint, after: &Fingerprint) {
    let mut problems: Vec<String> = Vec::new();

    // 1. Indexes, byte for byte, plus mtime.
    for (path, bytes) in &before.index_bytes {
        match after.index_bytes.get(path) {
            None => problems.push(format!("index removed: {}", path.display())),
            Some(now) if now != bytes => problems.push(format!(
                "index rewritten: {} ({} bytes -> {} bytes)",
                path.display(),
                bytes.len(),
                now.len()
            )),
            Some(_) => {}
        }
    }
    for path in after.index_bytes.keys() {
        if !before.index_bytes.contains_key(path) {
            problems.push(format!("index created: {}", path.display()));
        }
    }
    for (path, mtime) in &before.index_mtimes {
        if after.index_mtimes.get(path) != Some(mtime) {
            problems.push(format!(
                "index mtime moved (stat-cache writeback): {}",
                path.display()
            ));
        }
    }

    // 2 and 3. Working trees, refs, reflogs, and the rest of the git dir.
    for (path, was) in &before.files {
        match after.files.get(path) {
            None => problems.push(format!("file removed: {}", path.display())),
            Some(now) if now != was => problems.push(format!("file modified: {}", path.display())),
            Some(_) => {}
        }
    }
    for path in after.files.keys() {
        if !before.files.contains_key(path) {
            problems.push(format!("file created: {}", path.display()));
        }
    }
    if before.refs != after.refs {
        problems.push(format!(
            "refs changed:\n--- before\n{}\n--- after\n{}",
            before.refs, after.refs
        ));
    }
    for (path, was) in &before.reflogs {
        if after.reflogs.get(path) != Some(was) {
            problems.push(format!("reflog changed: {}", path.display()));
        }
    }
    for path in after.reflogs.keys() {
        if !before.reflogs.contains_key(path) {
            problems.push(format!("reflog created: {}", path.display()));
        }
    }

    // 4. The object store. Growth here is the failure mode that matters most:
    // `git add` and `merge-tree --write-tree` both write objects unless the
    // GIT_OBJECT_DIRECTORY redirection is holding.
    let grew: Vec<&PathBuf> = after.odb.difference(&before.odb).collect();
    if !grew.is_empty() {
        problems.push(format!(
            "{} object(s) leaked into the user's ODB, first few: {:?}",
            grew.len(),
            grew.iter().take(5).collect::<Vec<_>>()
        ));
    }
    assert_eq!(
        before.odb.len(),
        after.odb.len(),
        "object count changed: {} -> {}",
        before.odb.len(),
        after.odb.len()
    );

    // 5. Locks. Anything new is a leftover.
    let new_locks: Vec<&PathBuf> = after.locks.difference(&before.locks).collect();
    if !new_locks.is_empty() {
        problems.push(format!("lock files left behind: {new_locks:?}"));
    }

    assert!(
        problems.is_empty(),
        "the repository was modified:\n  {}",
        problems.join("\n  ")
    );
}

/// The plugin state dir is shared by every collide process on the machine, so
/// the leftover check has to be scoped twice over: to this process's own
/// scratch directories (`collide-<pid>-<seq>`), and — via [`scratch_guard`] —
/// to a window in which no sibling test in this binary holds a live predictor.
/// Without both, a passing run only means the other tests happened to finish
/// first.
fn assert_no_scratch_leftovers() {
    let root = collide::config::state_dir().join("scratch");
    let ours = format!("collide-{}-", std::process::id());
    let mut leftovers = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(&ours) {
                let mut files = BTreeMap::new();
                let mut locks = BTreeSet::new();
                walk(&entry.path(), &[], &mut files, &mut locks);
                leftovers.push((entry.path(), files.len(), locks.len()));
            }
        }
    }
    assert!(
        leftovers.is_empty(),
        "scratch directories left under the plugin state dir: {leftovers:?}"
    );
}

/// Serialises the tests that assert about the shared plugin state dir. They
/// each create predictors under it, so they cannot judge each other's leftovers.
fn scratch_guard() -> std::sync::MutexGuard<'static, ()> {
    static GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
    GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// The pipeline under test
// ---------------------------------------------------------------------------

/// A repository with every worktree shape the pipeline has to handle, plus
/// untracked and ignored files that must survive untouched.
fn kitchen_sink(tag: &str) -> (Fixture, Vec<PathBuf>) {
    let fixture = Fixture::new(tag);

    // Committed conflict, committed clean overlap: the pure-commit path.
    let (ca, cb) = fixture.committed_conflict_pair();
    let (oa, ob) = fixture.committed_clean_overlap_pair();
    // Uncommitted conflict on both sides: forces the temp-index snapshot.
    let (da, db) = fixture.uncommitted_conflict_pair();
    // Uncommitted renames: forces the snapshot and the rename prefilter path.
    let (ra, rb) = fixture.uncommitted_rename_pair();
    // A merge in progress: an unmerged index, which cannot `write-tree` from
    // the raw copied index.
    let mid = fixture.merge_in_progress_worktree("mid-merge");
    let detached = fixture.detached_worktree("detached");
    fixture.write(&detached, "conflict.txt", "DETACHED\nbeta\ngamma\n");

    // Untracked and ignored files in the dirty worktrees. Untracked files are
    // staged by the snapshot, so they are the likeliest thing to get written
    // back somewhere they should not be.
    for wt in [&da, &db] {
        fixture.tricky_untracked(wt);
        fixture.ignored_files(wt);
        fixture.write(wt, "untracked-blob.bin", "\0\0\0binary\0\0\0");
    }

    let worktrees = vec![
        fixture.repo.clone(),
        ca,
        cb,
        oa,
        ob,
        da,
        db,
        ra,
        rb,
        mid,
        detached,
    ];
    (fixture, worktrees)
}

fn checkouts_for(fixture: &Fixture, worktrees: &[PathBuf]) -> Vec<Checkout> {
    let key = git::repo_key(&fixture.repo, TIMEOUT).expect("repo key");
    worktrees
        .iter()
        .enumerate()
        .map(|(i, path)| checkout(&format!("ws{i}"), path, &key.0))
        .collect()
}

/// Runs everything the plugin ever runs against a repository: change-set
/// collection for every worktree, then conflict prediction for every pair,
/// including the uncommitted-side snapshot.
fn run_full_pipeline(fixture: &Fixture, worktrees: &[PathBuf]) -> Vec<String> {
    let config = config();
    let mut notes = Vec::new();

    // The real entry point, which is what the daemon and `--once` call.
    let cycle = gather_for(checkouts_for(fixture, worktrees), &config).expect("gather");
    notes.extend(cycle.notes);

    // `gather_for` only primes worktrees that landed in a pairing, so drive the
    // predictor directly as well to guarantee every worktree is snapshotted and
    // every pair is predicted.
    let mut predictor = Predictor::new(config.git_timeout).expect("predictor");
    let mut primed = Vec::new();
    for wt in worktrees {
        match predictor.prime(wt) {
            Ok(()) => primed.push(wt.clone()),
            Err(err) => notes.push(format!("prime {}: {err}", wt.display())),
        }
    }
    for (i, left) in primed.iter().enumerate() {
        for right in primed.iter().skip(i + 1) {
            let set = git::change_set(left, "main", config.git_timeout).expect("change set");
            let paths: Vec<String> = set.paths.iter().map(|p| p.path.clone()).collect();
            if let Err(err) = predictor.predict_pair(left, right, &paths) {
                notes.push(format!(
                    "predict {} vs {}: {err}",
                    left.display(),
                    right.display()
                ));
            }
        }
    }
    notes
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn the_full_pipeline_changes_nothing_in_the_repository() {
    let _serialised = scratch_guard();

    let (fixture, worktrees) = kitchen_sink("read-only");

    let before = fingerprint(&fixture, &worktrees);
    assert!(
        before.odb.len() > 5,
        "fixture has no objects to protect: {}",
        before.odb.len()
    );

    let notes = run_full_pipeline(&fixture, &worktrees);
    assert!(notes.is_empty(), "pipeline reported problems: {notes:?}");

    let after = fingerprint(&fixture, &worktrees);
    assert_unchanged(&before, &after);
    assert_no_scratch_leftovers();
}

#[test]
fn a_worktree_mid_merge_is_snapshotted_without_touching_it() {
    let _serialised = scratch_guard();

    let fixture = Fixture::new("mid-merge-readonly");
    let mid = fixture.merge_in_progress_worktree("mid-merge");
    let (da, _db) = fixture.uncommitted_conflict_pair();
    let worktrees = vec![fixture.repo.clone(), mid.clone(), da.clone()];

    // Sanity: the fixture really is mid-merge, with an unmerged index.
    let status = fixture.git(&mid, &["status", "--porcelain"]);
    assert!(status.contains("UU conflict.txt"), "{status}");
    let set = git::change_set(&mid, "main", TIMEOUT).expect("change set");
    assert!(set
        .degraded_reason
        .as_deref()
        .unwrap_or_default()
        .contains(git::DEGRADED_UNMERGED));

    let before = fingerprint(&fixture, &worktrees);

    let mut predictor = Predictor::new(TIMEOUT).expect("predictor");
    predictor.prime(&mid).expect("snapshot an unmerged index");
    predictor.prime(&da).expect("prime");
    let prediction = predictor
        .predict_pair(&mid, &da, &["conflict.txt".to_string()])
        .expect("predict against an unmerged worktree");
    assert!(
        prediction.advisory,
        "a prediction against a mid-merge worktree must be marked advisory"
    );
    drop(predictor);

    let after = fingerprint(&fixture, &worktrees);
    assert_unchanged(&before, &after);
    assert_no_scratch_leftovers();

    // The conflict markers are still on disk exactly as the merge left them.
    let body = std::fs::read_to_string(mid.join("conflict.txt")).expect("read");
    assert!(body.contains("<<<<<<<"), "conflict markers were rewritten");
}

#[test]
fn concurrent_pipelines_are_safe_while_another_process_holds_index_lock() {
    let _serialised = scratch_guard();

    let (fixture, worktrees) = kitchen_sink("concurrent");

    // A separate process holding `index.lock` is exactly what an agent running
    // `git add` looks like from the outside. Nothing collide does may need that
    // lock, and nothing it does may disturb the holder.
    let mut holders = LockHolders::default();
    let mut lock_paths = Vec::new();
    for wt in [&fixture.repo, &worktrees[5]] {
        let git_dir =
            PathBuf::from(fixture.git(wt, &["rev-parse", "--path-format=absolute", "--git-dir"]));
        let lock = git_dir.join("index.lock");
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                ": > '{}'; exec sleep 120",
                lock.to_string_lossy().replace('\'', "'\\''")
            ))
            // Never inherit the harness's pipes: a leaked holder would keep
            // stdout open and hang `cargo test` long after the test finished.
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn lock holder");
        holders.0.push(child);
        lock_paths.push(lock);
    }
    // Wait for the locks to actually exist before measuring anything.
    for _ in 0..200 {
        if lock_paths.iter().all(|p| p.exists()) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    for lock in &lock_paths {
        assert!(lock.exists(), "lock holder never took {}", lock.display());
    }

    let before = fingerprint(&fixture, &worktrees);

    let notes: Vec<String> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|_| scope.spawn(|| run_full_pipeline(&fixture, &worktrees)))
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("a pipeline thread panicked"))
            .collect()
    });
    assert!(
        notes.is_empty(),
        "concurrent pipelines reported problems: {notes:?}"
    );

    let after = fingerprint(&fixture, &worktrees);
    assert_unchanged(&before, &after);
    assert_no_scratch_leftovers();

    // The holder still owns its locks; collide never stole or cleared them.
    for lock in &lock_paths {
        assert!(
            lock.exists(),
            "collide removed a lock it did not take: {}",
            lock.display()
        );
    }
    drop(holders);
    for lock in &lock_paths {
        let _ = std::fs::remove_file(lock);
    }
}

/// Reaps the lock-holding processes even when an assertion above panics, so a
/// failing test never leaves `sleep` children behind.
#[derive(Default)]
struct LockHolders(Vec<std::process::Child>);

impl Drop for LockHolders {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn sweep_scratch_reclaims_dead_runs_and_spares_live_ones() {
    let _serialised = scratch_guard();

    let root = collide::config::state_dir().join("scratch");
    std::fs::create_dir_all(&root).expect("create scratch root");

    // A directory named for a pid that cannot be running.
    let dead = root.join("collide-4294967290-999999");
    std::fs::create_dir_all(dead.join("odb")).expect("create dead scratch");
    std::fs::write(dead.join("index-0"), b"stale").expect("write stale index");
    std::fs::write(dead.join("index-0.lock"), b"").expect("write stale lock");

    // A live predictor's directory, which must survive the sweep.
    let live = Predictor::new(TIMEOUT).expect("predictor");
    let live_dir = live.scratch_dir().to_path_buf();
    assert!(live_dir.exists());

    git::sweep_scratch();

    assert!(!dead.exists(), "a dead run's scratch directory survived");
    assert!(
        live_dir.exists(),
        "sweep deleted a live run's scratch directory"
    );
    drop(live);
    assert!(!live_dir.exists());
}
