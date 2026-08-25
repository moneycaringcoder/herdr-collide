//! Observed git behaviour for repository layouts whose identity is easy to
//! infer incorrectly.

#[path = "fixtures.rs"]
mod fixtures;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use collide::config::Config;
use collide::git;
use collide::model::Checkout;

use fixtures::{checkout, Fixture};

const TIMEOUT: Duration = Duration::from_secs(60);

fn repo_key(path: &Path) -> collide::model::RepoKey {
    git::repo_key(path, TIMEOUT)
        .unwrap_or_else(|err| panic!("git repo key failed in {}: {err}", path.display()))
}

/// `git rev-parse --show-toplevel`, kept independent of the plugin's filesystem
/// walk so unusual layouts are measured against git rather than an assumption.
fn git_toplevel(path: &Path) -> PathBuf {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--path-format=absolute", "--show-toplevel"])
        .output()
        .expect("git rev-parse");
    assert!(
        out.status.success(),
        "git rev-parse --show-toplevel failed in {}: {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    let raw = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
    std::fs::canonicalize(&raw).unwrap_or(raw)
}

fn observed_checkout(id: &str, path: &Path, linked: bool) -> Checkout {
    let mut found = checkout(id, path, "ignored-herdr-key");
    found.is_linked_worktree = linked;
    found
}

#[test]
fn separate_git_dir_worktrees_resolve_to_the_same_common_store() {
    let fixture = Fixture::new("separate-key");
    let (main, store, linked) = fixture.separate_git_dir_repo("separate");

    let main_key = repo_key(&main);
    let linked_key = repo_key(&linked);
    assert_eq!(
        main_key,
        linked_key,
        "{} and {} are worktrees of one repository but resolved to different keys",
        main.display(),
        linked.display()
    );
    assert_eq!(
        PathBuf::from(&main_key.0),
        std::fs::canonicalize(&store).expect("canonical separate git store"),
        "repo key for {} did not resolve to its observed common store {}",
        main.display(),
        store.display()
    );
}

#[test]
fn superproject_worktrees_share_a_key_but_the_submodule_is_never_paired_with_them() {
    let fixture = Fixture::new("submodule-key");
    let (superproject, first, second, submodule) = fixture.superproject_with_submodule("embedded");

    let super_key = repo_key(&superproject);
    for worktree in [&first, &second] {
        assert_eq!(
            repo_key(worktree),
            super_key,
            "superproject worktree {} did not share {}'s repo key",
            worktree.display(),
            superproject.display()
        );
    }
    let submodule_key = repo_key(&submodule);
    assert_ne!(
        submodule_key,
        super_key,
        "submodule {} was grouped with superproject {}",
        submodule.display(),
        superproject.display()
    );

    // Matching relative paths across repository boundaries are overlap, not a
    // conflict, so make that tempting false pairing observable.
    fixture.write(&first, "payload.txt", "superproject payload\n");
    fixture.write(&submodule, "payload.txt", "changed submodule payload\n");
    let cycle = collide::collide::gather_for(
        vec![
            observed_checkout("super", &first, true),
            observed_checkout("submodule", &submodule, true),
        ],
        &Config::default(),
    )
    .expect("gather superproject and submodule");
    assert!(
        cycle.notes.is_empty(),
        "git could not make the pairing observation for {} and {}: {:?}",
        first.display(),
        submodule.display(),
        cycle.notes
    );
    let gathered_keys: BTreeSet<&str> = cycle
        .report
        .checkouts
        .iter()
        .map(|found| found.repo_key.0.as_str())
        .collect();
    assert_eq!(
        gathered_keys.len(),
        2,
        "{} and {} were grouped despite git reporting distinct repositories: {gathered_keys:?}",
        first.display(),
        submodule.display()
    );
    assert!(
        cycle.report.pairings.is_empty(),
        "superproject {} was paired with its independent submodule {}: {:?}",
        first.display(),
        submodule.display(),
        cycle.report.pairings
    );
}

#[test]
fn work_tree_roots_match_git_in_both_unusual_layouts() {
    let separate_fixture = Fixture::new("separate-toplevel");
    let (separate_main, _store, separate_linked) =
        separate_fixture.separate_git_dir_repo("separate");
    for path in [&separate_main, &separate_linked] {
        assert_eq!(
            collide::collide::work_tree_root(path),
            git_toplevel(path),
            "filesystem worktree root disagreed with git for separate-git-dir checkout {}",
            path.display()
        );
    }

    let submodule_fixture = Fixture::new("submodule-toplevel");
    let (superproject, first, second, submodule) =
        submodule_fixture.superproject_with_submodule("embedded");
    for path in [&superproject, &first, &second, &submodule] {
        assert_eq!(
            collide::collide::work_tree_root(path),
            git_toplevel(path),
            "filesystem worktree root disagreed with git for superproject/submodule checkout {}",
            path.display()
        );
    }
}

#[test]
fn separate_git_dir_checkouts_agree_on_the_main_root_when_it_is_present() {
    let fixture = Fixture::new("separate-root-main");
    let (main, _store, linked) = fixture.separate_git_dir_repo("separate");
    let key = repo_key(&main);

    // Under `--separate-git-dir` the main worktree's `.git` is a gitfile naming
    // the common store. Resolving the gitfile therefore yields the repository
    // key, which is the exact premise rule 1 uses to identify the main worktree.
    let git_dir = git::worktree_git_dir(&main).expect("resolve .git gitfile");
    assert_eq!(
        git_dir,
        PathBuf::from(&key.0),
        "the observed .git gitfile at {} did not resolve as the common store",
        main.display()
    );

    let cycle = collide::collide::gather_for(
        vec![
            observed_checkout("main", &main, false),
            observed_checkout("linked", &linked, true),
        ],
        &Config::default(),
    )
    .expect("gather separate-git-dir repository with main worktree");
    assert!(
        cycle.notes.is_empty() && cycle.report.checkouts.len() == 2,
        "git did not yield both separate-git-dir checkouts {} and {}: notes={:?}",
        main.display(),
        linked.display(),
        cycle.notes
    );
    let roots: BTreeSet<PathBuf> = cycle
        .report
        .checkouts
        .iter()
        .map(|found| found.repo_root.clone())
        .collect();
    assert_eq!(
        roots.len(),
        1,
        "checkouts {} and {} reported different repository roots: {roots:?}",
        main.display(),
        linked.display()
    );
    let observed = roots.into_iter().next().expect("one agreed root");
    assert_eq!(
        std::fs::canonicalize(&observed).expect("canonical agreed root"),
        std::fs::canonicalize(&main).expect("canonical main worktree"),
        "separate-git-dir checkouts agreed on {}, not observed main worktree {}",
        observed.display(),
        main.display()
    );
}

#[test]
fn dot_git_named_separate_store_uses_the_main_worktree_as_repo_root() {
    let fixture = Fixture::new("dot-git-store-root");
    let (main, store, linked) = fixture.separate_git_dir_dot_git_store_repo("dot-git-store");
    let key = repo_key(&main);
    assert_eq!(
        PathBuf::from(&key.0),
        std::fs::canonicalize(&store).expect("canonical .git-named separate store"),
        "fixture repository key did not name separate store {}",
        store.display()
    );

    let cycle = collide::collide::gather_for(
        vec![
            observed_checkout("main", &main, false),
            observed_checkout("linked", &linked, true),
        ],
        &Config::default(),
    )
    .expect("gather .git-named separate-git-dir repository");
    assert!(
        cycle.notes.is_empty() && cycle.report.checkouts.len() == 2,
        "git did not yield both separate-git-dir checkouts {} and {}: notes={:?}",
        main.display(),
        linked.display(),
        cycle.notes
    );
    let roots: BTreeSet<PathBuf> = cycle
        .report
        .checkouts
        .iter()
        .map(|found| found.repo_root.clone())
        .collect();
    assert_eq!(
        roots.len(),
        1,
        "checkouts {} and {} reported different repository roots: {roots:?}",
        main.display(),
        linked.display()
    );
    let observed = roots.into_iter().next().expect("one agreed root");
    assert_eq!(
        std::fs::canonicalize(&observed).expect("canonical agreed root"),
        std::fs::canonicalize(&main).expect("canonical main worktree"),
        ".git-named separate store made checkouts agree on {}, not main worktree {}",
        observed.display(),
        main.display()
    );
}

#[test]
fn separate_git_dir_linked_checkouts_agree_on_a_root_when_the_main_is_absent() {
    let fixture = Fixture::new("separate-root-no-main");
    let (_main, store, linked) = fixture.separate_git_dir_repo("separate");
    let key = repo_key(&linked);
    assert_ne!(
        Path::new(&key.0).file_name(),
        Some(std::ffi::OsStr::new(".git")),
        "fixture store {} unexpectedly exercised the .git-name guess instead of member fallback",
        store.display()
    );

    let other = fixture.root().join("separate-secondary-linked");
    fixture.git(
        &linked,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "separate-secondary-linked",
            other.to_str().unwrap(),
            "main",
        ],
    );
    let cycle = collide::collide::gather_for(
        vec![
            observed_checkout("linked", &linked, true),
            observed_checkout("other", &other, true),
        ],
        &Config::default(),
    )
    .expect("gather separate-git-dir repository without main worktree");
    assert!(
        cycle.notes.is_empty() && cycle.report.checkouts.len() == 2,
        "git did not yield both linked checkouts {} and {}: notes={:?}",
        linked.display(),
        other.display(),
        cycle.notes
    );
    let roots: BTreeSet<PathBuf> = cycle
        .report
        .checkouts
        .iter()
        .map(|found| found.repo_root.clone())
        .collect();
    assert_eq!(
        roots.len(),
        1,
        "linked checkouts {} and {} reported different roots without the main: {roots:?}",
        linked.display(),
        other.display()
    );
    let observed = roots.into_iter().next().expect("one agreed root");
    assert_eq!(
        std::fs::canonicalize(&observed).expect("canonical agreed root"),
        std::fs::canonicalize(&linked).expect("canonical linked worktree"),
        "without main worktree, member fallback chose {} instead of shorter checkout {}",
        observed.display(),
        linked.display()
    );
}

#[test]
fn superproject_checkouts_agree_on_one_root_while_the_submodule_keeps_its_own() {
    let fixture = Fixture::new("submodule-root");
    let (superproject, first, second, submodule) = fixture.superproject_with_submodule("embedded");
    let super_key = repo_key(&superproject);
    let submodule_key = repo_key(&submodule);

    let cycle = collide::collide::gather_for(
        vec![
            observed_checkout("main", &superproject, false),
            observed_checkout("first", &first, true),
            observed_checkout("second", &second, true),
            observed_checkout("submodule", &submodule, true),
        ],
        &Config::default(),
    )
    .expect("gather superproject and submodule roots");
    assert!(
        cycle.notes.is_empty() && cycle.report.checkouts.len() == 4,
        "git did not yield superproject {}, worktrees {}, {}, and submodule {}: notes={:?}",
        superproject.display(),
        first.display(),
        second.display(),
        submodule.display(),
        cycle.notes
    );
    let super_roots: BTreeSet<PathBuf> = cycle
        .report
        .checkouts
        .iter()
        .filter(|found| found.repo_key == super_key)
        .map(|found| found.repo_root.clone())
        .collect();
    assert_eq!(
        super_roots.len(),
        1,
        "superproject checkouts {}, {}, and {} disagreed on repo root: {super_roots:?}",
        superproject.display(),
        first.display(),
        second.display()
    );
    let super_root = super_roots
        .into_iter()
        .next()
        .expect("one superproject root");
    assert_eq!(
        std::fs::canonicalize(&super_root).expect("canonical superproject root"),
        std::fs::canonicalize(&superproject).expect("canonical superproject"),
        "superproject worktrees agreed on {}, not {}",
        super_root.display(),
        superproject.display()
    );

    let submodule_checkout = cycle
        .report
        .checkouts
        .iter()
        .find(|found| found.repo_key == submodule_key)
        .expect("submodule checkout retained as its own repository");
    assert_eq!(
        std::fs::canonicalize(&submodule_checkout.repo_root).expect("canonical submodule root"),
        std::fs::canonicalize(&submodule).expect("canonical submodule checkout"),
        "submodule {} reported superproject root {}",
        submodule.display(),
        submodule_checkout.repo_root.display()
    );
}
