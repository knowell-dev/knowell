//! Integration tests for `knowell_source::watch` against real repositories.
//!
//! File-system notifications are asynchronous, so every expectation waits
//! (generously) until the expected events have arrived instead of sleeping
//! for a fixed time; assertions about what must *not* be reported only look
//! at events that arrived before or with an expected later event.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use knowell_core::RepoPath;
use knowell_secrets::ExclusionPolicy;
use knowell_source::watch::{SourceEvent, WatchOptions, Watcher};
use support::{Sandbox, open};

/// Generous upper bound for one expectation; normally met in well under a
/// second.
const PATIENCE: Duration = Duration::from_secs(20);

fn options() -> WatchOptions {
    WatchOptions {
        debounce: Duration::from_millis(100),
        ..WatchOptions::default()
    }
}

/// Collects events until `done` holds for everything collected so far.
fn collect_until(
    rx: &Receiver<SourceEvent>,
    what: &str,
    done: impl Fn(&[SourceEvent]) -> bool,
) -> Vec<SourceEvent> {
    let deadline = Instant::now() + PATIENCE;
    let mut seen = Vec::new();
    while !done(&seen) {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(event) => seen.push(event),
            Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for {what}; got {seen:?}"),
            Err(RecvTimeoutError::Disconnected) => {
                panic!("channel closed waiting for {what}; got {seen:?}")
            }
        }
    }
    seen
}

fn changed_paths(events: &[SourceEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            SourceEvent::FilesChanged(paths) => Some(paths),
            _ => None,
        })
        .flatten()
        .map(RepoPath::to_string)
        .collect()
}

fn has_path(events: &[SourceEvent], path: &str) -> bool {
    changed_paths(events).iter().any(|p| p == path)
}

#[test]
fn reports_saved_files_and_commits_but_not_noise() {
    let sb = Sandbox::new();
    let repo_path = sb.init("watched");
    sb.write(&repo_path, ".gitignore", b"target/\n*.tmp\n");
    sb.write(&repo_path, "src/lib.rs", b"pub fn a() {}\n");
    sb.commit_all(&repo_path, "c1");
    let repo = open(&repo_path);
    let (watcher, rx) = Watcher::start(&repo, ExclusionPolicy::builtin(), options()).unwrap();

    // Noise first, then the file we wait for: the noise events are
    // delivered no later than the batch that carries the real change.
    sb.write(&repo_path, ".env", b"SECRET=fake\n");
    sb.write(&repo_path, "target/debug/out.o", b"\0\0");
    sb.write(&repo_path, "scratch.tmp", b"tmp\n");
    sb.write(&repo_path, "src/lib.rs", b"pub fn a() {}\npub fn b() {}\n");
    sb.write(&repo_path, "src/new.rs", b"pub fn c() {}\n");
    let events = collect_until(&rx, "the saved files", |seen| {
        has_path(seen, "src/lib.rs") && has_path(seen, "src/new.rs")
    });
    for path in changed_paths(&events) {
        assert!(
            !path.starts_with(".git/") && path != ".git",
            "git internals reported: {path}"
        );
        assert!(!path.starts_with(".env"), "excluded file reported: {path}");
        assert!(!path.starts_with("target"), "ignored file reported: {path}");
        assert!(!path.ends_with(".tmp"), "ignored file reported: {path}");
    }
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SourceEvent::HeadMoved | SourceEvent::RefsChanged)),
        "saving files moves no ref: {events:?}"
    );

    // A commit moves the checked-out branch, hence HEAD.
    sb.git(&repo_path, &["add", "src"]);
    sb.git(&repo_path, &["commit", "-q", "-m", "c2"]);
    collect_until(&rx, "HEAD and ref signals after a commit", |seen| {
        seen.contains(&SourceEvent::HeadMoved) && seen.contains(&SourceEvent::RefsChanged)
    });

    // Creating a branch elsewhere is a ref change only.
    sb.git(&repo_path, &["branch", "side"]);
    collect_until(&rx, "a ref signal after creating a branch", |seen| {
        seen.contains(&SourceEvent::RefsChanged)
    });

    // Switching branches rewrites HEAD.
    sb.git(&repo_path, &["checkout", "-q", "side"]);
    collect_until(&rx, "HEAD signal after checkout", |seen| {
        seen.contains(&SourceEvent::HeadMoved)
    });

    watcher.stop();
    // After stop the channel is closed (pending events may still drain).
    let deadline = Instant::now() + PATIENCE;
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(_) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => panic!("channel still open after stop"),
        }
    }
}

#[test]
fn watches_the_real_git_dirs_of_a_linked_worktree() {
    let sb = Sandbox::new();
    let main = sb.init("main");
    sb.write(&main, "a.txt", b"1\n");
    sb.commit_all(&main, "c1");
    let wt = sb.path().join("wt");
    sb.git(
        &main,
        &["worktree", "add", "-q", "-b", "topic", wt.to_str().unwrap()],
    );
    let repo = open(&wt);
    assert!(repo.is_linked_worktree());
    let (watcher, rx) = Watcher::start(&repo, ExclusionPolicy::builtin(), options()).unwrap();

    sb.write(&wt, "b.txt", b"saved in the linked worktree\n");
    let events = collect_until(&rx, "a save in the linked worktree", |seen| {
        has_path(seen, "b.txt")
    });
    assert!(!changed_paths(&events).iter().any(|p| p == ".git"));

    // A commit here updates `refs/heads/topic` in the common directory,
    // outside the worktree: it must still be seen, as a HEAD move.
    sb.git(&wt, &["add", "b.txt"]);
    sb.git(&wt, &["commit", "-q", "-m", "c2"]);
    collect_until(&rx, "HEAD signal from the common git dir", |seen| {
        seen.contains(&SourceEvent::HeadMoved) && seen.contains(&SourceEvent::RefsChanged)
    });

    // Detaching rewrites the private HEAD of this worktree.
    sb.git(&wt, &["checkout", "-q", "--detach"]);
    collect_until(&rx, "HEAD signal from the private git dir", |seen| {
        seen.contains(&SourceEvent::HeadMoved)
    });
    drop(watcher);
}

#[test]
fn stops_promptly_and_closes_the_channel() {
    let sb = Sandbox::new();
    let repo_path = sb.init("quiet");
    sb.write(&repo_path, "a.txt", b"a\n");
    sb.commit_all(&repo_path, "c1");
    let repo = open(&repo_path);
    let (watcher, rx) = Watcher::start(&repo, ExclusionPolicy::builtin(), options()).unwrap();
    assert!(watcher.root().is_absolute());
    let started = Instant::now();
    watcher.stop();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(matches!(
        rx.recv_timeout(PATIENCE),
        Err(RecvTimeoutError::Disconnected)
    ));
}

#[test]
fn bare_repositories_cannot_be_watched() {
    let sb = Sandbox::new();
    let bare = sb.path().join("bare.git");
    sb.git(sb.path(), &["init", "-q", "--bare", bare.to_str().unwrap()]);
    let repo = open(&bare);
    assert!(Watcher::start(&repo, ExclusionPolicy::builtin(), options()).is_err());
}
