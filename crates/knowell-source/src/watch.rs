//! Debounced change signals for one git worktree.
//!
//! [`Watcher::start`] watches a worktree recursively (via `notify`) plus the
//! parts of its git directory that matter — `HEAD`, `refs/**`,
//! `packed-refs` and `reftable/` — and delivers [`SourceEvent`]s over a
//! `std::sync::mpsc` channel; no async runtime is involved. For a linked
//! worktree the real git directories (the private one holding `HEAD` and
//! the common one holding refs) are resolved and watched.
//!
//! Filtered out before anything is sent: object-database and index churn
//! (`objects/`, `index`, `*.lock`, logs), paths excluded by the
//! [`ExclusionPolicy`] (including `.git` / `.knowell` and nested
//! repositories' internals), and — unless disabled — files ignored by git
//! that are not tracked. Events are coalesced: a batch is sent once no new
//! event has arrived for [`WatchOptions::debounce`], or at the latest after
//! [`WatchOptions::max_delay`] of continuous activity.
//!
//! # Contract for consumers
//!
//! - [`SourceEvent::FilesChanged`] lists worktree paths below which
//!   something was created, written, renamed or deleted. A listed path may
//!   be a directory (a directory moved in or out reports only its own
//!   path) and may no longer exist (deleted or moved away): consumers
//!   recheck each path, and a whole subtree for directories.
//! - [`SourceEvent::HeadMoved`] means the commit `HEAD` resolves to may
//!   have changed (checkout, commit, reset, merge, pull). It is a hint:
//!   re-resolve and compare. It can be spurious, for example after
//!   `git pack-refs`.
//! - [`SourceEvent::RefsChanged`] means some ref (branch, tag,
//!   remote-tracking branch) was written or deleted: re-resolve tracked
//!   targets.
//! - [`SourceEvent::Rescan`] means the operating system reported lost
//!   events: reconcile fully (for example with
//!   [`crate::git::GitRepo::working_changes`] and a ref re-resolve).
//!
//! # Platform limits
//!
//! Some losses are never reported, so periodic reconciliation remains
//! necessary:
//!
//! - Windows (`ReadDirectoryChangesW`): when its 16 KiB change buffer
//!   overflows during a burst (large checkout, `npm install`), the current
//!   `notify` backend drops the burst without a rescan flag, and on an
//!   overflow error it may stop watching that directory altogether (it only
//!   logs). Nothing reaches this crate in either case.
//! - Linux (inotify): a recursive watch adds one kernel watch per directory,
//!   including ignored ones such as `node_modules/` and `.git/objects/`;
//!   large trees can hit `fs.inotify.max_user_watches`, which fails
//!   [`Watcher::start`] or loses directories created later. Queue overflow
//!   is reported as [`SourceEvent::Rescan`].
//! - Network and virtual file systems may deliver no events at all.
//! - Editors that save through a temporary file and rename produce a
//!   rename pair; the final path is reported, the temporary one usually is
//!   too (and no longer exists).

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use knowell_core::RepoPath;
use knowell_secrets::ExclusionPolicy;
use notify::event::{AccessKind, AccessMode, EventKind, ModifyKind};
use notify::{RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::git::{GitError, GitRepo};

/// How long the worker sleeps at most between checks of the stop flag.
const POLL: Duration = Duration::from_millis(50);

/// A change signal for one worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceEvent {
    /// Worktree paths that changed, sorted and de-duplicated (see the
    /// module docs for the exact contract).
    FilesChanged(Vec<RepoPath>),
    /// The worktree's `HEAD` may now resolve to another commit.
    HeadMoved,
    /// One or more refs were updated, created or deleted.
    RefsChanged,
    /// Events were lost; the consumer must reconcile fully.
    Rescan,
}

/// Options for [`Watcher::start`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchOptions {
    /// Quiet period that ends a batch. Default 300 ms.
    pub debounce: Duration,
    /// Upper bound on how long a batch is held back during continuous
    /// activity. Default 3 s.
    pub max_delay: Duration,
    /// Drop changes to files git ignores (unless they are tracked), so
    /// build output such as `target/` does not cause churn. Default `true`.
    pub respect_gitignore: bool,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(300),
            max_delay: Duration::from_secs(3),
            respect_gitignore: true,
        }
    }
}

/// Errors starting a [`Watcher`].
#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    /// The repository could not be inspected.
    #[error(transparent)]
    Git(#[from] GitError),
    /// A directory to watch could not be resolved.
    #[error("cannot watch `{}` ({error_kind})", path.display())]
    Path {
        /// The directory.
        path: PathBuf,
        /// `std::io::ErrorKind` name.
        error_kind: String,
    },
    /// The platform file watcher failed.
    #[error("file watching failed: {message}")]
    Notify {
        /// Description from the platform watcher.
        message: String,
    },
    /// The worker thread could not be started.
    #[error("watcher thread could not be started ({error_kind})")]
    Thread {
        /// `std::io::ErrorKind` name.
        error_kind: String,
    },
}

/// The directories a worktree's signals come from, canonicalised so that
/// event paths (which the platform reports below the watched paths) can be
/// matched by prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Layout {
    root: PathBuf,
    /// Holds this worktree's `HEAD` (private directory for linked ones).
    git_dir: PathBuf,
    /// Holds the shared refs and `packed-refs`.
    common_dir: PathBuf,
}

/// What one event path means.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Signal {
    File(RepoPath),
    Head,
    /// A loose ref, as a `/`-separated name such as `refs/heads/main`.
    Ref(String),
    /// `packed-refs` or the reftable store: any ref may have changed.
    RefStore,
}

/// `rel` as a `/`-joined string; `None` for non-UTF-8 components.
fn slash_path(rel: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?),
            _ => return None,
        }
    }
    Some(parts.join("/"))
}

/// Classifies a path inside a git directory. `private` directories hold
/// `HEAD`; `shared` ones hold `packed-refs` / `reftable`. Lock files are
/// ignored: git renames them into place, and the final name is reported.
fn git_signal(rel: &str, private: bool, shared: bool) -> Option<Signal> {
    if rel.ends_with(".lock") {
        return None;
    }
    if private && rel == "HEAD" {
        return Some(Signal::Head);
    }
    if rel.starts_with("refs/") {
        return Some(Signal::Ref(rel.to_owned()));
    }
    if shared && (rel == "packed-refs" || rel == "reftable" || rel.starts_with("reftable/")) {
        return Some(Signal::RefStore);
    }
    None
}

fn classify(layout: &Layout, path: &Path) -> Option<Signal> {
    let linked = layout.git_dir != layout.common_dir;
    // The private directory of a linked worktree lies inside the common
    // one, so it is checked first.
    if let Ok(rel) = path.strip_prefix(&layout.git_dir) {
        return git_signal(&slash_path(rel)?, true, !linked);
    }
    if let Ok(rel) = path.strip_prefix(&layout.common_dir) {
        return git_signal(&slash_path(rel)?, false, true);
    }
    let rel = path.strip_prefix(&layout.root).ok()?;
    RepoPath::from_relative(rel).ok().map(Signal::File)
}

/// Whether an event kind can mean a change. Opens, reads and non-writing
/// closes cannot.
fn is_change(kind: &EventKind) -> bool {
    match kind {
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        EventKind::Access(_) => false,
        _ => true,
    }
}

/// Windows reports a directory as modified whenever an entry inside it
/// changes; the entry itself is reported too, so the directory is noise.
fn is_directory_noise(kind: &EventKind, path: &Path) -> bool {
    matches!(
        kind,
        EventKind::Modify(ModifyKind::Any | ModifyKind::Data(_) | ModifyKind::Metadata(_))
    ) && path.is_dir()
}

/// `root` joined with a [`RepoPath`] component by component, so the result
/// is valid even for verbatim (`\\?\`) Windows paths.
fn join(root: &Path, path: &RepoPath) -> PathBuf {
    let mut full = root.to_path_buf();
    for part in path.components() {
        full.push(part);
    }
    full
}

/// The ref `HEAD` points to (`refs/heads/main`), `None` when detached.
fn head_referent(git_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let name = text.strip_prefix("ref:")?.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

#[derive(Debug, Default)]
struct Pending {
    files: BTreeSet<RepoPath>,
    head: bool,
    refs: BTreeSet<String>,
    ref_store: bool,
    rescan: bool,
    first: Option<Instant>,
    last: Option<Instant>,
}

impl Pending {
    fn touch(&mut self, now: Instant) {
        self.first.get_or_insert(now);
        self.last = Some(now);
    }

    /// Whether the batch should be sent at `now`.
    fn is_due(&self, options: &WatchOptions, now: Instant) -> bool {
        match (self.first, self.last) {
            (Some(first), Some(last)) => {
                now.saturating_duration_since(last) >= options.debounce
                    || now.saturating_duration_since(first) >= options.max_delay
            }
            _ => false,
        }
    }

    /// How long to wait for the next event before re-checking.
    fn wait(&self, options: &WatchOptions, now: Instant) -> Duration {
        let due = match (self.first, self.last) {
            (Some(first), Some(last)) => {
                let quiet = last.checked_add(options.debounce);
                let cap = first.checked_add(options.max_delay);
                match (quiet, cap) {
                    (Some(q), Some(c)) => Some(q.min(c)),
                    (q, c) => q.or(c),
                }
            }
            _ => None,
        };
        due.map_or(POLL, |due| due.saturating_duration_since(now).min(POLL))
    }
}

struct Worker {
    layout: Layout,
    repo: GitRepo,
    policy: ExclusionPolicy,
    options: WatchOptions,
    raw: Receiver<notify::Result<notify::Event>>,
    out: Sender<SourceEvent>,
    stop: Arc<AtomicBool>,
}

impl Worker {
    fn run(self) {
        let mut pending = Pending::default();
        loop {
            if self.stop.load(Ordering::Acquire) {
                self.flush(&mut pending);
                return;
            }
            let wait = pending.wait(&self.options, Instant::now());
            match self.raw.recv_timeout(wait) {
                Ok(Ok(event)) => self.absorb(&event, &mut pending),
                Ok(Err(_)) => {
                    pending.rescan = true;
                    pending.touch(Instant::now());
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    self.flush(&mut pending);
                    return;
                }
            }
            if pending.is_due(&self.options, Instant::now()) && !self.flush(&mut pending) {
                return;
            }
        }
    }

    fn absorb(&self, event: &notify::Event, pending: &mut Pending) {
        let now = Instant::now();
        if event.need_rescan() {
            pending.rescan = true;
            pending.touch(now);
        }
        if !is_change(&event.kind) {
            return;
        }
        for path in &event.paths {
            match classify(&self.layout, path) {
                Some(Signal::File(rel)) => {
                    if self.policy.check(&rel).is_some() || is_directory_noise(&event.kind, path) {
                        continue;
                    }
                    pending.files.insert(rel);
                }
                Some(Signal::Head) => pending.head = true,
                Some(Signal::Ref(name)) => {
                    pending.refs.insert(name);
                }
                Some(Signal::RefStore) => pending.ref_store = true,
                None => continue,
            }
            pending.touch(now);
        }
    }

    /// Drops untracked paths that git ignores. On any failure to evaluate
    /// ignore rules the paths are kept: a spurious signal is cheaper than
    /// a missed one.
    fn drop_ignored(&self, files: BTreeSet<RepoPath>) -> Vec<RepoPath> {
        if !self.options.respect_gitignore || files.is_empty() {
            return files.into_iter().collect();
        }
        let repo = self.repo.local();
        let Ok(index) = repo.index_or_empty() else {
            return files.into_iter().collect();
        };
        let Ok(mut excludes) = repo.excludes(
            &index,
            None,
            gix::worktree::stack::state::ignore::Source::WorktreeThenIdMappingIfNotSkipped,
        ) else {
            return files.into_iter().collect();
        };
        files
            .into_iter()
            .filter(|path| {
                if index.entry_by_path(path.as_str().into()).is_some() {
                    return true;
                }
                let is_dir = join(&self.layout.root, path).is_dir();
                let mode = is_dir.then_some(gix::index::entry::Mode::DIR);
                excludes
                    .at_entry(path.as_str(), mode)
                    .map_or(true, |platform| !platform.is_excluded())
            })
            .collect()
    }

    /// Sends the pending batch. Returns `false` once the receiver is gone.
    fn flush(&self, pending: &mut Pending) -> bool {
        if pending.first.is_none() {
            return true;
        }
        let batch = std::mem::take(pending);
        let mut events = Vec::with_capacity(4);
        if batch.rescan {
            events.push(SourceEvent::Rescan);
        }
        let files = self.drop_ignored(batch.files);
        if !files.is_empty() {
            events.push(SourceEvent::FilesChanged(files));
        }
        let referent = head_referent(&self.layout.git_dir);
        let head_moved = batch.head
            || referent
                .as_ref()
                .is_some_and(|name| batch.refs.contains(name) || batch.ref_store);
        if head_moved {
            events.push(SourceEvent::HeadMoved);
        }
        if !batch.refs.is_empty() || batch.ref_store {
            events.push(SourceEvent::RefsChanged);
        }
        events.into_iter().all(|event| self.out.send(event).is_ok())
    }
}

/// A running watcher for one worktree. Stops on [`Watcher::stop`] or drop;
/// the event channel is closed once the worker has exited.
pub struct Watcher {
    root: PathBuf,
    notify: Option<RecommendedWatcher>,
    worker: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl std::fmt::Debug for Watcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watcher")
            .field("root", &self.root)
            .field("running", &self.worker.is_some())
            .finish()
    }
}

fn canonical(path: &Path) -> Result<PathBuf, WatchError> {
    std::fs::canonicalize(path).map_err(|e| WatchError::Path {
        path: path.to_path_buf(),
        error_kind: format!("{:?}", e.kind()),
    })
}

fn notify_error(error: &notify::Error) -> WatchError {
    WatchError::Notify {
        message: knowell_secrets::scan::redact(&error.to_string()).text,
    }
}

impl Watcher {
    /// Starts watching the worktree of `repo` and returns the watcher and
    /// the receiving end of its event channel.
    ///
    /// The watch is registered before this returns, so changes made after
    /// it are reported.
    ///
    /// # Errors
    /// [`GitError::NoWorktree`] (as [`WatchError::Git`]) for a bare
    /// repository; [`WatchError::Path`] / [`WatchError::Notify`] when a
    /// directory cannot be watched; [`WatchError::Thread`].
    pub fn start(
        repo: &GitRepo,
        policy: ExclusionPolicy,
        options: WatchOptions,
    ) -> Result<(Self, Receiver<SourceEvent>), WatchError> {
        let root = repo.workdir().ok_or_else(|| GitError::NoWorktree {
            path: repo.git_dir().to_path_buf(),
        })?;
        let layout = Layout {
            root: canonical(root)?,
            git_dir: canonical(repo.git_dir())?,
            common_dir: canonical(repo.common_dir())?,
        };

        let (raw_tx, raw_rx) = mpsc::channel();
        let mut notify = notify::recommended_watcher(raw_tx).map_err(|e| notify_error(&e))?;
        let mut watch = |path: &Path, mode: RecursiveMode| {
            notify.watch(path, mode).map_err(|e| notify_error(&e))
        };
        watch(&layout.root, RecursiveMode::Recursive)?;
        // Git directories outside the worktree (linked worktrees,
        // `--separate-git-dir`) need their own, narrower watches: `HEAD`
        // lives in the private directory, refs in the common one. The
        // object database is deliberately not watched.
        if !layout.git_dir.starts_with(&layout.root) {
            watch(&layout.git_dir, RecursiveMode::NonRecursive)?;
        }
        if !layout.common_dir.starts_with(&layout.root) {
            watch(&layout.common_dir, RecursiveMode::NonRecursive)?;
            for sub in ["refs", "reftable"] {
                let dir = layout.common_dir.join(sub);
                if dir.is_dir() {
                    watch(&dir, RecursiveMode::Recursive)?;
                }
            }
        }

        let (out_tx, out_rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker = Worker {
            layout: layout.clone(),
            repo: repo.clone(),
            policy,
            options,
            raw: raw_rx,
            out: out_tx,
            stop: Arc::clone(&stop),
        };
        let handle = std::thread::Builder::new()
            .name("knowell-watch".to_owned())
            .spawn(move || worker.run())
            .map_err(|e| WatchError::Thread {
                error_kind: format!("{:?}", e.kind()),
            })?;
        Ok((
            Self {
                root: layout.root,
                notify: Some(notify),
                worker: Some(handle),
                stop,
            },
            out_rx,
        ))
    }

    /// The canonical worktree root being watched.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Stops watching, delivers the batch collected so far, and waits for
    /// the worker thread to exit (at most a few tens of milliseconds plus
    /// the time to send that batch). The channel is closed afterwards.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        // Dropping the platform watcher first means no further raw events.
        drop(self.notify.take());
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            // The worker cannot panic (no panicking code paths); a join
            // error would only repeat that, so there is nothing to report.
            let _ = worker.join();
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(linked: bool) -> Layout {
        let root: PathBuf = ["", "w", "repo"].iter().collect();
        if linked {
            let common: PathBuf = ["", "w", "main", ".git"].iter().collect();
            Layout {
                root,
                git_dir: common.join("worktrees").join("feature"),
                common_dir: common,
            }
        } else {
            let git = root.join(".git");
            Layout {
                root,
                git_dir: git.clone(),
                common_dir: git,
            }
        }
    }

    fn at(base: &Path, rel: &str) -> PathBuf {
        let mut p = base.to_path_buf();
        for part in rel.split('/') {
            p.push(part);
        }
        p
    }

    #[test]
    fn classifies_main_worktree_paths() {
        let l = layout(false);
        let git = l.git_dir.clone();
        assert_eq!(
            classify(&l, &at(&l.root, "src/main.rs")),
            Some(Signal::File(RepoPath::new("src/main.rs").unwrap()))
        );
        assert_eq!(classify(&l, &at(&git, "HEAD")), Some(Signal::Head));
        assert_eq!(
            classify(&l, &at(&git, "refs/heads/main")),
            Some(Signal::Ref("refs/heads/main".to_owned()))
        );
        assert_eq!(
            classify(&l, &at(&git, "packed-refs")),
            Some(Signal::RefStore)
        );
        assert_eq!(
            classify(&l, &at(&git, "reftable/tables.list")),
            Some(Signal::RefStore)
        );
        for noise in [
            "objects/ab/cdef",
            "index",
            "index.lock",
            "HEAD.lock",
            "refs/heads/main.lock",
            "packed-refs.lock",
            "logs/HEAD",
            "ORIG_HEAD",
            "FETCH_HEAD",
        ] {
            assert_eq!(classify(&l, &at(&git, noise)), None, "{noise}");
        }
        assert_eq!(classify(&l, &git), None);
        assert_eq!(classify(&l, &l.root), None);
        assert_eq!(classify(&l, &at(Path::new("/elsewhere"), "x")), None);
    }

    #[test]
    fn classifies_linked_worktree_paths() {
        let l = layout(true);
        assert_eq!(classify(&l, &at(&l.git_dir, "HEAD")), Some(Signal::Head));
        // The main worktree's HEAD is not ours.
        assert_eq!(classify(&l, &at(&l.common_dir, "HEAD")), None);
        assert_eq!(
            classify(&l, &at(&l.common_dir, "refs/remotes/origin/dev")),
            Some(Signal::Ref("refs/remotes/origin/dev".to_owned()))
        );
        assert_eq!(
            classify(&l, &at(&l.common_dir, "packed-refs")),
            Some(Signal::RefStore)
        );
        assert_eq!(classify(&l, &at(&l.git_dir, "index")), None);
        assert_eq!(
            classify(&l, &at(&l.common_dir, "objects/pack/x.pack")),
            None
        );
        // The `.git` file of a linked worktree is a worktree path; the
        // exclusion policy drops it.
        assert_eq!(
            classify(&l, &at(&l.root, ".git")),
            Some(Signal::File(RepoPath::new(".git").unwrap()))
        );
    }

    #[test]
    fn access_events_are_not_changes() {
        assert!(!is_change(&EventKind::Access(AccessKind::Open(
            AccessMode::Any
        ))));
        assert!(!is_change(&EventKind::Access(AccessKind::Close(
            AccessMode::Read
        ))));
        assert!(is_change(&EventKind::Access(AccessKind::Close(
            AccessMode::Write
        ))));
        assert!(is_change(&EventKind::Any));
    }

    #[test]
    fn debounce_timing() {
        let options = WatchOptions {
            debounce: Duration::from_millis(100),
            max_delay: Duration::from_millis(250),
            respect_gitignore: true,
        };
        let t0 = Instant::now();
        let mut p = Pending::default();
        assert!(!p.is_due(&options, t0));
        assert_eq!(p.wait(&options, t0), POLL);
        p.touch(t0);
        assert!(!p.is_due(&options, t0 + Duration::from_millis(99)));
        assert!(p.is_due(&options, t0 + Duration::from_millis(100)));
        // Continuous activity: each event restarts the quiet period, but the
        // batch is due once max_delay has passed since the first event.
        p.touch(t0 + Duration::from_millis(200));
        assert!(!p.is_due(&options, t0 + Duration::from_millis(240)));
        assert!(p.is_due(&options, t0 + Duration::from_millis(250)));
        assert!(p.wait(&options, t0 + Duration::from_millis(240)) <= Duration::from_millis(10));
    }

    #[test]
    fn head_referent_parsing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("HEAD"), "ref: refs/heads/topic\n").unwrap();
        assert_eq!(
            head_referent(dir.path()).as_deref(),
            Some("refs/heads/topic")
        );
        std::fs::write(dir.path().join("HEAD"), format!("{}\n", "a".repeat(40))).unwrap();
        assert_eq!(head_referent(dir.path()), None);
        std::fs::write(dir.path().join("HEAD"), "ref:").unwrap();
        assert_eq!(head_referent(dir.path()), None);
        assert_eq!(head_referent(&dir.path().join("missing")), None);
    }
}
