//! Corpora built the way Knowell indexes: through the real file walker.
//!
//! [`Fixture::to_corpus`] is the raw workspace. A measured run must instead
//! see exactly what the engine would index: sensitive files excluded by path
//! before they are read, and secret-looking spans redacted. This module walks
//! a written fixture with `knowell-source` and enforces the canary gate:
//! if any fixture canary survives into the corpus, the run fails.

use std::collections::BTreeMap;
use std::path::Path;

use knowell_secrets::ExclusionPolicy;
use knowell_source::{SkipReason, WalkOptions};

use crate::corpus::{Corpus, CorpusDoc};
use crate::error::EvalError;
use crate::fixture::Fixture;

/// A walked corpus plus what the secret boundary did to it.
#[derive(Debug, Clone)]
pub struct WalkedCorpus {
    /// The documents as Knowell would index them.
    pub corpus: Corpus,
    /// Files excluded by path, as `<project>/<path>` with the reason code.
    pub excluded: Vec<(String, String)>,
    /// Number of redacted spans per document id.
    pub redactions: BTreeMap<String, usize>,
    /// Files skipped for other reasons (binary, too large, …), by reason.
    pub other_skipped: BTreeMap<String, usize>,
}

/// Walks every project of `fixture` written below `root` (by
/// [`Fixture::write_to`]) with the built-in exclusion policy, then checks
/// that none of the fixture's canaries reached the corpus.
pub fn walk_fixture(root: &Path, fixture: &Fixture) -> Result<WalkedCorpus, EvalError> {
    let policy = ExclusionPolicy::builtin();
    let options = WalkOptions::default();
    let mut docs = Vec::new();
    let mut excluded = Vec::new();
    let mut redactions = BTreeMap::new();
    let mut other_skipped: BTreeMap<String, usize> = BTreeMap::new();

    for project in fixture.projects() {
        let name = project.name.as_str();
        let report = knowell_source::walk(&root.join(name), &policy, &options).map_err(|err| {
            EvalError::Walk {
                project: name.to_owned(),
                message: err.to_string(),
            }
        })?;
        for file in report.files {
            let id = format!("{name}/{}", file.path);
            if !file.redactions.is_empty() {
                redactions.insert(id.clone(), file.redactions.len());
            }
            docs.push(CorpusDoc {
                id,
                text: file.text,
            });
        }
        for skipped in report.skipped {
            let id = format!("{name}/{}", skipped.path);
            match skipped.reason {
                SkipReason::Excluded(exclusion) => {
                    let code = match exclusion {
                        knowell_secrets::Exclusion::Sensitive(kind) => kind.as_str().to_owned(),
                        knowell_secrets::Exclusion::Pattern(glob) => format!("pattern:{glob}"),
                        knowell_secrets::Exclusion::Internal => "internal".to_owned(),
                    };
                    excluded.push((id, code));
                }
                other => {
                    *other_skipped.entry(format!("{other:?}")).or_default() += 1;
                }
            }
        }
    }

    let corpus = Corpus::from_docs(docs)?.with_fixture(fixture.info());
    let canaries = fixture.canaries();
    for doc in corpus.docs() {
        if canaries
            .iter()
            .any(|canary| doc.text.contains(canary.as_str()))
        {
            return Err(EvalError::CanaryLeak {
                doc: doc.id.clone(),
            });
        }
    }
    Ok(WalkedCorpus {
        corpus,
        excluded,
        redactions,
        other_skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{FixtureSpec, Scale, WriteOptions, generate};

    #[test]
    fn walked_fixture_excludes_env_and_redacts_the_planted_key() {
        let fixture = generate(&FixtureSpec {
            seed: 42,
            scale: Scale::Small,
        });
        let dir = tempfile::tempdir().unwrap();
        fixture
            .write_to(dir.path(), &WriteOptions { git: false })
            .unwrap();
        let walked = walk_fixture(dir.path(), &fixture).unwrap();

        assert!(
            walked
                .excluded
                .iter()
                .any(|(id, code)| id == "infra/.env" && code == "env_file"),
            "{:?}",
            walked.excluded
        );
        assert!(!walked.corpus.contains("infra/.env"));
        assert!(!walked.redactions.is_empty(), "the planted key is redacted");
        for canary in fixture.canaries() {
            for doc in walked.corpus.docs() {
                assert!(!doc.text.contains(&canary), "{}", doc.id);
            }
        }
        // Everything else from the raw workspace is still there.
        assert_eq!(
            walked.corpus.len() + walked.excluded.len(),
            fixture.file_count()
        );
    }
}
