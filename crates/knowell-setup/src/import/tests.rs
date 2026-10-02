use std::fs;
use std::path::Path;

use knowell_config::parse_workspace;
use pretty_assertions::assert_eq;

use super::*;

fn write(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

fn repo(root: &Path, rel: &str, branch: &str) {
    // An empty `rel` means the root itself; never build an absolute path.
    let git = if rel.is_empty() {
        ".git/HEAD".to_owned()
    } else {
        format!("{rel}/.git/HEAD")
    };
    write(root, &git, &format!("ref: refs/heads/{branch}\n"));
}

fn detect_default(root: &Path) -> ImportPlan {
    detect(root, &ImportOptions::default()).unwrap()
}

fn names(plan: &ImportPlan) -> Vec<&str> {
    plan.projects.iter().map(|p| p.name.as_str()).collect()
}

fn project<'a>(plan: &'a ImportPlan, name: &str) -> &'a PlannedProject {
    plan.projects
        .iter()
        .find(|p| p.name.as_str() == name)
        .unwrap()
}

fn track(p: &PlannedProject) -> Option<String> {
    p.track.as_ref().map(ToString::to_string)
}

fn named(name: &str) -> ImportOptions {
    ImportOptions {
        workspace_name: Some(name.to_owned()),
        ..ImportOptions::default()
    }
}

#[test]
fn submodules_with_and_without_branch() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    repo(r, "", "main");
    write(
        r,
        ".gitmodules",
        "[submodule \"libs/core\"]\n\tpath = libs/core\n\turl = https://user:pw@example.com/o/core.git\n\tbranch = develop\n\
         [submodule \"web\"]\n\tpath = web\n\turl = git@example.com:o/web.git\n\
         [submodule \"same\"]\n\tpath = same\n\tbranch = .\n\
         [submodule \"gone\"]\n\tpath = gone\n",
    );
    for d in ["libs/core", "web", "same"] {
        repo(r, d, "feature/x");
    }
    let plan = detect(r, &named("demo")).unwrap();
    assert_eq!(names(&plan), ["core", "web", "same", "gone"]);
    let core = project(&plan, "core");
    assert_eq!(core.path, "libs/core");
    assert_eq!(track(core).as_deref(), Some("branch:develop"));
    assert_eq!(
        core.remote.as_deref(),
        Some("https://example.com/o/core.git")
    );
    assert_eq!(core.source, ImportSource::Gitmodules);
    assert_eq!(track(project(&plan, "web")), None, "never guessed");
    assert_eq!(track(project(&plan, "same")), None);
    assert_eq!(plan.projects_needing_track().len(), 3);
    assert!(plan.warnings.iter().any(|w| w.contains("branch = .")));
    assert!(plan.warnings.iter().any(|w| w.contains("not checked out")));
    assert!(!format!("{plan:?}").contains("pw@"), "credentials removed");
}

#[test]
fn track_current_reads_head_only_when_asked() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    repo(r, "alpha", "release/2.x");
    repo(r, "beta", "main");
    write(
        r,
        "gamma/.git/HEAD",
        "0123456789012345678901234567890123456789\n",
    );
    write(
        r,
        "linked/.git",
        "gitdir: ../elsewhere/.git/modules/linked\n",
    );
    write(
        r,
        "elsewhere/.git/modules/linked/HEAD",
        "ref: refs/heads/topic\n",
    );

    let off = detect_default(r);
    assert!(off.projects.iter().all(|p| p.track.is_none()));

    let on = detect(
        r,
        &ImportOptions {
            track_current: true,
            ..ImportOptions::default()
        },
    )
    .unwrap();
    assert_eq!(
        track(project(&on, "alpha")).as_deref(),
        Some("branch:release/2.x")
    );
    assert_eq!(track(project(&on, "beta")).as_deref(), Some("branch:main"));
    assert_eq!(track(project(&on, "gamma")), None);
    assert_eq!(
        track(project(&on, "linked")).as_deref(),
        Some("branch:topic")
    );
    assert!(on.warnings.iter().any(|w| w.contains("detached")));
}

#[test]
fn gitmodules_branch_wins_over_current_branch() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    write(
        r,
        ".gitmodules",
        "[submodule \"a\"]\n path = a\n branch = stable\n",
    );
    repo(r, "a", "other");
    let plan = detect(
        r,
        &ImportOptions {
            track_current: true,
            ..ImportOptions::default()
        },
    )
    .unwrap();
    assert_eq!(track(project(&plan, "a")).as_deref(), Some("branch:stable"));
    assert_eq!(plan.projects.len(), 1, "folder scan does not duplicate it");
}

#[test]
fn go_work_modules_become_root_projects() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    repo(r, "", "trunk");
    write(
        r,
        "go.work",
        "go 1.22\nuse (\n\t./svc/api\n\t./svc/worker\n\t../outside\n\t./missing\n\t./own\n)\n",
    );
    for d in ["svc/api", "svc/worker"] {
        fs::create_dir_all(r.join(d)).unwrap();
    }
    repo(r, "own", "dev");
    let plan = detect(
        r,
        &ImportOptions {
            track_current: true,
            scan_folders: false,
            ..ImportOptions::default()
        },
    )
    .unwrap();
    assert_eq!(names(&plan), ["api", "worker", "own"]);
    let api = project(&plan, "api");
    assert_eq!(
        (api.path.as_str(), api.root.as_ref().map(|r| r.as_str())),
        (".", Some("svc/api"))
    );
    assert_eq!(track(api).as_deref(), Some("branch:trunk"));
    let own = project(&plan, "own");
    assert_eq!((own.path.as_str(), own.root.is_none()), ("own", true));
    assert_eq!(track(own).as_deref(), Some("branch:dev"));
    assert!(plan.warnings.iter().any(|w| w.contains("outside")));
    assert!(plan.warnings.iter().any(|w| w.contains("does not exist")));
}

#[test]
fn pnpm_and_package_json_workspaces() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    repo(r, "", "main");
    write(
        r,
        "pnpm-workspace.yaml",
        "packages:\n  - 'apps/*'\n  - '!apps/legacy'\n  - 'libs/*'\n",
    );
    for d in [
        "apps/web",
        "apps/legacy",
        "libs/web",
        "libs/ui",
        "apps/no-package",
    ] {
        if d != "apps/no-package" {
            write(r, &format!("{d}/package.json"), "{}");
        } else {
            fs::create_dir_all(r.join(d)).unwrap();
        }
    }
    write(
        r,
        "package.json",
        "{\"workspaces\": {\"packages\": [\"tools/*\"]}}",
    );
    write(r, "tools/lint/package.json", "{}");
    let plan = detect(
        r,
        &ImportOptions {
            scan_folders: false,
            ..ImportOptions::default()
        },
    )
    .unwrap();
    assert_eq!(names(&plan), ["web", "ui", "web-2", "lint"]);
    assert_eq!(project(&plan, "web-2").path, ".");
    assert_eq!(
        project(&plan, "web-2").root.as_ref().unwrap().as_str(),
        "libs/web"
    );
    assert_eq!(project(&plan, "lint").source, ImportSource::PackageJson);
    let collision: Vec<_> = plan
        .renames
        .iter()
        .filter(|r| r.reason == RenameReason::Collision)
        .collect();
    assert_eq!(collision.len(), 1);
    assert_eq!(collision[0].original, "web");
    assert_eq!(collision[0].assigned.as_str(), "web-2");
}

#[test]
fn cargo_members_with_globs_and_excludes() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    repo(r, "", "main");
    write(
        r,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/*\", \"tools/cli\", \"../escape\"]\nexclude = [\"crates/old\"]\n",
    );
    for d in ["crates/a", "crates/b", "crates/old", "tools/cli"] {
        write(r, &format!("{d}/Cargo.toml"), "[package]\nname=\"x\"\n");
    }
    fs::create_dir_all(r.join("crates/not-a-crate")).unwrap();
    let plan = detect(
        r,
        &ImportOptions {
            scan_folders: false,
            ..ImportOptions::default()
        },
    )
    .unwrap();
    assert_eq!(names(&plan), ["a", "b", "cli"]);
    assert!(
        plan.projects
            .iter()
            .all(|p| p.path == "." && p.root.is_some())
    );
    assert!(plan.warnings.iter().any(|w| w.contains("../escape")));
}

#[test]
fn folder_scan_reports_worktrees_not_projects() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    repo(r, "service-a", "main");
    repo(r, "My Service", "main");
    repo(r, ".worktree/feature-x/service-a", "feature-x");
    repo(r, ".worktree/feature-x/service-b", "feature-x");
    repo(r, ".hidden", "main");
    repo(r, "node_modules/dep", "main");
    fs::create_dir_all(r.join("docs")).unwrap();
    write(
        r,
        "wt-linked/.git",
        "gitdir: /somewhere/service-a/.git/worktrees/wt-linked\n",
    );

    let plan = detect_default(r);
    assert_eq!(names(&plan), ["my-service", "service-a"]);
    assert!(
        plan.renames
            .iter()
            .any(|x| x.reason == RenameReason::Slugified && x.assigned.as_str() == "my-service")
    );
    let wts: Vec<_> = plan
        .worktrees
        .iter()
        .map(|w| (w.path.as_str(), w.kind))
        .collect();
    assert_eq!(
        wts,
        [
            (".worktree/feature-x/service-a", WorktreeKind::Pattern),
            (".worktree/feature-x/service-b", WorktreeKind::Pattern),
            ("wt-linked", WorktreeKind::LinkedWorktree),
        ]
    );
}

#[test]
fn custom_worktree_patterns_and_unsafe_ones() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    repo(r, "wt/one", "x");
    repo(r, "main-repo", "main");
    let plan = detect(
        r,
        &ImportOptions {
            worktree_patterns: vec!["wt/*".into(), "../escape/*".into()],
            ..ImportOptions::default()
        },
    )
    .unwrap();
    assert_eq!(plan.worktrees.len(), 1);
    assert_eq!(plan.worktrees[0].path, "wt/one");
    assert_eq!(names(&plan), ["main-repo"]);
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("unsafe worktree pattern"))
    );
}

#[test]
fn hostile_gitmodules_entries_are_skipped() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    write(
        r,
        ".gitmodules",
        "[submodule \"a\"]\npath = ../../etc\n[submodule \"b\"]\npath = /abs\n[submodule \"c\"]\n[submodule \"d\"]\npath = ok\n",
    );
    let plan = detect(r, &named("x")).unwrap();
    assert_eq!(names(&plan), ["d"]);
    assert_eq!(plan.projects[0].path, "ok");
    assert_eq!(
        plan.warnings
            .iter()
            .filter(|w| w.contains("unusable path"))
            .count(),
        2
    );
}

#[test]
fn root_repository_fallback_and_empty_directory() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    let empty = detect_default(r);
    assert!(empty.projects.is_empty());
    repo(r, "", "main");
    let plan = detect(r, &named("solo")).unwrap();
    assert_eq!(plan.projects.len(), 1);
    assert_eq!(plan.projects[0].path, ".");
    assert_eq!(plan.projects[0].source, ImportSource::RootRepository);
}

#[test]
fn bad_inputs_are_errors() {
    let t = tempfile::tempdir().unwrap();
    assert!(detect(&t.path().join("missing"), &ImportOptions::default()).is_err());
    assert!(detect(t.path(), &named("Bad Name")).is_err());
    let derived = detect_default(t.path());
    assert!(!derived.workspace_name.as_str().is_empty());
}

#[test]
fn rendered_toml_round_trips_through_knowell_config() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    repo(r, "", "main");
    write(r, "pnpm-workspace.yaml", "packages:\n  - 'apps/*'\n");
    write(r, "apps/web/package.json", "{}");
    write(r, "apps/api/package.json", "{}");
    write(
        r,
        ".gitmodules",
        "[submodule \"lib\"]\n path = lib\n url = https://example.com/o/lib.git\n branch = main\n",
    );
    repo(r, "lib", "main");
    let plan = detect(
        r,
        &ImportOptions {
            workspace_name: Some("demo".into()),
            track_current: true,
            ..ImportOptions::default()
        },
    )
    .unwrap();
    let text = render_toml(&plan);
    let cfg = parse_workspace(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
    assert_eq!(cfg.workspace.name.as_str(), "demo");
    assert_eq!(cfg.project.len(), 3);
    assert_eq!(
        cfg.workspace
            .track
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("branch:main"),
        "shared track is hoisted:\n{text}"
    );
    assert!(cfg.project.iter().all(|p| p.track.is_none()));
    let resolved = cfg.resolve(r).unwrap_or_else(|e| panic!("{e:?}\n{text}"));
    assert_eq!(resolved.projects.len(), 3);
    let api = resolved
        .projects
        .iter()
        .find(|p| p.name.as_str() == "api")
        .unwrap();
    assert_eq!(api.root.as_ref().unwrap().as_str(), "apps/api");
}

#[test]
fn different_tracks_stay_per_project() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    repo(r, "a", "main");
    repo(r, "b", "develop");
    let plan = detect(
        r,
        &ImportOptions {
            track_current: true,
            ..ImportOptions::default()
        },
    )
    .unwrap();
    let text = render_toml(&plan);
    let cfg = parse_workspace(&text).unwrap();
    assert!(cfg.workspace.track.is_none());
    assert_eq!(cfg.project.len(), 2);
    cfg.resolve(r).unwrap();
}

#[test]
fn missing_tracks_render_todo_and_fail_resolution_clearly() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    repo(r, "a", "main");
    repo(r, "b", "main");
    let plan = detect_default(r);
    let text = render_toml(&plan);
    assert_eq!(text.matches("# TODO: choose the ref").count(), 2, "{text}");
    let cfg = parse_workspace(&text).unwrap();
    let err = cfg.resolve(r).unwrap_err();
    assert!(err.to_string().contains("no track target"), "{err}");
    assert!(!text.lines().any(|l| l.starts_with("track =")));
}

#[test]
fn comments_cannot_break_out_of_the_rendered_file() {
    let t = tempfile::tempdir().unwrap();
    let mut plan = detect_default(t.path());
    plan.warnings
        .push("line one\n[[project]]\nname = \"evil\"".to_owned());
    let text = render_toml(&plan);
    let cfg = parse_workspace(&text).unwrap();
    assert!(cfg.project.is_empty());
}

#[test]
fn empty_plan_renders_valid_file() {
    let t = tempfile::tempdir().unwrap();
    let plan = detect(t.path(), &named("empty")).unwrap();
    let cfg = parse_workspace(&render_toml(&plan)).unwrap();
    assert_eq!(cfg.workspace.name.as_str(), "empty");
}
