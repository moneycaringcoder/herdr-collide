//! One-click sidebar setup.
//!
//! herdr renders a plugin's custom tokens only if the user's `config.toml`
//! names them, so without this the badge silently never appears. Rather than
//! asking people to hand-merge TOML, `--setup` splices the four token entries
//! into their existing `[ui.sidebar.spaces]` rows, reloads herdr's config, and
//! restores the backup automatically if that reload fails.
//!
//! Safety rules this module holds to, because it edits a file it does not own:
//!
//!   * every run takes a timestamp-free, non-clobbering backup first;
//!   * the edit is line-oriented and additive — nothing is ever deleted;
//!   * a failed reload restores the backup byte for byte;
//!   * running it twice is a no-op rather than a duplicate insert.

use std::path::{Path, PathBuf};

use crate::config::non_empty_env;
use crate::model::Severity;
use crate::Result;

const SECTION: &str = "[ui.sidebar.spaces]";
const BACKUP_SUFFIX: &str = ".collide-backup";

/// Rows written into the user's config: amber for overlap, orange for a runaway
/// change set, red for a predicted conflict. Colours chosen to read on both
/// light and dark themes.
///
/// `collide_clean` is deliberately absent. A clean workspace renders an empty
/// badge, which the daemon treats as "clear the token" rather than "write an
/// empty one", so that token is never set and a row naming it would be three
/// lines of config that can never display anything. The sweep in `daemon` still
/// clears all four names defensively, which costs nothing and cannot go stale.
const TOKEN_COLOURS: [(&str, &str); 3] = [
    ("collide_overlap", "#FFC799"),
    ("collide_runaway", "#FFB27F"),
    ("collide_conflict", "#FF8080"),
];

pub fn config_path() -> PathBuf {
    if let Some(explicit) = non_empty_env("HERDR_CONFIG_PATH") {
        return PathBuf::from(explicit);
    }
    herdr_dir().join("config.toml")
}

fn herdr_dir() -> PathBuf {
    if let Some(xdg) = non_empty_env("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("herdr");
    }
    match non_empty_env("HOME") {
        Some(home) => PathBuf::from(home).join(".config").join("herdr"),
        None => PathBuf::from(".config/herdr"),
    }
}

fn backup_path(config: &Path) -> PathBuf {
    let mut name = config.as_os_str().to_os_string();
    name.push(BACKUP_SUFFIX);
    PathBuf::from(name)
}

/// The rows this plugin contributes, rendered as TOML lines at the indentation
/// herdr's own examples use.
fn token_lines() -> Vec<String> {
    TOKEN_COLOURS
        .iter()
        .map(|(token, colour)| format!("    {{ token = \"${token}\", fg = \"{colour}\" }},"))
        .collect()
}

fn already_configured(text: &str) -> bool {
    Severity::ALL_TOKENS
        .iter()
        .any(|token| text.contains(&format!("\"${token}\"")))
}

/// Splices the token entries into an existing `[ui.sidebar.spaces]` rows array,
/// or appends a complete section when the user has none.
///
/// Returns `None` when the file already mentions our tokens, so a second run is
/// a no-op rather than a duplicate insert.
pub fn plan_edit(text: &str) -> Option<String> {
    if already_configured(text) {
        return None;
    }

    let lines: Vec<&str> = text.lines().collect();
    let section_start = lines
        .iter()
        .position(|line| line.trim_start().starts_with(SECTION));

    let Some(section_start) = section_start else {
        return Some(append_section(text));
    };

    // Find this section's rows array, stopping at the next section header so we
    // can never reach into a neighbouring table.
    let mut insert_at = None;
    let mut depth = 0usize;
    let mut in_rows = false;
    for (offset, line) in lines.iter().enumerate().skip(section_start + 1) {
        let trimmed = line.trim_start();
        if !in_rows && trimmed.starts_with('[') && !trimmed.starts_with("[[") {
            break; // next table; this section has no rows array
        }
        if !in_rows && trimmed.starts_with("rows") && trimmed.contains('[') {
            in_rows = true;
        }
        if in_rows {
            depth += line.matches('[').count();
            depth = depth.saturating_sub(line.matches(']').count());
            if depth == 0 {
                insert_at = Some(offset); // the array's closing bracket
                break;
            }
        }
    }

    let insert_at = insert_at?;
    let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    for (n, line) in token_lines().into_iter().enumerate() {
        out.insert(insert_at + n, line);
    }
    Some(finish(out, text))
}

fn append_section(text: &str) -> String {
    let mut out = text.trim_end().to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(SECTION);
    out.push_str("\nrows = [\n  [\"state_icon\", \"workspace\"],\n  [\"branch\",\n");
    for line in token_lines() {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str("  ],\n]\n");
    out
}

fn finish(lines: Vec<String>, original: &str) -> String {
    let mut out = lines.join("\n");
    if original.ends_with('\n') {
        out.push('\n');
    }
    out
}

pub fn run_setup() -> Result<()> {
    let config = config_path();
    let text = std::fs::read_to_string(&config)
        .map_err(|e| format!("cannot read {}: {e}", config.display()))?;

    let Some(updated) = plan_edit(&text) else {
        println!("collide: sidebar tokens are already configured; nothing to do.");
        return Ok(());
    };

    let backup = backup_path(&config);
    if backup.exists() {
        return Err(format!(
            "refusing to overwrite an existing backup at {}; move it aside first",
            backup.display()
        )
        .into());
    }
    std::fs::write(&backup, &text)?;
    std::fs::write(&config, &updated)?;

    match reload_herdr_config() {
        Ok(()) => {
            println!(
                "collide: added sidebar tokens to {} (backup at {}).",
                config.display(),
                backup.display()
            );
            println!("collide: run `collide --setup-rollback` to undo.");
            Ok(())
        }
        Err(err) => {
            // The edit is the only thing that changed, so restoring it is a
            // complete undo. Report the original failure, not the restore.
            std::fs::write(&config, &text)?;
            let _ = std::fs::remove_file(&backup);
            Err(
                format!("herdr rejected the updated config, so it was restored unchanged: {err}")
                    .into(),
            )
        }
    }
}

pub fn run_rollback() -> Result<()> {
    let config = config_path();
    let backup = backup_path(&config);
    if !backup.exists() {
        return Err(format!("no backup found at {}", backup.display()).into());
    }
    let text = std::fs::read_to_string(&backup)?;
    std::fs::write(&config, text)?;
    std::fs::remove_file(&backup)?;
    let _ = reload_herdr_config();
    println!("collide: restored {} from backup.", config.display());
    Ok(())
}

/// Sidebar rows reload live, so the user never has to restart herdr.
fn reload_herdr_config() -> Result<()> {
    let bin = non_empty_env("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".to_string());
    let output = std::process::Command::new(bin)
        .args(["server", "reload-config"])
        .output()?;
    if output.status.success() {
        return Ok(());
    }
    Err(String::from_utf8_lossy(&output.stderr)
        .trim()
        .to_string()
        .into())
}
