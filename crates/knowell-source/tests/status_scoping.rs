//! Scoped status regressions against synthetic, temporary repositories.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::path::Path;

use knowell_core::RepoPath;
use knowell_secrets::ExclusionPolicy;
use knowell_source::WalkOptions;
use knowell_source::git::{Change, GitError};
use support::{Sandbox, open};

fn path(value: &str) -> RepoPath {
    RepoPath::new(value).unwrap()
}

fn supported_repo(sandbox: &Sandbox, name: &str) -> std::path::PathBuf {
    // Retain Git's normal comment-only info/exclude and the empty synthetic
    // global excludes setting, just like the original integration tests.
    sandbox.init(name)
}

fn incomplete(error: GitError, expected: &str) {
    assert!(
        matches!(&error, GitError::Git { operation: "scoped status" | "scoped tracked status", message } if message.contains(expected)),
        "{error}"
    );
    assert!(!error.to_string().contains("KNOWELL_CANARY"));
}

#[test]
fn same_size_excluded_and_sibling_edits_do_not_affect_selected_status() {
    let sandbox = Sandbox::new();
    let repository = supported_repo(&sandbox, "scope");
    for name in [
        "app/change.rs",
        "app/same.rs",
        "app/.env",
        "app/private.rs",
        "sibling/file.rs",
    ] {
        sandbox.write(&repository, name, b"let x = 1;\n");
    }
    sandbox.commit_all(&repository, "synthetic base");
    for name in [
        "app/change.rs",
        "app/.env",
        "app/private.rs",
        "sibling/file.rs",
    ] {
        sandbox.write(&repository, name, b"let x = 2;\n");
    }
    let before = std::fs::read(repository.join(".git/index")).unwrap();
    let policy = ExclusionPolicy::with_patterns(&["private.rs".to_owned()]).unwrap();
    let changes = open(&repository)
        .working_changes_scoped(Some(&path("app")), &policy, &WalkOptions::default())
        .unwrap();
    assert_eq!(changes, [Change::Modified(path("app/change.rs"))]);
    assert_eq!(
        std::fs::read(repository.join(".git/index")).unwrap(),
        before
    );
}

#[test]
fn literal_metacharacter_paths_and_empty_selection_are_safe() {
    let sandbox = Sandbox::new();
    let repository = supported_repo(&sandbox, "literal");
    for name in ["app/[pick].rs", "app/p.rs", "app/.env"] {
        sandbox.write(&repository, name, b"let x = 1;\n");
    }
    sandbox.commit_all(&repository, "synthetic literal base");
    for name in ["app/[pick].rs", "app/p.rs", "app/.env"] {
        sandbox.write(&repository, name, b"let x = 2;\n");
    }
    let policy = ExclusionPolicy::with_patterns(&["p.rs".to_owned()]).unwrap();
    assert_eq!(
        open(&repository)
            .working_changes_scoped(Some(&path("app")), &policy, &WalkOptions::default())
            .unwrap(),
        [Change::Modified(path("app/[pick].rs"))]
    );
    let deny = ExclusionPolicy::with_patterns(&["**".to_owned()]).unwrap();
    assert!(
        open(&repository)
            .working_changes_scoped(Some(&path("app")), &deny, &WalkOptions::default())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn selected_size_and_mode_omissions_are_explicit_errors() {
    let sandbox = Sandbox::new();
    let repository = supported_repo(&sandbox, "limits");
    sandbox.write(&repository, "app/a.rs", b"short\n");
    sandbox.commit_all(&repository, "synthetic limited base");
    let options = WalkOptions {
        max_file_bytes: 4,
        ..Default::default()
    };
    incomplete(
        open(&repository)
            .working_changes_scoped(Some(&path("app")), &ExclusionPolicy::builtin(), &options)
            .unwrap_err(),
        "max_file_bytes",
    );
    std::fs::remove_file(repository.join("app/a.rs")).unwrap();
    std::fs::create_dir(repository.join("app/a.rs")).unwrap();
    incomplete(
        open(&repository)
            .working_changes_scoped(
                Some(&path("app")),
                &ExclusionPolicy::builtin(),
                &WalkOptions::default(),
            )
            .unwrap_err(),
        "regular file",
    );
}

fn assert_rejected(repository: &Path, root: Option<&RepoPath>, needle: &str) {
    incomplete(
        open(repository)
            .working_changes_scoped(root, &ExclusionPolicy::builtin(), &WalkOptions::default())
            .unwrap_err(),
        needle,
    );
}

#[test]
fn external_and_unsupported_git_controls_refuse_without_echoing_input() {
    let sandbox = Sandbox::new();
    let repository = supported_repo(&sandbox, "controls");
    sandbox.write(&repository, "app/file.txt", b"plain\n");
    sandbox.commit_all(&repository, "synthetic controls base");
    let root = path("app");
    let external = sandbox.path().join("external-control");
    std::fs::write(&external, b"KNOWELL_CANARY_external_rule\n").unwrap();
    for key in ["core.excludesFile", "core.attributesFile"] {
        sandbox.git(&repository, &["config", key, external.to_str().unwrap()]);
        assert_rejected(&repository, Some(&root), "external Git controls");
        sandbox.git(&repository, &["config", "--unset", key]);
    }
    // A custom path is still outside the approved control set even inside
    // the repository; sensitivity and ordinary content scope stay separate.
    sandbox.write(&repository, ".env", b"KNOWELL_CANARY_private_control\n");
    sandbox.git(
        &repository,
        &[
            "config",
            "core.attributesFile",
            repository.join(".env").to_str().unwrap(),
        ],
    );
    assert_rejected(&repository, Some(&root), "external Git controls");
    sandbox.git(&repository, &["config", "--unset", "core.attributesFile"]);
    for rule in [
        b"file.txt filter=KNOWELL_CANARY_fake_driver\n".as_slice(),
        b"file.txt working-tree-encoding=KNOWELL_CANARY_encoding\n",
        b"file.txt ident\n",
    ] {
        sandbox.write(&repository, "app/.gitattributes", rule);
        assert_rejected(&repository, Some(&root), "filter, encoding or ident");
    }
    sandbox.write(
        &repository,
        "app/.gitattributes",
        b"!KNOWELL_CANARY_negative text\n",
    );
    assert_rejected(&repository, Some(&root), "attribute rule is invalid");
    sandbox.write(
        &repository,
        "app/.gitattributes",
        b"file.txt KNOWELL_CANARY/invalid\n",
    );
    assert_rejected(&repository, Some(&root), "attribute assignment is invalid");
    sandbox.write(
        &repository,
        "app/.gitattributes",
        b"file.txt text\0KNOWELL_CANARY\n",
    );
    assert_rejected(&repository, Some(&root), "not valid text");
    sandbox.write(
        &repository,
        "app/.gitattributes",
        &vec![b'#'; 64 * 1024 + 1],
    );
    assert_rejected(&repository, Some(&root), "bounded regular file");
}

#[test]
fn excluded_controls_and_ignored_tracked_attribute_ancestors_are_metadata_only() {
    let sandbox = Sandbox::new();
    let repository = supported_repo(&sandbox, "nested-controls");
    sandbox.write(&repository, "app/src/file.txt", b"plain\n");
    sandbox.write(&repository, "app/.gitignore", b"src/\n");
    sandbox.write(&repository, "app/src/.gitattributes", b"* text eol=lf\n");
    sandbox.git(&repository, &["add", "--force", "app/src"]);
    sandbox.commit_all(&repository, "synthetic nested base");
    sandbox.write(&repository, "app/src/file.txt", b"plain\r\n");
    sandbox.write(&repository, "app/src/hidden.txt", b"untracked ignored\n");
    let policy =
        ExclusionPolicy::with_patterns(&[".gitignore".to_owned(), "src/.gitattributes".to_owned()])
            .unwrap();
    let before = std::fs::read(repository.join(".git/index")).unwrap();
    assert!(
        open(&repository)
            .working_changes_scoped(Some(&path("app")), &policy, &WalkOptions::default())
            .unwrap()
            .is_empty(),
        "ignored tracked ancestors must still supply normalization metadata"
    );
    assert_eq!(
        std::fs::read(repository.join(".git/index")).unwrap(),
        before
    );
}

#[test]
fn same_repository_ignore_precedence_does_not_widen_source_scope() {
    let sandbox = Sandbox::new();
    let repository = supported_repo(&sandbox, "ignore-precedence");
    sandbox.write(&repository, ".gitignore", b"/app/outer.txt\n*.tmp\n");
    sandbox.write(&repository, "app/.gitignore", b"!keep.tmp\n");
    sandbox.write(&repository, "app/nested/.gitignore", b"hidden.txt\n");
    sandbox.write(&repository, "app/tracked.txt", b"plain\n");
    sandbox.write(&repository, ".git/info/exclude", b"info.txt\nkeep.tmp\n");
    sandbox.commit_all(&repository, "synthetic ignore precedence base");
    // Changed outside-root controls influence status, but are not candidates.
    sandbox.write(
        &repository,
        ".gitignore",
        b"/app/outer.txt\n*.tmp\n# changed metadata\n",
    );
    for name in [
        "app/outer.txt",
        "app/info.txt",
        "app/drop.tmp",
        "app/keep.tmp",
        "app/nested/hidden.txt",
        "app/nested/new.txt",
    ] {
        sandbox.write(&repository, name, b"synthetic untracked\n");
    }
    let before = std::fs::read(repository.join(".git/index")).unwrap();
    assert_eq!(
        open(&repository)
            .working_changes_scoped(
                Some(&path("app")),
                &ExclusionPolicy::builtin(),
                &WalkOptions::default()
            )
            .unwrap(),
        [
            Change::Added(path("app/keep.tmp")),
            Change::Added(path("app/nested/new.txt"))
        ]
    );
    assert_eq!(
        std::fs::read(repository.join(".git/index")).unwrap(),
        before
    );
}

#[test]
fn ignored_project_root_ancestors_suppress_untracked_descendants_only() {
    let sandbox = Sandbox::new();
    let repository = supported_repo(&sandbox, "ignored-root-ancestor");
    sandbox.write(&repository, ".gitignore", b"packages/\n");
    sandbox.write(&repository, "packages/.gitignore", b"!app/\n");
    sandbox.write(
        &repository,
        "packages/app/.gitignore",
        b"!scratch.txt\n!nested/\n!nested/scratch.txt\n",
    );
    sandbox.write(&repository, "packages/app/tracked.txt", b"first\n");
    sandbox.git(
        &repository,
        &[
            "add",
            "--force",
            ".gitignore",
            "packages/app/tracked.txt",
            "packages/app/.gitignore",
        ],
    );
    sandbox.commit_all(&repository, "synthetic ignored project parent base");
    sandbox.write(&repository, "packages/app/tracked.txt", b"other\n");
    for name in [
        "packages/app/scratch.txt",
        "packages/app/nested/scratch.txt",
    ] {
        sandbox.write(&repository, name, b"untracked descendant\n");
    }
    let before = std::fs::read(repository.join(".git/index")).unwrap();
    assert_eq!(
        open(&repository)
            .working_changes_scoped(
                Some(&path("packages/app")),
                &ExclusionPolicy::builtin(),
                &WalkOptions::default()
            )
            .unwrap(),
        [Change::Modified(path("packages/app/tracked.txt"))],
        "nested negations must not re-include files beneath an ignored project ancestor"
    );
    assert_eq!(
        std::fs::read(repository.join(".git/index")).unwrap(),
        before
    );
}

#[test]
fn native_eol_and_info_attribute_precedence_preserve_clean_status() {
    let sandbox = Sandbox::new();
    let repository = supported_repo(&sandbox, "eol-precedence");
    sandbox.write(&repository, ".gitattributes", b"app/*.txt text eol=lf\n");
    sandbox.write(&repository, "app/.gitattributes", b"file.txt -text\n");
    sandbox.write(&repository, "app/file.txt", b"first\nsecond\n");
    sandbox.write(
        &repository,
        ".git/info/attributes",
        b"app/file.txt text eol=lf\n",
    );
    sandbox.commit_all(&repository, "synthetic EOL precedence base");
    sandbox.write(
        &repository,
        ".gitattributes",
        b"app/*.txt text eol=lf\n# changed metadata\n",
    );
    sandbox.write(&repository, "app/file.txt", b"first\r\nsecond\r\n");
    let before = std::fs::read(repository.join(".git/index")).unwrap();
    assert!(
        open(&repository)
            .working_changes_scoped(
                Some(&path("app")),
                &ExclusionPolicy::builtin(),
                &WalkOptions::default()
            )
            .unwrap()
            .is_empty(),
        "info attributes must override nested -text without raw-size false changes"
    );
    assert_eq!(
        std::fs::read(repository.join(".git/index")).unwrap(),
        before
    );
    sandbox.write(&repository, "app/file.txt", b"first\r\nchanged\r\n");
    assert_eq!(
        open(&repository)
            .working_changes_scoped(
                Some(&path("app")),
                &ExclusionPolicy::builtin(),
                &WalkOptions::default()
            )
            .unwrap(),
        [Change::Modified(path("app/file.txt"))]
    );
}

#[test]
fn irrelevant_attributes_and_worker_reuse_do_not_leak_normalization_between_files() {
    let sandbox = Sandbox::new();
    let repository = supported_repo(&sandbox, "attribute-outcome-reuse");
    let mut attributes = String::from(
        "[attr]native text eol=lf\napp/normalized.txt native\napp/binary.txt -text\nunused.txt",
    );
    for number in 0..2000 {
        attributes.push_str(&format!(" irrelevant_{number}"));
    }
    attributes.push('\n');
    assert!(attributes.len() <= 64 * 1024);
    sandbox.write(&repository, ".gitattributes", attributes.as_bytes());
    sandbox.write(
        &repository,
        ".git/info/attributes",
        b"app/normalized.txt eol=crlf\n",
    );
    sandbox.write(&repository, "app/normalized.txt", b"first\nsecond\n");
    sandbox.write(&repository, "app/binary.txt", b"first\r\nsecond\r\n");
    sandbox.commit_all(&repository, "synthetic independent normalization base");
    sandbox.write(&repository, "app/normalized.txt", b"first\r\nsecond\r\n");
    let before = std::fs::read(repository.join(".git/index")).unwrap();
    assert!(
        open(&repository)
            .working_changes_scoped(
                Some(&path("app")),
                &ExclusionPolicy::builtin(),
                &WalkOptions::default()
            )
            .unwrap()
            .is_empty(),
        "reused native matching must normalize one file and retain binary bytes in the other"
    );
    sandbox.write(&repository, "app/binary.txt", b"first\nsecond\n");
    assert_eq!(
        open(&repository)
            .working_changes_scoped(
                Some(&path("app")),
                &ExclusionPolicy::builtin(),
                &WalkOptions::default()
            )
            .unwrap(),
        [Change::Modified(path("app/binary.txt"))]
    );
    assert_eq!(
        std::fs::read(repository.join(".git/index")).unwrap(),
        before
    );
}

#[test]
fn autocrlf_uses_native_normalization_and_bounds_the_approved_old_blob() {
    let sandbox = Sandbox::new();
    let repository = supported_repo(&sandbox, "autocrlf");
    sandbox.write(&repository, "app/file.txt", b"first\nsecond\n");
    sandbox.commit_all(&repository, "synthetic autocrlf base");
    sandbox.write(&repository, "app/file.txt", b"first\r\nsecond\r\n");
    for value in ["true", "input"] {
        sandbox.git(&repository, &["config", "core.autocrlf", value]);
        assert!(
            open(&repository)
                .working_changes_scoped(
                    Some(&path("app")),
                    &ExclusionPolicy::builtin(),
                    &WalkOptions::default()
                )
                .unwrap()
                .is_empty()
        );
    }
    let options = WalkOptions {
        max_file_bytes: 8,
        ..Default::default()
    };
    // A small current file does not authorize an oversized prior index blob.
    sandbox.write(&repository, "app/file.txt", b"x\r\n");
    incomplete(
        open(&repository)
            .working_changes_scoped(Some(&path("app")), &ExclusionPolicy::builtin(), &options)
            .unwrap_err(),
        "native Git normalization",
    );
}

#[test]
fn captured_control_metadata_has_an_aggregate_byte_limit() {
    let sandbox = Sandbox::new();
    let repository = supported_repo(&sandbox, "control-budget");
    let mut comment = vec![b'x'; 64 * 1024];
    comment[0] = b'#';
    for number in 0..17 {
        sandbox.write(
            &repository,
            &format!("app/dir-{number}/.gitattributes"),
            &comment,
        );
        sandbox.write(
            &repository,
            &format!("app/dir-{number}/file.txt"),
            b"plain\n",
        );
    }
    sandbox.commit_all(&repository, "synthetic bounded metadata base");
    assert_rejected(
        &repository,
        Some(&path("app")),
        "metadata exceeds its limit",
    );
}

#[cfg(unix)]
#[test]
fn symbolic_control_files_are_rejected_without_following_them() {
    use std::os::unix::fs::symlink;
    let sandbox = Sandbox::new();
    let repository = supported_repo(&sandbox, "symlink-controls");
    sandbox.write(&repository, "app/file.txt", b"plain\n");
    sandbox.commit_all(&repository, "synthetic control symlink base");
    let outside = sandbox.path().join("private-control");
    std::fs::write(&outside, b"KNOWELL_CANARY_symlink_rule\n").unwrap();
    for name in [".gitignore", ".gitattributes", ".git/info/attributes"] {
        symlink(&outside, repository.join(name)).unwrap();
        assert_rejected(&repository, Some(&path("app")), "symbolic link");
        std::fs::remove_file(repository.join(name)).unwrap();
    }
}

#[test]
fn case_insensitive_ignore_rules_do_not_admit_untracked_paths() {
    let sandbox = Sandbox::new();
    let repository = supported_repo(&sandbox, "case-ignore");
    sandbox.write(&repository, "app/.gitignore", b"ignored.txt\n");
    sandbox.write(&repository, "app/file.txt", b"plain\n");
    sandbox.commit_all(&repository, "synthetic ignore base");
    sandbox.git(&repository, &["config", "core.ignoreCase", "true"]);
    sandbox.write(&repository, "app/IGNORED.txt", b"untracked\n");
    assert!(
        open(&repository)
            .working_changes_scoped(
                Some(&path("app")),
                &ExclusionPolicy::builtin(),
                &WalkOptions::default()
            )
            .unwrap()
            .is_empty()
    );
}

#[test]
fn case_aliases_preserve_indexed_spelling_instead_of_adding_untracked_copies() {
    let sandbox = Sandbox::new();
    let repository = supported_repo(&sandbox, "case-alias");
    sandbox.write(&repository, "app/File.txt", b"plain\n");
    sandbox.commit_all(&repository, "synthetic case alias base");
    sandbox.git(&repository, &["config", "core.ignoreCase", "true"]);
    std::fs::rename(
        repository.join("app/File.txt"),
        repository.join("app/file.txt"),
    )
    .unwrap();
    let changes = open(&repository)
        .working_changes_scoped(
            Some(&path("app")),
            &ExclusionPolicy::builtin(),
            &WalkOptions::default(),
        )
        .unwrap();
    // On case-sensitive hosts the indexed spelling is now absent; hosts with
    // folded paths still find the unchanged file. Neither case may add an alias.
    assert!(
        changes
            .iter()
            .all(|change| *change == Change::Deleted(path("app/File.txt")))
    );
    assert!(!changes.contains(&Change::Added(path("app/file.txt"))));
}
