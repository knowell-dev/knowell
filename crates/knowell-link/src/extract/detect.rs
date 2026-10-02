//! Framework detection: dependency names from package manifests, matched
//! against pack hints.

use std::collections::{BTreeMap, BTreeSet};

use knowell_core::RepoPath;

use crate::pack::{Detect, Ecosystem};

/// Dependency names per ecosystem, collected from every manifest of a
/// project (monorepos have several).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProjectDeps {
    pub(crate) deps: BTreeMap<Ecosystem, BTreeSet<String>>,
}

impl ProjectDeps {
    fn add(&mut self, ecosystem: Ecosystem, name: &str) {
        let name = name.trim();
        if name.is_empty() || name.len() > 200 {
            return;
        }
        let name = if ecosystem.case_insensitive() {
            name.to_lowercase().replace('_', "-")
        } else {
            name.to_owned()
        };
        self.deps.entry(ecosystem).or_default().insert(name);
    }
}

/// Which manifest format a path is, if any.
pub(crate) fn manifest_kind(path: &RepoPath) -> Option<ManifestKind> {
    let name = path.file_name().to_ascii_lowercase();
    Some(match name.as_str() {
        "package.json" => ManifestKind::PackageJson,
        "go.mod" => ManifestKind::GoMod,
        "pyproject.toml" => ManifestKind::Pyproject,
        "pipfile" => ManifestKind::Pipfile,
        "pubspec.yaml" => ManifestKind::Pubspec,
        "cargo.toml" => ManifestKind::CargoToml,
        "pom.xml" => ManifestKind::Pom,
        "build.gradle" | "build.gradle.kts" => ManifestKind::Gradle,
        _ if name.starts_with("requirements") && name.ends_with(".txt") => {
            ManifestKind::Requirements
        }
        _ if name.ends_with(".csproj") || name.ends_with(".fsproj") => ManifestKind::Csproj,
        _ => return None,
    })
}

/// Supported manifest formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManifestKind {
    PackageJson,
    GoMod,
    Pyproject,
    Pipfile,
    Requirements,
    Pubspec,
    CargoToml,
    Pom,
    Gradle,
    Csproj,
}

/// Adds the dependencies declared in one manifest. Malformed manifests
/// contribute nothing.
pub(crate) fn read_manifest(kind: ManifestKind, text: &str, deps: &mut ProjectDeps) {
    match kind {
        ManifestKind::PackageJson => package_json(text, deps),
        ManifestKind::GoMod => go_mod(text, deps),
        ManifestKind::Pyproject | ManifestKind::Pipfile => pyproject(text, deps),
        ManifestKind::Requirements => {
            for line in text.lines() {
                let line = line.split('#').next().unwrap_or("").trim();
                if !line.is_empty() && !line.starts_with('-') {
                    deps.add(Ecosystem::Python, pep508_name(line));
                }
            }
        }
        ManifestKind::Pubspec => pubspec(text, deps),
        ManifestKind::CargoToml => cargo_toml(text, deps),
        ManifestKind::Pom => pom(text, deps),
        ManifestKind::Gradle => gradle(text, deps),
        ManifestKind::Csproj => csproj(text, deps),
    }
}

fn package_json(text: &str, deps: &mut ProjectDeps) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    for section in [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ] {
        if let Some(map) = value.get(section).and_then(|v| v.as_object()) {
            for name in map.keys() {
                deps.add(Ecosystem::Npm, name);
            }
        }
    }
}

fn go_mod(text: &str, deps: &mut ProjectDeps) {
    let mut in_block = false;
    for line in text.lines() {
        let line = line.split("//").next().unwrap_or("").trim();
        if in_block {
            if line.starts_with(')') {
                in_block = false;
            } else if let Some(module) = line.split_whitespace().next() {
                deps.add(Ecosystem::Go, module);
            }
        } else if line == "require (" || line == "require(" {
            in_block = true;
        } else if let Some(rest) = line.strip_prefix("require ")
            && let Some(module) = rest.split_whitespace().next()
        {
            deps.add(Ecosystem::Go, module);
        }
    }
}

fn pep508_name(spec: &str) -> &str {
    let end = spec
        .find(|c: char| !(c.is_alphanumeric() || matches!(c, '-' | '_' | '.')))
        .unwrap_or(spec.len());
    spec.get(..end).unwrap_or(spec)
}

fn pyproject(text: &str, deps: &mut ProjectDeps) {
    let Ok(value) = text.parse::<toml::Table>() else {
        return;
    };
    let mut add_list = |list: Option<&toml::Value>| {
        if let Some(items) = list.and_then(|v| v.as_array()) {
            for item in items {
                if let Some(spec) = item.as_str() {
                    deps.add(Ecosystem::Python, pep508_name(spec));
                }
            }
        }
    };
    let project = value.get("project");
    add_list(project.and_then(|p| p.get("dependencies")));
    if let Some(optional) = project
        .and_then(|p| p.get("optional-dependencies"))
        .and_then(|v| v.as_table())
    {
        for list in optional.values() {
            add_list(Some(list));
        }
    }
    let mut tables: Vec<&toml::Table> = Vec::new();
    if let Some(poetry) = value
        .get("tool")
        .and_then(|t| t.get("poetry"))
        .and_then(|p| p.as_table())
    {
        for section in ["dependencies", "dev-dependencies"] {
            if let Some(t) = poetry.get(section).and_then(|v| v.as_table()) {
                tables.push(t);
            }
        }
    }
    for section in ["packages", "dev-packages"] {
        if let Some(t) = value.get(section).and_then(|v| v.as_table()) {
            tables.push(t);
        }
    }
    for table in tables {
        for name in table.keys() {
            if name != "python" {
                deps.add(Ecosystem::Python, name);
            }
        }
    }
}

fn pubspec(text: &str, deps: &mut ProjectDeps) {
    let mut in_section = false;
    let mut section_indent = 0usize;
    for line in text.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim();
        if indent == 0 {
            in_section = matches!(
                trimmed,
                "dependencies:" | "dev_dependencies:" | "dependency_overrides:"
            );
            section_indent = usize::MAX;
            continue;
        }
        if !in_section {
            continue;
        }
        if section_indent == usize::MAX {
            section_indent = indent;
        }
        if indent == section_indent
            && let Some((name, _)) = trimmed.split_once(':')
        {
            deps.add(Ecosystem::Pub, name);
        }
    }
}

fn cargo_toml(text: &str, deps: &mut ProjectDeps) {
    let Ok(value) = text.parse::<toml::Table>() else {
        return;
    };
    let mut tables: Vec<&toml::Table> = Vec::new();
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(t) = value.get(section).and_then(|v| v.as_table()) {
            tables.push(t);
        }
    }
    if let Some(t) = value
        .get("workspace")
        .and_then(|w| w.get("dependencies"))
        .and_then(|v| v.as_table())
    {
        tables.push(t);
    }
    if let Some(targets) = value.get("target").and_then(|v| v.as_table()) {
        for target in targets.values() {
            for section in ["dependencies", "dev-dependencies"] {
                if let Some(t) = target.get(section).and_then(|v| v.as_table()) {
                    tables.push(t);
                }
            }
        }
    }
    for table in tables {
        for name in table.keys() {
            deps.add(Ecosystem::Cargo, name);
        }
    }
}

fn between<'a>(text: &'a str, open: &str, close: &str) -> Vec<(usize, &'a str)> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    while let Some(start) = text.get(offset..).and_then(|t| t.find(open)) {
        let begin = offset + start + open.len();
        let Some(len) = text.get(begin..).and_then(|t| t.find(close)) else {
            break;
        };
        if let Some(inner) = text.get(begin..begin + len) {
            out.push((begin, inner.trim()));
        }
        offset = begin + len;
    }
    out
}

fn pom(text: &str, deps: &mut ProjectDeps) {
    let groups = between(text, "<groupId>", "</groupId>");
    let artifacts = between(text, "<artifactId>", "</artifactId>");
    for (position, artifact) in artifacts {
        let group = groups
            .iter()
            .rev()
            .find(|(p, _)| *p < position)
            .map(|(_, g)| *g)
            .unwrap_or("");
        deps.add(Ecosystem::Maven, &format!("{group}:{artifact}"));
    }
}

fn gradle(text: &str, deps: &mut ProjectDeps) {
    for quote in ['"', '\''] {
        for part in text.split(quote).skip(1).step_by(2) {
            let fields: Vec<&str> = part.split(':').collect();
            if fields.len() >= 2
                && fields
                    .iter()
                    .take(2)
                    .all(|f| !f.is_empty() && !f.contains(char::is_whitespace))
            {
                let group = fields.first().copied().unwrap_or("");
                let artifact = fields.get(1).copied().unwrap_or("");
                deps.add(Ecosystem::Maven, &format!("{group}:{artifact}"));
            }
        }
    }
}

fn csproj(text: &str, deps: &mut ProjectDeps) {
    for (_, attrs) in between(text, "<PackageReference", ">") {
        if let Some((_, rest)) = attrs.split_once("Include=\"")
            && let Some(name) = rest.split('"').next()
        {
            deps.add(Ecosystem::Nuget, name);
        }
    }
}

fn hint_matches(hint: &str, name: &str, case_insensitive: bool) -> bool {
    let (hint, name) = if case_insensitive {
        (hint.to_lowercase().replace('_', "-"), name.to_owned())
    } else {
        (hint.to_owned(), name.to_owned())
    };
    if let Some(prefix) = hint.strip_suffix('*') {
        return name.starts_with(prefix);
    }
    if hint.ends_with('/') || hint.ends_with(':') {
        return name.starts_with(&hint);
    }
    name == hint
}

/// Whether project-level hints (dependencies, files) match.
pub(crate) fn project_matches(detect: &Detect, deps: &ProjectDeps, paths: &[RepoPath]) -> bool {
    if detect.is_empty() || detect.always {
        return true;
    }
    for (ecosystem, hints) in &detect.deps {
        if let Some(names) = deps.deps.get(ecosystem)
            && hints.iter().any(|hint| {
                names
                    .iter()
                    .any(|name| hint_matches(hint, name, ecosystem.case_insensitive()))
            })
        {
            return true;
        }
    }
    if let Some(files) = &detect.files
        && paths.iter().any(|p| files.is_match(p.as_str()))
    {
        return true;
    }
    false
}

/// Whether a file's imports match the import hints.
pub(crate) fn imports_match(detect: &Detect, imports: &[String]) -> bool {
    detect.imports.iter().any(|hint| {
        imports
            .iter()
            .any(|import| import == hint || import.starts_with(hint.as_str()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(deps: &ProjectDeps, ecosystem: Ecosystem) -> Vec<String> {
        deps.deps
            .get(&ecosystem)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }

    #[test]
    fn manifests() {
        let mut deps = ProjectDeps::default();
        read_manifest(
            ManifestKind::PackageJson,
            r#"{"dependencies":{"@nestjs/common":"1"},"devDependencies":{"jest":"1"}}"#,
            &mut deps,
        );
        read_manifest(
            ManifestKind::GoMod,
            "module x\n\nrequire (\n\tgithub.com/gin-gonic/gin v1 // indirect\n)\nrequire github.com/x/y v2\n",
            &mut deps,
        );
        read_manifest(
            ManifestKind::Pyproject,
            "[project]\ndependencies = [\"FastAPI==0.1\", \"sqlalchemy[asyncio]>=2\"]\n[tool.poetry.dependencies]\npython = \"3\"\nhttpx = \"*\"\n",
            &mut deps,
        );
        read_manifest(
            ManifestKind::Pubspec,
            "name: x\ndependencies:\n  flutter:\n    sdk: flutter\n  dio: ^5\ndev_dependencies:\n  test: any\n",
            &mut deps,
        );
        read_manifest(
            ManifestKind::CargoToml,
            "[dependencies]\nrdkafka = \"0.36\"\n",
            &mut deps,
        );
        read_manifest(
            ManifestKind::Pom,
            "<dependency><groupId>org.springframework.kafka</groupId><artifactId>spring-kafka</artifactId></dependency>",
            &mut deps,
        );
        read_manifest(
            ManifestKind::Gradle,
            "implementation(\"com.squareup.retrofit2:retrofit:2.9.0\")\n",
            &mut deps,
        );
        read_manifest(
            ManifestKind::Csproj,
            "<PackageReference Include=\"Confluent.Kafka\" Version=\"2\" />",
            &mut deps,
        );
        read_manifest(
            ManifestKind::Requirements,
            "pika>=1 # amqp\n-r other.txt\n",
            &mut deps,
        );
        assert_eq!(names(&deps, Ecosystem::Npm), ["@nestjs/common", "jest"]);
        assert_eq!(
            names(&deps, Ecosystem::Go),
            ["github.com/gin-gonic/gin", "github.com/x/y"]
        );
        assert_eq!(
            names(&deps, Ecosystem::Python),
            ["fastapi", "httpx", "pika", "sqlalchemy"]
        );
        assert_eq!(names(&deps, Ecosystem::Pub), ["dio", "flutter", "test"]);
        assert_eq!(names(&deps, Ecosystem::Cargo), ["rdkafka"]);
        assert_eq!(
            names(&deps, Ecosystem::Maven),
            [
                "com.squareup.retrofit2:retrofit",
                "org.springframework.kafka:spring-kafka"
            ]
        );
        assert_eq!(names(&deps, Ecosystem::Nuget), ["confluent.kafka"]);
    }

    #[test]
    fn malformed_manifests_are_ignored() {
        let mut deps = ProjectDeps::default();
        read_manifest(ManifestKind::PackageJson, "{not json", &mut deps);
        read_manifest(ManifestKind::Pyproject, "[[[", &mut deps);
        read_manifest(ManifestKind::CargoToml, "= x", &mut deps);
        read_manifest(ManifestKind::Pom, "<groupId>unterminated", &mut deps);
        assert!(deps.deps.is_empty());
    }

    #[test]
    fn hints() {
        assert!(hint_matches("@nestjs/", "@nestjs/common", false));
        assert!(hint_matches(
            "github.com/go-chi/chi*",
            "github.com/go-chi/chi/v5",
            false
        ));
        assert!(hint_matches("FastAPI", "fastapi", true));
        assert!(!hint_matches("next", "nextjs-progressbar", false));
    }
}
