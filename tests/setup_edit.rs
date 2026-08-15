//! Tests for the config.toml splice. The edit itself is where the risk lives,
//! since it rewrites a file the plugin does not own, so it is a pure function
//! over strings and is tested as one.
//!
//! Every assertion about a spliced file goes through `rows_of`, a small
//! independent parser at the bottom of this file, rather than through
//! `text.contains("$collide_conflict")`. A "does the token appear" assertion
//! passes just as happily when the token was appended inside a comment or
//! dropped in beside the rows instead of inside one — both of which produce a
//! file that parses, reloads and renders nothing, and both of which shipped.

use collide::setup::{plan_edit, EditPlan};

// Note the `r##` delimiter: hex colours end in `"#...`, which would close a
// plain `r#` raw string mid-literal.
const REAL_WORLD: &str = r##"[theme]
name = "vesper"

[ui.sidebar.agents]
rows = [
  ["state_icon", "agent", "$title"],
]

[ui.sidebar.spaces]
rows = [
  ["state_icon", "workspace"],
  ["branch", "git_status",
    { token = "$git_dirty", fg = "#FFC799" }],
]

[[keys.command]]
key = "prefix+f"
"##;

/// The four token names `--setup` contributes, in the order it writes them.
const OURS: [&str; 4] = [
    "collide_overlap",
    "collide_runaway",
    "collide_unknown",
    "collide_conflict",
];

fn edited(text: &str) -> String {
    match plan_edit(text) {
        EditPlan::Edit(splice) => splice.text,
        other => panic!("expected an edit, got {other:?}"),
    }
}

fn added(text: &str) -> Vec<String> {
    match plan_edit(text) {
        EditPlan::Edit(splice) => splice.added.iter().map(|t| t.to_string()).collect(),
        other => panic!("expected an edit, got {other:?}"),
    }
}

fn reason(text: &str) -> String {
    match plan_edit(text) {
        EditPlan::CouldNotPlace(reason) => reason,
        other => panic!("expected CouldNotPlace, got {other:?}"),
    }
}

/// Asserts that every one of our tokens sits inside a row of
/// `[ui.sidebar.spaces].rows`, and that every element of that array is a row.
fn assert_tokens_are_inside_rows(text: &str, expected: &[&str]) {
    let rows = rows_of(text).unwrap_or_else(|e| panic!("could not parse rows: {e}\n\n{text}"));
    let Node::Array(elements) = rows else {
        panic!("rows is not an array: {rows:?}");
    };
    for element in &elements {
        assert!(
            matches!(element, Node::Array(_)),
            "an element of `rows` is not a row, so herdr renders nothing: {element:?}\n\n{text}"
        );
    }
    for token in expected {
        let needle = format!("${token}");
        let found = elements.iter().any(|row| match row {
            Node::Array(cells) => cells.iter().any(|cell| cell.mentions(&needle)),
            _ => false,
        });
        assert!(found, "{needle} is not inside any row\n\n{text}");
    }
}

#[test]
fn splices_into_an_existing_spaces_section() {
    let out = edited(REAL_WORLD);

    assert_tokens_are_inside_rows(&out, &OURS);

    // A clean workspace clears its badge rather than setting a token, so a row
    // naming `collide_clean` could never display anything and is not written.
    assert!(
        !out.contains("$collide_clean"),
        "wrote a row for a token that is never set"
    );

    // The user's own rows survive untouched.
    assert!(out.contains("{ token = \"$git_dirty\", fg = \"#FFC799\" }"));
    assert!(out.contains("[ui.sidebar.agents]"));
    assert!(out.contains("key = \"prefix+f\""));

    // The insert stays inside the spaces section.
    let spaces = out.find("[ui.sidebar.spaces]").unwrap();
    let keys = out.find("[[keys.command]]").unwrap();
    let ours = out.find("$collide_conflict").unwrap();
    assert!(spaces < ours && ours < keys, "token escaped its section");
}

#[test]
fn is_idempotent() {
    let once = edited(REAL_WORLD);
    assert_eq!(
        plan_edit(&once),
        EditPlan::AlreadyConfigured,
        "second run must be a genuine no-op"
    );
}

/// The upgrade path. Everyone who installed before `collide_unknown` existed
/// has a config naming the other three; an all-or-nothing "is any of our tokens
/// here?" gate made `--setup` print "already configured" and add nothing, so
/// the new severity could never render for any of them.
#[test]
fn a_config_written_by_an_older_version_gains_only_the_missing_tokens() {
    let older = r##"[ui.sidebar.spaces]
rows = [
  ["state_icon", "workspace"],
  ["branch", "git_status",
    { token = "$git_clean",       fg = "#99FFE4" },
    { token = "$collide_overlap",  fg = "#FFC799" },
    { token = "$collide_runaway",  fg = "#FFB27F" },
    { token = "$collide_conflict", fg = "#FF8080" }],
]
"##;

    assert_eq!(
        added(older),
        vec!["collide_unknown"],
        "only the token the older version never knew about"
    );

    let out = edited(older);
    assert_tokens_are_inside_rows(&out, &OURS);

    // The three it already had are not duplicated.
    for token in ["collide_overlap", "collide_runaway", "collide_conflict"] {
        assert_eq!(
            out.matches(&format!("\"${token}\"")).count(),
            1,
            "{token} was written twice"
        );
    }
    // And the user's own token is untouched.
    assert_eq!(out.matches("\"$git_clean\"").count(), 1);

    // A second run now really has nothing to do.
    assert_eq!(plan_edit(&out), EditPlan::AlreadyConfigured);
}

#[test]
fn a_token_named_only_in_a_comment_does_not_count_as_configured() {
    let text = r##"[ui.sidebar.spaces]
rows = [
  # { token = "$collide_overlap", fg = "#FFC799" },
  ["branch"],
]
"##;
    assert_eq!(added(text), OURS.to_vec(), "a comment configures nothing");
    assert_tokens_are_inside_rows(&edited(text), &OURS);
}

#[test]
fn appends_a_section_when_none_exists() {
    let input = "[theme]\nname = \"vesper\"\n";
    let out = edited(input);
    assert!(out.contains("[ui.sidebar.spaces]"));
    assert_tokens_are_inside_rows(&out, &OURS);
    assert!(
        out.starts_with("[theme]"),
        "existing content must be preserved"
    );
}

#[test]
fn preserves_trailing_newline_state() {
    let with_newline = edited(REAL_WORLD);
    assert!(with_newline.ends_with('\n'));

    let without = REAL_WORLD.trim_end().to_string();
    let out = edited(&without);
    assert!(!out.ends_with("\n\n"), "must not grow blank lines at EOF");
}

/// A CRLF file used to come back entirely LF: every line in the user's config
/// changed, not just the row that was edited.
#[test]
fn preserves_crlf_line_endings() {
    let crlf = REAL_WORLD.replace('\n', "\r\n");
    let out = edited(&crlf);

    assert!(out.contains("\r\n"), "line endings were rewritten to LF");
    assert_eq!(
        out.matches('\n').count(),
        out.matches("\r\n").count(),
        "the file gained a bare LF: {out:?}"
    );
    assert_tokens_are_inside_rows(&out.replace("\r\n", "\n"), &OURS);

    // And an LF file stays LF.
    let lf = edited(REAL_WORLD);
    assert!(!lf.contains('\r'), "an LF file gained carriage returns");
}

/// Case (a) from the review: the whole array on one line. This used to return
/// "nothing to do". It now works — a one-line rows array is an ordinary thing
/// to write — and the entries have to land inside the *last row*, not before
/// the bracket that closes the whole array.
#[test]
fn a_rows_array_written_entirely_on_one_line_is_spliced_into_its_last_row() {
    let input = "[ui.sidebar.spaces]\nrows = [[\"state_icon\", \"workspace\"], [\"branch\"]]\n";
    let out = edited(input);

    assert_tokens_are_inside_rows(&out, &OURS);
    assert_eq!(
        out.lines().count(),
        2,
        "a one-line array should stay one line: {out}"
    );
}

/// Case (b): a `[ui.sidebar.spaces]` section with no rows array of its own.
/// Doing nothing is right; *saying* nothing needed doing is not.
#[test]
fn a_spaces_section_with_no_rows_array_fails_loudly() {
    let input = "[ui.sidebar.spaces]\nrow_gap = 0\n\n[ui.sidebar.agents]\nrows = [\n  [\"state_icon\"],\n]\n";
    let reason = reason(input);
    assert!(
        reason.contains("rows"),
        "the reason must name what was missing: {reason}"
    );
}

/// Case (c): a commented-out row at the end of the array. The old scanner
/// counted its brackets, decided it was the last row, and appended the token
/// tables *inside the comment* — a file that parses, reloads clean, and renders
/// nothing at all, reported to the user as a success.
#[test]
fn a_commented_out_row_is_not_treated_as_a_row() {
    let input = "[ui.sidebar.spaces]\nrows = [\n  [\"state_icon\", \"workspace\"],\n  [\"branch\"],\n  # [\"retired\", \"row\"],\n]\n";
    let out = edited(input);

    assert_tokens_are_inside_rows(&out, &OURS);
    for line in out.lines() {
        if line.trim_start().starts_with('#') {
            assert!(
                !line.contains("$collide_"),
                "a token was spliced into a comment: {line}"
            );
        }
    }
}

/// A bracket inside a trailing comment used to underflow the depth counter:
/// a panic in a debug build, and in a release build — which is what
/// `[[build]]` produces — a silent "nothing to do".
#[test]
fn a_bracket_in_a_comment_neither_panics_nor_silently_gives_up() {
    let input = "[ui.sidebar.spaces]\nrows = [] # empty for now ]\n";
    let reason = reason(input);
    assert!(
        reason.contains("row"),
        "the reason must say what could not be found: {reason}"
    );
}

/// A `]` inside a string is not a bracket either.
#[test]
fn a_bracket_inside_a_string_is_not_a_bracket() {
    let input = "[ui.sidebar.spaces]\nrows = [\n  [\"state_icon\", \"workspace\"],\n  [\"a ] literal\", \"branch\"],\n]\n";
    let out = edited(input);
    assert_tokens_are_inside_rows(&out, &OURS);
    assert!(out.contains("\"a ] literal\""), "the string was mangled");
}

/// `rows_per_page = [...]` is somebody else's setting.
#[test]
fn a_key_that_merely_starts_with_rows_is_not_the_rows_array() {
    let input = "[ui.sidebar.spaces]\nrows_per_page = [[1], [2]]\n";
    let reason = reason(input);
    assert!(reason.contains("rows"), "{reason}");
}

#[test]
fn a_multi_line_row_keeps_its_shape() {
    let input = "[ui.sidebar.spaces]\nrows = [\n  [\n    \"state_icon\",\n    \"workspace\",\n  ],\n  [\n    \"branch\",\n    { token = \"$git_dirty\", fg = \"#FFC799\" },\n  ],\n]\n";
    let out = edited(input);
    assert_tokens_are_inside_rows(&out, &OURS);
    assert!(out.contains("{ token = \"$git_dirty\", fg = \"#FFC799\" },"));
}

/// A file that is not valid TOML at all has no `[ui.sidebar.spaces]` line for
/// the scanner to find (the header is unterminated), so the section is
/// appended. herdr then rejects the reload, and `run_setup` restores the
/// backup — the loud path, not the silent one.
#[test]
fn a_file_that_is_not_valid_toml_still_produces_a_definite_answer() {
    let input = "[ui.sidebar.spaces\nrows = [\n  [\"branch\"],\n]\n";
    match plan_edit(input) {
        EditPlan::Edit(_) | EditPlan::CouldNotPlace(_) => {}
        EditPlan::AlreadyConfigured => panic!("a broken file must never read as configured"),
    }
}

// ---------------------------------------------------------------------------
// An independent parser, so the assertions above do not lean on the same
// bracket-counting the code under test uses.
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum Node {
    Array(Vec<Node>),
    /// An inline table or a bare scalar, kept as its source text.
    Leaf(String),
}

impl Node {
    fn mentions(&self, needle: &str) -> bool {
        match self {
            Node::Leaf(text) => text.contains(needle),
            Node::Array(children) => children.iter().any(|c| c.mentions(needle)),
        }
    }
}

/// Extracts and parses the `rows` value of `[ui.sidebar.spaces]`.
///
/// Deliberately a recursive-descent value parser rather than a line scan: if it
/// agreed with the implementation about what a bracket is, it could not catch
/// the implementation being wrong about it.
fn rows_of(text: &str) -> Result<Node, String> {
    let section = text
        .find("[ui.sidebar.spaces]")
        .ok_or("no [ui.sidebar.spaces] section")?;
    let rest = &text[section + "[ui.sidebar.spaces]".len()..];
    // Stop at the next table header so `rows` from another section cannot be
    // picked up by mistake.
    // A table header is `[` followed by a bare key, which starts with a letter
    // or an underscore — unlike a row, which starts with a quoted string.
    let end = rest
        .match_indices('\n')
        .map(|(i, _)| i + 1)
        .find(|start| {
            let line = rest[*start..].trim_start();
            let after = line.trim_start_matches('[');
            line.starts_with('[')
                && after
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        })
        .unwrap_or(rest.len());
    let body = &rest[..end];

    let mut cursor = Cursor::new(body);
    cursor.seek_rows_value()?;
    cursor.value()
}

struct Cursor<'a> {
    bytes: &'a [u8],
    text: &'a str,
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            bytes: text.as_bytes(),
            text,
            at: 0,
        }
    }

    /// Moves to the first byte after the `=` of the `rows` key.
    fn seek_rows_value(&mut self) -> Result<(), String> {
        for (offset, line) in line_starts(self.text) {
            let trimmed = line.trim_start();
            let Some(rest) = trimmed.strip_prefix("rows") else {
                continue;
            };
            if !rest.trim_start().starts_with('=') {
                continue;
            }
            let indent = line.len() - trimmed.len();
            let eq = rest.find('=').ok_or("no = after rows")?;
            self.at = offset + indent + "rows".len() + eq + 1;
            return Ok(());
        }
        Err("no rows key".to_string())
    }

    fn skip_trivia(&mut self) {
        while self.at < self.bytes.len() {
            match self.bytes[self.at] {
                b' ' | b'\t' | b'\r' | b'\n' | b',' => self.at += 1,
                b'#' => {
                    while self.at < self.bytes.len() && self.bytes[self.at] != b'\n' {
                        self.at += 1;
                    }
                }
                _ => break,
            }
        }
    }

    fn value(&mut self) -> Result<Node, String> {
        self.skip_trivia();
        if self.at >= self.bytes.len() {
            return Err("unexpected end of value".to_string());
        }
        if self.bytes[self.at] == b'[' {
            self.at += 1;
            let mut children = Vec::new();
            loop {
                self.skip_trivia();
                if self.at >= self.bytes.len() {
                    return Err("unterminated array".to_string());
                }
                if self.bytes[self.at] == b']' {
                    self.at += 1;
                    return Ok(Node::Array(children));
                }
                children.push(self.value()?);
            }
        }
        // A scalar or an inline table: consume to the next `,` or `]` that is
        // not inside a string or a nested `{}`.
        let start = self.at;
        let mut braces = 0usize;
        while self.at < self.bytes.len() {
            match self.bytes[self.at] {
                b'"' | b'\'' => {
                    let quote = self.bytes[self.at];
                    self.at += 1;
                    while self.at < self.bytes.len() {
                        if quote == b'"' && self.bytes[self.at] == b'\\' {
                            self.at += 2;
                            continue;
                        }
                        if self.bytes[self.at] == quote {
                            break;
                        }
                        self.at += 1;
                    }
                    self.at += 1;
                }
                b'{' => {
                    braces += 1;
                    self.at += 1;
                }
                b'}' => {
                    braces = braces.saturating_sub(1);
                    self.at += 1;
                }
                b',' | b']' if braces == 0 => break,
                b'#' if braces == 0 => break,
                _ => self.at += 1,
            }
        }
        Ok(Node::Leaf(self.text[start..self.at].trim().to_string()))
    }
}

fn line_starts(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        out.push((offset, line.trim_end_matches(['\n', '\r'])));
        offset += line.len();
    }
    out
}
