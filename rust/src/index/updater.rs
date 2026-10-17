use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use std::collections::HashSet;

use crate::error::Result;
use crate::git::{bytes_to_path, find_toplevel_fs, git_cmd, split_nul};
use crate::index::builder::is_xgrep_internal;
use crate::index::format::FileEntry;
use crate::index::reader::IndexReader;
use crate::safefs;
use ignore::WalkBuilder;

/// Index freshness check result.
#[derive(Debug)]
pub enum IndexStatus {
    /// Index is up-to-date, no changes.
    Fresh,
    /// Index exists but some files have changed. Use index search + direct scan of changed files.
    Stale { changed_files: Vec<PathBuf> },
    /// Index does not exist, full build required.
    NeedsFullBuild,
}

/// Metadata stored alongside the index.
///
/// File format (line oriented, backwards compatible with the old single-line
/// format that only held the commit hash):
///
/// ```text
/// <commit hash or empty>
/// verified-ns <start> <end>     # build window (unix nanos); index reflects the tree as of <start>
/// dir <path>                    # walked directory without indexed files (repeated)
/// ```
///
/// `dir` lines list the directories the build walked that contain no indexed
/// file anywhere below them (empty directories, directories holding only
/// binary or oversized files). The probe stats them along with the parents
/// of indexed files, so a file created in one of them later is found.
///
/// Older versions also wrote `dirty <path>` / `dirty-overflow` lines (git
/// status at build time); they are ignored now that the freshness probe
/// compares file timestamps directly.
#[derive(Debug, Default)]
struct IndexMeta {
    commit_hash: Option<String>,
    verified_at: Option<Window>,
    /// Walked directories not implied by indexed file paths (index form).
    dirs: Vec<String>,
}

impl IndexMeta {
    fn path_for(index_path: &Path) -> PathBuf {
        index_path.with_extension("meta")
    }

    fn load(index_path: &Path) -> Option<Self> {
        let meta_path = Self::path_for(index_path);
        let content = fs::read_to_string(&meta_path).ok()?;
        let mut lines = content.lines();
        // The meta file sits next to the index and may be repo-local, so keep
        // only a well-formed object id and paths that stay under the root.
        let commit_hash = lines
            .next()
            .map(|s| s.trim().to_string())
            .filter(|h| is_object_id(h));
        let mut meta = IndexMeta {
            commit_hash,
            ..Default::default()
        };
        for line in lines {
            if let Some(v) = line.strip_prefix("verified-ns ") {
                let mut it = v.split_whitespace().map(|n| n.parse::<u128>().ok());
                if let (Some(Some(start)), end) = (it.next(), it.next()) {
                    meta.verified_at = Some((start, end.flatten().unwrap_or(start)));
                }
            } else if let Some(d) = line.strip_prefix("dir ") {
                if safefs::is_safe_rel(Path::new(d)) {
                    meta.dirs.push(d.to_string());
                }
            }
        }
        Some(meta)
    }

    fn save(
        index_path: &Path,
        commit_hash: Option<&str>,
        verified_at: Option<Window>,
        dirs: &[String],
    ) -> Result<()> {
        use std::fmt::Write as _;
        let meta_path = Self::path_for(index_path);
        let mut content = commit_hash.unwrap_or("").to_string();
        if let Some((start, end)) = verified_at {
            let _ = write!(content, "\nverified-ns {start} {end}");
        }
        for d in dirs {
            // `extra_dirs` never yields names with line breaks.
            let _ = write!(content, "\ndir {d}");
        }
        // Write atomically so concurrent readers never see a truncated file.
        safefs::write_atomic(&meta_path, content.as_bytes())?;
        Ok(())
    }
}

/// True for a full hex object id (SHA-1 or SHA-256).
fn is_object_id(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Build window `(start, end)` in nanoseconds since the UNIX epoch.
type Window = (u128, u128);

/// Nanoseconds since the UNIX epoch for a `SystemTime` (0 if before the epoch).
fn unix_nanos(t: std::time::SystemTime) -> u128 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

const NANOS_PER_SEC: u128 = 1_000_000_000;

/// Get the current git HEAD commit hash.
fn current_commit_hash(root: &Path) -> Option<String> {
    let output = git_cmd()
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

/// Get the newest file mtime in the directory (UNIX epoch seconds).
///
/// The index file and its sidecars (`.meta`, `.cache`) are excluded so that
/// writing the index itself never makes the source tree look "newer than the
/// index". These sidecars are derived with `with_extension`, so they replace
/// the index extension (`index.xgrep` -> `index.meta`), not append to it.
fn newest_file_mtime(root: &Path, index_path: &Path) -> Option<u64> {
    let sidecars = [
        index_path.to_path_buf(),
        index_path.with_extension("meta"),
        index_path.with_extension("cache"),
    ];
    let mut newest = 0u64;
    for entry in WalkBuilder::new(root).build().flatten() {
        if entry.file_type().is_some_and(|ft| ft.is_file()) {
            // Skip the index and its sidecars so they don't affect freshness
            // detection for the source tree.
            if sidecars.iter().any(|s| s == entry.path()) {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                if let Ok(mtime) = meta.modified() {
                    let secs = mtime
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    if secs > newest {
                        newest = secs;
                    }
                }
            }
        }
    }
    if newest > 0 {
        Some(newest)
    } else {
        None
    }
}

/// Paths reported by `git status --porcelain=v2 -z`.
///
/// Paths are raw bytes relative to the repository top level.
#[derive(Debug, Default)]
struct GitStatusSnapshot {
    paths: Vec<Vec<u8>>,
}

/// Parse `git status --porcelain=v2 -z` output (`#` header lines are skipped).
///
/// `-z` output is never quoted or escaped, so non-ASCII and otherwise odd
/// file names (`日本.txt`, names with spaces or quotes) come through intact.
/// Every path mentioned is returned, including both sides of a rename/copy,
/// so stale index entries for the old name are dropped too.
fn parse_status_v2(out: &[u8]) -> GitStatusSnapshot {
    /// Return the remainder of `tok` after skipping `n` space-separated fields.
    fn after_fields(tok: &[u8], n: usize) -> Option<&[u8]> {
        let mut rest = tok;
        for _ in 0..n {
            let sp = rest.iter().position(|&b| b == b' ')?;
            rest = &rest[sp + 1..];
        }
        (!rest.is_empty()).then_some(rest)
    }

    let mut snap = GitStatusSnapshot::default();
    let mut toks = out.split(|&b| b == 0);
    while let Some(tok) = toks.next() {
        let Some(&kind) = tok.first() else { continue };
        let path = match kind {
            b'#' => None,
            // 1 XY sub mH mI mW hH hI <path>
            b'1' => after_fields(tok, 8),
            // 2 XY sub mH mI mW hH hI Xscore <path>\0<origPath>
            b'2' => {
                if let Some(orig) = toks.next().filter(|o| !o.is_empty()) {
                    snap.paths.push(orig.to_vec());
                }
                after_fields(tok, 9)
            }
            // u XY sub m1 m2 m3 mW h1 h2 h3 <path>
            b'u' => after_fields(tok, 10),
            // ? <path> / ! <path>
            b'?' | b'!' => after_fields(tok, 1),
            _ => None,
        };
        if let Some(p) = path {
            snap.paths.push(p.to_vec());
        }
    }
    snap
}

/// Run `git status` for the subtree at `root` (tracked changes, staged
/// changes, and untracked files respecting `.gitignore`).
///
/// Returns `None` if `root` is not inside a git work tree or git failed.
/// `--no-optional-locks` keeps this read-only so it never contends with the
/// user's own git commands for `.git/index.lock`.
///
/// Gives up with `Err(TimedOut)` once `deadline` passes.
fn git_status(
    root: &Path,
    deadline: Option<Instant>,
) -> std::result::Result<Option<GitStatusSnapshot>, TimedOut> {
    let out = run_git(
        root,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--",
            ".",
        ],
        deadline,
    )?;
    Ok(out.map(|o| parse_status_v2(&o)))
}

/// Marker error: a freshness probe exceeded its time budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TimedOut;

fn check_deadline(deadline: Option<Instant>) -> std::result::Result<(), TimedOut> {
    match deadline {
        Some(d) if Instant::now() >= d => Err(TimedOut),
        _ => Ok(()),
    }
}

/// Run git in `root` and return stdout, `Ok(None)` if git failed, or
/// `Err(TimedOut)` if it did not finish before `deadline`. A timed-out
/// child is killed and reaped, so no zombie process is left behind.
fn run_git(
    root: &Path,
    args: &[&str],
    deadline: Option<Instant>,
) -> std::result::Result<Option<Vec<u8>>, TimedOut> {
    use std::io::Read;
    use std::process::Stdio;

    let Ok(mut child) = git_cmd()
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return Ok(None);
    };
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Ok(None);
    };
    let Some(deadline) = deadline else {
        let mut buf = Vec::new();
        let read_ok = stdout.read_to_end(&mut buf).is_ok();
        drop(stdout);
        let ok = child.wait().is_ok_and(|st| st.success());
        return Ok((read_ok && ok).then_some(buf));
    };

    // Read on a helper thread so the wait can be bounded.
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let res = stdout.read_to_end(&mut buf).map(|_| buf);
        let _ = tx.send(res);
    });
    match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Ok(buf)) => {
            let _ = reader.join();
            let ok = child.wait().is_ok_and(|st| st.success());
            Ok(ok.then_some(buf))
        }
        Ok(Err(_)) => {
            let _ = child.kill();
            let _ = child.wait();
            Ok(None)
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            // The reader thread sees EOF once the child is gone.
            let _ = reader.join();
            Err(TimedOut)
        }
    }
}

/// Relative path from the git top level to `root` (empty when `root` is the
/// top level). `None` when `root` is not inside a git work tree.
///
/// Uses a filesystem walk instead of `git rev-parse` to save a process spawn
/// on the per-search hot path.
fn git_prefix(root: &Path) -> Option<PathBuf> {
    let top = find_toplevel_fs(root)?;
    // Both sides canonicalized (e.g. /var -> /private/var on macOS).
    let canon_root = fs::canonicalize(root).ok()?;
    canon_root.strip_prefix(&top).ok().map(PathBuf::from)
}

/// Convert raw git-top-level-relative path bytes to a root-relative path.
/// Returns `None` for paths outside `root`.
fn convert_git_bytes(prefix: &Path, bytes: &[u8]) -> Option<PathBuf> {
    let p = bytes_to_path(bytes);
    p.strip_prefix(prefix).ok().map(PathBuf::from)
}

/// Common helper to collect uncommitted changes (staged + unstaged) and untracked files.
///
/// Returned paths are relative to `root`, not to the git repository root.
/// Paths outside of `root` are filtered out.
fn collect_uncommitted_changes(root: &Path) -> Result<std::collections::HashSet<PathBuf>> {
    let mut changed = std::collections::HashSet::new();
    let (Some(prefix), Ok(Some(snap))) = (git_prefix(root), git_status(root, None)) else {
        return Ok(changed);
    };
    changed.extend(
        snap.paths
            .iter()
            .filter_map(|p| convert_git_bytes(&prefix, p)),
    );
    Ok(changed)
}

/// Get files changed between two commits.
///
/// Returned paths are relative to `root`, not to the git repository root.
/// Paths outside of `root` are filtered out.
fn changed_files_since(root: &Path, old_hash: &str) -> Result<Vec<PathBuf>> {
    let files = diff_tree_paths(root, old_hash, "HEAD", None)
        .ok()
        .flatten()
        .unwrap_or_default();
    let mut result: Vec<PathBuf> = files.into_iter().collect();
    result.sort();
    result.dedup();
    Ok(result)
}

/// Root-relative paths changed between two commits (`git diff-tree -z`).
/// `Ok(None)` if git fails (e.g. `old` is no longer a known object).
fn diff_tree_paths(
    root: &Path,
    old: &str,
    new: &str,
    deadline: Option<Instant>,
) -> std::result::Result<Option<Vec<PathBuf>>, TimedOut> {
    let Some(prefix) = git_prefix(root) else {
        return Ok(None);
    };
    let out = run_git(
        root,
        &[
            "diff-tree",
            "-r",
            "-z",
            "--name-only",
            "--no-commit-id",
            old,
            new,
        ],
        deadline,
    )?;
    Ok(out.map(|o| {
        split_nul(&o)
            .filter_map(|p| convert_git_bytes(&prefix, p))
            .collect()
    }))
}

/// Save index metadata (call after build_index).
///
/// `build_started` is the time the build began walking the tree; together
/// with the current time it bounds the window in which a file could have
/// changed without its mtime differing from the indexed one.
pub fn save_meta(
    root: &Path,
    index_path: &Path,
    build_started: Option<std::time::SystemTime>,
) -> Result<()> {
    // Walked after the build: a directory created or filled meanwhile has an
    // mtime inside the build window and is re-listed by the next probe.
    let dirs = extra_dirs(root, index_path);
    let verified = build_started.map(|s| (unix_nanos(s), unix_nanos(std::time::SystemTime::now())));
    // Only spawn git inside a work tree (cheap filesystem check first).
    let hash = find_toplevel_fs(root).and_then(|_| current_commit_hash(root));
    IndexMeta::save(index_path, hash.as_deref(), verified, &dirs)
}

/// Directories the builder walks (same ignore rules) that contain no
/// indexed file below them, in index form, sorted. These are invisible to
/// [`indexed_dirs`], so without them the probe would never notice a file
/// created in, say, an empty `src/new/` or an `assets/` holding only
/// binaries.
fn extra_dirs(root: &Path, index_path: &Path) -> Vec<String> {
    let Ok(reader) = IndexReader::open(index_path) else {
        return Vec::new();
    };
    let known = indexed_dirs(&reader);
    let found: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(|e| {
            e.file_type().is_some_and(|t| t.is_dir())
                && !is_xgrep_internal(&e.file_name().to_string_lossy())
        })
        .build_parallel()
        .run(|| {
            let known = &known;
            struct Flush<'a>(Vec<String>, &'a std::sync::Mutex<Vec<String>>);
            impl Drop for Flush<'_> {
                fn drop(&mut self) {
                    if let Ok(mut out) = self.1.lock() {
                        out.append(&mut self.0);
                    }
                }
            }
            let mut sink = Flush(Vec::new(), &found);
            Box::new(move |entry| {
                let Ok(entry) = entry else {
                    return ignore::WalkState::Continue;
                };
                // Paths the index cannot store (non-UTF-8, line breaks) are
                // skipped, like the builder skips such files.
                if let Some(rel) = entry
                    .path()
                    .strip_prefix(root)
                    .ok()
                    .and_then(Path::to_str)
                    .filter(|r| !r.is_empty() && !r.contains(['\n', '\r']))
                {
                    let key = index_key(Path::new(rel));
                    if !known.contains(key.as_str()) {
                        sink.0.push(key);
                    }
                }
                ignore::WalkState::Continue
            })
        });
    let mut dirs = found.into_inner().unwrap_or_else(|e| e.into_inner());
    dirs.sort_unstable();
    dirs
}

/// Check index freshness and return changed file list (does not rebuild).
///
/// Detects modified, deleted, and new (including untracked) files using the
/// same probe as the default search overlay.
pub fn check_index_status(root: &Path, index_path: &Path) -> Result<IndexStatus> {
    // Missing, or in an older format version (rebuilt on next use).
    if !index_path.exists() || crate::index::reader::needs_rebuild(index_path) {
        return Ok(IndexStatus::NeedsFullBuild);
    }
    let reader = IndexReader::open(index_path)?;
    let overlay = compute_overlay(root, index_path, &reader, None).unwrap_or_default();
    if overlay.changed.is_empty() {
        Ok(IndexStatus::Fresh)
    } else {
        Ok(IndexStatus::Stale {
            changed_files: overlay.changed,
        })
    }
}

// ---------------------------------------------------------------------------
// Freshness overlay
// ---------------------------------------------------------------------------

/// Upper bound on files collected while walking newly created directories.
/// Beyond this the overlay reports `truncated`.
const NEW_FILE_WALK_LIMIT: usize = 10_000;

/// Result of the cheap freshness probe run before index searches.
#[derive(Debug, Default, Clone)]
pub(crate) struct Overlay {
    /// Root-relative paths whose index entries are stale: modified, deleted,
    /// or new files the index does not know about. Sorted and deduplicated.
    pub changed: Vec<PathBuf>,
    /// True when detection stopped early; `changed` is then a lower bound.
    pub truncated: bool,
    /// True when the index metadata is incomplete (old format without a
    /// build window), so a background rebuild is worthwhile even if nothing
    /// changed.
    pub meta_outdated: bool,
}

/// Window during which the index was built (falls back to the index mtime).
fn verified_window(meta: &IndexMeta, index_path: &Path) -> Window {
    meta.verified_at.unwrap_or_else(|| {
        let m = fs::metadata(index_path)
            .and_then(|m| m.modified())
            .map(unix_nanos)
            .unwrap_or(0);
        (m, m)
    })
}

/// Determine which files differ from the index, without reading contents.
///
/// Every indexed file is stat'ed and compared with its index entry (size,
/// mtime, and on unix also ctime against the build window, which catches
/// a file replaced by `mv` that kept an old mtime). Every directory that
/// contains indexed files is stat'ed too, and directories modified since the
/// build are re-listed with the builder's ignore rules to find new files.
///
/// The same probe is used inside git work trees: it needs no child process
/// (a `git status` spawn alone costs more than the whole scan on a typical
/// repository, and 300+ ms on WSL `/mnt/c`), it sees exactly the files the
/// builder indexes, and it handles commits, checkouts and reverts through
/// the files' own timestamps.
///
/// Stats run on a few scoped threads, relative to an open handle on the
/// root, so a 9k-file tree is checked in about a millisecond.
///
/// Returns `None` if the probe did not finish before `deadline`; with
/// `deadline: None` it always completes.
pub(crate) fn compute_overlay(
    root: &Path,
    index_path: &Path,
    reader: &IndexReader,
    deadline: Option<Instant>,
) -> Option<Overlay> {
    let meta = IndexMeta::load(index_path).unwrap_or_default();
    let meta_outdated = meta.verified_at.is_none();
    let window = verified_window(&meta, index_path);
    let mut ov = stat_scan(root, reader, &meta.dirs, window, deadline).ok()?;
    ov.meta_outdated = meta_outdated;
    // The index and its sidecars (meta, cache, stamps) may live inside the
    // root (e.g. tests, custom layouts); they are never source files.
    ov.changed
        .retain(|rel| !is_index_sidecar(root, index_path, rel));
    Some(ov)
}

// --- persisted probe result ------------------------------------------------

/// First line of a `<index>.probe` file.
const PROBE_CACHE_MAGIC: &str = "xgrep-probe 1";

fn probe_cache_path(index_path: &Path) -> PathBuf {
    index_path.with_extension("probe")
}

/// Identity of the index build a probe compared against: mtime (ns) and
/// size of the index file, and mtime of its meta file (rewritten after
/// every build, including no-op ones).
fn index_identity(index_path: &Path) -> Option<(u128, u64, u128)> {
    let idx = fs::metadata(index_path).ok()?;
    let meta = fs::metadata(IndexMeta::path_for(index_path)).ok();
    Some((
        idx.modified().map(unix_nanos).unwrap_or(0),
        idx.len(),
        meta.and_then(|m| m.modified().ok())
            .map(unix_nanos)
            .unwrap_or(0),
    ))
}

fn load_probe_cache(
    index_path: &Path,
    identity: (u128, u64, u128),
    ttl: std::time::Duration,
) -> Option<Overlay> {
    let content = fs::read_to_string(probe_cache_path(index_path)).ok()?;
    let mut lines = content.lines();
    if lines.next()? != PROBE_CACHE_MAGIC {
        return None;
    }
    let f: Vec<u128> = lines
        .next()?
        .split_whitespace()
        .map(|v| v.parse().ok())
        .collect::<Option<_>>()?;
    let [start, idx_mtime, idx_size, meta_mtime, truncated, meta_outdated] = f[..] else {
        return None;
    };
    if (idx_mtime, idx_size as u64, meta_mtime) != identity {
        return None;
    }
    let now = unix_nanos(std::time::SystemTime::now());
    // `now < start`: the clock went backwards; do not trust the entry.
    if now < start || now - start >= ttl.as_nanos() {
        return None;
    }
    let changed: Vec<PathBuf> = lines.map(PathBuf::from).collect();
    if !changed.iter().all(|p| safefs::is_safe_rel(p)) {
        return None;
    }
    Some(Overlay {
        changed,
        truncated: truncated != 0,
        meta_outdated: meta_outdated != 0,
    })
}

fn save_probe_cache(index_path: &Path, identity: (u128, u64, u128), start: u128, ov: &Overlay) {
    use std::fmt::Write as _;
    let path = probe_cache_path(index_path);
    let mut out = String::new();
    let _ = write!(
        out,
        "{PROBE_CACHE_MAGIC}\n{start} {} {} {} {} {}\n",
        identity.0,
        identity.1,
        identity.2,
        u8::from(ov.truncated),
        u8::from(ov.meta_outdated)
    );
    for p in &ov.changed {
        match p.to_str() {
            Some(s) if !s.contains('\n') => {
                out.push_str(s);
                out.push('\n');
            }
            // Not representable: do not cache this result.
            _ => {
                let _ = fs::remove_file(&path);
                return;
            }
        }
    }
    let _ = safefs::write_atomic(&path, out.as_bytes());
}

/// [`compute_overlay`] with a persisted result reused for `ttl`.
///
/// The probe stats every indexed file, which costs ~1 us per file (more on
/// slow filesystems); back-to-back searches (scripts, an agent issuing
/// several searches in one turn) reuse the previous result instead. The
/// entry is valid only while the index and its meta file are unchanged (any
/// rebuild invalidates it) and for at most `ttl` after the probe *started*,
/// so an edit is picked up by the first search at least `ttl` after it.
/// `ttl` of zero disables the cache.
///
/// Returns the overlay and whether it came from the cache.
pub(crate) fn compute_overlay_cached(
    root: &Path,
    index_path: &Path,
    reader: &IndexReader,
    deadline: Option<Instant>,
    ttl: std::time::Duration,
) -> Option<(Overlay, bool)> {
    let identity = if ttl.is_zero() {
        None
    } else {
        index_identity(index_path)
    };
    if let Some(id) = identity {
        if let Some(ov) = load_probe_cache(index_path, id, ttl) {
            return Some((ov, true));
        }
    }
    let start = unix_nanos(std::time::SystemTime::now());
    let ov = compute_overlay(root, index_path, reader, deadline)?;
    if let Some(id) = identity {
        save_probe_cache(index_path, id, start, &ov);
    }
    Some((ov, false))
}

/// Overlay for an explicit set of root-relative paths reported by a file
/// watcher (see [`crate::watch`]).
///
/// - existing regular file: rescanned directly;
/// - existing directory (created or renamed in): walked with the builder's
///   ignore rules, every file in it rescanned, and indexed files under it
///   excluded from the index scan;
/// - missing path (deleted file or directory): it and every indexed file
///   under it are excluded from the index scan.
pub(crate) fn overlay_from_paths(
    root: &Path,
    index_path: &Path,
    reader: &IndexReader,
    paths: impl IntoIterator<Item = PathBuf>,
) -> Overlay {
    let mut changed = Vec::new();
    let mut prefixes: Vec<String> = Vec::new();
    let mut truncated = false;
    for rel in paths {
        match fs::symlink_metadata(root.join(&rel)) {
            Ok(md) if md.is_file() => changed.push(rel),
            Ok(md) if md.is_dir() => {
                let (files, hit_limit) = walk_subtree(root, &rel, NEW_FILE_WALK_LIMIT);
                truncated |= hit_limit;
                changed.extend(files);
                prefixes.push(index_key(&rel));
            }
            _ => {
                prefixes.push(index_key(&rel));
                changed.push(rel);
            }
        }
    }
    if !prefixes.is_empty() {
        let sep = std::path::MAIN_SEPARATOR;
        for fid in 0..reader.file_count() {
            let p = reader.file_path(fid);
            let under = prefixes.iter().any(|pre| {
                p.strip_prefix(pre.as_str())
                    .is_some_and(|rest| rest.starts_with(sep) || rest.starts_with('/'))
            });
            if under {
                changed.push(PathBuf::from(p));
            }
        }
    }
    changed.retain(|rel| !is_index_sidecar(root, index_path, rel));
    changed.sort();
    changed.dedup();
    Overlay {
        changed,
        truncated,
        meta_outdated: false,
    }
}

/// All files under `root/rel` that the builder would index (ignore rules
/// of `rel` and its parents apply). Returns root-relative paths and whether
/// `limit` was hit.
fn walk_subtree(root: &Path, rel: &Path, limit: usize) -> (Vec<PathBuf>, bool) {
    let walker = WalkBuilder::new(root.join(rel))
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(|e| !is_xgrep_internal(&e.file_name().to_string_lossy()))
        .build();
    let mut out = Vec::new();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if let Ok(r) = entry.path().strip_prefix(root) {
            out.push(r.to_path_buf());
            if out.len() >= limit {
                return (out, true);
            }
        }
    }
    (out, false)
}

/// True if `root/rel` is the index file or one of its sidecars
/// (`<stem>.meta`, `<stem>.cache`, `<stem>.rebuild`, ...).
pub(crate) fn is_index_sidecar(root: &Path, index_path: &Path, rel: &Path) -> bool {
    let full = root.join(rel);
    if full.parent() != index_path.parent() {
        return false;
    }
    let (Some(stem), Some(name)) = (index_path.file_stem(), full.file_name()) else {
        return false;
    };
    let (stem, name) = (stem.to_string_lossy(), name.to_string_lossy());
    name == stem
        || name
            .strip_prefix(stem.as_ref())
            .is_some_and(|r| r.starts_with('.'))
}

/// Lookup key matching how the builder stores paths in the index.
pub(crate) fn index_key(rel: &Path) -> String {
    crate::index::rel_key(rel)
}

// --- stat helpers ----------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Dir,
    Other,
}

/// The parts of a `stat` the probe needs.
#[derive(Debug, Clone, Copy)]
struct ProbeStat {
    kind: Kind,
    size: u64,
    mtime_ns: u128,
    /// Inode change time (unix only).
    ctime_ns: Option<u128>,
}

/// Stats paths relative to the root.
///
/// On unix this holds an open directory descriptor and uses `fstatat`, so
/// the kernel resolves only the root-relative part of each path.
struct StatRoot {
    #[cfg(unix)]
    fd: libc::c_int,
    #[cfg(not(unix))]
    root: PathBuf,
}

#[cfg(unix)]
impl StatRoot {
    fn open(root: &Path) -> Option<Self> {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(root.as_os_str().as_bytes()).ok()?;
        // SAFETY: `c` is a valid NUL-terminated path; the returned fd is
        // owned by `StatRoot` and closed in `Drop`.
        let fd = unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        (fd >= 0).then_some(Self { fd })
    }

    /// `buf` is per-thread scratch space for the NUL-terminated path.
    // `mode_t`, `time_t` etc. differ in width between unix targets.
    #[allow(clippy::unnecessary_cast)]
    fn stat(&self, rel: &str, buf: &mut Vec<u8>) -> Option<ProbeStat> {
        buf.clear();
        if rel.is_empty() {
            buf.push(b'.');
        } else {
            buf.extend_from_slice(rel.as_bytes());
        }
        if buf.contains(&0) {
            return None;
        }
        buf.push(0);
        // SAFETY: `buf` is NUL-terminated and `st` is a properly sized,
        // writable stat buffer; `self.fd` is an open directory descriptor.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::fstatat(
                self.fd,
                buf.as_ptr() as *const libc::c_char,
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc != 0 {
            return None;
        }
        let fmt = st.st_mode as u32 & libc::S_IFMT as u32;
        let kind = if fmt == libc::S_IFREG as u32 {
            Kind::File
        } else if fmt == libc::S_IFDIR as u32 {
            Kind::Dir
        } else {
            Kind::Other
        };
        let ts = |s: i64, ns: i64| -> u128 {
            if s < 0 {
                0
            } else {
                s as u128 * NANOS_PER_SEC + ns.clamp(0, 999_999_999) as u128
            }
        };
        Some(ProbeStat {
            kind,
            size: st.st_size as u64,
            mtime_ns: ts(st.st_mtime as i64, st.st_mtime_nsec as i64),
            ctime_ns: Some(ts(st.st_ctime as i64, st.st_ctime_nsec as i64)),
        })
    }
}

#[cfg(unix)]
impl Drop for StatRoot {
    fn drop(&mut self) {
        // SAFETY: `fd` was opened by `StatRoot::open` and is closed once.
        unsafe {
            libc::close(self.fd);
        }
    }
}

#[cfg(not(unix))]
impl StatRoot {
    fn open(root: &Path) -> Option<Self> {
        Some(Self {
            root: root.to_path_buf(),
        })
    }

    fn stat(&self, rel: &str, _buf: &mut Vec<u8>) -> Option<ProbeStat> {
        let md = fs::symlink_metadata(self.root.join(rel)).ok()?;
        let kind = if md.is_file() {
            Kind::File
        } else if md.is_dir() {
            Kind::Dir
        } else {
            Kind::Other
        };
        Some(ProbeStat {
            kind,
            size: md.len(),
            mtime_ns: md.modified().map(unix_nanos).unwrap_or(0),
            ctime_ns: None,
        })
    }
}

/// True if `ns` is at or after `start` (second granularity for timestamps
/// without a sub-second part, see [`at_or_after`]).
fn ns_at_or_after(ns: u128, start: u128) -> bool {
    if ns % NANOS_PER_SEC == 0 {
        ns / NANOS_PER_SEC >= start / NANOS_PER_SEC
    } else {
        ns >= start
    }
}

/// Compare a stored index mtime (seconds, millis or nanos) with `ns`.
fn mtime_matches_ns(stored: u64, ns: u128) -> bool {
    let stored = stored as u128;
    stored == ns / NANOS_PER_SEC || stored == ns / 1_000_000 || stored == ns
}

/// True if the on-disk file (`st`, `None` = missing) no longer matches its
/// index entry, or cannot be proven to match.
fn entry_is_stale(st: Option<ProbeStat>, entry: &FileEntry, window: Window) -> bool {
    let Some(st) = st else {
        return true; // deleted
    };
    if st.kind != Kind::File || st.size != entry.size || !mtime_matches_ns(entry.mtime, st.mtime_ns)
    {
        return true;
    }
    // Racy entry: modified while the build was reading files, so an
    // in-place edit may share the stored mtime.
    if ns_at_or_after(st.mtime_ns, window.0)
        && st.mtime_ns / NANOS_PER_SEC <= window.1 / NANOS_PER_SEC
    {
        return true;
    }
    // Inode changed after the build started (e.g. replaced via rename by a
    // file that kept an older mtime and the same size).
    st.ctime_ns.is_some_and(|c| ns_at_or_after(c, window.0))
}

/// Number of threads for stat'ing `n` paths.
fn probe_threads(n: usize, slow_fs: bool) -> usize {
    static AVAIL: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let avail = *AVAIL.get_or_init(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    });
    // Each thread costs a spawn (~20-50 us); give it enough work. On
    // network/9p filesystems a stat is a round trip, so fan out wider.
    let (per_thread, max) = if slow_fs { (8, 32) } else { (1024, 16) };
    n.div_ceil(per_thread).clamp(1, max).min(avail.max(1))
}

/// Run `f` for every index in `0..n` on `threads` scoped threads. Work is
/// claimed in contiguous chunks; each thread gets its own state from
/// `init`. Returns everything pushed to the output vectors, in no
/// particular order.
fn par_scan<T: Send, L>(
    n: usize,
    threads: usize,
    chunk: usize,
    deadline: Option<Instant>,
    init: impl Fn() -> L + Sync,
    f: impl Fn(usize, &mut L, &mut Vec<T>) + Sync,
) -> std::result::Result<Vec<T>, TimedOut> {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let next = AtomicUsize::new(0);
    let timed_out = AtomicBool::new(false);
    let work = || {
        let mut local = init();
        let mut out = Vec::new();
        loop {
            let start = next.fetch_add(chunk, Ordering::Relaxed);
            if start >= n || timed_out.load(Ordering::Relaxed) {
                break;
            }
            if check_deadline(deadline).is_err() {
                timed_out.store(true, Ordering::Relaxed);
                break;
            }
            for i in start..(start + chunk).min(n) {
                f(i, &mut local, &mut out);
            }
        }
        out
    };
    let out = if threads <= 1 {
        work()
    } else {
        std::thread::scope(|s| {
            let handles: Vec<_> = (1..threads).map(|_| s.spawn(work)).collect();
            let mut all = work();
            for h in handles {
                match h.join() {
                    Ok(v) => all.extend(v),
                    Err(e) => std::panic::resume_unwind(e),
                }
            }
            all
        })
    };
    if timed_out.load(Ordering::Relaxed) {
        Err(TimedOut)
    } else {
        Ok(out)
    }
}

/// Parent of an index path ("" for top-level files), without allocating.
fn parent_str(p: &str) -> &str {
    let is_sep = |c: char| c == '/' || c == std::path::MAIN_SEPARATOR;
    p.rfind(is_sep).map_or("", |i| &p[..i])
}

/// Directories known to the index: parents of indexed files and their
/// ancestors ("" is the root). Names are borrowed from the mmap.
fn indexed_dirs(reader: &IndexReader) -> HashSet<&str> {
    let mut seen: HashSet<&str> = HashSet::new();
    seen.insert("");
    let mut prev: Option<&str> = None;
    for fid in 0..reader.file_count() {
        let parent = parent_str(reader.file_path(fid));
        // Paths are sorted, so siblings are adjacent.
        if prev == Some(parent) {
            continue;
        }
        prev = Some(parent);
        let mut d = parent;
        while !d.is_empty() && seen.insert(d) {
            d = parent_str(d);
        }
    }
    seen
}

/// Per-thread state of [`stat_scan`].
struct ScanLocal<'a> {
    buf: Vec<u8>,
    /// Directories this thread already stat'ed.
    dirs_seen: HashSet<&'a str>,
}

/// Freshness probe: stat every indexed file, every directory containing
/// indexed files and the `extra_dirs` recorded at build time (walked
/// directories without indexed files); re-list directories modified since
/// the build.
fn stat_scan(
    root: &Path,
    reader: &IndexReader,
    extra_dirs: &[String],
    window: Window,
    deadline: Option<Instant>,
) -> std::result::Result<Overlay, TimedOut> {
    let Some(sr) = StatRoot::open(root) else {
        // Root is gone or unreadable: nothing in the index can be trusted.
        let changed = (0..reader.file_count())
            .map(|fid| PathBuf::from(reader.file_path(fid)))
            .collect();
        return Ok(Overlay {
            changed,
            ..Default::default()
        });
    };
    let n = reader.file_count() as usize;
    let total = n + extra_dirs.len();
    let slow = crate::fsinfo::is_remote_fs(root);
    let threads = probe_threads(total, slow);
    let chunk = if slow { 4 } else { 256 };

    // Hits: `Ok(fid)` = stale indexed file, `Err(dir)` = directory
    // modified since the build. Directories are discovered on the fly from
    // the (sorted) file paths; each thread stats a directory at most once.
    // Indices past the files are the recorded extra directories (their
    // ancestors are indexed or extra directories themselves).
    let hits = par_scan(
        total,
        threads,
        chunk,
        deadline,
        || ScanLocal {
            buf: Vec::with_capacity(256),
            dirs_seen: HashSet::new(),
        },
        |i, local, out| {
            if let Some(d) = i.checked_sub(n).and_then(|j| extra_dirs.get(j)) {
                if let Some(st) = sr.stat(d, &mut local.buf) {
                    if st.kind == Kind::Dir && ns_at_or_after(st.mtime_ns, window.0) {
                        out.push(Err(d.as_str()));
                    }
                }
                return;
            }
            let fid = i as u32;
            let path = reader.file_path(fid);
            if let Some(entry) = reader.file_entry(fid) {
                if entry_is_stale(sr.stat(path, &mut local.buf), &entry, window) {
                    out.push(Ok(fid));
                }
            }
            let mut d = parent_str(path);
            loop {
                if !local.dirs_seen.insert(d) {
                    break;
                }
                if let Some(st) = sr.stat(d, &mut local.buf) {
                    if st.kind == Kind::Dir && ns_at_or_after(st.mtime_ns, window.0) {
                        out.push(Err(d));
                    }
                }
                if d.is_empty() {
                    break;
                }
                d = parent_str(d);
            }
        },
    )?;

    let mut changed = Vec::new();
    let mut changed_dirs: HashSet<String> = HashSet::new();
    for h in hits {
        match h {
            Ok(fid) => changed.push(PathBuf::from(reader.file_path(fid))),
            Err(d) => {
                changed_dirs.insert(d.to_string());
            }
        }
    }
    check_deadline(deadline)?;

    let mut truncated = false;
    if !changed_dirs.is_empty() {
        let indexed: HashSet<&str> = (0..reader.file_count())
            .map(|fid| reader.file_path(fid))
            .collect();
        let known: HashSet<String> = indexed_dirs(reader)
            .into_iter()
            .chain(extra_dirs.iter().map(String::as_str))
            .map(str::to_string)
            .collect();
        let (new_files, hit_limit) = walk_new_files(root, known, changed_dirs, &indexed, deadline)?;
        truncated = hit_limit;
        changed.extend(new_files);
    }
    changed.sort();
    changed.dedup();
    Ok(Overlay {
        changed,
        truncated,
        meta_outdated: false,
    })
}

/// Walk only the parts of the tree that can contain files missing from the
/// index: directories whose mtime moved since the build (non-recursively)
/// and directories unknown to the index (recursively). Uses the same
/// ignore rules as the builder.
fn walk_new_files(
    root: &Path,
    known: HashSet<String>,
    changed_dirs: HashSet<String>,
    indexed: &HashSet<&str>,
    deadline: Option<Instant>,
) -> std::result::Result<(Vec<PathBuf>, bool), TimedOut> {
    // Ancestors of changed directories must be traversed to reach them.
    let mut ancestors: HashSet<String> = HashSet::new();
    for d in &changed_dirs {
        let mut cur = Path::new(d.as_str()).parent();
        while let Some(a) = cur {
            if !ancestors.insert(index_key(a)) {
                break;
            }
            cur = a.parent();
        }
    }
    let known = std::sync::Arc::new(known);
    let changed_dirs = std::sync::Arc::new(changed_dirs);
    let root_owned = root.to_path_buf();

    let walker = WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(move |e| {
            if is_xgrep_internal(&e.file_name().to_string_lossy()) {
                return false;
            }
            let Ok(rel) = e.path().strip_prefix(&root_owned) else {
                return true;
            };
            let rel_s = index_key(rel);
            if rel_s.is_empty() {
                return true;
            }
            let is_dir = e.file_type().is_some_and(|t| t.is_dir());
            if is_dir && known.contains(rel_s.as_str()) {
                return ancestors.contains(rel_s.as_str()) || changed_dirs.contains(rel_s.as_str());
            }
            // Unknown dir or a file: include only if its nearest known
            // ancestor directory was modified since the build.
            let mut cur = rel.parent();
            while let Some(a) = cur {
                let a_s = index_key(a);
                if known.contains(a_s.as_str()) {
                    return changed_dirs.contains(a_s.as_str());
                }
                cur = a.parent();
            }
            false
        })
        .build();

    let mut out = Vec::new();
    for entry in walker.flatten() {
        check_deadline(deadline)?;
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        if indexed.contains(index_key(rel).as_str()) {
            continue;
        }
        out.push(rel.to_path_buf());
        if out.len() >= NEW_FILE_WALK_LIMIT {
            return Ok((out, true));
        }
    }
    Ok((out, false))
}

/// Build the index with cache (incremental update).
#[allow(dead_code)]
fn build_with_cache(root: &Path, index_path: &Path) -> Result<()> {
    let cache_path = crate::index::cache::cache_path_for(index_path);
    let _ = crate::index::builder::build_index_with_cache(root, index_path, Some(&cache_path))?;
    Ok(())
}

/// Check if the index is up-to-date and rebuild if necessary.
#[allow(dead_code)]
pub fn ensure_fresh_index(root: &Path, index_path: &Path) -> Result<()> {
    let started = Some(std::time::SystemTime::now());
    if !index_path.exists() {
        // Index does not exist: full build (with cache creation)
        eprintln!("[indexing...]");
        build_with_cache(root, index_path)?;
        save_meta(root, index_path, started)?;
        eprintln!("[done]");
        return Ok(());
    }

    // Index exists: check if update is needed
    let meta = IndexMeta::load(index_path);
    let current_hash = current_commit_hash(root);

    match (&meta, &current_hash) {
        (Some(m), Some(curr)) if m.commit_hash.as_deref() == Some(curr.as_str()) => {
            // Same commit. Check for uncommitted changes
            let uncommitted = collect_uncommitted_changes(root)?;
            if uncommitted.is_empty() {
                // No changes, index is up-to-date
                return Ok(());
            }
            // Uncommitted changes found, incremental rebuild with cache
            eprintln!("[updating index...]");
            build_with_cache(root, index_path)?;
            save_meta(root, index_path, started)?;
            eprintln!("[done]");
        }
        (Some(m), Some(_)) => {
            // Different commit
            let old_hash = m.commit_hash.as_deref().unwrap_or("");
            let changed = changed_files_since(root, old_hash)?;

            if changed.is_empty() {
                // No file changes (e.g. merge commit). The index was not
                // rebuilt, so do not claim a new verification window.
                save_meta(root, index_path, None)?;
                return Ok(());
            }

            // Changes detected, incremental rebuild with cache
            eprintln!("[updating index ({} files changed)...]", changed.len());
            build_with_cache(root, index_path)?;
            save_meta(root, index_path, started)?;
            eprintln!("[done]");
        }
        _ => {
            // Non-git repo or no metadata: determine freshness via mtime
            let index_mtime = fs::metadata(index_path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);

            let needs_rebuild = match newest_file_mtime(root, index_path) {
                Some(newest) => index_mtime < newest,
                None => true,
            };

            if needs_rebuild {
                eprintln!("[updating index...]");
                build_with_cache(root, index_path)?;
                save_meta(root, index_path, started)?;
                eprintln!("[done]");
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn init_git_repo(dir: &Path) {
        git_cmd().args(["init"]).current_dir(dir).output().unwrap();
        git_cmd()
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir)
            .output()
            .unwrap();
        git_cmd()
            .args(["config", "user.name", "test"])
            .current_dir(dir)
            .output()
            .unwrap();
    }

    #[test]
    fn test_ensure_fresh_index_creates_new() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("hello.txt"), "hello world").unwrap();
        let index_path = root.join("test.xgrep");

        ensure_fresh_index(root, &index_path).unwrap();

        assert!(index_path.exists());
    }

    #[test]
    fn test_ensure_fresh_index_no_rebuild_if_unchanged() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        init_git_repo(root);
        // Add index files to .gitignore so git status does not detect them
        fs::write(root.join(".gitignore"), "*.xgrep\n*.meta\n*.cache\n").unwrap();
        fs::write(root.join("hello.txt"), "hello world").unwrap();
        git_cmd()
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        git_cmd()
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();

        let index_path = root.join("test.xgrep");
        ensure_fresh_index(root, &index_path).unwrap();

        let mtime1 = fs::metadata(&index_path).unwrap().modified().unwrap();

        // Re-run without changes
        std::thread::sleep(std::time::Duration::from_millis(100));
        ensure_fresh_index(root, &index_path).unwrap();

        let mtime2 = fs::metadata(&index_path).unwrap().modified().unwrap();
        assert_eq!(mtime1, mtime2); // Index was not rebuilt
    }

    #[test]
    fn test_ensure_fresh_index_rebuilds_after_commit() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        init_git_repo(root);
        fs::write(root.join("hello.txt"), "hello world").unwrap();
        git_cmd()
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        git_cmd()
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();

        let index_path = root.join("test.xgrep");
        ensure_fresh_index(root, &index_path).unwrap();

        let mtime1 = fs::metadata(&index_path).unwrap().modified().unwrap();

        // Create a new commit
        std::thread::sleep(std::time::Duration::from_millis(100));
        fs::write(root.join("new_file.txt"), "new content").unwrap();
        git_cmd()
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        git_cmd()
            .args(["commit", "-m", "add file"])
            .current_dir(root)
            .output()
            .unwrap();

        ensure_fresh_index(root, &index_path).unwrap();

        let mtime2 = fs::metadata(&index_path).unwrap().modified().unwrap();
        assert_ne!(mtime1, mtime2); // Index was rebuilt
    }

    #[test]
    fn test_meta_save_load() {
        let dir = tempdir().unwrap();
        let index_path = dir.path().join("test.xgrep");
        fs::write(&index_path, "dummy").unwrap();

        IndexMeta::save(
            &index_path,
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            None,
            &[],
        )
        .unwrap();
        let meta = IndexMeta::load(&index_path).unwrap();
        assert_eq!(
            meta.commit_hash,
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string())
        );
        assert_eq!(meta.verified_at, None);

        let dirs = vec!["assets".to_string(), "src/empty dir".to_string()];
        IndexMeta::save(
            &index_path,
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            Some((10, 12)),
            &dirs,
        )
        .unwrap();
        let meta = IndexMeta::load(&index_path).unwrap();
        assert_eq!(
            meta.commit_hash,
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string())
        );
        assert_eq!(meta.verified_at, Some((10, 12)));
        assert_eq!(meta.dirs, dirs);

        // Lines written by older versions are ignored.
        fs::write(
            index_path.with_extension("meta"),
            "cccccccccccccccccccccccccccccccccccccccc\nverified-ns 5 6\ndirty a.txt\ndirty-overflow\n",
        )
        .unwrap();
        let meta = IndexMeta::load(&index_path).unwrap();
        assert_eq!(
            meta.commit_hash,
            Some("cccccccccccccccccccccccccccccccccccccccc".to_string())
        );
        assert_eq!(meta.verified_at, Some((5, 6)));

        // A malformed commit id or a dir escaping the root is dropped.
        fs::write(
            index_path.with_extension("meta"),
            "--output=x
dir ../outside
dir /abs
dir ok
",
        )
        .unwrap();
        let meta = IndexMeta::load(&index_path).unwrap();
        assert_eq!(meta.commit_hash, None);
        assert_eq!(meta.dirs, vec!["ok".to_string()]);
    }

    #[test]
    fn test_meta_load_legacy_single_line() {
        let dir = tempdir().unwrap();
        let index_path = dir.path().join("test.xgrep");
        fs::write(
            index_path.with_extension("meta"),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .unwrap();
        let meta = IndexMeta::load(&index_path).unwrap();
        assert_eq!(
            meta.commit_hash,
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string())
        );
        assert_eq!(meta.verified_at, None);
    }

    #[test]
    fn test_meta_load_missing() {
        let dir = tempdir().unwrap();
        let index_path = dir.path().join("nonexistent.xgrep");
        assert!(IndexMeta::load(&index_path).is_none());
    }

    #[test]
    fn test_ensure_fresh_index_rebuilds_on_uncommitted_changes() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        init_git_repo(root);
        fs::write(root.join("hello.txt"), "hello world").unwrap();
        git_cmd()
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        git_cmd()
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();

        let index_path = root.join("test.xgrep");
        ensure_fresh_index(root, &index_path).unwrap();

        let mtime1 = fs::metadata(&index_path).unwrap().modified().unwrap();

        // Make uncommitted changes (same commit, dirty working tree)
        std::thread::sleep(std::time::Duration::from_millis(100));
        fs::write(root.join("hello.txt"), "changed content").unwrap();

        ensure_fresh_index(root, &index_path).unwrap();

        let mtime2 = fs::metadata(&index_path).unwrap().modified().unwrap();
        assert_ne!(mtime1, mtime2); // Index was rebuilt due to uncommitted changes
    }

    #[test]
    fn test_ensure_fresh_index_rebuilds_on_new_untracked_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        init_git_repo(root);
        fs::write(root.join("hello.txt"), "hello world").unwrap();
        git_cmd()
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        git_cmd()
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();

        let index_path = root.join("test.xgrep");
        ensure_fresh_index(root, &index_path).unwrap();

        let mtime1 = fs::metadata(&index_path).unwrap().modified().unwrap();

        // Add a new untracked file
        std::thread::sleep(std::time::Duration::from_millis(100));
        fs::write(root.join("new_file.txt"), "new content").unwrap();

        ensure_fresh_index(root, &index_path).unwrap();

        let mtime2 = fs::metadata(&index_path).unwrap().modified().unwrap();
        assert_ne!(mtime1, mtime2); // Index was rebuilt due to new untracked file
    }

    #[test]
    fn test_parse_status_v2_entries() {
        let out = b"# branch.oid 0123abcd\0# branch.head main\0\
1 .M N... 100644 100644 100644 aaa aaa hello.txt\0\
1 .M N... 100644 100644 100644 aaa aaa file with spaces.txt\0\
2 R. N... 100644 100644 100644 aaa aaa R100 new.txt\0old.txt\0\
u UU N... 100644 100644 100644 100644 aaa bbb ccc conflict.rs\0\
? sub/untracked.txt\0";
        let snap = parse_status_v2(out);
        let paths: Vec<String> = snap
            .paths
            .iter()
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect();
        assert!(paths.contains(&"hello.txt".to_string()));
        assert!(paths.contains(&"file with spaces.txt".to_string()));
        assert!(paths.contains(&"new.txt".to_string()));
        assert!(paths.contains(&"old.txt".to_string()));
        assert!(paths.contains(&"conflict.rs".to_string()));
        assert!(paths.contains(&"sub/untracked.txt".to_string()));
        assert_eq!(paths.len(), 6);
    }

    #[test]
    fn test_parse_status_v2_initial_and_empty() {
        let snap = parse_status_v2(b"# branch.oid (initial)\0# branch.head main\0");
        assert!(snap.paths.is_empty());
        assert!(parse_status_v2(b"").paths.is_empty());
        // Truncated record is ignored rather than mis-parsed.
        assert!(parse_status_v2(b"1 .M\0").paths.is_empty());
    }

    /// Regression: non-ASCII file names must not come back quoted/escaped
    /// (`"\346\227\245..."`), which made --fresh and status miss them.
    #[test]
    fn test_collect_uncommitted_changes_non_ascii() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        init_git_repo(root);
        fs::write(root.join("日本.txt"), "one").unwrap();
        git_cmd()
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        git_cmd()
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();
        fs::write(root.join("日本.txt"), "two").unwrap();
        fs::write(root.join("新規 ファイル.txt"), "new").unwrap();

        let changed = collect_uncommitted_changes(root).unwrap();
        assert!(changed.contains(&PathBuf::from("日本.txt")), "{changed:?}");
        assert!(
            changed.contains(&PathBuf::from("新規 ファイル.txt")),
            "{changed:?}"
        );
    }

    #[test]
    fn test_check_index_status_fresh() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        init_git_repo(root);
        fs::write(root.join(".gitignore"), "*.xgrep\n*.meta\n*.cache\n").unwrap();
        fs::write(root.join("hello.txt"), "hello world").unwrap();
        git_cmd()
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        git_cmd()
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();

        let index_path = root.join("test.xgrep");
        ensure_fresh_index(root, &index_path).unwrap();

        let status = check_index_status(root, &index_path).unwrap();
        assert!(matches!(status, IndexStatus::Fresh));
    }

    #[test]
    fn test_check_index_status_stale() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        init_git_repo(root);
        fs::write(root.join(".gitignore"), "*.xgrep\n*.meta\n*.cache\n").unwrap();
        fs::write(root.join("hello.txt"), "hello world").unwrap();
        git_cmd()
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        git_cmd()
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();

        let index_path = root.join("test.xgrep");
        ensure_fresh_index(root, &index_path).unwrap();

        // Modify a file
        fs::write(root.join("hello.txt"), "changed").unwrap();

        let status = check_index_status(root, &index_path).unwrap();
        match status {
            IndexStatus::Stale { changed_files } => {
                assert!(!changed_files.is_empty());
            }
            other => panic!("expected Stale, got {:?}", other),
        }
    }

    #[test]
    fn test_check_index_status_no_index() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let index_path = root.join("nonexistent.xgrep");

        let status = check_index_status(root, &index_path).unwrap();
        assert!(matches!(status, IndexStatus::NeedsFullBuild));
    }

    #[test]
    fn test_check_index_status_stale_new_untracked_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        init_git_repo(root);
        fs::write(root.join(".gitignore"), "*.xgrep\n*.meta\n*.cache\n").unwrap();
        fs::write(root.join("hello.txt"), "hello world").unwrap();
        git_cmd()
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        git_cmd()
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();

        let index_path = root.join("test.xgrep");
        ensure_fresh_index(root, &index_path).unwrap();

        // Add a new untracked file
        fs::write(root.join("new_file.txt"), "new content").unwrap();

        let status = check_index_status(root, &index_path).unwrap();
        // Untracked files are reported (they are what default searches
        // would otherwise silently miss).
        match status {
            IndexStatus::Stale { changed_files } => {
                assert_eq!(changed_files, vec![PathBuf::from("new_file.txt")]);
            }
            other => panic!("expected Stale, got {:?}", other),
        }
    }

    #[test]
    fn test_non_git_mtime_freshness() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello").unwrap();

        let index_path = root.join("index.xgrep");
        let started = std::time::SystemTime::now();
        crate::index::builder::build_index(root, &index_path).unwrap();
        save_meta(root, &index_path, Some(started)).unwrap();

        // Index was just built, should be Fresh
        let status = check_index_status(root, &index_path).unwrap();
        assert!(
            matches!(status, IndexStatus::Fresh),
            "expected Fresh, got {:?}",
            status
        );

        // Wait for filesystem timestamp to advance (Windows NTFS needs >1s)
        std::thread::sleep(std::time::Duration::from_secs(2));
        fs::write(root.join("b.txt"), "world").unwrap();

        // The new file is detected without a full rebuild
        let status = check_index_status(root, &index_path).unwrap();
        match status {
            IndexStatus::Stale { changed_files } => {
                assert_eq!(changed_files, vec![PathBuf::from("b.txt")]);
            }
            other => panic!("expected Stale, got {:?}", other),
        }
    }

    #[test]
    fn test_non_git_ensure_fresh_skips_rebuild_when_fresh() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello").unwrap();

        let index_path = root.join("test.xgrep");
        ensure_fresh_index(root, &index_path).unwrap();

        let mtime1 = fs::metadata(&index_path).unwrap().modified().unwrap();

        // Re-run without changes
        std::thread::sleep(std::time::Duration::from_millis(100));
        ensure_fresh_index(root, &index_path).unwrap();

        let mtime2 = fs::metadata(&index_path).unwrap().modified().unwrap();
        assert_eq!(mtime1, mtime2); // Not rebuilt
    }

    #[test]
    fn test_check_index_status_stale_after_commit() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        init_git_repo(root);
        fs::write(root.join(".gitignore"), "*.xgrep\n*.meta\n*.cache\n").unwrap();
        fs::write(root.join("hello.txt"), "hello world").unwrap();
        git_cmd()
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        git_cmd()
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();

        let index_path = root.join("test.xgrep");
        ensure_fresh_index(root, &index_path).unwrap();

        // New commit
        fs::write(root.join("new_file.txt"), "new content").unwrap();
        git_cmd()
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        git_cmd()
            .args(["commit", "-m", "add file"])
            .current_dir(root)
            .output()
            .unwrap();

        let status = check_index_status(root, &index_path).unwrap();
        match status {
            IndexStatus::Stale { changed_files } => {
                assert!(!changed_files.is_empty());
            }
            other => panic!("expected Stale, got {:?}", other),
        }
    }

    #[test]
    fn test_convert_git_bytes() {
        let prefix = PathBuf::from("sub/dir");
        assert_eq!(
            convert_git_bytes(&prefix, b"sub/dir/file.rs"),
            Some(PathBuf::from("file.rs"))
        );
        assert_eq!(
            convert_git_bytes(&prefix, b"sub/dir/nested/deep.rs"),
            Some(PathBuf::from("nested/deep.rs"))
        );
        // Outside root (sibling directory)
        assert!(convert_git_bytes(&prefix, b"other/file.rs").is_none());
        // Empty prefix (root == git toplevel): used as-is
        assert_eq!(
            convert_git_bytes(Path::new(""), "日本.txt".as_bytes()),
            Some(PathBuf::from("日本.txt"))
        );
    }

    #[test]
    fn test_collect_uncommitted_changes_subdir() {
        // Set up a git repo with a subdirectory structure
        let dir = tempdir().unwrap();
        let git_root = dir.path();
        init_git_repo(git_root);

        // Create subdirectory structure
        let sub_dir = git_root.join("sub").join("dir");
        fs::create_dir_all(&sub_dir).unwrap();
        fs::write(git_root.join(".gitignore"), "*.xgrep\n*.meta\n*.cache\n").unwrap();
        fs::write(sub_dir.join("file.rs"), "fn main() {}").unwrap();
        fs::write(git_root.join("root_file.txt"), "root").unwrap();

        git_cmd()
            .args(["add", "."])
            .current_dir(git_root)
            .output()
            .unwrap();
        git_cmd()
            .args(["commit", "-m", "init"])
            .current_dir(git_root)
            .output()
            .unwrap();

        // Modify a file in the subdirectory
        fs::write(sub_dir.join("file.rs"), "fn main() { changed }").unwrap();
        // Also modify a file outside the subdirectory
        fs::write(git_root.join("root_file.txt"), "changed root").unwrap();

        // Collect changes from the subdirectory (xgrep root = sub/dir)
        let changes = collect_uncommitted_changes(&sub_dir).unwrap();

        // Should contain file.rs (relative to sub/dir), NOT sub/dir/file.rs
        assert!(
            changes.contains(&PathBuf::from("file.rs")),
            "expected file.rs in changes, got: {:?}",
            changes
        );

        // Should NOT contain root_file.txt (outside of sub/dir)
        assert!(
            !changes.contains(&PathBuf::from("root_file.txt")),
            "root_file.txt should be filtered out, got: {:?}",
            changes
        );

        // Should NOT contain git-root-relative path
        assert!(
            !changes.contains(&PathBuf::from("sub/dir/file.rs")),
            "should not contain git-root-relative path, got: {:?}",
            changes
        );
    }

    #[test]
    fn test_check_index_status_subdir_stale() {
        // When xgrep root is a subdirectory of git root, changed_files
        // should be relative to xgrep root, not git root.
        let dir = tempdir().unwrap();
        let git_root = dir.path();
        init_git_repo(git_root);

        let sub_dir = git_root.join("sub").join("dir");
        fs::create_dir_all(&sub_dir).unwrap();
        fs::write(git_root.join(".gitignore"), "*.xgrep\n*.meta\n*.cache\n").unwrap();
        fs::write(sub_dir.join("file.rs"), "fn main() {}").unwrap();

        git_cmd()
            .args(["add", "."])
            .current_dir(git_root)
            .output()
            .unwrap();
        git_cmd()
            .args(["commit", "-m", "init"])
            .current_dir(git_root)
            .output()
            .unwrap();

        // Build index from the subdirectory
        let index_path = sub_dir.join("test.xgrep");
        ensure_fresh_index(&sub_dir, &index_path).unwrap();

        // Modify a file in the subdirectory
        fs::write(sub_dir.join("file.rs"), "fn main() { changed }").unwrap();

        let status = check_index_status(&sub_dir, &index_path).unwrap();
        match status {
            IndexStatus::Stale { changed_files } => {
                // Paths should be relative to sub/dir, not git root
                assert!(
                    changed_files.contains(&PathBuf::from("file.rs")),
                    "expected file.rs, got: {:?}",
                    changed_files
                );
                assert!(
                    !changed_files.contains(&PathBuf::from("sub/dir/file.rs")),
                    "should not contain git-root-relative path: {:?}",
                    changed_files
                );
            }
            other => panic!("expected Stale, got {:?}", other),
        }
    }

    #[test]
    fn test_changed_files_since_subdir() {
        let dir = tempdir().unwrap();
        let git_root = dir.path();
        init_git_repo(git_root);

        let sub_dir = git_root.join("sub");
        fs::create_dir_all(&sub_dir).unwrap();
        fs::write(sub_dir.join("a.txt"), "hello").unwrap();
        fs::write(git_root.join("root.txt"), "root").unwrap();

        git_cmd()
            .args(["add", "."])
            .current_dir(git_root)
            .output()
            .unwrap();
        git_cmd()
            .args(["commit", "-m", "init"])
            .current_dir(git_root)
            .output()
            .unwrap();

        let old_hash = current_commit_hash(git_root).unwrap();

        // New commit with changes in both locations
        fs::write(sub_dir.join("a.txt"), "changed").unwrap();
        fs::write(git_root.join("root.txt"), "changed root").unwrap();
        git_cmd()
            .args(["add", "."])
            .current_dir(git_root)
            .output()
            .unwrap();
        git_cmd()
            .args(["commit", "-m", "changes"])
            .current_dir(git_root)
            .output()
            .unwrap();

        // Query from subdirectory
        let files = changed_files_since(&sub_dir, &old_hash).unwrap();

        // Should contain a.txt (relative to sub/), not sub/a.txt
        assert!(
            files.contains(&PathBuf::from("a.txt")),
            "expected a.txt, got: {:?}",
            files
        );

        // Should NOT contain root.txt (outside of sub/)
        assert!(
            !files.contains(&PathBuf::from("root.txt")),
            "root.txt should be filtered out: {:?}",
            files
        );
    }

    /// Build the index the way searches do (index + meta with a window).
    fn build_with_meta(root: &Path, index_path: &Path) {
        let started = std::time::SystemTime::now();
        crate::index::builder::build_index(root, index_path).unwrap();
        save_meta(root, index_path, Some(started)).unwrap();
    }

    /// Move file mtimes (and the build window) safely into the past so an
    /// edit is never "racy" just because it shares the build's second.
    fn backdate(path: &Path, secs: u64) {
        let t = std::time::SystemTime::now() - std::time::Duration::from_secs(secs);
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    fn overlay_paths(ov: &Overlay) -> Vec<String> {
        ov.changed
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect()
    }

    #[test]
    fn test_stat_probe_finds_edits_new_and_deleted_files() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("src");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("keep.rs"), "keep").unwrap();
        fs::write(root.join("a/edit.rs"), "edit me").unwrap();
        fs::write(root.join("a/b/gone.rs"), "gone").unwrap();
        for f in ["keep.rs", "a/edit.rs", "a/b/gone.rs"] {
            backdate(&root.join(f), 10);
        }
        let index_path = dir.path().join("idx").join("index");
        fs::create_dir_all(index_path.parent().unwrap()).unwrap();
        build_with_meta(&root, &index_path);
        let reader = IndexReader::open(&index_path).unwrap();

        let ov = compute_overlay(&root, &index_path, &reader, None).unwrap();
        assert!(ov.changed.is_empty(), "{:?}", ov.changed);
        assert!(!ov.truncated && !ov.meta_outdated);

        fs::write(root.join("a/edit.rs"), "edited!").unwrap();
        fs::remove_file(root.join("a/b/gone.rs")).unwrap();
        fs::create_dir_all(root.join("a/b/new/deeper")).unwrap();
        fs::write(root.join("a/b/new/deeper/n.rs"), "new").unwrap();
        fs::write(root.join("top.rs"), "new top").unwrap();
        fs::write(root.join(".hidden.rs"), "hidden").unwrap();

        let ov = compute_overlay(&root, &index_path, &reader, None).unwrap();
        assert_eq!(
            overlay_paths(&ov),
            vec!["a/b/gone.rs", "a/b/new/deeper/n.rs", "a/edit.rs", "top.rs"]
        );
    }

    /// A file replaced by `mv` with one that has the same size and the
    /// indexed mtime is caught through its inode change time.
    #[cfg(unix)]
    #[test]
    fn test_stat_probe_detects_replacement_with_old_mtime() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("src");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.rs"), "aaaa").unwrap();
        backdate(&root.join("a.rs"), 10);
        let old_mtime = fs::metadata(root.join("a.rs")).unwrap().modified().unwrap();
        let index_path = dir.path().join("index");
        build_with_meta(&root, &index_path);
        let reader = IndexReader::open(&index_path).unwrap();

        let tmp = dir.path().join("replacement");
        fs::write(&tmp, "bbbb").unwrap();
        fs::File::options()
            .write(true)
            .open(&tmp)
            .unwrap()
            .set_modified(old_mtime)
            .unwrap();
        fs::rename(&tmp, root.join("a.rs")).unwrap();

        let ov = compute_overlay(&root, &index_path, &reader, None).unwrap();
        assert_eq!(overlay_paths(&ov), vec!["a.rs"]);
    }

    #[test]
    fn test_stat_probe_deadline_times_out() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("src");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.rs"), "a").unwrap();
        let index_path = dir.path().join("index");
        build_with_meta(&root, &index_path);
        let reader = IndexReader::open(&index_path).unwrap();
        let past = Instant::now() - std::time::Duration::from_millis(1);
        assert!(compute_overlay(&root, &index_path, &reader, Some(past)).is_none());
    }

    #[test]
    fn test_probe_cache_reuse_and_invalidation() {
        use std::time::Duration;

        let dir = tempdir().unwrap();
        let root = dir.path().join("src");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.rs"), "one").unwrap();
        backdate(&root.join("a.rs"), 10);
        let index_path = dir.path().join("index");
        build_with_meta(&root, &index_path);
        let ttl = Duration::from_secs(60);

        let reader = IndexReader::open(&index_path).unwrap();
        let (ov, cached) = compute_overlay_cached(&root, &index_path, &reader, None, ttl).unwrap();
        assert!(!cached && ov.changed.is_empty());
        assert!(probe_cache_path(&index_path).exists());

        // Within the TTL the stored result is reused (the documented
        // trade-off: an in-place edit is not seen yet).
        fs::write(root.join("a.rs"), "two").unwrap();
        let (ov, cached) = compute_overlay_cached(&root, &index_path, &reader, None, ttl).unwrap();
        assert!(cached && ov.changed.is_empty());

        // An expired entry is not reused.
        let (ov, cached) =
            compute_overlay_cached(&root, &index_path, &reader, None, Duration::from_nanos(1))
                .unwrap();
        assert!(!cached);
        assert_eq!(overlay_paths(&ov), vec!["a.rs"]);
        // ... and the fresh result is what gets reused next.
        let (ov, cached) = compute_overlay_cached(&root, &index_path, &reader, None, ttl).unwrap();
        assert!(cached);
        assert_eq!(overlay_paths(&ov), vec!["a.rs"]);

        // A rebuild (index + meta rewritten) invalidates the entry.
        drop(reader);
        std::thread::sleep(Duration::from_millis(20));
        build_with_meta(&root, &index_path);
        let reader = IndexReader::open(&index_path).unwrap();
        let (_, cached) = compute_overlay_cached(&root, &index_path, &reader, None, ttl).unwrap();
        assert!(!cached);

        // Even a meta-only rewrite (no-op build) invalidates it.
        std::thread::sleep(Duration::from_millis(20));
        save_meta(&root, &index_path, Some(std::time::SystemTime::now())).unwrap();
        let (_, cached) = compute_overlay_cached(&root, &index_path, &reader, None, ttl).unwrap();
        assert!(!cached);

        // A corrupt cache file is ignored, and a zero TTL never reads or
        // writes it.
        fs::write(probe_cache_path(&index_path), "garbage").unwrap();
        let (_, cached) = compute_overlay_cached(&root, &index_path, &reader, None, ttl).unwrap();
        assert!(!cached);
        fs::remove_file(probe_cache_path(&index_path)).unwrap();
        let (_, cached) =
            compute_overlay_cached(&root, &index_path, &reader, None, Duration::ZERO).unwrap();
        assert!(!cached);
        assert!(!probe_cache_path(&index_path).exists());
    }

    #[test]
    fn test_overlay_from_paths_expands_directories() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("src");
        fs::create_dir_all(root.join("old")).unwrap();
        fs::write(root.join("old/x.rs"), "x").unwrap();
        fs::write(root.join("old/y.rs"), "y").unwrap();
        fs::write(root.join("keep.rs"), "k").unwrap();
        let index_path = dir.path().join("index");
        build_with_meta(&root, &index_path);
        let reader = IndexReader::open(&index_path).unwrap();

        // `old` renamed to `new` (with a hidden file inside), plus an edit.
        fs::rename(root.join("old"), root.join("new")).unwrap();
        fs::write(root.join("new/.secret.rs"), "s").unwrap();
        fs::write(root.join("keep.rs"), "k2").unwrap();
        let ov = overlay_from_paths(
            &root,
            &index_path,
            &reader,
            vec![
                PathBuf::from("old"),
                PathBuf::from("new"),
                PathBuf::from("keep.rs"),
            ],
        );
        assert_eq!(
            overlay_paths(&ov),
            vec!["keep.rs", "new/x.rs", "new/y.rs", "old", "old/x.rs", "old/y.rs"]
        );
    }

    /// Files the builder never indexes (its `is_xgrep_internal` list, e.g.
    /// sidecars of an index stored inside the tree) are not reported as new
    /// by the probe or the watcher overlay either.
    #[test]
    fn test_overlay_skips_builder_internal_files() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("src");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/a.rs"), "a").unwrap();
        backdate(&root.join("sub/a.rs"), 10);
        let index_path = dir.path().join("index");
        build_with_meta(&root, &index_path);
        let reader = IndexReader::open(&index_path).unwrap();

        let internal = [
            "i.xgrep.cache",
            "i.xgrep.bincache",
            "i.xgrep.fp.tmp",
            ".xgrep_cache_tmp_1",
        ];
        for name in internal {
            assert!(crate::index::builder::is_xgrep_internal(name), "{name}");
            fs::write(root.join("sub").join(name), "x").unwrap();
        }
        fs::write(root.join("sub/new.rs"), "n").unwrap();

        let ov = compute_overlay(&root, &index_path, &reader, None).unwrap();
        assert_eq!(overlay_paths(&ov), vec!["sub/new.rs"]);
        let ov = overlay_from_paths(&root, &index_path, &reader, vec![PathBuf::from("sub")]);
        assert_eq!(overlay_paths(&ov), vec!["sub/a.rs", "sub/new.rs"]);
    }

    /// New files in directories that held no indexed file at build time
    /// (empty, or only binaries) are found by the probe.
    #[test]
    fn test_stat_probe_finds_files_in_dirs_without_indexed_files() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("src");
        fs::create_dir_all(root.join("code")).unwrap();
        fs::create_dir_all(root.join("empty/deeper")).unwrap();
        fs::create_dir_all(root.join("assets")).unwrap();
        fs::create_dir_all(root.join("ignored")).unwrap();
        fs::write(root.join(".ignore"), "ignored/\n").unwrap();
        fs::write(root.join("code/a.rs"), "a").unwrap();
        fs::write(root.join("assets/logo.bin"), b"\x00\x01\x02binary").unwrap();
        for f in [".ignore", "code/a.rs", "assets/logo.bin"] {
            backdate(&root.join(f), 10);
        }
        let index_path = dir.path().join("idx").join("index");
        fs::create_dir_all(index_path.parent().unwrap()).unwrap();
        build_with_meta(&root, &index_path);
        let reader = IndexReader::open(&index_path).unwrap();
        let meta = IndexMeta::load(&index_path).unwrap();
        let mut expected = vec!["assets", "empty", "empty/deeper"]
            .into_iter()
            .map(|d| index_key(Path::new(d)))
            .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(meta.dirs, expected);

        // Directory mtimes must predate the build window for the old
        // probe to miss the new files; wait out the window's second.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let ov = compute_overlay(&root, &index_path, &reader, None).unwrap();
        assert!(ov.changed.is_empty(), "{:?}", ov.changed);

        fs::write(root.join("empty/deeper/n.rs"), "new").unwrap();
        fs::write(root.join("assets/readme.txt"), "new").unwrap();
        fs::write(root.join("ignored/x.rs"), "ignored").unwrap();
        // (Unindexed binaries in a re-listed directory are rescanned too,
        // as anywhere else.)
        let ov = compute_overlay(&root, &index_path, &reader, None).unwrap();
        assert_eq!(
            overlay_paths(&ov),
            vec!["assets/logo.bin", "assets/readme.txt", "empty/deeper/n.rs"]
        );
    }
}
