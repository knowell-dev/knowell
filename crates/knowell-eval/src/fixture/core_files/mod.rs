//! Hand-written core files: the semantics every query is judged against.
//!
//! Core files are identical for every seed and scale, except for the two
//! planted secret values, which are derived from the seed at generation time.

mod billing_api;
mod contracts;
mod db_migrations;
mod handbook;
mod infra;
mod ledger_service;
mod mobile_app;
mod notification_worker;
mod orders_service;
mod storefront_web;

use super::noise::NoiseStyle;

/// One project of the fixture workspace.
pub(crate) struct CoreProject {
    /// Project (and directory) name.
    pub(crate) name: &'static str,
    /// Primary language, as reported in the manifest.
    pub(crate) language: &'static str,
    /// Share of the noise budget, in percent of all noise files.
    pub(crate) noise_weight: usize,
    /// Kind of generated noise.
    pub(crate) noise_style: NoiseStyle,
    /// `(path, content)`; content starts with one newline that is stripped.
    pub(crate) files: &'static [(&'static str, &'static str)],
}

/// All projects, sorted by name. Noise weights sum to 100.
pub(crate) const PROJECTS: &[CoreProject] = &[
    CoreProject {
        name: "billing-api",
        language: "typescript",
        noise_weight: 15,
        noise_style: NoiseStyle::Nest,
        files: billing_api::FILES,
    },
    CoreProject {
        name: "contracts",
        language: "protobuf",
        noise_weight: 5,
        noise_style: NoiseStyle::Contracts,
        files: contracts::FILES,
    },
    CoreProject {
        name: "db-migrations",
        language: "sql",
        noise_weight: 6,
        noise_style: NoiseStyle::Sql,
        files: db_migrations::FILES,
    },
    CoreProject {
        name: "handbook",
        language: "markdown",
        noise_weight: 9,
        noise_style: NoiseStyle::Markdown,
        files: handbook::FILES,
    },
    CoreProject {
        name: "infra",
        language: "yaml",
        noise_weight: 4,
        noise_style: NoiseStyle::Kubernetes,
        files: infra::FILES,
    },
    CoreProject {
        name: "ledger-service",
        language: "python",
        noise_weight: 13,
        noise_style: NoiseStyle::Python,
        files: ledger_service::FILES,
    },
    CoreProject {
        name: "mobile-app",
        language: "dart",
        noise_weight: 12,
        noise_style: NoiseStyle::Dart,
        files: mobile_app::FILES,
    },
    CoreProject {
        name: "notification-worker",
        language: "rust",
        noise_weight: 7,
        noise_style: NoiseStyle::Rust,
        files: notification_worker::FILES,
    },
    CoreProject {
        name: "orders-service",
        language: "go",
        noise_weight: 13,
        noise_style: NoiseStyle::Go,
        files: orders_service::FILES,
    },
    CoreProject {
        name: "storefront-web",
        language: "typescript",
        noise_weight: 16,
        noise_style: NoiseStyle::React,
        files: storefront_web::FILES,
    },
];

/// Placeholder for the canary in `infra/.env`.
pub(crate) const CANARY_PLACEHOLDER: &str = "{{CANARY}}";
/// Placeholder for the fake access key id in the Go export job.
pub(crate) const AWS_KEY_PLACEHOLDER: &str = "{{AWS_KEY_ID}}";

/// Number of core files over all projects.
pub(crate) fn core_file_count() -> usize {
    PROJECTS.iter().map(|p| p.files.len()).sum()
}

/// Strips the single leading newline used for readable literals and
/// normalises line endings (defensive: literals are already `\n`).
pub(crate) fn normalize(content: &str) -> String {
    let trimmed = content.strip_prefix('\n').unwrap_or(content);
    trimmed.replace("\r\n", "\n")
}

/// Doc ids (`<project>/<path>`) of all core files, for query validation
/// without generating noise.
pub(crate) fn core_doc_ids() -> Vec<String> {
    let mut ids: Vec<String> = PROJECTS
        .iter()
        .flat_map(|p| {
            p.files
                .iter()
                .map(move |(path, _)| format!("{}/{path}", p.name))
        })
        .collect();
    ids.sort();
    ids
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use knowell_core::{Name, RepoPath};

    use super::*;

    #[test]
    fn projects_are_sorted_valid_and_weights_sum_to_100() {
        let names: Vec<&str> = PROJECTS.iter().map(|p| p.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
        assert_eq!(PROJECTS.iter().map(|p| p.noise_weight).sum::<usize>(), 100);
        for project in PROJECTS {
            assert!(Name::new(project.name).is_ok(), "{}", project.name);
            let mut seen = BTreeSet::new();
            for (path, content) in project.files {
                assert!(RepoPath::new(*path).is_ok(), "{}/{path}", project.name);
                assert!(seen.insert(*path), "duplicate {}/{path}", project.name);
                assert!(
                    content.starts_with('\n'),
                    "{}/{path} must start with a newline",
                    project.name
                );
                assert!(
                    content.ends_with('\n'),
                    "{}/{path} must end with a newline",
                    project.name
                );
                assert!(!content.contains('\r'), "{}/{path}", project.name);
                assert!(
                    !content.contains('\t') || path.ends_with(".go") || path.ends_with("go.mod"),
                    "{}/{path}: tabs only in Go files",
                    project.name
                );
            }
            assert!(
                seen.contains("README.md"),
                "{} needs a README",
                project.name
            );
        }
    }

    #[test]
    fn placeholders_appear_exactly_once_in_their_files() {
        let count = |needle: &str| -> Vec<String> {
            PROJECTS
                .iter()
                .flat_map(|p| {
                    p.files
                        .iter()
                        .filter(move |(_, c)| c.contains(needle))
                        .map(move |(path, _)| format!("{}/{path}", p.name))
                })
                .collect()
        };
        assert_eq!(count(CANARY_PLACEHOLDER), ["infra/.env"]);
        assert_eq!(
            count(AWS_KEY_PLACEHOLDER),
            ["orders-service/internal/export/s3_export.go"]
        );
    }

    #[test]
    fn normalize_strips_one_leading_newline() {
        assert_eq!(normalize("\n\na\r\nb\n"), "\na\nb\n");
        assert_eq!(normalize("x"), "x");
    }
}
