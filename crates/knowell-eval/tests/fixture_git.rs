//! A git-backed fixture write is reproducible: same commit ids every time,
//! on every machine.

// Shared helpers outside #[test] functions may fail loudly too.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use knowell_eval::{FixtureManifest, FixtureSpec, Scale, WriteOptions, generate};

/// Whether `git` can be run. `KNOWELL_TEST_STRICT=1` (CI, the Docker test
/// runner) turns a missing `git` into a failure instead of a silent pass.
fn git_available() -> bool {
    let available = std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    assert!(
        available || !std::env::var("KNOWELL_TEST_STRICT").is_ok_and(|value| value == "1"),
        "git is not available, and KNOWELL_TEST_STRICT=1 forbids skipping"
    );
    available
}

fn commits(manifest: &FixtureManifest) -> Vec<(String, String)> {
    manifest
        .projects
        .iter()
        .map(|p| {
            (
                p.name.to_string(),
                p.commit.clone().expect("git write records commits"),
            )
        })
        .collect()
}

#[test]
fn git_write_yields_identical_commit_ids() {
    if !git_available() {
        return;
    }
    let fixture = generate(&FixtureSpec {
        seed: 42,
        scale: Scale::Small,
    });
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let a = fixture
        .write_to(first.path(), &WriteOptions { git: true })
        .unwrap();
    let b = fixture
        .write_to(second.path(), &WriteOptions { git: true })
        .unwrap();
    assert!(a.git);
    assert_eq!(commits(&a), commits(&b));

    // Pinned so that CI on other platforms proves machine independence.
    // Changes whenever the golden tree hash changes.
    let expected: &[(&str, &str)] = &[
        ("billing-api", "aa44d33644326abf2e39588c31de7bfdb7141c28"), // gitleaks:allow (test fixture, not a secret)
        ("contracts", "d540287b75a9fed8e052e877619d52555c4899b4"),
        ("db-migrations", "e1a694656a5ba4ce0edfa07008fb7eb6454f4418"),
        ("handbook", "9059247495b65624eed0ebd791d0d15ddb215678"),
        ("infra", "33c16e618cbdf8b0d7e3e64de1b8561a211b155e"),
        ("ledger-service", "2e3450b0226d3e8e279ca27007a314147477202a"),
        ("mobile-app", "83f9d5f63bffe05c512debf97d39c9e438ac1bea"),
        (
            "notification-worker",
            "43d3209e297b134bd297b013a1c8a2bd4d587cd4",
        ),
        ("orders-service", "c2275b4401326a3d8bdcd946fa120a48157dcbb0"),
        ("storefront-web", "68665c7b9675d847de4c7083ebac349c11ad775e"),
    ];
    let actual = commits(&a);
    let actual: Vec<(&str, &str)> = actual
        .iter()
        .map(|(n, c)| (n.as_str(), c.as_str()))
        .collect();
    assert_eq!(actual, expected);

    // The committed `.env` must be tracked despite any ignore rules.
    let listed = std::process::Command::new("git")
        .args(["ls-files", "--", ".env"])
        .current_dir(first.path().join("infra"))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&listed.stdout).trim(), ".env");
}
