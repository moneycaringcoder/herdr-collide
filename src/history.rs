//! Durable conflict episodes and their command-line report.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{self, Config};
use crate::model::{FileVerdict, Report};
use crate::Result;

/// The history is compacted to its newest complete lines after it crosses 1 MiB.
/// Conflict transitions are rare, but an unattended daemon still needs a hard
/// disk bound rather than relying on rarity forever.
pub const MAX_HISTORY_BYTES: u64 = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EpisodeRecord {
    pub repo_key: String,
    pub path: String,
    pub left_workspace_id: String,
    pub right_workspace_id: String,
    // Stable ids above define identity so a rename cannot split an episode.
    // These display labels are also retained because a human reading an old
    // record needs to know which worktrees and branches the ids meant then.
    pub left_workspace_label: String,
    pub right_workspace_label: String,
    pub left_branch: Option<String>,
    pub right_branch: Option<String>,
    pub first_seen_unix_seconds: u64,
    /// Present only on the closing transition of an episode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_unix_seconds: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct EpisodeKey {
    repo_key: String,
    path: String,
    left_workspace_id: String,
    right_workspace_id: String,
}

#[derive(Debug, Clone)]
struct ActiveEpisode {
    start: EpisodeRecord,
    last_seen_unix_seconds: u64,
}

#[derive(Debug, Default)]
pub struct EpisodeTracker {
    active: BTreeMap<EpisodeKey, ActiveEpisode>,
}

struct EpisodePlan {
    current: BTreeMap<EpisodeKey, ActiveEpisode>,
    records: Vec<EpisodeRecord>,
}

impl EpisodeRecord {
    fn key(&self) -> EpisodeKey {
        EpisodeKey {
            repo_key: self.repo_key.clone(),
            path: self.path.clone(),
            left_workspace_id: self.left_workspace_id.clone(),
            right_workspace_id: self.right_workspace_id.clone(),
        }
    }
}

impl EpisodeTracker {
    /// Reconstructs live episodes from persisted start and closing transitions.
    pub fn from_records(records: &[EpisodeRecord]) -> Self {
        let mut active = BTreeMap::new();
        for record in records {
            let key = record.key();
            if record.last_seen_unix_seconds.is_none() {
                active.insert(
                    key,
                    ActiveEpisode {
                        start: record.clone(),
                        last_seen_unix_seconds: record.first_seen_unix_seconds,
                    },
                );
            } else if active.get(&key).is_some_and(|episode| {
                episode.start.first_seen_unix_seconds == record.first_seen_unix_seconds
            }) {
                active.remove(&key);
            }
        }
        Self { active }
    }

    fn plan(&self, report: &Report, seen_at: u64) -> EpisodePlan {
        let checkouts = report
            .checkouts
            .iter()
            .map(|checkout| (checkout.workspace_id.as_str(), checkout))
            .collect::<BTreeMap<_, _>>();
        let mut current = BTreeMap::new();
        let mut records = Vec::new();

        for pairing in &report.pairings {
            let (Some(left), Some(right)) = (
                checkouts.get(pairing.left_workspace_id.as_str()),
                checkouts.get(pairing.right_workspace_id.as_str()),
            ) else {
                continue;
            };
            if left.repo_key != right.repo_key {
                continue;
            }

            let (left, right) = if left.workspace_id <= right.workspace_id {
                (*left, *right)
            } else {
                (*right, *left)
            };
            for shared in &pairing.shared {
                let key = EpisodeKey {
                    repo_key: left.repo_key.0.clone(),
                    path: shared.path.clone(),
                    left_workspace_id: left.workspace_id.clone(),
                    right_workspace_id: right.workspace_id.clone(),
                };

                match shared.verdict {
                    FileVerdict::Conflict => {
                        if current.contains_key(&key) {
                            continue;
                        }
                        if let Some(active) = self.active.get(&key) {
                            let mut continuing = active.clone();
                            continuing.last_seen_unix_seconds = seen_at;
                            current.insert(key, continuing);
                            continue;
                        }
                        let start = EpisodeRecord {
                            repo_key: key.repo_key.clone(),
                            path: key.path.clone(),
                            left_workspace_id: key.left_workspace_id.clone(),
                            right_workspace_id: key.right_workspace_id.clone(),
                            left_workspace_label: left.workspace_label.clone(),
                            right_workspace_label: right.workspace_label.clone(),
                            left_branch: left.branch.clone(),
                            right_branch: right.branch.clone(),
                            first_seen_unix_seconds: seen_at,
                            last_seen_unix_seconds: None,
                        };
                        records.push(start.clone());
                        current.insert(
                            key,
                            ActiveEpisode {
                                start,
                                last_seen_unix_seconds: seen_at,
                            },
                        );
                    }
                    FileVerdict::Unknown => {
                        // Unknown means prediction produced no answer: it must
                        // neither manufacture a conflict nor resolve one whose
                        // prediction is merely unavailable this cycle.
                        if let Some(active) = self.active.get(&key) {
                            current.insert(key, active.clone());
                        }
                    }
                    FileVerdict::Overlap => {}
                }
            }
        }

        for (key, active) in &self.active {
            if current.contains_key(key) {
                continue;
            }
            let mut closing = active.start.clone();
            closing.last_seen_unix_seconds = Some(active.last_seen_unix_seconds);
            records.push(closing);
        }

        EpisodePlan { current, records }
    }

    fn commit(&mut self, plan: EpisodePlan) {
        self.active = plan.current;
    }
}

/// Records newly conflicting `(repo, path, workspace pair)` episodes. With the
/// opt-in disabled this returns before opening or creating any state file.
pub fn record_cycle(config: &Config, tracker: &mut EpisodeTracker, report: &Report) -> Result<()> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    record_cycle_at(config, tracker, report, now)
}

/// Timestamp-injected form used to test transition boundaries deterministically.
pub fn record_cycle_at(
    config: &Config,
    tracker: &mut EpisodeTracker,
    report: &Report,
    seen_at: u64,
) -> Result<()> {
    if !config.conflict_history {
        return Ok(());
    }

    let plan = tracker.plan(report, seen_at);
    if !plan.records.is_empty() {
        // Commit only after the append succeeds. A transient state-dir failure
        // must be retried next cycle rather than silently consuming the edge.
        append_records(&plan.records)?;
    }
    tracker.commit(plan);
    Ok(())
}

pub fn append_records(records: &[EpisodeRecord]) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }

    // Reserve byte zero for a separator if the existing file has a killed
    // partial tail. Serializing and validating every record before opening the
    // file prevents a later bad record from leaving an earlier one durable.
    let mut batch = vec![b'\n'];
    for record in records {
        let line = serde_json::to_vec(record)?;
        if line.len() as u64 + 1 > MAX_HISTORY_BYTES {
            return Err(format!(
                "one conflict-history record is larger than the {} byte cap",
                MAX_HISTORY_BYTES
            )
            .into());
        }
        batch.extend_from_slice(&line);
        batch.push(b'\n');
    }

    let path = config::history_file();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .mode(0o600)
        // A state-file symlink must not turn an append into a repository write.
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    if !file.metadata()?.is_file() {
        return Err(format!("history path {} is not a regular file", path.display()).into());
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;

    let len = file.metadata()?.len();
    let needs_separator = if len == 0 {
        false
    } else {
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0_u8; 1];
        file.read_exact(&mut last)?;
        last[0] != b'\n'
    };
    let start = usize::from(!needs_separator);
    file.write_all(&batch[start..])?;
    file.flush()?;
    trim_if_owned(&mut file, &path, MAX_HISTORY_BYTES)?;
    file.sync_data()?;
    Ok(())
}

/// The destructive cap is allowed only for the regular file opened by the
/// plugin and still named by its configured history path. Public so tests can
/// exercise device/inode identity against real files, as the daemon log does.
pub fn should_trim_history(
    opened: &fs::Metadata,
    owned_path: Option<&fs::Metadata>,
    max: u64,
) -> bool {
    if !opened.is_file() {
        return false;
    }
    let Some(owned_path) = owned_path else {
        return false;
    };
    owned_path.is_file()
        && opened.dev() == owned_path.dev()
        && opened.ino() == owned_path.ino()
        && opened.len() > max
}

/// Keeps the newest complete JSON lines if `file` is still the file at
/// `owned_path`. A mismatched descriptor is left byte-for-byte untouched.
pub fn trim_if_owned(file: &mut File, owned_path: &Path, max: u64) -> Result<bool> {
    let opened = file.metadata()?;
    let owned = fs::metadata(owned_path).ok();
    if !should_trim_history(&opened, owned.as_ref(), max) {
        return Ok(false);
    }

    let keep = max.min(i64::MAX as u64) as usize;
    let start = opened.len().saturating_sub(keep as u64);
    file.seek(SeekFrom::Start(start))?;
    let mut tail = Vec::with_capacity(keep);
    file.take(keep as u64).read_to_end(&mut tail)?;
    if start > 0 {
        if let Some(end) = tail.iter().position(|byte| *byte == b'\n') {
            tail.drain(..=end);
        } else {
            tail.clear();
        }
    }

    // Re-check immediately before set_len: path replacement between the read
    // and the destructive operation must turn the trim into a no-op.
    let opened = file.metadata()?;
    let owned = fs::metadata(owned_path).ok();
    if !should_trim_history(&opened, owned.as_ref(), max) {
        return Ok(false);
    }
    file.set_len(0)?;
    file.write_all(&tail)?;
    file.flush()?;
    Ok(true)
}

/// Reads every intact JSON line. One malformed or killed-write tail is warned
/// about and skipped without hiding valid records before or after it.
pub fn load_records() -> Result<Vec<EpisodeRecord>> {
    let path = config::history_file();
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
    {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let mut records = Vec::new();
    let mut reader = BufReader::new(file);
    let mut raw = Vec::new();
    let mut line_number = 0_u64;
    loop {
        raw.clear();
        if reader.read_until(b'\n', &mut raw)? == 0 {
            break;
        }
        line_number += 1;
        if raw.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice(&raw) {
            Ok(record) => records.push(record),
            Err(err) => eprintln!(
                "collide: ignoring malformed history line {line_number} in {}: {err}",
                path.display()
            ),
        }
    }
    Ok(records)
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct EpisodeIdentity {
    key: EpisodeKey,
    first_seen_unix_seconds: u64,
}

#[derive(Debug)]
struct Summary {
    episodes: usize,
    latest: EpisodeRecord,
}

fn sighting_time(record: &EpisodeRecord) -> u64 {
    record
        .last_seen_unix_seconds
        .unwrap_or(record.first_seen_unix_seconds)
}

pub fn render_records(records: &[EpisodeRecord]) -> String {
    // Start and closing transitions both contain the full episode identity so
    // compaction may retain either one without making the remainder unreadable.
    let mut episodes = BTreeMap::new();
    for record in records {
        let identity = EpisodeIdentity {
            key: record.key(),
            first_seen_unix_seconds: record.first_seen_unix_seconds,
        };
        let episode = episodes.entry(identity).or_insert_with(|| record.clone());
        if record.last_seen_unix_seconds.is_some() {
            *episode = record.clone();
        }
    }

    let mut grouped: BTreeMap<EpisodeKey, Summary> = BTreeMap::new();
    for (identity, record) in episodes {
        let summary = grouped.entry(identity.key).or_insert_with(|| Summary {
            episodes: 0,
            latest: record.clone(),
        });
        summary.episodes += 1;
        if sighting_time(&record) >= sighting_time(&summary.latest) {
            summary.latest = record;
        }
    }

    let mut summaries = grouped.into_values().collect::<Vec<_>>();
    summaries.sort_by(|left, right| {
        right
            .episodes
            .cmp(&left.episodes)
            .then_with(|| sighting_time(&right.latest).cmp(&sighting_time(&left.latest)))
            .then_with(|| left.latest.path.cmp(&right.latest.path))
            .then_with(|| {
                left.latest
                    .left_workspace_id
                    .cmp(&right.latest.left_workspace_id)
            })
    });

    if summaries.is_empty() {
        return "No conflict history recorded.\n".to_string();
    }

    let mut output = String::from("Conflict history (most episodes first):\n");
    for summary in summaries {
        let record = summary.latest;
        let noun = if summary.episodes == 1 {
            "episode"
        } else {
            "episodes"
        };
        let last_seen = match record.last_seen_unix_seconds {
            Some(timestamp) => format!("last seen {timestamp} (Unix seconds)"),
            None => "episode still open".to_string(),
        };
        output.push_str(&format!(
            "{} {noun} | {} :: {} | {} <-> {} | {last_seen}\n",
            summary.episodes,
            record.repo_key,
            record.path,
            display_worktree(
                &record.left_workspace_label,
                &record.left_workspace_id,
                record.left_branch.as_deref()
            ),
            display_worktree(
                &record.right_workspace_label,
                &record.right_workspace_id,
                record.right_branch.as_deref()
            )
        ));
    }
    output
}

fn display_worktree(label: &str, workspace_id: &str, branch: Option<&str>) -> String {
    match branch {
        Some(branch) => format!("{label} [{branch}; {workspace_id}]"),
        None => format!("{label} [detached; {workspace_id}]"),
    }
}

pub fn run_history() -> Result<()> {
    print!("{}", render_records(&load_records()?));
    Ok(())
}

pub fn clear() -> Result<()> {
    match fs::remove_file(config::history_file()) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

pub fn run_clear() -> Result<()> {
    clear()?;
    println!("Conflict history cleared.");
    Ok(())
}
