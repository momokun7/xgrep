//! File watching for long-lived processes (`xg serve`, library users).
//!
//! A [`WatchHandle`] keeps an in-memory set of paths that changed under the
//! root since the index was last rebuilt (the *dirty set*). While it is
//! healthy, index searches take that set instead of probing the filesystem:
//! dirty files are excluded from the index scan and read directly, exactly
//! like the probe-based overlay in [`crate::index::updater`].
//!
//! A worker thread owns the OS watcher and rebuilds the index incrementally
//! once changes have been quiet for a moment (or many piled up), then drops
//! the dirty entries the rebuild covered.
//!
//! The dirty set is only trusted while it is known to be complete:
//!
//! - **Starting**: until watches are in place *and* an index rebuild that
//!   started afterwards has finished, searches keep probing.
//! - **Lost events** (inotify queue overflow, watcher errors, too many
//!   dirty paths): searches probe until the next rebuild completes.
//!
//! Remote filesystems (WSL `/mnt/<drive>` 9p/drvfs, NFS, SMB, FUSE) are not
//! watched at all: change events there are unreliable (on WSL2 `/mnt/c`,
//! inotify reports nothing, not even edits made from Linux). For those,
//! [`crate::Xgrep::start_watching`] switches to *polling mode* instead: the
//! probe runs as usual but its result is reused for
//! [`remote_probe_ttl`] (see [`WatchStatus::Polling`]).
//!
//! Events are taken undebounced: a debouncer would hold each event back for
//! its timeout, and a search issued right after an edit would then miss it.
//! Rebuild scheduling does its own (quiet-period) batching instead. Even
//! undebounced events arrive asynchronously; on Linux each snapshot is
//! therefore *fenced* (see `WatchHandle::fence`) so that a search always
//! sees writes that completed before it started. FSEvents and
//! ReadDirectoryChangesW offer no such ordering across handles, so on macOS
//! and Windows an edit becomes visible once its event arrives (normally
//! within milliseconds).
//!
//! On Linux each non-ignored directory gets its own inotify watch, so
//! `target/`, `node_modules/` and other ignored trees cost nothing; other
//! platforms watch the root recursively (FSEvents / ReadDirectoryChangesW
//! are recursive natively) and filter events.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::{Match, WalkBuilder};
use notify::event::{EventKind, ModifyKind};
use notify::{RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::index::builder::is_xgrep_internal;

/// Default reuse window for probe results in polling mode.
const REMOTE_PROBE_TTL_MS: u64 = 2000;

/// How long a probe result is reused in polling mode (remote filesystems,
/// see module docs). Override with `XGREP_REMOTE_PROBE_CACHE_MS`.
pub(crate) fn remote_probe_ttl() -> Duration {
    let ms = std::env::var("XGREP_REMOTE_PROBE_CACHE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(REMOTE_PROBE_TTL_MS);
    Duration::from_millis(ms)
}

/// Watcher state reported by [`crate::Xgrep::watch_status`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchStatus {
    /// No watcher (the default, e.g. for one-shot CLI searches).
    Off,
    /// Setting up watches and syncing the index; searches probe meanwhile.
    Starting,
    /// Watching: searches use the tracked change set without probing.
    Active,
    /// The root is on a filesystem without reliable change events (WSL
    /// `/mnt/<drive>`, network mounts), so it is not watched; searches probe
    /// and reuse the result for a couple of seconds, so an edit made there
    /// may take that long to show up.
    Polling,
    /// Change events were lost (overflow or watcher error); searches probe
    /// until the next index rebuild completes.
    Degraded,
    /// The watcher could not be started; searches probe as without it.
    Failed(String),
}

impl std::fmt::Display for WatchStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WatchStatus::Off => write!(f, "watching: no"),
            WatchStatus::Starting => write!(f, "watching: starting"),
            WatchStatus::Active => write!(f, "watching: yes"),
            WatchStatus::Polling => write!(
                f,
                "watching: no (no change events on this filesystem; rechecked every {:.1}s)",
                remote_probe_ttl().as_secs_f64()
            ),
            WatchStatus::Degraded => write!(f, "watching: degraded (probing)"),
            WatchStatus::Failed(_) => write!(f, "watching: no (watcher failed)"),
        }
    }
}

/// Rebuild scheduling knobs.
#[derive(Debug, Clone)]
pub(crate) struct WatchConfig {
    /// Rebuild once no event arrived for this long.
    pub quiet: Duration,
    /// ... or as soon as this many paths are dirty.
    pub max_pending: usize,
    /// ... or when the oldest dirty entry is this old (continuous churn).
    pub max_delay: Duration,
    /// Minimum gap between two rebuilds.
    pub min_interval: Duration,
    /// Wait after a failed rebuild (e.g. another process holds the lock).
    pub retry_after_error: Duration,
    /// Beyond this many dirty paths, drop the set and probe until the next
    /// rebuild (bounded memory and search cost).
    pub dirty_limit: usize,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            quiet: Duration::from_secs(2),
            max_pending: 256,
            max_delay: Duration::from_secs(30),
            min_interval: Duration::from_secs(1),
            retry_after_error: Duration::from_secs(10),
            dirty_limit: 20_000,
        }
    }
}

/// Rebuilds the index; returns whether anything was rewritten.
pub(crate) type RebuildFn = Box<dyn Fn() -> crate::error::Result<bool> + Send + 'static>;

/// What a search needs from the watcher. Take it *before* opening the index
/// reader: the worker clears dirty entries only after the rebuilt index is
/// in place, so a snapshot taken first is never newer than the index read
/// after it.
#[derive(Debug)]
pub(crate) struct Snapshot {
    /// Root-relative paths changed since the last rebuild.
    pub paths: Vec<PathBuf>,
    /// The set is complete; searches may skip the probe.
    pub usable: bool,
}

#[derive(Default)]
struct State {
    /// Root-relative path -> when the last event for it arrived.
    dirty: HashMap<PathBuf, Instant>,
    /// Events may have been lost since this instant.
    lost_since: Option<Instant>,
    /// Watches are set up and a rebuild started after that has finished.
    synced: bool,
    /// Watches are set up.
    active: bool,
    failed: Option<String>,
    shutdown: bool,
    rebuild_requested: bool,
    /// Re-walk the tree to add watches (Linux; after ignore-file changes).
    rescan_watches: bool,
    /// New directories that need watches (Linux).
    new_dirs: Vec<PathBuf>,
    last_event: Option<Instant>,
    rebuilds: u64,
    /// The fence directory is being watched.
    fence_ready: bool,
    /// Highest fence token whose event has been handled.
    fence_seen: u64,
}

struct Shared {
    state: Mutex<State>,
    cond: Condvar,
    /// Canonical root (event paths are relative to it).
    root: PathBuf,
    cfg: WatchConfig,
    /// Linux: a watched directory next to the index used to fence the event
    /// queue (see [`WatchHandle::fence`]). Canonical path.
    fence_dir: Option<PathBuf>,
    /// Serializes fences; holds the last token issued.
    fence_lock: Mutex<u64>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A running watcher. Dropping it stops the OS watcher and joins the worker.
pub(crate) struct WatchHandle {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl WatchHandle {
    /// Start watching `root`. Setup (adding watches, the initial index sync)
    /// happens on the worker thread; errors surface via [`Self::status`].
    pub(crate) fn start(
        root: &Path,
        index_path: &Path,
        rebuild: RebuildFn,
        cfg: WatchConfig,
    ) -> std::io::Result<Self> {
        let canon = std::fs::canonicalize(root)?;
        let fence_dir = if PER_DIR_WATCHES {
            let d = index_path.with_extension("watch");
            std::fs::create_dir_all(&d)
                .ok()
                .and_then(|_| std::fs::canonicalize(&d).ok())
        } else {
            None
        };
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            cond: Condvar::new(),
            root: canon,
            cfg,
            fence_dir,
            fence_lock: Mutex::new(0),
        });
        let index_dir = index_path
            .parent()
            .and_then(|d| std::fs::canonicalize(d).ok())
            .and_then(|d| d.strip_prefix(&shared.root).ok().map(Path::to_path_buf));
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("xgrep-watch".into())
            .spawn(move || run_worker(worker_shared, index_dir, rebuild))?;
        Ok(Self {
            shared,
            worker: Some(worker),
        })
    }

    pub(crate) fn status(&self) -> WatchStatus {
        let st = self.shared.lock();
        if let Some(e) = &st.failed {
            WatchStatus::Failed(e.clone())
        } else if !st.active || (!st.synced && st.lost_since.is_none()) {
            WatchStatus::Starting
        } else if st.lost_since.is_some() {
            WatchStatus::Degraded
        } else {
            WatchStatus::Active
        }
    }

    /// True unless the watcher failed to start.
    pub(crate) fn is_running(&self) -> bool {
        self.shared.lock().failed.is_none()
    }

    /// Make sure every change event queued before this call has been handled.
    ///
    /// The OS delivers events asynchronously, so a search issued right after
    /// an edit could otherwise run before the edit's event reached the dirty
    /// set. On Linux all watches share one inotify queue, which is ordered:
    /// creating a uniquely named file in the watched fence directory and
    /// waiting for *its* event guarantees that every earlier event (in
    /// particular those of writes that completed before the search started)
    /// has been processed. Costs one create/unlink plus a thread wake-up
    /// (tens of microseconds).
    ///
    /// Returns `false` if the fence could not be confirmed in time; the
    /// caller then must not trust the dirty set. Platforms without a fence
    /// (FSEvents / ReadDirectoryChangesW, where events arrive within
    /// milliseconds) return `true`.
    fn fence(&self) -> bool {
        let Some(dir) = self.shared.fence_dir.as_ref() else {
            return true;
        };
        if !self.shared.lock().fence_ready {
            return true;
        }
        let mut issued = self
            .shared
            .fence_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *issued += 1;
        let token = *issued;
        let file = dir.join(format!("{}-{token}", std::process::id()));
        if std::fs::File::create(&file).is_err() {
            return false;
        }
        let deadline = Instant::now() + FENCE_TIMEOUT;
        let mut st = self.shared.lock();
        let ok = loop {
            if st.fence_seen >= token {
                break true;
            }
            let now = Instant::now();
            if now >= deadline || st.shutdown {
                break false;
            }
            st = wait(&self.shared, st, deadline - now);
        };
        drop(st);
        let _ = std::fs::remove_file(&file);
        ok
    }

    pub(crate) fn snapshot(&self) -> Snapshot {
        let fenced = self.fence();
        let st = self.shared.lock();
        let usable =
            fenced && st.failed.is_none() && st.active && st.synced && st.lost_since.is_none();
        Snapshot {
            paths: if usable {
                st.dirty.keys().cloned().collect()
            } else {
                Vec::new()
            },
            usable,
        }
    }

    /// Ask for a rebuild even if no event arrived (e.g. a probe found
    /// changes the watcher cannot see). Still waits for the quiet period.
    pub(crate) fn request_rebuild(&self) {
        let mut st = self.shared.lock();
        if !st.rebuild_requested {
            st.rebuild_requested = true;
            self.shared.cond.notify_all();
        }
    }

    /// Record that the index was rebuilt by someone else in this process
    /// (e.g. an explicit `build_index`) with a walk that began at `started`.
    pub(crate) fn mark_rebuilt(&self, started: Instant) {
        let mut st = self.shared.lock();
        apply_rebuild(&mut st, started);
    }

    /// Number of rebuilds the worker completed (tests/diagnostics).
    #[cfg(test)]
    pub(crate) fn rebuilds(&self) -> u64 {
        self.shared.lock().rebuilds
    }

    /// Pretend the OS reported lost events (queue overflow).
    #[cfg(test)]
    pub(crate) fn simulate_lost_events(&self) {
        let mut st = self.shared.lock();
        st.lost_since.get_or_insert(Instant::now());
        st.last_event = Some(Instant::now());
        self.shared.cond.notify_all();
    }

    /// Current dirty set (sorted), regardless of whether it is usable.
    #[cfg(test)]
    pub(crate) fn dirty_paths(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = self.shared.lock().dirty.keys().cloned().collect();
        v.sort();
        v
    }

    /// Block until the watcher is usable (or failed), up to `timeout`.
    pub(crate) fn wait_synced(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut st = self.shared.lock();
        loop {
            if st.failed.is_some() {
                return false;
            }
            if st.active && st.synced && st.lost_since.is_none() {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            st = self
                .shared
                .cond
                .wait_timeout(st, (deadline - now).min(Duration::from_millis(50)))
                .map(|(g, _)| g)
                .unwrap_or_else(|e| e.into_inner().0);
        }
    }
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        {
            let mut st = self.shared.lock();
            st.shutdown = true;
            self.shared.cond.notify_all();
        }
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
        if let Some(d) = &self.shared.fence_dir {
            // Only succeeds when empty (another process may share it).
            let _ = std::fs::remove_dir(d);
        }
    }
}

/// Clear what a rebuild whose walk began at `started` covered.
fn apply_rebuild(st: &mut State, started: Instant) {
    st.dirty.retain(|_, t| *t >= started);
    if st.lost_since.is_some_and(|l| l < started) {
        st.lost_since = None;
    }
    if st.active {
        st.synced = true;
    }
    st.rebuilds += 1;
}

fn run_worker(shared: Arc<Shared>, index_dir: Option<PathBuf>, rebuild: RebuildFn) {
    let mut watcher = match setup_watcher(&shared, index_dir) {
        Ok(w) => w,
        Err(e) => {
            let mut st = shared.lock();
            st.failed = Some(e.to_string());
            shared.cond.notify_all();
            return;
        }
    };
    {
        let mut st = shared.lock();
        st.active = true;
        shared.cond.notify_all();
    }

    let cfg = shared.cfg.clone();
    let mut last_rebuild_end: Option<Instant> = None;
    let mut retry_at: Option<Instant> = None;
    let mut st = shared.lock();
    loop {
        if st.shutdown {
            break;
        }
        // Linux: watch directories that appeared (or became un-ignored).
        let new_dirs = std::mem::take(&mut st.new_dirs);
        let rescan = std::mem::take(&mut st.rescan_watches);
        if !new_dirs.is_empty() || rescan {
            drop(st);
            add_new_watches(&shared, &mut watcher, new_dirs, rescan);
            st = shared.lock();
            continue;
        }

        let now = Instant::now();
        let pending =
            !st.synced || st.rebuild_requested || st.lost_since.is_some() || !st.dirty.is_empty();
        if !pending {
            st = wait(&shared, st, Duration::from_secs(60));
            continue;
        }
        // When is a rebuild due?
        let due = if !st.synced && st.lost_since.is_none() {
            now
        } else {
            let quiet_at = st.last_event.map_or(now, |t| t + cfg.quiet);
            let oldest = st.dirty.values().min().map(|t| *t + cfg.max_delay);
            let mut due = quiet_at;
            if let Some(o) = oldest {
                due = due.min(o);
            }
            if st.dirty.len() >= cfg.max_pending {
                due = now;
            }
            due
        };
        let mut earliest = due;
        if let Some(t) = last_rebuild_end {
            earliest = earliest.max(t + cfg.min_interval);
        }
        if let Some(t) = retry_at {
            earliest = earliest.max(t);
        }
        if earliest > now {
            st = wait(&shared, st, earliest - now);
            continue;
        }

        st.rebuild_requested = false;
        drop(st);
        let started = Instant::now();
        let res = rebuild();
        st = shared.lock();
        last_rebuild_end = Some(Instant::now());
        match res {
            Ok(_) => {
                retry_at = None;
                apply_rebuild(&mut st, started);
            }
            Err(_) => {
                retry_at = Some(Instant::now() + cfg.retry_after_error);
            }
        }
        shared.cond.notify_all();
    }
    drop(st);
    // Stops the OS watcher (and its event thread) before the worker exits.
    drop(watcher);
}

fn wait<'a>(shared: &'a Shared, st: MutexGuard<'a, State>, d: Duration) -> MutexGuard<'a, State> {
    shared
        .cond
        .wait_timeout(st, d)
        .map(|(g, _)| g)
        .unwrap_or_else(|e| e.into_inner().0)
}

/// Directories under `dir` (inclusive) that the builder would descend into.
fn watchable_dirs(dir: &Path) -> impl Iterator<Item = PathBuf> {
    WalkBuilder::new(dir)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(|e| {
            e.file_type().is_some_and(|t| t.is_dir())
                && !is_xgrep_internal(&e.file_name().to_string_lossy())
        })
        .build()
        .flatten()
        .filter(|e| e.file_type().is_some_and(|t| t.is_dir()))
        .map(|e| e.into_path())
}

const PER_DIR_WATCHES: bool = cfg!(any(target_os = "linux", target_os = "android"));

/// How long a search waits for its fence event before falling back to the
/// probe.
const FENCE_TIMEOUT: Duration = Duration::from_millis(250);

fn setup_watcher(
    shared: &Arc<Shared>,
    index_dir: Option<PathBuf>,
) -> notify::Result<RecommendedWatcher> {
    let handler_shared = Arc::clone(shared);
    let mut filter = IgnoreFilter::new(&shared.root);
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        handle_event(&handler_shared, &mut filter, index_dir.as_deref(), res);
    })?;
    if PER_DIR_WATCHES {
        if let Some(fd) = &shared.fence_dir {
            if watcher.watch(fd, RecursiveMode::NonRecursive).is_ok() {
                shared.lock().fence_ready = true;
            }
        }
        for d in watchable_dirs(&shared.root) {
            watcher.watch(&d, RecursiveMode::NonRecursive)?;
        }
    } else {
        watcher.watch(&shared.root, RecursiveMode::Recursive)?;
    }
    Ok(watcher)
}

/// Add watches for new directories (and everything below them). Afterwards
/// each is marked dirty again: files created before its watch existed are
/// then found by walking it at search time.
fn add_new_watches(
    shared: &Shared,
    watcher: &mut RecommendedWatcher,
    new_dirs: Vec<PathBuf>,
    rescan: bool,
) {
    if !PER_DIR_WATCHES {
        return;
    }
    let mut failed = false;
    let roots: Vec<PathBuf> = if rescan {
        vec![shared.root.clone()]
    } else {
        new_dirs.clone()
    };
    for r in &roots {
        for d in watchable_dirs(r) {
            if watcher.watch(&d, RecursiveMode::NonRecursive).is_err() {
                failed = true;
            }
        }
    }
    let now = Instant::now();
    let mut st = shared.lock();
    for d in new_dirs {
        if let Ok(rel) = d.strip_prefix(&shared.root) {
            st.dirty.insert(rel.to_path_buf(), now);
        }
    }
    if failed {
        // e.g. inotify watch limit: events below those dirs are invisible.
        st.lost_since.get_or_insert(now);
    }
    shared.cond.notify_all();
}

fn handle_event(
    shared: &Shared,
    filter: &mut IgnoreFilter,
    index_dir: Option<&Path>,
    res: notify::Result<notify::Event>,
) {
    let now = Instant::now();
    let ev = match res {
        Ok(ev) => ev,
        Err(_) => {
            let mut st = shared.lock();
            st.lost_since.get_or_insert(now);
            shared.cond.notify_all();
            return;
        }
    };
    let lost = ev.need_rescan();
    let mut fence: Option<u64> = None;
    let mut adds: Vec<PathBuf> = Vec::new();
    let mut new_dirs: Vec<PathBuf> = Vec::new();
    let mut ignore_changed = false;
    for p in &ev.paths {
        if shared.fence_dir.is_some() && p.parent() == shared.fence_dir.as_deref() {
            if !matches!(ev.kind, EventKind::Remove(_)) {
                fence = fence.max(parse_fence_token(p));
            }
            continue;
        }
        if matches!(ev.kind, EventKind::Access(_)) {
            continue;
        }
        let Ok(rel) = p.strip_prefix(&shared.root) else {
            continue;
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let name = rel.file_name().map(|n| n.to_string_lossy());
        if matches!(name.as_deref(), Some(".gitignore") | Some(".ignore")) {
            if let Some(dir) = p.parent() {
                filter.invalidate(dir);
            }
            ignore_changed = true;
            continue;
        }
        let hidden_or_internal = rel.iter().any(|c| {
            let c = c.to_string_lossy();
            c.starts_with('.') || is_xgrep_internal(&c)
        });
        if hidden_or_internal || index_dir.is_some_and(|d| rel.starts_with(d)) {
            continue;
        }
        let is_dir = std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir());
        if filter.is_ignored(p, is_dir) {
            continue;
        }
        if is_dir {
            match ev.kind {
                EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(_)) | EventKind::Any => {
                    new_dirs.push(p.clone());
                }
                EventKind::Remove(_) => {}
                // Attribute/mtime changes of a directory itself carry no
                // information about its files (those get their own events).
                _ => continue,
            }
        }
        adds.push(rel.to_path_buf());
    }
    if adds.is_empty() && new_dirs.is_empty() && !ignore_changed && !lost && fence.is_none() {
        return;
    }
    let mut st = shared.lock();
    if let Some(t) = fence {
        st.fence_seen = st.fence_seen.max(t);
        if adds.is_empty() && new_dirs.is_empty() && !ignore_changed && !lost {
            shared.cond.notify_all();
            return;
        }
    }
    if lost {
        st.lost_since.get_or_insert(now);
        st.rescan_watches = true;
    }
    for a in adds {
        st.dirty.insert(a, now);
    }
    if st.dirty.len() > shared.cfg.dirty_limit {
        // Too much churn to track path by path: probe until a rebuild.
        st.dirty.clear();
        st.lost_since.get_or_insert(now);
    }
    st.new_dirs.extend(new_dirs);
    if ignore_changed {
        // The set of indexed files changes without per-file events.
        st.rebuild_requested = true;
        st.rescan_watches = true;
    }
    st.last_event = Some(now);
    shared.cond.notify_all();
}

/// Token of a fence file named `<pid>-<token>` created by this process.
fn parse_fence_token(p: &Path) -> Option<u64> {
    let name = p.file_name()?.to_str()?;
    let (pid, token) = name.split_once('-')?;
    (pid.parse::<u32>().ok()? == std::process::id())
        .then(|| token.parse().ok())
        .flatten()
}

// ---------------------------------------------------------------------------
// Ignore rules
// ---------------------------------------------------------------------------

/// Per-directory ignore matchers.
#[derive(Default)]
struct DirIgnores {
    ignore: Option<Gitignore>,
    gitignore: Option<Gitignore>,
}

/// Decides whether an event path is something the builder would skip,
/// mirroring `ignore::WalkBuilder` with `hidden`, `git_ignore`,
/// `git_global` and `git_exclude` enabled: `.ignore` beats `.gitignore`
/// beats `.git/info/exclude` beats the global excludes file, and within a
/// kind the deepest directory wins. `.gitignore` rules apply only inside a
/// git work tree.
///
/// A wrong answer here is never fatal: an ignored path that slips through
/// is merely rescanned until the next rebuild clears it.
struct IgnoreFilter {
    root: PathBuf,
    /// Top-most directory whose ignore files apply (git top level, or root).
    top: PathBuf,
    in_git: bool,
    exclude: Option<Gitignore>,
    global: Option<Gitignore>,
    dirs: HashMap<PathBuf, DirIgnores>,
}

impl IgnoreFilter {
    fn new(root: &Path) -> Self {
        let top = crate::git::find_toplevel_fs(root);
        let in_git = top.is_some();
        let top = top.unwrap_or_else(|| root.to_path_buf());
        let (exclude, global) = if in_git {
            let ex = top.join(".git").join("info").join("exclude");
            let exclude = ex.is_file().then(|| {
                let mut b = GitignoreBuilder::new(&top);
                b.add(&ex);
                b.build().ok()
            });
            let (gl, _) = Gitignore::global();
            (exclude.flatten(), (!gl.is_empty()).then_some(gl))
        } else {
            (None, None)
        };
        Self {
            root: root.to_path_buf(),
            top,
            in_git,
            exclude,
            global,
            dirs: HashMap::new(),
        }
    }

    fn invalidate(&mut self, dir: &Path) {
        self.dirs.remove(dir);
    }

    fn load(dir: &Path, in_git: bool) -> DirIgnores {
        let one = |name: &str| -> Option<Gitignore> {
            let f = dir.join(name);
            if !f.is_file() {
                return None;
            }
            let mut b = GitignoreBuilder::new(dir);
            b.add(&f);
            b.build().ok().filter(|g| !g.is_empty())
        };
        DirIgnores {
            ignore: one(".ignore"),
            gitignore: if in_git { one(".gitignore") } else { None },
        }
    }

    /// True if `path` (absolute, under root) or any of its ancestors below
    /// the root is ignored.
    fn is_ignored(&mut self, path: &Path, is_dir: bool) -> bool {
        let Ok(rel) = path.strip_prefix(&self.root) else {
            return false;
        };
        let comps: Vec<_> = rel.components().collect();
        let mut q = self.root.clone();
        for (i, c) in comps.iter().enumerate() {
            q.push(c);
            let q_is_dir = i + 1 < comps.len() || is_dir;
            if self.decide(&q, q_is_dir) {
                return true;
            }
        }
        false
    }

    fn decide(&mut self, q: &Path, is_dir: bool) -> bool {
        // Ancestor directories from deepest to `top`.
        let mut chain: Vec<PathBuf> = Vec::new();
        let mut cur = q.parent();
        while let Some(d) = cur {
            chain.push(d.to_path_buf());
            if d == self.top || !d.starts_with(&self.top) {
                break;
            }
            cur = d.parent();
        }
        for d in &chain {
            if !self.dirs.contains_key(d) {
                let loaded = Self::load(d, self.in_git);
                self.dirs.insert(d.clone(), loaded);
            }
        }
        let verdict = |m: Match<&ignore::gitignore::Glob>| match m {
            Match::Ignore(_) => Some(true),
            Match::Whitelist(_) => Some(false),
            Match::None => None,
        };
        for pick in [
            (|d: &DirIgnores| d.ignore.as_ref()) as fn(&DirIgnores) -> Option<&Gitignore>,
            |d: &DirIgnores| d.gitignore.as_ref(),
        ] {
            for d in &chain {
                if let Some(g) = self.dirs.get(d).and_then(pick) {
                    if let Some(v) = verdict(g.matched(q, is_dir)) {
                        return v;
                    }
                }
            }
        }
        for g in [self.exclude.as_ref(), self.global.as_ref()]
            .into_iter()
            .flatten()
        {
            if let Some(v) = verdict(g.matched_path_or_any_parents(q, is_dir)) {
                return v;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, SearchOptions, Xgrep};
    use std::fs;

    /// Generous upper bound for anything event-driven (CI machines can be
    /// slow); polls return as soon as the condition holds.
    const TIMEOUT: Duration = Duration::from_secs(20);

    fn poll(mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        cond()
    }

    /// Config whose rebuilds never fire during a test (except the initial
    /// sync), so the dirty set stays observable.
    fn no_rebuild_cfg() -> WatchConfig {
        WatchConfig {
            quiet: Duration::from_secs(3600),
            max_delay: Duration::from_secs(3600),
            max_pending: usize::MAX,
            ..WatchConfig::default()
        }
    }

    fn quick_rebuild_cfg() -> WatchConfig {
        WatchConfig {
            quiet: Duration::from_millis(150),
            min_interval: Duration::ZERO,
            ..WatchConfig::default()
        }
    }

    fn setup(files: &[(&str, &str)]) -> (tempfile::TempDir, Xgrep) {
        let dir = tempfile::tempdir().unwrap();
        for (p, c) in files {
            let full = dir.path().join(p);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, c).unwrap();
        }
        let xg = Xgrep::open_local(dir.path())
            .unwrap()
            .with_config(Config { quiet: true });
        xg.build_index().unwrap();
        (dir, xg)
    }

    fn hits(xg: &Xgrep, pat: &str) -> Vec<String> {
        let mut v: Vec<String> = xg
            .search(pat, &SearchOptions::new())
            .unwrap()
            .into_iter()
            .map(|r| r.file.replace('\\', "/"))
            .collect();
        v.dedup();
        v
    }

    fn source(xg: &Xgrep) -> &'static str {
        *xg.last_overlay_source.lock().unwrap()
    }

    fn dirty(xg: &Xgrep) -> Vec<PathBuf> {
        xg.watcher_guard().as_ref().unwrap().dirty_paths()
    }

    fn rel(p: &str) -> PathBuf {
        p.split('/').collect()
    }

    #[test]
    fn test_watcher_tracks_create_modify_delete_without_probe() {
        let (dir, xg) = setup(&[
            ("a.rs", "fn alpha_old() {}\n"),
            ("gone.rs", "fn gone_sym() {}\n"),
        ]);
        let root = dir.path();
        xg.start_watching_with(no_rebuild_cfg()).unwrap();
        assert!(xg.wait_for_watcher(TIMEOUT), "{:?}", xg.watch_status());
        assert!(xg.watch_status() == WatchStatus::Active);

        assert_eq!(hits(&xg, "alpha_old"), vec!["a.rs"]);
        assert_eq!(source(&xg), "watcher");
        assert_eq!(
            xg.freshness_note(),
            format!("index: fresh; {}", xg.watch_status())
        );

        fs::write(root.join("a.rs"), "fn alpha_new() {}\n").unwrap();
        fs::write(root.join("created.rs"), "fn created_sym() {}\n").unwrap();
        fs::remove_file(root.join("gone.rs")).unwrap();
        assert!(
            poll(|| dirty(&xg).len() == 3),
            "dirty set: {:?}",
            dirty(&xg)
        );
        assert_eq!(
            dirty(&xg),
            vec![rel("a.rs"), rel("created.rs"), rel("gone.rs")]
        );

        assert_eq!(hits(&xg, "alpha_new"), vec!["a.rs"]);
        assert_eq!(source(&xg), "watcher");
        assert!(hits(&xg, "alpha_old").is_empty());
        assert_eq!(hits(&xg, "created_sym"), vec!["created.rs"]);
        assert!(hits(&xg, "gone_sym").is_empty());
        let f = xg.last_search_freshness().unwrap();
        assert_eq!(f.changed_files, 3, "{f:?}");
        assert_eq!(f.rescanned_files, 2, "{f:?}");
        assert!(xg
            .freshness_note()
            .starts_with("index stale: 2 files rescanned; watching: "));
    }

    /// A search issued immediately after a write sees it: on Linux the
    /// fence guarantees the event was handled; elsewhere events arrive
    /// quickly but asynchronously, so only Linux asserts without retrying.
    #[test]
    fn test_search_immediately_after_write() {
        let (dir, xg) = setup(&[("a.rs", "fn a() {}\n")]);
        let root = dir.path();
        xg.start_watching_with(no_rebuild_cfg()).unwrap();
        assert!(xg.wait_for_watcher(TIMEOUT));
        for i in 0..40 {
            let name = format!("w{i}.rs");
            fs::write(root.join(&name), format!("fn immediate_{i}_sym() {{}}\n")).unwrap();
            let pat = format!("immediate_{i}_sym");
            if PER_DIR_WATCHES {
                assert_eq!(hits(&xg, &pat), vec![name.clone()], "iteration {i}");
                assert_eq!(source(&xg), "watcher");
            } else {
                assert!(poll(|| hits(&xg, &pat) == vec![name.clone()]));
            }
        }
    }

    #[test]
    fn test_watcher_new_directory_and_rename() {
        let (dir, xg) = setup(&[("old/x.rs", "fn in_old_dir() {}\n")]);
        let root = dir.path();
        xg.start_watching_with(no_rebuild_cfg()).unwrap();
        assert!(xg.wait_for_watcher(TIMEOUT));

        // A whole tree appears at once (files may be written before a watch
        // on the new directory exists).
        fs::create_dir_all(root.join("new/deep/er")).unwrap();
        fs::write(root.join("new/deep/er/n.rs"), "fn deep_new_sym() {}\n").unwrap();
        assert!(poll(
            || hits(&xg, "deep_new_sym") == vec!["new/deep/er/n.rs"]
        ));
        assert_eq!(source(&xg), "watcher");

        // Renaming a directory moves its files.
        fs::rename(root.join("old"), root.join("moved")).unwrap();
        assert!(poll(|| hits(&xg, "in_old_dir") == vec!["moved/x.rs"]));

        // Files created later inside the new directory are watched too.
        fs::write(root.join("new/deep/er/m.rs"), "fn later_sym() {}\n").unwrap();
        assert!(poll(|| hits(&xg, "later_sym") == vec!["new/deep/er/m.rs"]));
    }

    #[test]
    fn test_watcher_respects_ignore_rules() {
        let (dir, xg) = setup(&[
            (".ignore", "target/\n*.log\n"),
            ("src/lib.rs", "fn lib() {}\n"),
        ]);
        let root = dir.path();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        xg.start_watching_with(no_rebuild_cfg()).unwrap();
        assert!(xg.wait_for_watcher(TIMEOUT));

        // Ignored and hidden paths first; the visible file last, so once it
        // shows up the earlier events have been processed (one ordered queue).
        fs::write(root.join("target/debug/gen.rs"), "fn ignored_sym() {}\n").unwrap();
        fs::write(root.join("build.log"), "ignored_sym\n").unwrap();
        fs::create_dir_all(root.join(".cache")).unwrap();
        fs::write(root.join(".cache/c.rs"), "fn ignored_sym() {}\n").unwrap();
        fs::write(root.join("src/ok.rs"), "fn visible_sym() {}\n").unwrap();
        assert!(poll(|| dirty(&xg).contains(&rel("src/ok.rs"))));
        assert_eq!(dirty(&xg), vec![rel("src/ok.rs")]);
        assert!(hits(&xg, "ignored_sym").is_empty());
        assert_eq!(hits(&xg, "visible_sym"), vec!["src/ok.rs"]);
    }

    #[test]
    fn test_watcher_background_rebuild_clears_dirty_set() {
        let (dir, xg) = setup(&[("a.rs", "fn before_sym() {}\n")]);
        let root = dir.path();
        xg.start_watching_with(quick_rebuild_cfg()).unwrap();
        assert!(xg.wait_for_watcher(TIMEOUT));
        let initial = xg.watcher_guard().as_ref().unwrap().rebuilds();

        fs::write(root.join("a.rs"), "fn after_sym() {}\n").unwrap();
        fs::write(root.join("b.rs"), "fn b_sym() {}\n").unwrap();
        assert!(poll(|| {
            xg.watcher_guard().as_ref().unwrap().rebuilds() > initial && dirty(&xg).is_empty()
        }));
        // Now served from the rebuilt index alone.
        assert_eq!(hits(&xg, "after_sym"), vec!["a.rs"]);
        assert_eq!(hits(&xg, "b_sym"), vec!["b.rs"]);
        assert!(hits(&xg, "before_sym").is_empty());
        assert_eq!(
            xg.last_search_freshness(),
            Some(crate::Freshness::default())
        );
        assert_eq!(source(&xg), "watcher");
    }

    #[test]
    fn test_watcher_lost_events_fall_back_to_probe_until_rebuild() {
        let (dir, xg) = setup(&[("a.rs", "fn a_old() {}\n")]);
        let root = dir.path();
        xg.start_watching_with(quick_rebuild_cfg()).unwrap();
        assert!(xg.wait_for_watcher(TIMEOUT));

        let initial = xg.watcher_guard().as_ref().unwrap().rebuilds();
        // Pretend the kernel queue overflowed; then edit (the edit is seen by
        // the probe even though the watcher no longer vouches for its set).
        xg.watcher_guard().as_ref().unwrap().simulate_lost_events();
        assert_eq!(xg.watch_status(), WatchStatus::Degraded);
        fs::write(root.join("a.rs"), "fn a_new_after_overflow() {}\n").unwrap();
        // Searches fall back to the probe while degraded.
        let found = hits(&xg, "a_new_after_overflow");
        if xg.watch_status() == WatchStatus::Degraded {
            assert_eq!(source(&xg), "probe");
        }
        assert_eq!(found, vec!["a.rs"]);

        // A rebuild after the loss restores normal operation.
        assert!(poll(
            || xg.watcher_guard().as_ref().unwrap().rebuilds() > initial
        ));
        assert!(xg.wait_for_watcher(TIMEOUT));
        assert!(xg.watch_status() == WatchStatus::Active);
        assert_eq!(hits(&xg, "a_new_after_overflow"), vec!["a.rs"]);
        assert_eq!(source(&xg), "watcher");
    }

    #[test]
    fn test_explicit_build_marks_rebuilt_and_stop_watching() {
        let (dir, xg) = setup(&[("a.rs", "fn a() {}\n")]);
        let root = dir.path();
        xg.start_watching_with(no_rebuild_cfg()).unwrap();
        assert!(xg.wait_for_watcher(TIMEOUT));
        fs::write(root.join("b.rs"), "fn b_sym() {}\n").unwrap();
        assert!(poll(|| !dirty(&xg).is_empty()));
        // An explicit build covers the change and clears the dirty set.
        xg.build_index().unwrap();
        assert!(dirty(&xg).is_empty());
        assert_eq!(hits(&xg, "b_sym"), vec!["b.rs"]);

        xg.stop_watching();
        assert_eq!(xg.watch_status(), WatchStatus::Off);
        assert_eq!(xg.freshness_note(), "index: fresh; watching: no");
        // Searches keep working with the probe.
        fs::write(root.join("c.rs"), "fn c_sym() {}\n").unwrap();
        assert_eq!(hits(&xg, "c_sym"), vec!["c.rs"]);
        assert_eq!(source(&xg), "probe");
    }

    #[test]
    fn test_ignore_filter_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join(".ignore"), "*.gen\nbuild/\n").unwrap();
        fs::write(root.join("a/.ignore"), "!keep.gen\n").unwrap();
        let mut f = IgnoreFilter::new(&root);
        assert!(f.is_ignored(&root.join("x.gen"), false));
        assert!(!f.is_ignored(&root.join("x.rs"), false));
        // Deeper whitelist wins.
        assert!(!f.is_ignored(&root.join("a/keep.gen"), false));
        assert!(f.is_ignored(&root.join("a/other.gen"), false));
        // Anything below an ignored directory is ignored.
        assert!(f.is_ignored(&root.join("build"), true));
        assert!(f.is_ignored(&root.join("build/deep/x.rs"), false));
        // Not in a git repo: .gitignore does not apply (like the builder).
        fs::write(root.join(".gitignore"), "*.rs\n").unwrap();
        let mut f = IgnoreFilter::new(&root);
        assert!(!f.is_ignored(&root.join("x.rs"), false));
        // Invalidation picks up edits.
        fs::write(root.join(".ignore"), "*.rs\n").unwrap();
        f.invalidate(&root);
        assert!(f.is_ignored(&root.join("x.rs"), false));
    }

    /// Polling mode (what `start_watching` picks on WSL `/mnt/<drive>` and
    /// network mounts): no OS watcher, the probe result is reused for the
    /// TTL and then refreshed.
    #[test]
    fn test_polling_mode_reuses_probe_for_ttl() {
        let (dir, xg) = setup(&[("a.rs", "fn poll_old() {}\n")]);
        let root = dir.path();
        xg.polling.store(true, std::sync::atomic::Ordering::Relaxed);
        // Idempotent; must not start an OS watcher in polling mode.
        xg.start_watching().unwrap();
        assert!(xg.watcher_guard().is_none());
        assert_eq!(xg.watch_status(), WatchStatus::Polling);
        assert!(xg
            .freshness_note()
            .contains("watching: no (no change events"));

        assert_eq!(hits(&xg, "poll_old"), vec!["a.rs"]);
        assert_eq!(source(&xg), "probe-polling");
        let probed_at = Instant::now();
        fs::write(root.join("b.rs"), "fn poll_new() {}\n").unwrap();
        let first = hits(&xg, "poll_new");
        if probed_at.elapsed() < remote_probe_ttl() {
            // Within the TTL the cached probe result is reused.
            assert!(first.is_empty(), "{first:?}");
        }
        std::thread::sleep(remote_probe_ttl());
        assert_eq!(hits(&xg, "poll_new"), vec!["b.rs"]);

        xg.stop_watching();
        assert_eq!(xg.watch_status(), WatchStatus::Off);
    }

    #[test]
    fn test_status_display() {
        assert_eq!(WatchStatus::Off.to_string(), "watching: no");
        assert_eq!(WatchStatus::Active.to_string(), "watching: yes");
        assert!(WatchStatus::Polling
            .to_string()
            .starts_with("watching: no (no change events"));
    }
}
