//! Analysis: group checkouts by repo, pair them, and turn shared paths into
//! severities. Pure over its inputs so it can be tested without herdr or git.
//!
//! The split is deliberate:
//!
//! * [`analyse`] and [`apply_predictions`] are pure functions over the data
//!   they are handed. No git, no socket, no clock.
//! * [`run_once`] and [`run_json`] do the impure gathering — talk to herdr,
//!   shell out to git, verify repo identity — and then call the pure pass.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::Config;
use crate::git;
use crate::model::{
    ChangeSet, Checkout, FileVerdict, Pairing, Report, Severity, SharedFile, WorkspaceStatus,
};
use crate::Result;

/// JSON schema version emitted by `--json`. Bump on any incompatible change.
pub const JSON_SCHEMA_VERSION: u32 = 1;

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
/// plain [`FileVerdict::Overlap`] and never escalates to a conflict.
pub fn analyse(checkouts: &[Checkout], changes: &[(String, ChangeSet)], config: &Config) -> Report {
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
                        verdict: unresolved,
                    })
                    .collect();
                if shared.is_empty() {
                    continue;
                }
                pairings.push(Pairing {
                    left_workspace_id: left.workspace_id.clone(),
                    right_workspace_id: right.workspace_id.clone(),
                    shared,
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
        let verdicts: BTreeMap<&str, bool> = prediction
            .verdicts
            .iter()
            .map(|(path, hit)| (path.as_str(), *hit))
            .collect();
        for shared in &mut pairing.shared {
            shared.verdict = match verdicts.get(shared.path.as_str()) {
                Some(true) => FileVerdict::Conflict,
                // A path git did not flag merges cleanly, even though both
                // sides touched it. That discrimination is the whole point of
                // this plugin: a shared file is not a collision.
                Some(false) | None => FileVerdict::Overlap,
            };
        }
        let known: BTreeSet<&str> = pairing.shared.iter().map(|s| s.path.as_str()).collect();
        let extra: Vec<String> = prediction
            .verdicts
            .iter()
            .filter(|(path, hit)| *hit && !known.contains(path.as_str()))
            .map(|(path, _)| path.clone())
            .collect();
        for path in extra {
            pairing.shared.push(SharedFile {
                path,
                verdict: FileVerdict::Conflict,
            });
        }
        pairing.shared.sort_by(|a, b| a.path.cmp(&b.path));
    }

    let filtered: BTreeMap<&str, FilteredChange> = changes
        .iter()
        .map(|(id, set)| (id.as_str(), FilteredChange::new(set, config)))
        .collect();
    report.statuses = statuses(&report.checkouts, &filtered, &report.pairings, config);
}

/// A change set with `ignore_suffixes` applied, reduced to what the pairing
/// pass needs. Lockfiles and generated manifests overlap on essentially every
/// concurrent branch and carry no information, so they are dropped before
/// anything counts them — including the runaway thresholds.
#[derive(Debug, Clone)]
struct FilteredChange {
    paths: BTreeSet<String>,
    lines_changed: u64,
    pairable: bool,
}

impl FilteredChange {
    fn new(set: &ChangeSet, config: &Config) -> Self {
        let paths: BTreeSet<String> = set
            .paths
            .iter()
            .map(|p| p.path.as_str())
            .filter(|path| !is_ignored(path, config))
            .map(str::to_string)
            .collect();
        Self {
            paths,
            lines_changed: set.lines_changed(),
            pairable: pairable(set),
        }
    }
}

/// Suffix match against `Config::ignore_suffixes`.
pub fn is_ignored(path: &str, config: &Config) -> bool {
    config
        .ignore_suffixes
        .iter()
        .any(|suffix| !suffix.is_empty() && path.ends_with(suffix.as_str()))
}

/// An unborn branch and a branch deleted underneath a worktree both leave the
/// checkout with no commit, so there is nothing to merge against and the
/// checkout is excluded from pairing rather than guessed at.
pub fn pairable(set: &ChangeSet) -> bool {
    match &set.degraded_reason {
        None => true,
        Some(reason) => !git::UNPAIRABLE_REASONS
            .iter()
            .any(|code| reason.contains(code)),
    }
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
            for pairing in pairings {
                if pairing.left_workspace_id != id && pairing.right_workspace_id != id {
                    continue;
                }
                for shared in &pairing.shared {
                    match shared.verdict {
                        FileVerdict::Conflict => {
                            conflicts.insert(shared.path.as_str());
                        }
                        FileVerdict::Overlap | FileVerdict::Unknown => {
                            overlaps.insert(shared.path.as_str());
                        }
                    }
                }
            }
            // A path that conflicts in one pairing is a conflict, full stop; it
            // should not also inflate the overlap count.
            for path in &conflicts {
                overlaps.remove(path);
            }

            let runaway = filtered
                .get(id)
                .map(|change| {
                    change.paths.len() > config.runaway_files
                        || change.lines_changed > config.runaway_lines
                })
                .unwrap_or(false);

            let severity = if !conflicts.is_empty() {
                Severity::Conflict
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
                runaway,
                lines_changed: filtered.get(id).map(|c| c.lines_changed).unwrap_or(0),
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
    gather_for(checkouts, config)
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
    git::integration_ref(checkout, config.git_timeout).unwrap_or_else(|_| "HEAD".to_string())
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

    let mut changes: Vec<(String, ChangeSet)> = Vec::new();
    for checkout in &verified {
        let base = base_ref_for(&checkout.checkout_path, config);
        match git::change_set(&checkout.checkout_path, &base, config.git_timeout) {
            Ok(set) => changes.push((checkout.workspace_id.clone(), set)),
            Err(err) => {
                notes.push(format!("{}: {err}", checkout.checkout_path.display()));
                changes.push((checkout.workspace_id.clone(), ChangeSet::default()));
            }
        }
    }

    let mut report = analyse(&verified, &changes, config);

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
                                },
                                None,
                            ),
                            Err(err) => (
                                PairVerdicts {
                                    left_workspace_id: pairing.left_workspace_id.clone(),
                                    right_workspace_id: pairing.right_workspace_id.clone(),
                                    verdicts: Vec::new(),
                                    failed: true,
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
            if let Ok(chunk) = handle.join() {
                results.extend(chunk);
            }
        }
    });

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
            "{:<9} {label} [{branch}]  {} conflict, {} overlap{}\n",
            severity_name(status.severity),
            status.conflict_count,
            status.overlap_count,
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
            "\n{} <-> {}\n",
            pairing.left_workspace_id, pairing.right_workspace_id
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
///   "schema":   1,                       // JSON_SCHEMA_VERSION
///   "checkouts": [ {
///       "workspace_id", "label", "repo_key", "repo_root", "checkout_path",
///       "branch": string|null, "agent": string|null, "is_linked_worktree": bool,
///       "changed_files": int, "lines_added": int, "lines_removed": int,
///       "degraded": bool, "degraded_reason": string|null } ],
///   "pairings":  [ { "left", "right", "conflict_count",
///                    "shared": [ { "path", "verdict": "conflict|overlap|unknown" } ] } ],
///   "statuses":  [ { "workspace_id", "severity": "clean|overlap|runaway|conflict",
///                    "token", "badge", "overlap_count", "conflict_count", "runaway" } ],
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
                "changed_files": set.paths.len(),
                "lines_added": set.lines_added,
                "lines_removed": set.lines_removed,
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
                "runaway": status.runaway,
                "lines_changed": status.lines_changed,
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
