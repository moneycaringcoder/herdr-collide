//! Throwaway git fixtures for the integration tests.
//!
//! Everything lives under a unique temp directory that is removed on drop, and
//! every fixture repository gets its own local `user.name`, `user.email` and
//! neutralised `core.excludesFile`/`core.hooksPath`, so a CI runner with no git
//! identity and no global config still produces the same results as a developer
//! laptop. The fixture's own git invocations additionally run with `HOME`,
//! `GIT_CONFIG_GLOBAL` and `GIT_CONFIG_SYSTEM` pointed inside the temp tree.
//!
//! This file is included by the other test binaries with
//! `#[path = "fixtures.rs"] mod fixtures;`, so cargo also builds it as an
//! integration test target of its own with no tests in it. That is harmless.

#![allow(dead_code)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A temp directory containing one or more git repositories.
pub struct Fixture {
    root: PathBuf,
    /// The main worktree of the primary repository.
    pub repo: PathBuf,
}

impl Fixture {
    /// A repository on `main` with a base commit containing the files the
    /// scenarios below build on.
    pub fn new(tag: &str) -> Fixture {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "collide-fixture-{}-{tag}-{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).expect("create fixture root");
        // An empty global config file, so nothing on the host leaks in.
        std::fs::write(root.join("home/.gitconfig"), "").expect("write empty global config");
        std::fs::write(root.join("empty-excludes"), "").expect("write empty excludes");
        std::fs::create_dir_all(root.join("no-hooks")).expect("create empty hooks dir");

        let fixture = Fixture {
            root: root.clone(),
            repo: root.join("repo"),
        };
        fixture.init_repo(&fixture.repo);

        fixture.write(&fixture.repo, ".gitignore", "ignored/\n*.log\n");
        fixture.write(
            &fixture.repo,
            "shared.txt",
            &(1..=12).map(|n| format!("line {n}\n")).collect::<String>(),
        );
        fixture.write(&fixture.repo, "conflict.txt", "alpha\nbeta\ngamma\n");
        // A subdirectory sorting *after* `conflict.txt` is not decoration: it
        // is what makes `quiet_trap_pair` reproduce the merge-tree `--quiet`
        // unsoundness. See docs/git-plumbing.md, "The --quiet trap".
        fixture.write(&fixture.repo, "docs/notes-a.md", "notes a\n");
        fixture.write(&fixture.repo, "docs/notes-b.md", "notes b\n");
        fixture.write(&fixture.repo, "renamed.txt", "movable\n");
        fixture.write(&fixture.repo, "Cargo.lock", "# lockfile\nversion = 1\n");
        fixture.git(&fixture.repo, &["add", "-A"]);
        fixture.commit(&fixture.repo, "base");
        fixture
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Creates and configures a fresh repository at `path`.
    pub fn init_repo(&self, path: &Path) {
        std::fs::create_dir_all(path).expect("create repo dir");
        self.git(path, &["init", "-q", "-b", "main"]);
        self.configure_repo(path);
    }

    fn configure_repo(&self, path: &Path) {
        // Local config only, so a runner with no identity still commits.
        self.git(path, &["config", "user.email", "fixture@example.invalid"]);
        self.git(path, &["config", "user.name", "collide fixture"]);
        self.git(path, &["config", "init.defaultBranch", "main"]);
        self.git(path, &["config", "commit.gpgsign", "false"]);
        self.git(path, &["config", "tag.gpgsign", "false"]);
        self.git(path, &["config", "gc.auto", "0"]);
        // Neutralise anything the host would otherwise contribute.
        self.git(
            path,
            &[
                "config",
                "core.excludesFile",
                self.root.join("empty-excludes").to_str().unwrap(),
            ],
        );
        self.git(
            path,
            &[
                "config",
                "core.hooksPath",
                self.root.join("no-hooks").to_str().unwrap(),
            ],
        );
    }

    /// Runs git, panicking with the stderr on failure. Returns trimmed stdout.
    pub fn git(&self, cwd: &Path, args: &[&str]) -> String {
        let (code, stdout, stderr) = self.try_git(cwd, args);
        assert_eq!(
            code,
            0,
            "git {} failed in {}: {stderr}",
            args.join(" "),
            cwd.display()
        );
        stdout
    }

    pub fn try_git(&self, cwd: &Path, args: &[&str]) -> (i32, String, String) {
        let out = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("home/.config"))
            .env("GIT_CONFIG_GLOBAL", self.root.join("home/.gitconfig"))
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "collide fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "collide fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00+0000")
            .env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00+0000")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .expect("spawn git");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        )
    }

    /// Writes a file, creating parent directories. `rel` is raw bytes, so it
    /// may contain a newline.
    pub fn write(&self, cwd: &Path, rel: &str, contents: &str) {
        let path = cwd.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        let mut file = std::fs::File::create(&path)
            .unwrap_or_else(|e| panic!("create {}: {e}", path.display()));
        file.write_all(contents.as_bytes()).expect("write file");
    }

    pub fn commit(&self, cwd: &Path, message: &str) {
        self.git(cwd, &["commit", "-q", "-m", message]);
    }

    pub fn commit_all(&self, cwd: &Path, message: &str) {
        self.git(cwd, &["add", "-A"]);
        self.commit(cwd, message);
    }

    /// A linked worktree on a new branch off `main`.
    pub fn worktree(&self, name: &str, branch: &str) -> PathBuf {
        let path = self.root.join(name);
        self.git(
            &self.repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                branch,
                path.to_str().unwrap(),
                "main",
            ],
        );
        path
    }

    /// A linked worktree *inside* the main worktree, under `.worktrees/`.
    ///
    /// This is the layout most agent-per-worktree setups use, so that the
    /// worktrees travel with the repository, and it is the one a sibling
    /// worktree cannot stand in for: the path sits underneath the main
    /// worktree's, so anything deciding "same working tree?" by path prefix
    /// wrongly calls it the same tree and stops comparing the two.
    ///
    /// The exclusion goes in `.git/info/exclude` rather than a committed
    /// `.gitignore`, so the base commit every other fixture shares is untouched
    /// and the nesting does not show up as a change of its own.
    pub fn nested_worktree(&self, name: &str, branch: &str) -> PathBuf {
        let exclude = self.repo.join(".git/info/exclude");
        if let Some(parent) = exclude.parent() {
            std::fs::create_dir_all(parent).expect("info dir");
        }
        std::fs::write(&exclude, ".worktrees/\n").expect("write exclude");

        let path = self.repo.join(".worktrees").join(name);
        self.git(
            &self.repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                branch,
                path.to_str().unwrap(),
                "main",
            ],
        );
        path
    }

    /// A linked worktree with a detached HEAD.
    pub fn detached_worktree(&self, name: &str) -> PathBuf {
        let path = self.root.join(name);
        self.git(
            &self.repo,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                path.to_str().unwrap(),
                "main",
            ],
        );
        path
    }

    /// A linked worktree whose branch has never had a commit.
    pub fn unborn_worktree(&self, name: &str, branch: &str) -> PathBuf {
        let path = self.root.join(name);
        self.git(
            &self.repo,
            &[
                "worktree",
                "add",
                "-q",
                "--orphan",
                "-b",
                branch,
                path.to_str().unwrap(),
            ],
        );
        path
    }

    /// A linked worktree whose branch was deleted underneath it. Byte-identical
    /// to the unborn case for `symbolic-ref`, which is exactly why it needs its
    /// own fixture.
    pub fn deleted_branch_worktree(&self, name: &str, branch: &str) -> PathBuf {
        let path = self.worktree(name, branch);
        self.git(
            &self.repo,
            &["update-ref", "-d", &format!("refs/heads/{branch}")],
        );
        path
    }

    /// A linked worktree whose HEAD is a symref to a ref that exists but whose
    /// object does not. The genuinely broken case, as distinct from a branch
    /// that simply has no commit.
    pub fn dangling_head_worktree(&self, name: &str, branch: &str) -> PathBuf {
        let path = self.worktree(name, branch);
        // Write the ref by hand rather than through git: no plumbing command
        // will point a branch at an object that is not there, which is the whole
        // point of the state.
        let common_dir = PathBuf::from(self.git(
            &path,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        ));
        let ref_path = common_dir.join("refs/heads").join(branch);
        std::fs::create_dir_all(ref_path.parent().expect("refs/heads")).expect("create refs/heads");
        std::fs::write(&ref_path, "0123456789abcdef0123456789abcdef01234567\n")
            .expect("write dangling ref");
        path
    }

    /// A linked worktree whose `.git/HEAD` is not a ref at all. Every HEAD probe
    /// then exits 128 — "I could not look", which must never be read as "there
    /// is no commit".
    pub fn garbage_head_worktree(&self, name: &str) -> PathBuf {
        let path = self.worktree(name, name);
        let git_dir =
            PathBuf::from(self.git(&path, &["rev-parse", "--path-format=absolute", "--git-dir"]));
        std::fs::write(git_dir.join("HEAD"), "this is not a ref\n").expect("write garbage HEAD");
        path
    }

    /// A worktree switched to an orphan branch *in place*, so it is genuinely
    /// unborn while still carrying the reflog of the branch it came from. The
    /// counter-example that disproves the `logs/HEAD` discriminator in one
    /// direction.
    pub fn orphaned_in_place_worktree(&self, name: &str, branch: &str) -> PathBuf {
        let path = self.worktree(name, name);
        self.git(&path, &["checkout", "-q", "--orphan", branch]);
        path
    }

    /// A repository with reflogging switched off, in which a branch is then
    /// deleted underneath its worktree. The counter-example that disproves the
    /// `logs/HEAD` discriminator in the other direction: the branch really was
    /// deleted, and there is no reflog to say so.
    pub fn deleted_branch_worktree_without_reflog(&self, name: &str, branch: &str) -> PathBuf {
        self.git(&self.repo, &["config", "core.logAllRefUpdates", "false"]);
        let path = self.worktree(name, branch);
        self.git(
            &self.repo,
            &["update-ref", "-d", &format!("refs/heads/{branch}")],
        );
        self.git(&self.repo, &["config", "core.logAllRefUpdates", "true"]);
        path
    }

    /// A repository whose trunk is named something the probe chain does not
    /// guess, with no remotes at all. Measured against `HEAD` this reports every
    /// workspace as clean however much they conflict.
    pub fn trunkless_repo(&self, name: &str, branch: &str) -> PathBuf {
        let path = self.root.join(name);
        std::fs::create_dir_all(&path).expect("create repo dir");
        self.git(&path, &["init", "-q", "-b", branch]);
        self.git(&path, &["config", "user.email", "fixture@example.invalid"]);
        self.git(&path, &["config", "user.name", "collide fixture"]);
        self.git(&path, &["config", "commit.gpgsign", "false"]);
        self.git(
            &path,
            &[
                "config",
                "core.excludesFile",
                self.root.join("empty-excludes").to_str().unwrap(),
            ],
        );
        self.git(
            &path,
            &[
                "config",
                "core.hooksPath",
                self.root.join("no-hooks").to_str().unwrap(),
            ],
        );
        self.write(&path, "conflict.txt", "alpha\nbeta\ngamma\n");
        self.git(&path, &["add", "-A"]);
        self.commit(&path, "base");
        path
    }

    /// A completely unrelated repository, with the same file names so that a
    /// naive path intersection would happily pair it with the primary repo.
    pub fn foreign_repo(&self, name: &str) -> PathBuf {
        let path = self.root.join(name);
        self.init_repo(&path);
        self.write(&path, "conflict.txt", "unrelated\nbeta\ngamma\n");
        self.write(&path, "shared.txt", "different\n");
        self.git(&path, &["add", "-A"]);
        self.commit(&path, "foreign base");
        path
    }

    /// A repository whose working tree and common git directory are siblings,
    /// plus one linked worktree of that repository.
    pub fn separate_git_dir_repo(&self, name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let repo = self.root.join(name);
        let store = self.root.join(format!("{name}-store"));
        std::fs::create_dir_all(&repo).expect("create separate-git-dir worktree");
        let separate_git_dir = format!("--separate-git-dir={}", store.display());
        self.git(
            &repo,
            &["init", "-q", "-b", "main", separate_git_dir.as_str()],
        );
        self.configure_repo(&repo);
        self.write(&repo, "base.txt", "base\n");
        self.commit_all(&repo, "base");

        let linked = self.root.join(format!("{name}-linked"));
        let branch = format!("{name}-linked");
        self.git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                branch.as_str(),
                linked.to_str().unwrap(),
                "main",
            ],
        );
        (repo, store, linked)
    }

    /// The primary repository as a superproject, with two linked worktrees and
    /// a real checkout of its independently initialized submodule.
    pub fn superproject_with_submodule(&self, name: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let source = self.root.join(format!("{name}-source"));
        self.init_repo(&source);
        self.write(&source, "payload.txt", "alpha\nbeta\ngamma\n");
        self.commit_all(&source, "submodule base");

        let (code, _stdout, stderr) = self.try_git(
            &self.repo,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                source.to_str().unwrap(),
                name,
            ],
        );
        assert_eq!(
            code,
            0,
            "git refused local-path submodule {} in {}: {stderr}",
            source.display(),
            self.repo.display()
        );
        self.commit_all(&self.repo, "add submodule");

        let first_name = format!("{name}-super-a");
        let first_branch = format!("{name}/super-a");
        let first = self.worktree(&first_name, &first_branch);
        let second_name = format!("{name}-super-b");
        let second_branch = format!("{name}/super-b");
        let second = self.worktree(&second_name, &second_branch);
        for worktree in [&first, &second] {
            let (code, _stdout, stderr) = self.try_git(
                worktree,
                &[
                    "-c",
                    "protocol.file.allow=always",
                    "submodule",
                    "update",
                    "--init",
                    "--",
                    name,
                ],
            );
            assert_eq!(
                code,
                0,
                "git could not initialize local-path submodule in {}: {stderr}",
                worktree.display()
            );
        }

        (self.repo.clone(), first.clone(), second, first.join(name))
    }

    // -----------------------------------------------------------------------
    // Scenarios
    // -----------------------------------------------------------------------

    /// Two worktrees that committed different edits to the *same line* of
    /// `conflict.txt`. A real merge conflict.
    pub fn committed_conflict_pair(&self) -> (PathBuf, PathBuf) {
        let a = self.worktree("conflict-a", "conflict-a");
        self.write(&a, "conflict.txt", "ALPHA-A\nbeta\ngamma\n");
        self.commit_all(&a, "a edits line 1");

        let b = self.worktree("conflict-b", "conflict-b");
        self.write(&b, "conflict.txt", "ALPHA-B\nbeta\ngamma\n");
        self.commit_all(&b, "b edits line 1");
        (a, b)
    }

    /// Two worktrees that committed edits to *opposite ends* of `shared.txt`.
    /// The same file is touched by both, and it merges cleanly. This is the
    /// discrimination the whole plugin exists for.
    pub fn committed_clean_overlap_pair(&self) -> (PathBuf, PathBuf) {
        let a = self.worktree("clean-a", "clean-a");
        let mut top: String = (1..=12).map(|n| format!("line {n}\n")).collect();
        top = top.replacen("line 1\n", "TOP\n", 1);
        self.write(&a, "shared.txt", &top);
        self.commit_all(&a, "a edits the top");

        let b = self.worktree("clean-b", "clean-b");
        let bottom: String = (1..=12)
            .map(|n| {
                if n == 12 {
                    "BOTTOM\n".to_string()
                } else {
                    format!("line {n}\n")
                }
            })
            .collect();
        self.write(&b, "shared.txt", &bottom);
        self.commit_all(&b, "b edits the bottom");
        (a, b)
    }

    /// A worktree stopped mid-merge, with an unmerged index and conflict
    /// markers on disk. `write-tree` cannot run against the raw copied index in
    /// this state, so it is the case the snapshot path has to survive.
    pub fn merge_in_progress_worktree(&self, name: &str) -> PathBuf {
        // Build the branch to merge in through a scratch worktree, then drop
        // the worktree and keep the branch.
        let source = self.root.join(format!("{name}-source"));
        self.git(
            &self.repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &format!("{name}-source"),
                source.to_str().unwrap(),
                "main",
            ],
        );
        self.write(&source, "conflict.txt", "FROM-SOURCE\nbeta\ngamma\n");
        self.commit_all(&source, "source edits line 1");
        self.git(
            &self.repo,
            &["worktree", "remove", "--force", source.to_str().unwrap()],
        );

        let path = self.worktree(name, name);
        self.write(&path, "conflict.txt", "FROM-TARGET\nbeta\ngamma\n");
        self.commit_all(&path, "target edits line 1");
        let (code, _out, _err) =
            self.try_git(&path, &["merge", "--no-edit", &format!("{name}-source")]);
        assert_ne!(code, 0, "the fixture merge was supposed to conflict");
        assert!(
            path.join(".git").exists(),
            "worktree {name} lost its gitlink"
        );
        path
    }

    /// Two worktrees that each added the same new path with different content.
    pub fn add_add_pair(&self) -> (PathBuf, PathBuf) {
        let a = self.worktree("addadd-a", "addadd-a");
        self.write(&a, "brand-new.txt", "from a\n");
        self.commit_all(&a, "a adds");

        let b = self.worktree("addadd-b", "addadd-b");
        self.write(&b, "brand-new.txt", "from b\n");
        self.commit_all(&b, "b adds");
        (a, b)
    }

    /// Two worktrees that renamed the same file to different names.
    pub fn rename_rename_pair(&self) -> (PathBuf, PathBuf) {
        let a = self.worktree("rename-a", "rename-a");
        self.git(&a, &["mv", "renamed.txt", "renamed-a.txt"]);
        self.commit(&a, "a renames");

        let b = self.worktree("rename-b", "rename-b");
        self.git(&b, &["mv", "renamed.txt", "renamed-b.txt"]);
        self.commit(&b, "b renames");
        (a, b)
    }

    /// Two worktrees that renamed the same file to different names *without*
    /// committing, so the renames show up as `2` records in status. This is the
    /// case the path-intersection prefilter must not skip.
    pub fn uncommitted_rename_pair(&self) -> (PathBuf, PathBuf) {
        let a = self.worktree("wrename-a", "wrename-a");
        self.git(&a, &["mv", "renamed.txt", "wrenamed-a.txt"]);

        let b = self.worktree("wrename-b", "wrename-b");
        self.git(&b, &["mv", "renamed.txt", "wrenamed-b.txt"]);
        (a, b)
    }

    /// Two worktrees with *uncommitted* conflicting edits: nothing is committed
    /// on either side, so prediction has to go through the temp-index snapshot.
    pub fn uncommitted_conflict_pair(&self) -> (PathBuf, PathBuf) {
        let a = self.worktree("dirty-a", "dirty-a");
        self.write(&a, "conflict.txt", "DIRTY-A\nbeta\ngamma\n");

        let b = self.worktree("dirty-b", "dirty-b");
        self.write(&b, "conflict.txt", "DIRTY-B\nbeta\ngamma\n");
        (a, b)
    }

    /// Two worktrees whose *uncommitted* state reproduces the merge-tree
    /// `--quiet` unsoundness.
    ///
    /// The shape that matters: a file both sides edit incompatibly
    /// (`conflict.txt`), plus a one-sided edit on each side inside a
    /// subdirectory that sorts *after* it (`docs/`). `--quiet` stops once it
    /// has processed a directory both sides touched and reports the whole
    /// merge clean, losing the conflict on every path that sorts before it.
    /// Flat fixtures cannot reproduce this, which is exactly why the bug
    /// survived until it was run against real worktrees.
    pub fn quiet_trap_pair(&self) -> (PathBuf, PathBuf) {
        let a = self.worktree("trap-a", "trap-a");
        self.write(&a, "conflict.txt", "TRAP-A\nbeta\ngamma\n");
        self.write(&a, "docs/notes-a.md", "edited by a\n");

        let b = self.worktree("trap-b", "trap-b");
        self.write(&b, "conflict.txt", "TRAP-B\nbeta\ngamma\n");
        self.write(&b, "docs/notes-b.md", "edited by b\n");
        (a, b)
    }

    /// The documented `cp`-seeded temp-index snapshot, done independently of
    /// `git::Predictor`, so tests can compare merge-tree forms against the same
    /// real worktree trees the plugin builds. Objects are redirected into a
    /// scratch store, so this leaves the fixture repo's ODB alone.
    pub fn snapshot_tree(&self, checkout: &Path) -> String {
        let git_dir = PathBuf::from(self.git(
            checkout,
            &["rev-parse", "--path-format=absolute", "--git-dir"],
        ));
        let common_dir = PathBuf::from(self.git(
            checkout,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        ));
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let index = self.root.join(format!("snap-index-{seq}"));
        let _ = std::fs::remove_file(&index);
        let _ = std::fs::copy(git_dir.join("index"), &index);

        let odb = self.scratch_odb();
        let run = |args: &[&str]| -> String {
            let out = Command::new("git")
                .arg("-C")
                .arg(checkout)
                .args(args)
                .env("HOME", self.root.join("home"))
                .env("GIT_CONFIG_GLOBAL", self.root.join("home/.gitconfig"))
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_INDEX_FILE", &index)
                .env("GIT_OBJECT_DIRECTORY", &odb)
                .env(
                    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                    common_dir.join("objects"),
                )
                .output()
                .expect("spawn git");
            assert!(
                out.status.success(),
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        run(&["add", "-A", "--"]);
        let tree = run(&["write-tree"]);
        let _ = std::fs::remove_file(&index);
        let mut lock = index.into_os_string();
        lock.push(".lock");
        let _ = std::fs::remove_file(PathBuf::from(lock));
        tree
    }

    /// Scratch object store shared by this fixture's snapshot and merge-tree
    /// calls, so neither ever writes into the fixture repo.
    pub fn scratch_odb(&self) -> PathBuf {
        let odb = self.root.join("scratch-odb");
        std::fs::create_dir_all(odb.join("pack")).expect("create scratch odb");
        std::fs::create_dir_all(odb.join("info")).expect("create scratch odb");
        odb
    }

    /// Runs `merge-tree` against the scratch object store and returns its exit
    /// code and raw stdout.
    pub fn merge_tree(&self, checkout: &Path, args: &[&str]) -> (i32, Vec<u8>) {
        let common_dir = PathBuf::from(self.git(
            checkout,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        ));
        let out = Command::new("git")
            .arg("-C")
            .arg(checkout)
            .arg("merge-tree")
            .args(args)
            .env("HOME", self.root.join("home"))
            .env("GIT_CONFIG_GLOBAL", self.root.join("home/.gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_OBJECT_DIRECTORY", self.scratch_odb())
            .env(
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                common_dir.join("objects"),
            )
            .output()
            .expect("spawn merge-tree");
        (out.status.code().unwrap_or(-1), out.stdout)
    }

    /// Two worktrees with uncommitted edits to opposite ends of `shared.txt`.
    pub fn uncommitted_clean_pair(&self) -> (PathBuf, PathBuf) {
        let a = self.worktree("softdirty-a", "softdirty-a");
        let top: String = (1..=12)
            .map(|n| {
                if n == 1 {
                    "TOP\n".to_string()
                } else {
                    format!("line {n}\n")
                }
            })
            .collect();
        self.write(&a, "shared.txt", &top);

        let b = self.worktree("softdirty-b", "softdirty-b");
        let bottom: String = (1..=12)
            .map(|n| {
                if n == 12 {
                    "BOTTOM\n".to_string()
                } else {
                    format!("line {n}\n")
                }
            })
            .collect();
        self.write(&b, "shared.txt", &bottom);
        (a, b)
    }

    /// Untracked files exercising the awkward corners of `-z` framing: a space
    /// and a literal newline in the path. Returns the two relative paths.
    pub fn tricky_untracked(&self, cwd: &Path) -> (String, String) {
        let spaced = "dir with space/a file.txt".to_string();
        let newline = "weird\nname.txt".to_string();
        self.write(cwd, &spaced, "spaced\n");
        self.write(cwd, &newline, "newline\n");
        (spaced, newline)
    }

    /// Two untracked files whose names are *different* invalid UTF-8. Returns
    /// their raw byte names. Replacing the bad bytes maps both onto the same
    /// display string, so anything that keys on that string alone reports two
    /// worktrees as sharing a file neither of them has.
    ///
    /// Returns `None` when the filesystem refuses the names outright. macOS's
    /// APFS and HFS+ enforce valid UTF-8 in filenames and answer `EILSEQ`,
    /// where ext4 and friends take any byte but `/` and NUL. That is a real
    /// difference between the platforms this plugin supports, not a flaw in the
    /// test, so the caller skips the on-disk half rather than the suite failing
    /// on a machine where the situation cannot arise.
    #[cfg(unix)]
    pub fn distinct_invalid_utf8_untracked(&self, cwd: &Path) -> Option<(Vec<u8>, Vec<u8>)> {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        // The names differ *only* in the invalid byte, so replacement alone
        // renders them identically and nothing but the digest tells them apart.
        let first = b"\xff.txt".to_vec();
        let second = b"\xfe.txt".to_vec();
        for (name, body) in [(&first, "first\n"), (&second, "second\nsecond\n")] {
            let path = cwd.join(OsStr::from_bytes(name));
            if std::fs::write(&path, body).is_err() {
                return None;
            }
        }
        Some((first, second))
    }

    /// Files that `.gitignore` covers, which must never enter a change set.
    pub fn ignored_files(&self, cwd: &Path) {
        self.write(cwd, "ignored/artifact.bin", "junk\n");
        self.write(cwd, "build.log", "noise\n");
    }

    /// Two worktrees whose committed state shares no path at all, because one
    /// renamed a whole directory and the other added a file into the old one. A
    /// real `CONFLICT (directory rename suggested)` that a path intersection
    /// cannot see.
    pub fn committed_directory_rename_pair(&self) -> (PathBuf, PathBuf) {
        let a = self.worktree("dirrename-a", "dirrename-a");
        self.git(&a, &["mv", "docs", "guide"]);
        self.commit(&a, "a renames the directory");

        let b = self.worktree("dirrename-b", "dirrename-b");
        self.write(&b, "docs/notes-c.md", "notes c\n");
        self.commit_all(&b, "b adds into the old directory");
        (a, b)
    }

    /// Two worktrees that committed conflicting edits to *two* files, so
    /// merge-tree emits more than one conflict record — and therefore more than
    /// one `Auto-merging` record interleaved with them.
    pub fn two_file_conflict_pair(&self) -> (PathBuf, PathBuf) {
        let a = self.worktree("twofile-a", "twofile-a");
        self.write(&a, "conflict.txt", "A-ONE\nbeta\ngamma\n");
        self.write(&a, "renamed.txt", "A-TWO\n");
        self.commit_all(&a, "a edits both");

        let b = self.worktree("twofile-b", "twofile-b");
        self.write(&b, "conflict.txt", "B-ONE\nbeta\ngamma\n");
        self.write(&b, "renamed.txt", "B-TWO\n");
        self.commit_all(&b, "b edits both");
        (a, b)
    }

    /// Two worktrees of one repository with no common ancestor: one on `main`,
    /// one on an orphan branch that added the same file independently.
    pub fn unrelated_history_pair(&self) -> (PathBuf, PathBuf) {
        let a = self.worktree("unrelated-a", "unrelated-a");
        self.write(&a, "conflict.txt", "A-SIDE\nbeta\ngamma\n");
        self.commit_all(&a, "a edits");

        let b = self.root.join("unrelated-b");
        self.git(
            &self.repo,
            &[
                "worktree",
                "add",
                "-q",
                "--orphan",
                "-b",
                "unrelated-b",
                b.to_str().unwrap(),
            ],
        );
        self.write(&b, "conflict.txt", "B-SIDE\nbeta\ngamma\n");
        self.commit_all(&b, "b starts a history of its own");
        (a, b)
    }

    /// A `core.fsmonitor` hook that answers git immediately but leaves a
    /// background process behind holding git's stderr.
    ///
    /// This is the shape that turned a "hard deadline" into no deadline at all:
    /// git itself finishes in milliseconds, and the pipe it wrote to only
    /// reaches EOF when the last holder of the write end closes it. Returns the
    /// number of seconds the holder lives.
    pub fn leaking_fsmonitor(&self, cwd: &Path, holder_seconds: u32) -> u32 {
        let hook = self.root.join(format!(
            "fsmonitor-{}.sh",
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        // stdout goes to /dev/null so git is not kept waiting for the hook's own
        // output; stderr is deliberately inherited, which is the leak.
        std::fs::write(
            &hook,
            format!("#!/bin/sh\nsleep {holder_seconds} >/dev/null &\nprintf '/\\0'\n"),
        )
        .expect("write fsmonitor hook");
        self.make_executable(&hook);
        self.git(cwd, &["config", "core.fsmonitor", hook.to_str().unwrap()]);
        holder_seconds
    }

    /// Configures a `filter.<driver>.clean` for the whole repository that
    /// records every invocation into a log, in the shape git-lfs uses. Returns
    /// the log path, which must still not exist after a full pipeline run.
    ///
    /// Repository-wide on purpose: linked worktrees share one `config`, so
    /// configuring this per worktree would silently leave only the last one in
    /// force and the assertion on the others would prove nothing.
    ///
    /// `required = true` matters too — that is git-lfs's default, and a filter
    /// cannot be neutralised by emptying `clean`/`process` while it is set.
    pub fn recording_clean_filter(&self) -> PathBuf {
        let log = self.root.join("filter-ran.log");
        let script = self
            .root
            .join(format!("clean-{}.sh", SEQ.fetch_add(1, Ordering::Relaxed)));
        std::fs::write(
            &script,
            // The calling command is recorded where /proc allows it, so a
            // failure names the git invocation that ran the filter instead of
            // only counting it. `echo ran` is unconditional, so the log still
            // appears where /proc does not exist.
            format!(
                "#!/bin/sh\n{{ echo -n 'ran: '; tr '\\0' ' ' < /proc/$PPID/cmdline 2>/dev/null; \
                 echo; }} >> '{}'\nexec cat\n",
                log.to_string_lossy()
            ),
        )
        .expect("write clean filter");
        self.make_executable(&script);
        self.git(
            &self.repo,
            &["config", "filter.demo.clean", script.to_str().unwrap()],
        );
        self.git(&self.repo, &["config", "filter.demo.required", "true"]);
        log
    }

    /// Gives one worktree something for the filter above to bite on: an
    /// attributes file selecting the driver, plus a matching payload.
    pub fn filtered_payload(&self, cwd: &Path, body: &str) {
        self.write(cwd, ".gitattributes", "*.bin filter=demo\n");
        self.write(cwd, "payload.bin", body);
    }

    fn make_executable(&self, path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod +x");
        }
    }

    /// Byte-for-byte copy of a worktree's real index, for asserting that
    /// snapshotting left it alone.
    pub fn index_bytes(&self, cwd: &Path) -> Vec<u8> {
        let git_dir = self.git(cwd, &["rev-parse", "--path-format=absolute", "--git-dir"]);
        std::fs::read(Path::new(&git_dir).join("index")).unwrap_or_default()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A `Checkout` pointing at a fixture worktree, for the analysis tests.
pub fn checkout(id: &str, path: &Path, repo_key: &str) -> collide::model::Checkout {
    collide::model::Checkout {
        workspace_id: id.to_string(),
        workspace_label: id.to_string(),
        repo_key: collide::model::RepoKey(repo_key.to_string()),
        repo_root: path.to_path_buf(),
        checkout_path: path.to_path_buf(),
        is_linked_worktree: true,
        branch: Some(id.to_string()),
        agent: None,
    }
}

/// A `ChangeSet` from a plain list of paths, all `Unstaged`.
pub fn change_set(paths: &[&str]) -> collide::model::ChangeSet {
    collide::model::ChangeSet {
        paths: paths
            .iter()
            .map(|p| collide::model::ChangedPath::new(*p, collide::model::ChangeKind::Unstaged))
            .collect(),
        ..Default::default()
    }
}

/// A `ChangeSet` whose paths each carry a line count, so tests can tell the
/// difference between "this path was dropped" and "this path's volume was
/// dropped with it".
pub fn change_set_with_lines(paths: &[(&str, u64)]) -> collide::model::ChangeSet {
    let mut set = collide::model::ChangeSet {
        paths: paths
            .iter()
            .map(|(path, added)| collide::model::ChangedPath {
                lines_added: *added,
                ..collide::model::ChangedPath::new(*path, collide::model::ChangeKind::Unstaged)
            })
            .collect(),
        ..Default::default()
    };
    set.lines_added = set.paths.iter().map(|p| p.lines_added).sum();
    set
}

/// A `ChangeSet` describing `count` renames, both halves recorded exactly as
/// `git::change_set` records them: `old-<n>` as the rename origin and
/// `new-<n>` as the surviving path.
pub fn change_set_renamed(count: usize) -> collide::model::ChangeSet {
    let mut paths = Vec::new();
    for n in 0..count {
        paths.push(collide::model::ChangedPath {
            is_rename_origin: true,
            ..collide::model::ChangedPath::new(
                format!("old-{n}.rs"),
                collide::model::ChangeKind::Unstaged,
            )
        });
        paths.push(collide::model::ChangedPath::new(
            format!("new-{n}.rs"),
            collide::model::ChangeKind::Unstaged,
        ));
    }
    collide::model::ChangeSet {
        paths,
        has_rename: true,
        ..Default::default()
    }
}

/// A `ChangeSet` carrying a degraded reason code, for the paths where "we could
/// not read this" must not look like "there was nothing to read".
pub fn change_set_degraded(reason: &str) -> collide::model::ChangeSet {
    collide::model::ChangeSet {
        degraded: true,
        degraded_reason: Some(reason.to_string()),
        ..Default::default()
    }
}
