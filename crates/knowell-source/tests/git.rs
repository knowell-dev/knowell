//! Integration tests for `knowell_source::git` against real repositories
//! created with the git CLI in temporary directories.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use knowell_core::{ContentHash, RepoPath, TrackTarget};
use knowell_secrets::{Exclusion, ExclusionPolicy, SensitiveKind};
use knowell_source::git::{
    Change, EntryKind, GitError, GitRepo, PathPattern, ResolvedKind, TaskGrouping,
    group_task_views, working_changes,
};
use knowell_source::{FileRead, SkipReason, WalkOptions, WalkReport, walk};
use support::{Sandbox, canary, canon, fake_token, open};

fn p(s: &str) -> RepoPath {
    RepoPath::new(s).unwrap()
}

fn target(s: &str) -> TrackTarget {
    s.parse().unwrap()
}

fn skip_of<'a>(report: &'a WalkReport, path: &str) -> Option<&'a SkipReason> {
    report
        .skipped
        .iter()
        .find(|s| s.path.as_str() == path)
        .map(|s| &s.reason)
}

fn file_paths(report: &WalkReport) -> Vec<&str> {
    report.files.iter().map(|f| f.path.as_str()).collect()
}

/// main: c1 -> c2; branch `dev` and remote-tracking `origin/dev` at c1;
/// lightweight tag `light` at c1, annotated tag `v1` at c2.
fn history(sb: &Sandbox) -> (std::path::PathBuf, String, String) {
    let repo = sb.init("hist");
    sb.write(&repo, "README.md", b"# one\n");
    let c1 = sb.commit_all(&repo, "c1");
    sb.write(&repo, "README.md", b"# two\n");
    let c2 = sb.commit_all(&repo, "c2");
    sb.git(&repo, &["branch", "dev", &c1]);
    sb.git(&repo, &["update-ref", "refs/remotes/origin/dev", &c1]);
    sb.git(&repo, &["tag", "light", &c1]);
    sb.git(&repo, &["tag", "-a", "v1", "-m", "release v1", &c2]);
    (repo, c1, c2)
}

#[test]
fn resolves_every_target_kind() {
    let sb = Sandbox::new();
    let (repo_path, c1, c2) = history(&sb);
    let repo = open(&repo_path);

    let cases = [
        (
            "branch:main",
            &c2,
            Some("refs/heads/main"),
            ResolvedKind::LocalBranch,
        ),
        (
            "branch:dev",
            &c1,
            Some("refs/heads/dev"),
            ResolvedKind::LocalBranch,
        ),
        (
            "remote:origin/dev",
            &c1,
            Some("refs/remotes/origin/dev"),
            ResolvedKind::RemoteBranch,
        ),
        ("tag:light", &c1, Some("refs/tags/light"), ResolvedKind::Tag),
        // Annotated tag: peeled to the commit, not the tag object.
        ("tag:v1", &c2, Some("refs/tags/v1"), ResolvedKind::Tag),
        (
            "worktree",
            &c2,
            Some("refs/heads/main"),
            ResolvedKind::WorktreeBranch,
        ),
    ];
    for (text, commit, reference, kind) in cases {
        let resolved = repo.resolve(&target(text)).unwrap();
        assert_eq!(&resolved.commit, commit, "{text}");
        assert_eq!(resolved.reference.as_deref(), reference, "{text}");
        assert_eq!(resolved.kind, kind, "{text}");
    }
    let tag_object = sb.git(&repo_path, &["rev-parse", "refs/tags/v1"]);
    assert_ne!(tag_object, c2, "v1 must be an annotated tag object");

    let by_id = repo.resolve(&target(&format!("commit:{c1}"))).unwrap();
    assert_eq!(by_id.commit, c1);
    assert_eq!(by_id.reference, None);
    assert_eq!(by_id.kind, ResolvedKind::Commit);

    sb.git(&repo_path, &["checkout", "-q", "--detach", &c1]);
    let detached = repo.resolve(&TrackTarget::WorktreeHead).unwrap();
    assert_eq!(detached.commit, c1);
    assert_eq!(detached.reference, None);
    assert_eq!(detached.kind, ResolvedKind::WorktreeDetached);
}

#[test]
fn missing_refs_are_errors_never_substituted() {
    let sb = Sandbox::new();
    let (repo_path, c1, _) = history(&sb);
    sb.git(&repo_path, &["tag", "release", &c1]);
    let repo = open(&repo_path);

    for text in [
        "branch:master",
        "branch:nope",
        "remote:origin/main",
        "remote:upstream/dev",
        "tag:v2",
        // A tag and a remote branch exist under these names, but not a
        // local branch: git's DWIM lookup must not be used as a fallback.
        "branch:release",
        "branch:origin/dev",
        "tag:dev",
    ] {
        match repo.resolve(&target(text)) {
            Err(GitError::RefNotFound { target: t }) => assert_eq!(t.to_string(), text),
            other => panic!("{text}: expected RefNotFound, got {other:?}"),
        }
    }

    let missing = "0123456789abcdef0123456789abcdef01234567";
    assert!(matches!(
        repo.resolve(&target(&format!("commit:{missing}"))),
        Err(GitError::CommitNotFound { id }) if id == missing
    ));

    let tree = sb.git(&repo_path, &["rev-parse", &format!("{c1}^{{tree}}")]);
    assert!(matches!(
        repo.resolve(&target(&format!("commit:{tree}"))),
        Err(GitError::NotACommit { kind, .. }) if kind == "tree"
    ));

    // A tag that points to a blob does not resolve to a commit.
    let blob = sb.git(&repo_path, &["rev-parse", &format!("{c1}:README.md")]);
    sb.git(&repo_path, &["tag", "blobtag", &blob]);
    assert!(matches!(
        repo.resolve(&target("tag:blobtag")),
        Err(GitError::NotACommit { kind, .. }) if kind == "blob"
    ));

    // Ids for the other operations are validated the same way.
    assert!(matches!(
        repo.list_tree("abc"),
        Err(GitError::InvalidObjectId { .. })
    ));
    assert!(matches!(
        repo.list_tree(&"g".repeat(40)),
        Err(GitError::InvalidObjectId { .. })
    ));
    assert!(matches!(
        repo.diff(missing, &c1),
        Err(GitError::CommitNotFound { .. })
    ));
    assert!(matches!(
        repo.is_ancestor(&c1, &tree),
        Err(GitError::NotACommit { .. })
    ));
}

#[test]
fn unborn_and_bare_repositories() {
    let sb = Sandbox::new();
    let fresh = sb.init("fresh");
    let repo = open(&fresh);
    assert!(matches!(
        repo.resolve(&TrackTarget::WorktreeHead),
        Err(GitError::UnbornHead { branch }) if branch == "refs/heads/main"
    ));
    let trees = repo.worktrees().unwrap();
    assert_eq!(trees.len(), 1);
    assert_eq!(trees[0].head_commit, None);
    assert_eq!(trees[0].branch.as_deref(), Some("main"));
    // Untracked files of an unborn branch are additions.
    sb.write(&fresh, "first.txt", b"hello\n");
    assert_eq!(
        repo.working_changes(&ExclusionPolicy::builtin()).unwrap(),
        [Change::Added(p("first.txt"))]
    );

    let bare = sb.path().join("bare.git");
    sb.git(sb.path(), &["init", "-q", "--bare", bare.to_str().unwrap()]);
    let repo = open(&bare);
    assert!(repo.workdir().is_none());
    assert!(repo.worktrees().unwrap().is_empty());
    assert!(matches!(
        repo.working_changes(&ExclusionPolicy::builtin()),
        Err(GitError::NoWorktree { .. })
    ));
}

#[test]
fn open_requires_a_repository_root() {
    let sb = Sandbox::new();
    let repo = sb.init("r");
    sb.write(&repo, "sub/a.txt", b"a\n");
    sb.commit_all(&repo, "c1");
    assert!(matches!(
        GitRepo::open(&repo.join("sub")),
        Err(GitError::NotARepository { .. })
    ));
    assert!(matches!(
        GitRepo::open(&sb.path().join("missing")),
        Err(GitError::NotARepository { .. })
    ));
    // The user-configuration mode agrees with the isolated one here.
    let honouring = GitRepo::open(&repo).unwrap();
    let isolated = open(&repo);
    let head = target("branch:main");
    assert_eq!(
        honouring.resolve(&head).unwrap(),
        isolated.resolve(&head).unwrap()
    );
    let commit = isolated.resolve(&head).unwrap().commit;
    assert_eq!(
        honouring.list_tree(&commit).unwrap(),
        isolated.list_tree(&commit).unwrap()
    );
}

#[test]
fn list_tree_reports_kinds_sorted() {
    let sb = Sandbox::new();
    let repo_path = sb.init("kinds");
    sb.write(&repo_path, "b/z.txt", b"z\n");
    sb.write(&repo_path, "a.txt", b"a\n");
    sb.write(&repo_path, "b.txt", b"b\n");
    sb.write(&repo_path, "tool.sh", b"#!/bin/sh\n");
    let c1 = sb.commit_all(&repo_path, "c1");
    // Symlink and gitlink entries, written straight into the index so the
    // test does not depend on symlink support of the platform.
    let link_blob = sb.git_stdin(&repo_path, &["hash-object", "-w", "--stdin"], b"a.txt");
    sb.git(
        &repo_path,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("120000,{link_blob},link"),
        ],
    );
    sb.git(
        &repo_path,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{c1},vendor/sub"),
        ],
    );
    sb.git(&repo_path, &["update-index", "--chmod=+x", "tool.sh"]);
    sb.git(&repo_path, &["commit", "-q", "-m", "c2"]);
    let c2 = sb.git(&repo_path, &["rev-parse", "HEAD"]);

    let entries = open(&repo_path).list_tree(&c2).unwrap();
    let summary: Vec<(&str, EntryKind)> =
        entries.iter().map(|e| (e.path.as_str(), e.mode)).collect();
    // Byte-wise path order ("b.txt" < "b/z.txt"), not git's tree order.
    assert_eq!(
        summary,
        [
            ("a.txt", EntryKind::File),
            ("b.txt", EntryKind::File),
            ("b/z.txt", EntryKind::File),
            ("link", EntryKind::Symlink),
            ("tool.sh", EntryKind::Executable),
            ("vendor/sub", EntryKind::Submodule),
        ]
    );
    for entry in &entries {
        let expected = sb.git(&repo_path, &["rev-parse", &format!("{c2}:{}", entry.path)]);
        assert_eq!(entry.blob, expected, "{}", entry.path);
    }
}

#[test]
fn walk_commit_excludes_redacts_and_matches_the_fs_walker() {
    let sb = Sandbox::new();
    let repo_path = sb.init("walk");
    let client = format!("// line1\nconst gh = \"{}\";\n", fake_token());
    sb.write(
        &repo_path,
        ".env",
        format!("API_TOKEN={}\n", canary()).as_bytes(),
    );
    sb.write(&repo_path, "config/id_rsa", canary().as_bytes());
    sb.write(&repo_path, ".ssh/known_hosts", canary().as_bytes());
    sb.write(&repo_path, "src/client.ts", client.as_bytes());
    sb.write(&repo_path, "README.md", b"\xEF\xBB\xBF# readme\r\n");
    sb.write(&repo_path, "big.txt", &[b'a'; 2048]);
    sb.write(&repo_path, "data.bin", &[0x89, b'P', 0, 1, 2]);
    sb.write(&repo_path, "latin1.txt", &[b'c', b'a', b'f', 0xE9]);
    sb.write(&repo_path, "vendor/lib.js", b"x\n");
    let c1 = sb.commit_all(&repo_path, "c1");
    let link_blob = sb.git_stdin(&repo_path, &["hash-object", "-w", "--stdin"], b".env");
    sb.git(
        &repo_path,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("120000,{link_blob},env-link"),
        ],
    );
    sb.git(
        &repo_path,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{c1},deps/sub"),
        ],
    );
    sb.git(&repo_path, &["commit", "-q", "-m", "c2"]);
    let c2 = sb.git(&repo_path, &["rev-parse", "HEAD"]);

    // Edit the checkout afterwards: the commit walk must not see it.
    sb.write(&repo_path, "src/client.ts", b"changed in the worktree\n");

    let repo = open(&repo_path);
    let policy = ExclusionPolicy::with_patterns(&["vendor".to_owned()]).unwrap();
    let options = WalkOptions {
        max_file_bytes: 1024,
        ..WalkOptions::default()
    };
    let report = repo.walk_commit(&c2, &policy, &options).unwrap();

    assert_eq!(file_paths(&report), ["README.md", "src/client.ts"]);
    let debug = format!("{report:?}");
    assert!(!debug.contains(&canary()));
    assert!(!debug.contains(&fake_token()));

    let client_file = &report.files[1];
    assert_eq!(
        client_file.text,
        "// line1\nconst gh = \"[REDACTED:github_token]\";\n"
    );
    assert_eq!(client_file.hash, ContentHash::of(client.as_bytes()));
    assert_eq!(client_file.size, client.len() as u64);
    assert_eq!(client_file.redactions.len(), 1);
    assert_eq!(report.files[0].text, "# readme\r\n");

    assert_eq!(
        skip_of(&report, ".env"),
        Some(&SkipReason::Excluded(Exclusion::Sensitive(
            SensitiveKind::EnvFile
        )))
    );
    assert_eq!(
        skip_of(&report, "config/id_rsa"),
        Some(&SkipReason::Excluded(Exclusion::Sensitive(
            SensitiveKind::PrivateKey
        )))
    );
    assert_eq!(
        skip_of(&report, ".ssh"),
        Some(&SkipReason::Excluded(Exclusion::Sensitive(
            SensitiveKind::PrivateKey
        )))
    );
    assert_eq!(
        skip_of(&report, "vendor"),
        Some(&SkipReason::Excluded(Exclusion::Pattern(
            "vendor".to_owned()
        )))
    );
    assert_eq!(
        skip_of(&report, "big.txt"),
        Some(&SkipReason::TooLarge { size: 2048 })
    );
    assert_eq!(skip_of(&report, "data.bin"), Some(&SkipReason::Binary));
    assert_eq!(skip_of(&report, "latin1.txt"), Some(&SkipReason::NotUtf8));
    assert_eq!(skip_of(&report, "env-link"), Some(&SkipReason::Symlink));
    // Submodules have no content here; pruned directories hide children.
    assert!(skip_of(&report, "deps/sub").is_none());
    assert!(skip_of(&report, "vendor/lib.js").is_none());
    assert!(skip_of(&report, ".ssh/known_hosts").is_none());
    assert_eq!(report.skipped.len(), 8);
    assert!(report.skipped.windows(2).all(|w| w[0].path <= w[1].path));

    // Same pipeline as the filesystem walker: identical results for files
    // that are identical in the commit and on disk.
    sb.write(&repo_path, "src/client.ts", client.as_bytes());
    let on_disk = walk(&repo_path, &policy, &options).unwrap();
    for name in ["README.md", "src/client.ts"] {
        let from_git = report
            .files
            .iter()
            .find(|f| f.path.as_str() == name)
            .unwrap();
        let from_fs = on_disk
            .files
            .iter()
            .find(|f| f.path.as_str() == name)
            .unwrap();
        assert_eq!(from_git, from_fs, "{name}");
    }

    // Deterministic.
    assert_eq!(repo.walk_commit(&c2, &policy, &options).unwrap(), report);
}

#[test]
fn walk_commit_reads_a_branch_that_is_not_checked_out() {
    let sb = Sandbox::new();
    let repo_path = sb.init("other");
    sb.write(&repo_path, "a.txt", b"main\n");
    sb.commit_all(&repo_path, "c1");
    sb.git(&repo_path, &["checkout", "-q", "-b", "feature"]);
    sb.write(&repo_path, "a.txt", b"feature version\n");
    sb.write(&repo_path, "only-feature.txt", b"f\n");
    sb.commit_all(&repo_path, "c2");
    sb.git(&repo_path, &["checkout", "-q", "main"]);
    sb.git(
        &repo_path,
        &["update-ref", "refs/remotes/origin/feature", "feature"],
    );

    let repo = open(&repo_path);
    let resolved = repo.resolve(&target("remote:origin/feature")).unwrap();
    let report = repo
        .walk_commit(
            &resolved.commit,
            &ExclusionPolicy::builtin(),
            &WalkOptions::default(),
        )
        .unwrap();
    assert_eq!(file_paths(&report), ["a.txt", "only-feature.txt"]);
    assert_eq!(report.files[0].text, "feature version\n");
    // The checkout is untouched.
    assert_eq!(std::fs::read(repo_path.join("a.txt")).unwrap(), b"main\n");
    assert!(!repo_path.join("only-feature.txt").exists());
}

#[test]
fn walk_commit_reports_unrepresentable_names() {
    let sb = Sandbox::new();
    let repo_path = sb.init("names");
    let blob = sb.git_stdin(&repo_path, &["hash-object", "-w", "--stdin"], b"x\n");
    let mut listing = Vec::new();
    for name in [&b"a\\b.txt"[..], b"bad\xffname.txt", b"c:x.txt", b"ok.txt"] {
        listing.extend_from_slice(format!("100644 blob {blob}\t").as_bytes());
        listing.extend_from_slice(name);
        listing.push(b'\n');
    }
    let tree = sb.git_stdin(&repo_path, &["mktree"], &listing);
    let commit = sb.git(&repo_path, &["commit-tree", &tree, "-m", "odd names"]);

    let repo = open(&repo_path);
    let report = repo
        .walk_commit(
            &commit,
            &ExclusionPolicy::builtin(),
            &WalkOptions::default(),
        )
        .unwrap();
    assert_eq!(file_paths(&report), ["ok.txt"]);
    let mut invalid: Vec<&str> = report
        .skipped
        .iter()
        .filter(|s| s.reason == SkipReason::InvalidPath)
        .map(|s| s.path.as_str())
        .collect();
    invalid.sort_unstable();
    assert_eq!(invalid, ["a?b.txt", "bad\u{FFFD}name.txt", "c?x.txt"]);
    // list_tree omits what it cannot represent.
    let listed: Vec<String> = repo
        .list_tree(&commit)
        .unwrap()
        .into_iter()
        .map(|e| e.path.to_string())
        .collect();
    assert_eq!(listed, ["ok.txt"]);
}

#[test]
fn read_commit_files_reads_only_requested_paths() {
    let sb = Sandbox::new();
    let repo_path = sb.init("read");
    sb.write(&repo_path, "src/a.rs", b"fn a() {}\n");
    sb.write(&repo_path, "src/b.rs", b"fn b() {}\n");
    sb.write(&repo_path, ".env", canary().as_bytes());
    let c1 = sb.commit_all(&repo_path, "c1");
    let repo = open(&repo_path);
    let policy = ExclusionPolicy::builtin();
    let options = WalkOptions::default();

    let report = repo
        .read_commit_files(
            &c1,
            &[p("src/b.rs"), p(".env"), p("src/b.rs")],
            &policy,
            &options,
        )
        .unwrap();
    assert_eq!(file_paths(&report), ["src/b.rs"]);
    assert_eq!(report.files[0].text, "fn b() {}\n");
    assert!(matches!(
        skip_of(&report, ".env"),
        Some(SkipReason::Excluded(_))
    ));
    assert!(!format!("{report:?}").contains(&canary()));

    for missing in ["src/missing.rs", "src", "src/a.rs/x"] {
        assert!(
            matches!(
                repo.read_commit_files(&c1, &[p(missing)], &policy, &options),
                Err(GitError::PathNotFound { path, .. }) if path.as_str() == missing
            ),
            "{missing}"
        );
    }
}

#[test]
fn read_commit_file_reads_one_path_like_the_walker() {
    let sb = Sandbox::new();
    let repo_path = sb.init("one");
    sb.write(
        &repo_path,
        "src/a.rs",
        format!("const T: &str = \"{}\";\n", fake_token()).as_bytes(),
    );
    sb.write(&repo_path, "src/big.txt", &[b'a'; 64]);
    sb.write(&repo_path, "src/bin.dat", b"a\0b");
    sb.write(&repo_path, ".env", canary().as_bytes());
    let c1 = sb.commit_all(&repo_path, "c1");
    let repo = open(&repo_path);
    let policy = ExclusionPolicy::builtin();
    // `src/a.rs` (60 bytes) fits, `src/big.txt` (64 bytes) does not.
    let options = WalkOptions {
        max_file_bytes: 63,
        ..WalkOptions::default()
    };
    let walked = repo.walk_commit(&c1, &policy, &options).unwrap();

    let read = |path: &str| {
        repo.read_commit_file(&c1, &p(path), &policy, &options)
            .unwrap()
    };
    let FileRead::File(file) = read("src/a.rs") else {
        panic!("src/a.rs should be readable");
    };
    assert_eq!(
        Some(&file),
        walked.files.iter().find(|f| f.path == file.path)
    );
    assert!(!file.text.contains(&fake_token()));
    assert_eq!(
        read("src/big.txt"),
        FileRead::Skipped(SkipReason::TooLarge { size: 64 })
    );
    assert_eq!(read("src/bin.dat"), FileRead::Skipped(SkipReason::Binary));
    assert!(matches!(
        read(".env"),
        FileRead::Skipped(SkipReason::Excluded(_))
    ));
    assert!(!format!("{:?}", read(".env")).contains(&canary()));
    // Absent paths and directories are missing, not errors.
    for missing in ["src/missing.rs", "src", "src/a.rs/x"] {
        assert_eq!(read(missing), FileRead::Missing, "{missing}");
    }
    // The checkout and the commit agree on the bytes, so both readers agree.
    let FileRead::File(on_disk) =
        knowell_source::read_file(&repo_path, &p("src/a.rs"), &policy, &options).unwrap()
    else {
        panic!("src/a.rs should be readable from the checkout");
    };
    assert_eq!(on_disk, file);
    assert!(matches!(
        repo.read_commit_file(&"0".repeat(40), &p("src/a.rs"), &policy, &options),
        Err(GitError::CommitNotFound { .. })
    ));
}

#[test]
fn diff_tracks_renames_deletes_modifications_and_additions() {
    let sb = Sandbox::new();
    let repo_path = sb.init("diff");
    let numbered = |word: &str| -> String { (1..=20).map(|i| format!("{word} {i}\n")).collect() };
    sb.write(&repo_path, "keep.txt", b"keep\n");
    sb.write(&repo_path, "old_name.txt", numbered("line").as_bytes());
    sb.write(&repo_path, "moved_edit.txt", numbered("row").as_bytes());
    sb.write(&repo_path, "gone.txt", b"bye\n");
    sb.write(&repo_path, "change.txt", b"v1\n");
    sb.write(&repo_path, "tool.sh", b"echo tool\n");
    sb.write(&repo_path, "dir/x.txt", b"x content unique 1\n");
    sb.write(&repo_path, "dir/y.txt", b"y content unique 2\n");
    let c1 = sb.commit_all(&repo_path, "c1");

    sb.git(&repo_path, &["mv", "old_name.txt", "new_name.txt"]);
    std::fs::create_dir_all(repo_path.join("edited")).unwrap();
    sb.git(&repo_path, &["mv", "moved_edit.txt", "edited/moved.txt"]);
    let edited = numbered("row").replace("row 5\n", "row five, edited\n");
    sb.write(&repo_path, "edited/moved.txt", edited.as_bytes());
    sb.git(&repo_path, &["mv", "dir", "dir2"]);
    std::fs::remove_file(repo_path.join("gone.txt")).unwrap();
    sb.write(&repo_path, "change.txt", b"version two\n");
    sb.write(&repo_path, "added.txt", b"fresh\n");
    sb.git(&repo_path, &["add", "-A"]);
    sb.git(&repo_path, &["update-index", "--chmod=+x", "tool.sh"]);
    sb.git(&repo_path, &["commit", "-q", "-m", "c2"]);
    let c2 = sb.git(&repo_path, &["rev-parse", "HEAD"]);

    let repo = open(&repo_path);
    let changes = repo.diff(&c1, &c2).unwrap();
    let similarity = match changes
        .iter()
        .find(|c| c.path().as_str() == "edited/moved.txt")
    {
        Some(Change::Renamed { similarity, .. }) => *similarity,
        other => panic!("expected a rename for edited/moved.txt, got {other:?}"),
    };
    assert!((50..100).contains(&similarity), "{similarity}");
    assert_eq!(
        changes,
        [
            Change::Added(p("added.txt")),
            Change::Modified(p("change.txt")),
            Change::Renamed {
                from: p("dir/x.txt"),
                to: p("dir2/x.txt"),
                similarity: 100
            },
            Change::Renamed {
                from: p("dir/y.txt"),
                to: p("dir2/y.txt"),
                similarity: 100
            },
            Change::Renamed {
                from: p("moved_edit.txt"),
                to: p("edited/moved.txt"),
                similarity
            },
            Change::Deleted(p("gone.txt")),
            Change::Renamed {
                from: p("old_name.txt"),
                to: p("new_name.txt"),
                similarity: 100
            },
            Change::Modified(p("tool.sh")),
        ]
    );
    // Reverse direction mirrors it; identical commits have no changes.
    let back = repo.diff(&c2, &c1).unwrap();
    assert!(back.contains(&Change::Deleted(p("added.txt"))));
    assert!(back.contains(&Change::Added(p("gone.txt"))));
    assert!(back.contains(&Change::Renamed {
        from: p("new_name.txt"),
        to: p("old_name.txt"),
        similarity: 100
    }));
    assert!(repo.diff(&c2, &c2).unwrap().is_empty());
}

fn remove_loose_blob(sb: &Sandbox, repository: &std::path::Path, commit: &str, path: &str) {
    let id = sb.git(repository, &["rev-parse", &format!("{commit}:{path}")]);
    assert_eq!(id.len(), 40);
    assert!(id.bytes().all(|byte| byte.is_ascii_hexdigit()));
    let object = repository
        .join(".git/objects")
        .join(&id[..2])
        .join(&id[2..]);
    assert!(object.is_file(), "the synthetic blob must be loose");
    std::fs::remove_file(object).unwrap();
}

#[test]
fn scoped_diff_filters_both_endpoints_before_rename_blob_access() {
    let sb = Sandbox::new();
    let repository = sb.init("scoped-diff");
    let numbered = |word: &str| -> String { (1..=20).map(|i| format!("{word} {i}\n")).collect() };
    sb.write(
        &repository,
        "selected/exact-old.txt",
        numbered("line").as_bytes(),
    );
    sb.write(
        &repository,
        "selected/edited-old.txt",
        numbered("row").as_bytes(),
    );
    sb.write(
        &repository,
        "selected/exit-root.txt",
        b"scope exit unique\n",
    );
    sb.write(
        &repository,
        "selected/exit-policy.txt",
        b"policy exit unique\n",
    );
    sb.write(
        &repository,
        "sibling/entry-root.txt",
        b"scope entry unique\n",
    );
    sb.write(
        &repository,
        "selected/private/entry-policy.txt",
        b"policy entry unique\n",
    );
    sb.write(
        &repository,
        "selected/.env.old",
        b"KNOWELL_CANARY_scoped_env_old\n",
    );
    sb.write(
        &repository,
        "selected/private/old.txt",
        b"KNOWELL_CANARY_scoped_private_old\n",
    );
    sb.write(
        &repository,
        "sibling/old.txt",
        b"KNOWELL_CANARY_scoped_sibling_old\n",
    );
    sb.write(
        &repository,
        "selected-neighbor/old.txt",
        b"KNOWELL_CANARY_scoped_neighbor_old\n",
    );
    sb.write(&repository, ".gitattributes", b"* -diff\n");
    let old = sb.commit_all(&repository, "old scoped fixture");
    for (from, to) in [
        ("selected/exact-old.txt", "selected/exact-new.txt"),
        ("selected/edited-old.txt", "selected/edited-new.txt"),
        ("selected/exit-root.txt", "sibling/exit-root.txt"),
        (
            "selected/exit-policy.txt",
            "selected/private/exit-policy.txt",
        ),
        ("sibling/entry-root.txt", "selected/entry-root.txt"),
        (
            "selected/private/entry-policy.txt",
            "selected/entry-policy.txt",
        ),
    ] {
        sb.git(&repository, &["mv", from, to]);
    }
    sb.write(
        &repository,
        "selected/edited-new.txt",
        numbered("row")
            .replace("row 5\n", "row five, edited\n")
            .as_bytes(),
    );
    for path in [
        "selected/.env.old",
        "selected/private/old.txt",
        "sibling/old.txt",
        "selected-neighbor/old.txt",
    ] {
        std::fs::remove_file(repository.join(path)).unwrap();
    }
    for (path, content) in [
        ("selected/.env.new", "KNOWELL_CANARY_scoped_env_new\n"),
        (
            "selected/private/new.txt",
            "KNOWELL_CANARY_scoped_private_new\n",
        ),
        ("sibling/new.txt", "KNOWELL_CANARY_scoped_sibling_new\n"),
        (
            "selected-neighbor/new.txt",
            "KNOWELL_CANARY_scoped_neighbor_new\n",
        ),
    ] {
        sb.write(&repository, path, content.as_bytes());
    }
    let new = sb.commit_all(&repository, "new scoped fixture");
    for (commit, path) in [
        (&old, "selected/.env.old"),
        (&new, "selected/.env.new"),
        (&old, "selected/private/old.txt"),
        (&new, "selected/private/new.txt"),
        (&old, "sibling/old.txt"),
        (&new, "sibling/new.txt"),
        (&old, "selected-neighbor/old.txt"),
        (&new, "selected-neighbor/new.txt"),
        (&old, ".gitattributes"),
    ] {
        remove_loose_blob(&sb, &repository, commit, path);
    }
    // Similarity must ignore all configured attributes, including ancestor files.
    sb.write(&repository, ".git/info/attributes", b"* -diff\n");
    let attributes = sb.path().join("global-attributes");
    std::fs::write(&attributes, b"* -diff\n").unwrap();
    sb.git(
        &repository,
        &[
            "config",
            "core.attributesFile",
            attributes.to_str().unwrap(),
        ],
    );
    let before_objects = sb.git(&repository, &["count-objects", "-v"]);
    let before_index = std::fs::read(repository.join(".git/index")).unwrap();
    let repo = open(&repository);
    let root = p("selected");
    let policy = ExclusionPolicy::with_patterns(&["private/**".to_owned()]).unwrap();
    let changes = repo
        .diff_scoped(&old, &new, Some(&root), &policy, &WalkOptions::default())
        .unwrap();
    let similarity = match changes
        .iter()
        .find(|change| change.path() == &p("selected/edited-new.txt"))
    {
        Some(Change::Renamed { similarity, .. }) => *similarity,
        other => panic!("expected a permitted edited rename, got {other:?}"),
    };
    assert!((50..100).contains(&similarity));
    let mut expected = vec![
        Change::Renamed {
            from: p("selected/exact-old.txt"),
            to: p("selected/exact-new.txt"),
            similarity: 100,
        },
        Change::Renamed {
            from: p("selected/edited-old.txt"),
            to: p("selected/edited-new.txt"),
            similarity,
        },
        Change::Deleted(p("selected/exit-root.txt")),
        Change::Deleted(p("selected/exit-policy.txt")),
        Change::Added(p("selected/entry-root.txt")),
        Change::Added(p("selected/entry-policy.txt")),
    ];
    expected.sort_by(|left, right| left.path().cmp(right.path()));
    assert_eq!(changes, expected);
    assert_eq!(
        sb.git(&repository, &["count-objects", "-v"]),
        before_objects
    );
    assert_eq!(
        std::fs::read(repository.join(".git/index")).unwrap(),
        before_index
    );
    assert_eq!(
        std::fs::read(repository.join(".gitattributes")).unwrap(),
        b"* -diff\n"
    );
}

#[test]
fn default_diff_excludes_missing_sensitive_blob_candidates() {
    let sb = Sandbox::new();
    let repository = sb.init("default-sensitive-diff");
    sb.write(&repository, ".env.old", b"KNOWELL_CANARY_default_env_old\n");
    sb.write(&repository, "allowed.txt", b"before\n");
    let old = sb.commit_all(&repository, "old sensitive fixture");
    std::fs::remove_file(repository.join(".env.old")).unwrap();
    sb.write(&repository, ".env.new", b"KNOWELL_CANARY_default_env_new\n");
    sb.write(&repository, "allowed.txt", b"after\n");
    let new = sb.commit_all(&repository, "new sensitive fixture");
    remove_loose_blob(&sb, &repository, &old, ".env.old");
    remove_loose_blob(&sb, &repository, &new, ".env.new");
    assert_eq!(
        open(&repository).diff(&old, &new).unwrap(),
        [Change::Modified(p("allowed.txt"))]
    );
}

#[test]
fn scoped_diff_limits_inexact_matching_to_permitted_blob_sizes() {
    let sb = Sandbox::new();
    let repository = sb.init("size-limited-diff");
    let original: String = (1..=20).map(|i| format!("row {i}\n")).collect();
    sb.write(&repository, "exact-old.txt", b"exact content unchanged\n");
    sb.write(&repository, "edited-old.txt", original.as_bytes());
    let old = sb.commit_all(&repository, "old size fixture");
    sb.git(&repository, &["mv", "exact-old.txt", "exact-new.txt"]);
    sb.git(&repository, &["mv", "edited-old.txt", "edited-new.txt"]);
    sb.write(
        &repository,
        "edited-new.txt",
        original.replace("row 5\n", "row five, edited\n").as_bytes(),
    );
    let new = sb.commit_all(&repository, "new size fixture");
    let repo = open(&repository);
    for limit in [0, 1] {
        let options = WalkOptions {
            max_file_bytes: limit,
            ..WalkOptions::default()
        };
        assert_eq!(
            repo.diff_scoped(&old, &new, None, &ExclusionPolicy::builtin(), &options)
                .unwrap(),
            [
                Change::Added(p("edited-new.txt")),
                Change::Deleted(p("edited-old.txt")),
                Change::Renamed {
                    from: p("exact-old.txt"),
                    to: p("exact-new.txt"),
                    similarity: 100
                },
            ]
        );
    }
}

#[test]
fn scoped_diff_filters_symlink_and_gitlink_modes_before_object_access() {
    let sb = Sandbox::new();
    let repository = sb.init("mode-filtered-diff");
    sb.write(
        &repository,
        "regular-to-link.txt",
        b"old ordinary unique content\n",
    );
    sb.write(
        &repository,
        "link-to-regular.txt",
        b"temporary worktree placeholder\n",
    );
    let link_old = sb.git_stdin(
        &repository,
        &["hash-object", "-w", "--stdin"],
        b"../old-target",
    );
    let link_new = sb.git_stdin(
        &repository,
        &["hash-object", "-w", "--stdin"],
        b"../new-target",
    );
    sb.git(&repository, &["add", "-A"]);
    for (mode, id, file) in [
        ("120000", link_old.as_str(), "link-to-regular.txt"),
        ("120000", link_old.as_str(), "link"),
        (
            "160000",
            "1111111111111111111111111111111111111111",
            "submodule",
        ),
    ] {
        sb.git(
            &repository,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("{mode},{id},{file}"),
            ],
        );
    }
    sb.git(&repository, &["commit", "-q", "-m", "old mode fixture"]);
    let old = sb.git(&repository, &["rev-parse", "HEAD"]);
    sb.write(
        &repository,
        "link-to-regular.txt",
        b"new entirely different payload\n",
    );
    sb.git(&repository, &["add", "link-to-regular.txt"]);
    // With core.symlinks=false, `add` can retain the fake symlink's index mode.
    // Set the intended regular-file transition explicitly on every platform.
    let regular = sb.git_stdin(
        &repository,
        &["hash-object", "-w", "--stdin"],
        b"new entirely different payload\n",
    );
    sb.git(
        &repository,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{regular},link-to-regular.txt"),
        ],
    );
    for (mode, id, file) in [
        ("120000", link_new.as_str(), "regular-to-link.txt"),
        ("120000", link_new.as_str(), "link"),
        (
            "160000",
            "2222222222222222222222222222222222222222",
            "submodule",
        ),
    ] {
        sb.git(
            &repository,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("{mode},{id},{file}"),
            ],
        );
    }
    sb.git(&repository, &["commit", "-q", "-m", "new mode fixture"]);
    let new = sb.git(&repository, &["rev-parse", "HEAD"]);
    assert!(
        sb.git(&repository, &["ls-tree", "HEAD", "link-to-regular.txt"])
            .starts_with("100644 blob ")
    );
    remove_loose_blob(&sb, &repository, &old, "link-to-regular.txt");
    remove_loose_blob(&sb, &repository, &new, "regular-to-link.txt");
    assert_eq!(
        open(&repository)
            .diff_scoped(
                &old,
                &new,
                None,
                &ExclusionPolicy::builtin(),
                &WalkOptions::default()
            )
            .unwrap(),
        [
            Change::Added(p("link-to-regular.txt")),
            Change::Deleted(p("regular-to-link.txt"))
        ]
    );
}

#[test]
fn force_push_is_detected_through_ancestry() {
    let sb = Sandbox::new();
    let repo_path = sb.init("force");
    sb.write(&repo_path, "a.txt", b"1\n");
    let c1 = sb.commit_all(&repo_path, "c1");
    sb.write(&repo_path, "a.txt", b"2\n");
    let c2 = sb.commit_all(&repo_path, "c2");
    let repo = open(&repo_path);
    let indexed = repo.resolve(&target("branch:main")).unwrap().commit;
    assert_eq!(indexed, c2);

    // Fast-forward: the old commit is an ancestor.
    sb.write(&repo_path, "a.txt", b"3\n");
    let c3 = sb.commit_all(&repo_path, "c3");
    assert!(repo.is_ancestor(&c2, &c3).unwrap());

    // Rewrite: reset to c1 and commit something else, as a force-push would.
    sb.git(&repo_path, &["reset", "-q", "--hard", &c1]);
    sb.write(&repo_path, "a.txt", b"rewritten\n");
    let rewritten = sb.commit_all(&repo_path, "c2'");
    let now = repo.resolve(&target("branch:main")).unwrap().commit;
    assert_eq!(now, rewritten);
    assert!(!repo.is_ancestor(&indexed, &now).unwrap());
    assert!(repo.is_ancestor(&c1, &now).unwrap());
    assert!(!repo.is_ancestor(&now, &c1).unwrap());
    assert!(repo.is_ancestor(&now, &now).unwrap());
    assert_eq!(repo.merge_base(&indexed, &now).unwrap(), Some(c1.clone()));
    assert_eq!(repo.merge_base(&c3, &c3).unwrap(), Some(c3.clone()));
    // The content diff is still exact across the rewrite.
    assert_eq!(
        repo.diff(&indexed, &now).unwrap(),
        [Change::Modified(p("a.txt"))]
    );

    // Unrelated history: no merge base, not an ancestor.
    sb.git(&repo_path, &["checkout", "-q", "--orphan", "island"]);
    sb.git(&repo_path, &["rm", "-rf", "-q", "."]);
    sb.write(&repo_path, "island.txt", b"alone\n");
    let island = sb.commit_all(&repo_path, "island");
    assert_eq!(repo.merge_base(&island, &now).unwrap(), None);
    assert!(!repo.is_ancestor(&c1, &island).unwrap());
}

#[test]
fn linked_worktrees_and_detached_heads() {
    let sb = Sandbox::new();
    let main = sb.init("main");
    sb.write(&main, "a.txt", b"1\n");
    let c1 = sb.commit_all(&main, "c1");
    sb.write(&main, "a.txt", b"22\n");
    let c2 = sb.commit_all(&main, "c2");
    let feature = sb.path().join("wt-feature");
    let detached = sb.path().join("wt-detached");
    let gone = sb.path().join("wt-gone");
    sb.git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            feature.to_str().unwrap(),
        ],
    );
    sb.git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            detached.to_str().unwrap(),
            &c1,
        ],
    );
    sb.git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            gone.to_str().unwrap(),
            &c1,
        ],
    );
    std::fs::remove_dir_all(&gone).unwrap();

    let linked = open(&feature);
    assert!(linked.is_linked_worktree());
    assert!(!open(&main).is_linked_worktree());
    assert_eq!(canon(linked.workdir().unwrap()), canon(&feature));
    assert_eq!(canon(linked.common_dir()), canon(&main.join(".git")));

    let head = linked.resolve(&TrackTarget::WorktreeHead).unwrap();
    assert_eq!(head.commit, c2);
    assert_eq!(head.reference.as_deref(), Some("refs/heads/feature"));
    assert_eq!(head.kind, ResolvedKind::WorktreeBranch);
    let head = open(&detached).resolve(&TrackTarget::WorktreeHead).unwrap();
    assert_eq!(head.commit, c1);
    assert_eq!(head.kind, ResolvedKind::WorktreeDetached);

    // A commit in the linked worktree moves its HEAD and the shared branch,
    // not the main worktree's HEAD.
    sb.write(&feature, "f.txt", b"feature\n");
    let c3 = sb.commit_all(&feature, "c3");
    assert_eq!(
        linked.resolve(&TrackTarget::WorktreeHead).unwrap().commit,
        c3
    );
    assert_eq!(
        open(&main)
            .resolve(&TrackTarget::WorktreeHead)
            .unwrap()
            .commit,
        c2
    );
    assert_eq!(
        open(&main)
            .resolve(&target("branch:feature"))
            .unwrap()
            .commit,
        c3
    );

    let list = open(&main).worktrees().unwrap();
    let summary: Vec<_> = list
        .iter()
        .map(|w| {
            (
                w.path.file_name().unwrap().to_str().unwrap().to_owned(),
                w.head_commit.clone(),
                w.branch.clone(),
                w.is_main,
                w.prunable,
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (
                "main".to_owned(),
                Some(c2.clone()),
                Some("main".to_owned()),
                true,
                false
            ),
            (
                "wt-detached".to_owned(),
                Some(c1.clone()),
                None,
                false,
                false
            ),
            (
                "wt-feature".to_owned(),
                Some(c3.clone()),
                Some("feature".to_owned()),
                false,
                false
            ),
            ("wt-gone".to_owned(), Some(c1.clone()), None, false, true),
        ]
    );
    assert_eq!(canon(&list[0].path), canon(&main));
    assert_eq!(canon(&list[2].path), canon(&feature));
    assert_eq!(list[0].path, std::fs::canonicalize(&main).unwrap());
    assert_eq!(list[2].path, std::fs::canonicalize(&feature).unwrap());
    // Same answer from any worktree of the repository.
    assert_eq!(linked.worktrees().unwrap(), list);
    assert_eq!(
        open(&main.join("..").join("main")).worktrees().unwrap(),
        list
    );
}

#[test]
fn worktrees_of_several_repositories_form_task_views() {
    let sb = Sandbox::new();
    let mut all = Vec::new();
    for module in ["api", "web"] {
        let repo = sb.init(&format!("ws/{module}"));
        sb.write(&repo, "a.txt", module.as_bytes());
        sb.commit_all(&repo, "c1");
        for (slug, branch) in [("pay", "feature/pay"), ("search", "feature/search")] {
            let path = sb
                .path()
                .join("ws")
                .join(".worktree")
                .join(slug)
                .join(module);
            sb.git(
                &repo,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    branch,
                    path.to_str().unwrap(),
                ],
            );
        }
        for info in open(&repo).worktrees().unwrap() {
            all.push((module.to_owned(), info));
        }
    }
    let input = || all.iter().map(|(repo, info)| (repo.as_str(), info));
    let views = group_task_views(input(), &TaskGrouping::default());
    let summary: Vec<(&str, Vec<&str>)> = views
        .iter()
        .map(|v| {
            (
                v.slug.as_str(),
                v.members.iter().map(|m| m.repo.as_str()).collect(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [("pay", vec!["api", "web"]), ("search", vec!["api", "web"])]
    );

    let explicit = TaskGrouping {
        path_pattern: Some(PathPattern::new("{root}/.worktree/{slug}/{module}").unwrap()),
        by_branch: false,
        include_main: false,
    };
    assert_eq!(
        group_task_views(input(), &explicit)
            .iter()
            .map(|v| v.slug.as_str())
            .collect::<Vec<_>>(),
        ["pay", "search"]
    );
}

#[test]
fn working_changes_compose_staged_unstaged_and_untracked() {
    let sb = Sandbox::new();
    let repo_path = sb.init("wc");
    sb.write(&repo_path, ".gitignore", b"*.log\nbuild/\n");
    for (name, body) in [
        ("mod.txt", "1\n"),
        ("del.txt", "d\n"),
        ("staged.txt", "s1\n"),
        ("both.txt", "b1\n"),
        ("cached.txt", "c\n"),
        ("same.txt", "same\n"),
    ] {
        sb.write(&repo_path, name, body.as_bytes());
    }
    sb.write(&repo_path, ".env", canary().as_bytes());
    sb.commit_all(&repo_path, "c1");

    sb.write(&repo_path, "mod.txt", b"1 changed\n");
    std::fs::remove_file(repo_path.join("del.txt")).unwrap();
    sb.write(&repo_path, "staged.txt", b"s2 staged\n");
    sb.git(&repo_path, &["add", "staged.txt"]);
    sb.write(&repo_path, "both.txt", b"b2 staged\n");
    sb.git(&repo_path, &["add", "both.txt"]);
    sb.write(&repo_path, "both.txt", b"b3 and unstaged\n");
    sb.git(&repo_path, &["rm", "-q", "--cached", "cached.txt"]);
    sb.write(&repo_path, "added.txt", b"new and staged\n");
    sb.git(&repo_path, &["add", "added.txt"]);
    sb.write(&repo_path, "temp.txt", b"staged then deleted\n");
    sb.git(&repo_path, &["add", "temp.txt"]);
    std::fs::remove_file(repo_path.join("temp.txt")).unwrap();
    sb.write(&repo_path, "new.txt", b"untracked\n");
    sb.write(&repo_path, "dir/nested.txt", b"untracked nested\n");
    sb.write(&repo_path, "x.log", b"ignored\n");
    sb.write(&repo_path, "build/out.txt", b"ignored dir\n");
    sb.write(
        &repo_path,
        ".env",
        format!("{} changed", canary()).as_bytes(),
    );
    sb.write(&repo_path, ".env.local", canary().as_bytes());

    let index = repo_path.join(".git").join("index");
    let index_before = std::fs::read(&index).unwrap();
    let repo = open(&repo_path);
    let changes = repo.working_changes(&ExclusionPolicy::builtin()).unwrap();
    assert_eq!(
        changes,
        [
            Change::Added(p("added.txt")),
            Change::Modified(p("both.txt")),
            Change::Modified(p("cached.txt")),
            Change::Deleted(p("del.txt")),
            Change::Added(p("dir/nested.txt")),
            Change::Modified(p("mod.txt")),
            Change::Added(p("new.txt")),
            Change::Modified(p("staged.txt")),
        ]
    );
    // Read-only: the index is not rewritten.
    assert_eq!(std::fs::read(&index).unwrap(), index_before);
    // The free function opens the worktree itself.
    assert_eq!(
        working_changes(&repo_path, &ExclusionPolicy::builtin()).unwrap(),
        changes
    );
    // User patterns drop paths too.
    let policy = ExclusionPolicy::with_patterns(&["dir".to_owned()]).unwrap();
    assert!(
        !repo
            .working_changes(&policy)
            .unwrap()
            .contains(&Change::Added(p("dir/nested.txt")))
    );
}

#[test]
fn working_changes_of_a_linked_worktree_use_its_own_head() {
    let sb = Sandbox::new();
    let main = sb.init("main");
    sb.write(&main, "a.txt", b"1\n");
    sb.commit_all(&main, "c1");
    let wt = sb.path().join("wt");
    sb.git(
        &main,
        &["worktree", "add", "-q", "-b", "topic", wt.to_str().unwrap()],
    );
    sb.write(&wt, "b.txt", b"only in the linked worktree\n");
    sb.commit_all(&wt, "c2 on topic");

    sb.write(&wt, "a.txt", b"edited in wt\n");
    sb.write(&wt, "c.txt", b"new in wt\n");
    assert_eq!(
        open(&wt)
            .working_changes(&ExclusionPolicy::builtin())
            .unwrap(),
        [Change::Modified(p("a.txt")), Change::Added(p("c.txt"))]
    );
    // The main worktree is clean and does not see the linked one's work.
    assert!(
        open(&main)
            .working_changes(&ExclusionPolicy::builtin())
            .unwrap()
            .is_empty()
    );
}
