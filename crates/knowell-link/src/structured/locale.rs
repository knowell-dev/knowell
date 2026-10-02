//! Translation catalogs: nested locale JSON files (i18next, vue-i18n,
//! custom loaders) and Flutter ARB files. Keys are recorded with the file's
//! locale; translated texts are not kept.

use std::collections::BTreeMap;

use knowell_graph::{ContractKind, EvidenceType};
use knowell_parse::Language;
use knowell_parse::tree_sitter::Node;

use super::tree::{self, entries, get, scalar};
use super::{Ctx, data_language};
use crate::model::{Extraction, Role};

const ATTR_LOCALE: &str = knowell_graph::ATTR_LOCALE;
/// Attribute: namespace of a `locales/<locale>/<namespace>.json` file.
const ATTR_NAMESPACE: &str = "namespace";

const LOCALE_DIRS: &[&str] = &[
    "locales",
    "locale",
    "i18n",
    "lang",
    "langs",
    "languages",
    "translations",
    "messages",
    "l10n",
    "intl",
];

/// `en`, `tr`, `en-US`, `pt_BR`, `zh-Hans`.
pub(crate) fn is_locale_code(text: &str) -> bool {
    let mut parts = text.splitn(2, ['-', '_']);
    let language = parts.next().unwrap_or("");
    let language_ok =
        (2..=3).contains(&language.len()) && language.chars().all(|c| c.is_ascii_lowercase());
    // Regions are upper-case (`US`) or numeric (`419`); scripts are title
    // case (`Hans`). `app_en` is therefore not a locale.
    let region_ok = parts.next().is_none_or(|region| {
        let upper = region.len() == 2 && region.chars().all(|c| c.is_ascii_uppercase());
        let numeric = region.len() == 3 && region.chars().all(|c| c.is_ascii_digit());
        let mut chars = region.chars();
        let script = region.len() == 4
            && chars.next().is_some_and(|c| c.is_ascii_uppercase())
            && chars.all(|c| c.is_ascii_lowercase());
        upper || numeric || script
    });
    language_ok && region_ok
}

fn flatten<'t>(
    node: Node<'t>,
    prefix: &str,
    text: &str,
    depth: usize,
    out: &mut Vec<(String, Node<'t>)>,
) {
    if depth > 16 {
        return;
    }
    for entry in entries(node) {
        let key = entry.key_text(text);
        if key.is_empty() || key.starts_with('@') || key.starts_with('$') {
            continue;
        }
        let full = if prefix.is_empty() {
            key
        } else {
            format!("{prefix}.{key}")
        };
        match entry.value {
            Some(value) if tree::is_map(value) => flatten(value, &full, text, depth + 1, out),
            _ => out.push((full, entry.key)),
        }
    }
}

fn emit(
    ctx: &Ctx<'_>,
    keys: Vec<(String, Node<'_>)>,
    locale: &str,
    namespace: Option<&str>,
) -> Vec<Extraction> {
    let mut out = Vec::new();
    for (key, node) in keys {
        let Some(range) = tree::line(node) else {
            continue;
        };
        let mut attrs = BTreeMap::new();
        attrs.insert(ATTR_LOCALE.to_owned(), locale.to_owned());
        if let Some(namespace) = namespace {
            attrs.insert(ATTR_NAMESPACE.to_owned(), namespace.to_owned());
        }
        out.extend(ctx.extraction(
            ContractKind::I18nKey,
            Role::Definition,
            &key,
            range,
            None,
            EvidenceType::Syntactic,
            attrs,
        ));
    }
    out
}

/// Keys of a locale JSON file: under a locale directory (`locales/`,
/// `i18n/`, ...) and named after a locale (`en.json`) or inside a locale
/// directory (`locales/en/common.json`, recorded with namespace `common`).
pub(crate) fn locale_json(ctx: &Ctx<'_>, text: &str) -> Vec<Extraction> {
    if ctx.path.extension() != Some("json") {
        return Vec::new();
    }
    let components: Vec<&str> = ctx.path.components().collect();
    let dirs = components
        .get(..components.len().saturating_sub(1))
        .unwrap_or(&[]);
    if !dirs
        .iter()
        .any(|d| LOCALE_DIRS.contains(&d.to_ascii_lowercase().as_str()))
    {
        return Vec::new();
    }
    let file_name = ctx.path.file_name();
    let stem = file_name.strip_suffix(".json").unwrap_or(file_name);
    let (locale, namespace) = if is_locale_code(stem) {
        (stem.to_owned(), None)
    } else {
        match dirs.last() {
            Some(parent) if is_locale_code(parent) => ((*parent).to_owned(), Some(stem)),
            _ => return Vec::new(),
        }
    };
    let Some(tree) = ctx.tree(Language::Json, text) else {
        return Vec::new();
    };
    let mut keys = Vec::new();
    for root in tree::roots(tree.root_node()) {
        if tree::is_map(root) {
            flatten(root, "", text, 0, &mut keys);
        }
    }
    emit(ctx, keys, &locale, namespace)
}

fn arb_locale(stem: &str) -> Option<String> {
    let parts: Vec<&str> = stem.split('_').collect();
    (1..parts.len())
        .filter_map(|i| parts.get(i..).map(|tail| tail.join("_")))
        .find(|candidate| is_locale_code(candidate))
}

/// Message keys of a Flutter ARB file (metadata `@` keys skipped); the
/// locale comes from `@@locale` or the file name (`app_en.arb`).
pub(crate) fn arb(ctx: &Ctx<'_>, text: &str) -> Vec<Extraction> {
    if ctx.path.extension() != Some("arb") || data_language(ctx.path) != Some(Language::Json) {
        return Vec::new();
    }
    let Some(tree) = ctx.tree(Language::Json, text) else {
        return Vec::new();
    };
    let stem = ctx
        .path
        .file_name()
        .strip_suffix(".arb")
        .unwrap_or(ctx.path.file_name());
    let mut out = Vec::new();
    for root in tree::roots(tree.root_node()) {
        if !tree::is_map(root) {
            continue;
        }
        let locale = get(root, "@@locale", text)
            .and_then(|n| scalar(n, text))
            .filter(|l| is_locale_code(l))
            .or_else(|| arb_locale(stem));
        let Some(locale) = locale else {
            continue;
        };
        let keys: Vec<(String, Node<'_>)> = entries(root)
            .into_iter()
            .map(|e| (e.key_text(text), e.key))
            .filter(|(k, _)| !k.is_empty() && !k.starts_with('@'))
            .collect();
        out.extend(emit(ctx, keys, &locale, None));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_codes() {
        for ok in ["en", "tr", "en-US", "pt_BR", "zh-Hans", "fil"] {
            assert!(is_locale_code(ok), "{ok}");
        }
        for bad in ["common", "EN", "e", "en-", "app_en", "messages"] {
            assert!(!is_locale_code(bad), "{bad}");
        }
        assert_eq!(arb_locale("app_en").as_deref(), Some("en"));
        assert_eq!(arb_locale("intl_pt_BR").as_deref(), Some("pt_BR"));
        assert_eq!(arb_locale("strings"), None);
    }
}
