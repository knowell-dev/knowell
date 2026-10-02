//! SARIF 2.1.0 output for CI code-scanning integrations.

use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

use crate::check::{CHECK_RULES, Finding, Location, Severity};
use crate::structured::hash_hex;

/// Key of the partial fingerprint (versioned so a scheme change is visible).
pub const FINGERPRINT_KEY: &str = "knowellFinding/v1";

fn level(severity: Severity) -> &'static str {
    match severity {
        Severity::Info => "note",
        Severity::Warning => "warning",
        Severity::Error => "error",
    }
}

/// Stable identity of a finding for de-duplication across runs: code,
/// project, subject, primary file and message. Line numbers are left out so
/// unrelated edits above a finding do not make it a "new" one.
pub fn fingerprint(finding: &Finding) -> String {
    let project = finding.project.as_ref().map_or("", |p| p.as_str());
    let subject = finding.subject.as_ref().map_or("", |s| s.as_str());
    let path = finding
        .locations
        .first()
        .map(|l| format!("{}/{}", l.project, l.path))
        .unwrap_or_default();
    hash_hex(
        "knowell-finding/v1",
        &[&finding.code, project, subject, &path, &finding.message],
    )
}

/// A SARIF `location` with a physical location relative to the project
/// root (`uriBaseId` = project name).
fn location(location: &Location, id: Option<usize>) -> Value {
    let mut physical = Map::new();
    physical.insert(
        "artifactLocation".to_owned(),
        json!({
            "uri": location.path.as_str(),
            "uriBaseId": location.project.as_str(),
        }),
    );
    if let Some(range) = location.range {
        physical.insert(
            "region".to_owned(),
            json!({ "startLine": range.start(), "endLine": range.end() }),
        );
    }
    let mut out = Map::new();
    if let Some(id) = id {
        out.insert("id".to_owned(), json!(id));
    }
    out.insert("physicalLocation".to_owned(), Value::Object(physical));
    Value::Object(out)
}

fn result(finding: &Finding) -> Value {
    let mut out = Map::new();
    out.insert("ruleId".to_owned(), json!(finding.code));
    if let Some(index) = CHECK_RULES.iter().position(|r| r.code == finding.code) {
        out.insert("ruleIndex".to_owned(), json!(index));
    }
    out.insert("level".to_owned(), json!(level(finding.severity)));
    out.insert("message".to_owned(), json!({ "text": finding.message }));
    if let Some(first) = finding.locations.first() {
        out.insert("locations".to_owned(), json!([location(first, None)]));
    }
    let related: Vec<Value> = finding
        .locations
        .iter()
        .skip(1)
        .enumerate()
        .map(|(i, l)| location(l, Some(i + 1)))
        .collect();
    if !related.is_empty() {
        out.insert("relatedLocations".to_owned(), Value::Array(related));
    }
    out.insert(
        "partialFingerprints".to_owned(),
        json!({ FINGERPRINT_KEY: fingerprint(finding) }),
    );
    let mut properties = Map::new();
    if let Some(project) = &finding.project {
        properties.insert("project".to_owned(), json!(project.as_str()));
    }
    if let Some(subject) = &finding.subject {
        properties.insert("subject".to_owned(), json!(subject.as_str()));
    }
    if !properties.is_empty() {
        out.insert("properties".to_owned(), Value::Object(properties));
    }
    Value::Object(out)
}

/// Renders findings as a SARIF 2.1.0 log with one run. Every known check
/// code is listed as a rule (fixed order); results reference rules by id and
/// index, carry physical locations relative to their project root
/// (`uriBaseId` = project name, described in `originalUriBaseIds`), related
/// locations and a partial fingerprint for stable de-duplication. The output
/// is deterministic for the same findings.
pub fn to_sarif(findings: &[Finding], tool_version: &str) -> Value {
    let rules: Vec<Value> = CHECK_RULES
        .iter()
        .map(|rule| {
            json!({
                "id": rule.code,
                "name": rule.code,
                "shortDescription": { "text": rule.summary },
                "fullDescription": { "text": rule.summary },
                "help": { "text": rule.help },
                "defaultConfiguration": { "level": level(rule.severity) },
            })
        })
        .collect();
    let projects: BTreeSet<String> = findings
        .iter()
        .flat_map(|f| f.locations.iter().map(|l| l.project.to_string()))
        .collect();
    let bases: Map<String, Value> = projects
        .into_iter()
        .map(|p| {
            let description =
                json!({ "description": { "text": format!("Root of project `{p}`") } });
            (p, description)
        })
        .collect();
    let results: Vec<Value> = findings.iter().map(result).collect();
    json!({
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "knowell",
                    "informationUri": "https://github.com/knowell-dev/knowell",
                    "version": tool_version,
                    "rules": rules,
                }
            },
            "originalUriBaseIds": bases,
            "columnKind": "unicodeCodePoints",
            "results": results,
        }]
    })
}
