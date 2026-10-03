//! SARIF 2.1.0 output for CI code-scanning integrations.

use std::collections::{BTreeMap, BTreeSet};

use knowell_core::Name;
use serde_json::{Map, Value, json};
use url::Url;

use crate::LinkError;
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

/// A SARIF `location` with an absolute, URI-encoded source-file location.
fn location(
    location: &Location,
    id: Option<usize>,
    roots: &BTreeMap<Name, Url>,
) -> Result<Value, LinkError> {
    let root = roots
        .get(&location.project)
        .ok_or_else(|| LinkError::MissingSarifRoot(location.project.clone()))?;
    let mut absolute = root.clone();
    absolute
        .path_segments_mut()
        .map_err(|_| LinkError::InvalidSarifRoot(location.project.clone()))?
        .pop_if_empty()
        .extend(location.path.components());
    let mut physical = Map::new();
    // GitHub ignores project URI bases when mapping a relative artifact URI.
    // An absolute URI lets the uploader map it against the repository checkout.
    physical.insert(
        "artifactLocation".to_owned(),
        json!({
            "uri": absolute.as_str(),
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
    Ok(Value::Object(out))
}

fn result(finding: &Finding, roots: &BTreeMap<Name, Url>) -> Result<Value, LinkError> {
    let mut out = Map::new();
    out.insert("ruleId".to_owned(), json!(finding.code));
    if let Some(index) = CHECK_RULES.iter().position(|r| r.code == finding.code) {
        out.insert("ruleIndex".to_owned(), json!(index));
    }
    out.insert("level".to_owned(), json!(level(finding.severity)));
    out.insert("message".to_owned(), json!({ "text": finding.message }));
    if let Some(first) = finding.locations.first() {
        out.insert(
            "locations".to_owned(),
            json!([location(first, None, roots)?]),
        );
    }
    let related: Vec<Value> = finding
        .locations
        .iter()
        .skip(1)
        .enumerate()
        .map(|(i, l)| location(l, Some(i + 1), roots))
        .collect::<Result<_, _>>()?;
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
    Ok(Value::Object(out))
}

/// Renders findings as a SARIF 2.1.0 log with one run. Every known check
/// code is listed as a rule (fixed order); results reference rules by id and
/// index, carry absolute `file:` URIs for primary and related locations and a
/// partial fingerprint for stable de-duplication. Paths are URI-encoded,
/// including reserved characters and non-ASCII bytes. Artifact locations do not
/// depend on `uriBaseId`; uploaders such as GitHub can resolve them against the
/// scanned repository's checkout root. `originalUriBaseIds` records the project
/// directories as metadata for other SARIF consumers.
///
/// `roots` maps each project to its absolute source-directory `file:` URI,
/// including a trailing slash. For a monorepo project this is the project
/// subdirectory, not the repository root. Callers should resolve symlinks
/// before constructing the URI. No file-system access is performed here.
/// Output is deterministic for the same findings, version and roots.
///
/// # Errors
/// [`LinkError::MissingSarifRoot`] if a finding names an unmapped project;
/// [`LinkError::InvalidSarifRoot`] if its root is not a directory file URI
/// without credentials, a query or a fragment. Rejected URIs are never echoed.
pub fn to_sarif(
    findings: &[Finding],
    tool_version: &str,
    roots: &BTreeMap<Name, Url>,
) -> Result<Value, LinkError> {
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
    let projects: BTreeSet<&Name> = findings
        .iter()
        .flat_map(|f| f.locations.iter().map(|l| &l.project))
        .collect();
    let bases: Map<String, Value> = projects
        .into_iter()
        .map(|p| {
            let root = roots
                .get(p)
                .ok_or_else(|| LinkError::MissingSarifRoot(p.clone()))?;
            if root.scheme() != "file"
                || !root.path().ends_with('/')
                || !root.username().is_empty()
                || root.password().is_some()
                || root.query().is_some()
                || root.fragment().is_some()
            {
                return Err(LinkError::InvalidSarifRoot(p.clone()));
            }
            Ok((
                p.to_string(),
                json!({
                    "uri": root.as_str(),
                    "description": { "text": format!("Root of project `{p}`") },
                }),
            ))
        })
        .collect::<Result<_, _>>()?;
    let results: Vec<Value> = findings
        .iter()
        .map(|finding| result(finding, roots))
        .collect::<Result<_, _>>()?;
    Ok(json!({
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
    }))
}

#[cfg(test)]
mod tests {
    use knowell_core::{LineRange, RepoPath};

    use super::*;

    fn finding() -> Finding {
        Finding {
            code: "link.endpoint_without_provider".to_owned(),
            severity: Severity::Info,
            message: "an endpoint has no provider".to_owned(),
            subject: None,
            project: Some(Name::new("web").unwrap()),
            locations: vec![Location {
                project: Name::new("web").unwrap(),
                path: RepoPath::new("src/space #100%/café.ts").unwrap(),
                range: Some(LineRange::new(7, 9).unwrap()),
            }],
            evidence: Vec::new(),
        }
    }

    #[test]
    fn encoded_locations_resolve_with_unix_and_windows_roots() {
        let finding = finding();
        for (base, checkout) in [
            ("file:///checkout/apps/web/", "file:///checkout/"),
            ("file:///C:/checkout/apps/web/", "file:///C:/checkout/"),
            (
                "file:///checkout%20%23100%25/apps/web/",
                "file:///checkout%20%23100%25/",
            ),
        ] {
            let root = Url::parse(base).unwrap();
            let roots = BTreeMap::from([(Name::new("web").unwrap(), root)]);
            let report = to_sarif(std::slice::from_ref(&finding), "test", &roots).unwrap();
            let run = &report["runs"][0];
            assert_eq!(run["originalUriBaseIds"]["web"]["uri"], base);
            let physical = &run["results"][0]["locations"][0]["physicalLocation"];
            let artifact = &physical["artifactLocation"];
            assert!(artifact.get("uriBaseId").is_none());
            let uri = artifact["uri"].as_str().unwrap();
            assert_eq!(uri, format!("{base}src/space%20%23100%25/caf%C3%A9.ts"));
            let absolute = Url::parse(uri).unwrap();
            assert_eq!(absolute.scheme(), "file");
            assert_eq!(absolute.query(), None);
            assert_eq!(absolute.fragment(), None);
            assert_eq!(
                Url::parse(checkout).unwrap().make_relative(&absolute),
                Some("apps/web/src/space%20%23100%25/caf%C3%A9.ts".to_owned())
            );
            assert_eq!(physical["region"]["startLine"], 7);
            assert_eq!(physical["region"]["endLine"], 9);
        }
    }

    #[test]
    fn related_locations_use_their_own_roots() {
        let mut finding = finding();
        finding.locations.push(Location {
            project: Name::new("api").unwrap(),
            path: RepoPath::new("openapi.yaml").unwrap(),
            range: None,
        });
        finding.locations.push(Location {
            project: Name::new("api").unwrap(),
            path: RepoPath::new("schema #100%/café.yaml").unwrap(),
            range: Some(LineRange::new(2, 4).unwrap()),
        });
        let roots = BTreeMap::from([
            (
                Name::new("web").unwrap(),
                Url::parse("file:///checkout/web/").unwrap(),
            ),
            (
                Name::new("api").unwrap(),
                Url::parse("file:///another/api/").unwrap(),
            ),
        ]);
        let report = to_sarif(&[finding], "test", &roots).unwrap();
        let run = &report["runs"][0];
        let related = &run["results"][0]["relatedLocations"][0];
        assert_eq!(related["id"], 1);
        let artifact = &related["physicalLocation"]["artifactLocation"];
        assert_eq!(artifact["uri"], "file:///another/api/openapi.yaml");
        assert!(artifact.get("uriBaseId").is_none());
        assert_eq!(
            run["originalUriBaseIds"]["api"]["uri"],
            "file:///another/api/"
        );
        let encoded = &run["results"][0]["relatedLocations"][1];
        assert_eq!(encoded["id"], 2);
        let physical = &encoded["physicalLocation"];
        assert_eq!(
            physical["artifactLocation"]["uri"],
            "file:///another/api/schema%20%23100%25/caf%C3%A9.yaml"
        );
        assert!(physical["artifactLocation"].get("uriBaseId").is_none());
        assert_eq!(physical["region"]["startLine"], 2);
        assert_eq!(physical["region"]["endLine"], 4);
    }

    #[test]
    fn matching_filenames_in_project_subroots_have_distinct_checkout_paths() {
        let mut web = finding();
        web.locations[0].path = RepoPath::new("src/client.ts").unwrap();
        let mut api = web.clone();
        let api_name = Name::new("api").unwrap();
        api.project = Some(api_name.clone());
        api.locations[0].project = api_name.clone();
        let roots = BTreeMap::from([
            (
                Name::new("web").unwrap(),
                Url::parse("file:///checkout/apps/web/").unwrap(),
            ),
            (
                api_name,
                Url::parse("file:///checkout/services/api/").unwrap(),
            ),
        ]);
        let report = to_sarif(&[web, api], "test", &roots).unwrap();
        let results = report["runs"][0]["results"].as_array().unwrap();
        let checkout = Url::parse("file:///checkout/").unwrap();
        let paths: Vec<_> = results
            .iter()
            .map(|result| {
                let artifact = &result["locations"][0]["physicalLocation"]["artifactLocation"];
                assert!(artifact.get("uriBaseId").is_none());
                let absolute = Url::parse(artifact["uri"].as_str().unwrap()).unwrap();
                checkout.make_relative(&absolute).unwrap()
            })
            .collect();
        assert_eq!(
            paths,
            ["apps/web/src/client.ts", "services/api/src/client.ts"]
        );
    }

    #[test]
    fn fingerprints_are_stable_when_the_checkout_directory_changes() {
        let finding = finding();
        let reports: Vec<_> = [
            "file:///checkout/apps/web/",
            "file:///another/checkout/web/",
        ]
        .into_iter()
        .map(|base| {
            let roots = BTreeMap::from([(Name::new("web").unwrap(), Url::parse(base).unwrap())]);
            to_sarif(std::slice::from_ref(&finding), "test", &roots).unwrap()
        })
        .collect();
        let first = &reports[0]["runs"][0]["results"][0];
        let second = &reports[1]["runs"][0]["results"][0];
        assert_eq!(first["partialFingerprints"], second["partialFingerprints"]);
        assert_eq!(
            first["partialFingerprints"][FINGERPRINT_KEY],
            fingerprint(&finding)
        );
        assert_ne!(first["locations"], second["locations"]);
    }

    #[test]
    fn missing_or_invalid_roots_fail_without_echoing_them() {
        let findings = [finding()];
        assert_eq!(
            to_sarif(&findings, "test", &BTreeMap::new()),
            Err(LinkError::MissingSarifRoot(Name::new("web").unwrap()))
        );
        for uri in [
            "file:///checkout",
            "file:///checkout/?KNOWELL_CANARY_QUERY",
            "file:///checkout/#KNOWELL_CANARY_FRAGMENT",
            "https://user:KNOWELL_CANARY_PASSWORD@example.com/",
            "mailto:KNOWELL_CANARY_MAIL",
        ] {
            let roots = BTreeMap::from([(Name::new("web").unwrap(), Url::parse(uri).unwrap())]);
            let error = to_sarif(&findings, "test", &roots).unwrap_err();
            assert_eq!(
                error,
                LinkError::InvalidSarifRoot(Name::new("web").unwrap())
            );
            assert!(!error.to_string().contains("KNOWELL_CANARY"));
        }
    }

    #[test]
    fn related_locations_require_known_valid_roots() {
        let mut finding = finding();
        finding.locations.push(Location {
            project: Name::new("api").unwrap(),
            path: RepoPath::new("openapi.yaml").unwrap(),
            range: None,
        });
        let mut roots = BTreeMap::from([(
            Name::new("web").unwrap(),
            Url::parse("file:///checkout/web/").unwrap(),
        )]);
        assert_eq!(
            to_sarif(std::slice::from_ref(&finding), "test", &roots),
            Err(LinkError::MissingSarifRoot(Name::new("api").unwrap()))
        );
        roots.insert(
            Name::new("api").unwrap(),
            Url::parse("file:///another/api/?KNOWELL_CANARY_QUERY").unwrap(),
        );
        let error = to_sarif(&[finding], "test", &roots).unwrap_err();
        assert_eq!(
            error,
            LinkError::InvalidSarifRoot(Name::new("api").unwrap())
        );
        assert!(!error.to_string().contains("KNOWELL_CANARY"));
    }

    #[test]
    fn empty_results_need_no_roots() {
        let report = to_sarif(&[], "test", &BTreeMap::new()).unwrap();
        assert_eq!(report["runs"][0]["results"], json!([]));
        assert_eq!(report["runs"][0]["originalUriBaseIds"], json!({}));
    }
}
