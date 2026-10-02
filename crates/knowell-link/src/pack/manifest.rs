//! The `pack.toml` schema (serde). See the crate README for the authoring
//! guide; unknown keys are rejected so typos surface at load time.

use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PackManifest {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) description: String,
    #[serde(default)]
    pub(crate) languages: Vec<String>,
    #[serde(default)]
    pub(crate) limits: String,
    #[serde(default)]
    pub(crate) detect: Option<DetectSpec>,
    #[serde(default)]
    pub(crate) rules: Vec<RuleSpec>,
    #[serde(default)]
    pub(crate) bindings: Vec<BindingSpec>,
    #[serde(default)]
    pub(crate) extractors: Vec<ExtractorSpec>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DetectSpec {
    #[serde(default)]
    pub(crate) always: bool,
    #[serde(default)]
    pub(crate) npm: Vec<String>,
    #[serde(default)]
    pub(crate) go: Vec<String>,
    #[serde(default)]
    pub(crate) python: Vec<String>,
    #[serde(default, rename = "pub")]
    pub(crate) pub_: Vec<String>,
    #[serde(default)]
    pub(crate) cargo: Vec<String>,
    #[serde(default)]
    pub(crate) maven: Vec<String>,
    #[serde(default)]
    pub(crate) nuget: Vec<String>,
    #[serde(default)]
    pub(crate) imports: Vec<String>,
    #[serde(default)]
    pub(crate) files: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LookupSpec {
    pub(crate) binding: String,
    pub(crate) by: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuleSpec {
    pub(crate) id: String,
    pub(crate) query: String,
    pub(crate) languages: Vec<String>,
    pub(crate) kind: String,
    pub(crate) role: String,
    pub(crate) key: String,
    #[serde(default)]
    pub(crate) symbol: Option<String>,
    #[serde(default)]
    pub(crate) anchor: Option<String>,
    #[serde(default)]
    pub(crate) evidence: Option<String>,
    #[serde(default)]
    pub(crate) normalize: Option<String>,
    #[serde(default)]
    pub(crate) resolve: Vec<String>,
    #[serde(default)]
    pub(crate) attrs: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) lookup: BTreeMap<String, LookupSpec>,
    #[serde(default)]
    pub(crate) defaults: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) tokens: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) require: Vec<String>,
    #[serde(default, rename = "where")]
    pub(crate) where_: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) unless: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) files: Vec<String>,
    #[serde(default)]
    pub(crate) exclude: Vec<String>,
    #[serde(default)]
    pub(crate) route: Option<String>,
    #[serde(default)]
    pub(crate) glob: Option<String>,
    #[serde(default)]
    pub(crate) key_shape: Option<String>,
    #[serde(default)]
    pub(crate) detect: Option<DetectSpec>,
    #[serde(default)]
    pub(crate) requires_rule: Option<String>,
    #[serde(default)]
    pub(crate) postprocess: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BindingSpec {
    pub(crate) id: String,
    pub(crate) query: String,
    pub(crate) languages: Vec<String>,
    pub(crate) scope: String,
    pub(crate) name: String,
    pub(crate) value: String,
    #[serde(default)]
    pub(crate) resolve: Vec<String>,
    #[serde(default)]
    pub(crate) constant: bool,
    #[serde(default)]
    pub(crate) files: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExtractorSpec {
    pub(crate) id: String,
    pub(crate) extractor: String,
    #[serde(default)]
    pub(crate) files: Vec<String>,
    #[serde(default)]
    pub(crate) exclude: Vec<String>,
}
