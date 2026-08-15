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
        let (code, _out, _err) = self.try_git(
            &path,
            &["merge", "--no-edit", &format!("{name}-source")],
        );
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

    /// Files that `.gitignore` covers, which must never enter a change set.
    pub fn ignored_files(&self, cwd: &Path) {
        self.write(cwd, "ignored/artifact.bin", "junk\n");
        self.write(cwd, "build.log", "noise\n");
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
            .map(|p| collide::model::ChangedPath {
                path: p.to_string(),
                kind: collide::model::ChangeKind::Unstaged,
            })
            .collect(),
        ..Default::default()
    }
}
