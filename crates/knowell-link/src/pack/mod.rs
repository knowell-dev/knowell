//! Rule packs: declarative contract extraction (TOML + tree-sitter queries).
//!
//! A pack is a directory `packs/<name>/` with a `pack.toml`, query files under
//! `<language>/*.scm` and fixtures under `tests/`. [`Pack::load_dir`] reads a
//! pack from disk and [`PackSet::builtin`] loads the packs bundled with the
//! crate. Loading validates everything that can be checked statically: the
//! schema, versions, that every query compiles against the grammar of every
//! language its rule lists, and that templates only use captures that exist.

mod builtin;
mod manifest;
pub(crate) mod template;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::str::FromStr;

use globset::{Glob, GlobBuilder, GlobMatcher, GlobSet, GlobSetBuilder};
use knowell_graph::{ContractKind, EvidenceType};
use knowell_parse::Language;
use knowell_parse::tree_sitter::Query;

use crate::error::LinkError;
use crate::model::{Role, parse_contract_kind};
use crate::normalize::Normalizer;
use manifest::{BindingSpec, DetectSpec, ExtractorSpec, PackManifest, RuleSpec};
use template::Template;

/// Built-in variables every template may use.
pub(crate) const PATH_VARIABLES: &[&str] = &["path.dir", "path.stem", "path.name", "route"];

/// Framework detection hints. A pack (or rule) is active for a project when
/// any hint matches; a pack without hints is always active.
#[derive(Debug, Clone, Default)]
pub(crate) struct Detect {
    pub(crate) always: bool,
    /// Ecosystem -> dependency names. A name ending in `/` or `*` matches by
    /// prefix; other names match exactly (case-insensitively for python and
    /// nuget).
    pub(crate) deps: BTreeMap<Ecosystem, Vec<String>>,
    /// Import specifier prefixes (file level).
    pub(crate) imports: Vec<String>,
    /// Globs over project paths.
    pub(crate) files: Option<GlobSet>,
}

impl Detect {
    pub(crate) fn is_empty(&self) -> bool {
        !self.always && self.deps.is_empty() && self.imports.is_empty() && self.files.is_none()
    }
}

/// A package ecosystem whose manifests are read for detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Ecosystem {
    Npm,
    Go,
    Python,
    Pub,
    Cargo,
    Maven,
    Nuget,
}

impl Ecosystem {
    pub(crate) fn case_insensitive(self) -> bool {
        matches!(self, Self::Python | Self::Nuget | Self::Maven)
    }
}

/// Which convention derives a route from a file path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteConvention {
    /// Next.js app router: `app/**/route.ts`.
    NextjsApp,
    /// Next.js pages router API routes: `pages/api/**`.
    NextjsPages,
}

/// How the shape of a final key is validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyShape {
    /// Contains a separator (`.`, `-`, `_`, `:`, `/`) and no whitespace.
    Dotted,
    /// An upper-case environment-style name (`[A-Z][A-Z0-9_]*`).
    UpperSnake,
}

/// Post-processing of a rendered key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PostProcess {
    /// The key is SQL text; tables after `FROM`/`JOIN` are read and tables
    /// after `INSERT INTO`/`UPDATE`/`DELETE FROM` are written.
    Sql,
}

/// A lookup of a variable in a binding table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Lookup {
    /// Binding id (within the pack).
    pub(crate) binding: String,
    /// Capture whose text is the name looked up, or `=text` for a fixed name.
    pub(crate) by: String,
}

/// Where a binding is visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum BindingScope {
    /// The enclosing symbol (a function body).
    Symbol,
    /// The file.
    File,
    /// The whole project (module-level constants).
    Project,
}

/// A compiled extraction rule.
#[derive(Debug)]
pub(crate) struct Rule {
    pub(crate) id: String,
    pub(crate) kind: ContractKind,
    pub(crate) role: Role,
    pub(crate) key: Template,
    pub(crate) attrs: Vec<(String, Template)>,
    pub(crate) symbol: Option<String>,
    pub(crate) anchor: Option<String>,
    pub(crate) evidence: EvidenceType,
    pub(crate) normalizer: Normalizer,
    pub(crate) resolve: BTreeSet<String>,
    pub(crate) lookups: BTreeMap<String, Lookup>,
    pub(crate) defaults: BTreeMap<String, Template>,
    pub(crate) tokens: Vec<(String, Template)>,
    pub(crate) require: Vec<String>,
    pub(crate) where_equal: Vec<(String, Template)>,
    pub(crate) unless: Vec<(String, GlobMatcher)>,
    pub(crate) files: Option<GlobSet>,
    pub(crate) exclude: Option<GlobSet>,
    pub(crate) route: Option<RouteConvention>,
    pub(crate) glob_all: bool,
    pub(crate) key_shape: Option<KeyShape>,
    pub(crate) detect: Option<Detect>,
    pub(crate) requires_rule: Option<String>,
    pub(crate) postprocess: Option<PostProcess>,
    pub(crate) queries: BTreeMap<Language, Query>,
}

/// A compiled binding: names bound to values (router prefixes, constants,
/// client variables) that rules look up.
#[derive(Debug)]
pub(crate) struct Binding {
    pub(crate) id: String,
    pub(crate) scope: BindingScope,
    pub(crate) name: String,
    pub(crate) value: Template,
    pub(crate) resolve: BTreeSet<String>,
    pub(crate) constant: bool,
    pub(crate) files: Option<GlobSet>,
    pub(crate) queries: BTreeMap<Language, Query>,
}

/// The structured (Rust) extractors packs can enable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum StructuredKind {
    /// OpenAPI / Swagger documents: paths x methods.
    OpenApi,
    /// AsyncAPI documents: channels.
    AsyncApi,
    /// JSON Schema files describing one event (`title` = topic).
    EventSchema,
    /// Protocol Buffers services and RPCs.
    Proto,
    /// docker-compose `environment` names.
    ComposeEnv,
    /// Kubernetes container `env` names.
    KubernetesEnv,
    /// docker-compose services and Kubernetes workloads / services.
    Infra,
    /// Nested locale JSON files.
    LocaleJson,
    /// Flutter ARB files.
    Arb,
    /// Prisma schema models.
    Prisma,
    /// SQL DDL in migration files (CREATE / ALTER / DROP TABLE).
    SqlDdl,
}

impl StructuredKind {
    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "openapi" => Self::OpenApi,
            "asyncapi" => Self::AsyncApi,
            "event-schema" => Self::EventSchema,
            "proto" => Self::Proto,
            "compose-env" => Self::ComposeEnv,
            "kubernetes-env" => Self::KubernetesEnv,
            "infra" => Self::Infra,
            "locale-json" => Self::LocaleJson,
            "arb" => Self::Arb,
            "prisma" => Self::Prisma,
            "sql-ddl" => Self::SqlDdl,
            _ => return None,
        })
    }
}

/// A compiled structured-extractor entry.
#[derive(Debug)]
pub(crate) struct Extractor {
    pub(crate) id: String,
    pub(crate) kind: StructuredKind,
    pub(crate) files: Option<GlobSet>,
    pub(crate) exclude: Option<GlobSet>,
}

/// A loaded, validated rule pack.
#[derive(Debug)]
pub struct Pack {
    name: String,
    version: String,
    description: String,
    languages: Vec<String>,
    limits: String,
    pub(crate) detect: Detect,
    pub(crate) rules: Vec<Rule>,
    pub(crate) bindings: Vec<Binding>,
    pub(crate) extractors: Vec<Extractor>,
    fixtures: Vec<(String, String)>,
}

impl Pack {
    /// Pack name (directory name, `[a-z0-9-]`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Semantic version (`MAJOR.MINOR.PATCH`).
    pub fn version(&self) -> &str {
        &self.version
    }

    /// `name@version`, recorded on every extraction.
    pub fn id(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }

    /// One-line description.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Languages the pack covers, as declared.
    pub fn languages(&self) -> &[String] {
        &self.languages
    }

    /// Known limits (free text, Markdown).
    pub fn limits(&self) -> &str {
        &self.limits
    }

    /// Ids of the rules, bindings and structured extractors, in pack order.
    pub fn entry_ids(&self) -> Vec<String> {
        self.rules
            .iter()
            .map(|r| r.id.clone())
            .chain(self.bindings.iter().map(|b| b.id.clone()))
            .chain(self.extractors.iter().map(|e| e.id.clone()))
            .collect()
    }

    /// Fixture files under `tests/` as `(file name, content)`, sorted.
    pub fn fixtures(&self) -> &[(String, String)] {
        &self.fixtures
    }

    /// Loads a pack from a directory containing `pack.toml`.
    ///
    /// # Errors
    /// [`LinkError::PackIo`] when a file cannot be read, and the validation
    /// errors of [`Pack::from_files`].
    pub fn load_dir(dir: &Path) -> Result<Self, LinkError> {
        let label = dir.display().to_string();
        let mut files = BTreeMap::new();
        collect_files(dir, dir, &label, 0, &mut files)?;
        Self::from_files(&label, &files)
    }

    /// Loads a pack from its files, keyed by `/`-separated path relative to
    /// the pack directory (`pack.toml`, `typescript/routes.scm`,
    /// `tests/pos-x.ts`).
    ///
    /// # Errors
    /// [`LinkError::PackManifest`] for TOML or schema errors,
    /// [`LinkError::InvalidPack`] / [`LinkError::InvalidRule`] for semantic
    /// errors and [`LinkError::QueryCompile`] when a query does not compile.
    pub fn from_files(label: &str, files: &BTreeMap<String, String>) -> Result<Self, LinkError> {
        let manifest_text = files.get("pack.toml").ok_or_else(|| LinkError::PackIo {
            pack: label.to_owned(),
            path: "pack.toml".to_owned(),
            message: "file not found".to_owned(),
        })?;
        let manifest: PackManifest =
            toml::from_str(manifest_text).map_err(|e| LinkError::PackManifest {
                pack: label.to_owned(),
                message: e.to_string(),
            })?;
        compile(manifest, files)
    }
}

fn collect_files(
    root: &Path,
    dir: &Path,
    label: &str,
    depth: usize,
    out: &mut BTreeMap<String, String>,
) -> Result<(), LinkError> {
    if depth > 3 {
        return Ok(());
    }
    let io = |path: &Path, e: std::io::Error| LinkError::PackIo {
        pack: label.to_owned(),
        path: path.display().to_string(),
        message: e.to_string(),
    };
    let entries = std::fs::read_dir(dir).map_err(|e| io(dir, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| io(dir, e))?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|e| io(&path, e))?;
        if file_type.is_dir() {
            collect_files(root, &path, label, depth + 1, out)?;
        } else if file_type.is_file() {
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let key = relative
                .components()
                .filter_map(|c| c.as_os_str().to_str())
                .collect::<Vec<_>>()
                .join("/");
            let text = std::fs::read_to_string(&path).map_err(|e| io(&path, e))?;
            out.insert(key, text);
        }
    }
    Ok(())
}

/// A set of packs with unique names, applied together.
#[derive(Debug, Default)]
pub struct PackSet {
    packs: Vec<Pack>,
}

impl PackSet {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// The packs bundled with this crate (the `packs/` directory of the
    /// repository), validated.
    ///
    /// # Errors
    /// Any validation error of a bundled pack (a bug; covered by tests).
    pub fn builtin() -> Result<Self, LinkError> {
        let mut set = Self::new();
        for (name, files) in builtin::pack_files() {
            set.insert(Pack::from_files(name, &files)?)?;
        }
        Ok(set)
    }

    /// Adds a pack.
    ///
    /// # Errors
    /// [`LinkError::DuplicatePack`] when a pack with the same name exists.
    pub fn insert(&mut self, pack: Pack) -> Result<(), LinkError> {
        if self.packs.iter().any(|p| p.name == pack.name) {
            return Err(LinkError::DuplicatePack(pack.name));
        }
        self.packs.push(pack);
        self.packs.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(())
    }

    /// The packs, sorted by name.
    pub fn packs(&self) -> &[Pack] {
        &self.packs
    }

    /// Looks a pack up by name.
    pub fn get(&self, name: &str) -> Option<&Pack> {
        self.packs.iter().find(|p| p.name == name)
    }
}

fn invalid(pack: &str, message: impl Into<String>) -> LinkError {
    LinkError::InvalidPack {
        pack: pack.to_owned(),
        message: message.into(),
    }
}

fn invalid_entry(pack: &str, entry: &str, message: impl Into<String>) -> LinkError {
    LinkError::InvalidRule {
        pack: pack.to_owned(),
        entry: entry.to_owned(),
        message: message.into(),
    }
}

fn valid_version(version: &str) -> bool {
    let parts: Vec<&str> = version.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 6 && p.chars().all(|c| c.is_ascii_digit()))
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !id.starts_with('-')
}

fn compile(manifest: PackManifest, files: &BTreeMap<String, String>) -> Result<Pack, LinkError> {
    let name = manifest.name.clone();
    if !valid_id(&name) {
        return Err(invalid(
            &name,
            "name must be 1-64 characters of `a-z`, `0-9` and `-`",
        ));
    }
    if !valid_version(&manifest.version) {
        return Err(invalid(
            &name,
            format!("version `{}` must be MAJOR.MINOR.PATCH", manifest.version),
        ));
    }
    if manifest.rules.is_empty() && manifest.bindings.is_empty() && manifest.extractors.is_empty() {
        return Err(invalid(
            &name,
            "a pack needs at least one rule, binding or extractor",
        ));
    }
    let detect = compile_detect(&name, "detect", manifest.detect.as_ref())?;
    let mut ids = BTreeSet::new();

    let mut bindings = Vec::new();
    for spec in &manifest.bindings {
        if !valid_id(&spec.id) || !ids.insert(spec.id.clone()) {
            return Err(invalid_entry(
                &name,
                &spec.id,
                "ids must be unique and use `a-z0-9-`",
            ));
        }
        bindings.push(compile_binding(&name, spec, files)?);
    }
    let binding_ids: BTreeMap<String, BindingScope> =
        bindings.iter().map(|b| (b.id.clone(), b.scope)).collect();

    let mut rules = Vec::new();
    for spec in &manifest.rules {
        if !valid_id(&spec.id) || !ids.insert(spec.id.clone()) {
            return Err(invalid_entry(
                &name,
                &spec.id,
                "ids must be unique and use `a-z0-9-`",
            ));
        }
        rules.push(compile_rule(&name, spec, files, &binding_ids)?);
    }
    let rule_ids: BTreeSet<String> = rules.iter().map(|r| r.id.clone()).collect();
    for rule in &rules {
        if let Some(required) = &rule.requires_rule
            && !rule_ids.contains(required)
        {
            return Err(invalid_entry(
                &name,
                &rule.id,
                format!("`requires_rule` names unknown rule `{required}`"),
            ));
        }
    }

    let mut extractors = Vec::new();
    for spec in &manifest.extractors {
        if !valid_id(&spec.id) || !ids.insert(spec.id.clone()) {
            return Err(invalid_entry(
                &name,
                &spec.id,
                "ids must be unique and use `a-z0-9-`",
            ));
        }
        extractors.push(compile_extractor(&name, spec)?);
    }

    let fixtures = files
        .iter()
        .filter_map(|(path, text)| {
            path.strip_prefix("tests/")
                .filter(|rest| !rest.contains('/'))
                .map(|rest| (rest.to_owned(), text.clone()))
        })
        .collect();

    Ok(Pack {
        name,
        version: manifest.version,
        description: manifest.description,
        languages: manifest.languages,
        limits: manifest.limits,
        detect,
        rules,
        bindings,
        extractors,
        fixtures,
    })
}

fn compile_detect(pack: &str, entry: &str, spec: Option<&DetectSpec>) -> Result<Detect, LinkError> {
    let Some(spec) = spec else {
        return Ok(Detect::default());
    };
    let mut deps = BTreeMap::new();
    for (ecosystem, names) in [
        (Ecosystem::Npm, &spec.npm),
        (Ecosystem::Go, &spec.go),
        (Ecosystem::Python, &spec.python),
        (Ecosystem::Pub, &spec.pub_),
        (Ecosystem::Cargo, &spec.cargo),
        (Ecosystem::Maven, &spec.maven),
        (Ecosystem::Nuget, &spec.nuget),
    ] {
        if !names.is_empty() {
            deps.insert(ecosystem, names.clone());
        }
    }
    Ok(Detect {
        always: spec.always,
        deps,
        imports: spec.imports.clone(),
        files: globs(pack, entry, &spec.files)?,
    })
}

fn globs(pack: &str, entry: &str, patterns: &[String]) -> Result<Option<GlobSet>, LinkError> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|e| invalid_entry(pack, entry, format!("invalid glob `{pattern}`: {e}")))?;
        builder.add(glob);
    }
    builder
        .build()
        .map(Some)
        .map_err(|e| invalid_entry(pack, entry, format!("invalid globs: {e}")))
}

/// Query directories tried for a language, most specific first: TSX and
/// JavaScript fall back to `typescript/`, JSX to `javascript/` then
/// `typescript/`; other languages use their own directory.
pub(crate) fn query_dirs(language: Language) -> Vec<&'static str> {
    match language {
        Language::Tsx => vec!["tsx", "typescript"],
        Language::JavaScript => vec!["javascript", "typescript"],
        Language::Jsx => vec!["jsx", "javascript", "typescript"],
        other => vec![other.as_str()],
    }
}

fn compile_queries(
    pack: &str,
    entry: &str,
    query: &str,
    languages: &[String],
    files: &BTreeMap<String, String>,
) -> Result<BTreeMap<Language, Query>, LinkError> {
    if languages.is_empty() {
        return Err(invalid_entry(pack, entry, "`languages` must not be empty"));
    }
    if query.contains("..") || query.starts_with('/') || query.contains('\\') {
        return Err(invalid_entry(
            pack,
            entry,
            format!("query path `{query}` must be a plain file name"),
        ));
    }
    let mut out = BTreeMap::new();
    for name in languages {
        let language = Language::from_str(name)
            .map_err(|_| invalid_entry(pack, entry, format!("unknown language `{name}`")))?;
        let grammar = knowell_parse::ts_language(language).ok_or_else(|| {
            invalid_entry(
                pack,
                entry,
                format!("language `{name}` has no tree-sitter grammar"),
            )
        })?;
        let (path, source) = query_dirs(language)
            .iter()
            .find_map(|dir| {
                let path = format!("{dir}/{query}");
                files.get(&path).map(|text| (path, text))
            })
            .ok_or_else(|| {
                invalid_entry(
                    pack,
                    entry,
                    format!(
                        "query `{query}` not found for {name} (looked in {})",
                        query_dirs(language)
                            .iter()
                            .map(|d| format!("{d}/"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                )
            })?;
        if let Some(message) = suspicious_escape(source) {
            return Err(LinkError::QueryCompile {
                pack: pack.to_owned(),
                entry: entry.to_owned(),
                query: path,
                language: name.clone(),
                message,
            });
        }
        let compiled = Query::new(&grammar, source).map_err(|e| LinkError::QueryCompile {
            pack: pack.to_owned(),
            entry: entry.to_owned(),
            query: path.clone(),
            language: name.clone(),
            message: format!(
                "row {}, column {}: {:?}: {}",
                e.row + 1,
                e.column + 1,
                e.kind,
                e.message
            ),
        })?;
        out.insert(language, compiled);
    }
    Ok(out)
}

/// tree-sitter query strings only know the escapes `\\`, `\"`, `\n`, `\r`,
/// `\t` and `\0`; any other `\x` silently becomes `x`, so a regex written as
/// `"\s"` would match the letter `s`. Such escapes are rejected with a hint.
fn suspicious_escape(source: &str) -> Option<String> {
    for (line_index, line) in source.lines().enumerate() {
        let mut in_string = false;
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            match (in_string, c) {
                // A comment runs to the end of the line.
                (false, ';') => break,
                (false, '"') => in_string = true,
                (true, '"') => in_string = false,
                (true, '\\') => match chars.next() {
                    Some('\\' | '"' | 'n' | 'r' | 't' | '0') | None => {}
                    Some(other) => {
                        return Some(format!(
                            "line {}: `\\{other}` in a query string becomes `{other}`; write `\\\\{other}` for a regex escape",
                            line_index + 1
                        ));
                    }
                },
                _ => {}
            }
        }
    }
    None
}

/// Capture names common to every compiled language of an entry.
fn capture_names(queries: &BTreeMap<Language, Query>) -> BTreeSet<String> {
    let mut sets = queries.values().map(|q| {
        q.capture_names()
            .iter()
            .map(|s| (*s).to_owned())
            .collect::<BTreeSet<String>>()
    });
    let Some(first) = sets.next() else {
        return BTreeSet::new();
    };
    sets.fold(first, |acc, set| acc.intersection(&set).cloned().collect())
}

fn template(pack: &str, entry: &str, field: &str, source: &str) -> Result<Template, LinkError> {
    Template::parse(source).map_err(|e| invalid_entry(pack, entry, format!("`{field}`: {e}")))
}

fn compile_rule(
    pack: &str,
    spec: &RuleSpec,
    files: &BTreeMap<String, String>,
    bindings: &BTreeMap<String, BindingScope>,
) -> Result<Rule, LinkError> {
    let id = spec.id.as_str();
    let kind = parse_contract_kind(&spec.kind)
        .ok_or_else(|| invalid_entry(pack, id, format!("unknown contract kind `{}`", spec.kind)))?;
    let role = Role::parse(&spec.role)
        .ok_or_else(|| invalid_entry(pack, id, format!("unknown role `{}`", spec.role)))?;
    let evidence = match spec.evidence.as_deref() {
        None | Some("syntactic") => EvidenceType::Syntactic,
        Some("heuristic") => EvidenceType::Heuristic,
        Some("contract") if role == Role::Definition => EvidenceType::ContractDerived,
        Some("contract") => {
            return Err(invalid_entry(
                pack,
                id,
                "evidence `contract` is only valid for `role = \"definition\"` (schema documents)",
            ));
        }
        Some(other) => {
            return Err(invalid_entry(
                pack,
                id,
                format!("evidence `{other}` must be `syntactic`, `heuristic` or `contract`"),
            ));
        }
    };
    let normalizer = match spec.normalize.as_deref() {
        Some(name) => Normalizer::parse(name)
            .ok_or_else(|| invalid_entry(pack, id, format!("unknown normaliser `{name}`")))?,
        None => default_normalizer(kind),
    };
    let queries = compile_queries(pack, id, &spec.query, &spec.languages, files)?;
    let captures = capture_names(&queries);

    let mut lookups = BTreeMap::new();
    for (var, lookup) in &spec.lookup {
        if !bindings.contains_key(&lookup.binding) {
            return Err(invalid_entry(
                pack,
                id,
                format!("lookup `{var}` names unknown binding `{}`", lookup.binding),
            ));
        }
        if !lookup.by.starts_with('=') && !captures.contains(&lookup.by) {
            return Err(invalid_entry(
                pack,
                id,
                format!(
                    "lookup `{var}` reads capture `@{}` which the query does not define",
                    lookup.by
                ),
            ));
        }
        lookups.insert(
            var.clone(),
            Lookup {
                binding: lookup.binding.clone(),
                by: lookup.by.clone(),
            },
        );
    }
    let mut defaults = BTreeMap::new();
    for (var, source) in &spec.defaults {
        defaults.insert(var.clone(), template(pack, id, "defaults", source)?);
    }
    let mut known: BTreeSet<String> = captures.clone();
    known.extend(lookups.keys().cloned());
    known.extend(defaults.keys().cloned());
    known.extend(PATH_VARIABLES.iter().map(|s| (*s).to_owned()));

    let check_vars = |field: &str, t: &Template| -> Result<(), LinkError> {
        for var in t.variables() {
            if !known.contains(&var) {
                return Err(invalid_entry(
                    pack,
                    id,
                    format!(
                        "`{field}` uses `{{{var}}}` but the query has no `@{var}` capture and no lookup or default defines it"
                    ),
                ));
            }
        }
        Ok(())
    };
    let key = template(pack, id, "key", &spec.key)?;
    check_vars("key", &key)?;
    let mut attrs = Vec::new();
    for (name, source) in &spec.attrs {
        let t = template(pack, id, "attrs", source)?;
        check_vars("attrs", &t)?;
        attrs.push((name.clone(), t));
    }
    for t in defaults.values() {
        check_vars("defaults", t)?;
    }
    let mut tokens = Vec::new();
    for (token, source) in &spec.tokens {
        let t = template(pack, id, "tokens", source)?;
        check_vars("tokens", &t)?;
        tokens.push((token.clone(), t));
    }
    let mut where_equal = Vec::new();
    for (var, source) in &spec.where_ {
        if !known.contains(var) {
            return Err(invalid_entry(
                pack,
                id,
                format!("`where` reads unknown variable `{var}`"),
            ));
        }
        let t = template(pack, id, "where", source)?;
        check_vars("where", &t)?;
        where_equal.push((var.clone(), t));
    }
    let mut unless = Vec::new();
    for (var, pattern) in &spec.unless {
        if !known.contains(var) {
            return Err(invalid_entry(
                pack,
                id,
                format!("`unless` reads unknown variable `{var}`"),
            ));
        }
        let matcher = Glob::new(pattern)
            .map_err(|e| {
                invalid_entry(pack, id, format!("invalid `unless` glob `{pattern}`: {e}"))
            })?
            .compile_matcher();
        unless.push((var.clone(), matcher));
    }
    for var in &spec.require {
        if !known.contains(var) {
            return Err(invalid_entry(
                pack,
                id,
                format!("`require` names unknown variable `{var}`"),
            ));
        }
    }
    for (field, capture) in [("symbol", &spec.symbol), ("anchor", &spec.anchor)] {
        if let Some(capture) = capture
            && !captures.contains(capture)
        {
            return Err(invalid_entry(
                pack,
                id,
                format!("`{field}` names capture `@{capture}` which the query does not define"),
            ));
        }
    }
    for capture in &spec.resolve {
        if !captures.contains(capture) {
            return Err(invalid_entry(
                pack,
                id,
                format!("`resolve` names capture `@{capture}` which the query does not define"),
            ));
        }
    }
    let route = match spec.route.as_deref() {
        None => None,
        Some("nextjs-app") => Some(RouteConvention::NextjsApp),
        Some("nextjs-pages") => Some(RouteConvention::NextjsPages),
        Some(other) => {
            return Err(invalid_entry(
                pack,
                id,
                format!("unknown route convention `{other}`"),
            ));
        }
    };
    let glob_all = match spec.glob.as_deref() {
        None | Some("one") => false,
        Some("all") => true,
        Some(other) => {
            return Err(invalid_entry(
                pack,
                id,
                format!("`glob` must be `one` or `all`, not `{other}`"),
            ));
        }
    };
    let key_shape = match spec.key_shape.as_deref() {
        None => None,
        Some("dotted") => Some(KeyShape::Dotted),
        Some("upper-snake") => Some(KeyShape::UpperSnake),
        Some(other) => {
            return Err(invalid_entry(
                pack,
                id,
                format!("unknown key shape `{other}`"),
            ));
        }
    };
    let postprocess = match spec.postprocess.as_deref() {
        None => None,
        Some("sql") => Some(PostProcess::Sql),
        Some(other) => {
            return Err(invalid_entry(
                pack,
                id,
                format!("unknown postprocess `{other}`"),
            ));
        }
    };
    if postprocess.is_some() && kind != ContractKind::Table {
        return Err(invalid_entry(
            pack,
            id,
            "`postprocess = \"sql\"` requires `kind = \"table\"`",
        ));
    }
    let detect = match &spec.detect {
        Some(d) => Some(compile_detect(pack, id, Some(d))?),
        None => None,
    };
    Ok(Rule {
        id: spec.id.clone(),
        kind,
        role,
        key,
        attrs,
        symbol: spec.symbol.clone(),
        anchor: spec.anchor.clone(),
        evidence,
        normalizer,
        resolve: spec.resolve.iter().cloned().collect(),
        lookups,
        defaults,
        tokens,
        require: spec.require.clone(),
        where_equal,
        unless,
        files: globs(pack, id, &spec.files)?,
        exclude: globs(pack, id, &spec.exclude)?,
        route,
        glob_all,
        key_shape,
        detect,
        requires_rule: spec.requires_rule.clone(),
        postprocess,
        queries,
    })
}

fn default_normalizer(kind: ContractKind) -> Normalizer {
    match kind {
        ContractKind::Endpoint => Normalizer::Http,
        ContractKind::Topic => Normalizer::Topic,
        ContractKind::EnvName => Normalizer::Env,
        ContractKind::I18nKey => Normalizer::I18n,
        ContractKind::Table => Normalizer::Table,
        ContractKind::Rpc => Normalizer::Rpc,
        ContractKind::Package | ContractKind::Infra => Normalizer::Plain,
    }
}

fn compile_binding(
    pack: &str,
    spec: &BindingSpec,
    files: &BTreeMap<String, String>,
) -> Result<Binding, LinkError> {
    let id = spec.id.as_str();
    let queries = compile_queries(pack, id, &spec.query, &spec.languages, files)?;
    let captures = capture_names(&queries);
    let scope = match spec.scope.as_str() {
        "symbol" => BindingScope::Symbol,
        "file" => BindingScope::File,
        "project" => BindingScope::Project,
        other => {
            return Err(invalid_entry(
                pack,
                id,
                format!("scope `{other}` must be `symbol`, `file` or `project`"),
            ));
        }
    };
    if !captures.contains(&spec.name) {
        return Err(invalid_entry(
            pack,
            id,
            format!(
                "`name` names capture `@{}` which the query does not define",
                spec.name
            ),
        ));
    }
    let value = template(pack, id, "value", &spec.value)?;
    let mut known = captures.clone();
    known.extend(PATH_VARIABLES.iter().map(|s| (*s).to_owned()));
    for var in value.variables() {
        if !known.contains(&var) {
            return Err(invalid_entry(
                pack,
                id,
                format!("`value` uses `{{{var}}}` but the query has no `@{var}` capture"),
            ));
        }
    }
    for capture in &spec.resolve {
        if !captures.contains(capture) {
            return Err(invalid_entry(
                pack,
                id,
                format!("`resolve` names capture `@{capture}` which the query does not define"),
            ));
        }
    }
    Ok(Binding {
        id: spec.id.clone(),
        scope,
        name: spec.name.clone(),
        value,
        resolve: spec.resolve.iter().cloned().collect(),
        constant: spec.constant,
        files: globs(pack, id, &spec.files)?,
        queries,
    })
}

fn compile_extractor(pack: &str, spec: &ExtractorSpec) -> Result<Extractor, LinkError> {
    let kind = StructuredKind::parse(&spec.extractor).ok_or_else(|| {
        invalid_entry(
            pack,
            &spec.id,
            format!("unknown extractor `{}`", spec.extractor),
        )
    })?;
    Ok(Extractor {
        id: spec.id.clone(),
        kind,
        files: globs(pack, &spec.id, &spec.files)?,
        exclude: globs(pack, &spec.id, &spec.exclude)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    const QUERY: &str = r#"(call_expression function: (identifier) @_f (#eq? @_f "fetch") arguments: (arguments . (string) @url))"#;

    fn manifest(rule: &str) -> String {
        format!("name = \"demo\"\nversion = \"1.0.0\"\ndescription = \"d\"\n\n[[rules]]\n{rule}\n")
    }

    #[test]
    fn loads_a_minimal_pack() {
        let toml = manifest(
            "id = \"fetch\"\nquery = \"fetch.scm\"\nlanguages = [\"typescript\"]\nkind = \"endpoint\"\nrole = \"consumer\"\nkey = \"GET {url}\"",
        );
        let pack = Pack::from_files(
            "demo",
            &files(&[
                ("pack.toml", &toml),
                ("typescript/fetch.scm", QUERY),
                ("tests/a.ts", "x"),
            ]),
        )
        .unwrap();
        assert_eq!(pack.id(), "demo@1.0.0");
        assert_eq!(pack.entry_ids(), ["fetch"]);
        assert_eq!(pack.fixtures().len(), 1);
    }

    #[test]
    fn rejects_unknown_captures_and_bad_queries() {
        let toml = manifest(
            "id = \"fetch\"\nquery = \"fetch.scm\"\nlanguages = [\"typescript\"]\nkind = \"endpoint\"\nrole = \"consumer\"\nkey = \"GET {nope}\"",
        );
        let err = Pack::from_files(
            "demo",
            &files(&[("pack.toml", &toml), ("typescript/fetch.scm", QUERY)]),
        )
        .unwrap_err();
        assert!(err.to_string().contains("`@nope`"), "{err}");

        let toml = manifest(
            "id = \"fetch\"\nquery = \"fetch.scm\"\nlanguages = [\"typescript\"]\nkind = \"endpoint\"\nrole = \"consumer\"\nkey = \"GET {url}\"",
        );
        let err = Pack::from_files(
            "demo",
            &files(&[
                ("pack.toml", &toml),
                ("typescript/fetch.scm", "(no_such_node) @x"),
            ]),
        )
        .unwrap_err();
        assert!(matches!(err, LinkError::QueryCompile { .. }), "{err}");

        let err = Pack::from_files("demo", &files(&[("pack.toml", &toml)])).unwrap_err();
        assert!(
            err.to_string().contains("not found for typescript"),
            "{err}"
        );
    }

    #[test]
    fn rejects_bad_manifests() {
        let err = Pack::from_files("demo", &files(&[("pack.toml", "name = ")])).unwrap_err();
        assert!(matches!(err, LinkError::PackManifest { .. }));
        let err = Pack::from_files(
            "demo",
            &files(&[(
                "pack.toml",
                "name = \"x\"\nversion = \"1.0\"\ndescription = \"d\"\n",
            )]),
        )
        .unwrap_err();
        assert!(err.to_string().contains("MAJOR.MINOR.PATCH"), "{err}");
        let err = Pack::from_files(
            "demo",
            &files(&[(
                "pack.toml",
                "name = \"x\"\nversion = \"1.0.0\"\ndescription = \"d\"\nunknown = 1\n",
            )]),
        )
        .unwrap_err();
        assert!(matches!(err, LinkError::PackManifest { .. }), "{err}");
        let toml = manifest(
            "id = \"fetch\"\nquery = \"../x.scm\"\nlanguages = [\"typescript\"]\nkind = \"endpoint\"\nrole = \"consumer\"\nkey = \"x\"",
        );
        assert!(Pack::from_files("demo", &files(&[("pack.toml", &toml)])).is_err());
        let toml = manifest(
            "id = \"fetch\"\nquery = \"fetch.scm\"\nlanguages = [\"typescript\"]\nkind = \"nope\"\nrole = \"consumer\"\nkey = \"x\"",
        );
        let err = Pack::from_files(
            "demo",
            &files(&[("pack.toml", &toml), ("typescript/fetch.scm", QUERY)]),
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown contract kind"), "{err}");
    }

    #[test]
    fn rejects_lossy_string_escapes() {
        assert!(suspicious_escape(r#"((x) @a (#match? @a "^\\s*y"))"#).is_none());
        let err = suspicious_escape(r#"((x) @a (#match? @a "^\s*y"))"#).unwrap();
        assert!(err.contains("line 1"), "{err}");
        assert!(suspicious_escape(r#"((x) @a (#eq? @a "\"q\""))"#).is_none());
        assert!(suspicious_escape("; a comment with \\s is fine\n(x) @a").is_none());
    }

    #[test]
    fn duplicate_pack_names_are_rejected() {
        let toml = manifest(
            "id = \"fetch\"\nquery = \"fetch.scm\"\nlanguages = [\"typescript\"]\nkind = \"endpoint\"\nrole = \"consumer\"\nkey = \"GET {url}\"",
        );
        let f = files(&[("pack.toml", &toml), ("typescript/fetch.scm", QUERY)]);
        let mut set = PackSet::new();
        set.insert(Pack::from_files("demo", &f).unwrap()).unwrap();
        let err = set
            .insert(Pack::from_files("demo", &f).unwrap())
            .unwrap_err();
        assert_eq!(err, LinkError::DuplicatePack("demo".into()));
    }
}
