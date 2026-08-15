//! Tests for the config.toml splice. The edit itself is where the risk lives,
//! since it rewrites a file the plugin does not own, so it is a pure function
//! over strings and is tested as one.

use collide::setup::plan_edit;

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

#[test]
fn splices_into_an_existing_spaces_section() {
    let out = plan_edit(REAL_WORLD).expect("an edit was planned");

    for token in ["collide_overlap", "collide_runaway", "collide_conflict"] {
        assert!(out.contains(&format!("\"${token}\"")), "missing {token}");
    }

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
    let once = plan_edit(REAL_WORLD).expect("first edit");
    assert!(plan_edit(&once).is_none(), "second run must be a no-op");
}

#[test]
fn appends_a_section_when_none_exists() {
    let input = "[theme]\nname = \"vesper\"\n";
    let out = plan_edit(input).expect("an edit was planned");
    assert!(out.contains("[ui.sidebar.spaces]"));
    assert!(out.contains("$collide_conflict"));
    assert!(
        out.starts_with("[theme]"),
        "existing content must be preserved"
    );
}

#[test]
fn preserves_trailing_newline_state() {
    let with_newline = plan_edit(REAL_WORLD).unwrap();
    assert!(with_newline.ends_with('\n'));

    let without = REAL_WORLD.trim_end().to_string();
    let out = plan_edit(&without).unwrap();
    assert!(!out.ends_with("\n\n"), "must not grow blank lines at EOF");
}

#[test]
fn leaves_a_spaces_section_with_no_rows_array_alone() {
    // Better to do nothing than to guess where a row belongs.
    let input = "[ui.sidebar.spaces]\nrow_gap = 0\n\n[[keys.command]]\nkey = \"prefix+f\"\n";
    assert!(plan_edit(input).is_none());
}

/// Every element of the `rows` array must itself be an array. A bare table
/// dropped between two rows is valid TOML, so herdr accepts the file and then
/// renders nothing — an invisible failure that a "does the token appear in the
/// text" assertion happily passes. This walks the array and reports any element
/// that is not a row.
fn stray_row_elements(text: &str) -> Vec<String> {
    let mut depth = 0usize;
    let mut in_rows = false;
    let mut stray = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if !in_rows {
            if trimmed.starts_with("rows") && trimmed.contains('[') {
                in_rows = true;
                depth = line.matches('[').count() - line.matches(']').count();
            }
            continue;
        }
        // At depth 1 we are directly inside `rows`, so anything starting a value
        // here must open a row.
        if depth == 1 && !trimmed.is_empty() && !trimmed.starts_with('[') && !trimmed.starts_with(']')
        {
            stray.push(trimmed.to_string());
        }
        for ch in line.chars() {
            match ch {
                '[' => depth += 1,
                ']' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        if depth == 0 {
            break;
        }
    }
    stray
}

#[test]
fn tokens_land_inside_a_row_not_beside_one() {
    let out = plan_edit(REAL_WORLD).expect("an edit was planned");
    let stray = stray_row_elements(&out);
    assert!(
        stray.is_empty(),
        "these ended up as siblings of the rows instead of inside one: {stray:#?}\n\n{out}"
    );
}

#[test]
fn a_single_line_row_is_spliced_in_place() {
    let input = "[ui.sidebar.spaces]\nrows = [\n  [\"state_icon\", \"workspace\"],\n  [\"branch\"],\n]\n";
    let out = plan_edit(input).expect("an edit was planned");
    assert!(stray_row_elements(&out).is_empty(), "{out}");
    assert!(out.contains("$collide_conflict"));
    // The row it joined must still be one row.
    let branch_line = out
        .lines()
        .find(|l| l.contains("\"branch\""))
        .expect("branch row survives");
    assert_eq!(branch_line.matches('[').count(), branch_line.matches(']').count());
}
