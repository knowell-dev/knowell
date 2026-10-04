//! Optional, tenant-scoped persisted products of parsing redacted source.
//!
//! This caches a local parse only. Store ids, view pins and resolved relations
//! are rebuilt for every snapshot; a cache hit says nothing about task coverage.

use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};

use knowell_core::{ContentHash, RepoPath};
use knowell_index::parser_version_tag;
use knowell_parse::{Language, ParseLimits, ParsedFile, parse_with};
use knowell_store::OrganizationId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::settings::ParseProductCacheSettings;

const SCHEMA_VERSION: u32 = 1;
const MAX_ENTRY_BYTES: usize = 16 * 1024 * 1024;
const MAX_ENTRIES: usize = 65_536;
const LOCK_NAME: &str = "budget.lock";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Key {
    schema: u32,
    organization: OrganizationId,
    path: RepoPath,
    redacted_hash: ContentHash,
    parser: String,
    limits: ParseLimits,
}

#[derive(Serialize)]
struct EnvelopeRef<'a> {
    key: &'a Key,
    product_hash: ContentHash,
    product: &'a ParsedFile,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    key: Key,
    product_hash: ContentHash,
    product: ParsedFile,
}

#[derive(Debug, thiserror::Error)]
enum CacheError {
    #[error("parse product cache io failed")]
    Io(#[from] io::Error),
    #[error("parse product cache entry is invalid")]
    InvalidEntry,
    #[error("parse product cache entry exceeds its byte limit")]
    EntryTooLarge,
    #[error("parse product cache disk budget is exhausted")]
    BudgetExhausted,
    #[error("parse product cache writer is busy")]
    Busy,
    #[error("parse product cache path is not a regular file or directory")]
    UnsafePath,
}

impl CacheError {
    fn kind(&self) -> &'static str {
        match self {
            Self::Io(_) => "io",
            Self::InvalidEntry => "invalid_entry",
            Self::EntryTooLarge => "entry_too_large",
            Self::BudgetExhausted => "budget_exhausted",
            Self::Busy => "writer_busy",
            Self::UnsafePath => "unsafe_path",
        }
    }
}

/// Persisted local parse products, with a separate bounded namespace per tenant.
#[derive(Debug)]
pub(crate) struct ParseProductCache {
    settings: ParseProductCacheSettings,
    organization: OrganizationId,
    namespace: PathBuf,
}

impl ParseProductCache {
    pub(crate) fn new(settings: ParseProductCacheSettings, organization: OrganizationId) -> Self {
        let namespace = settings
            .directory
            .join(format!("parse-products-v{SCHEMA_VERSION}"))
            .join(organization.to_string());
        Self {
            settings,
            organization,
            namespace,
        }
    }

    fn enabled(&self) -> bool {
        self.settings.max_entry_bytes > 0
            && self.settings.max_entry_bytes <= MAX_ENTRY_BYTES
            && self.settings.max_entries > 0
            && self.settings.max_entries <= MAX_ENTRIES
            && self.settings.max_total_bytes > 0
    }

    fn key(&self, path: &RepoPath, text: &str, limits: ParseLimits) -> Key {
        Key {
            schema: SCHEMA_VERSION,
            organization: self.organization,
            path: path.clone(),
            redacted_hash: ContentHash::of(text.as_bytes()),
            parser: parser_version_tag(),
            limits,
        }
    }

    fn entry_path(&self, key: &Key) -> Result<PathBuf, CacheError> {
        let bytes = bounded_json(key, self.settings.max_entry_bytes)?;
        Ok(self
            .namespace
            .join(format!("{}.json", ContentHash::of(&bytes))))
    }

    /// Returns the exact local parse and whether its validated product was reused.
    /// A corrupt or unavailable optimization always reparses this same source.
    pub(crate) fn parse(
        &self,
        path: &RepoPath,
        text: &str,
        limits: ParseLimits,
    ) -> (ParsedFile, bool) {
        if !self.enabled() {
            tracing::debug!(
                error_kind = "invalid_settings",
                "parse product cache disabled"
            );
            return (parse_with(path, text, &limits, None), false);
        }
        let key = self.key(path, text, limits);
        match self.read(&key, text) {
            Ok(Some(product)) => return (product, true),
            Ok(None) => {}
            Err(error) => {
                tracing::debug!(error_kind = error.kind(), "parse product cache read missed");
            }
        }
        let product = parse_with(path, text, &limits, None);
        // Timeout/cancellation and all other partial products stay observable,
        // and a temporary partial parse cannot poison future cache hits.
        if product.degraded.is_none()
            && let Err(error) = self.write(&key, &product)
        {
            tracing::debug!(
                error_kind = error.kind(),
                "parse product cache write skipped"
            );
        }
        (product, false)
    }

    fn read(&self, key: &Key, text: &str) -> Result<Option<ParsedFile>, CacheError> {
        if !self.valid_namespace()? {
            return Ok(None);
        }
        let path = self.entry_path(key)?;
        let Some(metadata) = regular_file(&path)? else {
            return Ok(None);
        };
        let limit = self.settings.max_entry_bytes as u64;
        if metadata.len() > limit {
            return Err(CacheError::EntryTooLarge);
        }
        let file = File::open(&path)?;
        let mut bytes = Vec::new();
        file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
        if bytes.len() > self.settings.max_entry_bytes {
            return Err(CacheError::EntryTooLarge);
        }
        let envelope: Envelope =
            serde_json::from_slice(&bytes).map_err(|_| CacheError::InvalidEntry)?;
        let wire: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| CacheError::InvalidEntry)?;
        let canonical =
            serde_json::to_value(&envelope.product).map_err(|_| CacheError::InvalidEntry)?;
        let product_bytes = bounded_json(&envelope.product, self.settings.max_entry_bytes)?;
        if envelope.key != *key
            || wire.get("product") != Some(&canonical)
            || envelope.product_hash != ContentHash::of(&product_bytes)
            || !valid_product(&envelope.product, key, text)
        {
            return Err(CacheError::InvalidEntry);
        }
        Ok(Some(envelope.product))
    }

    fn write(&self, key: &Key, product: &ParsedFile) -> Result<(), CacheError> {
        let product_bytes = bounded_json(product, self.settings.max_entry_bytes)?;
        let bytes = bounded_json(
            &EnvelopeRef {
                key,
                product_hash: ContentHash::of(&product_bytes),
                product,
            },
            self.settings.max_entry_bytes,
        )?;
        self.ensure_namespace()?;
        let lock_path = self.namespace.join(LOCK_NAME);
        regular_file(&lock_path)?;
        let mut options = private_options();
        let lock = options
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        lock.try_lock().map_err(|_| CacheError::Busy)?;
        // The OS lock coordinates independent CLI processes. Account for the
        // temporary file as well as the old destination before writing anything.
        self.reserve(bytes.len() as u64)?;
        let destination = self.entry_path(key)?;
        regular_file(&destination)?;
        let temporary = self
            .namespace
            .join(format!("pending-{}.tmp", Uuid::now_v7()));
        let mut created = false;
        let result = (|| {
            let mut options = private_options();
            let mut file = options.write(true).create_new(true).open(&temporary)?;
            created = true;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, &destination)?;
            Ok(())
        })();
        if result.is_err() && created {
            // Only this attempt's freshly created, namespace-local file is removed.
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn ensure_namespace(&self) -> Result<(), CacheError> {
        self.valid_namespace()?;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        builder.create(&self.namespace)?;
        self.valid_namespace()?;
        Ok(())
    }

    fn valid_namespace(&self) -> Result<bool, CacheError> {
        for path in [
            self.settings.directory.as_path(),
            self.namespace.parent().ok_or(CacheError::UnsafePath)?,
            self.namespace.as_path(),
        ] {
            let metadata = match fs::symlink_metadata(path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error.into()),
            };
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(CacheError::UnsafePath);
            }
        }
        Ok(true)
    }

    fn reserve(&self, incoming: u64) -> Result<(), CacheError> {
        let mut count = 0usize;
        let mut bytes = incoming;
        for entry in fs::read_dir(&self.namespace)? {
            let entry = entry?;
            if entry.file_name() == LOCK_NAME {
                continue;
            }
            count = count.saturating_add(1);
            if count >= self.settings.max_entries {
                return Err(CacheError::BudgetExhausted);
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(CacheError::UnsafePath);
            }
            bytes = bytes.saturating_add(metadata.len());
            if bytes > self.settings.max_total_bytes {
                return Err(CacheError::BudgetExhausted);
            }
        }
        if bytes > self.settings.max_total_bytes {
            return Err(CacheError::BudgetExhausted);
        }
        Ok(())
    }
}

fn regular_file(path: &Path) -> Result<Option<Metadata>, CacheError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            Ok(Some(metadata))
        }
        Ok(_) => Err(CacheError::UnsafePath),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn private_options() -> OpenOptions {
    let options = OpenOptions::new();
    #[cfg(unix)]
    let options = {
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut options = options;
        options.mode(0o600);
        options
    };
    options
}

struct BoundedBuffer {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for BoundedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) > self.limit {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "byte limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn bounded_json(value: &impl Serialize, limit: usize) -> Result<Vec<u8>, CacheError> {
    let mut buffer = BoundedBuffer {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut buffer, value).map_err(|_| CacheError::EntryTooLarge)?;
    Ok(buffer.bytes)
}

fn valid_product(product: &ParsedFile, key: &Key, text: &str) -> bool {
    let language = Language::detect(&key.path, text);
    let line_count = if text.is_empty() {
        0
    } else {
        crate::snapshot::line_count(text)
    };
    if product.path != key.path
        || product.content_hash != key.redacted_hash
        || product.byte_len != text.len()
        || product.line_count != line_count
        || product.language != language
        || product.tier != language.tier()
        || product.degraded.is_some()
        || product.symbols.len() > key.limits.max_symbols
        || product.blocks.len() > key.limits.max_symbols
    {
        return false;
    }
    let valid_range =
        |range: &Range<usize>| range.start <= range.end && text.get(range.clone()).is_some();
    for (index, symbol) in product.symbols.iter().enumerate() {
        if !valid_range(&symbol.byte_range)
            || symbol.range.end() > line_count
            || symbol.name_line < symbol.range.start()
            || symbol.name_line > symbol.range.end()
            || symbol.parent.is_some_and(|parent| parent >= index)
            // The parser can append a space plus ellipsis after its 12-line cut.
            || symbol.signature.len() > 600 + " …".len()
            || symbol
                .doc
                .as_ref()
                .is_some_and(|doc| doc.len() > 2_000 + "…".len())
        {
            return false;
        }
    }
    product
        .imports
        .iter()
        .all(|import| import.range.end() <= line_count && import.specifier.len() <= 512 + "…".len())
        && product
            .blocks
            .iter()
            .all(|block| block.range.end() <= line_count && valid_range(&block.byte_range))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use super::*;

    fn cache(directory: &Path, organization: u128) -> ParseProductCache {
        ParseProductCache::new(
            ParseProductCacheSettings::new(directory),
            OrganizationId(Uuid::from_u128(organization)),
        )
    }

    const TEXT: &str = "/// synthetic decoding entry.\npub fn decode() -> u32 { 7 }\n";

    #[test]
    fn identical_yaml_bodies_keep_path_dependent_dialects_separate() {
        let directory = tempfile::tempdir().unwrap();
        let cache = cache(directory.path(), 1);
        let compose_path = RepoPath::new("compose.yaml").unwrap();
        let generic_path = RepoPath::new("settings.yaml").unwrap();
        let text = "services:\n  fixture:\n    image: synthetic:1\n";
        let limits = ParseLimits::default();
        let (compose, reused) = cache.parse(&compose_path, text, limits);
        assert!(!reused);
        let (generic, reused) = cache.parse(&generic_path, text, limits);
        assert!(!reused);
        assert_eq!(compose.language, generic.language);
        assert_ne!(compose.dialect, generic.dialect);
        assert_eq!(cache.parse(&compose_path, text, limits), (compose, true));
        assert_eq!(cache.parse(&generic_path, text, limits), (generic, true));
    }

    #[test]
    fn persisted_reuse_requires_exact_body_path_limits_parser_and_tenant() {
        let directory = tempfile::tempdir().unwrap();
        let path = RepoPath::new("src/reader.rs").unwrap();
        let limits = ParseLimits::default();
        let first = cache(directory.path(), 1);
        let (parsed, reused) = first.parse(&path, TEXT, limits);
        assert!(!reused);
        let second = cache(directory.path(), 1);
        assert_eq!(second.parse(&path, TEXT, limits), (parsed.clone(), true));
        assert!(!second.parse(&path, "pub fn changed() {}\n", limits).1);
        assert!(
            !second
                .parse(&RepoPath::new("src/copy.rs").unwrap(), TEXT, limits)
                .1
        );
        let changed_limits = ParseLimits {
            max_symbols: limits.max_symbols + 1,
            ..limits
        };
        assert!(!second.parse(&path, TEXT, changed_limits).1);
        assert!(!cache(directory.path(), 2).parse(&path, TEXT, limits).1);
        let mut key = second.key(&path, TEXT, limits);
        key.parser = "p-future".into();
        second.write(&key, &parsed).unwrap();
        assert_ne!(
            second.entry_path(&key).unwrap(),
            second.entry_path(&second.key(&path, TEXT, limits)).unwrap()
        );
    }

    #[test]
    fn longest_line_truncated_signature_is_reused() {
        let directory = tempfile::tempdir().unwrap();
        let cache = cache(directory.path(), 1);
        let path = RepoPath::new("src/long_signature.rs").unwrap();
        let limits = ParseLimits::default();
        let mut lines = vec!["pub fn f(".to_owned()];
        lines.extend((0..11).map(|index| format!("    argument_{index}: u32,")));
        let padding = 600usize.checked_sub(lines.join("\n").len()).unwrap();
        lines[0] = format!("pub fn f{}(", "x".repeat(padding));
        assert_eq!(lines.join("\n").len(), 600);
        lines.push(") {}".to_owned());
        let text = format!("{}\n", lines.join("\n"));
        let (expected, reused) = cache.parse(&path, &text, limits);
        assert!(!reused);
        assert!(expected.degraded.is_none());
        assert_eq!(expected.symbols[0].signature.len(), 604);
        assert!(expected.symbols[0].signature.ends_with(" …"));
        assert_eq!(cache.parse(&path, &text, limits), (expected, true));
    }

    #[test]
    fn malformed_truncated_wrong_identity_and_wrong_digest_reparse_exact_source() {
        let directory = tempfile::tempdir().unwrap();
        let cache = cache(directory.path(), 1);
        let path = RepoPath::new("src/reader.rs").unwrap();
        let limits = ParseLimits::default();
        let (expected, _) = cache.parse(&path, TEXT, limits);
        let key = cache.key(&path, TEXT, limits);
        let entry = cache.entry_path(&key).unwrap();
        let original = fs::read(&entry).unwrap();
        for bytes in [
            b"{malformed".to_vec(),
            original.get(..original.len() / 2).unwrap().to_vec(),
        ] {
            fs::write(&entry, bytes).unwrap();
            assert_eq!(cache.parse(&path, TEXT, limits), (expected.clone(), false));
        }
        for field in [
            "path",
            "redacted_hash",
            "parser",
            "limits",
            "schema",
            "organization",
        ] {
            let mut json: serde_json::Value = serde_json::from_slice(&original).unwrap();
            match field {
                "path" => json["key"][field] = serde_json::json!("src/other.rs"),
                "redacted_hash" => {
                    json["key"][field] = serde_json::to_value(ContentHash::of(b"other")).unwrap()
                }
                "parser" => json["key"][field] = serde_json::json!("p-future"),
                "limits" => json["key"][field]["max_symbols"] = serde_json::json!(1),
                "schema" => json["key"][field] = serde_json::json!(99),
                "organization" => {
                    json["key"][field] =
                        serde_json::to_value(OrganizationId(Uuid::from_u128(2))).unwrap()
                }
                _ => unreachable!(),
            }
            fs::write(&entry, serde_json::to_vec(&json).unwrap()).unwrap();
            assert_eq!(cache.parse(&path, TEXT, limits), (expected.clone(), false));
        }
        let mut json: serde_json::Value = serde_json::from_slice(&original).unwrap();
        json["product"]["symbols"][0]["signature"] = serde_json::json!("forged signature");
        fs::write(&entry, serde_json::to_vec(&json).unwrap()).unwrap();
        assert_eq!(cache.parse(&path, TEXT, limits), (expected, false));
    }

    #[test]
    fn partial_products_and_oversized_entries_are_never_cached() {
        let directory = tempfile::tempdir().unwrap();
        let mut settings = ParseProductCacheSettings::new(directory.path());
        settings.max_entry_bytes = 64;
        let tiny = ParseProductCache::new(settings, OrganizationId(Uuid::from_u128(1)));
        let path = RepoPath::new("reader.rs").unwrap();
        let limits = ParseLimits::default();
        assert!(!tiny.parse(&path, TEXT, limits).1);
        assert!(!tiny.parse(&path, TEXT, limits).1);
        let cache = cache(directory.path(), 1);
        let small = ParseLimits {
            max_bytes: 1,
            ..limits
        };
        assert!(cache.parse(&path, TEXT, small).0.degraded.is_some());
        assert!(!cache.parse(&path, TEXT, small).1);
        assert!(
            !cache
                .entry_path(&cache.key(&path, TEXT, small))
                .unwrap()
                .exists()
        );
    }

    #[test]
    fn invalid_product_ranges_and_parents_are_rejected_even_with_matching_digest() {
        let directory = tempfile::tempdir().unwrap();
        let cache = cache(directory.path(), 1);
        let path = RepoPath::new("src/reader.rs").unwrap();
        let limits = ParseLimits::default();
        let (expected, _) = cache.parse(&path, TEXT, limits);
        let key = cache.key(&path, TEXT, limits);
        let mut wrong = expected.clone();
        wrong.symbols[0].parent = Some(0);
        cache.write(&key, &wrong).unwrap();
        assert_eq!(cache.parse(&path, TEXT, limits), (expected.clone(), false));
        wrong = expected.clone();
        wrong.symbols[0].byte_range = 0..TEXT.len() + 1;
        cache.write(&key, &wrong).unwrap();
        assert_eq!(cache.parse(&path, TEXT, limits), (expected.clone(), false));
        wrong = expected.clone();
        wrong.path = RepoPath::new("src/wrong.rs").unwrap();
        cache.write(&key, &wrong).unwrap();
        assert_eq!(cache.parse(&path, TEXT, limits), (expected, false));
    }

    #[test]
    fn reads_are_byte_bounded_and_unknown_product_fields_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let mut settings = ParseProductCacheSettings::new(directory.path());
        settings.max_entry_bytes = 4096;
        let cache = ParseProductCache::new(settings, OrganizationId(Uuid::from_u128(1)));
        let path = RepoPath::new("reader.rs").unwrap();
        let limits = ParseLimits::default();
        let (expected, _) = cache.parse(&path, TEXT, limits);
        let entry = cache.entry_path(&cache.key(&path, TEXT, limits)).unwrap();
        fs::write(&entry, vec![b' '; 4097]).unwrap();
        assert_eq!(cache.parse(&path, TEXT, limits), (expected.clone(), false));
        let mut json: serde_json::Value =
            serde_json::from_slice(&fs::read(&entry).unwrap()).unwrap();
        json["product"]["unknown_fixture_field"] = serde_json::json!(true);
        fs::write(&entry, serde_json::to_vec(&json).unwrap()).unwrap();
        assert_eq!(cache.parse(&path, TEXT, limits), (expected, false));
    }

    #[test]
    fn total_byte_budget_blocks_writes_without_changing_parse_results() {
        let directory = tempfile::tempdir().unwrap();
        let mut settings = ParseProductCacheSettings::new(directory.path());
        settings.max_total_bytes = 1;
        let cache = ParseProductCache::new(settings, OrganizationId(Uuid::from_u128(1)));
        let path = RepoPath::new("reader.rs").unwrap();
        let limits = ParseLimits::default();
        let expected = parse_with(&path, TEXT, &limits, None);
        assert_eq!(cache.parse(&path, TEXT, limits), (expected.clone(), false));
        assert_eq!(cache.parse(&path, TEXT, limits), (expected, false));
        assert!(
            !cache
                .entry_path(&cache.key(&path, TEXT, limits))
                .unwrap()
                .exists()
        );
    }

    #[test]
    fn independent_writers_publish_whole_products_within_disk_budget() {
        let directory = tempfile::tempdir().unwrap();
        let barrier = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();
        for index in 0..8 {
            let barrier = Arc::clone(&barrier);
            let directory = directory.path().to_path_buf();
            handles.push(std::thread::spawn(move || {
                let mut settings = ParseProductCacheSettings::new(directory);
                settings.max_entries = 3;
                settings.max_total_bytes = 16 * 1024;
                let cache = ParseProductCache::new(settings, OrganizationId(Uuid::from_u128(1)));
                let path = RepoPath::new(format!("reader_{index}.rs")).unwrap();
                barrier.wait();
                cache.parse(&path, TEXT, ParseLimits::default()).0
            }));
        }
        for handle in handles {
            assert!(!handle.join().unwrap().symbols.is_empty());
        }
        let cache = cache(directory.path(), 1);
        let mut entries = 0;
        let mut bytes = 0;
        for entry in fs::read_dir(&cache.namespace).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name() == LOCK_NAME {
                continue;
            }
            entries += 1;
            let content = fs::read(entry.path()).unwrap();
            bytes += content.len();
            let envelope: Envelope = serde_json::from_slice(&content).unwrap();
            assert!(valid_product(&envelope.product, &envelope.key, TEXT));
        }
        assert!(entries > 0 && entries <= 3);
        assert!(bytes <= 16 * 1024);
    }
}
