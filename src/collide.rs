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
    ChangeSet, Checkout, FileVerdict, Pairing, Report, Severity, SharedFile, TargetPrediction,
    TargetVerdict, WorkTrees, WorkspaceStatus,
};
use crate::Result;

/// JSON schema version emitted by `--json`.
///
/// The key deliberately remains `schema`: renaming the field consumers use to
/// detect incompatible changes would itself be incompatible and spend a version
/// bump on cosmetics. Adding a key or an element to an array does not bump the
/// version. Removing or renaming a key, changing a value's type, or adding a
/// value to the `severity` or `verdict` enum does bump it. Array order is not
/// part of the contract.
///
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
                        conflict_type: None,
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
    sort_pairings(&mut pairings);

    let statuses = statuses(checkouts, &filtered, &pairings, config);
    Report {
        checkouts: checkouts.to_vec(),
        pairings,
        statuses,
        targets: Vec::new(),
        changes: changes.to_vec(),
    }
}

fn sort_pairings(pairings: &mut [Pairing]) {
    // The model ties on workspace ids because `--json` emits ids and scripts do
    // not have display labels. The pane deliberately re-ties equal severities
    // on displayed labels because humans read those. Both keys are total, so
    // neither order can flicker between cycles.
    pairings.sort_by(|a, b| {
        a.severity_rank_key()
            .cmp(&b.severity_rank_key())
            .then_with(|| a.left_workspace_id.cmp(&b.left_workspace_id))
            .then_with(|| a.right_workspace_id.cmp(&b.right_workspace_id))
    });
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
    /// Direct-submodule results keyed by their superproject-relative gitlink.
    pub submodules: Vec<git::SubmodulePrediction>,
    /// Machine-stable merge-tree conflict tokens keyed by the paths in the
    /// records that reported them.
    pub conflict_types_by_path: BTreeMap<String, Vec<String>>,
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
        let submodules: BTreeMap<&str, &git::SubmodulePrediction> = prediction
            .submodules
            .iter()
            .map(|nested| (nested.path.as_str(), nested))
            .collect();
        let attributed_conflict_type = |path: &str| {
            prediction
                .conflict_types_by_path
                .get(path)
                .and_then(|types| {
                    types
                        .iter()
                        .find(|token| git::conflict_type_annotation(token).is_some())
                })
                .cloned()
        };
        for shared in &mut pairing.shared {
            let uncomparable_submodule = [key.0, key.1].iter().any(|id| {
                filtered
                    .get(id)
                    .is_some_and(|change| change.uncomparable_submodules.contains(&shared.path))
            });
            let nested = submodules.get(shared.path.as_str());
            shared.verdict = match verdicts.get(shared.path.as_str()) {
                // A gitlink conflict in the outer merge remains a conflict even
                // when the nested contents themselves would merge cleanly.
                Some(true) => {
                    shared.conflict_type = attributed_conflict_type(&shared.path);
                    FileVerdict::Conflict
                }
                _ if nested.is_some_and(|result| result.conflict == Some(true)) => {
                    FileVerdict::Conflict
                }
                _ if nested.is_some_and(|result| result.conflict == Some(false)) => {
                    FileVerdict::Overlap
                }
                _ if uncomparable_submodule => FileVerdict::Unknown,
                // A path git did not flag merges cleanly, even though both
                // sides touched it. That discrimination is the whole point of
                // this plugin: a shared file is not a collision.
                Some(false) | None => {
                    shared.conflict_type = None;
                    FileVerdict::Overlap
                }
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
        // Pair-level rename evidence still cannot say which conflict a rename
        // explains. An unlisted path is therefore a guess unless merge-tree
        // attached a rename-type record to that exact path. The former remains
        // approximate; the latter is git's precise attribution and should not
        // be weakened.
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
        let mut extra: Vec<(String, Option<String>)> = Vec::new();
        for (path, hit) in &prediction.verdicts {
            if !*hit || known.contains(path.as_str()) {
                continue;
            }
            // Ignored paths are ignored here too. `known` was built from the
            // filtered intersection, so without this a generated path that both
            // sides changed can come straight back as a conflict through the
            // unlisted-path door.
            if is_ignored(path, config) {
                continue;
            }
            let conflict_type = attributed_conflict_type(path);
            if listed(path) {
                extra.push((path.clone(), conflict_type));
            } else if renamed {
                guessed |= conflict_type.is_none();
                extra.push((path.clone(), conflict_type));
            }
        }
        if guessed {
            pairing.approximate = true;
        }
        for (path, conflict_type) in extra {
            pairing.shared.push(SharedFile {
                path,
                verdict: FileVerdict::Conflict,
                conflict_type,
            });
        }
        pairing.shared.sort_by(|a, b| a.path.cmp(&b.path));
    }

    // A pair kept only because one side renamed something has nothing to show
    // unless the prediction actually found a conflicting path. Dropping the
    // empty ones here keeps the probe invisible when it comes back clean.
    report.pairings.retain(|pairing| !pairing.shared.is_empty());
    sort_pairings(&mut report.pairings);

    report.statuses = statuses(&report.checkouts, &filtered, &report.pairings, config);
}

/// A change set with configured ignore rules applied, reduced to what the
/// pairing pass needs. Lockfiles and generated output overlap constantly and
/// carry no information, so they are dropped before anything counts them —
/// including the runaway thresholds.
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
/// 1. **The main worktree, when it is open.** Resolving its `<top level>/.git`
///    entry gives the `repo_key` itself with no guessing, and its top level *is*
///    the repository root. This fires for the ordinary layout, for
///    `--separate-git-dir`, and for a submodule whose git directory lives under
///    its superproject. A linked worktree cannot satisfy it because its gitfile
///    names `<store>/worktrees/<name>` rather than the common store.
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
            let git_dir = git::worktree_git_dir(top)?;
            (git_dir == std::path::Path::new(key)).then(|| top.to_path_buf())
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

/// Where a checkout's working tree starts, resolved by Git through a bounded
/// subprocess rather than by unbounded filesystem metadata calls.
///
/// The gathering path reuses the top level returned by
/// [`git::read_change_set`]. This wrapper remains for embedders and fixtures
/// that need the answer independently.
pub fn work_tree_root(path: &std::path::Path) -> std::path::PathBuf {
    git::work_tree_root(
        path,
        std::time::Duration::from_secs(crate::config::DEFAULT_GIT_TIMEOUT_SECONDS),
    )
    .unwrap_or_else(|_| path.to_path_buf())
}

/// Match configured suffix and glob rules against a repository-relative path.
///
/// A bare suffix `ends_with` is too eager: `go.sum` would swallow
/// `tools/cargo.sum` and `Cargo.lock` would swallow
/// `vendor/NotReallyCargo.lock`, dropping real changes from the change set with
/// nothing to show for it. A suffix that starts with `.` is an extension and
/// may match mid-name; anything else must begin at the start of the path or
/// straight after a `/`.
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
    }) || crate::ignore::matches_any(path, &config.ignore_globs)
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

struct PredictedPair {
    verdicts: PairVerdicts,
    prediction: Option<git::PairPrediction>,
}

struct RetainedGather {
    cycle: Cycle,
    predictor: Option<git::Predictor>,
    predictions: Vec<RetainedPrediction>,
}

struct RetainedPrediction {
    left_workspace_id: String,
    right_workspace_id: String,
    prediction: git::PairPrediction,
}

/// Talks to herdr and git, then runs the pure pass.
pub fn gather(config: &Config) -> Result<Cycle> {
    Ok(gather_retained(config)?.cycle)
}

fn gather_retained(config: &Config) -> Result<RetainedGather> {
    let mut herdr = crate::herdr::Herdr::connect()?;
    let checkouts = herdr.checkouts()?;
    let skipped = herdr.skipped_worktrees();
    let scope = crate::config::non_empty_env("HERDR_WORKSPACE_ID");
    let mut gathered = gather_for_retained_scoped(checkouts, config, scope.as_deref())?;
    // A workspace herdr calls a repository but whose worktree object this client
    // could not read is dropped, which makes the session look smaller than it
    // is. The daemon reports that; so must the one-shot commands, which are what
    // somebody runs when they are actually looking.
    if skipped > 0 {
        gathered.cycle.notes.push(format!(
            "{skipped} workspace(s) carried a worktree object this client could not read; \
             they are missing from this report"
        ));
    }
    Ok(gathered)
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
    Ok(gather_for_retained(checkouts, config)?.cycle)
}

/// Gathers only the verified repository containing `workspace_id`.
///
/// Herdr actions and pane entrypoints provide this id in their invocation
/// environment. The daemon deliberately uses [`gather_for`] instead so its
/// badges remain session-wide.
pub fn gather_for_workspace(
    checkouts: Vec<Checkout>,
    config: &Config,
    workspace_id: &str,
) -> Result<Cycle> {
    Ok(gather_for_retained_scoped(checkouts, config, Some(workspace_id))?.cycle)
}

fn gather_for_retained(checkouts: Vec<Checkout>, config: &Config) -> Result<RetainedGather> {
    gather_for_retained_scoped(checkouts, config, None)
}

fn gather_for_retained_scoped(
    checkouts: Vec<Checkout>,
    config: &Config,
    workspace_id: Option<&str>,
) -> Result<RetainedGather> {
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
    if let Some(workspace_id) = workspace_id {
        let repo_key = verified
            .iter()
            .find(|checkout| checkout.workspace_id == workspace_id)
            .map(|checkout| checkout.repo_key.clone())
            .ok_or_else(|| {
                format!(
                    "invocation workspace `{workspace_id}` is not a readable git-backed workspace"
                )
            })?;
        verified.retain(|checkout| checkout.repo_key == repo_key);
    }

    // Filled from the top-level answer `read_change_set` already needs for
    // untracked-file volume. The previous filesystem walk duplicated that
    // discovery outside the configured Git deadline.
    let mut trees = WorkTrees::new();

    // The integration ref is a repository property. Resolve its name once from
    // one checkout and reuse that exact answer for every change set and target
    // prediction in the repository.
    let mut integration_refs = BTreeMap::new();
    for checkout in &verified {
        integration_refs
            .entry(checkout.repo_key.clone())
            .or_insert_with(|| base_ref_for(&checkout.checkout_path, config));
    }

    let mut changes: Vec<(String, ChangeSet)> = Vec::new();
    for checkout in &verified {
        let base = integration_refs
            .get(&checkout.repo_key)
            .expect("every verified checkout has an integration-ref decision");
        match git::read_change_set(&checkout.checkout_path, base, config.git_timeout) {
            Ok(read) => {
                trees.insert(checkout.workspace_id.clone(), read.top_level);
                changes.push((checkout.workspace_id.clone(), read.change_set));
            }
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
    agree_on_repo_root(&mut verified, &trees);

    let mut report = analyse(&verified, &changes, &trees, config);

    let mut retained_predictor = None;
    let mut retained_predictions = Vec::new();
    if config.predict_conflicts && !verified.is_empty() {
        let by_id: BTreeMap<&str, &Checkout> = verified
            .iter()
            .map(|c| (c.workspace_id.as_str(), c))
            .collect();
        let changes_by_id: BTreeMap<&str, &ChangeSet> =
            changes.iter().map(|(id, set)| (id.as_str(), set)).collect();
        let paired: BTreeSet<&str> = report
            .pairings
            .iter()
            .flat_map(|pairing| {
                [
                    pairing.left_workspace_id.as_str(),
                    pairing.right_workspace_id.as_str(),
                ]
            })
            .collect();

        let mut predictor = git::Predictor::new(config.git_timeout)?;
        let mut primed: BTreeSet<&str> = BTreeSet::new();
        let mut prime_errors: BTreeMap<&str, String> = BTreeMap::new();
        for checkout in &verified {
            let change = changes_by_id
                .get(checkout.workspace_id.as_str())
                .expect("every verified checkout has a change set");
            let target_ref = integration_refs
                .get(&checkout.repo_key)
                .expect("integration-ref decision exists");
            // A commitless checkout cannot participate in either prediction.
            // With no target and no pairing, no later result can consume a
            // snapshot, so paying to create one would only touch scratch state.
            if !pairable(change)
                || (target_ref == git::NO_INTEGRATION_REF
                    && !paired.contains(checkout.workspace_id.as_str()))
            {
                continue;
            }
            match predictor.prime(&checkout.checkout_path) {
                Ok(()) => {
                    primed.insert(checkout.workspace_id.as_str());
                }
                Err(err) => {
                    let message = err.to_string();
                    notes.push(format!("{}: {message}", checkout.checkout_path.display()));
                    prime_errors.insert(checkout.workspace_id.as_str(), message);
                }
            }
        }

        // Resolve each local ref to a commit before the immutable prediction
        // phase. No command here fetches or consults a remote; a stale
        // origin/main intentionally remains a prediction about that stale ref.
        let mut target_prime_errors: BTreeMap<&crate::model::RepoKey, String> = BTreeMap::new();
        let mut target_primed: BTreeSet<&crate::model::RepoKey> = BTreeSet::new();
        for checkout in &verified {
            let target_ref = integration_refs
                .get(&checkout.repo_key)
                .expect("integration-ref decision exists");
            if target_ref == git::NO_INTEGRATION_REF
                || target_primed.contains(&checkout.repo_key)
                || target_prime_errors.contains_key(&checkout.repo_key)
                || !primed.contains(checkout.workspace_id.as_str())
            {
                continue;
            }
            match predictor.prime_target(&checkout.checkout_path, target_ref) {
                Ok(()) => {
                    target_primed.insert(&checkout.repo_key);
                }
                Err(err) => {
                    target_prime_errors.insert(&checkout.repo_key, err.to_string());
                }
            }
        }

        // Direct submodules are repositories in their own right. Resolve and
        // snapshot every needed nested side here, while the predictor is still
        // mutable and this phase is deliberately sequential.
        let changes_by_id: BTreeMap<&str, &ChangeSet> =
            changes.iter().map(|(id, set)| (id.as_str(), set)).collect();
        for pairing in &report.pairings {
            let Some(left) = by_id.get(pairing.left_workspace_id.as_str()) else {
                continue;
            };
            let Some(right) = by_id.get(pairing.right_workspace_id.as_str()) else {
                continue;
            };
            for shared in &pairing.shared {
                let needs_nested = [
                    pairing.left_workspace_id.as_str(),
                    pairing.right_workspace_id.as_str(),
                ]
                .iter()
                .any(|id| {
                    changes_by_id.get(id).is_some_and(|set| {
                        set.paths.iter().any(|changed| {
                            changed.path == shared.path && changed.submodule_contents_uncomparable
                        })
                    })
                });
                if needs_nested {
                    predictor.prime_submodule(&left.checkout_path, &shared.path);
                    predictor.prime_submodule(&right.checkout_path, &shared.path);
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

        let predicted = predict_all(&predictor, &jobs, &mut notes);
        let verdicts: Vec<PairVerdicts> =
            predicted.iter().map(|pair| pair.verdicts.clone()).collect();
        apply_predictions(&mut report, &verdicts, &changes, config);
        retained_predictions = predicted
            .into_iter()
            .filter_map(|pair| {
                pair.prediction.map(|prediction| RetainedPrediction {
                    left_workspace_id: pair.verdicts.left_workspace_id,
                    right_workspace_id: pair.verdicts.right_workspace_id,
                    prediction,
                })
            })
            .collect();

        report.targets = verified
            .iter()
            .map(|checkout| {
                let target_ref = integration_refs
                    .get(&checkout.repo_key)
                    .expect("integration-ref decision exists");
                if target_ref == git::NO_INTEGRATION_REF {
                    return TargetPrediction {
                        workspace_id: checkout.workspace_id.clone(),
                        target_ref: None,
                        verdict: TargetVerdict::Unknown,
                        approximate: false,
                        advisory: false,
                        reason: Some("no integration ref found".to_string()),
                    };
                }
                let unknown = |reason: String| TargetPrediction {
                    workspace_id: checkout.workspace_id.clone(),
                    target_ref: Some(target_ref.clone()),
                    verdict: TargetVerdict::Unknown,
                    approximate: false,
                    advisory: false,
                    reason: Some(reason),
                };
                if !pairable(
                    changes_by_id
                        .get(checkout.workspace_id.as_str())
                        .expect("every verified checkout has a change set"),
                ) {
                    return unknown("checkout has no commit to predict".to_string());
                }
                if let Some(err) = prime_errors.get(checkout.workspace_id.as_str()) {
                    return unknown(format!(
                        "prediction against `{target_ref}` failed: checkout could not be primed: {err}"
                    ));
                }
                if let Some(err) = target_prime_errors.get(&checkout.repo_key) {
                    return unknown(err.clone());
                }
                match predictor.predict_target(&checkout.checkout_path, target_ref) {
                    Ok(git::TargetMergeOutcome::NoCommonAncestor) => {
                        unknown(format!("no common ancestor with `{target_ref}`"))
                    }
                    Ok(git::TargetMergeOutcome::Predicted {
                        conflicts,
                        approximate,
                        advisory,
                    }) => TargetPrediction {
                        workspace_id: checkout.workspace_id.clone(),
                        target_ref: Some(target_ref.clone()),
                        verdict: if conflicts {
                            TargetVerdict::Conflict
                        } else {
                            TargetVerdict::Clean
                        },
                        approximate,
                        advisory,
                        reason: None,
                    },
                    Err(err) => unknown(format!(
                        "prediction against `{target_ref}` failed: {err}"
                    )),
                }
            })
            .collect();

        // Deliberately do not recompute statuses here. The badge has one slot,
        // and promoting every stale-branch target conflict before this signal
        // has been observed in real sessions could mute the pairwise collision
        // warning users already rely on.

        retained_predictor = Some(predictor);
    }

    Ok(RetainedGather {
        cycle: Cycle {
            report,
            changes,
            notes,
        },
        predictor: retained_predictor,
        predictions: retained_predictions,
    })
}

/// Runs phase 1 (and, where needed, phase 2) for every pair, fanning out across
/// std threads. Every checkout is already primed, so the predictor is immutable
/// here and git itself is lock-free for everything except `status`.
fn predict_all(
    predictor: &git::Predictor,
    jobs: &[(&Pairing, &Checkout, &Checkout)],
    notes: &mut Vec<String>,
) -> Vec<PredictedPair> {
    if jobs.is_empty() {
        return Vec::new();
    }
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 8)
        .min(jobs.len());

    let mut results: Vec<(PredictedPair, Vec<String>)> = Vec::with_capacity(jobs.len());
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
                            Ok(prediction) => {
                                let mut nested_notes = Vec::new();
                                for nested in &prediction.submodules {
                                    if nested.approximate {
                                        nested_notes.push(format!(
                                            "{} vs {}: submodule `{}` has multiple nested merge \
                                             bases, so one was forced and its verdict approximates \
                                             what a real nested merge would do",
                                            pairing.left_workspace_id,
                                            pairing.right_workspace_id,
                                            nested.path
                                        ));
                                    }
                                    match nested.conflict {
                                        Some(true) => {
                                            let detail = if nested.conflicting_paths.is_empty() {
                                                "the nested merge reported a pair-level conflict \
                                                 without naming a path"
                                                    .to_string()
                                            } else {
                                                format!(
                                                    "nested paths {} conflict",
                                                    nested
                                                        .conflicting_paths
                                                        .iter()
                                                        .map(|path| format!("`{path}`"))
                                                        .collect::<Vec<_>>()
                                                        .join(", ")
                                                )
                                            };
                                            nested_notes.push(format!(
                                                "{} vs {}: submodule `{}` conflicts internally; \
                                                 {detail}",
                                                pairing.left_workspace_id,
                                                pairing.right_workspace_id,
                                                nested.path
                                            ));
                                        }
                                        None => nested_notes.push(format!(
                                            "{} vs {}: submodule `{}` comparison is unknown: {}",
                                            pairing.left_workspace_id,
                                            pairing.right_workspace_id,
                                            nested.path,
                                            nested.reason.as_deref().unwrap_or(
                                                "the nested comparison could not complete"
                                            )
                                        )),
                                        Some(false) => {}
                                    }
                                }
                                // `--why` keeps the prediction to read its merged
                                // tree, so every field it needs is cloned rather
                                // than moved out of it here.
                                let verdicts = PairVerdicts {
                                    left_workspace_id: pairing.left_workspace_id.clone(),
                                    right_workspace_id: pairing.right_workspace_id.clone(),
                                    verdicts: prediction.verdicts.clone(),
                                    conflict_types_by_path: prediction
                                        .conflict_types_by_path
                                        .clone(),
                                    submodules: prediction.submodules.clone(),
                                    failed: false,
                                    approximate: prediction.approximate,
                                };
                                (
                                    PredictedPair {
                                        verdicts,
                                        prediction: Some(prediction),
                                    },
                                    nested_notes,
                                )
                            }
                            Err(err) => (
                                PredictedPair {
                                    verdicts: PairVerdicts {
                                        left_workspace_id: pairing.left_workspace_id.clone(),
                                        right_workspace_id: pairing.right_workspace_id.clone(),
                                        verdicts: Vec::new(),
                                        submodules: Vec::new(),
                                        conflict_types_by_path: BTreeMap::new(),
                                        failed: true,
                                        approximate: false,
                                    },
                                    prediction: None,
                                },
                                vec![format!(
                                    "{} vs {}: {err}",
                                    left.checkout_path.display(),
                                    right.checkout_path.display()
                                )],
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
    for (prediction, prediction_notes) in results {
        notes.extend(prediction_notes);
        predictions.push(prediction);
    }
    predictions
}

/// Result of explaining one path. A failed prediction is kept separate from
/// the text so the CLI can print the reason and still return a non-zero status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhyReport {
    pub text: String,
    pub prediction_failed: bool,
}

/// Drives `--why` from a supplied checkout list, for fixtures and embedders.
pub fn why_for(checkouts: Vec<Checkout>, config: &Config, path: &str) -> Result<WhyReport> {
    let mut gather_config = config.clone();
    gather_config.predict_conflicts = true;
    let gathered = gather_for_retained(checkouts, &gather_config)?;
    explain_path(&gathered, path)
}

fn explain_path(gathered: &RetainedGather, path: &str) -> Result<WhyReport> {
    let cycle = &gathered.cycle;
    let shown_path = git::lossy(path.as_bytes());
    let pairs: Vec<(&Pairing, bool)> = cycle
        .report
        .pairings
        .iter()
        .filter_map(|pairing| {
            if pairing.shared.iter().any(|shared| shared.path == path) {
                return Some((pairing, false));
            }
            let prediction = retained_prediction(&gathered.predictions, pairing)?;
            let unnamed_pair_conflict =
                prediction.pair_conflict && prediction.conflicted_paths.is_empty();
            if unnamed_pair_conflict && path_listed_for_pair(cycle, pairing, path) {
                Some((pairing, true))
            } else {
                None
            }
        })
        .collect();

    if pairs.is_empty() {
        if let Some(report) = unavailable_path_report(cycle, &shown_path) {
            return Ok(report);
        }
        return Ok(WhyReport {
            text: format!("`{shown_path}` is not shared by any worktree pair\n"),
            prediction_failed: false,
        });
    }

    let by_id: BTreeMap<&str, &Checkout> = cycle
        .report
        .checkouts
        .iter()
        .map(|checkout| (checkout.workspace_id.as_str(), checkout))
        .collect();
    let mut out = String::new();
    let mut prediction_failed = false;

    for (pairing, unnamed_pair_conflict) in pairs {
        let Some(left) = by_id.get(pairing.left_workspace_id.as_str()).copied() else {
            prediction_failed = true;
            out.push_str(&format!(
                "unknown: `{shown_path}`: prediction did not run because the left worktree \
                 disappeared from the gathered report\n"
            ));
            continue;
        };
        let Some(right) = by_id.get(pairing.right_workspace_id.as_str()).copied() else {
            prediction_failed = true;
            out.push_str(&format!(
                "unknown: `{shown_path}`: prediction did not run because the right worktree \
                 disappeared from the gathered report\n"
            ));
            continue;
        };
        let left_name = why_name(Some(left));
        let right_name = why_name(Some(right));
        let Some(prediction) = retained_prediction(&gathered.predictions, pairing) else {
            prediction_failed = true;
            out.push_str(&format!(
                "unknown: `{shown_path}` between {left_name} and {right_name}: \
                 prediction did not run\n"
            ));
            continue;
        };

        let verdict = if unnamed_pair_conflict {
            FileVerdict::Conflict
        } else {
            pairing
                .shared
                .iter()
                .find(|shared| shared.path == path)
                .map(|shared| shared.verdict)
                .unwrap_or(FileVerdict::Unknown)
        };
        let blob = if verdict == FileVerdict::Conflict {
            match gathered.predictor.as_ref() {
                Some(predictor) => {
                    match predictor.merged_blob(&left.checkout_path, &prediction.merged_tree, path)
                    {
                        Ok(blob) if blob.is_empty() => WhyBlob::Empty,
                        Ok(blob) => WhyBlob::Content(crate::render::sanitize_content(&blob)),
                        Err(err) => WhyBlob::Unreadable(err.to_string()),
                    }
                }
                None => WhyBlob::Unreadable("the retained prediction tree is unavailable".into()),
            }
        } else {
            WhyBlob::NotNeeded
        };

        let pair_report = explain_pair_prediction(
            prediction,
            verdict,
            !path_listed_for_pair(cycle, pairing, path),
            &left_name,
            &right_name,
            &shown_path,
            blob,
        );
        out.push_str(&pair_report.text);
        prediction_failed |= pair_report.prediction_failed;
    }

    for note in &cycle.notes {
        prediction_failed = true;
        out.push_str(&format!("note: {note}\n"));
    }

    Ok(WhyReport {
        text: out,
        prediction_failed,
    })
}

fn retained_prediction<'a>(
    predictions: &'a [RetainedPrediction],
    pairing: &Pairing,
) -> Option<&'a git::PairPrediction> {
    predictions
        .iter()
        .find(|prediction| {
            prediction.left_workspace_id == pairing.left_workspace_id
                && prediction.right_workspace_id == pairing.right_workspace_id
        })
        .map(|prediction| &prediction.prediction)
}

fn path_listed_for_pair(cycle: &Cycle, pairing: &Pairing, path: &str) -> bool {
    cycle.changes.iter().any(|(id, changes)| {
        (id == &pairing.left_workspace_id || id == &pairing.right_workspace_id)
            && changes.paths.iter().any(|changed| changed.path == path)
    })
}

fn unavailable_path_report(cycle: &Cycle, shown_path: &str) -> Option<WhyReport> {
    let changes: BTreeMap<&str, &ChangeSet> = cycle
        .changes
        .iter()
        .map(|(id, changes)| (id.as_str(), changes))
        .collect();
    let mut out = String::new();
    for checkout in &cycle.report.checkouts {
        match changes.get(checkout.workspace_id.as_str()) {
            Some(changes) if changes.degraded => {
                let reason = changes
                    .degraded_reason
                    .as_deref()
                    .unwrap_or("reason not reported");
                out.push_str(&format!(
                    "unknown: `{shown_path}` may be shared with {}: \
                     its change set is degraded: {reason}\n",
                    why_name(Some(checkout))
                ));
            }
            None => out.push_str(&format!(
                "unknown: `{shown_path}` may be shared with {}: \
                 its change set is unavailable\n",
                why_name(Some(checkout))
            )),
            Some(_) => {}
        }
    }
    if out.is_empty() && !cycle.notes.is_empty() {
        out.push_str(&format!(
            "unknown: `{shown_path}` may be shared by a worktree that could not be read\n"
        ));
    }
    if out.is_empty() {
        return None;
    }
    for note in &cycle.notes {
        out.push_str(&format!("note: {note}\n"));
    }
    Some(WhyReport {
        text: out,
        prediction_failed: true,
    })
}

enum WhyBlob {
    NotNeeded,
    Content(String),
    Empty,
    Unreadable(String),
}

fn explain_pair_prediction(
    prediction: &git::PairPrediction,
    verdict: FileVerdict,
    rename_inferred: bool,
    left_name: &str,
    right_name: &str,
    shown_path: &str,
    blob: WhyBlob,
) -> WhyReport {
    let mut out = String::new();

    let mut named_advisory = false;
    for (advisory, name) in [
        (prediction.left_advisory, left_name),
        (prediction.right_advisory, right_name),
    ] {
        if advisory {
            named_advisory = true;
            out.push_str(&format!(
                "advisory: a merge is in progress in {name}, so these verdicts were computed \
                 from a tree that still contains conflict markers.\n"
            ));
        }
    }
    if prediction.advisory && !named_advisory {
        out.push_str(&format!(
            "advisory: a merge is in progress in one of {left_name} or {right_name}, so these \
             verdicts were computed from a tree that still contains conflict markers.\n"
        ));
    }
    if prediction.approximate {
        out.push_str(
            "approximate: these two histories offer no single merge base, so one was forced and \
             the verdicts below approximate what a real merge would do.\n",
        );
    }
    if rename_inferred {
        out.push_str(
            "approximate: git reported this conflicting path, but neither change set listed it; \
             a rename makes the match plausible rather than certain.\n",
        );
    }

    let prediction_failed = match verdict {
        FileVerdict::Overlap => {
            out.push_str(&format!(
                "overlap: `{shown_path}` was touched by {left_name} and {right_name}, \
                 but their changes merge cleanly\n"
            ));
            false
        }
        FileVerdict::Unknown => {
            out.push_str(&format!(
                "unknown: `{shown_path}` between {left_name} and {right_name}: \
                 prediction did not produce a verdict\n"
            ));
            true
        }
        FileVerdict::Conflict => match blob {
            WhyBlob::Content(content) if has_conflict_marker(&content) => {
                out.push_str(&format!(
                    "conflict: `{shown_path}` between {left_name} and {right_name}\n"
                ));
                out.push_str(&content);
                if !content.ends_with('\n') {
                    out.push('\n');
                }
                false
            }
            WhyBlob::Content(_) if prediction.conflicted_paths.is_empty() => {
                let kinds = if prediction.conflict_types.is_empty() {
                    "conflict type not reported".to_string()
                } else {
                    prediction.conflict_types.join(", ")
                };
                out.push_str(&format!(
                    "conflict: the merge between {left_name} and {right_name} conflicts as a \
                     whole ({kinds}); git named no conflicting file, so there are no hunks for \
                     `{shown_path}`\n"
                ));
                true
            }
            WhyBlob::Content(_) => {
                let kinds = if prediction.conflict_types.is_empty() {
                    "conflict type not reported".to_string()
                } else {
                    prediction.conflict_types.join(", ")
                };
                out.push_str(&format!(
                    "conflict: the merge between {left_name} and {right_name} conflicts as a \
                     whole ({kinds}); git named `{shown_path}` as conflicting, but its merged \
                     blob contains no conflict markers, so there are no hunks to show\n"
                ));
                true
            }
            WhyBlob::Empty => {
                out.push_str(&format!(
                    "unknown: `{shown_path}` between {left_name} and {right_name}: \
                     prediction ran but its conflicting blob was empty\n"
                ));
                true
            }
            WhyBlob::Unreadable(err) => {
                out.push_str(&format!(
                    "unknown: `{shown_path}` between {left_name} and {right_name}: \
                     prediction ran but its conflicting hunks could not be read: {err}\n"
                ));
                true
            }
            WhyBlob::NotNeeded => {
                out.push_str(&format!(
                    "unknown: `{shown_path}` between {left_name} and {right_name}: \
                     prediction marked a conflict without retaining its merged blob\n"
                ));
                true
            }
        },
    };

    WhyReport {
        text: out,
        prediction_failed,
    }
}

fn has_conflict_marker(content: &str) -> bool {
    content.starts_with("<<<<<<<") || content.contains("\n<<<<<<<")
}

fn why_name(checkout: Option<&Checkout>) -> String {
    match checkout {
        Some(checkout) => git::lossy(checkout.workspace_label.as_bytes()),
        None => "(missing worktree)".to_string(),
    }
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

pub fn run_why(config: &Config, path: &str) -> Result<()> {
    let mut gather_config = config.clone();
    gather_config.predict_conflicts = true;
    let gathered = gather_retained(&gather_config)?;
    let report = explain_path(&gathered, path)?;
    print!("{}", report.text);
    if report.prediction_failed {
        Err("one or more conflict predictions did not produce an answer".into())
    } else {
        Ok(())
    }
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
///       "degraded": bool, "degraded_reason": string|null,
///       "target_ref": string|null,
///       "target_verdict": "clean|conflict|unknown"|null,
///       "target_approximate": bool|null, "target_advisory": bool|null,
///       "target_reason": string|null } ],
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
            let target = cycle.report.target_prediction(&checkout.workspace_id);
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
                "target_ref": target.and_then(|target| target.target_ref.as_deref()),
                "target_verdict": target.map(|target| target_verdict_name(target.verdict)),
                "target_approximate": target.map(|target| target.approximate),
                "target_advisory": target.map(|target| target.advisory),
                "target_reason": target.and_then(|target| target.reason.as_deref()),
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

pub fn target_verdict_name(verdict: TargetVerdict) -> &'static str {
    match verdict {
        TargetVerdict::Clean => "clean",
        TargetVerdict::Conflict => "conflict",
        TargetVerdict::Unknown => "unknown",
    }
}

#[cfg(test)]
mod why_tests {
    use super::{explain_pair_prediction, FileVerdict, WhyBlob};
    use crate::git::PairPrediction;

    fn prediction() -> PairPrediction {
        PairPrediction {
            verdicts: vec![("conflict.txt".to_string(), true)],
            conflicted_paths: vec!["conflict.txt".to_string()],
            merged_tree: "tree".to_string(),
            ..PairPrediction::default()
        }
    }

    #[test]
    fn why_qualifiers_precede_the_verdict_they_limit() {
        let mut prediction = prediction();
        prediction.approximate = true;
        prediction.advisory = true;
        prediction.left_advisory = true;
        let report = explain_pair_prediction(
            &prediction,
            FileVerdict::Conflict,
            false,
            "left",
            "right",
            "conflict.txt",
            WhyBlob::Content("<<<<<<< left\n=======\n>>>>>>> right\n".to_string()),
        );

        assert!(!report.prediction_failed, "{}", report.text);
        let advisory = report.text.find("advisory:").expect("advisory qualifier");
        let approximate = report
            .text
            .find("approximate:")
            .expect("approximate qualifier");
        let verdict = report.text.find("conflict:").expect("conflict verdict");
        assert!(advisory < verdict, "{}", report.text);
        assert!(approximate < verdict, "{}", report.text);
        assert!(report.text.contains("merge is in progress in left"));
        assert!(report.text.contains("no single merge base"));
    }

    #[test]
    fn marker_free_pair_conflict_is_not_presented_as_hunks() {
        let mut prediction = prediction();
        prediction.conflicted_paths.clear();
        prediction.pair_conflict = true;
        prediction.conflict_types = vec!["CONFLICT (directory rename suggested)".to_string()];
        let report = explain_pair_prediction(
            &prediction,
            FileVerdict::Conflict,
            false,
            "left",
            "right",
            "docs/notes-c.md",
            WhyBlob::Content("clean merged content\n".to_string()),
        );

        assert!(report.prediction_failed, "{}", report.text);
        assert!(
            report.text.contains("conflicts as a whole"),
            "{}",
            report.text
        );
        assert!(
            report.text.contains("git named no conflicting file"),
            "{}",
            report.text
        );
        assert!(
            !report.text.contains("conflict: `docs/notes-c.md`"),
            "{}",
            report.text
        );
    }

    #[test]
    fn empty_and_unreadable_conflict_blobs_request_failure_status() {
        let prediction = prediction();
        for blob in [
            WhyBlob::Empty,
            WhyBlob::Unreadable("permission denied".to_string()),
        ] {
            let report = explain_pair_prediction(
                &prediction,
                FileVerdict::Conflict,
                false,
                "left",
                "right",
                "conflict.txt",
                blob,
            );
            assert!(report.prediction_failed, "{}", report.text);
            assert!(report.text.starts_with("unknown:"), "{}", report.text);
        }
    }
}
