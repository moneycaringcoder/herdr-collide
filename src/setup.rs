//! One-click sidebar setup.
//!
//! herdr renders a plugin's custom tokens only if the user's `config.toml`
//! names them, so without this the badge silently never appears. Rather than
//! asking people to hand-merge TOML, `--setup` splices the token entries into
//! their existing `[ui.sidebar.spaces]` rows, reloads herdr's config over the
//! socket, and restores the backup automatically if that reload fails.
//!
//! Safety rules this module holds to, because it edits a file it does not own:
//!
//!   * every run takes a timestamp-free, non-clobbering backup first;
//!   * the edit is line-oriented and additive — nothing is ever deleted;
//!   * the file's own line endings survive the edit;
//!   * a failed reload restores the backup byte for byte;
//!   * running it twice adds nothing, and running it after a new token was
//!     introduced adds exactly that token, in the row its siblings are in;
//!   * nothing outside `[ui.sidebar.spaces]` is ever read as configuration, and
//!     nothing outside it is ever written to;
//!   * when the file is a shape this splice cannot handle, it says so and fails
//!     rather than reporting that there was nothing to do.
//!
//! That last rule is the one this module got wrong for longest, and the cost was
//! the highest: the walk for the `rows` array stopped at `[table]` but not at
//! `[[array.of.tables]]`, so an empty `[ui.sidebar.spaces]` sent it into the
//! next `[[keys.command]]` block to splice four token tables into a keybinding —
//! a file that parses, reloads and renders nothing, reported as a success.

use std::path::{Path, PathBuf};

use crate::config::{self, non_empty_env};
use crate::herdr::Herdr;
use crate::Result;

const SECTION: &str = "[ui.sidebar.spaces]";
const BACKUP_SUFFIX: &str = ".collide-backup";

/// Rows written into the user's config: amber for overlap, orange for a runaway
/// change set, grey for a verdict we could not establish, red for a predicted
/// conflict. Colours chosen to read on both light and dark themes.
///
/// `collide_clean` is deliberately absent. A clean workspace renders an empty
/// badge, which the daemon treats as "clear the token" rather than "write an
/// empty one", so that token is never set and a row naming it would be three
/// lines of config that can never display anything. The sweep in `daemon` still
/// clears every name defensively, which costs nothing and cannot go stale.
const TOKEN_COLOURS: [(&str, &str); 4] = [
    ("collide_overlap", "#FFC799"),
    ("collide_runaway", "#FFB27F"),
    // Grey rather than a warning colour: an unknown verdict is an absence of
    // information, not a severity, and colouring it like a conflict would
    // overstate it exactly as badly as the overlap badge used to understate it.
    ("collide_unknown", "#9399B2"),
    ("collide_conflict", "#FF8080"),
];

pub fn config_path() -> PathBuf {
    if let Some(explicit) = non_empty_env("HERDR_CONFIG_PATH") {
        return PathBuf::from(explicit);
    }
    herdr_dir().join("config.toml")
}

/// herdr's own config directory, resolved through the same helper the plugin's
/// state and config directories use.
///
/// It used to have its own rules: it honoured a *relative* `XDG_CONFIG_HOME`,
/// which the spec says to ignore, and with no `HOME` at all it returned the
/// relative path `.config/herdr` — which resolves against the process cwd, and
/// a plugin command's cwd is the plugin root. `--setup` would then have edited
/// a `config.toml` inside the plugin's own directory and reported success.
fn herdr_dir() -> PathBuf {
    config::xdg_dir("XDG_CONFIG_HOME", ".config").join("herdr")
}

fn backup_path(config: &Path) -> PathBuf {
    let mut name = config.as_os_str().to_os_string();
    name.push(BACKUP_SUFFIX);
    PathBuf::from(name)
}

/// The rows this plugin contributes, rendered as TOML lines at the indentation
/// herdr's own examples use.
fn token_lines(tokens: &[(&str, &str)]) -> Vec<String> {
    tokens
        .iter()
        .map(|(token, colour)| format!("    {{ token = \"${token}\", fg = \"{colour}\" }},"))
        .collect()
}

/// The token rows that are not in the section this splice writes into.
///
/// Per token, not all-or-nothing. The all-or-nothing version made `--setup` a
/// no-op for every user who had installed before a new token existed: their
/// file already named `$collide_overlap`, so the whole splice was skipped and
/// the new severity could never render.
///
/// Scoped to `[ui.sidebar.spaces]`, not to the whole file, and the difference is
/// not academic: a token named in some *other* section — an experiment in
/// `[ui.sidebar.agents]`, a half-finished move between sidebars — used to count
/// as configured, so the splice omitted it from the section it was actually
/// building. The token then rendered nowhere while `--setup` reported success
/// and a second run answered "already configured". When there is no section at
/// all the scope is empty and every token is missing, which is right: the
/// section about to be appended contains none of them yet.
///
/// A token named only inside a comment does not count as configured either —
/// herdr cannot render a commented-out row.
fn missing_tokens(scope: &[&str]) -> Vec<(&'static str, &'static str)> {
    TOKEN_COLOURS
        .iter()
        .filter(|(token, _)| !mentions(scope, token))
        .copied()
        .collect()
}

fn mentions(scope: &[&str], token: &str) -> bool {
    let needle = format!("\"${token}\"");
    scope.iter().any(|line| code_of(line).contains(&needle))
}

/// The lines of the `[ui.sidebar.spaces]` table: its header, and everything up
/// to the next table header.
fn section_lines<'a>(lines: &[&'a str], section_start: usize) -> Vec<&'a str> {
    let end = lines
        .iter()
        .enumerate()
        .skip(section_start + 1)
        .find(|(_, line)| is_table_header(line))
        .map_or(lines.len(), |(offset, _)| offset);
    lines[section_start..end].to_vec()
}

/// Whether a line opens a new TOML table.
///
/// `[[x]]` is an array-of-tables header and is every bit as much a new table as
/// `[x]`; treating it as anything else is what let the row walk wander out of
/// the section it was given and splice into a keybinding table.
///
/// A row inside a `rows` array also begins with `[`, so the bracket alone is not
/// enough: a header's name is a bare key, which starts with a letter or an
/// underscore, while a row starts with a quoted string. A *quoted* table header
/// (`["my table"]`) would read as a row here — legal TOML, but not a shape
/// herdr's own config uses, and the independent parser in `tests/setup_edit.rs`
/// draws the line in the same place.
fn is_table_header(line: &str) -> bool {
    let trimmed = code_of(line).trim_start();
    trimmed.starts_with('[')
        && trimmed
            .trim_start_matches('[')
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// What `plan_edit` decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditPlan {
    /// Every token this plugin contributes is already in the file.
    AlreadyConfigured,
    /// An edit to apply.
    Edit(Splice),
    /// The file is a shape this splice cannot handle. Carries a reason the user
    /// can act on, because "I did nothing" and "everything is already set up"
    /// must never read the same.
    CouldNotPlace(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Splice {
    pub text: String,
    /// Token names this edit adds, in the order they appear in the file.
    pub added: Vec<&'static str>,
}

/// Splices the missing token entries into an existing `[ui.sidebar.spaces]`
/// rows array, or appends a complete section when the user has none.
pub fn plan_edit(text: &str) -> EditPlan {
    let lines: Vec<&str> = text.lines().collect();
    let section_start = lines
        .iter()
        .position(|line| code_of(line).trim_start().starts_with(SECTION));

    // Only the target section decides what is already configured. With no
    // section there is nothing configured yet, whatever the rest of the file
    // says.
    let scope = match section_start {
        Some(start) => section_lines(&lines, start),
        None => Vec::new(),
    };
    let missing = missing_tokens(&scope);
    if missing.is_empty() {
        return EditPlan::AlreadyConfigured;
    }
    let added: Vec<&'static str> = missing.iter().map(|(token, _)| *token).collect();

    let Some(section_start) = section_start else {
        return EditPlan::Edit(Splice {
            text: append_section(text, &missing),
            added,
        });
    };

    let rows = match find_rows(&lines, section_start) {
        Ok(rows) => rows,
        Err(reason) => return EditPlan::CouldNotPlace(reason),
    };
    // The row that already holds one of our tokens, so an upgrade keeps
    // collide's badges together instead of scattering the new one into whatever
    // row happens to be last. Falling back to the last row is the fresh-install
    // case, where none of them is anywhere yet.
    let Some(span) = rows
        .iter()
        .find(|span| row_mentions_ours(&lines, span))
        .or_else(|| rows.last())
    else {
        return EditPlan::CouldNotPlace(format!(
            "found {SECTION} but no row inside a `rows = [ ... ]` array to add to"
        ));
    };

    let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    if span.start_line == span.end_line {
        // A single-line row: splice the entries in at the bracket that closed
        // *this row*, which is not necessarily the last bracket on the line —
        // on `rows = [["a"], ["b"]]` the last one closes the whole array, and
        // inserting there would drop the tables in beside the rows instead of
        // inside one. That is valid TOML which herdr accepts and then renders
        // nothing for.
        let line = out[span.end_line].clone();
        let (head, tail) = line.split_at(span.end_column);
        let head = head.trim_end();
        let separator = if head.ends_with('[') { "" } else { "," };
        let entries: Vec<String> = token_lines(&missing)
            .into_iter()
            .map(|l| l.trim().trim_end_matches(',').to_string())
            .collect();
        out[span.end_line] = format!("{head}{separator} {}{tail}", entries.join(", "));
    } else {
        // A multi-line row: insert before its final line, which carries the
        // closing bracket. The preceding line already ends in a comma.
        for (n, line) in token_lines(&missing).into_iter().enumerate() {
            out.insert(span.end_line + n, line);
        }
    }
    EditPlan::Edit(Splice {
        text: finish(out, text),
        added,
    })
}

/// Where one row of the section's `rows` array lives.
struct RowSpan {
    start_line: usize,
    /// Byte index, within `start_line`, of the `[` that opened the row.
    start_column: usize,
    end_line: usize,
    /// Byte index, within `end_line`, of the `]` that closed the row.
    end_column: usize,
}

/// Whether a row already names one of this plugin's tokens.
fn row_mentions_ours(lines: &[&str], span: &RowSpan) -> bool {
    let text = if span.start_line == span.end_line {
        let code = code_of(lines[span.start_line]);
        code.get(span.start_column..=span.end_column)
            .unwrap_or(code)
            .to_string()
    } else {
        lines[span.start_line..=span.end_line]
            .iter()
            .map(|line| code_of(line))
            .collect::<Vec<_>>()
            .join("\n")
    };
    TOKEN_COLOURS
        .iter()
        .any(|(token, _)| text.contains(&format!("\"${token}\"")))
}

/// Walks the `rows` array of the section starting at `section_start` and
/// returns every row in it, in order.
///
/// Depth 1 is inside `rows`, depth 2 is inside a row. Brackets inside comments
/// and inside strings do not count — a commented-out row used to be picked as
/// the last row, and the entries were then appended *inside the comment*, which
/// leaves a file that parses, reloads and renders nothing at all.
///
/// The walk stops at the next table header of **any** kind. Exempting `[[x]]`
/// was a real bug and a silent one: given a `[ui.sidebar.spaces]` with no rows
/// of its own, the walk ran on into the following `[[keys.command]]` block,
/// found *its* `rows` key, and spliced this plugin's token tables into a
/// keybinding table. The file still parsed, herdr still reloaded it (it does not
/// validate token names), the sidebar still rendered nothing — and `--setup`
/// reported that it had added four rows. An array-of-tables header is a new
/// table exactly as a plain one is.
fn find_rows(lines: &[&str], section_start: usize) -> std::result::Result<Vec<RowSpan>, String> {
    let mut depth = 0usize;
    let mut in_rows = false;
    let mut row_start: Option<(usize, usize)> = None;
    let mut rows: Vec<RowSpan> = Vec::new();

    for (offset, line) in lines.iter().enumerate().skip(section_start + 1) {
        let code = code_of(line);
        let mut from = 0usize;
        if !in_rows {
            if is_table_header(line) {
                break; // next table; this section has no rows array
            }
            let Some(open) = rows_open(code)? else {
                continue;
            };
            in_rows = true;
            from = open;
        }

        for event in brackets(&code[from..])? {
            let column = from + event.column;
            match event.bracket {
                Bracket::Open => {
                    depth += 1;
                    if depth == 2 && row_start.is_none() {
                        row_start = Some((offset, column));
                    }
                }
                Bracket::Close => {
                    depth = depth.saturating_sub(1);
                    if depth == 1 {
                        if let Some((start_line, start_column)) = row_start.take() {
                            rows.push(RowSpan {
                                start_line,
                                start_column,
                                end_line: offset,
                                end_column: column,
                            });
                        }
                    }
                }
            }
            if depth == 0 {
                break;
            }
        }
        if in_rows && depth == 0 {
            break; // rows array closed
        }
    }

    Ok(rows)
}

/// The byte index of the `[` that opens a `rows = [` on this line, if it does.
///
/// The key has to be `rows` exactly: `rows_per_page = [..]` is somebody else's
/// setting, and splicing into it would be silent nonsense.
fn rows_open(code: &str) -> std::result::Result<Option<usize>, String> {
    let trimmed = code.trim_start();
    let Some(rest) = trimmed.strip_prefix("rows") else {
        return Ok(None);
    };
    if !rest.trim_start().starts_with('=') {
        return Ok(None);
    }
    Ok(brackets(code)?
        .into_iter()
        .find(|event| event.bracket == Bracket::Open)
        .map(|event| event.column))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bracket {
    Open,
    Close,
}

#[derive(Debug, Clone, Copy)]
struct BracketEvent {
    column: usize,
    bracket: Bracket,
}

/// Every `[` and `]` on one line that is really a bracket: not inside a string,
/// and not after a `#`.
fn brackets(code: &str) -> std::result::Result<Vec<BracketEvent>, String> {
    let mut events = Vec::new();
    let bytes = code.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'"' | b'\'' => {
                let quote = bytes[i];
                if bytes[i..].starts_with(&[quote, quote, quote]) {
                    // A multi-line string carries bracket-looking content
                    // across lines, and guessing is how a splice ends up
                    // somewhere absurd.
                    return Err(
                        "the rows array contains a multi-line string, which this splice \
                         does not handle"
                            .to_string(),
                    );
                }
                i += 1;
                while i < bytes.len() {
                    if quote == b'"' && bytes[i] == b'\\' {
                        i += 2; // escape; literal strings have none
                        continue;
                    }
                    if bytes[i] == quote {
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'#' => break, // comment to end of line
            b'[' => {
                events.push(BracketEvent {
                    column: i,
                    bracket: Bracket::Open,
                });
                i += 1;
            }
            b']' => {
                events.push(BracketEvent {
                    column: i,
                    bracket: Bracket::Close,
                });
                i += 1;
            }
            _ => i += 1,
        }
    }
    Ok(events)
}

/// The part of a line that is TOML rather than a comment. A `#` inside a string
/// is not a comment marker — hex colours are written `"#FFC799"`, so getting
/// this wrong would truncate every row this plugin writes.
fn code_of(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'"' | b'\'' => {
                let quote = bytes[i];
                i += 1;
                while i < bytes.len() {
                    if quote == b'"' && bytes[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == quote {
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'#' => return &line[..i],
            _ => i += 1,
        }
    }
    line
}

fn append_section(text: &str, missing: &[(&str, &str)]) -> String {
    let newline = newline_of(text);
    let mut out = text.trim_end().to_string();
    if !out.is_empty() {
        out.push_str(newline);
        out.push_str(newline);
    }
    out.push_str(SECTION);
    out.push_str(newline);
    out.push_str("rows = [");
    out.push_str(newline);
    out.push_str("  [\"state_icon\", \"workspace\"],");
    out.push_str(newline);
    out.push_str("  [\"branch\",");
    out.push_str(newline);
    for line in token_lines(missing) {
        out.push_str(&line);
        out.push_str(newline);
    }
    out.push_str("  ],");
    out.push_str(newline);
    out.push(']');
    out.push_str(newline);
    out
}

/// The line ending the file mostly uses. Rejoining a CRLF file with bare `\n`
/// rewrites every line in it, which is a much larger diff than the user asked
/// for. A file with mixed endings comes out uniform, using whichever it had
/// more of.
fn newline_of(text: &str) -> &'static str {
    let crlf = text.matches("\r\n").count();
    let lf_only = text.matches('\n').count().saturating_sub(crlf);
    if crlf > lf_only {
        "\r\n"
    } else {
        "\n"
    }
}

fn finish(lines: Vec<String>, original: &str) -> String {
    let newline = newline_of(original);
    let mut out = lines.join(newline);
    if original.ends_with('\n') {
        out.push_str(newline);
    }
    out
}

/// The snippet the README ships, for the message we print when we cannot place
/// the rows ourselves.
fn manual_snippet() -> String {
    let mut out = String::from(
        "[ui.sidebar.spaces]\nrows = [\n  [\"state_icon\", \"workspace\"],\n  [\"branch\",\n",
    );
    for line in token_lines(&TOKEN_COLOURS) {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str("  ],\n]\n");
    out
}

fn name_list(tokens: &[&'static str]) -> String {
    tokens
        .iter()
        .map(|token| format!("${token}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The tokens `--setup` writes that `text` does not name in its
/// `[ui.sidebar.spaces]` rows.
///
/// Pure, and the same computation `plan_edit` uses to decide what to splice, so
/// the two can never disagree about what "configured" means.
pub fn unconfigured_tokens(text: &str) -> Vec<&'static str> {
    let lines: Vec<&str> = text.lines().collect();
    let scope = match lines
        .iter()
        .position(|line| code_of(line).trim_start().starts_with(SECTION))
    {
        Some(start) => section_lines(&lines, start),
        None => Vec::new(),
    };
    missing_tokens(&scope)
        .into_iter()
        .map(|(token, _)| token)
        .collect()
}

/// A note for the daemon when herdr's sidebar cannot render a badge this plugin
/// is about to compute.
///
/// The case that forced this: an installation that ran `--setup` before
/// `collide_unknown` existed names the other three, so a workspace whose verdict
/// is now `Unknown` clears its old token — correctly — and sets one herdr has
/// never been told to render. The cell goes blank, which reads as clean. A
/// severity added to stop things disappearing silently would itself disappear
/// silently, and nothing anywhere would say why.
///
/// Compared against the tokens `--setup` writes rather than
/// `Severity::ALL_TOKENS`, deliberately: `collide_clean` is in `ALL_TOKENS`
/// because the sweep clears it defensively, but it is never *set* — a clean
/// workspace clears its token instead — so a row naming it could never render
/// anything and its absence is not a problem to report. Reporting it would make
/// this note fire on every correctly configured installation, which is the
/// fastest way to teach someone to ignore it.
///
/// Read-only, and never fatal: a missing or unreadable `config.toml` is
/// something to say out loud, not something to stop the refresh loop for.
pub fn sidebar_token_note() -> Option<String> {
    let path = config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Some(format!(
                "herdr has no {}, so no collide badge can render; \
                 run the `{ACTION_TITLE}` action",
                path.display()
            ))
        }
        Err(err) => {
            return Some(format!(
                "could not read {} to check herdr's sidebar rows, so it is unknown whether \
                 collide's badges can render at all: {err}",
                path.display()
            ))
        }
    };
    let missing = unconfigured_tokens(&text);
    if missing.is_empty() {
        return None;
    }
    Some(format!(
        "herdr's sidebar does not name {}, so {} that severity renders as an empty cell \
         rather than a badge; run the `{ACTION_TITLE}` action to add {} to {}",
        name_list(&missing),
        if missing.len() == 1 {
            "a workspace at"
        } else {
            "a workspace at any of"
        },
        if missing.len() == 1 { "it" } else { "them" },
        path.display()
    ))
}

/// The action a user has to run, spelled as herdr's own menu spells it.
const ACTION_TITLE: &str = "Collide: set up sidebar (start here)";

pub fn run_setup() -> Result<()> {
    let config = config_path();
    let text = std::fs::read_to_string(&config)
        .map_err(|e| format!("cannot read {}: {e}", config.display()))?;

    let splice = match plan_edit(&text) {
        EditPlan::AlreadyConfigured => {
            println!(
                "collide: all {} sidebar rows are already in {}; nothing to do.",
                TOKEN_COLOURS.len(),
                config.display()
            );
            return Ok(());
        }
        EditPlan::CouldNotPlace(reason) => {
            return Err(format!(
                "could not add collide's sidebar rows to {}: {reason}.\n\n\
                 Add them by hand and run the setup action again:\n\n{}",
                config.display(),
                manual_snippet()
            )
            .into())
        }
        EditPlan::Edit(splice) => splice,
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
    std::fs::write(&config, &splice.text)?;

    match reload_herdr_config() {
        Ok(()) => {
            let count = splice.added.len();
            let plural = if count == 1 { "row" } else { "rows" };
            println!(
                "collide: added {count} sidebar {plural} to {}: {} (backup at {}).",
                config.display(),
                name_list(&splice.added),
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
            // Deliberately not "herdr rejected the config": the reload can also
            // fail because herdr was unreachable, and blaming the file for that
            // sends the reader to the wrong place entirely. The inner error says
            // which it was.
            Err(format!(
                "the sidebar rows were rolled back because the config reload did not \
                 succeed: {err}"
            )
            .into())
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
///
/// Over the socket rather than by shelling out to `herdr server reload-config`.
/// Plugin commands run with no shell and a minimal PATH — `/usr/local/bin:/bin:/usr/bin`
/// on the machine this was written against — and herdr installs itself in
/// `~/.local/bin`, so the bare name would not resolve and the setup action would
/// report that herdr had rejected a config it never saw. The socket is injected
/// into every command herdr spawns, so it is always reachable.
///
/// It also lets us read the answer. `server.reload_config` does not return
/// `{"type":"ok"}`; it returns a status that can be `partial` or `failed` with
/// diagnostics, and the process exit status of the CLI cannot express that.
fn reload_herdr_config() -> Result<()> {
    Herdr::connect()?.reload_config()
}
