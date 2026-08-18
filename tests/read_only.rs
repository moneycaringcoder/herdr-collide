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
    /// Every file in each common git dir (excluding its object store) and in
    /// every working tree, untracked and ignored files included.
    files: BTreeMap<PathBuf, FileStamp>,
    /// Every file in every real object store, by path.
    odb: BTreeSet<PathBuf>,
    /// Refs and worktree registrations as git reports them, keyed by common dir.
    refs: BTreeMap<PathBuf, String>,
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
    let checkouts: Vec<&Path> = std::iter::once(fixture.repo.as_path())
        .chain(worktrees.iter().map(PathBuf::as_path))
        .collect();
    let mut repositories: BTreeMap<PathBuf, &Path> = BTreeMap::new();
    for checkout in &checkouts {
        let raw = PathBuf::from(fixture.git(
            checkout,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        ));
        let common_dir = std::fs::canonicalize(&raw).unwrap_or(raw);
        repositories.entry(common_dir).or_insert(checkout);
    }

    let mut files = BTreeMap::new();
    let mut locks = BTreeSet::new();
    let mut odb = BTreeSet::new();
    for common_dir in repositories.keys() {
        let objects = common_dir.join("objects");
        // Object stores are compared as name sets instead of file-by-file, so
        // each one is excluded from the byte walk.
        walk(
            common_dir,
            std::slice::from_ref(&objects),
            &mut files,
            &mut locks,
        );
        collect_paths(&objects, &mut odb);
    }
    for wt in &checkouts {
        // `.git` is repository state rather than worktree content. Each
        // repository owning one is already covered by its common-dir walk.
        walk(wt, &[wt.join(".git")], &mut files, &mut locks);
    }

    let mut index_bytes = BTreeMap::new();
    let mut index_mtimes = BTreeMap::new();
    for wt in &checkouts {
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

    let mut refs = BTreeMap::new();
    for (common_dir, checkout) in &repositories {
        let mut observed = fixture.git(
            checkout,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname) %(objecttype)",
            ],
        );
        observed.push('\n');
        observed.push_str(&fixture.git(checkout, &["worktree", "list", "--porcelain"]));
        refs.insert(common_dir.clone(), observed);
    }

    let mut reflogs = BTreeMap::new();
    let mut reflog_locks = BTreeSet::new();
    for common_dir in repositories.keys() {
        walk(
            &common_dir.join("logs"),
            &[],
            &mut reflogs,
            &mut reflog_locks,
        );
    }
    for wt in &checkouts {
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
            "refs changed:\n--- before\n{:?}\n--- after\n{:?}",
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
fn nested_prediction_leaves_both_submodule_repositories_unchanged() {
    let _serialised = scratch_guard();

    let fixture = Fixture::new("submodule-read-only");
    let (_superproject, first, second, first_submodule) =
        fixture.superproject_with_submodule("embedded");
    let second_submodule = second.join("embedded");
    fixture.write(&first_submodule, "payload.txt", "FIRST-LONG\nbeta\ngamma\n");
    fixture.write(
        &second_submodule,
        "payload.txt",
        "SECOND-LONGER\nbeta\ngamma\n",
    );

    let pipeline_worktrees = vec![first.clone(), second.clone()];
    // Both nested clones are protected independently: each has its own index,
    // refs, reflogs and object store, and neither is a herdr checkout.
    let protected_checkouts = vec![
        first,
        second,
        first_submodule.clone(),
        second_submodule.clone(),
    ];
    let nested: Vec<(PathBuf, PathBuf)> = [&first_submodule, &second_submodule]
        .iter()
        .map(|checkout| {
            let git_dir = PathBuf::from(fixture.git(
                checkout,
                &["rev-parse", "--path-format=absolute", "--git-dir"],
            ));
            let raw_common_dir = PathBuf::from(fixture.git(
                checkout,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            ));
            let common_dir = std::fs::canonicalize(&raw_common_dir).unwrap_or(raw_common_dir);
            (git_dir, common_dir)
        })
        .collect();

    let before = fingerprint(&fixture, &protected_checkouts);
    for (git_dir, common_dir) in &nested {
        let index = git_dir.join("index");
        assert!(
            before.index_bytes.contains_key(&index) && before.index_mtimes.contains_key(&index),
            "nested index bytes and mtime were not fingerprinted: {}",
            index.display()
        );
        assert!(
            before.refs.contains_key(common_dir),
            "nested refs were not fingerprinted under {}",
            common_dir.display()
        );
        assert!(
            before.reflogs.keys().any(|path| {
                path.starts_with(common_dir.join("logs")) || path.starts_with(git_dir.join("logs"))
            }),
            "nested reflogs were not fingerprinted under {} or {}",
            common_dir.join("logs").display(),
            git_dir.join("logs").display()
        );
        assert!(
            before
                .odb
                .iter()
                .any(|path| path.starts_with(common_dir.join("objects"))),
            "nested object paths were not fingerprinted under {}",
            common_dir.join("objects").display()
        );
    }

    let notes = run_full_pipeline(&fixture, &pipeline_worktrees);
    assert!(
        notes
            .iter()
            .any(|note| note.contains("nested paths `payload.txt` conflict")),
        "nested prediction did not run: {notes:?}"
    );
    assert_eq!(
        notes
            .iter()
            .filter(|note| note.contains("submodule `embedded`"))
            .count(),
        1,
        "nested prediction produced unexpected notes: {notes:?}"
    );

    let after = fingerprint(&fixture, &protected_checkouts);
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

    // `Predictor::new` falls back to the system temp dir when the state dir
    // cannot be created, and sweeping only the state dir leaked those forever.
    let dead_fallback = std::env::temp_dir().join("collide-4294967290-999998");
    std::fs::create_dir_all(dead_fallback.join("odb")).expect("create dead fallback scratch");

    // A neighbour that merely shares the prefix. Its first segment is not a pid,
    // so it is none of our business — the fixtures themselves are named this way.
    let neighbour = std::env::temp_dir().join("collide-fixture-not-a-pid-1");
    std::fs::create_dir_all(&neighbour).expect("create neighbour dir");

    // A live predictor's directory, which must survive the sweep.
    let live = Predictor::new(TIMEOUT).expect("predictor");
    let live_dir = live.scratch_dir().to_path_buf();
    assert!(live_dir.exists());

    git::sweep_scratch();

    assert!(!dead.exists(), "a dead run's scratch directory survived");
    assert!(
        !dead_fallback.exists(),
        "a dead run's temp-dir fallback survived the sweep"
    );
    assert!(
        neighbour.exists(),
        "the sweep deleted a directory that only shares the prefix"
    );
    assert!(
        live_dir.exists(),
        "sweep deleted a live run's scratch directory"
    );
    let _ = std::fs::remove_dir_all(&neighbour);
    drop(live);
    assert!(!live_dir.exists());
}

// ---------------------------------------------------------------------------
// The deadline, and what git leaves behind
// ---------------------------------------------------------------------------

/// The timeout used to kill the child and then block forever anyway.
///
/// A pipe reaches EOF only when every holder of its write end has closed it, so
/// a process git leaves behind — here a `core.fsmonitor` hook's background
/// child, in the wild a clean filter that daemonises or a credential helper —
/// kept the drain threads reading long after git itself was gone. Measured
/// before the fix: a `git status` that git completes in milliseconds took the
/// full lifetime of the holder to return through `run_git`.
///
/// In the daemon that is not a wrong answer but a silent stop: the refresh loop
/// parks, every badge freezes at its last value, and nothing is written to
/// `notes`. A frozen collide is indistinguishable from a quiet repository.
#[test]
fn a_git_that_leaks_a_child_holding_the_pipe_cannot_park_the_refresh_loop() {
    let fixture = Fixture::new("pipe-leak");
    let wt = fixture.worktree("leaky", "leaky");
    let holder_seconds = fixture.leaking_fsmonitor(&wt, 90);
    fixture.write(&wt, "conflict.txt", "dirty\nbeta\ngamma\n");

    // Sanity: git itself is fast. Both streams go to /dev/null rather than to a
    // pipe, so the leaked holder has nothing of ours to keep open and this
    // measures git alone — `Fixture::git` would inherit the very bug under test.
    let bare = std::time::Instant::now();
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&wt)
        .args(["--no-optional-locks", "status", "--porcelain=v2"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("spawn git status");
    assert!(status.success(), "fixture status failed: {status:?}");
    assert!(
        bare.elapsed() < Duration::from_secs(5),
        "git itself is slow here; the fixture is not measuring what it claims"
    );

    let started = std::time::Instant::now();
    let set = git::change_set(&wt, "main", Duration::from_secs(2));
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(30),
        "the call took {elapsed:?} with a 2 s git timeout and a {holder_seconds} s holder; \
         the deadline is not a deadline"
    );
    // And the answer is not merely fast, it is complete: killing the process
    // group releases the pipe, so the output is drained rather than truncated.
    let set = set.expect("change set");
    assert!(
        set.paths.iter().any(|p| p.path == "conflict.txt"),
        "output was lost rather than drained: {:?}",
        set.paths
    );
}

/// The snapshot's `git add` runs every configured content filter, which is
/// arbitrary user code — and for git-lfs, code that writes into the user's own
/// `.git/lfs`. A tool whose whole claim is that it changes nothing cannot
/// execute them on every refresh cycle.
///
/// `tests/read_only.rs` could not catch this before because no fixture defined a
/// filter, which is exactly the shape of guarantee that passes CI and fails in
/// the wild. `required = true` is git-lfs's default and matters: emptying
/// `clean`/`process` while it is set makes `add` fail outright.
#[test]
fn the_snapshot_never_runs_the_repositorys_content_filters() {
    let _serialised = scratch_guard();

    let fixture = Fixture::new("filters");
    let a = fixture.worktree("filter-a", "filter-a");
    let b = fixture.worktree("filter-b", "filter-b");
    let log = fixture.recording_clean_filter();
    fixture.filtered_payload(&a, "media bytes\n");
    fixture.filtered_payload(&b, "media bytes\n");
    assert!(!log.exists(), "the filter ran before the test started");

    let worktrees = vec![fixture.repo.clone(), a.clone(), b.clone()];
    let before = fingerprint(&fixture, &worktrees);

    let notes = run_full_pipeline(&fixture, &worktrees);
    assert!(notes.is_empty(), "pipeline reported problems: {notes:?}");

    assert!(
        !log.exists(),
        "the clean filter ran during a read-only pass:\n{}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
    let after = fingerprint(&fixture, &worktrees);
    assert_unchanged(&before, &after);
    assert_no_scratch_leftovers();

    // The prediction still works with filters off: both sides added the same new
    // path with the same content, so the pair merges rather than conflicting,
    // and neither the payload nor the attributes file went missing.
    let set = git::change_set(&a, "main", TIMEOUT).expect("change set");
    let paths: Vec<&str> = set.paths.iter().map(|p| p.path.as_str()).collect();
    assert!(paths.contains(&"payload.bin"), "{paths:?}");
    assert!(paths.contains(&".gitattributes"), "{paths:?}");
}
