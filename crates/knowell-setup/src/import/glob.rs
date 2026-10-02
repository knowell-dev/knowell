//! Directory-only glob expansion under a root, used for workspace member
//! patterns (`apps/*`, `crates/**`) and worktree folders (`.worktree/*/*`).
//!
//! Patterns come from untrusted repository files, so they can never leave the
//! root (`..` and absolute patterns match nothing), symlinked directories are
//! not followed, and the walk depth is bounded.

use std::fs;
use std::path::Path;

use globset::Glob;

const MAX_DEPTH: usize = 8;

/// Directory names a wildcard never descends into.
const SKIPPED: [&str; 3] = [".git", "node_modules", "target"];

/// Returns whether `pattern` is usable (relative, no `..`).
pub(crate) fn is_safe_pattern(pattern: &str) -> bool {
    let p = pattern.trim();
    !p.is_empty()
        && !p.starts_with('/')
        && !p.starts_with('\\')
        && !p.contains(':')
        && !p.contains('\\')
        && p.split('/').all(|s| s != "..")
}

/// Expands `pattern` to existing directories below `root`, as sorted,
/// `/`-separated paths relative to `root`.
pub(crate) fn expand_dirs(root: &Path, pattern: &str) -> Vec<String> {
    if !is_safe_pattern(pattern) {
        return Vec::new();
    }
    let segments: Vec<&str> = pattern
        .trim()
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    let mut out = Vec::new();
    walk(root, "", &segments, 0, &mut out);
    out.sort();
    out.dedup();
    out
}

/// Whether `rel` (a `/`-separated relative path) matches `pattern`
/// (gitignore-ish glob, `*` does not cross `/`).
pub(crate) fn matches(pattern: &str, rel: &str) -> bool {
    let normalized = pattern
        .trim()
        .trim_start_matches("./")
        .trim_end_matches('/');
    match globset::GlobBuilder::new(normalized)
        .literal_separator(true)
        .build()
    {
        Ok(g) => g.compile_matcher().is_match(rel),
        Err(_) => false,
    }
}

fn is_wild(segment: &str) -> bool {
    segment.contains(['*', '?', '[', '{'])
}

fn join(base: &str, name: &str) -> String {
    if base.is_empty() {
        name.to_owned()
    } else {
        format!("{base}/{name}")
    }
}

fn child_dirs(dir: &Path) -> Vec<String> {
    let Ok(read) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = read
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

fn walk(root: &Path, base: &str, segments: &[&str], depth: usize, out: &mut Vec<String>) {
    let Some((segment, rest)) = segments.split_first() else {
        if !base.is_empty() {
            out.push(base.to_owned());
        }
        return;
    };
    if depth > MAX_DEPTH {
        return;
    }
    let here = root.join(base);
    if *segment == "**" {
        walk(root, base, rest, depth + 1, out);
        for name in child_dirs(&here) {
            if name.starts_with('.') || SKIPPED.contains(&name.as_str()) {
                continue;
            }
            walk(root, &join(base, &name), segments, depth + 1, out);
        }
    } else if is_wild(segment) {
        let Ok(glob) = Glob::new(segment) else { return };
        let matcher = glob.compile_matcher();
        for name in child_dirs(&here) {
            let hidden_ok = segment.starts_with('.') || !name.starts_with('.');
            if hidden_ok && !SKIPPED.contains(&name.as_str()) && matcher.is_match(&name) {
                walk(root, &join(base, &name), rest, depth + 1, out);
            }
        }
    } else {
        let next = join(base, segment);
        if root.join(&next).is_dir() {
            walk(root, &next, rest, depth + 1, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(root: &Path, dirs: &[&str]) {
        for d in dirs {
            fs::create_dir_all(root.join(d)).unwrap();
        }
    }

    #[test]
    fn expands_literals_stars_and_double_stars() {
        let t = tempfile::tempdir().unwrap();
        mk(
            t.path(),
            &[
                "apps/web",
                "apps/api",
                "apps/.hidden",
                "libs/a/b",
                "node_modules/x",
                ".worktree/f/one",
            ],
        );
        let r = t.path();
        assert_eq!(expand_dirs(r, "apps/*"), ["apps/api", "apps/web"]);
        assert_eq!(expand_dirs(r, "./apps/web/"), ["apps/web"]);
        assert_eq!(expand_dirs(r, "libs/**"), ["libs", "libs/a", "libs/a/b"]);
        assert_eq!(expand_dirs(r, ".worktree/*/*"), [".worktree/f/one"]);
        assert!(expand_dirs(r, "*/x").is_empty(), "node_modules is skipped");
        assert!(expand_dirs(r, "missing/*").is_empty());
    }

    #[test]
    fn hostile_patterns_match_nothing() {
        let t = tempfile::tempdir().unwrap();
        mk(t.path(), &["a"]);
        for p in [
            "../a", "a/../..", "/etc", "C:/x", "", "  ", "a\\b", "[", "{a",
        ] {
            assert!(expand_dirs(t.path(), p).is_empty(), "{p:?}");
        }
    }

    #[test]
    fn deep_recursion_is_bounded() {
        let t = tempfile::tempdir().unwrap();
        let deep = (0..20)
            .map(|i| format!("d{i}"))
            .collect::<Vec<_>>()
            .join("/");
        mk(t.path(), &[&deep]);
        let found = expand_dirs(t.path(), "**");
        assert!(found.len() <= MAX_DEPTH + 2, "{}", found.len());
    }

    #[test]
    fn match_respects_separators() {
        assert!(matches("apps/*", "apps/web"));
        assert!(!matches("apps/*", "apps/web/x"));
        assert!(matches("**/test", "a/b/test"));
        assert!(!matches("[", "x"));
    }
}
