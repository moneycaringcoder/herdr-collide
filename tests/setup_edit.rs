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
