use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use std::collections::{HashMap, HashSet};

use crate::error::Result;
use crate::git::{bytes_to_path, find_toplevel_fs, git_cmd, split_nul};
use crate::index::format::FileEntry;
use crate::index::reader::IndexReader;
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

/// Maximum number of build-time dirty paths remembered in the meta file.
/// Beyond this the meta records an overflow marker and the overlay falls
/// back to the (more expensive) full stat scan.
const META_DIRTY_LIMIT: usize = 2000;

/// Metadata stored alongside the index.
///
/// File format (line oriented, backwards compatible with the old single-line
/// format that only held the commit hash):
///
/// ```text
/// <commit hash or empty>
/// verified-ns <start> <end>     # build window (unix nanos); index reflects the tree as of <start>
/// dirty <root-relative path>    # files that differed from HEAD at build time (repeatable)
/// dirty-overflow                # too many dirty paths to record
/// ```
#[derive(Debug, Default)]
struct IndexMeta {
    commit_hash: Option<String>,
    verified_at: Option<Window>,
    dirty: Vec<PathBuf>,
    dirty_overflow: bool,
}

impl IndexMeta {
    fn path_for(index_path: &Path) -> PathBuf {
        index_path.with_extension("meta")
    }

    fn load(index_path: &Path) -> Option<Self> {
        let meta_path = Self::path_for(index_path);
        let content = fs::read_to_string(&meta_path).ok()?;
        let mut lines = content.lines();
        let commit_hash = lines.next().map(|s| s.trim().to_string());
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
            } else if let Some(p) = line.strip_prefix("dirty ") {
                meta.dirty.push(PathBuf::from(p));
            } else if line == "dirty-overflow" {
                meta.dirty_overflow = true;
            }
        }
        Some(meta)
    }

    fn save(
        index_path: &Path,
        commit_hash: Option<&str>,
        verified_at: Option<Window>,
        dirty: Option<&[PathBuf]>,
    ) -> Result<()> {
        use std::fmt::Write as _;
        let meta_path = Self::path_for(index_path);
        let mut content = commit_hash.unwrap_or("").to_string();
        if let Some((start, end)) = verified_at {
            let _ = write!(content, "\nverified-ns {start} {end}");
        }
        match dirty {
            Some(d) if d.len() > META_DIRTY_LIMIT => content.push_str("\ndirty-overflow"),
            Some(d) => {
                for p in d {
                    let s = p.to_string_lossy();
                    if s.contains('\n') {
                        // Cannot be represented line-wise; force the slow path.
                        content.push_str("\ndirty-overflow");
                        break;
                    }
                    let _ = write!(content, "\ndirty {s}");
                }
            }
            None => {}
        }
        // Write atomically so concurrent readers never see a truncated file.
        let tmp = meta_path.with_extension("meta.tmp");
        fs::write(&tmp, content)?;
        fs::rename(&tmp, &meta_path)?;
        Ok(())
    }
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

/// True if `t` is at or after `start`.
///
/// Timestamps without a sub-second part are assumed to come from a
/// filesystem with coarse (1-2 s) resolution and are compared at second
/// granularity, conservatively.
fn at_or_after(t: std::time::SystemTime, start: u128) -> bool {
    let ns = unix_nanos(t);
    if ns % NANOS_PER_SEC == 0 {
        ns / NANOS_PER_SEC >= start / NANOS_PER_SEC
    } else {
        ns >= start
    }
}

/// True if a file with mtime `t` may have been modified after the builder
/// read it while still matching the stored (whole-second) mtime.
///
/// The builder read the file at some instant in `[start, end]` and stores
/// whole seconds, so an edit is invisible to the mtime comparison only if
/// it happened at or after `start` and within a second no later than the
/// second containing `end`.
fn within_window(t: std::time::SystemTime, window: Window) -> bool {
    at_or_after(t, window.0) && unix_nanos(t) / NANOS_PER_SEC <= window.1 / NANOS_PER_SEC
}

/// Compare a stored index mtime with the current file mtime.
///
/// The builder currently stores whole seconds; millisecond and nanosecond
/// encodings are accepted too so this keeps working if the on-disk unit is
/// made finer-grained.
fn mtime_matches(stored: u64, actual: std::time::SystemTime) -> bool {
    let d = match actual.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d,
        Err(_) => return stored == 0,
    };
    stored == d.as_secs() || stored as u128 == d.as_millis() || stored as u128 == d.as_nanos()
}

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

/// Paths reported by `git status --porcelain=v2 -z`, plus the HEAD commit.
///
/// Paths are raw bytes relative to the repository top level.
#[derive(Debug, Default)]
struct GitStatusSnapshot {
    head: Option<String>,
    paths: Vec<Vec<u8>>,
}

/// Parse `git status --porcelain=v2 -z [--branch]` output.
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
            b'#' => {
                if let Some(oid) = tok.strip_prefix(b"# branch.oid ") {
                    if oid != b"(initial)" {
                        snap.head = Some(String::from_utf8_lossy(oid).into_owned());
                    }
                }
                None
            }
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
            "--branch",
            "--no-ahead-behind",
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
/// changed without its mtime differing from the indexed one. Also records
/// which files differed from HEAD at build time, so that a later revert of
/// such a file (which removes it from `git status`) is still detected.
pub fn save_meta(
    root: &Path,
    index_path: &Path,
    build_started: Option<std::time::SystemTime>,
) -> Result<()> {
    let verified = build_started.map(|s| (unix_nanos(s), unix_nanos(std::time::SystemTime::now())));
    let snapshot =
        git_prefix(root).and_then(|pfx| git_status(root, None).ok().flatten().map(|s| (pfx, s)));
    match snapshot {
        Some((prefix, snap)) => {
            let dirty: Vec<PathBuf> = snap
                .paths
                .iter()
                .filter_map(|p| convert_git_bytes(&prefix, p))
                .collect();
            IndexMeta::save(index_path, snap.head.as_deref(), verified, Some(&dirty))
        }
        None => {
            let hash = current_commit_hash(root);
            IndexMeta::save(index_path, hash.as_deref(), verified, None)
        }
    }
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

/// Upper bound on files collected while walking newly created directories
/// in a non-git tree. Beyond this the overlay reports `truncated`.
const NEW_FILE_WALK_LIMIT: usize = 10_000;

/// Result of the cheap freshness probe run before index searches.
#[derive(Debug, Default)]
pub(crate) struct Overlay {
    /// Root-relative paths whose index entries are stale: modified, deleted,
    /// or new files the index does not know about. Sorted and deduplicated.
    pub changed: Vec<PathBuf>,
    /// True when detection stopped early; `changed` is then a lower bound.
    pub truncated: bool,
    /// True when the index metadata is incomplete (old format, or not tied
    /// to a commit), so a background rebuild is worthwhile even if nothing
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
/// - git work tree: candidates come from `git status` (modified, staged,
///   untracked), `git diff-tree <indexed commit> HEAD`, and files that were
///   dirty when the index was built. Each candidate is then compared with
///   its index entry (mtime/size) so that untracked-but-already-indexed
///   files are not rescanned on every search.
/// - otherwise: every indexed file is stat'ed (in parallel) and compared
///   with its entry; directories modified since the build are re-listed to
///   find new files.
///
/// Returns `None` if the probe did not finish before `deadline` (any git
/// child is killed); with `deadline: None` it always completes.
pub(crate) fn compute_overlay(
    root: &Path,
    index_path: &Path,
    reader: &IndexReader,
    deadline: Option<Instant>,
) -> Option<Overlay> {
    let mut ov = compute_overlay_inner(root, index_path, reader, deadline).ok()?;
    // The index and its sidecars (meta, cache, stamps) may live inside the
    // root (e.g. tests, custom layouts); they are never source files.
    ov.changed
        .retain(|rel| !is_index_sidecar(root, index_path, rel));
    Some(ov)
}

/// True if `root/rel` is the index file or one of its sidecars
/// (`<stem>.meta`, `<stem>.cache`, `<stem>.rebuild`, ...).
fn is_index_sidecar(root: &Path, index_path: &Path, rel: &Path) -> bool {
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

fn compute_overlay_inner(
    root: &Path,
    index_path: &Path,
    reader: &IndexReader,
    deadline: Option<Instant>,
) -> std::result::Result<Overlay, TimedOut> {
    let meta = IndexMeta::load(index_path);
    let meta_outdated = meta
        .as_ref()
        .is_none_or(|m| m.verified_at.is_none() || m.dirty_overflow);
    let meta = meta.unwrap_or_default();
    let window = verified_window(&meta, index_path);

    if let Some(prefix) = git_prefix(root) {
        if let Some(snap) = git_status(root, deadline)? {
            if let Some(cands) = git_candidates(root, &prefix, &snap, &meta, deadline)? {
                let mut ov = filter_candidates(root, reader, window, cands);
                ov.meta_outdated = meta_outdated;
                return Ok(ov);
            }
            // In a git repo but cannot diff against the indexed state:
            // fall through to the stat scan and ask for a rebuild so the
            // next meta is tied to a commit.
            let mut ov = stat_scan(root, reader, window, deadline)?;
            ov.meta_outdated = true;
            return Ok(ov);
        }
    }
    let mut ov = stat_scan(root, reader, window, deadline)?;
    ov.meta_outdated = meta_outdated;
    Ok(ov)
}

/// Candidate paths (root-relative) that may differ from the index in a git
/// work tree. `None` if the indexed state cannot be related to HEAD.
fn git_candidates(
    root: &Path,
    prefix: &Path,
    snap: &GitStatusSnapshot,
    meta: &IndexMeta,
    deadline: Option<Instant>,
) -> std::result::Result<Option<HashSet<PathBuf>>, TimedOut> {
    if meta.dirty_overflow {
        return Ok(None);
    }
    let base = meta.commit_hash.as_deref().filter(|h| !h.is_empty());
    let mut set: HashSet<PathBuf> = snap
        .paths
        .iter()
        .filter_map(|p| convert_git_bytes(prefix, p))
        .collect();
    set.extend(meta.dirty.iter().cloned());
    match (base, snap.head.as_deref()) {
        (Some(b), Some(h)) if b != h => match diff_tree_paths(root, b, h, deadline)? {
            Some(paths) => set.extend(paths),
            None => return Ok(None),
        },
        (Some(_), Some(_)) => {}
        // Index not tied to a commit (built outside git or before the
        // first commit): cannot diff.
        (None, Some(_)) => return Ok(None),
        // Unborn branch: status already lists every file as untracked.
        (_, None) => {}
    }
    Ok(Some(set))
}

/// Lookup key matching how the builder stores paths in the index.
pub(crate) fn index_key(rel: &Path) -> String {
    rel.iter()
        .collect::<PathBuf>()
        .to_string_lossy()
        .into_owned()
}

/// True if the on-disk file no longer matches its index entry.
fn entry_is_stale(full: &Path, entry: &FileEntry, window: Window) -> bool {
    let Ok(md) = fs::metadata(full) else {
        return true; // deleted
    };
    if !md.is_file() || md.len() != entry.size {
        return true;
    }
    match md.modified() {
        Ok(t) => {
            if !mtime_matches(entry.mtime, t) {
                return true;
            }
            // Racy entry: modified while the build was reading files, so an
            // in-place edit may share the stored whole-second mtime.
            within_window(t, window)
        }
        Err(_) => true,
    }
}

/// Mirrors the builder's exclusions for a file it has never seen: hidden
/// path components and xgrep's own files are skipped.
fn is_indexable_new(root: &Path, rel: &Path) -> bool {
    let hidden_or_internal = rel.iter().any(|c| {
        let c = c.to_string_lossy();
        c.starts_with('.') || is_xgrep_internal_name(&c)
    });
    if hidden_or_internal {
        return false;
    }
    fs::symlink_metadata(root.join(rel)).is_ok_and(|m| m.is_file())
}

fn is_xgrep_internal_name(name: &str) -> bool {
    name == ".xgrep"
        || name.ends_with(".xgrep")
        || name.ends_with(".xgrep.fp")
        || name.ends_with(".xgrep.lock")
        || name.starts_with(".xgrep_tmp_")
}

/// Reduce git candidates to those that actually differ from the index.
fn filter_candidates(
    root: &Path,
    reader: &IndexReader,
    window: Window,
    cands: HashSet<PathBuf>,
) -> Overlay {
    if cands.is_empty() {
        return Overlay::default();
    }
    let mut pending: HashMap<String, PathBuf> =
        cands.into_iter().map(|p| (index_key(&p), p)).collect();
    let mut changed = Vec::new();
    for fid in 0..reader.file_count() {
        if pending.is_empty() {
            break;
        }
        if let Some(rel) = pending.remove(reader.file_path(fid)) {
            let stale = reader
                .file_entry(fid)
                .is_none_or(|e| entry_is_stale(&root.join(&rel), &e, window));
            if stale {
                changed.push(rel);
            }
        }
    }
    // Not in the index: new files (or paths the builder never indexes).
    changed.extend(
        pending
            .into_values()
            .filter(|rel| is_indexable_new(root, rel)),
    );
    changed.sort();
    changed.dedup();
    Overlay {
        changed,
        ..Default::default()
    }
}

/// Non-git freshness probe: stat every indexed file and every directory
/// containing indexed files; re-list directories modified since the build.
fn stat_scan(
    root: &Path,
    reader: &IndexReader,
    window: Window,
    deadline: Option<Instant>,
) -> std::result::Result<Overlay, TimedOut> {
    use rayon::prelude::*;

    let n = reader.file_count();
    // Stat every indexed file (parallel) while collecting the directories
    // known to the index (parents of indexed files and their ancestors;
    // "" is the root). Directory names are borrowed from the mmap.
    let (changed, (known, known_list)) = rayon::join(
        || -> std::result::Result<Vec<Option<PathBuf>>, TimedOut> {
            (0..n)
                .into_par_iter()
                .with_min_len(256)
                .map(|fid| {
                    check_deadline(deadline)?;
                    let rel = reader.file_path(fid);
                    let Some(entry) = reader.file_entry(fid) else {
                        return Ok(None);
                    };
                    Ok(entry_is_stale(&root.join(rel), &entry, window).then(|| PathBuf::from(rel)))
                })
                .collect()
        },
        || {
            let mut known: HashSet<&str> = HashSet::new();
            let mut known_list: Vec<&str> = vec![""];
            known.insert("");
            for fid in 0..n {
                let mut cur = Path::new(reader.file_path(fid)).parent();
                while let Some(dir) = cur {
                    let Some(d) = dir.to_str() else { break };
                    if !known.insert(d) {
                        break;
                    }
                    known_list.push(d);
                    cur = dir.parent();
                }
            }
            (known, known_list)
        },
    );
    let mut changed: Vec<PathBuf> = changed?.into_iter().flatten().collect();
    check_deadline(deadline)?;
    let changed_dirs: Vec<&str> = known_list
        .par_iter()
        .with_min_len(128)
        .filter(|d| {
            fs::metadata(root.join(d))
                .and_then(|m| m.modified())
                .is_ok_and(|t| at_or_after(t, window.0))
        })
        .copied()
        .collect();
    check_deadline(deadline)?;

    let mut truncated = false;
    if !changed_dirs.is_empty() {
        let indexed: HashSet<&str> = (0..n).map(|fid| reader.file_path(fid)).collect();
        let owned = |it: &mut dyn Iterator<Item = &str>| -> HashSet<String> {
            it.map(str::to_string).collect()
        };
        let (new_files, hit_limit) = walk_new_files(
            root,
            owned(&mut known.into_iter()),
            owned(&mut changed_dirs.into_iter()),
            &indexed,
            deadline,
        )?;
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
            if !ancestors.insert(a.to_string_lossy().into_owned()) {
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
            if is_xgrep_internal_name(&e.file_name().to_string_lossy()) {
                return false;
            }
            let Ok(rel) = e.path().strip_prefix(&root_owned) else {
                return true;
            };
            let rel_s = rel.to_string_lossy();
            if rel_s.is_empty() {
                return true;
            }
            let is_dir = e.file_type().is_some_and(|t| t.is_dir());
            if is_dir && known.contains(rel_s.as_ref()) {
                return ancestors.contains(rel_s.as_ref()) || changed_dirs.contains(rel_s.as_ref());
            }
            // Unknown dir or a file: include only if its nearest known
            // ancestor directory was modified since the build.
            let mut cur = rel.parent();
            while let Some(a) = cur {
                let a_s = a.to_string_lossy();
                if known.contains(a_s.as_ref()) {
                    return changed_dirs.contains(a_s.as_ref());
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
        if indexed.contains(rel.to_string_lossy().as_ref()) {
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

        IndexMeta::save(&index_path, Some("abc123"), None, None).unwrap();
        let meta = IndexMeta::load(&index_path).unwrap();
        assert_eq!(meta.commit_hash, Some("abc123".to_string()));
        assert_eq!(meta.verified_at, None);
        assert!(meta.dirty.is_empty());

        let dirty = vec![PathBuf::from("a b.txt"), PathBuf::from("日本.txt")];
        IndexMeta::save(&index_path, Some("def"), Some((10, 12)), Some(&dirty)).unwrap();
        let meta = IndexMeta::load(&index_path).unwrap();
        assert_eq!(meta.commit_hash, Some("def".to_string()));
        assert_eq!(meta.verified_at, Some((10, 12)));
        assert_eq!(meta.dirty, dirty);
        assert!(!meta.dirty_overflow);
    }

    #[test]
    fn test_meta_load_legacy_single_line() {
        let dir = tempdir().unwrap();
        let index_path = dir.path().join("test.xgrep");
        fs::write(index_path.with_extension("meta"), "abc123").unwrap();
        let meta = IndexMeta::load(&index_path).unwrap();
        assert_eq!(meta.commit_hash, Some("abc123".to_string()));
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
        assert_eq!(snap.head.as_deref(), Some("0123abcd"));
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
        assert!(snap.head.is_none());
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
}
