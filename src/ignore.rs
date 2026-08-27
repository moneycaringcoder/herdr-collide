//! Repository-relative ignore glob matching.

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Literal(u8),
    Star { crosses_directories: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pattern {
    tokens: Vec<Token>,
    directory: bool,
    literal: bool,
}

/// Compiled repository-relative ignore globs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Matcher {
    patterns: Vec<Pattern>,
}

impl Matcher {
    pub fn new(patterns: &[String]) -> Self {
        let patterns = patterns
            .iter()
            .filter_map(|pattern| {
                if pattern.is_empty() || pattern == "/" {
                    return None;
                }
                let (body, directory) = pattern
                    .strip_suffix('/')
                    .map_or((pattern.as_str(), false), |body| (body, true));
                let literal = !body.contains('*');
                let bytes = body.as_bytes();
                let mut tokens = Vec::with_capacity(bytes.len());
                let mut index = 0;
                while index < bytes.len() {
                    if bytes[index] == b'*' {
                        let crosses_directories = bytes.get(index + 1) == Some(&b'*');
                        tokens.push(Token::Star {
                            crosses_directories,
                        });
                        index += usize::from(crosses_directories) + 1;
                    } else {
                        tokens.push(Token::Literal(bytes[index]));
                        index += 1;
                    }
                }
                Some(Pattern {
                    tokens,
                    directory,
                    literal,
                })
            })
            .collect();
        Self { patterns }
    }

    pub fn matches(&self, path: &str) -> bool {
        if path.is_empty() || path.starts_with('/') {
            return false;
        }
        self.patterns.iter().any(|pattern| {
            if !pattern.directory {
                return matches_whole(path.as_bytes(), &pattern.tokens);
            }
            path.match_indices('/')
                .any(|(boundary, _)| matches_whole(&path.as_bytes()[..boundary], &pattern.tokens))
                || (pattern.literal && matches_whole(path.as_bytes(), &pattern.tokens))
        })
    }
}

/// Convenience path for callers that do not retain a matcher.
pub fn matches_any(path: &str, patterns: &[String]) -> bool {
    Matcher::new(patterns).matches(path)
}

fn matches_whole(path: &[u8], pattern: &[Token]) -> bool {
    let (mut path_index, mut pattern_index) = (0usize, 0usize);
    let mut star: Option<(usize, usize, bool)> = None;
    while path_index < path.len() {
        match pattern.get(pattern_index) {
            Some(Token::Literal(byte)) if *byte == path[path_index] => {
                path_index += 1;
                pattern_index += 1;
            }
            Some(Token::Star {
                crosses_directories,
            }) => {
                pattern_index += 1;
                star = Some((pattern_index, path_index, *crosses_directories));
            }
            _ => {
                let Some((after_star, consumed, crosses_directories)) = star else {
                    return false;
                };
                if consumed >= path.len() || (!crosses_directories && path[consumed] == b'/') {
                    return false;
                }
                let next = consumed + 1;
                star = Some((after_star, next, crosses_directories));
                pattern_index = after_star;
                path_index = next;
            }
        }
    }
    while matches!(pattern.get(pattern_index), Some(Token::Star { .. })) {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}
