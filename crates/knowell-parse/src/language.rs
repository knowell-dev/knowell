//! Language identification and analysis tiers.

use std::fmt;
use std::str::FromStr;

use knowell_core::RepoPath;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// How much structure Knowell extracts for a language.
///
/// Mirrors the capability matrix in `docs/ARCHITECTURE.md` §7.2. The tier is
/// a property of the language, not of one file: a file of an `Exact`
/// language can still be degraded (see [`crate::Degradation`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// tree-sitter symbols today; the language also has a SCIP / language
    /// tool path for semantically resolved references (later milestone).
    Exact,
    /// tree-sitter symbols, imports and chunks; no semantic resolution.
    Structural,
    /// Contract and structure files (schemas, APIs, config, docs): their
    /// declarations (tables, endpoints, RPCs, headings, keys) are symbols.
    Contract,
    /// No structure: text chunking only.
    TextOnly,
}

impl Tier {
    /// Stable lowercase identifier (`exact`, `structural`, `contract`, `text_only`).
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Exact => "exact",
            Tier::Structural => "structural",
            Tier::Contract => "contract",
            Tier::TextOnly => "text_only",
        }
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A source language (or structure format) recognised by Knowell.
///
/// Serialised as its stable lowercase identifier ([`Language::as_str`]).
/// Unknown files are [`Language::Text`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[allow(missing_docs)] // variant names are the language names
pub enum Language {
    // Exact tier.
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Jsx,
    Python,
    Go,
    Java,
    Kotlin,
    CSharp,
    // Structural tier.
    Dart,
    Swift,
    Php,
    Ruby,
    C,
    Cpp,
    Scala,
    Bash,
    Css,
    // Contract / structure files.
    Sql,
    Protobuf,
    Markdown,
    Yaml,
    Json,
    Toml,
    Dockerfile,
    // Text only (recognised so context headers can name them).
    Html,
    Vue,
    Svelte,
    Xml,
    GraphQl,
    Hcl,
    Lua,
    Perl,
    R,
    Elixir,
    Erlang,
    Haskell,
    OCaml,
    Clojure,
    Zig,
    ObjectiveC,
    PowerShell,
    Batch,
    Ini,
    Makefile,
    CMake,
    Groovy,
    /// Plain text and anything unrecognised.
    Text,
}

impl Language {
    /// Every language, in declaration order.
    pub const ALL: &'static [Language] = &[
        Language::Rust,
        Language::TypeScript,
        Language::Tsx,
        Language::JavaScript,
        Language::Jsx,
        Language::Python,
        Language::Go,
        Language::Java,
        Language::Kotlin,
        Language::CSharp,
        Language::Dart,
        Language::Swift,
        Language::Php,
        Language::Ruby,
        Language::C,
        Language::Cpp,
        Language::Scala,
        Language::Bash,
        Language::Css,
        Language::Sql,
        Language::Protobuf,
        Language::Markdown,
        Language::Yaml,
        Language::Json,
        Language::Toml,
        Language::Dockerfile,
        Language::Html,
        Language::Vue,
        Language::Svelte,
        Language::Xml,
        Language::GraphQl,
        Language::Hcl,
        Language::Lua,
        Language::Perl,
        Language::R,
        Language::Elixir,
        Language::Erlang,
        Language::Haskell,
        Language::OCaml,
        Language::Clojure,
        Language::Zig,
        Language::ObjectiveC,
        Language::PowerShell,
        Language::Batch,
        Language::Ini,
        Language::Makefile,
        Language::CMake,
        Language::Groovy,
        Language::Text,
    ];

    /// Stable lowercase identifier, used in serialisation, context headers
    /// and the capability matrix.
    pub fn as_str(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::TypeScript => "typescript",
            Language::Tsx => "tsx",
            Language::JavaScript => "javascript",
            Language::Jsx => "jsx",
            Language::Python => "python",
            Language::Go => "go",
            Language::Java => "java",
            Language::Kotlin => "kotlin",
            Language::CSharp => "csharp",
            Language::Dart => "dart",
            Language::Swift => "swift",
            Language::Php => "php",
            Language::Ruby => "ruby",
            Language::C => "c",
            Language::Cpp => "cpp",
            Language::Scala => "scala",
            Language::Bash => "bash",
            Language::Css => "css",
            Language::Sql => "sql",
            Language::Protobuf => "protobuf",
            Language::Markdown => "markdown",
            Language::Yaml => "yaml",
            Language::Json => "json",
            Language::Toml => "toml",
            Language::Dockerfile => "dockerfile",
            Language::Html => "html",
            Language::Vue => "vue",
            Language::Svelte => "svelte",
            Language::Xml => "xml",
            Language::GraphQl => "graphql",
            Language::Hcl => "hcl",
            Language::Lua => "lua",
            Language::Perl => "perl",
            Language::R => "r",
            Language::Elixir => "elixir",
            Language::Erlang => "erlang",
            Language::Haskell => "haskell",
            Language::OCaml => "ocaml",
            Language::Clojure => "clojure",
            Language::Zig => "zig",
            Language::ObjectiveC => "objc",
            Language::PowerShell => "powershell",
            Language::Batch => "batch",
            Language::Ini => "ini",
            Language::Makefile => "makefile",
            Language::CMake => "cmake",
            Language::Groovy => "groovy",
            Language::Text => "text",
        }
    }

    /// The analysis tier of this language.
    pub fn tier(self) -> Tier {
        match self {
            Language::Rust
            | Language::TypeScript
            | Language::Tsx
            | Language::JavaScript
            | Language::Jsx
            | Language::Python
            | Language::Go
            | Language::Java
            | Language::Kotlin
            | Language::CSharp => Tier::Exact,
            Language::Dart
            | Language::Swift
            | Language::Php
            | Language::Ruby
            | Language::C
            | Language::Cpp
            | Language::Scala
            | Language::Bash
            | Language::Css => Tier::Structural,
            Language::Sql
            | Language::Protobuf
            | Language::Markdown
            | Language::Yaml
            | Language::Json
            | Language::Toml
            | Language::Dockerfile => Tier::Contract,
            _ => Tier::TextOnly,
        }
    }

    /// Detects the language of a file from its path and, when the path is not
    /// conclusive, its content (shebang line; `.h` C/C++/Objective-C sniff).
    /// Never fails: unrecognised files are [`Language::Text`].
    pub fn detect(path: &RepoPath, text: &str) -> Language {
        match Language::from_path(path) {
            Some(Language::C)
                if path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("h")) =>
            {
                sniff_header(text)
            }
            Some(language) => language,
            None => first_line(text)
                .and_then(Language::from_shebang)
                .unwrap_or(Language::Text),
        }
    }

    /// Detects the language from the file name and extension alone.
    ///
    /// Returns `None` when the path says nothing (no or unknown extension);
    /// `.h` maps to C here, [`Language::detect`] refines it from content.
    pub fn from_path(path: &RepoPath) -> Option<Language> {
        let name = path.file_name();
        if let Some(language) = from_file_name(name) {
            return Some(language);
        }
        let extension = path.extension()?.to_ascii_lowercase();
        from_extension(&extension)
    }

    /// Detects the language from a `#!` interpreter line (`#!/usr/bin/env
    /// python3`, `#!/bin/sh`, `#!/usr/bin/env -S deno run`).
    pub fn from_shebang(line: &str) -> Option<Language> {
        let rest = line.strip_prefix("#!")?.trim();
        let mut words = rest.split_whitespace();
        let mut program = program_name(words.next()?);
        if program == "env" {
            // `env [-S] [-i] [NAME=value]… program`
            program = words
                .find(|w| !w.starts_with('-') && !w.contains('='))
                .map(program_name)?;
        }
        let program = program.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
        Some(match program {
            "python" | "pypy" => Language::Python,
            "node" | "nodejs" | "bun" => Language::JavaScript,
            "deno" | "ts-node" | "tsx" => Language::TypeScript,
            "bash" | "sh" | "zsh" | "ksh" | "dash" | "ash" => Language::Bash,
            "ruby" => Language::Ruby,
            "php" => Language::Php,
            "perl" => Language::Perl,
            "pwsh" | "powershell" => Language::PowerShell,
            "lua" | "luajit" => Language::Lua,
            "Rscript" => Language::R,
            "swift" => Language::Swift,
            "scala" => Language::Scala,
            "groovy" => Language::Groovy,
            "elixir" => Language::Elixir,
            "escript" => Language::Erlang,
            "runhaskell" | "runghc" => Language::Haskell,
            "kotlin" | "kscript" => Language::Kotlin,
            _ => return None,
        })
    }
}

fn program_name(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

fn first_line(text: &str) -> Option<&str> {
    let line = text.lines().next()?;
    line.starts_with("#!").then_some(line)
}

fn from_file_name(name: &str) -> Option<Language> {
    let lower = name.to_ascii_lowercase();
    let language = match lower.as_str() {
        "dockerfile" | "containerfile" => Language::Dockerfile,
        "makefile" | "gnumakefile" => Language::Makefile,
        "cmakelists.txt" => Language::CMake,
        "gemfile" | "rakefile" | "podfile" | "vagrantfile" | "fastfile" | "appfile"
        | "brewfile" | "guardfile" | "capfile" | "dangerfile" | "berksfile" | "thorfile" => {
            Language::Ruby
        }
        "jenkinsfile" => Language::Groovy,
        ".bashrc" | ".bash_profile" | ".bash_aliases" | ".zshrc" | ".zprofile" | ".profile"
        | ".envrc" => Language::Bash,
        "pipfile" | "cargo.lock" | "poetry.lock" | "uv.lock" => Language::Toml,
        _ if lower.starts_with("dockerfile.") || lower.starts_with("containerfile.") => {
            Language::Dockerfile
        }
        _ => return None,
    };
    Some(language)
}

fn from_extension(extension: &str) -> Option<Language> {
    let language = match extension {
        "rs" => Language::Rust,
        "ts" | "mts" | "cts" => Language::TypeScript,
        "tsx" => Language::Tsx,
        "js" | "mjs" | "cjs" => Language::JavaScript,
        "jsx" => Language::Jsx,
        "py" | "pyi" | "pyw" => Language::Python,
        "go" => Language::Go,
        "java" => Language::Java,
        "kt" | "kts" => Language::Kotlin,
        "cs" | "csx" => Language::CSharp,
        "dart" => Language::Dart,
        "swift" => Language::Swift,
        "php" => Language::Php,
        "rb" | "rake" | "gemspec" | "ru" => Language::Ruby,
        "c" | "h" => Language::C,
        "cc" | "cpp" | "cxx" | "c++" | "hpp" | "hh" | "hxx" | "h++" | "ipp" | "tpp" | "inl" => {
            Language::Cpp
        }
        "scala" | "sc" => Language::Scala,
        "sh" | "bash" | "zsh" | "ksh" => Language::Bash,
        "css" => Language::Css,
        "sql" | "psql" | "pgsql" | "ddl" => Language::Sql,
        "proto" => Language::Protobuf,
        "md" | "markdown" | "mdx" | "mkd" => Language::Markdown,
        "yaml" | "yml" => Language::Yaml,
        "json" | "jsonc" | "geojson" | "webmanifest" => Language::Json,
        "toml" => Language::Toml,
        "dockerfile" | "containerfile" => Language::Dockerfile,
        "html" | "htm" | "xhtml" => Language::Html,
        "vue" => Language::Vue,
        "svelte" => Language::Svelte,
        "xml" | "xsd" | "xsl" | "xslt" | "plist" | "csproj" | "fsproj" | "vbproj" | "props"
        | "targets" => Language::Xml,
        "graphql" | "gql" | "graphqls" => Language::GraphQl,
        "tf" | "tfvars" | "hcl" => Language::Hcl,
        "lua" => Language::Lua,
        "pl" | "pm" => Language::Perl,
        "r" => Language::R,
        "ex" | "exs" => Language::Elixir,
        "erl" | "hrl" => Language::Erlang,
        "hs" | "lhs" => Language::Haskell,
        "ml" | "mli" => Language::OCaml,
        "clj" | "cljs" | "cljc" | "edn" => Language::Clojure,
        "zig" => Language::Zig,
        "m" | "mm" => Language::ObjectiveC,
        "ps1" | "psm1" | "psd1" => Language::PowerShell,
        "bat" | "cmd" => Language::Batch,
        "ini" | "cfg" | "conf" | "properties" => Language::Ini,
        "mk" | "mak" => Language::Makefile,
        "cmake" => Language::CMake,
        "groovy" | "gradle" => Language::Groovy,
        "txt" | "text" | "rst" | "adoc" | "asciidoc" => Language::Text,
        _ => return None,
    };
    Some(language)
}

/// `.h` is shared by C, C++ and Objective-C; look at the first 64 KiB.
fn sniff_header(text: &str) -> Language {
    let head = crate::text::prefix(text, 64 * 1024);
    if head.contains("@interface") || head.contains("#import ") || head.contains("@protocol") {
        return Language::ObjectiveC;
    }
    const CPP_MARKERS: [&str; 10] = [
        "class ",
        "namespace ",
        "template<",
        "template <",
        "std::",
        "public:",
        "private:",
        "nullptr",
        "constexpr",
        "virtual ",
    ];
    if CPP_MARKERS.iter().any(|marker| head.contains(marker)) {
        Language::Cpp
    } else {
        Language::C
    }
}

impl fmt::Display for Language {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when parsing an unknown language identifier.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown language identifier `{0}`")]
pub struct UnknownLanguage(pub String);

impl FromStr for Language {
    type Err = UnknownLanguage;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Language::ALL
            .iter()
            .copied()
            .find(|language| language.as_str() == s)
            .ok_or_else(|| UnknownLanguage(s.to_owned()))
    }
}

impl Serialize for Language {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Language {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// A schema-level flavour of a structure file, detected from its content
/// (and, for Compose, its name). It decides which contract symbols are
/// extracted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dialect {
    /// OpenAPI 3.x or Swagger 2.0 (`openapi:` / `swagger:` root key).
    OpenApi,
    /// AsyncAPI 2.x / 3.x (`asyncapi:` root key).
    AsyncApi,
    /// Docker Compose (`compose*.yaml`, `docker-compose*.yml`).
    Compose,
    /// Kubernetes manifests (`apiVersion` + `kind` root keys).
    Kubernetes,
}

impl Dialect {
    /// Stable lowercase identifier.
    pub fn as_str(self) -> &'static str {
        match self {
            Dialect::OpenApi => "openapi",
            Dialect::AsyncApi => "asyncapi",
            Dialect::Compose => "compose",
            Dialect::Kubernetes => "kubernetes",
        }
    }
}

/// Whether a YAML file name follows the Compose naming convention.
pub(crate) fn is_compose_name(path: &RepoPath) -> bool {
    let name = path.file_name().to_ascii_lowercase();
    let stem = name
        .strip_suffix(".yaml")
        .or_else(|| name.strip_suffix(".yml"));
    stem.is_some_and(|stem| {
        stem == "compose"
            || stem == "docker-compose"
            || stem.starts_with("compose.")
            || stem.starts_with("docker-compose.")
            || stem.starts_with("docker-compose-")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(p: &str) -> RepoPath {
        RepoPath::new(p).unwrap()
    }

    #[test]
    fn detects_by_extension_and_name() {
        let cases = [
            ("src/lib.rs", Language::Rust),
            ("a/b.tsx", Language::Tsx),
            ("a/b.mts", Language::TypeScript),
            ("a/b.cjs", Language::JavaScript),
            ("a/b.jsx", Language::Jsx),
            ("x.PY", Language::Python),
            ("Dockerfile", Language::Dockerfile),
            ("deploy/Dockerfile.prod", Language::Dockerfile),
            ("api.dockerfile", Language::Dockerfile),
            ("Makefile", Language::Makefile),
            ("Gemfile", Language::Ruby),
            ("Cargo.lock", Language::Toml),
            ("openapi.yml", Language::Yaml),
            ("db/001_init.sql", Language::Sql),
            ("api/v1/billing.proto", Language::Protobuf),
            ("README.md", Language::Markdown),
            ("main.tf", Language::Hcl),
            ("notes", Language::Text),
        ];
        for (p, expected) in cases {
            assert_eq!(Language::detect(&path(p), ""), expected, "{p}");
        }
    }

    #[test]
    fn detects_by_shebang() {
        let cases = [
            ("#!/usr/bin/env python3\nprint(1)\n", Language::Python),
            ("#!/bin/sh\necho hi\n", Language::Bash),
            (
                "#!/usr/bin/env -S deno run --allow-net\n",
                Language::TypeScript,
            ),
            ("#!/usr/bin/env node\n", Language::JavaScript),
            ("#!/usr/bin/ruby -w\n", Language::Ruby),
            ("#!/usr/bin/env FOO=1 bash\n", Language::Bash),
            ("#!/usr/bin/unknown\n", Language::Text),
            ("no shebang\n", Language::Text),
        ];
        for (text, expected) in cases {
            assert_eq!(
                Language::detect(&path("bin/tool"), text),
                expected,
                "{text}"
            );
        }
        // The extension wins over the shebang.
        assert_eq!(
            Language::detect(&path("tool.rb"), "#!/usr/bin/env python\n"),
            Language::Ruby
        );
    }

    #[test]
    fn sniffs_headers() {
        assert_eq!(
            Language::detect(&path("a.h"), "int f(void);\n"),
            Language::C
        );
        assert_eq!(
            Language::detect(&path("a.h"), "namespace x { class A {}; }\n"),
            Language::Cpp
        );
        assert_eq!(
            Language::detect(&path("a.h"), "@interface Foo : NSObject\n@end\n"),
            Language::ObjectiveC
        );
    }

    #[test]
    fn tiers_follow_the_matrix() {
        for language in [Language::Rust, Language::Kotlin, Language::CSharp] {
            assert_eq!(language.tier(), Tier::Exact);
        }
        for language in [Language::Dart, Language::Cpp, Language::Scala] {
            assert_eq!(language.tier(), Tier::Structural);
        }
        for language in [Language::Sql, Language::Dockerfile, Language::Toml] {
            assert_eq!(language.tier(), Tier::Contract);
        }
        for language in [Language::Html, Language::Lua, Language::Text] {
            assert_eq!(language.tier(), Tier::TextOnly);
        }
    }

    #[test]
    fn identifiers_round_trip() {
        for language in Language::ALL {
            assert_eq!(language.as_str().parse::<Language>().unwrap(), *language);
        }
        assert!("klingon".parse::<Language>().is_err());
    }

    #[test]
    fn compose_names() {
        assert!(is_compose_name(&path("docker-compose.yml")));
        assert!(is_compose_name(&path("deploy/compose.yaml")));
        assert!(is_compose_name(&path("docker-compose.override.yml")));
        assert!(!is_compose_name(&path("composer.json")));
        assert!(!is_compose_name(&path("config.yaml")));
    }
}
