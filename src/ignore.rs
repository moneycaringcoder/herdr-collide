//! Repository-relative ignore glob matching.

/// Returns whether a repository-relative path matches any configured pattern.
///
/// The grammar is deliberately small because an over-eager ignore silently
/// hides a real collision: `*` consumes zero or more non-`/` characters, `**`
/// may also consume `/`, and a trailing `/` selects the matched directory and
/// everything below it. `?`, character classes, brace expansion, and negation
/// have no special meaning and are matched literally. Patterns match the whole
/// path from the repository root, so `*.gen.rs` matches
/// `a.gen.rs` but not `src/a.gen.rs`.
///
/// Callers must pass the repository-relative path reported by git. Absolute
/// paths are rejected rather than matched against a pattern accidentally.
pub fn matches_any(path: &str, patterns: &[String]) -> bool {
    if path.is_empty() || path.starts_with('/') {
        return false;
    }

    patterns
        .iter()
        .any(|pattern| matches_pattern(path, pattern))
}

fn matches_pattern(path: &str, pattern: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }

    let Some(directory_pattern) = pattern.strip_suffix('/') else {
        return matches_whole_path(path, pattern);
    };
    if directory_pattern.is_empty() {
        return false;
    }

    // Directory rules are tested at component boundaries. The full-path
    // fallback lets a literal rule cover a changed submodule whose path names
    // the directory itself. Wildcard rules cannot use that fallback because
    // they would also hide matching root-level files.
    path.match_indices('/')
        .any(|(boundary, _)| matches_whole_path(&path[..boundary], directory_pattern))
        || (!directory_pattern.contains('*') && matches_whole_path(path, directory_pattern))
}

fn matches_whole_path(path: &str, pattern: &str) -> bool {
    let path = path.as_bytes();
    let pattern = pattern.as_bytes();
    let mut matched = vec![false; path.len() + 1];
    matched[0] = true;

    let mut pattern_index = 0;
    while pattern_index < pattern.len() {
        if pattern[pattern_index] == b'*' {
            let crosses_directories = pattern.get(pattern_index + 1) == Some(&b'*');
            if crosses_directories {
                pattern_index += 1;
            }

            for path_index in 1..=path.len() {
                let may_consume = crosses_directories || path[path_index - 1] != b'/';
                matched[path_index] =
                    matched[path_index] || (may_consume && matched[path_index - 1]);
            }
        } else {
            for path_index in (1..=path.len()).rev() {
                matched[path_index] =
                    matched[path_index - 1] && path[path_index - 1] == pattern[pattern_index];
            }
            matched[0] = false;
        }

        pattern_index += 1;
    }

    matched[path.len()]
}
