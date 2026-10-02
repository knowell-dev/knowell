//! Structured extractors written in Rust, for formats whose meaning lives in
//! document structure rather than in code patterns (YAML / JSON documents,
//! Protocol Buffers, Prisma schemas). Packs enable them with
//! `[[extractors]]` entries; see the crate README.

mod api;
mod deploy;
mod locale;
mod prisma;
mod proto;
mod sql_ddl;
pub(crate) mod tree;

use std::collections::BTreeMap;

use knowell_core::{ContentHash, LineRange, Name, RepoPath};
use knowell_graph::{ContractKind, EvidenceType};
use knowell_parse::tree_sitter::Tree;
use knowell_parse::{Language, ParseLimits};

use crate::model::{Extraction, Role, SymbolRef};
use crate::normalize::{Normalizer, normalize};
use crate::pack::StructuredKind;

/// Where an extractor runs.
pub(crate) struct Ctx<'a> {
    pub(crate) project: &'a Name,
    pub(crate) path: &'a RepoPath,
    pub(crate) content_hash: ContentHash,
    pub(crate) pack: String,
    pub(crate) rule: &'a str,
    pub(crate) limits: &'a ParseLimits,
}

impl Ctx<'_> {
    /// Builds an extraction, normalising `raw` with the default normaliser
    /// of `kind`. `None` when the key is not valid for the kind.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn extraction(
        &self,
        kind: ContractKind,
        role: Role,
        raw: &str,
        range: LineRange,
        symbol: Option<SymbolRef>,
        evidence: EvidenceType,
        attrs: BTreeMap<String, String>,
    ) -> Option<Extraction> {
        let normalizer = match kind {
            ContractKind::Endpoint => Normalizer::Http,
            ContractKind::Topic => Normalizer::Topic,
            ContractKind::EnvName => Normalizer::Env,
            ContractKind::I18nKey => Normalizer::I18n,
            ContractKind::Table => Normalizer::Table,
            ContractKind::Rpc => Normalizer::Rpc,
            ContractKind::Package | ContractKind::Infra => Normalizer::Plain,
        };
        let normalized = normalize(normalizer, raw, "*")?;
        if normalized.key.is_empty() || normalized.unresolved {
            return None;
        }
        Some(Extraction {
            project: self.project.clone(),
            path: self.path.clone(),
            content_hash: self.content_hash,
            range,
            kind,
            role,
            key: normalized.key,
            dynamic: normalized.dynamic,
            symbol,
            evidence,
            pack: self.pack.clone(),
            rule: self.rule.to_owned(),
            attrs,
        })
    }

    /// Parses `text` as `language` within the limits.
    pub(crate) fn tree(&self, language: Language, text: &str) -> Option<Tree> {
        knowell_parse::parse_tree(language, text, self.limits)
    }
}

/// Runs one structured extractor over a file. Files that are not of the
/// extractor's format yield nothing.
pub(crate) fn run(kind: StructuredKind, ctx: &Ctx<'_>, text: &str) -> Vec<Extraction> {
    match kind {
        StructuredKind::OpenApi => api::openapi(ctx, text),
        StructuredKind::AsyncApi => api::asyncapi(ctx, text),
        StructuredKind::EventSchema => api::event_schema(ctx, text),
        StructuredKind::Proto => proto::extract(ctx, text),
        StructuredKind::ComposeEnv => deploy::compose_env(ctx, text),
        StructuredKind::KubernetesEnv => deploy::kubernetes_env(ctx, text),
        StructuredKind::Infra => deploy::infra(ctx, text),
        StructuredKind::LocaleJson => locale::locale_json(ctx, text),
        StructuredKind::Arb => locale::arb(ctx, text),
        StructuredKind::Prisma => prisma::extract(ctx, text),
        StructuredKind::SqlDdl => sql_ddl::extract(ctx, text),
    }
}

/// The language of a YAML or JSON file by extension (`.arb` is JSON).
pub(crate) fn data_language(path: &RepoPath) -> Option<Language> {
    match path.extension().map(str::to_ascii_lowercase).as_deref() {
        Some("yaml" | "yml") => Some(Language::Yaml),
        Some("json" | "arb") => Some(Language::Json),
        _ => None,
    }
}

/// Lower-case hex of a hash over `parts` with a format tag.
pub(crate) fn hash_hex(tag: &str, parts: &[&str]) -> String {
    let bytes = std::iter::once(tag.as_bytes()).chain(parts.iter().map(|p| p.as_bytes()));
    ContentHash::of_parts(bytes).to_string()
}
