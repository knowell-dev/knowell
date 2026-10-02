//! Parsers for the files workspace import reads. All of them take untrusted
//! text, never fail (unrecognised lines are skipped) and never panic.

/// One `[submodule "name"]` section of `.gitmodules`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Submodule {
    pub(crate) name: String,
    pub(crate) path: Option<String>,
    pub(crate) url: Option<String>,
    pub(crate) branch: Option<String>,
}

fn unquote(s: &str) -> &str {
    let s = s.trim();
    s.strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .or_else(|| s.strip_prefix('\'').and_then(|t| t.strip_suffix('\'')))
        .unwrap_or(s)
}

/// Parses `.gitmodules` (git-config syntax).
pub(crate) fn parse_gitmodules(text: &str) -> Vec<Submodule> {
    let mut out: Vec<Submodule> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let header = header.trim();
            let name = header
                .strip_prefix("submodule")
                .map(|rest| unquote(rest).to_owned());
            if let Some(name) = name {
                out.push(Submodule {
                    name,
                    ..Submodule::default()
                });
            } else {
                // Some other section: keys below it must not leak into the
                // previous submodule.
                out.push(Submodule {
                    name: String::new(),
                    ..Submodule::default()
                });
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let Some(current) = out.last_mut() else {
            continue;
        };
        let value = unquote(value).to_owned();
        match key.trim().to_ascii_lowercase().as_str() {
            "path" => current.path = Some(value),
            "url" => current.url = Some(value),
            "branch" => current.branch = Some(value),
            _ => {}
        }
    }
    out.retain(|s| !s.name.is_empty());
    out
}

/// Removes credentials from a clone URL: `https://user:secret@host/x` becomes
/// `https://host/x`. `git@host:x` (no password) is kept as is.
pub(crate) fn sanitize_remote(url: &str) -> String {
    let url = url.trim();
    if let Some((scheme, rest)) = url.split_once("://") {
        let (authority, path) = match rest.find('/') {
            Some(i) => rest.split_at(i),
            None => (rest, ""),
        };
        if let Some((userinfo, host)) = authority.rsplit_once('@')
            && userinfo.contains(':')
        {
            return format!("{scheme}://{host}{path}");
        }
        if scheme.starts_with("http")
            && let Some((_, host)) = authority.rsplit_once('@')
        {
            return format!("{scheme}://{host}{path}");
        }
    }
    url.to_owned()
}

/// Parses `go.work`: the directories of `use` directives.
pub(crate) fn parse_go_work(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut block: Option<String> = None;
    for raw in text.lines() {
        let line = raw.split("//").next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(kind) = &block {
            if line == ")" {
                block = None;
            } else if kind == "use" {
                out.push(unquote(line).to_owned());
            }
            continue;
        }
        let mut words = line.splitn(2, char::is_whitespace);
        let keyword = words.next().unwrap_or("");
        let rest = words.next().unwrap_or("").trim();
        if rest == "(" {
            block = Some(keyword.to_owned());
        } else if keyword == "use" && !rest.is_empty() {
            out.push(unquote(rest).to_owned());
        }
    }
    out
}

fn strip_yaml_comment(s: &str) -> &str {
    match s.find(" #") {
        Some(i) => s.get(..i).unwrap_or(s),
        None => s,
    }
}

/// Parses the `packages:` list of `pnpm-workspace.yaml` (block or inline
/// sequence). Negated patterns keep their leading `!`.
pub(crate) fn parse_pnpm_workspace(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_packages = false;
    for raw in text.lines() {
        let line = strip_yaml_comment(raw).trim_end();
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let indented = line.starts_with(char::is_whitespace);
        if !in_packages {
            if let Some(rest) = line.strip_prefix("packages:") {
                let rest = rest.trim();
                if let Some(inline) = rest.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
                    out.extend(
                        inline
                            .split(',')
                            .map(|p| unquote(p).to_owned())
                            .filter(|p| !p.is_empty()),
                    );
                } else if rest.is_empty() {
                    in_packages = true;
                }
            }
            continue;
        }
        let item = line.trim_start();
        if let Some(value) = item.strip_prefix('-') {
            out.push(unquote(value).to_owned());
        } else if !indented {
            in_packages = false;
        }
    }
    out.retain(|p| !p.is_empty());
    out
}

/// Parses `workspaces` of a `package.json` (array, or `{ "packages": [...] }`).
pub(crate) fn parse_package_workspaces(text: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let list = match value.get("workspaces") {
        Some(serde_json::Value::Array(a)) => Some(a),
        Some(serde_json::Value::Object(o)) => o.get("packages").and_then(|p| p.as_array()),
        _ => None,
    };
    list.map(|a| {
        a.iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect()
    })
    .unwrap_or_default()
}

/// Members and excludes of a Cargo `[workspace]`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct CargoWorkspace {
    pub(crate) members: Vec<String>,
    pub(crate) exclude: Vec<String>,
}

/// Parses the `[workspace]` table of a `Cargo.toml`.
pub(crate) fn parse_cargo_workspace(text: &str) -> CargoWorkspace {
    let Ok(table) = text.parse::<toml::Table>() else {
        return CargoWorkspace::default();
    };
    let list = |key: &str| -> Vec<String> {
        table
            .get("workspace")
            .and_then(|w| w.get(key))
            .and_then(toml::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    CargoWorkspace {
        members: list("members"),
        exclude: list("exclude"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gitmodules_sections() {
        let text = "# c\n[submodule \"libs/core\"]\n\tpath = libs/core\n\turl = https://example.com/a/core.git\n\tbranch = develop\n[submodule \"ui\"]\n\tpath = ui\n\turl=git@example.com:a/ui.git\n[other]\n\tpath = nope\n[submodule \"bare\"]\n";
        let subs = parse_gitmodules(text);
        assert_eq!(subs.len(), 3);
        assert_eq!(subs[0].name, "libs/core");
        assert_eq!(subs[0].branch.as_deref(), Some("develop"));
        assert_eq!(subs[1].url.as_deref(), Some("git@example.com:a/ui.git"));
        assert_eq!(subs[1].branch, None);
        assert_eq!(subs[2].path, None);
    }

    #[test]
    fn gitmodules_garbage_is_harmless() {
        assert!(parse_gitmodules("").is_empty());
        assert!(parse_gitmodules("path = x\n[[[\n=\n\u{0}").is_empty());
        assert!(parse_gitmodules("[submodule \"a").is_empty());
    }

    #[test]
    fn remotes_lose_credentials() {
        assert_eq!(
            sanitize_remote("https://user:pw@example.com/a/b.git"),
            "https://example.com/a/b.git"
        );
        assert_eq!(
            sanitize_remote("https://tokenonly@example.com/a/b.git"),
            "https://example.com/a/b.git"
        );
        assert_eq!(
            sanitize_remote("git@example.com:a/b.git"),
            "git@example.com:a/b.git"
        );
        assert_eq!(
            sanitize_remote("ssh://git@example.com/a/b.git"),
            "ssh://git@example.com/a/b.git"
        );
    }

    #[test]
    fn go_work_forms() {
        let text = "go 1.22\n\nuse ./single // c\nuse (\n\t./a\n\t\"./b c\"\n\t// ignored\n)\nreplace (\n\tx => ./nope\n)\n";
        assert_eq!(parse_go_work(text), ["./single", "./a", "./b c"]);
        assert!(parse_go_work("use (\n").is_empty());
    }

    #[test]
    fn pnpm_forms() {
        let text = "packages:\n  # all\n  - 'apps/*'\n  - \"libs/**\"\n  - '!**/test/**'\ncatalog:\n  - nope\n";
        assert_eq!(
            parse_pnpm_workspace(text),
            ["apps/*", "libs/**", "!**/test/**"]
        );
        assert_eq!(
            parse_pnpm_workspace("packages: ['a/*', \"b\"]\n"),
            ["a/*", "b"]
        );
        assert_eq!(
            parse_pnpm_workspace("packages:\n- x\n- y\nother: 1\n"),
            ["x", "y"]
        );
        assert!(parse_pnpm_workspace("nothing: here").is_empty());
    }

    #[test]
    fn package_json_forms() {
        assert_eq!(
            parse_package_workspaces("{\"workspaces\":[\"a/*\",1]}"),
            ["a/*"]
        );
        assert_eq!(
            parse_package_workspaces("{\"workspaces\":{\"packages\":[\"p/*\"]}}"),
            ["p/*"]
        );
        assert!(parse_package_workspaces("{\"workspaces\":\"x\"}").is_empty());
        assert!(parse_package_workspaces("{ broken").is_empty());
    }

    #[test]
    fn cargo_forms() {
        let w = parse_cargo_workspace(
            "[workspace]\nmembers = [\"crates/*\", \"tools/x\"]\nexclude = [\"crates/old\"]\n",
        );
        assert_eq!(w.members, ["crates/*", "tools/x"]);
        assert_eq!(w.exclude, ["crates/old"]);
        assert_eq!(
            parse_cargo_workspace("[package]\nname=\"a\""),
            CargoWorkspace::default()
        );
        assert_eq!(parse_cargo_workspace("= broken"), CargoWorkspace::default());
    }
}
