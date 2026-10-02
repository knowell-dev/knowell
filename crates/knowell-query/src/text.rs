//! Text helpers shared by the planner and the glossary.

/// Folds text for matching: Unicode lowercase, then Turkish and circumflex
/// letters mapped to their ASCII base (`ç→c ğ→g ı→i İ→i ö→o ş→s ü→u â→a
/// î→i û→u`), combining dot above removed. Users type Turkish with and without
/// diacritics; folding makes `ödeme`, `Ödeme` and `odeme` the same key.
pub(crate) fn fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            'ç' | 'Ç' => out.push('c'),
            'ğ' | 'Ğ' => out.push('g'),
            'ı' | 'I' | 'İ' | 'î' | 'Î' => out.push('i'),
            'ö' | 'Ö' => out.push('o'),
            'ş' | 'Ş' => out.push('s'),
            'ü' | 'Ü' | 'û' | 'Û' => out.push('u'),
            'â' | 'Â' => out.push('a'),
            '\u{0307}' => {}
            other => out.extend(other.to_lowercase().filter(|l| *l != '\u{0307}')),
        }
    }
    out
}

/// Folded English and Turkish function words that carry no search content.
/// Question words are listed too: they steer classification, not retrieval.
const STOPWORDS: &[&str] = &[
    // English
    "a", "about", "all", "also", "am", "an", "and", "any", "are", "as", "at", "be", "been", "being",
    "but", "by", "can", "could", "did", "do", "does", "doing", "done", "else", "for", "from",
    "had", "has", "have", "he", "her", "here", "his", "how", "i", "if", "in", "into", "is", "it",
    "its", "just", "may", "me", "might", "must", "my", "no", "not", "of", "on", "only", "or",
    "our", "please", "shall", "she", "should", "show", "so", "some", "such", "tell", "than",
    "that", "the", "their", "them", "then", "there", "these", "they", "this", "those", "to", "too",
    "us", "very", "was", "we", "were", "what", "when", "where", "which", "who", "whom", "whose",
    "why", "will", "with", "without", "would", "you", "your", // Turkish (folded)
    "acaba", "ama", "ancak", "bir", "bu", "bul", "da", "daha", "de", "dir", "en", "fakat", "gibi",
    "goster", "hangi", "hangisi", "icin", "ile", "ise", "ki", "kim", "kimler", "lutfen", "mi",
    "midir", "mu", "nasil", "ne", "neden", "nedir", "nerede", "nereden", "nereye", "nicin", "niye",
    "o", "olan", "olarak", "su", "ve", "var", "veya", "ya", "yok",
];

/// Whether a folded word is a stopword.
pub(crate) fn is_stopword(folded: &str) -> bool {
    STOPWORDS.contains(&folded)
}

/// File extensions that make a token a file reference rather than a
/// qualified name (`service.ts` vs `billing.service`). Compared lowercase.
const SOURCE_EXTENSIONS: &[&str] = &[
    "bash", "c", "cc", "cjs", "conf", "cpp", "cs", "css", "cxx", "dart", "ex", "exs", "go", "gql",
    "gradle", "graphql", "h", "hpp", "html", "ini", "java", "js", "json", "jsx", "kt", "kts",
    "less", "lua", "md", "mdx", "mjs", "php", "proto", "py", "rb", "rs", "sass", "scala", "scss",
    "sh", "sql", "svelte", "swift", "tf", "toml", "ts", "tsx", "txt", "vue", "xml", "yaml", "yml",
    "zig",
];

/// File names without an extension that are still unmistakably files.
const SPECIAL_FILES: &[&str] = &[
    "Containerfile",
    "Dockerfile",
    "Gemfile",
    "Jenkinsfile",
    "Makefile",
    "Procfile",
    "Rakefile",
];

/// Technology names that look like file names in prose (`Node.js`).
const TECH_NAMES: &[&str] = &[
    "angular.js",
    "chart.js",
    "d3.js",
    "express.js",
    "nest.js",
    "next.js",
    "node.js",
    "nuxt.js",
    "react.js",
    "three.js",
    "vue.js",
];

/// Whether `name` (a single path segment) has a known source/config extension.
pub(crate) fn has_source_extension(name: &str) -> bool {
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty() && SOURCE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
}

/// Whether `name` (a single segment) names a file: a known extension or a
/// well-known extensionless file, but not a technology name in prose.
pub(crate) fn is_file_name(name: &str) -> bool {
    if SPECIAL_FILES.contains(&name) {
        return true;
    }
    if TECH_NAMES.contains(&name.to_ascii_lowercase().as_str()) {
        return false;
    }
    let plausible = name
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '@' | '+'));
    plausible && has_source_extension(name)
}

/// Truncates `text` to at most `max` characters (for signal evidence).
pub(crate) fn clip(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_turkish() {
        assert_eq!(fold("Ödeme İptali"), "odeme iptali");
        assert_eq!(fold("ŞIRKET ığüşöç"), "sirket igusoc");
        assert_eq!(fold("Kullanıcı"), "kullanici");
        assert_eq!(fold("PaymentService"), "paymentservice");
    }

    #[test]
    fn stopwords_are_folded_forms() {
        assert!(is_stopword("nasil"));
        assert!(is_stopword("the"));
        assert!(!is_stopword("payment"));
        let mut sorted = STOPWORDS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), STOPWORDS.len(), "no duplicate stopwords");
    }

    #[test]
    fn file_names() {
        assert!(is_file_name("payment.service.ts"));
        assert!(is_file_name("Cargo.toml"));
        assert!(is_file_name("Dockerfile"));
        assert!(!is_file_name("Node.js"));
        assert!(!is_file_name("billing.cancel"));
        assert!(!is_file_name(".ts"));
        assert!(!is_file_name("e.g"));
    }
}
