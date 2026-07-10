use std::collections::BTreeSet;
use std::path::Path;

use anyhow::Result;

/// Parse a Makefile-style depfile (`gcc -MD -MF` output) and return the
/// dependency paths, deduplicated, with targets excluded.
///
/// Handles: `\`-newline continuations, `\ ` escaped spaces, `\\`, `\#`,
/// `$$`, and multiple rules in one file (gcc emits phony rules for headers).
/// Paths under `workspace_root` are returned workspace-relative; others
/// (system headers) stay absolute.
pub fn parse(text: &str, workspace_root: &Path) -> Result<Vec<String>> {
    let mut deps: BTreeSet<String> = BTreeSet::new();
    let mut targets: BTreeSet<String> = BTreeSet::new();

    let mut token = String::new();
    let mut in_deps = false; // false: reading targets, true: reading deps
    let mut chars = text.chars().peekable();

    let flush = |token: &mut String,
                 in_deps: bool,
                 deps: &mut BTreeSet<String>,
                 targets: &mut BTreeSet<String>| {
        if token.is_empty() {
            return;
        }
        let t = std::mem::take(token);
        if in_deps {
            deps.insert(t);
        } else {
            targets.insert(t);
        }
    };

    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.peek() {
                Some('\n') => {
                    chars.next();
                }
                Some('\r') => {
                    chars.next();
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                }
                Some(' ') => {
                    chars.next();
                    token.push(' ');
                }
                Some('#') => {
                    chars.next();
                    token.push('#');
                }
                Some('\\') => {
                    chars.next();
                    token.push('\\');
                }
                _ => token.push('\\'),
            },
            '$' if chars.peek() == Some(&'$') => {
                chars.next();
                token.push('$');
            }
            ':' if !in_deps => {
                // `foo.o:` — colon terminates the target list. A colon inside
                // a later dep token (rare, e.g. absolute Windows paths) is
                // kept verbatim because in_deps is already true.
                flush(&mut token, false, &mut deps, &mut targets);
                in_deps = true;
            }
            '\n' => {
                flush(&mut token, in_deps, &mut deps, &mut targets);
                in_deps = false; // next rule starts with its target
            }
            c if c.is_whitespace() => {
                flush(&mut token, in_deps, &mut deps, &mut targets);
            }
            c => token.push(c),
        }
    }
    flush(&mut token, in_deps, &mut deps, &mut targets);

    let root = workspace_root.to_string_lossy();
    let root_prefix = format!("{}/", root.trim_end_matches('/'));
    let result = deps
        .into_iter()
        .filter(|d| !targets.contains(d))
        .map(|d| {
            if let Some(rel) = d.strip_prefix(&root_prefix) {
                rel.to_string()
            } else {
                d
            }
        })
        .collect();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_simple(text: &str) -> Vec<String> {
        parse(text, Path::new("/ws")).unwrap()
    }

    #[test]
    fn parses_basic_rule() {
        let deps = parse_simple("main.o: src/main.c include/util.h\n");
        assert_eq!(deps, vec!["include/util.h", "src/main.c"]);
    }

    #[test]
    fn handles_line_continuations() {
        let deps = parse_simple("main.o: src/main.c \\\n  include/util.h \\\n  include/other.h\n");
        assert_eq!(
            deps,
            vec!["include/other.h", "include/util.h", "src/main.c"]
        );
    }

    #[test]
    fn handles_escaped_spaces() {
        let deps = parse_simple("main.o: src/my\\ file.c\n");
        assert_eq!(deps, vec!["src/my file.c"]);
    }

    #[test]
    fn excludes_phony_header_targets() {
        // gcc -MP emits phony rules like `include/util.h:` after the main rule.
        let deps = parse_simple("main.o: src/main.c include/util.h\n\ninclude/util.h:\n");
        assert_eq!(deps, vec!["src/main.c"]);
    }

    #[test]
    fn relativizes_paths_under_root() {
        let deps = parse(
            "main.o: /ws/src/main.c /usr/include/stdio.h\n",
            Path::new("/ws"),
        )
        .unwrap();
        assert_eq!(deps, vec!["/usr/include/stdio.h", "src/main.c"]);
    }

    #[test]
    fn handles_dollar_and_hash_escapes() {
        let deps = parse_simple("main.o: src/a$$b.c src/c\\#d.c\n");
        assert_eq!(deps, vec!["src/a$b.c", "src/c#d.c"]);
    }
}
