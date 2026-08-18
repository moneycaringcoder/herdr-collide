//! Analysis: group checkouts by repo, pair them, and turn shared paths into
//! severities. Pure over its inputs so it can be tested without herdr or git.
//!
//! The split is deliberate:
//!
//! * [`analyse`] and [`apply_predictions`] are pure functions over the data
//!   they are handed. No git, no socket, no clock, no filesystem — which is why
//!   [`analyse`] is given a resolved [`WorkTrees`] rather than resolving one:
//!   every other external call in this crate is bounded by `config.git_timeout`,
//!   and a `canonicalize` on a hung mount inside the pure pass would stop the
//!   badge daemon with no error and no note.
//! * [`run_once`] and [`run_json`] do the impure gathering — talk to herdr,
//!   shell out to git, verify repo identity, resolve working trees — and then
//!   call the pure pass.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::Config;
use crate::git;
use crate::model::{
    ChangeSet, Checkout, FileVerdict, Pairing, Report, Severity, SharedFile, WorkTrees,
    WorkspaceStatus,
};
use crate::Result;

/// JSON schema version emitted by `--json`. Bump on any incompatible change.
/// Bumped to 2 when `severity` gained the `unknown` value: a consumer matching
/// exhaustively on the old four would break on it, which is exactly what a
/// schema version is for.
pub const JSON_SCHEMA_VERSION: u32 = 2;

// ---------------------------------------------------------------------------
// Pure analysis
// ---------------------------------------------------------------------------

/// Groups checkouts by repo key, pairs every distinct checkout within a repo,
/// and derives a per-workspace severity. Checkouts from different repos are
/// never compared.
///
/// When `config.predict_conflicts` is set, shared files come back as
/// [`FileVerdict::Unknown`]: the caller is expected to run
/// [`git::Predictor::predict_pair`] and feed the answers back through
/// [`apply_predictions`]. With prediction off, a shared file is reported as a
/// plain [`FileVerdict::Overlap`] unless the snapshot cannot represent changed
/// submodule contents, which stays [`FileVerdict::Unknown`].
///
/// `trees` says where each checkout's working tree starts, so that two
/// workspaces sharing one tree are not compared with themselves. It is resolved
/// by [`gather_for`]; a workspace missing from it is simply compared, because an
/// unresolved top level is not evidence of anything.
pub fn analyse(
    checkouts: &[Checkout],
    changes: &[(String, ChangeSet)],
    trees: &WorkTrees,
    config: &Config,
) -> Report {
    let filtered: BTreeMap<&str, FilteredChange> = changes
        .iter()
        .map(|(id, set)| (id.as_str(), FilteredChange::new(set, config)))
        .collect();

    // Group by repo key. `run_once` has already rewritten every repo key to the
    // canonicalized `--git-common-dir`, so equal keys really are one repo.
    let mut groups: BTreeMap<&str, Vec<&Checkout>> = BTreeMap::new();
    for checkout in checkouts {
        groups
            .entry(checkout.repo_key.0.as_str())
            .or_default()
            .push(checkout);
    }

    let unresolved = if config.predict_conflicts {
        FileVerdict::Unknown
    } else {
        FileVerdict::Overlap
    };

    let mut pairings = Vec::new();
    for members in groups.values() {
        for (i, left) in members.iter().enumerate() {
            for right in members.iter().skip(i + 1) {
                if left.workspace_id == right.workspace_id {
                    continue;
                }
                // Two herdr workspaces can point at one working tree — the same
                // directory twice, or one opened on a subdirectory of the other.
                // git then reports one change set twice, every changed file
                // looks "shared", and the pair badges a collision that does not
                // exist. Same tree, no comparison.
                //
                // Compared by resolved top level, never by path prefix: a linked
                // worktree at `<root>/.worktrees/api` sits *under* the main
                // worktree's path and is a different tree entirely.
                if trees.same_tree(&left.workspace_id, &right.workspace_id) {
                    continue;
                }
                let (Some(lc), Some(rc)) = (
                    filtered.get(left.workspace_id.as_str()),
                    filtered.get(right.workspace_id.as_str()),
                ) else {
                    continue;
                };
                // A checkout with no commit cannot be merged against anything.
                if !lc.pairable || !rc.pairable {
                    continue;
                }
                let shared: Vec<SharedFile> = lc
                    .paths
                    .intersection(&rc.paths)
                    .map(|path| SharedFile {
                        path: path.clone(),
                        verdict: if lc.uncomparable_submodules.contains(path)
                            || rc.uncomparable_submodules.contains(path)
                        {
                            FileVerdict::Unknown
                        } else {
                            unresolved
                        },
                    })
                    .collect();
                // An empty intersection normally means the pair cannot
                // collide, and dropping it is free. A rename breaks that: the
                // same content can appear under a different name on each side,
                // so the merge can conflict on a path neither change set
                // lists. `git::Predictor::predict_pair` knows how to handle
                // that, but only if it is given the pair at all.
                let rename_probe = config.predict_conflicts && (lc.has_rename || rc.has_rename);
                if shared.is_empty() && !rename_probe {
                    continue;
                }
                pairings.push(Pairing {
                    left_workspace_id: left.workspace_id.clone(),
                    right_workspace_id: right.workspace_id.clone(),
                    shared,
                    approximate: false,
                });
            }
        }
    }

    let statuses = statuses(checkouts, &filtered, &pairings, config);
    Report {
        checkouts: checkouts.to_vec(),
        pairings,
        statuses,
        changes: changes.to_vec(),
    }
}

/// One prediction result, keyed by the pair it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairVerdicts {
    pub left_workspace_id: String,
    pub right_workspace_id: String,
    /// `(path, conflicts)`. Paths not already shared are added to the pairing:
    /// a rename can conflict on a path that appears under a different name in
    /// each change set.
    pub verdicts: Vec<(String, bool)>,
    /// Prediction could not run for this pair; the shared files stay `Unknown`.
    pub failed: bool,
    /// A single merge base had to be forced although the histories offer more
    /// than one, or there was no common ancestor at all, so the verdicts
    /// approximate what a real merge would do.
    pub approximate: bool,
}

/// Folds conflict predictions into a report and recomputes severities. Pure.
pub fn apply_predictions(
    report: &mut Report,
    predictions: &[PairVerdicts],
    changes: &[(String, ChangeSet)],
    config: &Config,
) {
    let by_pair: BTreeMap<(&str, &str), &PairVerdicts> = predictions
        .iter()
        .map(|p| {
            (
                (p.left_workspace_id.as_str(), p.right_workspace_id.as_str()),
                p,
            )
        })
        .collect();

    let pair_changes: BTreeMap<&str, &ChangeSet> =
        changes.iter().map(|(id, set)| (id.as_str(), set)).collect();
    let filtered: BTreeMap<&str, FilteredChange> = changes
        .iter()
        .map(|(id, set)| (id.as_str(), FilteredChange::new(set, config)))
        .collect();

    for pairing in &mut report.pairings {
        let key = (
            pairing.left_workspace_id.as_str(),
            pairing.right_workspace_id.as_str(),
        );
        let Some(prediction) = by_pair.get(&key) else {
            continue;
        };
        if prediction.failed {
            continue;
        }
        pairing.approximate = prediction.approximate;
        let verdicts: BTreeMap<&str, bool> = prediction
            .verdicts
            .iter()
            .map(|(path, hit)| (path.as_str(), *hit))
            .collect();
        for shared in &mut pairing.shared {
            let uncomparable_submodule = [key.0, key.1].iter().any(|id| {
                filtered
                    .get(id)
                    .is_some_and(|change| change.uncomparable_submodules.contains(&shared.path))
            });
            shared.verdict = match verdicts.get(shared.path.as_str()) {
                Some(true) => FileVerdict::Conflict,
                _ if uncomparable_submodule => FileVerdict::Unknown,
                // A path git did not flag merges cleanly, even though both
                // sides touched it. That discrimination is the whole point of
                // this plugin: a shared file is not a collision.
                Some(false) | None => FileVerdict::Overlap,
            };
        }
        let known: BTreeSet<&str> = pairing.shared.iter().map(|s| s.path.as_str()).collect();
        // git can name a conflicting path that is in neither change set. Two
        // causes, and they need telling apart:
        //
        // * a rename — the file exists under a different name on each side, so
        //   neither change set lists the merged path. This is real, and it is
        //   why the pair was predicted at all.
        // * a content filter the snapshot deliberately did not run. A filtered
        //   path that is stat-dirty but content-identical re-hashes to its raw
        //   bytes and so differs from a base holding the filtered blob, even
        //   though `status` — which is what the change set is built from —
        //   correctly reports the worktree clean.
        //
        // The second would report a conflict on a file neither agent touched,
        // which is a false alarm of exactly the kind this plugin exists to
        // avoid raising. So an unlisted path is only believed when a rename
        // could explain it, or when a change set lists it after all.
        //
        // `renamed` is per *pair*, not per path — nothing in the prediction says
        // which conflict a rename explains — so admitting a path on that
        // strength alone is a guess. It is a guess worth making, because the
        // alternative is losing the rename conflicts this pair was predicted
        // for, but the pairing is marked `approximate` so the pane says the
        // verdict is not firm rather than presenting it as flat fact.
        let renamed = pair_changes
            .get(key.0)
            .is_some_and(|c: &&ChangeSet| c.has_rename)
            || pair_changes
                .get(key.1)
                .is_some_and(|c: &&ChangeSet| c.has_rename);
        let listed = |path: &str| {
            [key.0, key.1].iter().any(|id| {
                pair_changes
                    .get(id)
                    .is_some_and(|c| c.paths.iter().any(|p| p.path == path))
            })
        };
        let mut guessed = false;
        let mut extra: Vec<String> = Vec::new();
        for (path, hit) in &prediction.verdicts {
            if !*hit || known.contains(path.as_str()) {
                continue;
            }
            // An ignored path is ignored here too. `known` was built from the
            // filtered intersection, so without this a `Cargo.lock` that both
            // sides regenerated comes straight back as a conflict through the
            // unlisted-path door — the single commonest false alarm there is,
            // and the one `ignore_suffixes` exists to suppress.
            if is_ignored(path, config) {
                continue;
            }
            if listed(path) {
                extra.push(path.clone());
            } else if renamed {
                guessed = true;
                extra.push(path.clone());
            }
        }
        if guessed {
            pairing.approximate = true;
        }
        for path in extra {
            pairing.shared.push(SharedFile {
                path,
                verdict: FileVerdict::Conflict,
            });
        }
        pairing.shared.sort_by(|a, b| a.path.cmp(&b.path));
    }

    // A pair kept only because one side renamed something has nothing to show
    // unless the prediction actually found a conflicting path. Dropping the
    // empty ones here keeps the probe invisible when it comes back clean.
    report.pairings.retain(|pairing| !pairing.shared.is_empty());

    report.statuses = statuses(&report.checkouts, &filtered, &report.pairings, config);
}

/// A change set with `ignore_suffixes` applied, reduced to what the pairing
/// pass needs. Lockfiles and generated manifests overlap on essentially every
/// concurrent branch and carry no information, so they are dropped before
/// anything counts them — including the runaway thresholds.
#[derive(Debug, Clone)]
struct FilteredChange {
    paths: BTreeSet<String>,
    uncomparable_submodules: BTreeSet<String>,
    lines_changed: u64,
    /// Distinct changed files, with the origin half of a rename counted once.
    changed_files: usize,
    has_rename: bool,
    pairable: bool,
    /// The git pass failed for this checkout, so an empty change set means
    /// "not read" rather than "nothing changed".
    unreadable: bool,
}

impl FilteredChange {
    fn new(set: &ChangeSet, config: &Config) -> Self {
        let kept: Vec<&crate::model::ChangedPath> = set
            .paths
            .iter()
            .filter(|p| !is_ignored(&p.path, config))
            .collect();
        let paths: BTreeSet<String> = kept.iter().map(|p| p.path.clone()).collect();
        let uncomparable_submodules = kept
            .iter()
            .filter(|p| p.submodule_contents_uncomparable)
            .map(|p| p.path.clone())
            .collect();
        // Volume is filtered along with the paths. A `package-lock.json` the
        // plugin has decided to ignore must not still trip the runaway
        // threshold, and the origin half of a rename is one file, not two.
        let lines_changed = kept
            .iter()
            .fold(0u64, |total, p| total.saturating_add(p.lines_changed()));
        let changed_files = kept.iter().filter(|p| !p.is_rename_origin).count();
        Self {
            paths,
            uncomparable_submodules,
            lines_changed,
            changed_files,
            has_rename: set.has_rename,
            pairable: pairable(set),
            unreadable: has_reason_code(set, git::DEGRADED_UNREADABLE),
        }
    }
}

/// The machine-readable codes in a `degraded_reason`.
///
/// `git::change_set` writes reasons as `code: human text`, joined with `"; "`,
/// and the human half interpolates branch and ref names the user chose. A
/// `contains` test against the whole string therefore fires on a *branch* called
/// `unborn-branch`, which silently excluded its checkout from every comparison.
/// Splitting the way `render::explain_reason` already does keeps the two halves
/// of the codebase agreeing on what a code is.
fn reason_codes(reason: &str) -> impl Iterator<Item = &str> {
    reason
        .split("; ")
        .map(|part| part.trim())
        .map(|part| part.split_once(": ").map(|(code, _)| code).unwrap_or(part))
}

/// Whether a change set was degraded for a specific reason.
fn has_reason_code(set: &ChangeSet, code: &str) -> bool {
    set.degraded_reason
        .as_deref()
        .is_some_and(|reason| reason_codes(reason).any(|found| found == code))
}

/// Makes every checkout of one repository report the same `repo_root`.
///
/// Repo *identity* is re-derived from git, but `repo_root` is taken from herdr,
/// which falls back to the checkout path when it has nothing better. Two
/// worktrees of one repository could therefore disagree, and the detail view —
/// which prints one header per repository — showed whichever of them happened to
/// sort first, so the header named a worktree rather than the repository, and
/// changed when a workspace was renamed or closed.
///
/// Three rules, in order, and only the first is exact:
///
/// 1. **The main worktree, when it is open.** Its top level holds the
///    `--git-common-dir` itself, so `<top level>/.git == repo_key` identifies it
///    with no guessing, and its top level *is* the repository root. This covers
///    every layout, including `--separate-git-dir` and a repository whose root is
///    not named after its git directory.
/// 2. **The parent of the key, when the key is named `.git`.** Right for the
///    ordinary layout, and a guess: a `--separate-git-dir` store that happens to
///    be named `.git` passes this test and yields the store rather than the
///    working tree. Nothing available here can tell those apart, which is why
///    rule 1 comes first and why this rule only runs when the main worktree is
///    not among the open workspaces.
/// 3. **A deterministic pick among the members' own top levels.** Any rule will
///    do provided every member lands on the same answer; a checkout that is not
///    a linked worktree first, then the shortest path.
fn agree_on_repo_root(checkouts: &mut [Checkout], trees: &WorkTrees) {
    let mut roots: BTreeMap<String, std::path::PathBuf> = BTreeMap::new();
    for checkout in checkouts.iter() {
        let key = checkout.repo_key.0.as_str();
        if roots.contains_key(key) {
            continue;
        }
        let members: Vec<&Checkout> = checkouts.iter().filter(|c| c.repo_key.0 == key).collect();

        // 1. The member whose own top level owns the common dir.
        let from_main = members.iter().find_map(|c| {
            let top = trees.get(&c.workspace_id)?;
            let dot_git = top.join(".git");
            let canonical = std::fs::canonicalize(&dot_git).unwrap_or(dot_git);
            (canonical.to_string_lossy() == key).then(|| top.to_path_buf())
        });

        // 2. The parent of a key named `.git`.
        let from_key = || {
            std::path::Path::new(key)
                .file_name()
                .filter(|name| *name == ".git")
                .and_then(|_| std::path::Path::new(key).parent())
                .map(std::path::Path::to_path_buf)
        };

        // 3. A deterministic member.
        let from_members = || {
            let mut candidates = members.clone();
            candidates.sort_by_key(|c| {
                let top = trees
                    .get(&c.workspace_id)
                    .map(std::path::Path::to_path_buf)
                    .unwrap_or_else(|| c.repo_root.clone());
                (c.is_linked_worktree, top.as_os_str().len(), top)
            });
            candidates
                .first()
                .map(|c| {
                    trees
                        .get(&c.workspace_id)
                        .map(std::path::Path::to_path_buf)
                        .unwrap_or_else(|| c.repo_root.clone())
                })
                .unwrap_or_default()
        };

        let root = from_main.or_else(from_key).unwrap_or_else(from_members);
        roots.insert(key.to_string(), root);
    }
    for checkout in checkouts.iter_mut() {
        if let Some(root) = roots.get(checkout.repo_key.0.as_str()) {
            checkout.repo_root = root.clone();
        }
    }
}

/// Where a checkout's working tree starts: the nearest ancestor of `path`,
/// itself included, that holds a `.git` entry.
///
/// This is git's own top-level discovery for every layout this plugin meets. A
/// linked worktree carries a `.git` *file*, so `<root>/.worktrees/api` resolves
/// to itself; an ordinary subdirectory carries nothing, so `<root>/src` walks up
/// to `<root>`. That difference is the whole point — it is what a path-prefix
/// test cannot see, and getting it wrong stopped every worktree in a
/// `.worktrees/` layout being compared with the repository it lives in.
///
/// It is a filesystem walk rather than `git rev-parse --show-toplevel` because
/// `src/git.rs` exposes no helper for it and belongs to somebody else; the walk
/// is pinned against git's own answer by
/// `resolved_work_trees_match_git_rev_parse` in `tests/conflict_detection.rs`,
/// so a divergence fails the suite rather than going quiet. A checkout with no
/// `.git` anywhere above it resolves to itself, which pairs it with everything —
/// the visible failure direction.
pub fn work_tree_root(path: &std::path::Path) -> std::path::PathBuf {
    let start = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut candidate: &std::path::Path = &start;
    loop {
        if candidate.join(".git").exists() {
            return candidate.to_path_buf();
        }
        match candidate.parent() {
            Some(parent) => candidate = parent,
            None => return start,
        }
    }
}

/// Suffix match against `Config::ignore_suffixes`, anchored to a path-component
/// or extension boundary.
///
/// A bare `ends_with` is too eager: `go.sum` would swallow `tools/cargo.sum`
/// and `Cargo.lock` would swallow `vendor/NotReallyCargo.lock`, dropping real
/// changes from the change set with nothing to show for it. A suffix that
/// starts with `.` is an extension and may match mid-name; anything else must
/// begin at the start of the path or straight after a `/`.
pub fn is_ignored(path: &str, config: &Config) -> bool {
    config.ignore_suffixes.iter().any(|suffix| {
        if suffix.is_empty() || !path.ends_with(suffix.as_str()) {
            return false;
        }
        if suffix.starts_with('.') {
            return true;
        }
        let start = path.len() - suffix.len();
        start == 0 || path.as_bytes()[start - 1] == b'/'
    })
}

/// An unborn branch and a branch deleted underneath a worktree both leave the
/// checkout with no commit, so there is nothing to merge against and the
/// checkout is excluded from pairing rather than guessed at.
pub fn pairable(set: &ChangeSet) -> bool {
    !git::UNPAIRABLE_REASONS
        .iter()
        .any(|code| has_reason_code(set, code))
}

fn statuses(
    checkouts: &[Checkout],
    filtered: &BTreeMap<&str, FilteredChange>,
    pairings: &[Pairing],
    config: &Config,
) -> Vec<WorkspaceStatus> {
    checkouts
        .iter()
        .map(|checkout| {
            let id = checkout.workspace_id.as_str();
            let mut overlaps: BTreeSet<&str> = BTreeSet::new();
            let mut conflicts: BTreeSet<&str> = BTreeSet::new();
            let mut unknowns: BTreeSet<&str> = BTreeSet::new();
            for pairing in pairings {
                if pairing.left_workspace_id != id && pairing.right_workspace_id != id {
                    continue;
                }
                for shared in &pairing.shared {
                    match shared.verdict {
                        FileVerdict::Conflict => {
                            conflicts.insert(shared.path.as_str());
                        }
                        FileVerdict::Overlap => {
                            overlaps.insert(shared.path.as_str());
                        }
                        // Not a weaker overlap. An overlap badge claims the
                        // file merges clean, and a prediction that could not
                        // run has not earned that claim.
                        FileVerdict::Unknown => {
                            unknowns.insert(shared.path.as_str());
                        }
                    }
                }
            }
            // A path that conflicts in one pairing is a conflict, full stop; it
            // should not also inflate the overlap or unknown counts.
            for path in &conflicts {
                overlaps.remove(path);
                unknowns.remove(path);
            }
            for path in &unknowns {
                overlaps.remove(path);
            }

            // A checkout the git pass could not read at all has no entry here.
            // Reporting it clean would be a claim about a repository we failed
            // to look at, so it is unknown instead.
            let change = filtered.get(id);
            let unreadable = change.map(|c| c.unreadable).unwrap_or(true);
            let runaway = change
                .map(|change| {
                    change.changed_files > config.runaway_files
                        || change.lines_changed > config.runaway_lines
                })
                .unwrap_or(false);

            let severity = if !conflicts.is_empty() {
                Severity::Conflict
            } else if !unknowns.is_empty() || unreadable {
                Severity::Unknown
            } else if runaway {
                Severity::Runaway
            } else if !overlaps.is_empty() {
                Severity::Overlap
            } else {
                Severity::Clean
            };

            WorkspaceStatus {
                workspace_id: checkout.workspace_id.clone(),
                severity,
                overlap_count: overlaps.len(),
                conflict_count: conflicts.len(),
                unknown_count: unknowns.len(),
                runaway,
                lines_changed: change.map(|c| c.lines_changed).unwrap_or(0),
                changed_files: change.map(|c| c.changed_files).unwrap_or(0),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Impure gathering
// ---------------------------------------------------------------------------

/// Everything one refresh cycle produces, including the per-checkout change
/// sets that the report itself does not carry.
pub struct Cycle {
    pub report: Report,
    pub changes: Vec<(String, ChangeSet)>,
    /// Non-fatal problems worth showing the user: a checkout that vanished, a
    /// pair whose prediction failed.
    pub notes: Vec<String>,
}

/// Talks to herdr and git, then runs the pure pass.
pub fn gather(config: &Config) -> Result<Cycle> {
    let mut herdr = crate::herdr::Herdr::connect()?;
    let checkouts = herdr.checkouts()?;
    let skipped = herdr.skipped_worktrees();
    let mut cycle = gather_for(checkouts, config)?;
    // A workspace herdr calls a repository but whose worktree object this client
    // could not read is dropped, which makes the session look smaller than it
    // is. The daemon reports that; so must the one-shot commands, which are what
    // somebody runs when they are actually looking.
    if skipped > 0 {
        cycle.notes.push(format!(
            "{skipped} workspace(s) carried a worktree object this client could not read; \
             they are missing from this report"
        ));
    }
    Ok(cycle)
}

/// The ref one checkout's change set is measured against.
///
/// A `base_ref` the user actually set wins outright, whether it resolves or
/// not: silently probing for something else would make `--base-ref` a lie, and
/// `git::change_set` already degrades gracefully when the ref is missing. Only
/// the untouched default hands over to the probing chain, which is there
/// precisely because `origin/HEAD` does not exist in every repo.
pub fn base_ref_for(checkout: &std::path::Path, config: &Config) -> String {
    if config.base_ref != crate::config::DEFAULT_BASE_REF {
        return config.base_ref.clone();
    }
    // No honest answer is available when the probing chain finds nothing, and
    // substituting `HEAD` is the one option indistinguishable from success: the
    // committed half of every change set silently becomes empty, so two agents
    // about to collide head-on read as two clean workspaces. Hand `change_set`
    // the sentinel instead and let the checkout degrade visibly.
    match git::integration_ref(checkout, config.git_timeout) {
        Ok(Some(found)) => found,
        Ok(None) | Err(_) => git::NO_INTEGRATION_REF.to_string(),
    }
}

/// The gathering pass, given a checkout list. Split out from [`gather`] so it
/// can be driven from a fixture without a herdr socket.
pub fn gather_for(checkouts: Vec<Checkout>, config: &Config) -> Result<Cycle> {
    let mut notes = Vec::new();

    // Repo identity is re-derived from git rather than trusted from herdr: two
    // checkouts are only ever compared when their canonicalized
    // `--git-common-dir` matches, and a symlinked or relocated worktree can
    // easily report a repo_key that no longer agrees with it.
    let mut verified: Vec<Checkout> = Vec::new();
    for mut checkout in checkouts {
        match git::repo_key(&checkout.checkout_path, config.git_timeout) {
            Ok(key) => {
                checkout.repo_key = key;
                // Ask git for the branch rather than herdr. We already have the
                // checkout path, and `worktree.list` is per-repo and errors on
                // workspaces that are not repos at all. A genuinely detached
                // HEAD comes back as `None` so the view stays truthful; on a
                // lookup failure whatever herdr supplied is left alone.
                if let Ok(branch) = git::current_branch(&checkout.checkout_path, config.git_timeout)
                {
                    checkout.branch = branch;
                }
                verified.push(checkout);
            }
            Err(err) => notes.push(format!(
                "skipping {}: {err}",
                checkout.checkout_path.display()
            )),
        }
    }

    // Resolved here, with the other calls that touch the outside world, so the
    // pure pass stays pure and one unresponsive mount cannot stall it silently.
    let mut trees = WorkTrees::new();
    for checkout in &verified {
        trees.insert(
            checkout.workspace_id.clone(),
            work_tree_root(&checkout.checkout_path),
        );
    }

    agree_on_repo_root(&mut verified, &trees);

    let mut changes: Vec<(String, ChangeSet)> = Vec::new();
    for checkout in &verified {
        let base = base_ref_for(&checkout.checkout_path, config);
        match git::change_set(&checkout.checkout_path, &base, config.git_timeout) {
            Ok(set) => changes.push((checkout.workspace_id.clone(), set)),
            Err(err) => {
                notes.push(format!("{}: {err}", checkout.checkout_path.display()));
                // Not `ChangeSet::default()`. An empty, healthy-looking change
                // set is indistinguishable from a clean worktree, so a
                // checkout we failed to read would badge as clean — the
                // quietest possible wrong answer. Say so instead.
                changes.push((
                    checkout.workspace_id.clone(),
                    ChangeSet {
                        degraded: true,
                        degraded_reason: Some(format!(
                            "{}: could not read this checkout: {err}",
                            git::DEGRADED_UNREADABLE
                        )),
                        ..ChangeSet::default()
                    },
                ));
            }
        }
    }

    let mut report = analyse(&verified, &changes, &trees, config);

    if config.predict_conflicts && !report.pairings.is_empty() {
        let by_id: BTreeMap<&str, &Checkout> = verified
            .iter()
            .map(|c| (c.workspace_id.as_str(), c))
            .collect();

        let mut predictor = git::Predictor::new(config.git_timeout)?;
        let mut primed: BTreeSet<&str> = BTreeSet::new();
        for pairing in &report.pairings {
            for id in [
                pairing.left_workspace_id.as_str(),
                pairing.right_workspace_id.as_str(),
            ] {
                if primed.contains(id) {
                    continue;
                }
                let Some(checkout) = by_id.get(id) else {
                    continue;
                };
                // Snapshot each worktree once and reuse its tree OID for every
                // pair it takes part in.
                match predictor.prime(&checkout.checkout_path) {
                    Ok(()) => {
                        primed.insert(id);
                    }
                    Err(err) => notes.push(format!("{}: {err}", checkout.checkout_path.display())),
                }
            }
        }

        let jobs: Vec<(&Pairing, &Checkout, &Checkout)> = report
            .pairings
            .iter()
            .filter_map(|pairing| {
                let left = by_id.get(pairing.left_workspace_id.as_str())?;
                let right = by_id.get(pairing.right_workspace_id.as_str())?;
                if !primed.contains(pairing.left_workspace_id.as_str())
                    || !primed.contains(pairing.right_workspace_id.as_str())
                {
                    return None;
                }
                Some((pairing, *left, *right))
            })
            .collect();

        let predictions = predict_all(&predictor, &jobs, &mut notes);
        apply_predictions(&mut report, &predictions, &changes, config);
    }

    Ok(Cycle {
        report,
        changes,
        notes,
    })
}

/// Runs phase 1 (and, where needed, phase 2) for every pair, fanning out across
/// std threads. Every checkout is already primed, so the predictor is immutable
/// here and git itself is lock-free for everything except `status`.
fn predict_all(
    predictor: &git::Predictor,
    jobs: &[(&Pairing, &Checkout, &Checkout)],
    notes: &mut Vec<String>,
) -> Vec<PairVerdicts> {
    if jobs.is_empty() {
        return Vec::new();
    }
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 8)
        .min(jobs.len());

    let mut results: Vec<(PairVerdicts, Option<String>)> = Vec::with_capacity(jobs.len());
    let mut panicked = 0usize;
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for chunk in jobs.chunks(jobs.len().div_ceil(workers)) {
            handles.push(scope.spawn(move || {
                chunk
                    .iter()
                    .map(|(pairing, left, right)| {
                        let paths: Vec<String> =
                            pairing.shared.iter().map(|s| s.path.clone()).collect();
                        match predictor.predict_pair(
                            &left.checkout_path,
                            &right.checkout_path,
                            &paths,
                        ) {
                            Ok(prediction) => (
                                PairVerdicts {
                                    left_workspace_id: pairing.left_workspace_id.clone(),
                                    right_workspace_id: pairing.right_workspace_id.clone(),
                                    verdicts: prediction.verdicts,
                                    failed: false,
                                    approximate: prediction.approximate,
                                },
                                None,
                            ),
                            Err(err) => (
                                PairVerdicts {
                                    left_workspace_id: pairing.left_workspace_id.clone(),
                                    right_workspace_id: pairing.right_workspace_id.clone(),
                                    verdicts: Vec::new(),
                                    failed: true,
                                    approximate: false,
                                },
                                Some(format!(
                                    "{} vs {}: {err}",
                                    left.checkout_path.display(),
                                    right.checkout_path.display()
                                )),
                            ),
                        }
                    })
                    .collect::<Vec<_>>()
            }));
        }
        for handle in handles {
            match handle.join() {
                Ok(chunk) => results.extend(chunk),
                // A panicked worker takes its whole chunk's verdicts with it.
                // Those pairs stay `Unknown`, which is the honest outcome, but
                // it must not happen silently.
                Err(_) => panicked += 1,
            }
        }
    });
    if panicked > 0 {
        notes.push(format!(
            "{panicked} conflict-prediction worker(s) panicked; the pairs they held are unknown"
        ));
    }

    let mut predictions = Vec::with_capacity(results.len());
    for (prediction, note) in results {
        if let Some(note) = note {
            notes.push(note);
        }
        predictions.push(prediction);
    }
    predictions
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

pub fn run_once(config: &Config) -> Result<()> {
    let cycle = gather(config)?;
    print!("{}", text_report(&cycle));
    Ok(())
}

pub fn run_json(config: &Config) -> Result<()> {
    let cycle = gather(config)?;
    println!("{}", serde_json::to_string_pretty(&json_report(&cycle))?);
    Ok(())
}

/// Plain-text one-shot report. Deliberately independent of `render`, which owns
/// the live pane and the badge string.
pub fn text_report(cycle: &Cycle) -> String {
    let mut out = String::new();
    let changes: BTreeMap<&str, &ChangeSet> = cycle
        .changes
        .iter()
        .map(|(id, set)| (id.as_str(), set))
        .collect();

    if cycle.report.checkouts.is_empty() {
        out.push_str("no git-backed workspaces\n");
        return out;
    }

    for status in &cycle.report.statuses {
        let checkout = cycle
            .report
            .checkouts
            .iter()
            .find(|c| c.workspace_id == status.workspace_id);
        let label = checkout
            .map(|c| c.workspace_label.as_str())
            .unwrap_or(status.workspace_id.as_str());
        let branch = checkout
            .and_then(|c| c.branch.as_deref())
            .unwrap_or("(detached)");
        out.push_str(&format!(
            "{:<9} {label} [{branch}]  {} conflict, {} overlap, {} unknown{}\n",
            severity_name(status.severity),
            status.conflict_count,
            status.overlap_count,
            status.unknown_count,
            if status.runaway { ", runaway" } else { "" },
        ));
        if let Some(set) = changes.get(status.workspace_id.as_str()) {
            if let Some(reason) = &set.degraded_reason {
                out.push_str(&format!("          degraded: {reason}\n"));
            }
        }
    }

    for pairing in &cycle.report.pairings {
        out.push_str(&format!(
            "\n{} <-> {}{}\n",
            pairing.left_workspace_id,
            pairing.right_workspace_id,
            if pairing.approximate {
                "  (approximate: merge base forced)"
            } else {
                ""
            }
        ));
        for shared in &pairing.shared {
            out.push_str(&format!(
                "  {:<8} {}\n",
                verdict_name(shared.verdict),
                shared.path
            ));
        }
    }

    for note in &cycle.notes {
        out.push_str(&format!("\nnote: {note}\n"));
    }
    out
}

/// Stable JSON for `--json`.
///
/// ```text
/// {
///   "schema":   2,                       // JSON_SCHEMA_VERSION
///   "checkouts": [ {
///       "workspace_id", "label", "repo_key", "repo_root", "checkout_path",
///       "branch": string|null, "agent": string|null, "is_linked_worktree": bool,
///       "changed_files": int, "lines_added": int, "lines_removed": int,
///       "has_rename": bool,
///       "degraded": bool, "degraded_reason": string|null } ],
///   "pairings":  [ { "left", "right", "conflict_count", "unknown_count",
///                    "approximate": bool,
///                    "shared": [ { "path", "verdict": "conflict|overlap|unknown" } ] } ],
///   "statuses":  [ { "workspace_id",
///                    "severity": "clean|overlap|runaway|unknown|conflict",
///                    "token", "badge", "overlap_count", "conflict_count",
///                    "unknown_count", "runaway", "lines_changed",
///                    "changed_files" } ],
///   "notes": [ string ]
/// }
/// ```
pub fn json_report(cycle: &Cycle) -> serde_json::Value {
    let changes: BTreeMap<&str, &ChangeSet> = cycle
        .changes
        .iter()
        .map(|(id, set)| (id.as_str(), set))
        .collect();

    let checkouts: Vec<serde_json::Value> = cycle
        .report
        .checkouts
        .iter()
        .map(|checkout| {
            let empty = ChangeSet::default();
            let set = changes
                .get(checkout.workspace_id.as_str())
                .copied()
                .unwrap_or(&empty);
            serde_json::json!({
                "workspace_id": checkout.workspace_id,
                "label": checkout.workspace_label,
                "repo_key": checkout.repo_key.0,
                "repo_root": checkout.repo_root.to_string_lossy(),
                "checkout_path": checkout.checkout_path.to_string_lossy(),
                "branch": checkout.branch,
                "agent": checkout.agent,
                "is_linked_worktree": checkout.is_linked_worktree,
                // Distinct files, with the origin half of a rename counted
                // once, so this agrees with what the runaway threshold sees.
                "changed_files": set.paths.iter().filter(|p| !p.is_rename_origin).count(),
                "lines_added": set.lines_added,
                "lines_removed": set.lines_removed,
                "has_rename": set.has_rename,
                "degraded": set.degraded,
                "degraded_reason": set.degraded_reason,
            })
        })
        .collect();

    let pairings: Vec<serde_json::Value> = cycle
        .report
        .pairings
        .iter()
        .map(|pairing| {
            serde_json::json!({
                "left": pairing.left_workspace_id,
                "right": pairing.right_workspace_id,
                "conflict_count": pairing.conflicts(),
                "unknown_count": pairing.unknowns(),
                "approximate": pairing.approximate,
                "shared": pairing.shared.iter().map(|shared| serde_json::json!({
                    "path": shared.path,
                    "verdict": verdict_name(shared.verdict),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();

    let statuses: Vec<serde_json::Value> = cycle
        .report
        .statuses
        .iter()
        .map(|status| {
            serde_json::json!({
                "workspace_id": status.workspace_id,
                "severity": severity_name(status.severity),
                "token": status.severity.token_name(),
                "badge": crate::render::badge(status),
                "overlap_count": status.overlap_count,
                "conflict_count": status.conflict_count,
                "unknown_count": status.unknown_count,
                "runaway": status.runaway,
                "lines_changed": status.lines_changed,
                "changed_files": status.changed_files,
            })
        })
        .collect();

    serde_json::json!({
        "schema": JSON_SCHEMA_VERSION,
        "checkouts": checkouts,
        "pairings": pairings,
        "statuses": statuses,
        "notes": cycle.notes,
    })
}

pub fn severity_name(severity: Severity) -> &'static str {
    match severity {
        Severity::Clean => "clean",
        Severity::Overlap => "overlap",
        Severity::Runaway => "runaway",
        Severity::Unknown => "unknown",
        Severity::Conflict => "conflict",
    }
}

pub fn verdict_name(verdict: FileVerdict) -> &'static str {
    match verdict {
        FileVerdict::Overlap => "overlap",
        FileVerdict::Conflict => "conflict",
        FileVerdict::Unknown => "unknown",
    }
}
