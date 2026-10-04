//! T1: chunks and per-path chunk inputs, symbols, occurrences (definitions
//! and references), syntactic edges, and the re-resolution of unchanged
//! files whose imports or references the build invalidated.
//!
//! # Dependents
//!
//! Import edges and reference edges of a file are resolved against the
//! files and definitions of the generation the file was analysed in. When
//! other files appear or disappear, an unchanged file's edges can dangle or
//! miss a new target. T1 therefore also re-analyses (from stored text) the
//! unchanged files that
//!
//! - import a path this build removed (all languages), or a changed Rust file
//!   whose scoped import bindings must be checked against its declarations,
//! - have an unresolved import whose specifier now resolves to a path this
//!   build added,
//! - reference or call a symbol whose definition this build removed,
//!
//! at most [`MAX_DEPENDENTS`] of them per build (sorted by path; the rest
//! keep their edges until they change themselves, and the cut is logged and
//! counted). Their content did not change, so they are not re-chunked or
//! re-embedded. An unchanged file whose identifiers could now match a *new*
//! definition is only re-resolved through one of the rules above (for
//! example its import of the new file), not by a project-wide name search.
//!
//! A stale manifest syntax policy instead refreshes every retained file from
//! stored text. This one-time evidence upgrade writes no unchanged chunk or
//! prepared-input rows; chunk and embedding format versions stay independent.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use knowell_core::{ContentHash, RepoPath};
use knowell_embed::Embedder;
use knowell_graph::EdgeKind;
use knowell_store::analysis;
use knowell_store::content::{self, NewChunkInput};
use knowell_store::graph::{self, NodeRef};
use knowell_store::symbols::{self, NewOccurrence, NewSymbol};
use knowell_store::views::GenerationPin;
use knowell_store::{OccurrenceRole, PgConnection, StoreError, SymbolId};

use crate::analyze::{
    AnalysedFile, import_targets, parser_version_tag, resolve_import, specifier_tails,
    split_symbol_key, symbol_key, symbol_rows, syntactic_edges,
};
use crate::context::ViewContext;
use crate::error::IndexError;
use crate::indexer::{Counters, Inner, JobRun};
use crate::jobs::StagePayload;
use crate::manifest;
use crate::pipeline::{Delta, JobOutcome, Purpose, Upsert, derive_delta, files_map};
use crate::references::{
    FileDefs, FileReferences, MAX_IMPORTED_FILES, MAX_SIBLING_FILES, family, file_references,
    path_family,
};
use crate::status::{Tier, TierState};

/// Chunk rows per store call.
const CHUNK_BATCH: usize = 2_000;
/// Files whose symbols, occurrences and edges are written per store call.
const FILE_BATCH: usize = 200;
/// Files whose chunk inputs are written per store call.
const INPUT_FILE_BATCH: usize = 500;
/// Files whose stored definitions are read per store call.
const PATH_BATCH: usize = 1_000;
/// Most unchanged files re-resolved per build (see the module docs).
pub(crate) const MAX_DEPENDENTS: usize = 500;

/// The path an edge's origin names, for T1 edges (whose origin is the
/// analysed file's path).
fn origin_path(origin: &str) -> Option<RepoPath> {
    RepoPath::new(origin).ok()
}

impl<E: Embedder + 'static> Inner<E> {
    /// Keeps the ids of a moved file's symbols: symbols defined at `from`
    /// in the active generation that still exist (same kind and in-file
    /// name) are renamed to the new path.
    async fn carry_symbols(
        &self,
        conn: &mut PgConnection,
        ctx: &ViewContext,
        active: i64,
        from: &RepoPath,
        file: &AnalysedFile,
    ) -> Result<(), IndexError> {
        let wanted: BTreeSet<(&str, &str)> = file
            .parsed
            .symbols
            .iter()
            .map(|s| (s.kind.as_str(), s.qualified_name.as_str()))
            .collect();
        let pin = GenerationPin {
            view: ctx.view,
            generation: active,
        };
        let definitions =
            symbols::definitions_in_paths(conn, pin, std::slice::from_ref(from)).await?;
        let mut done: BTreeSet<SymbolId> = BTreeSet::new();
        for definition in definitions {
            let symbol = definition.symbol;
            if !done.insert(symbol.id) {
                continue;
            }
            let Some((path, local)) = split_symbol_key(&symbol.qualified_name) else {
                continue;
            };
            if &path != from || !wanted.contains(&(symbol.kind.as_str(), local)) {
                continue;
            }
            let renamed = symbol_key(&file.path, local);
            match symbols::rename_symbol(conn, symbol.id, &renamed).await {
                Ok(_) => Counters::add(&self.stats.symbols_renamed, 1),
                // A symbol of that name exists already (the file moved back
                // to a path it had before); the older identity wins.
                Err(StoreError::AlreadyExists { .. }) => {
                    tracing::debug!(%renamed, "symbol identity at the new path already exists");
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// T1 of generation `g`; queues T3.
    pub(crate) async fn stage_symbols(
        self: &Arc<Self>,
        p: StagePayload,
        run: &JobRun,
    ) -> Result<JobOutcome, IndexError> {
        let ctx = self.context(p.view)?;
        let generation = p
            .generation
            .ok_or_else(|| IndexError::invalid("job payload", "the stage needs a generation"))?;
        let _guard = self.view_lock(p.view).await;
        let mut conn = self.store.acquire().await?;
        let active = match self.check_building(&mut conn, &ctx, &p, generation).await? {
            Ok(active) => active,
            Err(reason) => return Ok(JobOutcome::Superseded(reason)),
        };
        self.set_tier(&ctx, generation, &p.target, Tier::T1, TierState::Running);
        let pin = GenerationPin {
            view: p.view,
            generation,
        };
        let delta = derive_delta(&mut conn, p.view, generation, active).await?;
        let hashes = files_map(&mut conn, pin).await?;
        let files: BTreeSet<RepoPath> = hashes.keys().cloned().collect();
        let analysed = self
            .analysed_upserts(
                &mut conn,
                &ctx,
                generation,
                &delta.upserts,
                run,
                Purpose::Symbols,
            )
            .await?;
        if let Some(active) = active {
            for up in &delta.upserts {
                if let (Some(from), Some(file)) = (&up.renamed_from, analysed.get(&up.path)) {
                    self.carry_symbols(&mut conn, &ctx, active, from, file)
                        .await?;
                }
            }
        }
        let syntax_refresh = active.is_some_and(|active| {
            manifest::load(&self.config.data_dir, p.view, active)
                .is_none_or(|recorded| recorded.needs_syntax_refresh())
        });
        let dependents = match active {
            // A policy upgrade is complete only after every retained path has
            // current evidence, so it cannot use the ordinary dependents cap.
            Some(_) if syntax_refresh => hashes
                .iter()
                .filter(|(path, _)| !analysed.contains_key(*path))
                .map(|(path, hash)| Upsert {
                    path: path.clone(),
                    hash: *hash,
                    renamed_from: None,
                })
                .collect(),
            Some(active) => {
                self.dependents(&mut conn, &ctx, pin, active, &delta, &analysed, &hashes)
                    .await?
            }
            None => Vec::new(),
        };
        let reanalysed = if dependents.is_empty() {
            BTreeMap::new()
        } else {
            self.analysed_upserts(
                &mut conn,
                &ctx,
                generation,
                &dependents,
                run,
                Purpose::Symbols,
            )
            .await?
        };
        if !syntax_refresh {
            Counters::add(&self.stats.dependents_reresolved, reanalysed.len() as u64);
        }

        // Content-level chunk rows: one set per content (ranges, kinds). The
        // first path in order writes them; the embedding input of every path
        // is recorded separately below.
        let mut claimed: BTreeSet<ContentHash> = BTreeSet::new();
        let mut rows = Vec::new();
        let mut structures = Vec::new();
        for file in analysed.values() {
            if file.chunks.is_empty() || !claimed.insert(file.content_hash) {
                continue;
            }
            rows.extend(file.chunks.iter().map(|c| c.row.clone()));
            structures.extend(file.chunks.iter().map(|c| c.structure.clone()));
        }
        for batch in rows.chunks(CHUNK_BATCH) {
            content::upsert_chunks(&mut conn, ctx.organization, batch).await?;
        }
        for batch in structures.chunks(CHUNK_BATCH) {
            content::upsert_chunk_structures(&mut conn, ctx.organization, batch).await?;
        }
        Counters::add(&self.stats.chunks_written, rows.len() as u64);
        self.write_chunk_inputs(&mut conn, pin, analysed.values())
            .await?;
        run.check()?;

        // Symbols of every analysed file first: references need the ids of
        // all of them.
        let all: BTreeMap<RepoPath, Arc<AnalysedFile>> = analysed
            .iter()
            .chain(reanalysed.iter())
            .map(|(path, file)| (path.clone(), Arc::clone(file)))
            .collect();
        let list: Vec<&Arc<AnalysedFile>> = all.values().collect();
        let mut ids: BTreeMap<RepoPath, Vec<Option<SymbolId>>> = BTreeMap::new();
        for batch in list.chunks(FILE_BATCH) {
            let mut new_symbols: Vec<NewSymbol> = Vec::new();
            let mut slots: Vec<Vec<Option<usize>>> = Vec::with_capacity(batch.len());
            for file in batch {
                let mut file_slots = Vec::new();
                for row in symbol_rows(file) {
                    match row {
                        Some(row) => {
                            file_slots.push(Some(new_symbols.len()));
                            new_symbols.push(row);
                        }
                        None => file_slots.push(None),
                    }
                }
                slots.push(file_slots);
            }
            let stored = symbols::upsert_symbols(&mut conn, ctx.project, &new_symbols).await?;
            for (file, file_slots) in batch.iter().zip(&slots) {
                let file_ids = file_slots
                    .iter()
                    .map(|slot| slot.and_then(|i| stored.get(i).copied()))
                    .collect();
                ids.insert(file.path.clone(), file_ids);
            }
        }
        run.check()?;
        let mut references = self
            .references(&mut conn, &ctx, pin, &all, &ids, &files)
            .await?;
        run.check()?;

        for batch in list.chunks(FILE_BATCH) {
            let mut occurrences = Vec::new();
            let mut edges = Vec::new();
            for file in batch {
                let empty = Vec::new();
                let file_ids = ids.get(&file.path).unwrap_or(&empty);
                for (symbol, id) in file.parsed.symbols.iter().zip(file_ids) {
                    if let Some(id) = id {
                        occurrences.push(NewOccurrence {
                            symbol: *id,
                            path: file.path.clone(),
                            content_hash: file.content_hash,
                            lines: symbol.range,
                            role: OccurrenceRole::Definition,
                        });
                    }
                }
                edges.extend(syntactic_edges(ctx.project, file, file_ids, &files));
                if let Some(found) = references.remove(&file.path) {
                    let details = serde_json::to_value(&found.coverage).map_err(|_| {
                        IndexError::invalid("reference coverage", "cannot encode syntax coverage")
                    })?;
                    analysis::upsert_coverage(
                        &mut conn,
                        ctx.organization,
                        pin,
                        &file.path,
                        &file.content_hash,
                        "syntax",
                        &details,
                    )
                    .await?;
                    occurrences.extend(found.occurrences);
                    edges.extend(found.edges);
                }
            }
            let paths: Vec<RepoPath> = batch.iter().map(|f| f.path.clone()).collect();
            let origins: Vec<String> = paths.iter().map(RepoPath::to_string).collect();
            symbols::replace_occurrences(&mut conn, p.view, generation, &paths, &occurrences)
                .await?;
            graph::replace_edges(&mut conn, p.view, generation, &origins, &edges).await?;
            run.check()?;
        }
        if !delta.removed.is_empty() {
            let origins: Vec<String> = delta.removed.iter().map(RepoPath::to_string).collect();
            symbols::replace_occurrences(&mut conn, p.view, generation, &delta.removed, &[])
                .await?;
            graph::replace_edges(&mut conn, p.view, generation, &origins, &[]).await?;
        }
        self.apply_staged_scip(&mut conn, &ctx, pin, &p).await?;
        self.cache().put_analysed(
            (p.view, generation),
            analysed.into_values().collect(),
            self.config.build_cache_bytes,
        );
        self.enqueue_next(&mut conn, Tier::T3, &p, generation)
            .await?;
        self.set_tier(&ctx, generation, &p.target, Tier::T1, TierState::Done);
        Ok(JobOutcome::Completed)
    }

    /// Records the per-path prepared inputs of the analysed files' chunks
    /// (every chunk, with whether the content policy lets it be embedded).
    pub(crate) async fn write_chunk_inputs<'a>(
        &self,
        conn: &mut PgConnection,
        pin: GenerationPin,
        files: impl Iterator<Item = &'a Arc<AnalysedFile>>,
    ) -> Result<(), IndexError> {
        let parser = parser_version_tag();
        let files: Vec<&Arc<AnalysedFile>> = files.collect();
        for batch in files.chunks(INPUT_FILE_BATCH) {
            let paths: Vec<RepoPath> = batch.iter().map(|f| f.path.clone()).collect();
            let inputs: Vec<NewChunkInput> = batch
                .iter()
                .flat_map(|file| {
                    file.chunks.iter().map(|chunk| NewChunkInput {
                        path: file.path.clone(),
                        content_hash: file.content_hash,
                        ordinal: chunk.row.ordinal,
                        prepared_input_hash: chunk.input.hash,
                        embed: file.embed,
                    })
                })
                .collect();
            content::replace_chunk_inputs(conn, pin, &parser, &paths, &inputs).await?;
        }
        Ok(())
    }

    /// Unchanged files whose import or reference edges this build
    /// invalidated (see the module docs), bounded by [`MAX_DEPENDENTS`].
    #[allow(clippy::too_many_arguments)]
    async fn dependents(
        &self,
        conn: &mut PgConnection,
        ctx: &ViewContext,
        pin: GenerationPin,
        active: i64,
        delta: &Delta,
        analysed: &BTreeMap<RepoPath, Arc<AnalysedFile>>,
        hashes: &BTreeMap<RepoPath, ContentHash>,
    ) -> Result<Vec<Upsert>, IndexError> {
        let imports = EdgeKind::Imports.as_str();
        let mut found: BTreeSet<RepoPath> = BTreeSet::new();
        if !delta.removed.is_empty() {
            let targets: Vec<NodeRef> = delta
                .removed
                .iter()
                .map(|path| NodeRef::File {
                    project: ctx.project,
                    path: path.clone(),
                })
                .collect();
            for edge in graph::edges_into(conn, pin, imports, &targets).await? {
                found.extend(origin_path(&edge.edge.origin));
            }
        }
        // Rust aliases name a particular declaration inside an imported file.
        // Refresh those bindings when that file changes. Generic name-based
        // importers do not acquire a new dependency merely because an unrelated
        // declaration was appended: preserving that boundary keeps one-file
        // edits and rewritten-history reuse incremental.
        let changed_rust: Vec<NodeRef> = delta
            .upserts
            .iter()
            .filter(|up| {
                analysed
                    .get(&up.path)
                    .is_some_and(|file| file.parsed.language == knowell_parse::Language::Rust)
            })
            .map(|up| NodeRef::File {
                project: ctx.project,
                path: up.path.clone(),
            })
            .collect();
        if !changed_rust.is_empty() {
            for edge in graph::edges_into(conn, pin, imports, &changed_rust).await? {
                if let Some(path) = origin_path(&edge.edge.origin)
                    && path_family(&path) == Some("rust")
                {
                    found.insert(path);
                }
            }
        }
        if !delta.added.is_empty() {
            let tails: Vec<String> = delta
                .added
                .iter()
                .flat_map(specifier_tails)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let files: BTreeSet<RepoPath> = hashes.keys().cloned().collect();
            for edge in
                graph::edges_into_name_tails(conn, pin, imports, ctx.project, &tails).await?
            {
                let (Some(from), NodeRef::Name { name, .. }) =
                    (origin_path(&edge.edge.origin), &edge.edge.to)
                else {
                    continue;
                };
                if resolve_import(&from, name, &files).is_some() {
                    found.insert(from);
                }
            }
        }
        let touched: Vec<RepoPath> = delta
            .removed
            .iter()
            .chain(delta.upserts.iter().map(|u| &u.path))
            .cloned()
            .collect();
        if !touched.is_empty() {
            let before = GenerationPin {
                view: ctx.view,
                generation: active,
            };
            let now: BTreeSet<(String, String)> = analysed
                .values()
                .flat_map(|file| symbol_rows(file).into_iter().flatten())
                .map(|row| (row.kind, row.qualified_name))
                .collect();
            let mut vanished: BTreeSet<SymbolId> = BTreeSet::new();
            for batch in touched.chunks(PATH_BATCH) {
                for definition in symbols::definitions_in_paths(conn, before, batch).await? {
                    let key = (definition.symbol.kind, definition.symbol.qualified_name);
                    if !now.contains(&key) {
                        vanished.insert(definition.symbol.id);
                    }
                }
            }
            if !vanished.is_empty() {
                let targets: Vec<NodeRef> = vanished.into_iter().map(NodeRef::Symbol).collect();
                for kind in [EdgeKind::References, EdgeKind::Calls] {
                    for edge in graph::edges_into(conn, pin, kind.as_str(), &targets).await? {
                        found.extend(origin_path(&edge.edge.origin));
                    }
                }
            }
        }
        let changed: BTreeSet<&RepoPath> = delta.upserts.iter().map(|u| &u.path).collect();
        found.retain(|path| !changed.contains(path) && hashes.contains_key(path));
        if found.len() > MAX_DEPENDENTS {
            let skipped = found.len() - MAX_DEPENDENTS;
            tracing::warn!(
                project = %ctx.project_name,
                skipped,
                "more dependents than the per-build bound; the rest keep their edges until they change"
            );
            Counters::add(&self.stats.dependents_skipped, skipped as u64);
        }
        Ok(found
            .into_iter()
            .take(MAX_DEPENDENTS)
            .filter_map(|path| {
                let hash = *hashes.get(&path)?;
                Some(Upsert {
                    path,
                    hash,
                    renamed_from: None,
                })
            })
            .collect())
    }

    /// Reference occurrences and edges of the analysed files (see
    /// [`crate::references`]). Candidate definitions of files not analysed
    /// in this build are read from the store at `pin`.
    async fn references(
        &self,
        conn: &mut PgConnection,
        ctx: &ViewContext,
        pin: GenerationPin,
        all: &BTreeMap<RepoPath, Arc<AnalysedFile>>,
        ids: &BTreeMap<RepoPath, Vec<Option<SymbolId>>>,
        files: &BTreeSet<RepoPath>,
    ) -> Result<BTreeMap<RepoPath, FileReferences>, IndexError> {
        let referencing: Vec<Arc<AnalysedFile>> = all.values().cloned().collect();
        if referencing.is_empty() {
            return Ok(BTreeMap::new());
        }
        let mut by_dir: BTreeMap<(Option<RepoPath>, &'static str), Vec<RepoPath>> = BTreeMap::new();
        for path in files {
            if let Some(family) = path_family(path) {
                by_dir
                    .entry((path.parent(), family))
                    .or_default()
                    .push(path.clone());
            }
        }
        let mut plans = Vec::new();
        let mut needed: BTreeSet<RepoPath> = BTreeSet::new();
        for file in referencing {
            let rust = (file.parsed.language == knowell_parse::Language::Rust).then(|| {
                crate::rust_imports::resolve(&file.path, &file.text, files, &file.parse_limits)
            });
            let mut imported: Vec<RepoPath> = if let Some(imports) = &rust {
                let mut paths = imports.target_paths();
                for ident in &file.identifiers {
                    if let Some(qualified) = &ident.qualified {
                        let segments: Vec<String> =
                            qualified.split('.').map(str::to_owned).collect();
                        if let Some((path, _)) =
                            imports.qualified_target(&file.path, files, &segments, ident.start_byte)
                        {
                            paths.insert(path);
                        }
                    }
                }
                paths.into_iter().filter(|p| p != &file.path).collect()
            } else {
                import_targets(&file, files)
            };
            let imports_cut = imported.len() > MAX_IMPORTED_FILES;
            imported.truncate(MAX_IMPORTED_FILES);
            let siblings: Vec<RepoPath> = family(file.parsed.language)
                .and_then(|fam| by_dir.get(&(file.path.parent(), fam)))
                .filter(|list| list.len() <= MAX_SIBLING_FILES)
                .map(|list| list.iter().filter(|p| **p != file.path).cloned().collect())
                .unwrap_or_default();
            needed.extend(
                imported
                    .iter()
                    .chain(siblings.iter())
                    .filter(|p| !all.contains_key(*p))
                    .cloned(),
            );
            plans.push((file, imported, siblings, rust, imports_cut));
        }
        let mut defs: BTreeMap<RepoPath, FileDefs> = BTreeMap::new();
        for (path, file) in all {
            let empty = Vec::new();
            defs.insert(
                path.clone(),
                FileDefs::from_analysis(file, ids.get(path).unwrap_or(&empty)),
            );
        }
        let needed: Vec<RepoPath> = needed.into_iter().collect();
        for batch in needed.chunks(PATH_BATCH) {
            let mut grouped: BTreeMap<RepoPath, Vec<symbols::Definition>> = BTreeMap::new();
            for definition in symbols::definitions_in_paths(conn, pin, batch).await? {
                grouped
                    .entry(definition.path.clone())
                    .or_default()
                    .push(definition);
            }
            for (path, list) in grouped {
                defs.insert(path, FileDefs::from_definitions(list.iter()));
            }
        }
        let ids = ids.clone();
        let files = files.clone();
        let project = ctx.project;
        let resolved = tokio::task::spawn_blocking(move || {
            let empty_defs = FileDefs::default();
            let empty_ids = Vec::new();
            let mut out = BTreeMap::new();
            for (file, imported, siblings, rust, imports_cut) in plans {
                let own = defs.get(&file.path).unwrap_or(&empty_defs);
                let imported: Vec<&FileDefs> = imported
                    .iter()
                    .filter_map(|p| defs.get(p))
                    .filter(|d| !d.is_empty())
                    .collect();
                let siblings: Vec<&FileDefs> = siblings
                    .iter()
                    .filter_map(|p| defs.get(p))
                    .filter(|d| !d.is_empty())
                    .collect();
                let file_ids = ids.get(&file.path).unwrap_or(&empty_ids);
                let mut found = file_references(
                    project,
                    &file,
                    file_ids,
                    own,
                    &imported,
                    &siblings,
                    rust.as_ref().map(|r| (r, &files)),
                );
                found.coverage.truncated |= imports_cut;
                out.insert(file.path.clone(), found);
            }
            out
        })
        .await?;
        let written: usize = resolved
            .values()
            .map(|r: &FileReferences| r.edges.len() + r.occurrences.len())
            .sum();
        Counters::add(&self.stats.references_written, written as u64);
        Ok(resolved)
    }
}
