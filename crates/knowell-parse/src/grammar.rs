//! tree-sitter grammars and their queries, compiled once per process.
//!
//! Queries live in `queries/<grammar>/{symbols,imports}.scm` and are embedded
//! at build time. Capture conventions (see the crate README):
//!
//! - `@definition.<kind>` — the declaration node; `<kind>` is a
//!   [`crate::SymbolKind`] identifier.
//! - `@name` — its name; `@receiver` — an explicit container name (Go
//!   receiver type, SQL table of `ALTER TABLE … ADD COLUMN`); `@body` — the
//!   body that signatures stop at and skeletons elide.
//! - `@import` — an import statement; `@import.source` — its specifier.
//! - Captures starting with `_` are only used by predicates.

use std::sync::OnceLock;

use tree_sitter::{Language as TsLanguage, Query};

use crate::language::Language;

/// A grammar Knowell bundles. Several languages may share one (JSX uses the
/// JavaScript grammar).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Grammar {
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Python,
    Go,
    Java,
    Kotlin,
    CSharp,
    Dart,
    Swift,
    Php,
    Ruby,
    C,
    Cpp,
    Scala,
    Bash,
    Css,
    Sql,
    Protobuf,
    Markdown,
    Yaml,
    Json,
    Toml,
}

const GRAMMAR_COUNT: usize = 24;

/// Every bundled grammar (used by tests to compile every query).
#[cfg(test)]
pub(crate) const ALL_GRAMMARS: [Grammar; GRAMMAR_COUNT] = [
    Grammar::Rust,
    Grammar::TypeScript,
    Grammar::Tsx,
    Grammar::JavaScript,
    Grammar::Python,
    Grammar::Go,
    Grammar::Java,
    Grammar::Kotlin,
    Grammar::CSharp,
    Grammar::Dart,
    Grammar::Swift,
    Grammar::Php,
    Grammar::Ruby,
    Grammar::C,
    Grammar::Cpp,
    Grammar::Scala,
    Grammar::Bash,
    Grammar::Css,
    Grammar::Sql,
    Grammar::Protobuf,
    Grammar::Markdown,
    Grammar::Yaml,
    Grammar::Json,
    Grammar::Toml,
];

impl Grammar {
    /// The grammar used for `language`, if it has one.
    pub(crate) fn for_language(language: Language) -> Option<Grammar> {
        Some(match language {
            Language::Rust => Grammar::Rust,
            Language::TypeScript => Grammar::TypeScript,
            Language::Tsx => Grammar::Tsx,
            Language::JavaScript | Language::Jsx => Grammar::JavaScript,
            Language::Python => Grammar::Python,
            Language::Go => Grammar::Go,
            Language::Java => Grammar::Java,
            Language::Kotlin => Grammar::Kotlin,
            Language::CSharp => Grammar::CSharp,
            Language::Dart => Grammar::Dart,
            Language::Swift => Grammar::Swift,
            Language::Php => Grammar::Php,
            Language::Ruby => Grammar::Ruby,
            Language::C => Grammar::C,
            Language::Cpp => Grammar::Cpp,
            Language::Scala => Grammar::Scala,
            Language::Bash => Grammar::Bash,
            Language::Css => Grammar::Css,
            Language::Sql => Grammar::Sql,
            Language::Protobuf => Grammar::Protobuf,
            Language::Markdown => Grammar::Markdown,
            Language::Yaml => Grammar::Yaml,
            Language::Json => Grammar::Json,
            Language::Toml => Grammar::Toml,
            _ => return None,
        })
    }

    pub(crate) fn ts_language(self) -> TsLanguage {
        match self {
            Grammar::Rust => tree_sitter_rust::LANGUAGE.into(),
            Grammar::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Grammar::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Grammar::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Grammar::Python => tree_sitter_python::LANGUAGE.into(),
            Grammar::Go => tree_sitter_go::LANGUAGE.into(),
            Grammar::Java => tree_sitter_java::LANGUAGE.into(),
            Grammar::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
            Grammar::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            Grammar::Dart => tree_sitter_dart::LANGUAGE.into(),
            Grammar::Swift => tree_sitter_swift::LANGUAGE.into(),
            Grammar::Php => tree_sitter_php::LANGUAGE_PHP.into(),
            Grammar::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            Grammar::C => tree_sitter_c::LANGUAGE.into(),
            Grammar::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Grammar::Scala => tree_sitter_scala::LANGUAGE.into(),
            Grammar::Bash => tree_sitter_bash::LANGUAGE.into(),
            Grammar::Css => tree_sitter_css::LANGUAGE.into(),
            Grammar::Sql => tree_sitter_sequel::LANGUAGE.into(),
            Grammar::Protobuf => tree_sitter_proto::LANGUAGE.into(),
            Grammar::Markdown => tree_sitter_md::LANGUAGE.into(),
            Grammar::Yaml => tree_sitter_yaml::LANGUAGE.into(),
            Grammar::Json => tree_sitter_json::LANGUAGE.into(),
            Grammar::Toml => tree_sitter_toml_ng::LANGUAGE.into(),
        }
    }

    /// `(symbols query, imports query)` sources; empty when the grammar's
    /// extraction is written in Rust (YAML, JSON, TOML).
    fn query_sources(self) -> (&'static str, &'static str) {
        match self {
            Grammar::Rust => (
                include_str!("../queries/rust/symbols.scm"),
                include_str!("../queries/rust/imports.scm"),
            ),
            Grammar::TypeScript | Grammar::Tsx => (
                include_str!("../queries/typescript/symbols.scm"),
                include_str!("../queries/typescript/imports.scm"),
            ),
            Grammar::JavaScript => (
                include_str!("../queries/javascript/symbols.scm"),
                include_str!("../queries/javascript/imports.scm"),
            ),
            Grammar::Python => (
                include_str!("../queries/python/symbols.scm"),
                include_str!("../queries/python/imports.scm"),
            ),
            Grammar::Go => (
                include_str!("../queries/go/symbols.scm"),
                include_str!("../queries/go/imports.scm"),
            ),
            Grammar::Java => (
                include_str!("../queries/java/symbols.scm"),
                include_str!("../queries/java/imports.scm"),
            ),
            Grammar::Kotlin => (
                include_str!("../queries/kotlin/symbols.scm"),
                include_str!("../queries/kotlin/imports.scm"),
            ),
            Grammar::CSharp => (
                include_str!("../queries/csharp/symbols.scm"),
                include_str!("../queries/csharp/imports.scm"),
            ),
            Grammar::Dart => (
                include_str!("../queries/dart/symbols.scm"),
                include_str!("../queries/dart/imports.scm"),
            ),
            Grammar::Swift => (
                include_str!("../queries/swift/symbols.scm"),
                include_str!("../queries/swift/imports.scm"),
            ),
            Grammar::Php => (
                include_str!("../queries/php/symbols.scm"),
                include_str!("../queries/php/imports.scm"),
            ),
            Grammar::Ruby => (
                include_str!("../queries/ruby/symbols.scm"),
                include_str!("../queries/ruby/imports.scm"),
            ),
            Grammar::C => (
                include_str!("../queries/c/symbols.scm"),
                include_str!("../queries/c/imports.scm"),
            ),
            Grammar::Cpp => (
                include_str!("../queries/cpp/symbols.scm"),
                include_str!("../queries/cpp/imports.scm"),
            ),
            Grammar::Scala => (
                include_str!("../queries/scala/symbols.scm"),
                include_str!("../queries/scala/imports.scm"),
            ),
            Grammar::Bash => (
                include_str!("../queries/bash/symbols.scm"),
                include_str!("../queries/bash/imports.scm"),
            ),
            Grammar::Css => (
                include_str!("../queries/css/symbols.scm"),
                include_str!("../queries/css/imports.scm"),
            ),
            Grammar::Sql => (include_str!("../queries/sql/symbols.scm"), ""),
            Grammar::Protobuf => (
                include_str!("../queries/proto/symbols.scm"),
                include_str!("../queries/proto/imports.scm"),
            ),
            Grammar::Markdown => (include_str!("../queries/markdown/symbols.scm"), ""),
            Grammar::Yaml | Grammar::Json | Grammar::Toml => ("", ""),
        }
    }
}

/// A grammar with its compiled queries.
pub(crate) struct Compiled {
    pub(crate) language: TsLanguage,
    pub(crate) symbols: Option<Query>,
    pub(crate) imports: Option<Query>,
}

type Slot = OnceLock<Result<Compiled, String>>;

static CACHE: [Slot; GRAMMAR_COUNT] = [const { OnceLock::new() }; GRAMMAR_COUNT];

/// The compiled grammar, built on first use and cached for the process.
pub(crate) fn compiled(grammar: Grammar) -> Result<&'static Compiled, String> {
    let slot = CACHE
        .get(grammar as usize)
        .ok_or_else(|| format!("no cache slot for grammar {grammar:?}"))?;
    slot.get_or_init(|| compile(grammar))
        .as_ref()
        .map_err(Clone::clone)
}

fn compile(grammar: Grammar) -> Result<Compiled, String> {
    let language = grammar.ts_language();
    let (symbols, imports) = grammar.query_sources();
    let build = |kind: &str, source: &str| -> Result<Option<Query>, String> {
        if source.trim().is_empty() {
            return Ok(None);
        }
        Query::new(&language, source)
            .map(Some)
            .map_err(|e| format!("{grammar:?} {kind} query: {e}"))
    };
    Ok(Compiled {
        symbols: build("symbols", symbols)?,
        imports: build("imports", imports)?,
        language,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_grammar_and_query_compiles() {
        for grammar in ALL_GRAMMARS {
            let compiled = compiled(grammar).unwrap_or_else(|e| panic!("{e}"));
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&compiled.language)
                .unwrap_or_else(|e| panic!("{grammar:?}: {e}"));
        }
    }

    #[test]
    fn grammars_are_indexed_consistently() {
        for (index, grammar) in ALL_GRAMMARS.iter().enumerate() {
            assert_eq!(*grammar as usize, index);
        }
    }
}
