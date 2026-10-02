//! Safe project names: slugs that satisfy `knowell_core::Name`, made unique.

use std::collections::HashSet;

use knowell_core::Name;

/// Turns arbitrary text into a valid [`Name`] candidate: lowercase ASCII
/// letters, digits, `-` and `_`; runs of other characters become one `-`;
/// leading `-`/`_` are dropped; at most 64 characters. Empty results become
/// `project`.
pub(crate) fn slugify(text: &str) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for c in text.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            if out.is_empty() && c == '_' {
                continue;
            }
            out.push(c);
        } else {
            pending_dash = true;
        }
    }
    out.truncate(Name::MAX_LEN);
    let trimmed = out.trim_end_matches(['-', '_']);
    if trimmed.is_empty() {
        "project".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Picks `base`, or `base-2`, `base-3`, … (kept within the length limit)
/// until the name is not in `taken`.
pub(crate) fn unique(base: &str, taken: &HashSet<String>) -> String {
    if !taken.contains(base) {
        return base.to_owned();
    }
    let mut n: u32 = 2;
    loop {
        let suffix = format!("-{n}");
        let keep = Name::MAX_LEN.saturating_sub(suffix.len());
        let stem: String = base.chars().take(keep).collect();
        let candidate = format!("{stem}{suffix}");
        if !taken.contains(&candidate) {
            return candidate;
        }
        n = n.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_valid_names() {
        for (input, expected) in [
            ("My Service", "my-service"),
            ("  --Weird__Name!! ", "weird__name"),
            ("API.v2", "api-v2"),
            ("Ünïcode", "n-code"),
            ("", "project"),
            ("!!!", "project"),
            ("_hidden", "hidden"),
            ("a/b\\c", "a-b-c"),
        ] {
            let s = slugify(input);
            assert_eq!(s, expected, "{input:?}");
            assert!(Name::new(s).is_ok());
        }
        let long = "x".repeat(200);
        assert_eq!(slugify(&long).len(), 64);
    }

    #[test]
    fn collisions_get_suffixes_within_limit() {
        let mut taken = HashSet::new();
        taken.insert("app".to_owned());
        taken.insert("app-2".to_owned());
        assert_eq!(unique("app", &taken), "app-3");
        assert_eq!(unique("web", &taken), "web");
        let long = "y".repeat(64);
        taken.insert(long.clone());
        let u = unique(&long, &taken);
        assert_eq!(u.len(), 64);
        assert!(u.ends_with("-2"));
        assert!(Name::new(u).is_ok());
    }
}
