//! Bounded Rust import bindings for conventional same-crate module layouts.
//!
//! These are syntactic candidates, not compiler resolution. Only explicit
//! `crate`, `self` and `super` paths, named aliases, and declared modules are
//! followed. External crates, glob imports, conditional/custom attributes and
//! ambiguous module files remain gaps. A target name is kept qualified inside
//! its file so importing one item never exposes every declaration in that file.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::time::{Duration, Instant};

use knowell_core::RepoPath;
use knowell_parse::tree_sitter::Node;
use knowell_parse::{Language, ParseLimits, parse_tree};

const MAX_NODES: usize = 200_000;
const MAX_IMPORTS: usize = 4_096;
const MAX_SEGMENTS: usize = 64;
const MAX_NAME_BYTES: usize = 128;

/// One explicit Rust import, visible throughout its enclosing byte scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportBinding {
    /// Local alias, with a raw-identifier prefix removed.
    pub(crate) local_name: String,
    /// Existing source file selected by the conventional module layout.
    pub(crate) target_path: RepoPath,
    /// Qualified name inside the target file, using `.` separators. Empty
    /// means the imported file/module, whose members still need a suffix.
    pub(crate) target_name: String,
    /// Half-open UTF-8 byte range where the alias is visible.
    pub(crate) scope: Range<usize>,
    /// Half-open UTF-8 byte range of the complete `use` declaration.
    pub(crate) declaration: Range<usize>,
    /// 1-based line of the complete `use` declaration.
    pub(crate) line: u32,
}

/// One external-file `mod name;` declaration, never an identifier use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModuleImport {
    /// Declared module name, with a raw-identifier prefix removed.
    pub(crate) local_name: String,
    /// Unique existing `.rs` or `mod.rs` file for this declaration.
    pub(crate) target_path: RepoPath,
    /// Half-open UTF-8 byte range of the containing module or block.
    pub(crate) scope: Range<usize>,
    /// Half-open UTF-8 byte range of the complete module declaration.
    pub(crate) declaration: Range<usize>,
    /// 1-based line of the declaration.
    pub(crate) line: u32,
}

/// Why a Rust import could not be bound by this syntactic resolver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ImportGapReason {
    /// Parsing was unavailable under the caller's resource limits.
    ParserUnavailable,
    /// Source syntax is incomplete or malformed.
    Malformed,
    /// Conditional or custom attributes prevent source-only resolution.
    Guarded,
    /// Imported names are unknown without resolving a glob target.
    Wildcard,
    /// The path does not explicitly anchor a same-crate namespace.
    ExternalOrUnqualified,
    /// The path uses syntax or identifiers this resolver cannot bind.
    UnsupportedPath,
    /// No conventional same-crate root is present in the pinned paths.
    MissingRoot,
    /// More than one root could own the requested root-level item.
    AmbiguousRoot,
    /// The declaring file or declared module is absent from pinned paths.
    MissingModule,
    /// Both conventional module file forms are present.
    AmbiguousModule,
    /// Extraction exceeded its bounded work or output budget.
    Limit,
}

/// A bounded diagnostic with no copied source text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportGap {
    /// Explicit source-only resolution limitation.
    pub(crate) reason: ImportGapReason,
    /// 1-based source line; zero denotes a whole-file diagnostic.
    pub(crate) line: u32,
}

#[derive(Debug, Clone)]
struct ModuleScope {
    range: Range<usize>,
    names: Vec<String>,
}

#[derive(Debug, Clone)]
struct ScopedBinding {
    scope: Range<usize>,
    target: Option<ImportTarget>,
}

#[derive(Debug, Clone)]
struct BlockedModule {
    name_prefix: String,
    file_stem: String,
    reason: ImportGapReason,
}

/// Import bindings and explicit coverage limitations for one Rust file.
#[derive(Debug, Clone, Default)]
pub(crate) struct RustImports {
    /// Explicit named imports; one item may have several scoped aliases.
    pub(crate) bindings: Vec<ImportBinding>,
    /// Unique external-file module declarations.
    pub(crate) modules: Vec<ModuleImport>,
    /// At most `MAX_IMPORTS` diagnostics, in source traversal order.
    pub(crate) gaps: Vec<ImportGap>,
    /// Count of unresolved clauses and extraction limitations, saturating.
    pub(crate) unresolved_count: usize,
    /// Whether work or output limits prevented complete extraction.
    pub(crate) truncated: bool,
    module_scopes: Vec<ModuleScope>,
    disabled_scopes: Vec<Range<usize>>,
    blocked_bindings: Vec<(String, Range<usize>)>,
    wildcard_scopes: Vec<Range<usize>>,
    binding_index: BTreeMap<String, Vec<ScopedBinding>>,
    blocked_modules: Vec<BlockedModule>,
}

/// A file and qualified item selected by an explicit Rust path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ImportTarget {
    target_path: RepoPath,
    /// `.`-separated name inside the file; empty denotes the module itself.
    target_name: String,
}

impl RustImports {
    fn gap(&mut self, reason: ImportGapReason, line: u32) {
        self.truncated |= reason == ImportGapReason::Limit;
        self.unresolved_count = self.unresolved_count.saturating_add(1);
        if self.gaps.len() < MAX_IMPORTS {
            self.gaps.push(ImportGap { reason, line });
        } else {
            self.truncated = true;
        }
    }

    fn block(&mut self, name: String, scope: Range<usize>, line: u32) {
        if self.blocked_bindings.len() < MAX_IMPORTS {
            self.blocked_bindings.push((name, scope));
        } else {
            self.truncated = true;
            self.gap(ImportGapReason::Limit, line);
        }
    }

    fn index_bindings(&mut self) {
        for binding in &self.bindings {
            self.binding_index
                .entry(binding.local_name.clone())
                .or_default()
                .push(ScopedBinding {
                    scope: binding.scope.clone(),
                    target: Some(ImportTarget {
                        target_path: binding.target_path.clone(),
                        target_name: binding.target_name.clone(),
                    }),
                });
        }
        for module in &self.modules {
            self.binding_index
                .entry(module.local_name.clone())
                .or_default()
                .push(ScopedBinding {
                    scope: module.scope.clone(),
                    target: Some(ImportTarget {
                        target_path: module.target_path.clone(),
                        target_name: String::new(),
                    }),
                });
        }
        for (name, scope) in &self.blocked_bindings {
            self.binding_index
                .entry(name.clone())
                .or_default()
                .push(ScopedBinding {
                    scope: scope.clone(),
                    target: None,
                });
        }
    }

    fn block_module(
        &mut self,
        from: &RepoPath,
        node: Node<'_>,
        bytes: &[u8],
        reason: ImportGapReason,
    ) {
        let Some(name) = node
            .child_by_field_name("name")
            .and_then(|name| source_name(name, bytes))
        else {
            return;
        };
        let Ok(mut names) = inline_modules(node, bytes) else {
            return;
        };
        let Some(directory) = child_directory(from) else {
            return;
        };
        names.push(name);
        let Some(stem) = path_in(&directory, &names.join("/")) else {
            return;
        };
        if self.blocked_modules.len() < MAX_IMPORTS {
            self.blocked_modules.push(BlockedModule {
                name_prefix: names.join("."),
                file_stem: stem.to_string(),
                reason,
            });
        }
    }

    fn blocked_target(&self, from: &RepoPath, target: &ImportTarget) -> Option<ImportGapReason> {
        self.blocked_modules.iter().find_map(|module| {
            let own_name = target.target_path == *from
                && (target.target_name == module.name_prefix
                    || target
                        .target_name
                        .strip_prefix(&module.name_prefix)
                        .is_some_and(|rest| rest.starts_with('.')));
            let file = target.target_path.as_str();
            let module_file = file
                .strip_prefix(&module.file_stem)
                .is_some_and(|rest| rest == ".rs" || rest.starts_with('/'));
            (own_name || module_file).then_some(module.reason)
        })
    }

    fn reject_blocked_targets(&mut self, from: &RepoPath) {
        let bindings = std::mem::take(&mut self.bindings);
        for binding in bindings {
            let target = ImportTarget {
                target_path: binding.target_path.clone(),
                target_name: binding.target_name.clone(),
            };
            if let Some(reason) = self.blocked_target(from, &target) {
                self.block(binding.local_name, binding.scope, binding.line);
                self.gap(reason, binding.line);
            } else {
                self.bindings.push(binding);
            }
        }
    }

    /// Whether an explicit import binds this name at a UTF-8 byte offset,
    /// including aliases whose target is unavailable or conditional. A visible
    /// wildcard also blocks unqualified fallback, because its names are unknown.
    pub(crate) fn has_binding(&self, local_name: &str, offset: usize) -> bool {
        let Some(local_name) = clean_name(local_name) else {
            return false;
        };
        let wildcard = !matches!(local_name.as_str(), "crate" | "self" | "super")
            && self
                .wildcard_scopes
                .iter()
                .any(|scope| scope.contains(&offset));
        wildcard
            || self.binding_index.get(&local_name).is_some_and(|bindings| {
                bindings
                    .iter()
                    .any(|binding| binding.scope.contains(&offset))
            })
    }

    /// Existing files needed to validate this file's explicit import targets.
    pub(crate) fn target_paths(&self) -> BTreeSet<RepoPath> {
        self.bindings
            .iter()
            .map(|binding| binding.target_path.clone())
            .chain(self.modules.iter().map(|module| module.target_path.clone()))
            .collect()
    }

    /// Resolves a qualified path at a half-open UTF-8 byte offset. Local
    /// aliases use the innermost enclosing import scope; equal-scope conflicts
    /// return `None`. A module-only target has an empty name and is not a symbol
    /// reference. This does not resolve receiver types or associated methods.
    pub(crate) fn qualified_target(
        &self,
        from: &RepoPath,
        files: &BTreeSet<RepoPath>,
        segments: &[String],
        offset: usize,
    ) -> Option<(RepoPath, String)> {
        if self.truncated
            || segments.is_empty()
            || segments.len() > MAX_SEGMENTS
            || self
                .disabled_scopes
                .iter()
                .any(|scope| scope.contains(&offset))
        {
            return None;
        }
        let first = segments.first()?;
        let first = clean_name(first)?;
        let suffix = segments.get(1..)?;
        let mut visible: BTreeMap<usize, Option<BTreeSet<ImportTarget>>> = BTreeMap::new();
        for binding in self.binding_index.get(&first).into_iter().flatten() {
            if !binding.scope.contains(&offset) {
                continue;
            }
            let length = binding.scope.end.saturating_sub(binding.scope.start);
            if let Some(target) = &binding.target {
                if let Some(candidates) = visible
                    .entry(length)
                    .or_insert_with(|| Some(BTreeSet::new()))
                {
                    candidates.insert(ImportTarget {
                        target_path: target.target_path.clone(),
                        target_name: extend_name(&target.target_name, suffix)?,
                    });
                }
            } else {
                visible.insert(length, None);
            }
        }
        let wildcard = (!matches!(first.as_str(), "crate" | "self" | "super"))
            .then(|| {
                self.wildcard_scopes
                    .iter()
                    .filter(|scope| scope.contains(&offset))
                    .map(|scope| scope.end.saturating_sub(scope.start))
                    .min()
            })
            .flatten();
        if let Some((length, candidates)) = visible.first_key_value() {
            let candidates = candidates.as_ref()?;
            if wildcard.is_some_and(|wildcard| wildcard < *length) {
                return None;
            }
            if candidates.len() != 1 {
                return None;
            }
            return candidates
                .first()
                .map(|target| (target.target_path.clone(), target.target_name.clone()));
        }
        if wildcard.is_some() {
            return None;
        }
        let inline = self
            .module_scopes
            .iter()
            .filter(|scope| scope.range.contains(&offset))
            .min_by_key(|scope| scope.range.end.saturating_sub(scope.range.start))?;
        resolve_path(from, &inline.names, segments, files)
            .ok()
            .filter(|target| self.blocked_target(from, target).is_none())
            .map(|target| (target.target_path, target.target_name))
    }
}

fn extend_name(prefix: &str, suffix: &[String]) -> Option<String> {
    let mut names = Vec::new();
    if !prefix.is_empty() {
        names.push(prefix.to_owned());
    }
    for name in suffix {
        names.push(clean_name(name)?);
    }
    Some(names.join("."))
}

fn clean_name(name: &str) -> Option<String> {
    let name = name.strip_prefix("r#").unwrap_or(name);
    let mut characters = name.chars();
    let first = characters.next()?;
    (name.len() <= MAX_NAME_BYTES
        && (first.is_alphabetic() || first == '_')
        && characters.all(|character| character.is_alphanumeric() || character == '_'))
    .then(|| name.to_owned())
}

fn source_name(node: Node<'_>, bytes: &[u8]) -> Option<String> {
    clean_name(node.utf8_text(bytes).ok()?)
}

fn line(node: Node<'_>) -> u32 {
    u32::try_from(node.start_position().row)
        .unwrap_or(u32::MAX)
        .saturating_add(1)
}

fn is_comment(node: Node<'_>) -> bool {
    matches!(node.kind(), "line_comment" | "block_comment")
}

fn unsupported_attribute(node: Node<'_>, bytes: &[u8]) -> bool {
    let name = node
        .named_child(0)
        .and_then(|attribute| attribute.named_child(0))
        .and_then(|name| source_name(name, bytes));
    // Lints and documentation cannot rewrite module paths or item bindings.
    // Conditional and custom attributes can, so they are deliberately rejected.
    !name.is_some_and(|name| {
        matches!(
            name.as_str(),
            "allow" | "warn" | "deny" | "forbid" | "expect" | "doc"
        )
    })
}

fn has_attributes(node: Node<'_>, bytes: &[u8]) -> bool {
    let mut previous = node.prev_named_sibling();
    while let Some(sibling) = previous {
        if sibling.kind() == "attribute_item" {
            if unsupported_attribute(sibling, bytes) {
                return true;
            }
        } else if !is_comment(sibling) {
            break;
        }
        previous = sibling.prev_named_sibling();
    }
    false
}

fn inner_attributes(node: Node<'_>, bytes: &[u8]) -> bool {
    for index in 0..node.named_child_count() {
        let Ok(index) = u32::try_from(index) else {
            return true;
        };
        let Some(child) = node.named_child(index) else {
            continue;
        };
        if child.kind() == "inner_attribute_item" {
            if unsupported_attribute(child, bytes) {
                return true;
            }
        } else if !is_comment(child) && child.kind() != "shebang" {
            break;
        }
    }
    false
}

fn guarded(mut node: Node<'_>, bytes: &[u8]) -> bool {
    loop {
        if has_attributes(node, bytes)
            || (matches!(node.kind(), "source_file" | "declaration_list" | "block")
                && inner_attributes(node, bytes))
        {
            return true;
        }
        let Some(parent) = node.parent() else {
            return false;
        };
        node = parent;
    }
}

fn enclosing_scope(mut node: Node<'_>) -> Range<usize> {
    while let Some(parent) = node.parent() {
        if matches!(parent.kind(), "source_file" | "declaration_list" | "block") {
            return parent.byte_range();
        }
        node = parent;
    }
    node.byte_range()
}

fn inline_modules(mut node: Node<'_>, bytes: &[u8]) -> Result<Vec<String>, ImportGapReason> {
    let mut names = Vec::new();
    while let Some(parent) = node.parent() {
        if parent.kind() == "mod_item" && parent.child_by_field_name("body").is_some() {
            let name = parent
                .child_by_field_name("name")
                .and_then(|name| source_name(name, bytes))
                .ok_or(ImportGapReason::UnsupportedPath)?;
            names.push(name);
            if names.len() >= MAX_SEGMENTS {
                return Err(ImportGapReason::Limit);
            }
        }
        node = parent;
    }
    names.reverse();
    Ok(names)
}

fn path_segments(mut node: Node<'_>, bytes: &[u8]) -> Result<Vec<String>, ImportGapReason> {
    let mut reversed = Vec::new();
    loop {
        if reversed.len() >= MAX_SEGMENTS {
            return Err(ImportGapReason::Limit);
        }
        match node.kind() {
            "scoped_identifier" | "scoped_type_identifier" => {
                let name = node
                    .child_by_field_name("name")
                    .and_then(|name| source_name(name, bytes))
                    .ok_or(ImportGapReason::UnsupportedPath)?;
                reversed.push(name);
                node = node
                    .child_by_field_name("path")
                    .ok_or(ImportGapReason::ExternalOrUnqualified)?;
            }
            "identifier" | "type_identifier" | "crate" | "self" | "super" => {
                reversed.push(source_name(node, bytes).ok_or(ImportGapReason::UnsupportedPath)?);
                break;
            }
            _ => return Err(ImportGapReason::UnsupportedPath),
        }
    }
    reversed.reverse();
    Ok(reversed)
}

#[derive(Debug)]
struct CrateLayout {
    directory: String,
    roots: Vec<RepoPath>,
    own_module: Vec<String>,
}

fn path_in(directory: &str, suffix: &str) -> Option<RepoPath> {
    if directory.is_empty() {
        RepoPath::new(suffix).ok()
    } else {
        RepoPath::new(format!("{directory}/{suffix}")).ok()
    }
}

fn crate_layout(from: &RepoPath, files: &BTreeSet<RepoPath>) -> Option<CrateLayout> {
    let mut directory = from
        .parent()
        .map(|parent| parent.to_string())
        .unwrap_or_default();
    loop {
        let mut roots: Vec<RepoPath> = ["lib.rs", "main.rs"]
            .iter()
            .filter_map(|name| path_in(&directory, name))
            .filter(|path| files.contains(path))
            .collect();
        let entry_directory = |name: &str| matches!(name, "bin" | "tests" | "examples" | "benches");
        // Conventional standalone entries are separate crates. Their children
        // live under the entry's stem, rather than under its parent's directory.
        if directory.rsplit('/').next().is_some_and(entry_directory)
            && from.parent().as_ref().map(RepoPath::as_str) == Some(directory.as_str())
            && files.contains(from)
        {
            roots = vec![from.clone()];
        } else if let Some((parent, _)) = directory.rsplit_once('/')
            && parent.rsplit('/').next().is_some_and(entry_directory)
            && let Ok(candidate) = RepoPath::new(format!("{directory}.rs"))
            && files.contains(&candidate)
        {
            roots.push(candidate);
        }
        let roots = if roots.contains(from) {
            vec![from.clone()]
        } else {
            roots
        };
        if !roots.is_empty() {
            let module_directory = roots.first().and_then(child_directory)?;
            let own_module = if roots.contains(from) {
                Vec::new()
            } else {
                let relative = if module_directory.is_empty() {
                    from.as_str()
                } else {
                    from.as_str()
                        .strip_prefix(&format!("{module_directory}/"))?
                };
                let mut components: Vec<String> = relative.split('/').map(str::to_owned).collect();
                let name = components.pop()?;
                if name != "mod.rs" {
                    components.push(clean_name(name.strip_suffix(".rs")?)?);
                }
                components
            };
            return Some(CrateLayout {
                directory: module_directory,
                roots,
                own_module,
            });
        }
        let Some((parent, _)) = directory.rsplit_once('/') else {
            if directory.is_empty() {
                return None;
            }
            directory.clear();
            continue;
        };
        directory = parent.to_owned();
    }
}

fn module_candidates(
    directory: &str,
    names: &[String],
    files: &BTreeSet<RepoPath>,
) -> Vec<RepoPath> {
    let stem = names.join("/");
    [format!("{stem}.rs"), format!("{stem}/mod.rs")]
        .iter()
        .filter_map(|suffix| path_in(directory, suffix))
        .filter(|path| files.contains(path))
        .collect()
}

fn child_directory(from: &RepoPath) -> Option<String> {
    let parent = from
        .parent()
        .map(|parent| parent.to_string())
        .unwrap_or_default();
    if matches!(from.file_name(), "lib.rs" | "main.rs" | "mod.rs") {
        return Some(parent);
    }
    let stem = clean_name(from.file_name().strip_suffix(".rs")?)?;
    if parent.is_empty() {
        Some(stem)
    } else {
        Some(format!("{parent}/{stem}"))
    }
}

fn declared_module(
    from: &RepoPath,
    inline: &[String],
    name: &str,
    files: &BTreeSet<RepoPath>,
) -> Result<RepoPath, ImportGapReason> {
    let directory = child_directory(from).ok_or(ImportGapReason::UnsupportedPath)?;
    let mut names = inline.to_vec();
    names.push(name.to_owned());
    let candidates = module_candidates(&directory, &names, files);
    match candidates.as_slice() {
        [target] => Ok(target.clone()),
        [] => Err(ImportGapReason::MissingModule),
        _ => Err(ImportGapReason::AmbiguousModule),
    }
}

fn resolve_path(
    from: &RepoPath,
    inline: &[String],
    path: &[String],
    files: &BTreeSet<RepoPath>,
) -> Result<ImportTarget, ImportGapReason> {
    if path.is_empty() || path.len().saturating_add(inline.len()) > MAX_SEGMENTS {
        return Err(ImportGapReason::UnsupportedPath);
    }
    let path: Vec<String> = path
        .iter()
        .map(|name| clean_name(name).ok_or(ImportGapReason::UnsupportedPath))
        .collect::<Result<_, _>>()?;
    let first = path.first().ok_or(ImportGapReason::UnsupportedPath)?;
    let mut suffix = path.get(1..).ok_or(ImportGapReason::UnsupportedPath)?;
    if first == "self" {
        if suffix
            .iter()
            .any(|name| matches!(name.as_str(), "crate" | "self" | "super"))
        {
            return Err(ImportGapReason::UnsupportedPath);
        }
        let directory = child_directory(from).ok_or(ImportGapReason::UnsupportedPath)?;
        let mut selected = from.clone();
        let mut consumed = 0usize;
        for length in 1..=suffix.len() {
            let mut names = inline.to_vec();
            names.extend_from_slice(
                suffix
                    .get(..length)
                    .ok_or(ImportGapReason::UnsupportedPath)?,
            );
            match module_candidates(&directory, &names, files).as_slice() {
                [target] => {
                    selected = target.clone();
                    consumed = length;
                }
                [] => break,
                _ => return Err(ImportGapReason::AmbiguousModule),
            }
        }
        let mut names = if consumed == 0 {
            inline.to_vec()
        } else {
            Vec::new()
        };
        names.extend_from_slice(
            suffix
                .get(consumed..)
                .ok_or(ImportGapReason::UnsupportedPath)?,
        );
        return Ok(ImportTarget {
            target_path: selected,
            target_name: names.join("."),
        });
    }
    let Some(layout) = crate_layout(from, files) else {
        return Err(if matches!(first.as_str(), "crate" | "super") {
            ImportGapReason::MissingRoot
        } else {
            ImportGapReason::ExternalOrUnqualified
        });
    };
    let mut absolute = match first.as_str() {
        "crate" => Vec::new(),
        "super" => {
            let mut names = layout.own_module.clone();
            names.extend_from_slice(inline);
            if first == "super" {
                names.pop().ok_or(ImportGapReason::UnsupportedPath)?;
                while suffix.first().is_some_and(|name| name == "super") {
                    names.pop().ok_or(ImportGapReason::UnsupportedPath)?;
                    suffix = suffix.get(1..).ok_or(ImportGapReason::UnsupportedPath)?;
                }
            }
            names
        }
        _ => return Err(ImportGapReason::ExternalOrUnqualified),
    };
    absolute.extend_from_slice(suffix);
    if absolute.len() > MAX_SEGMENTS
        || absolute.iter().any(|name| {
            clean_name(name).is_none() || matches!(name.as_str(), "crate" | "self" | "super")
        })
    {
        return Err(ImportGapReason::UnsupportedPath);
    }
    let mut selected = (layout.roots.len() == 1)
        .then(|| layout.roots.first().cloned())
        .flatten();
    let mut consumed = 0usize;
    for length in 1..=absolute.len() {
        let prefix = absolute
            .get(..length)
            .ok_or(ImportGapReason::UnsupportedPath)?;
        match module_candidates(&layout.directory, prefix, files).as_slice() {
            [target] => {
                selected = Some(target.clone());
                consumed = length;
            }
            [] => break,
            _ => return Err(ImportGapReason::AmbiguousModule),
        }
    }
    let target_path = selected.ok_or(ImportGapReason::AmbiguousRoot)?;
    let target_name = absolute
        .get(consumed..)
        .ok_or(ImportGapReason::UnsupportedPath)?
        .join(".");
    Ok(ImportTarget {
        target_path,
        target_name,
    })
}

fn use_bindings(
    from: &RepoPath,
    declaration: Node<'_>,
    bytes: &[u8],
    files: &BTreeSet<RepoPath>,
    out: &mut RustImports,
    blocked: Option<ImportGapReason>,
    budget: (Instant, Duration),
) {
    let declaration_line = line(declaration);
    let Some(argument) = declaration.child_by_field_name("argument") else {
        out.gap(ImportGapReason::Malformed, declaration_line);
        return;
    };
    let inline = match inline_modules(declaration, bytes) {
        Ok(inline) => inline,
        Err(reason) => {
            out.gap(reason, declaration_line);
            return;
        }
    };
    let mut pending = vec![(argument, Vec::<String>::new())];
    let mut clauses = 0usize;
    while let Some((node, prefix)) = pending.pop() {
        clauses = clauses.saturating_add(1);
        if clauses > MAX_IMPORTS
            || out.bindings.len().saturating_add(out.modules.len()) >= MAX_IMPORTS
            || budget.0.elapsed() >= budget.1
        {
            out.truncated = true;
            if out.wildcard_scopes.len() < MAX_IMPORTS {
                out.wildcard_scopes.push(enclosing_scope(declaration));
            }
            out.gap(ImportGapReason::Limit, declaration_line);
            return;
        }
        match node.kind() {
            "use_list" => {
                if node.named_child_count().saturating_add(pending.len()) > MAX_IMPORTS {
                    out.truncated = true;
                    if out.wildcard_scopes.len() < MAX_IMPORTS {
                        out.wildcard_scopes.push(enclosing_scope(declaration));
                    }
                    out.gap(ImportGapReason::Limit, declaration_line);
                    return;
                }
                for index in (0..node.named_child_count()).rev() {
                    let Ok(index) = u32::try_from(index) else {
                        out.gap(ImportGapReason::Limit, declaration_line);
                        return;
                    };
                    if let Some(child) = node.named_child(index)
                        && !is_comment(child)
                    {
                        pending.push((child, prefix.clone()));
                    }
                }
                continue;
            }
            "scoped_use_list" => {
                let parts = node
                    .child_by_field_name("path")
                    .map(|path| path_segments(path, bytes));
                let Some(Ok(parts)) = parts else {
                    out.gap(ImportGapReason::UnsupportedPath, declaration_line);
                    continue;
                };
                let mut prefix = prefix;
                prefix.extend(parts);
                if prefix.len() > MAX_SEGMENTS {
                    out.gap(ImportGapReason::Limit, declaration_line);
                    out.truncated = true;
                    continue;
                }
                if let Some(list) = node.child_by_field_name("list") {
                    pending.push((list, prefix));
                } else {
                    out.gap(ImportGapReason::Malformed, declaration_line);
                }
                continue;
            }
            "use_wildcard" => {
                if out.wildcard_scopes.len() < MAX_IMPORTS {
                    out.wildcard_scopes.push(enclosing_scope(declaration));
                } else {
                    out.truncated = true;
                }
                out.gap(
                    blocked.unwrap_or(ImportGapReason::Wildcard),
                    declaration_line,
                );
                continue;
            }
            _ => {}
        }
        let (path, alias) = if node.kind() == "use_as_clause" {
            let path = node.child_by_field_name("path");
            let alias = node
                .child_by_field_name("alias")
                .and_then(|alias| source_name(alias, bytes));
            let (Some(path), Some(alias)) = (path, alias) else {
                out.gap(ImportGapReason::Malformed, declaration_line);
                continue;
            };
            (path, Some(alias))
        } else {
            (node, None)
        };
        let parts = match path_segments(path, bytes) {
            Ok(parts) => parts,
            Err(reason) => {
                out.gap(reason, declaration_line);
                continue;
            }
        };
        let mut path = prefix;
        path.extend(parts);
        if path.len() > 1 && path.last().is_some_and(|name| name == "self") {
            path.pop();
        }
        let local_name = alias.or_else(|| path.last().cloned());
        let Some(local_name) = local_name.filter(|name| name != "_") else {
            out.gap(ImportGapReason::UnsupportedPath, declaration_line);
            continue;
        };
        if let Some(reason) = blocked {
            out.block(local_name, enclosing_scope(declaration), declaration_line);
            out.gap(reason, declaration_line);
            continue;
        }
        match resolve_path(from, &inline, &path, files) {
            Ok(target) => out.bindings.push(ImportBinding {
                local_name,
                target_path: target.target_path,
                target_name: target.target_name,
                scope: enclosing_scope(declaration),
                declaration: declaration.byte_range(),
                line: declaration_line,
            }),
            Err(reason) => {
                out.block(local_name, enclosing_scope(declaration), declaration_line);
                out.gap(reason, declaration_line);
            }
        }
    }
}

/// Extracts supported Rust aliases and module files from already-admitted
/// source text. Existing paths are supplied by the pinned view; no filesystem
/// reads or external tooling occur. Work and diagnostics are bounded.
pub(crate) fn resolve(
    from: &RepoPath,
    text: &str,
    files: &BTreeSet<RepoPath>,
    limits: &ParseLimits,
) -> RustImports {
    let mut out = RustImports::default();
    if text.trim().is_empty() {
        return out;
    }
    if !files.contains(from) {
        out.gap(ImportGapReason::MissingModule, 0);
        return out;
    }
    let started = Instant::now();
    let Some(tree) = parse_tree(Language::Rust, text, limits) else {
        out.gap(ImportGapReason::ParserUnavailable, 0);
        return out;
    };
    let bytes = text.as_bytes();
    let root = tree.root_node();
    out.module_scopes.push(ModuleScope {
        range: root.byte_range(),
        names: Vec::new(),
    });
    if root.has_error() {
        out.gap(ImportGapReason::Malformed, 0);
    }
    if inner_attributes(root, bytes) {
        out.disabled_scopes.push(root.byte_range());
    }
    let mut cursor = root.walk();
    let mut visited = 0usize;
    'walk: loop {
        visited = visited.saturating_add(1);
        if visited > MAX_NODES || started.elapsed() >= limits.timeout {
            out.truncated = true;
            out.gap(ImportGapReason::Limit, line(cursor.node()));
            break;
        }
        let node = cursor.node();
        if has_attributes(node, bytes)
            && !matches!(node.kind(), "attribute_item" | "inner_attribute_item")
        {
            out.disabled_scopes.push(node.byte_range());
        }
        if matches!(node.kind(), "mod_item" | "use_declaration") {
            let blocked = if node.has_error() {
                Some(ImportGapReason::Malformed)
            } else if guarded(node, bytes) {
                Some(ImportGapReason::Guarded)
            } else {
                None
            };
            if node.kind() == "use_declaration" {
                use_bindings(
                    from,
                    node,
                    bytes,
                    files,
                    &mut out,
                    blocked,
                    (started, limits.timeout),
                );
            } else if let Some(reason) = blocked {
                if let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|name| source_name(name, bytes))
                {
                    out.block(name, enclosing_scope(node), line(node));
                }
                out.block_module(from, node, bytes, reason);
                out.gap(reason, line(node));
            } else if let Some(body) = node.child_by_field_name("body") {
                match inline_modules(body, bytes) {
                    Ok(names) => out.module_scopes.push(ModuleScope {
                        range: body.byte_range(),
                        names,
                    }),
                    Err(reason) => out.gap(reason, line(node)),
                }
                if inner_attributes(body, bytes) {
                    out.disabled_scopes.push(body.byte_range());
                }
            } else {
                let name = node
                    .child_by_field_name("name")
                    .and_then(|name| source_name(name, bytes));
                let inline = inline_modules(node, bytes);
                match name.zip(inline.ok()) {
                    Some((name, inline)) => match declared_module(from, &inline, &name, files) {
                        Ok(target_path) => out.modules.push(ModuleImport {
                            local_name: name,
                            target_path,
                            scope: enclosing_scope(node),
                            declaration: node.byte_range(),
                            line: line(node),
                        }),
                        Err(reason) => {
                            out.block(name, enclosing_scope(node), line(node));
                            out.gap(reason, line(node));
                            out.block_module(from, node, bytes, reason);
                        }
                    },
                    None => out.gap(ImportGapReason::UnsupportedPath, line(node)),
                }
            }
        }
        if out
            .bindings
            .len()
            .saturating_add(out.modules.len())
            .saturating_add(out.module_scopes.len())
            .saturating_add(out.blocked_bindings.len())
            .saturating_add(out.wildcard_scopes.len())
            >= MAX_IMPORTS
        {
            out.truncated = true;
            out.gap(ImportGapReason::Limit, line(node));
            break;
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                continue 'walk;
            }
            if !cursor.goto_parent() {
                break 'walk;
            }
        }
    }
    out.reject_blocked_targets(from);
    out.index_bindings();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(value: &str) -> RepoPath {
        RepoPath::new(value).unwrap()
    }

    fn files(values: &[&str]) -> BTreeSet<RepoPath> {
        values.iter().map(|value| path(value)).collect()
    }

    fn scan(from: &str, text: &str, values: &[&str]) -> RustImports {
        resolve(&path(from), text, &files(values), &ParseLimits::default())
    }

    fn segments(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn grouped_aliases_bind_only_named_targets() {
        let source = "use crate::helpers::{function as aliased, Thing as Writer};\nfn run() { aliased(); Writer::raw(); }";
        let set = files(&["src/lib.rs", "src/helpers.rs"]);
        let imports = resolve(&path("src/lib.rs"), source, &set, &ParseLimits::default());
        assert_eq!(imports.unresolved_count, 0);
        assert_eq!(imports.bindings.len(), 2);
        assert_eq!(imports.bindings[0].local_name, "aliased");
        assert_eq!(imports.bindings[0].target_name, "function");
        assert_eq!(imports.bindings[0].target_path, path("src/helpers.rs"));
        assert_eq!(imports.bindings[0].scope, 0..source.len());
        let target = imports
            .qualified_target(
                &path("src/lib.rs"),
                &set,
                &segments(&["Writer", "raw"]),
                source.find("Writer::raw").unwrap(),
            )
            .unwrap();
        assert_eq!(target.1, "Thing.raw");
        assert_eq!(
            imports.target_paths(),
            BTreeSet::from([path("src/helpers.rs")])
        );
    }

    #[test]
    fn modules_use_the_correct_file_module_directory() {
        let imports = scan(
            "src/outer.rs",
            "mod child;",
            &[
                "src/lib.rs",
                "src/outer.rs",
                "src/outer/child/mod.rs",
                "src/child.rs",
            ],
        );
        assert_eq!(imports.modules.len(), 1);
        assert_eq!(
            imports.modules[0].target_path,
            path("src/outer/child/mod.rs")
        );
        let imports = scan(
            "src/outer/mod.rs",
            "mod child;",
            &["src/lib.rs", "src/outer/mod.rs", "src/outer/child.rs"],
        );
        assert_eq!(imports.modules[0].target_path, path("src/outer/child.rs"));
    }

    #[test]
    fn inline_modules_preserve_qualified_names_and_parent_scope() {
        let source = "mod inner { use self::local as call; use super::root_fn; fn local() {} fn run() { call(); root_fn(); } }";
        let imports = scan("src/lib.rs", source, &["src/lib.rs"]);
        assert_eq!(imports.bindings.len(), 2);
        assert_eq!(imports.bindings[0].target_name, "inner.local");
        assert_eq!(imports.bindings[1].target_name, "root_fn");
        assert!(
            imports
                .bindings
                .iter()
                .all(|binding| binding.scope.start > 0)
        );
    }

    #[test]
    fn self_super_and_module_alias_paths_are_bounded() {
        let source = "use self::local; use super::helper::function as parent_fn; use crate::helper::{self as helpers}; fn run() { helpers::function(); }";
        let set = files(&["src/lib.rs", "src/nested.rs", "src/helper.rs"]);
        let imports = resolve(
            &path("src/nested.rs"),
            source,
            &set,
            &ParseLimits::default(),
        );
        assert_eq!(imports.bindings.len(), 3);
        assert_eq!(imports.bindings[0].target_path, path("src/nested.rs"));
        assert_eq!(imports.bindings[0].target_name, "local");
        assert_eq!(imports.bindings[1].target_path, path("src/helper.rs"));
        assert_eq!(imports.bindings[2].target_name, "");
        let target = imports
            .qualified_target(
                &path("src/nested.rs"),
                &set,
                &segments(&["helpers", "function"]),
                source.find("helpers::function").unwrap(),
            )
            .unwrap();
        assert_eq!(target.1, "function");
    }

    #[test]
    fn nested_block_aliases_are_visible_for_the_whole_block() {
        let source = "use crate::first::function as call; fn run() { { call(); use crate::second::function as call; } call(); }";
        let set = files(&["src/lib.rs", "src/first.rs", "src/second.rs"]);
        let imports = resolve(&path("src/lib.rs"), source, &set, &ParseLimits::default());
        let inner = imports
            .qualified_target(
                &path("src/lib.rs"),
                &set,
                &segments(&["call"]),
                source.find("call();").unwrap(),
            )
            .unwrap();
        assert_eq!(inner.0, path("src/second.rs"));
        let outer = imports
            .qualified_target(
                &path("src/lib.rs"),
                &set,
                &segments(&["call"]),
                source.rfind("call();").unwrap(),
            )
            .unwrap();
        assert_eq!(outer.0, path("src/first.rs"));
    }

    #[test]
    fn conventional_roots_stay_inside_the_nearest_crate() {
        let imports = scan(
            "crates/sample/src/current.rs",
            "use crate::helpers::function;",
            &[
                "src/lib.rs",
                "src/helpers.rs",
                "crates/sample/src/lib.rs",
                "crates/sample/src/current.rs",
                "crates/sample/src/helpers.rs",
            ],
        );
        assert_eq!(
            imports.bindings[0].target_path,
            path("crates/sample/src/helpers.rs")
        );
        let imports = scan(
            "src/bin/tool.rs",
            "use crate::local;",
            &["src/lib.rs", "src/bin/tool.rs"],
        );
        assert_eq!(imports.bindings[0].target_path, path("src/bin/tool.rs"));
        let imports = scan(
            "src/bin/tool.rs",
            "mod child; use crate::child::function;",
            &[
                "src/lib.rs",
                "src/bin/tool.rs",
                "src/bin/tool/child.rs",
                "src/bin/child.rs",
            ],
        );
        assert_eq!(
            imports.bindings[0].target_path,
            path("src/bin/tool/child.rs")
        );
        let imports = scan(
            "src/bin/tool/child.rs",
            "use crate::function;",
            &["src/lib.rs", "src/bin/tool.rs", "src/bin/tool/child.rs"],
        );
        assert_eq!(imports.bindings[0].target_path, path("src/bin/tool.rs"));
    }

    #[test]
    fn explicit_current_root_and_inline_children_do_not_guess_another_root() {
        let imports = scan(
            "src/lib.rs",
            "use crate::function;",
            &["src/lib.rs", "src/main.rs"],
        );
        assert_eq!(imports.bindings[0].target_path, path("src/lib.rs"));
        let imports = scan(
            "src/lib.rs",
            "mod inner { mod child; use self::child::function; }",
            &["src/lib.rs", "src/inner/child.rs"],
        );
        assert_eq!(imports.bindings[0].target_path, path("src/inner/child.rs"));
        let imports = scan("src/current.rs", "use self::function;", &["src/current.rs"]);
        assert_eq!(imports.bindings[0].target_path, path("src/current.rs"));
    }

    #[test]
    fn lint_and_documentation_attributes_do_not_hide_normal_imports() {
        let imports = scan(
            "src/lib.rs",
            "#![forbid(unsafe_code)]\n#[allow(unused_imports)]\nuse crate::helper::function;",
            &["src/lib.rs", "src/helper.rs"],
        );
        assert_eq!(imports.unresolved_count, 0);
        assert_eq!(imports.bindings.len(), 1);
    }

    #[test]
    fn unresolved_guarded_and_wildcard_imports_block_same_name_fallback() {
        let source = "use missing::function as call; #[cfg(feature = \"optional\")] use crate::helper::function as guarded; fn run() { call(); guarded(); { use unknown::*; another(); } }";
        let set = files(&["src/lib.rs", "src/helper.rs"]);
        let imports = resolve(&path("src/lib.rs"), source, &set, &ParseLimits::default());
        for name in ["call", "guarded", "another"] {
            let offset = source.find(&format!("{name}();")).unwrap();
            assert!(imports.has_binding(name, offset));
            assert!(
                imports
                    .qualified_target(&path("src/lib.rs"), &set, &segments(&[name]), offset)
                    .is_none()
            );
        }
        assert!(!imports.has_binding("crate", source.find("another();").unwrap()));
    }

    #[test]
    fn conditional_module_targets_do_not_resolve_through_direct_paths_or_aliases() {
        let source = "#[cfg(feature = \"optional\")] mod conditional; use crate::conditional::function as call; fn run() { crate::conditional::function(); call(); }";
        let set = files(&["src/lib.rs", "src/conditional.rs"]);
        let imports = resolve(&path("src/lib.rs"), source, &set, &ParseLimits::default());
        assert!(imports.bindings.is_empty());
        let offset = source.find("crate::conditional::function();").unwrap();
        assert!(
            imports
                .qualified_target(
                    &path("src/lib.rs"),
                    &set,
                    &segments(&["crate", "conditional", "function"]),
                    offset
                )
                .is_none()
        );
        let offset = source.find("call();").unwrap();
        assert!(imports.has_binding("call", offset));
        assert!(
            imports
                .qualified_target(&path("src/lib.rs"), &set, &segments(&["call"]), offset)
                .is_none()
        );
    }

    #[test]
    fn unsupported_inputs_remain_explicit_gaps() {
        let source = "use external::function; use crate::helpers::*; #[cfg(feature = \"optional\")] use crate::helpers::function; #[path = \"custom.rs\"] mod renamed;";
        let imports = scan(
            "src/lib.rs",
            source,
            &[
                "src/lib.rs",
                "src/helpers.rs",
                "src/renamed.rs",
                "src/custom.rs",
            ],
        );
        assert!(imports.bindings.is_empty());
        assert!(imports.modules.is_empty());
        assert_eq!(imports.unresolved_count, 4);
        assert!(
            imports
                .gaps
                .iter()
                .any(|gap| gap.reason == ImportGapReason::ExternalOrUnqualified)
        );
        assert!(
            imports
                .gaps
                .iter()
                .any(|gap| gap.reason == ImportGapReason::Wildcard)
        );
        assert_eq!(
            imports
                .gaps
                .iter()
                .filter(|gap| gap.reason == ImportGapReason::Guarded)
                .count(),
            2
        );
    }

    #[test]
    fn absent_and_ambiguous_roots_are_never_substituted() {
        let imports = scan(
            "src/current.rs",
            "use crate::helpers::function;",
            &["src/current.rs", "src/helpers.rs"],
        );
        assert!(imports.bindings.is_empty());
        assert_eq!(imports.gaps[0].reason, ImportGapReason::MissingRoot);
        let imports = scan(
            "src/current.rs",
            "use crate::function;",
            &["src/lib.rs", "src/main.rs", "src/current.rs"],
        );
        assert!(imports.bindings.is_empty());
        assert_eq!(imports.gaps[0].reason, ImportGapReason::AmbiguousRoot);
        let imports = scan(
            "src/lib.rs",
            "mod helpers;",
            &["src/lib.rs", "src/helpers.rs", "src/helpers/mod.rs"],
        );
        assert!(imports.modules.is_empty());
        assert_eq!(imports.gaps[0].reason, ImportGapReason::AmbiguousModule);
    }

    #[test]
    fn malformed_hostile_and_truncated_sources_are_bounded() {
        for source in [
            "use crate::helpers::{function as",
            "use ::external::function;",
            "use crate::../../outside;",
            "use crate::helpers::{function as _};",
        ] {
            let imports = scan("src/lib.rs", source, &["src/lib.rs", "src/helpers.rs"]);
            assert!(
                imports.bindings.is_empty(),
                "unexpected binding for {source}"
            );
            assert!(imports.unresolved_count > 0);
        }
        let source = "use crate::helpers::function;";
        let limits = ParseLimits {
            max_bytes: 1,
            ..ParseLimits::default()
        };
        let imports = resolve(
            &path("src/lib.rs"),
            source,
            &files(&["src/lib.rs", "src/helpers.rs"]),
            &limits,
        );
        assert_eq!(imports.gaps[0].reason, ImportGapReason::ParserUnavailable);
        // Newlines keep the source below the driver's minified-file guard so
        // this fixture exercises the import queue bound, rather than rejection
        // before tree-sitter and this resolver are invoked.
        let hostile = format!(
            "use crate::{{\n{}\n}};",
            std::iter::repeat_n("function", MAX_IMPORTS + 1)
                .collect::<Vec<_>>()
                .join(",\n")
        );
        let imports = scan("src/lib.rs", &hostile, &["src/lib.rs"]);
        assert!(imports.truncated);
        assert!(
            imports
                .gaps
                .iter()
                .any(|gap| gap.reason == ImportGapReason::Limit)
        );
        assert!(
            imports
                .gaps
                .iter()
                .all(|gap| gap.reason != ImportGapReason::ParserUnavailable)
        );
        assert!(imports.bindings.len() <= MAX_IMPORTS);
        assert!(imports.gaps.len() <= MAX_IMPORTS);
    }
}
