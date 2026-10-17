use std::collections::HashMap;
use std::fs;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

use crate::error::{Result, XgrepError};
use ignore::{WalkBuilder, WalkState};
use rayon::prelude::*;
use xxhash_rust::xxh64::Xxh64;

use crate::index::cache::{FileStat, SidecarView, SidecarWriter};
use crate::index::format::*;
use crate::trigram::{self, Gram};

/// Size of the 24-bit trigram space; flat arrays indexed by trigram use this length.
const TRIGRAM_SPACE: usize = 1 << 24;

/// Environment variable that overrides the maximum indexed file size.
///
/// Accepts a byte count with an optional `K`/`M`/`G` suffix (binary units,
/// e.g. `512K`, `20M`), or `0` / `none` / `unlimited` to disable the limit.
/// Files larger than the limit are left out of the index (and therefore are
/// not found by indexed searches), like ripgrep's `--max-filesize`.
pub const MAX_FILESIZE_ENV: &str = "XGREP_MAX_FILESIZE";

/// Default maximum indexed file size: 10 MiB. Large enough for virtually all
/// hand-written and most generated source files, small enough to keep huge
/// logs/dumps (which contain almost every trigram and would become a candidate
/// for every query) out of the index.
pub const DEFAULT_MAX_FILESIZE: u64 = 10 * 1024 * 1024;

/// Bytes inspected up-front for a NUL byte before reading the rest of a file.
const BINARY_PEEK_BYTES: usize = 8192;

/// A diff update is abandoned in favor of a full rebuild once the number of
/// changed + added + deleted files exceeds this percentage of the indexed files.
///
/// Measured on an 8.5K-file / 118 MB corpus (crates.io registry sources), with
/// the fallback full build reusing the trigram cache for unchanged files:
///
/// | files edited | diff (28 thr) | full (28 thr) | diff (4 thr) | full (4 thr) |
/// |-------------:|--------------:|--------------:|-------------:|-------------:|
/// |  2 %         | 0.11 s        | 0.20 s        | 0.15 s       | 0.27 s       |
/// | 10 %         | 0.13 s        | 0.20 s        | 0.13 s       | 0.21 s       |
/// | 20 %         | 0.16 s        | 0.18 s        | 0.24 s       | 0.26 s       |
/// | 35 %         | 0.22 s        | 0.19 s        | 0.32 s       | 0.27 s       |
///
/// Deletions and additions behave similarly. The break-even point is ~20-25 %;
/// 15 % leaves margin for corpora where an early deletion forces an ID remap
/// of most posting lists. (Measured with index v3. With v4 a full build no
/// longer reuses cached trigrams; on a 13.8K-file / 361 MB corpus diff
/// updates of 1/5/10 % of the files took 0.12/0.13/0.16 s vs 0.24 s for a
/// full build, so the threshold stays conservative.)
const DIFF_FALLBACK_PERCENT: usize = 15;

macro_rules! debug_log {
    ($($arg:tt)*) => {
        if std::env::var_os("XGREP_DEBUG").is_some() {
            eprintln!("[xgrep] {}", format!($($arg)*));
        }
    };
}

/// Effective maximum file size for indexing (`None` = unlimited).
/// See [`MAX_FILESIZE_ENV`].
pub fn max_filesize() -> Option<u64> {
    match std::env::var(MAX_FILESIZE_ENV) {
        Ok(v) => match parse_size(&v) {
            Some(0) => None,
            Some(n) => Some(n),
            None => {
                eprintln!(
                    "warning: ignoring invalid {}={:?} (expected e.g. 10M, 512K, 0 for unlimited)",
                    MAX_FILESIZE_ENV, v
                );
                Some(DEFAULT_MAX_FILESIZE)
            }
        },
        Err(_) => Some(DEFAULT_MAX_FILESIZE),
    }
}

fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("none") || s.eq_ignore_ascii_case("unlimited") {
        return Some(0);
    }
    let (num, mult): (&str, u64) = match s.as_bytes().last()? {
        b'k' | b'K' => (&s[..s.len() - 1], 1 << 10),
        b'm' | b'M' => (&s[..s.len() - 1], 1 << 20),
        b'g' | b'G' => (&s[..s.len() - 1], 1 << 30),
        _ => (s, 1),
    };
    num.trim().parse::<u64>().ok()?.checked_mul(mult)
}

/// Files left out of the index because they exceed the max file size,
/// recorded at build time in a small JSON sidecar (`<index>.oversize`) so
/// `status` and zero-result hints can mention them. The main index format is
/// not involved; the sidecar is absent when nothing was skipped.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OversizeInfo {
    /// The limit in effect for the build, in bytes.
    pub limit: u64,
    /// Number of walked files larger than `limit`.
    pub count: usize,
    /// A few of them (largest first), relative to the root.
    pub examples: Vec<String>,
}

/// Maximum number of example paths kept in [`OversizeInfo`].
const OVERSIZE_EXAMPLES: usize = 5;

impl OversizeInfo {
    /// `10MB`, `512KB`, `1.5MB`-style rendering of the limit.
    pub fn limit_display(&self) -> String {
        human_size(self.limit)
    }

    /// One-line explanation for zero-result hints: what is missing and how
    /// to search it anyway (`how` names the way to search single files).
    pub fn hint(&self, how: &str) -> String {
        let eg = match self.examples.first() {
            Some(p) => format!(" (e.g. {})", p),
            None => String::new(),
        };
        format!(
            "{} over {} {} not indexed{}, so indexed searches cannot find matches in {}; set {}=0 (or a larger size) and rebuild the index, or {}",
            if self.count == 1 {
                "1 file".to_string()
            } else {
                format!("{} files", self.count)
            },
            self.limit_display(),
            if self.count == 1 { "is" } else { "are" },
            eg,
            if self.count == 1 { "it" } else { "them" },
            MAX_FILESIZE_ENV,
            how
        )
    }
}

/// Human-readable byte count with binary units (`10MB` = 10 MiB).
pub fn human_size(n: u64) -> String {
    const UNITS: [(&str, u64); 3] = [("GB", 1 << 30), ("MB", 1 << 20), ("KB", 1 << 10)];
    for (unit, size) in UNITS {
        if n >= size {
            return if n % size == 0 {
                format!("{}{}", n / size, unit)
            } else {
                format!("{:.1}{}", n as f64 / size as f64, unit)
            };
        }
    }
    format!("{}B", n)
}

/// Path of the oversize sidecar for an index file.
pub fn oversize_path(index_path: &Path) -> PathBuf {
    let mut s = index_path.as_os_str().to_os_string();
    s.push(".oversize");
    PathBuf::from(s)
}

/// Files skipped for size by the last build of `index_path` (`None` when
/// none were, or the index predates this record).
pub fn read_oversize(index_path: &Path) -> Option<OversizeInfo> {
    let data = fs::read(oversize_path(index_path)).ok()?;
    serde_json::from_slice::<OversizeInfo>(&data)
        .ok()
        .filter(|i| i.count > 0)
}

/// Record the walked files over `max` (cheap: stat data only). Errors are
/// ignored; the record is informational.
fn write_oversize(index_path: &Path, walk: &[WalkEntry], max: Option<u64>) {
    let path = oversize_path(index_path);
    let Some(limit) = max else {
        let _ = fs::remove_file(&path);
        return;
    };
    let mut big: Vec<&WalkEntry> = walk.iter().filter(|e| e.stat.size > limit).collect();
    if big.is_empty() {
        let _ = fs::remove_file(&path);
        return;
    }
    big.sort_by(|a, b| b.stat.size.cmp(&a.stat.size).then(a.rel.cmp(&b.rel)));
    let info = OversizeInfo {
        limit,
        count: big.len(),
        examples: big
            .iter()
            .take(OVERSIZE_EXAMPLES)
            .map(|e| e.rel.clone())
            .collect(),
    };
    if read_oversize(index_path).as_ref() == Some(&info) {
        return;
    }
    let Ok(json) = serde_json::to_vec(&info) else {
        return;
    };
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    if fs::write(&tmp, json).is_ok() && fs::rename(&tmp, &path).is_err() {
        let _ = fs::remove_file(&tmp);
    }
}

fn exceeds_max(size: u64, max: Option<u64>) -> bool {
    max.is_some_and(|m| size > m)
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

// ============================================================
// File reading / binary detection (shared by full and diff builds)
// ============================================================

enum ReadOutcome<R> {
    Text(R),
    /// Contains a NUL byte: excluded from the index.
    Binary,
    /// Grew beyond the size limit since it was stat'ed.
    TooLarge,
    Unreadable,
}

thread_local! {
    /// Per-thread reusable read buffer (avoids allocator churn and the memory
    /// that per-thread allocator caches would otherwise retain).
    static READ_BUF: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Read buffers larger than this are released after use instead of kept.
const READ_BUF_KEEP: usize = 256 << 10;

/// Read a file for indexing and, if it is text, pass its content to `f`.
///
/// Binary rule (used by every build path, and matching the search-time check):
/// a file is binary if it contains a NUL byte anywhere. The first 8 KiB are
/// checked before reading the rest, so most binaries are rejected cheaply.
fn with_text_file<R>(
    path: &Path,
    size_hint: u64,
    max: Option<u64>,
    f: impl FnOnce(&[u8]) -> R,
) -> ReadOutcome<R> {
    READ_BUF.with(|cell| {
        let mut buf = cell.borrow_mut();
        buf.clear();
        let out = match read_text_into(&mut buf, path, size_hint, max) {
            ReadOutcome::Text(()) => ReadOutcome::Text(f(&buf)),
            ReadOutcome::Binary => ReadOutcome::Binary,
            ReadOutcome::TooLarge => ReadOutcome::TooLarge,
            ReadOutcome::Unreadable => ReadOutcome::Unreadable,
        };
        if buf.capacity() > READ_BUF_KEEP {
            *buf = Vec::new();
        }
        out
    })
}

fn read_text_into(
    content: &mut Vec<u8>,
    path: &Path,
    size_hint: u64,
    max: Option<u64>,
) -> ReadOutcome<()> {
    let Ok(mut file) = fs::File::open(path) else {
        return ReadOutcome::Unreadable;
    };
    // Exact-size buffer (+1 so read_to_end can detect EOF without growing).
    let cap = size_hint.min(max.unwrap_or(u64::MAX)).saturating_add(1);
    content.reserve_exact(cap.min(1 << 30) as usize);
    if (&mut file)
        .take(BINARY_PEEK_BYTES as u64)
        .read_to_end(content)
        .is_err()
    {
        return ReadOutcome::Unreadable;
    }
    if memchr::memchr(0, content).is_some() {
        return ReadOutcome::Binary;
    }
    if content.len() == BINARY_PEEK_BYTES {
        let peeked = content.len();
        let res = match max {
            // Read at most max+1 bytes so a file that grew is detected without
            // reading all of it.
            Some(m) => (&mut file)
                .take((m + 1).saturating_sub(peeked as u64))
                .read_to_end(content),
            None => file.read_to_end(content),
        };
        if res.is_err() {
            return ReadOutcome::Unreadable;
        }
        if exceeds_max(content.len() as u64, max) {
            return ReadOutcome::TooLarge;
        }
        if memchr::memchr(0, &content[peeked..]).is_some() {
            return ReadOutcome::Binary;
        }
    } else if exceeds_max(content.len() as u64, max) {
        return ReadOutcome::TooLarge;
    }
    ReadOutcome::Text(())
}

// ============================================================
// Corpus walk + fingerprint: fast "nothing changed" detection
// ============================================================

/// One file found by the corpus walk.
struct WalkEntry {
    /// Path relative to the root (always valid UTF-8; see [`walk_corpus`]).
    rel: String,
    /// Metadata captured during the walk, i.e. *before* the content is read.
    stat: FileStat,
}

fn fingerprint_path(index_path: &Path) -> PathBuf {
    let mut s = index_path.as_os_str().to_os_string();
    s.push(".fp");
    PathBuf::from(s)
}

/// Stored fingerprint state: (corpus fingerprint, build start time in ns).
/// The legacy 8-byte format has no build start time.
fn read_fingerprint(fp_path: &Path) -> Option<(u64, Option<u64>)> {
    let data = fs::read(fp_path).ok()?;
    let fp = u64::from_le_bytes(data.get(..8)?.try_into().ok()?);
    let start = data
        .get(8..16)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_le_bytes);
    Some((fp, start))
}

fn write_fingerprint(fp_path: &Path, fingerprint: u64, build_start_ns: u64) -> Result<()> {
    let mut tmp_s = fp_path.as_os_str().to_os_string();
    tmp_s.push(".tmp");
    let tmp = PathBuf::from(tmp_s);
    let mut buf = [0u8; 16];
    buf[..8].copy_from_slice(&fingerprint.to_le_bytes());
    buf[8..].copy_from_slice(&build_start_ns.to_le_bytes());
    fs::write(&tmp, buf)?;
    fs::rename(&tmp, fp_path)?;
    Ok(())
}

fn binary_cache_path(index_path: &Path) -> PathBuf {
    let mut s = index_path.as_os_str().to_os_string();
    s.push(".bincache");
    PathBuf::from(s)
}

/// Load cache of known binary files.
/// Format: each line `{path}\t{mtime_ns}\t{size}\n`
fn load_binary_cache(cache_path: &Path) -> HashMap<String, (u64, u64)> {
    let Ok(data) = fs::read_to_string(cache_path) else {
        return HashMap::new();
    };
    let mut map = HashMap::new();
    for line in data.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(path), Some(mtime_s), Some(size_s)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let (Ok(mtime), Ok(size)) = (mtime_s.parse::<u64>(), size_s.parse::<u64>()) else {
            continue;
        };
        map.insert(path.to_owned(), (mtime, size));
    }
    map
}

fn save_binary_cache(cache_path: &Path, cache: &HashMap<String, (u64, u64)>) {
    let mut data = String::with_capacity(cache.len() * 80);
    let mut entries: Vec<(&str, u64, u64)> = cache
        .iter()
        .map(|(p, &(m, s))| (p.as_str(), m, s))
        .collect();
    entries.sort_unstable_by_key(|e| e.0);
    for (path, mtime, size) in entries {
        data.push_str(path);
        data.push('\t');
        data.push_str(&mtime.to_string());
        data.push('\t');
        data.push_str(&size.to_string());
        data.push('\n');
    }
    let _ = fs::write(cache_path, data.as_bytes());
}

/// Returns `true` for files and directories that are xgrep internal artefacts
/// and should never be included in the corpus walk.
pub(crate) fn is_xgrep_internal(name: &str) -> bool {
    name == ".xgrep"              // index directory
        || name.ends_with(".xgrep")   // index file (e.g. index.xgrep)
        || name.ends_with(".xgrep.fp")    // fingerprint file
        || name.ends_with(".xgrep.fp.tmp")
        || name.ends_with(".xgrep.bincache")
        || name.ends_with(".xgrep.cache") // default sidecar (index.xgrep.cache)
        || name.ends_with(".xgrep.lock")  // build lock file
        || name.ends_with(".xgrep.oversize") // files skipped for size
        || name.ends_with(".xgrep.oversize.tmp")
        || name.starts_with(".xgrep_tmp_") // atomic write tmp files
        || name.starts_with(".xgrep_cache_tmp_")
}

/// Walk the corpus (respecting ignore files) and stat every regular file.
///
/// Uses the parallel walker. Paths that are not valid UTF-8 are skipped (with a
/// message when `XGREP_DEBUG` is set): the index stores UTF-8 paths, and a lossily
/// converted name could never be opened again. Walk errors (e.g. unreadable
/// directories) are skipped the same way. The result is sorted by path.
fn walk_corpus(root: &Path, lock_path: &Path) -> Vec<WalkEntry> {
    let collected: Mutex<Vec<WalkEntry>> = Mutex::new(Vec::new());

    struct Flush<'a> {
        local: Vec<WalkEntry>,
        out: &'a Mutex<Vec<WalkEntry>>,
    }
    impl Drop for Flush<'_> {
        fn drop(&mut self) {
            if let Ok(mut out) = self.out.lock() {
                out.append(&mut self.local);
            }
        }
    }

    WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(|e| !is_xgrep_internal(&e.file_name().to_string_lossy()))
        .build_parallel()
        .run(|| {
            let mut sink = Flush {
                local: Vec::new(),
                out: &collected,
            };
            Box::new(move |entry| {
                let entry = match entry {
                    Ok(e) => e,
                    Err(err) => {
                        debug_log!("walk error (skipped): {}", err);
                        return WalkState::Continue;
                    }
                };
                if entry.file_type().is_none_or(|ft| !ft.is_file()) {
                    return WalkState::Continue;
                }
                let path = entry.path();
                if path == lock_path {
                    return WalkState::Continue;
                }
                let Ok(rel) = path.strip_prefix(root) else {
                    return WalkState::Continue;
                };
                if rel.to_str().is_none() {
                    debug_log!("skipping file with non-UTF-8 path: {}", path.display());
                    return WalkState::Continue;
                }
                let rel = crate::index::rel_key(rel);
                let Ok(meta) = entry.metadata() else {
                    return WalkState::Continue;
                };
                sink.local.push(WalkEntry {
                    rel,
                    stat: FileStat::from_metadata(&meta),
                });
                WalkState::Continue
            })
        });

    let mut entries = collected.into_inner().unwrap_or_else(|e| e.into_inner());
    entries.sort_unstable_by(|a, b| a.rel.cmp(&b.rel));
    entries
}

/// Fingerprint of the whole walked tree: paths + full stat data (including
/// files excluded from the index such as binaries), plus the size limit so that
/// changing `XGREP_MAX_FILESIZE` triggers an update.
///
/// Computed from the walk taken *before* any file content is read, so a file
/// modified during the build always mismatches next time.
fn corpus_fingerprint(entries: &[WalkEntry], max: Option<u64>) -> u64 {
    let mut h = Xxh64::new(0);
    h.update(b"xgrep-fp-v2");
    h.update(&max.unwrap_or(0).to_le_bytes());
    for e in entries {
        h.update(e.rel.as_bytes());
        h.update(&[0u8]); // NUL separator prevents "ab"+"c" == "a"+"bc"
        h.update(&e.stat.mtime_ns.to_le_bytes());
        h.update(&e.stat.size.to_le_bytes());
        h.update(&e.stat.ctime_ns.to_le_bytes());
        h.update(&e.stat.ino.to_le_bytes());
    }
    h.digest()
}

// ============================================================
// Lock Guard: advisory file lock to prevent concurrent builds
// ============================================================

struct LockGuard {
    path: PathBuf,
    /// Keeps the flock held for the guard's lifetime (released on close).
    #[cfg(unix)]
    _file: fs::File,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        // Unlink while still holding the lock; see acquire_lock for why the
        // inode check makes this safe against racing acquirers.
        let _ = fs::remove_file(&self.path);
    }
}

/// Acquire the build lock (unix): `flock(LOCK_EX | LOCK_NB)` on `<index>.lock`.
///
/// The kernel releases the lock when the holder exits or crashes, so no
/// stale-lock heuristics (and no racy "remove stale file, then retry") are
/// needed. Because holders unlink the file on release, a waiter could end up
/// locking an unlinked inode; after locking we therefore verify that the path
/// still refers to the locked inode and retry otherwise.
#[cfg(unix)]
fn acquire_lock(index_path: &Path) -> Result<LockGuard> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::io::AsRawFd;

    let lock_path = index_path.with_extension("lock");
    for _ in 0..10 {
        let mut f = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| XgrepError::LockError(format!("failed to create lock file: {}", e)))?;
        // SAFETY: flock only operates on the open file descriptor owned by `f`.
        let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                return Err(XgrepError::LockError(format!(
                    "index build already in progress (lock: {})",
                    lock_path.display()
                )));
            }
            return Err(XgrepError::LockError(format!(
                "failed to lock {}: {}",
                lock_path.display(),
                err
            )));
        }
        let same_inode = match (f.metadata(), fs::metadata(&lock_path)) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        };
        if !same_inode {
            // The previous holder unlinked the file between our open and flock.
            continue;
        }
        let _ = f.set_len(0);
        let _ = write!(f, "{}", std::process::id());
        return Ok(LockGuard {
            path: lock_path,
            _file: f,
        });
    }
    Err(XgrepError::LockError(format!(
        "failed to acquire lock after retries (lock: {})",
        lock_path.display()
    )))
}

/// Acquire the build lock (non-unix): exclusive create of `<index>.lock`,
/// treating locks older than 5 minutes as stale.
#[cfg(not(unix))]
fn acquire_lock(index_path: &Path) -> Result<LockGuard> {
    let lock_path = index_path.with_extension("lock");
    for _ in 0..3 {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(mut f) => {
                let _ = write!(f, "{}", std::process::id());
                return Ok(LockGuard { path: lock_path });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = fs::metadata(&lock_path)
                    .and_then(|m| m.modified())
                    .map(|m| m.elapsed().unwrap_or_default().as_secs() > 300)
                    .unwrap_or(false);
                if !stale {
                    return Err(XgrepError::LockError(format!(
                        "index build already in progress (lock: {})",
                        lock_path.display()
                    )));
                }
                // Take over the stale lock by renaming it away first: only one
                // contender can win the rename, so two processes cannot both
                // delete-and-recreate the lock.
                let grave = lock_path.with_extension(format!("lock.stale.{}", std::process::id()));
                if fs::rename(&lock_path, &grave).is_ok() {
                    let _ = fs::remove_file(&grave);
                }
            }
            Err(e) => {
                return Err(XgrepError::LockError(format!(
                    "failed to create lock file: {}",
                    e
                )))
            }
        }
    }
    Err(XgrepError::LockError(format!(
        "failed to acquire lock after retries (lock: {})",
        lock_path.display()
    )))
}

#[allow(dead_code)]
pub fn build_index(root: &Path, index_path: &Path) -> Result<bool> {
    build_index_with_cache(root, index_path, None)
}

/// Where and what to record as the corpus fingerprint once a build succeeds.
struct FingerprintState {
    path: PathBuf,
    fingerprint: u64,
    build_start_ns: u64,
}

impl FingerprintState {
    fn write(&self) {
        let _ = write_fingerprint(&self.path, self.fingerprint, self.build_start_ns);
    }
}

/// Sidecar path: the given cache path, or `<index>.cache` appended to the
/// index file name (e.g. `index.xgrep.cache`, skipped by the corpus walk).
fn sidecar_path(index_path: &Path, cache_path: Option<&Path>) -> PathBuf {
    match cache_path {
        Some(p) => p.to_path_buf(),
        None => {
            let mut s = index_path.as_os_str().to_os_string();
            s.push(".cache");
            PathBuf::from(s)
        }
    }
}

/// Id tying a main index to the sidecar written with it: a hash of the file
/// list (paths, stat data, content hashes). Deterministic, so rebuilding an
/// unchanged corpus yields a byte-identical index; a sidecar and an index
/// with equal ids describe the same file contents.
fn sidecar_id_for(files: &[FileInfo]) -> u64 {
    let mut h = Xxh64::new(0x5eed);
    h.update(&(files.len() as u64).to_le_bytes());
    for fi in files {
        h.update(fi.relative_path.as_bytes());
        h.update(&[0u8]);
        h.update(&fi.stat.mtime_ns.to_le_bytes());
        h.update(&fi.stat.size.to_le_bytes());
        h.update(&fi.content_hash.to_le_bytes());
    }
    h.digest().max(1)
}

/// Build or update the search index.
///
/// Returns `true` if the index was (re)built, `false` if nothing relevant
/// changed and no rebuild was needed.
///
/// `cache_path` is where the index sidecar (per-file trigram data used by
/// builds only, see `index::cache`) lives; `None` uses `<index>.cache`.
///
/// Change detection compares nanosecond mtime + size per file (plus ctime and
/// inode in the corpus fingerprint on unix). Files whose mtime falls in the
/// same second as (or after) the previous build's start are "racily clean":
/// their stat data cannot prove they are unchanged, so they are re-verified by
/// content hash (the same approach git uses for its index).
///
/// An index in an older format version is always rebuilt from scratch.
pub fn build_index_with_cache(
    root: &Path,
    index_path: &Path,
    cache_path: Option<&Path>,
) -> Result<bool> {
    build_index_with_limit(root, index_path, cache_path, max_filesize())
}

/// [`build_index_with_cache`] with an explicit max file size (`None` = unlimited).
fn build_index_with_limit(
    root: &Path,
    index_path: &Path,
    cache_path: Option<&Path>,
    max: Option<u64>,
) -> Result<bool> {
    let lock_path = index_path.with_extension("lock");
    let sidecar = sidecar_path(index_path, cache_path);

    // Taken before the walk: any modification made after the walk stat'ed a
    // file has mtime >= build_start and is therefore racy or mismatching.
    let build_start_ns = now_ns();
    let walk = walk_corpus(root, &lock_path);
    let fp = FingerprintState {
        path: fingerprint_path(index_path),
        fingerprint: corpus_fingerprint(&walk, max),
        build_start_ns,
    };

    // Only an index in the current format can be kept or diff-updated.
    let current = crate::index::reader::index_version(index_path) == Some(VERSION);
    let stored = if current {
        read_fingerprint(&fp.path)
    } else {
        None
    };

    // Fast path (pre-lock = no contention): nothing changed and no file is
    // racily clean relative to the previous build.
    if let Some((stored_fp, Some(prev_start))) = stored {
        if stored_fp == fp.fingerprint && !walk.iter().any(|e| e.stat.is_racy(prev_start)) {
            // An index built before the oversize record existed gets one
            // (the check is a scan of in-memory stat data).
            if walk.iter().any(|e| exceeds_max(e.stat.size, max))
                && read_oversize(index_path).is_none()
            {
                write_oversize(index_path, &walk, max);
            }
            return Ok(false);
        }
    }

    let _lock_guard = acquire_lock(index_path)?;

    if current {
        // Previous build start; without it (legacy fingerprint file) fall back to
        // the index file's mtime, which is never earlier than the build start.
        let prev_start_ns = stored
            .and_then(|(_, s)| s)
            .or_else(|| {
                fs::metadata(index_path)
                    .ok()
                    .map(|m| FileStat::from_metadata(&m).mtime_ns)
            })
            .unwrap_or(0);
        if let Some(v) =
            try_build_index_diff(root, index_path, &sidecar, &walk, prev_start_ns, max, &fp)?
        {
            write_oversize(index_path, &walk, max);
            return Ok(v);
        }
    }

    write_oversize(index_path, &walk, max);
    full_build(root, index_path, &sidecar, walk, max)?;
    fp.write();
    Ok(true)
}

struct FileInfo {
    relative_path: String,
    stat: FileStat,
    content_hash: u64,
}

/// Full (from scratch) build: reads and indexes every file.
///
/// (Up to index v3 unchanged files' trigrams were reused from a trigram
/// cache. The v4 sidecar no longer stores masks, which live only in the
/// posting lists, so a full build re-reads everything; most updates take the
/// diff path instead.)
fn full_build(
    root: &Path,
    index_path: &Path,
    sidecar: &Path,
    walk: Vec<WalkEntry>,
    max: Option<u64>,
) -> Result<()> {
    struct Indexed {
        content_hash: u64,
        grams: Vec<Gram>,
    }

    let t0 = std::time::Instant::now();
    let results: Vec<ReadOutcome<Indexed>> = walk
        .par_iter()
        .map(|e| {
            if exceeds_max(e.stat.size, max) {
                debug_log!("skipping {} ({} bytes > max filesize)", e.rel, e.stat.size);
                return ReadOutcome::TooLarge;
            }
            with_text_file(&root.join(&e.rel), e.stat.size, max, |content| Indexed {
                content_hash: xxhash_rust::xxh64::xxh64(content, 0),
                grams: trigram::extract_grams(content),
            })
        })
        .collect();

    let mut files: Vec<FileInfo> = Vec::with_capacity(results.len());
    let mut file_grams: Vec<Vec<Gram>> = Vec::with_capacity(results.len());
    // Known binaries, so later diff updates need not re-read them.
    let mut binaries: HashMap<String, (u64, u64)> = HashMap::new();
    for (e, r) in walk.into_iter().zip(results) {
        let r = match r {
            ReadOutcome::Text(r) => r,
            ReadOutcome::Binary => {
                binaries.insert(e.rel, (e.stat.mtime_ns, e.stat.size));
                continue;
            }
            ReadOutcome::TooLarge | ReadOutcome::Unreadable => continue,
        };
        files.push(FileInfo {
            relative_path: e.rel,
            stat: e.stat,
            content_hash: r.content_hash,
        });
        file_grams.push(r.grams);
    }

    // Sidecar first: if the main index write then fails, the id mismatch
    // makes the next build ignore the sidecar (full rebuild).
    let t1 = std::time::Instant::now();
    let sidecar_id = sidecar_id_for(&files);
    {
        // Encode in parallel, a bounded chunk at a time (keeps peak memory low).
        let mut w = SidecarWriter::create(sidecar, sidecar_id, files.len())?;
        for chunk in file_grams.chunks(4096) {
            let encoded: Vec<Result<Vec<u8>>> = chunk
                .par_iter()
                .map(|g| SidecarWriter::encode_entry(g))
                .collect();
            for e in encoded {
                w.write_raw(&e?)?;
            }
        }
        w.finish()?;
    }
    let t2 = std::time::Instant::now();
    write_full_index(index_path, &files, &file_grams, sidecar_id)?;
    debug_log!(
        "full build: read+extract {:?}, sidecar {:?}, index {:?}",
        t1 - t0,
        t2 - t1,
        t2.elapsed()
    );
    save_binary_cache(&binary_cache_path(index_path), &binaries);
    Ok(())
}

fn index_too_large(what: &str) -> XgrepError {
    XgrepError::IndexError(format!(
        "index too large for format v{}: {} exceeds the 32-bit limit",
        VERSION, what
    ))
}

/// Write `n` zero bytes without allocating `n` bytes at once.
fn write_zeros<W: Write>(w: &mut W, mut n: usize) -> Result<()> {
    let zeros = [0u8; 64 * 1024];
    while n > 0 {
        let k = n.min(zeros.len());
        w.write_all(&zeros[..k])?;
        n -= k;
    }
    Ok(())
}

/// Best-effort fsync of a directory so a completed rename survives a crash.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Write a complete v4 main index to a temp file and atomically move it into place.
///
/// Layout: header, trigram table (placeholder, filled at the end), postings
/// (written by `write_postings`, which must push one sorted `TrigramEntry` per
/// trigram), file table, string pool.
/// The temp file is fsync'ed before the rename and removed on error.
fn write_index_atomic<P>(
    index_path: &Path,
    trigram_count: usize,
    files: &[FileInfo],
    sidecar_id: u64,
    write_postings: P,
) -> Result<()>
where
    P: FnOnce(&mut BufWriter<fs::File>, &mut Vec<TrigramEntry>) -> Result<u64>,
{
    let trigram_count_u32 =
        u32::try_from(trigram_count).map_err(|_| index_too_large("unique trigram count"))?;
    let file_count_u32 = u32::try_from(files.len()).map_err(|_| index_too_large("file count"))?;

    let parent = index_path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let temp_path = parent.join(format!(".xgrep_tmp_{}", std::process::id()));

    let result = (|| -> Result<()> {
        let mut w = BufWriter::with_capacity(256 * 1024, fs::File::create(&temp_path)?);
        let mut header = Header {
            magic: MAGIC,
            version: VERSION,
            trigram_count: trigram_count_u32,
            file_count: file_count_u32,
            posting_total_bytes: 0,
            sidecar_id,
        };
        w.write_all(&header.to_bytes())?;
        write_zeros(&mut w, trigram_count * TrigramEntry::SIZE)?;

        let mut entries: Vec<TrigramEntry> = Vec::with_capacity(trigram_count);
        header.posting_total_bytes = write_postings(&mut w, &mut entries)?;
        if entries.len() != trigram_count {
            return Err(XgrepError::IndexError(format!(
                "internal error: wrote {} trigram entries, expected {}",
                entries.len(),
                trigram_count
            )));
        }

        // File Table + String Pool
        let mut string_pool: Vec<u8> = Vec::new();
        for fi in files {
            let path_offset = u32::try_from(string_pool.len())
                .map_err(|_| index_too_large("file path string pool size"))?;
            string_pool.extend_from_slice(fi.relative_path.as_bytes());
            string_pool.push(0);
            w.write_all(
                &FileEntry {
                    path_offset,
                    mtime: fi.stat.mtime_ns,
                    size: fi.stat.size,
                    content_hash: fi.content_hash,
                }
                .to_bytes(),
            )?;
        }
        w.write_all(&string_pool)?;
        drop(string_pool);

        w.flush()?;
        let mut file = w.into_inner().map_err(|e| XgrepError::Io(e.into_error()))?;

        file.seek(SeekFrom::Start(0))?;
        file.write_all(&header.to_bytes())?;
        {
            let mut tw = BufWriter::with_capacity(64 * 1024, &mut file);
            for entry in &entries {
                tw.write_all(&entry.to_bytes())?;
            }
            tw.flush()?;
        }
        // Make the data durable before the rename publishes it; otherwise a
        // crash could leave a renamed-but-empty/truncated index.
        file.sync_all()?;
        Ok(())
    })();

    if let Err(e) = result {
        let _ = fs::remove_file(&temp_path);
        return Err(e);
    }
    fs::rename(&temp_path, index_path)?;
    sync_dir(parent);
    Ok(())
}

/// Reinterpret a `u32` slice as atomics for lock-free parallel counting.
fn as_atomic_u32(s: &mut [u32]) -> &[AtomicU32] {
    // SAFETY: AtomicU32 has the same size, alignment and bit validity as u32,
    // and the exclusive borrow guarantees no non-atomic access while the
    // returned shared slice is alive.
    unsafe { &*(s as *mut [u32] as *const [AtomicU32]) }
}

/// Reinterpret a `u64` slice as atomics for lock-free parallel bit setting.
fn as_atomic_u64(s: &mut [u64]) -> &[AtomicU64] {
    // SAFETY: AtomicU64 has the same size and bit validity as u64; on the
    // platforms where AtomicU64 exists its alignment equals u64's (8), and the
    // exclusive borrow guarantees no non-atomic access meanwhile.
    unsafe { &*(s as *mut [u64] as *const [AtomicU64]) }
}

/// Dense numbering of the trigrams that occur in the corpus.
///
/// `bits` is a 2^24-bit presence bitset (2 MiB) and `rank[w]` the number of set
/// bits before word `w` (1 MiB), so a trigram's dense index is one table
/// lookup plus a popcount. This lets every per-trigram array be sized by the
/// number of *distinct* trigrams instead of 2^24 entries.
struct DenseTrigrams {
    bits: Vec<u64>,
    rank: Vec<u32>,
    /// Distinct trigrams, ascending (dense index → trigram).
    list: Vec<u32>,
}

impl DenseTrigrams {
    fn build(file_grams: &[Vec<Gram>]) -> Self {
        let mut bits = vec![0u64; TRIGRAM_SPACE / 64];
        {
            let atomic = as_atomic_u64(&mut bits);
            file_grams.par_iter().for_each(|gs| {
                for g in gs {
                    let t = g.key();
                    atomic[(t >> 6) as usize].fetch_or(1u64 << (t & 63), Ordering::Relaxed);
                }
            });
        }
        let mut rank = Vec::with_capacity(bits.len());
        let mut list = Vec::new();
        let mut acc = 0u32;
        for (w, &word) in bits.iter().enumerate() {
            rank.push(acc);
            acc += word.count_ones();
            let mut x = word;
            while x != 0 {
                list.push(((w as u32) << 6) | x.trailing_zeros());
                x &= x - 1;
            }
        }
        Self { bits, rank, list }
    }

    #[inline]
    fn dense(&self, t: u32) -> usize {
        let w = (t >> 6) as usize;
        let below = self.bits[w] & ((1u64 << (t & 63)) - 1);
        self.rank[w] as usize + below.count_ones() as usize
    }
}

/// Postings for one contiguous trigram range, encoded and ready to append.
struct RangeOut {
    bytes: Vec<u8>,
    /// (trigram as u32, encoded posting length)
    lens: Vec<(u32, u32)>,
}

/// Build and encode the posting lists for the distinct trigrams with dense
/// indices `[dlo, dlo + counts.len())`.
///
/// `counts` holds those trigrams' file counts and is reused in place as the
/// scatter cursor array. Files are visited in ID order, so every posting list
/// comes out sorted without a sort.
fn build_range(
    dlo: usize,
    counts: &mut [u32],
    dt: &DenseTrigrams,
    file_grams: &[Vec<Gram>],
) -> Result<RangeOut> {
    let mut out = RangeOut {
        bytes: Vec::new(),
        lens: Vec::with_capacity(counts.len()),
    };
    if counts.is_empty() {
        return Ok(out);
    }
    let t_lo = dt.list[dlo];
    let t_hi = dt.list[dlo + counts.len() - 1]; // inclusive

    // Exclusive prefix sum, relative to the start of this range.
    let mut acc: u32 = 0;
    for c in counts.iter_mut() {
        let v = *c;
        *c = acc;
        acc = acc
            .checked_add(v)
            .ok_or_else(|| index_too_large("trigram occurrences in one range"))?;
    }
    let mut ids: Vec<u32> = vec![0; acc as usize];
    let mut masks: Vec<u16> = vec![0; acc as usize];
    for (fid, gs) in file_grams.iter().enumerate() {
        let start = gs.partition_point(|g| g.key() < t_lo);
        for g in &gs[start..] {
            let t = g.key();
            if t > t_hi {
                break;
            }
            let slot = &mut counts[dt.dense(t) - dlo];
            ids[*slot as usize] = fid as u32;
            masks[*slot as usize] = pack_masks(g.loc, g.next);
            *slot += 1;
        }
    }

    // After the scatter, counts[i] is the end of the i-th trigram's list.
    out.bytes.reserve(acc as usize * 3);
    let mut prev_end: u32 = 0;
    for (i, &end) in counts.iter().enumerate() {
        let (a, b) = (prev_end as usize, end as usize);
        let before = out.bytes.len();
        encode_posting(&mut out.bytes, &ids[a..b], &masks[a..b]);
        let len = u32::try_from(out.bytes.len() - before)
            .map_err(|_| index_too_large("posting list size"))?;
        out.lens.push((dt.list[dlo + i], len));
        prev_end = end;
    }
    Ok(out)
}

/// Core full main-index write.
///
/// `file_grams[i]` must be sorted by trigram (unique). All per-trigram state
/// lives in flat arrays indexed by dense trigram number (no hashing);
/// counting is parallel (atomic increments), and posting lists are built per
/// trigram range in parallel, a bounded number of ranges at a time, so no
/// temporary posting file is needed.
fn write_full_index(
    index_path: &Path,
    files: &[FileInfo],
    file_grams: &[Vec<Gram>],
    sidecar_id: u64,
) -> Result<()> {
    if files.len() > u32::MAX as usize {
        return Err(index_too_large("file count"));
    }

    // 1. Distinct trigrams and per-trigram file counts.
    let dt = DenseTrigrams::build(file_grams);
    let unique = dt.list.len();
    let mut counts: Vec<u32> = vec![0; unique];
    {
        let atomic = as_atomic_u32(&mut counts);
        file_grams.par_iter().for_each(|gs| {
            for g in gs {
                atomic[dt.dense(g.key())].fetch_add(1, Ordering::Relaxed);
            }
        });
    }

    // 2. Range boundaries balanced by occurrence count.
    let total_pairs: usize = file_grams.iter().map(|t| t.len()).sum();
    let threads = rayon::current_num_threads().max(1);
    let target = (total_pairs / (threads * 8)).clamp(1 << 16, 1 << 24);
    let mut bounds: Vec<usize> = vec![0];
    let mut acc = 0usize;
    for (i, &c) in counts.iter().enumerate() {
        acc += c as usize;
        if acc >= target {
            bounds.push(i + 1);
            acc = 0;
        }
    }
    if *bounds.last().unwrap() != unique {
        bounds.push(unique);
    }

    // 3. Split the count array into disjoint per-range slices.
    let mut ranges: Vec<(usize, &mut [u32])> = Vec::with_capacity(bounds.len());
    {
        let mut rest: &mut [u32] = &mut counts;
        for w in bounds.windows(2) {
            let (head, tail) = rest.split_at_mut(w[1] - w[0]);
            ranges.push((w[0], head));
            rest = tail;
        }
    }

    // Ranges in flight at once (bounds peak memory of the scatter buffers).
    let batch = (threads * 2).max(1);

    write_index_atomic(index_path, unique, files, sidecar_id, |w, entries| {
        let mut offset: u64 = 0;
        for chunk in ranges.chunks_mut(batch) {
            let outs: Vec<Result<RangeOut>> = chunk
                .par_iter_mut()
                .map(|(dlo, slice)| build_range(*dlo, slice, &dt, file_grams))
                .collect();
            for out in outs {
                let out = out?;
                w.write_all(&out.bytes)?;
                for (t, len) in out.lens {
                    entries.push(TrigramEntry {
                        trigram: u32_to_trigram(t),
                        _padding: 0,
                        posting_offset: offset,
                        posting_len: len,
                    });
                    offset += len as u64;
                }
            }
        }
        Ok(offset)
    })
}

/// Set of trigrams backed by a 2^24-bit bitset plus an insertion list.
struct TrigramSet {
    bits: Vec<u64>,
    list: Vec<u32>,
}

impl TrigramSet {
    fn new() -> Self {
        Self {
            bits: vec![0; TRIGRAM_SPACE / 64],
            list: Vec::new(),
        }
    }
    fn insert(&mut self, t: u32) {
        let (w, b) = ((t >> 6) as usize, 1u64 << (t & 63));
        if self.bits[w] & b == 0 {
            self.bits[w] |= b;
            self.list.push(t);
        }
    }
    fn contains(&self, t: u32) -> bool {
        self.bits[(t >> 6) as usize] & (1u64 << (t & 63)) != 0
    }
}

fn too_many_changes(changes: usize, old_file_count: usize) -> bool {
    changes * 100 > old_file_count.max(1) * DIFF_FALLBACK_PERCENT
}

/// One posting edit: (trigram, new file id, is_add, packed masks).
type PostingOp = (u32, u32, bool, u16);

/// Apply sorted posting edits for one trigram to a sorted posting list
/// (`ids[i]` has masks `masks[i]`). When a file has several edits, the last
/// one (an addition, since `false < true`) wins.
fn apply_posting_ops(ids: &[u32], masks: &[u16], ops: &[PostingOp]) -> (Vec<u32>, Vec<u16>) {
    let mut out = Vec::with_capacity(ids.len() + ops.len());
    let mut out_m = Vec::with_capacity(ids.len() + ops.len());
    let (mut i, mut j) = (0usize, 0usize);
    while j < ops.len() {
        let fid = ops[j].1;
        let mut add = None;
        while j < ops.len() && ops[j].1 == fid {
            add = ops[j].2.then_some(ops[j].3);
            j += 1;
        }
        while i < ids.len() && ids[i] < fid {
            out.push(ids[i]);
            out_m.push(masks[i]);
            i += 1;
        }
        if i < ids.len() && ids[i] == fid {
            i += 1;
        }
        if let Some(m) = add {
            out.push(fid);
            out_m.push(m);
        }
    }
    out.extend_from_slice(&ids[i..]);
    out_m.extend_from_slice(&masks[i..]);
    (out, out_m)
}

/// A changed or new file that was read and indexed during a diff update.
struct DiffIndexed {
    stat: FileStat,
    content_hash: u64,
    /// Sorted by trigram.
    grams: Vec<Gram>,
}

/// Source of one sidecar entry of the updated index.
enum SideSrc<'a> {
    /// Unchanged file: entry copied verbatim from the old sidecar.
    Raw(&'a [u8]),
    /// Changed or new file.
    New(&'a DiffIndexed),
}

/// Try to perform a diff-based index update.
/// Returns Some(rebuilt) on success, None if fallback to full rebuild is needed.
fn try_build_index_diff(
    root: &Path,
    index_path: &Path,
    sidecar: &Path,
    walk: &[WalkEntry],
    prev_start_ns: u64,
    max: Option<u64>,
    fp: &FingerprintState,
) -> Result<Option<bool>> {
    use crate::index::reader::IndexReader;

    let reader = match IndexReader::open(index_path) {
        Ok(r) => r,
        Err(_) => return Ok(None),
    };
    let old_file_count = reader.file_count() as usize;

    // The sidecar must belong to exactly this index.
    let Some(side) = SidecarView::open(sidecar) else {
        return Ok(None);
    };
    if side.sidecar_id != reader.sidecar_id() || side.len() != old_file_count {
        return Ok(None);
    }

    // Load cache of known binary files (avoids unnecessary reads in phase 2).
    let bc_path = binary_cache_path(index_path);
    let mut binary_cache = load_binary_cache(&bc_path);
    let mut binary_cache_dirty = false;

    // path → (file_id, stored stat) from the fixed-size file table.
    struct Old {
        fid: u32,
        mtime_ns: u64,
        size: u64,
        content_hash: u64,
    }
    let mut old_by_path: HashMap<&str, Old> = HashMap::with_capacity(old_file_count);
    for fid in 0..old_file_count as u32 {
        let Some(fe) = reader.file_entry(fid) else {
            return Ok(None);
        };
        old_by_path.insert(
            reader.file_path(fid),
            Old {
                fid,
                mtime_ns: fe.mtime,
                size: fe.size,
                content_hash: fe.content_hash,
            },
        );
    }

    // ---- Phase 1: classify by stat data only (no reads).
    enum Task<'w> {
        /// Indexed file whose stat changed (`racy_only == false`) or whose stat
        /// matches but cannot be trusted because it is racily clean.
        Existing {
            e: &'w WalkEntry,
            fid: u32,
            racy_only: bool,
            old_hash: u64,
        },
        New(&'w WalkEntry),
    }
    let mut tasks: Vec<Task> = Vec::new();
    let mut seen = vec![false; old_file_count];
    let mut is_deleted = vec![false; old_file_count];
    let mut stat_changes = 0usize;
    for e in walk {
        let oversize = exceeds_max(e.stat.size, max);
        if let Some(o) = old_by_path.get(e.rel.as_str()) {
            seen[o.fid as usize] = true;
            if oversize {
                is_deleted[o.fid as usize] = true;
                stat_changes += 1;
            } else if o.mtime_ns != e.stat.mtime_ns || o.size != e.stat.size {
                stat_changes += 1;
                tasks.push(Task::Existing {
                    e,
                    fid: o.fid,
                    racy_only: false,
                    old_hash: o.content_hash,
                });
            } else if e.stat.is_racy(prev_start_ns) {
                tasks.push(Task::Existing {
                    e,
                    fid: o.fid,
                    racy_only: true,
                    old_hash: o.content_hash,
                });
            }
        } else if !oversize {
            let known_binary = binary_cache
                .get(&e.rel)
                .is_some_and(|&(m, s)| m == e.stat.mtime_ns && s == e.stat.size)
                && !e.stat.is_racy(prev_start_ns);
            if !known_binary {
                stat_changes += 1;
                tasks.push(Task::New(e));
            }
        }
    }
    for fid in 0..old_file_count {
        if !seen[fid] {
            is_deleted[fid] = true;
            stat_changes += 1;
        }
    }
    drop(old_by_path);

    // Bail out before reading anything when the change set is clearly too large.
    if too_many_changes(stat_changes, old_file_count) {
        return Ok(None);
    }

    // ---- Phase 2 (parallel): read changed/new/racy files once, hash + extract.
    enum Outcome<'w> {
        Changed(u32, DiffIndexed),
        Unchanged,
        /// Indexed file that is now binary / too large / unreadable: drop it,
        /// exactly as a full build would.
        Remove(u32),
        New(&'w WalkEntry, DiffIndexed),
        NewBinary(&'w WalkEntry),
        Skip,
    }
    let outcomes: Vec<Outcome> = tasks
        .par_iter()
        .map(|task| {
            let e = match task {
                Task::Existing { e, .. } | Task::New(e) => *e,
            };
            let outcome = with_text_file(&root.join(&e.rel), e.stat.size, max, |content| {
                (
                    xxhash_rust::xxh64::xxh64(content, 0),
                    trigram::extract_grams(content),
                )
            });
            match (task, outcome) {
                (
                    Task::Existing {
                        fid,
                        racy_only,
                        old_hash,
                        ..
                    },
                    ReadOutcome::Text((hash, grams)),
                ) => {
                    if *racy_only && hash == *old_hash {
                        Outcome::Unchanged
                    } else {
                        Outcome::Changed(
                            *fid,
                            DiffIndexed {
                                stat: e.stat,
                                content_hash: hash,
                                grams,
                            },
                        )
                    }
                }
                (Task::Existing { fid, .. }, _) => Outcome::Remove(*fid),
                (Task::New(_), ReadOutcome::Text((hash, grams))) => Outcome::New(
                    e,
                    DiffIndexed {
                        stat: e.stat,
                        content_hash: hash,
                        grams,
                    },
                ),
                (Task::New(_), ReadOutcome::Binary) => Outcome::NewBinary(e),
                (Task::New(_), _) => Outcome::Skip,
            }
        })
        .collect();
    drop(tasks);

    // changed[old_fid] = new content for changed files.
    let mut changed: Vec<Option<DiffIndexed>> = Vec::new();
    changed.resize_with(old_file_count, || None);
    let mut n_changed = 0usize;
    let mut new_files: Vec<(&WalkEntry, DiffIndexed)> = Vec::new();
    for o in outcomes {
        match o {
            Outcome::Changed(fid, d) => {
                changed[fid as usize] = Some(d);
                n_changed += 1;
            }
            Outcome::Remove(fid) => is_deleted[fid as usize] = true,
            Outcome::New(e, d) => new_files.push((e, d)),
            Outcome::NewBinary(e) => {
                binary_cache.insert(e.rel.clone(), (e.stat.mtime_ns, e.stat.size));
                binary_cache_dirty = true;
            }
            Outcome::Unchanged | Outcome::Skip => {}
        }
    }

    // Save files newly identified as binary (skip their phase 2 in subsequent runs).
    if binary_cache_dirty {
        save_binary_cache(&bc_path, &binary_cache);
    }

    let n_deleted = is_deleted.iter().filter(|&&d| d).count();
    let total_changes = n_changed + new_files.len() + n_deleted;

    // No index-relevant changes: only record the new fingerprint/build start.
    if total_changes == 0 {
        fp.write();
        return Ok(Some(false));
    }
    if too_many_changes(total_changes, old_file_count) {
        return Ok(None);
    }

    // ID remap: old_to_new is monotonic, so remapping keeps lists sorted.
    let mut old_to_new: Vec<u32> = vec![u32::MAX; old_file_count];
    let mut next_id = 0u32;
    for (old_id, slot) in old_to_new.iter_mut().enumerate() {
        if !is_deleted[old_id] {
            *slot = next_id;
            next_id += 1;
        }
    }
    let base_new_id = next_id;
    if (base_new_id as usize) + new_files.len() > u32::MAX as usize {
        return Err(index_too_large("file count"));
    }

    // Old trigrams of the files whose postings must be edited (changed files
    // and every file from the first deleted one on, whose IDs shift).
    let min_deleted = is_deleted.iter().position(|&d| d).unwrap_or(old_file_count);
    let needs_old: Vec<usize> = (0..old_file_count)
        .filter(|&fid| fid >= min_deleted || changed[fid].is_some())
        .collect();
    let old_trigrams: Vec<Option<Vec<u32>>> = needs_old
        .par_iter()
        .map(|&fid| side.trigrams(fid))
        .collect();
    if old_trigrams.iter().any(|t| t.is_none()) {
        return Ok(None); // corrupt sidecar entry
    }

    // affected: trigrams whose posting lists must be re-encoded. Posting lists
    // of all other trigrams are raw-copied from the old index.
    let mut affected = TrigramSet::new();
    for ts in old_trigrams.iter().flatten() {
        for &t in ts {
            affected.insert(t);
        }
    }
    for d in changed.iter().flatten() {
        for g in &d.grams {
            affected.insert(g.key());
        }
    }
    for (_, d) in &new_files {
        for g in &d.grams {
            affected.insert(g.key());
        }
    }

    // Posting edits as (trigram, new file id, is_add, masks), applied per
    // trigram in a single merge pass instead of O(n) mid-list inserts/removes
    // per file. For the same file a removal sorts before an addition, so
    // "removed then re-added" (trigram kept by an edit) ends up present.
    let mut ops: Vec<PostingOp> = Vec::new();
    for (&old_fid, ts) in needs_old.iter().zip(&old_trigrams) {
        let Some(d) = &changed[old_fid] else { continue };
        let new_fid = old_to_new[old_fid];
        ops.extend(ts.iter().flatten().map(|&t| (t, new_fid, false, 0)));
        ops.extend(
            d.grams
                .iter()
                .map(|g| (g.key(), new_fid, true, pack_masks(g.loc, g.next))),
        );
    }
    drop(old_trigrams);
    for (i, (_, d)) in new_files.iter().enumerate() {
        let id = base_new_id + i as u32;
        ops.extend(
            d.grams
                .iter()
                .map(|g| (g.key(), id, true, pack_masks(g.loc, g.next))),
        );
    }
    ops.par_sort_unstable();

    // Decode only affected posting lists, drop deleted IDs, remap, apply edits.
    let has_deletions = n_deleted > 0;
    let mut affected_list = std::mem::take(&mut affected.list);
    affected_list.par_sort_unstable();
    let affected_postings: Vec<(u32, Vec<u32>, Vec<u16>)> = affected_list
        .par_iter()
        .filter_map(|&t| {
            let (mut ids, mut masks) = match reader.posting_raw(u32_to_trigram(t)) {
                Some(raw) => {
                    let p = IndexReader::decode_posting(raw);
                    let masks: Vec<u16> = (0..p.ids.len()).map(|i| p.packed(i)).collect();
                    (p.ids, masks)
                }
                None => (Vec::new(), Vec::new()),
            };
            if has_deletions {
                let mut k = 0;
                for i in 0..ids.len() {
                    let id = ids[i] as usize;
                    if !is_deleted.get(id).copied().unwrap_or(true) {
                        ids[k] = old_to_new[id];
                        masks[k] = masks[i];
                        k += 1;
                    }
                }
                ids.truncate(k);
                masks.truncate(k);
            }
            let lo = ops.partition_point(|o| o.0 < t);
            let hi = ops.partition_point(|o| o.0 <= t);
            if lo < hi {
                (ids, masks) = apply_posting_ops(&ids, &masks, &ops[lo..hi]);
            }
            (!ids.is_empty()).then_some((t, ids, masks))
        })
        .collect();
    drop(ops);

    // New file list + sidecar entries.
    let mut files: Vec<FileInfo> = Vec::with_capacity(base_new_id as usize + new_files.len());
    let mut side_src: Vec<SideSrc<'_>> = Vec::with_capacity(files.capacity());
    for (old_id, slot) in changed.iter().enumerate() {
        if is_deleted[old_id] {
            continue;
        }
        let path = reader.file_path(old_id as u32).to_string();
        if let Some(d) = slot {
            files.push(FileInfo {
                relative_path: path,
                stat: d.stat,
                content_hash: d.content_hash,
            });
            side_src.push(SideSrc::New(d));
        } else {
            // Unchanged file: copy its sidecar entry verbatim.
            let fe = reader.file_entry(old_id as u32);
            let (mtime_ns, size, content_hash) = match fe {
                Some(fe) => (fe.mtime, fe.size, fe.content_hash),
                None => (0, 0, 0),
            };
            files.push(FileInfo {
                relative_path: path,
                stat: FileStat {
                    mtime_ns,
                    size,
                    ..Default::default()
                },
                content_hash,
            });
            side_src.push(SideSrc::Raw(side.raw(old_id)));
        }
    }
    for (e, d) in &new_files {
        files.push(FileInfo {
            relative_path: e.rel.clone(),
            stat: d.stat,
            content_hash: d.content_hash,
        });
        side_src.push(SideSrc::New(d));
    }

    // Sidecar first (see full_build for the ordering rationale). The old
    // sidecar stays mapped; it is replaced by rename, never overwritten.
    let sidecar_id = sidecar_id_for(&files);
    {
        let mut w = SidecarWriter::create(sidecar, sidecar_id, files.len())?;
        for src in &side_src {
            match src {
                SideSrc::Raw(raw) => w.write_raw(raw)?,
                SideSrc::New(d) => w.write_entry(&d.grams)?,
            }
        }
        w.finish()?;
    }
    drop(side_src);

    write_smart_diff_index(
        index_path,
        &reader,
        &files,
        &affected_postings,
        &affected,
        sidecar_id,
    )?;
    drop(side);
    drop(reader);

    fp.write();
    Ok(Some(true))
}

/// Smart differential index write.
///
/// 1. Only re-encode affected posting lists.
/// 2. Raw-copy unaffected posting lists from the old mmap.
///
/// Precondition: all trigrams requiring ID remap must be included in `affected`.
fn write_smart_diff_index(
    index_path: &Path,
    old_reader: &crate::index::reader::IndexReader,
    files: &[FileInfo],
    affected_postings: &[(u32, Vec<u32>, Vec<u16>)],
    affected: &TrigramSet,
    sidecar_id: u64,
) -> Result<()> {
    enum Src<'b> {
        Raw(&'b [u8]),
        New(&'b [u32], &'b [u16]),
    }
    // Final trigram list: (old - affected) ∪ non-empty affected, sorted.
    let mut final_list: Vec<(u32, Src<'_>)> = old_reader
        .all_trigram_entries_raw()
        .into_iter()
        .map(|(t, raw)| (trigram_to_u32(t), raw))
        .filter(|(t, _)| !affected.contains(*t))
        .map(|(t, raw)| (t, Src::Raw(raw)))
        .collect();
    final_list.extend(
        affected_postings
            .iter()
            .map(|(t, ids, masks)| (*t, Src::New(ids.as_slice(), masks.as_slice()))),
    );
    final_list.sort_unstable_by_key(|(t, _)| *t);

    write_index_atomic(
        index_path,
        final_list.len(),
        files,
        sidecar_id,
        |w, entries| {
            let mut posting_buf: Vec<u8> = Vec::with_capacity(4096);
            let mut offset: u64 = 0;
            for (t, src) in &final_list {
                let raw: &[u8] = match src {
                    Src::New(ids, masks) => {
                        posting_buf.clear();
                        encode_posting(&mut posting_buf, ids, masks);
                        &posting_buf
                    }
                    Src::Raw(r) => r,
                };
                let len =
                    u32::try_from(raw.len()).map_err(|_| index_too_large("posting list size"))?;
                w.write_all(raw)?;
                entries.push(TrigramEntry {
                    trigram: u32_to_trigram(*t),
                    _padding: 0,
                    posting_offset: offset,
                    posting_len: len,
                });
                offset += len as u64;
            }
            Ok(offset)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::cache::cache_path_for;
    use std::fs;
    use tempfile::tempdir;

    /// trigram → [(path, packed masks)] for the whole index.
    fn collect_trigram_to_paths(
        reader: &crate::index::reader::IndexReader,
    ) -> std::collections::HashMap<[u8; 3], Vec<(String, u16)>> {
        use crate::index::reader::IndexReader;
        let mut result = std::collections::HashMap::new();
        for t in reader.all_trigrams() {
            let p = IndexReader::decode_posting(reader.posting_raw(t).unwrap());
            let paths: Vec<(String, u16)> = p
                .ids
                .iter()
                .enumerate()
                .map(|(i, &fid)| (reader.file_path(fid).to_string(), p.packed(i)))
                .collect();
            result.insert(t, paths);
        }
        result
    }

    pub(super) fn assert_diff_eq_full(dir: &Path, index_path: &Path) {
        use crate::index::reader::IndexReader;

        // Read the diff-update result.
        let diff_reader = IndexReader::open(index_path).unwrap();
        let mut diff_map = collect_trigram_to_paths(&diff_reader);
        // Sort paths to make them comparable.
        for v in diff_map.values_mut() {
            v.sort();
        }
        drop(diff_reader);

        // Perform a full rebuild.
        let full_index_path = dir.join("full_rebuild.xgrep");
        build_index(dir, &full_index_path).unwrap();

        let full_reader = IndexReader::open(&full_index_path).unwrap();
        let mut full_map = collect_trigram_to_paths(&full_reader);
        for v in full_map.values_mut() {
            v.sort();
        }

        assert_eq!(
            diff_map, full_map,
            "diff update result does not match full rebuild"
        );
    }

    #[test]
    fn test_build_index_creates_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("hello.txt"), "hello world").unwrap();
        fs::write(root.join("foo.rs"), "fn main() {}").unwrap();

        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        assert!(index_path.exists());
        let data = fs::read(&index_path).unwrap();
        assert!(data.len() > Header::SIZE);
        assert_eq!(&data[0..4], b"XGRP");
    }

    #[test]
    fn test_build_index_header() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "abcdef").unwrap();

        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        let data = fs::read(&index_path).unwrap();
        let header = crate::index::reader::read_header(&data[..Header::SIZE]);
        assert_eq!(&header.magic, b"XGRP");
        assert_eq!(header.version, VERSION);
        assert_eq!(header.file_count, 1);
        assert!(header.trigram_count > 0);
    }

    #[test]
    fn test_build_respects_gitignore() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        // Create .gitignore
        fs::write(root.join(".gitignore"), "ignored_dir/\n*.log\n").unwrap();

        // Create files
        fs::write(root.join("real.txt"), "hello world").unwrap();
        fs::create_dir(root.join("ignored_dir")).unwrap();
        fs::write(root.join("ignored_dir/secret.txt"), "should be ignored").unwrap();
        fs::write(root.join("debug.log"), "should be ignored").unwrap();

        // Need to init git repo for .gitignore to work
        crate::git::git_cmd()
            .args(["init"])
            .current_dir(root)
            .output()
            .unwrap();

        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        let data = fs::read(&index_path).unwrap();
        let header = crate::index::reader::read_header(&data[..Header::SIZE]);
        // Only real.txt should be indexed (not .gitignore, not ignored files)
        assert_eq!(header.file_count, 1);
    }

    #[test]
    fn test_build_empty_directory() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // No files at all
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();
        let data = fs::read(&index_path).unwrap();
        let header = crate::index::reader::read_header(&data[..Header::SIZE]);
        assert_eq!(header.file_count, 0);
        assert_eq!(header.trigram_count, 0);
    }

    #[test]
    fn test_build_skips_binary_files() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // Binary file (contains NUL byte)
        fs::write(root.join("binary.bin"), b"hello\x00world").unwrap();
        fs::write(root.join("text.txt"), "hello world").unwrap();
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();
        let data = fs::read(&index_path).unwrap();
        let header = crate::index::reader::read_header(&data[..Header::SIZE]);
        assert_eq!(header.file_count, 1); // only text.txt
    }

    #[test]
    fn test_build_empty_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("empty.txt"), "").unwrap();
        fs::write(root.join("real.txt"), "hello world").unwrap();
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();
        let data = fs::read(&index_path).unwrap();
        let header = crate::index::reader::read_header(&data[..Header::SIZE]);
        // Empty file has no trigrams but is still indexed
        assert_eq!(header.file_count, 2);
    }

    #[test]
    fn test_build_file_shorter_than_3_bytes() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("tiny.txt"), "ab").unwrap();
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();
        let data = fs::read(&index_path).unwrap();
        let header = crate::index::reader::read_header(&data[..Header::SIZE]);
        assert_eq!(header.file_count, 1);
        // Only the end-of-file sentinel trigram "ab\0" (see trigram::SENTINEL).
        assert_eq!(header.trigram_count, 1);
    }

    #[test]
    fn test_build_nested_directories() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("a/b/c")).unwrap();
        fs::write(root.join("a/b/c/deep.txt"), "deep file content").unwrap();
        fs::write(root.join("top.txt"), "top level").unwrap();
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();
        let data = fs::read(&index_path).unwrap();
        let header = crate::index::reader::read_header(&data[..Header::SIZE]);
        assert_eq!(header.file_count, 2);
    }

    #[test]
    fn test_build_utf8_content() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("japanese.txt"), "これは日本語のテストです").unwrap();
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();
        let data = fs::read(&index_path).unwrap();
        let header = crate::index::reader::read_header(&data[..Header::SIZE]);
        assert_eq!(header.file_count, 1);
        assert!(header.trigram_count > 0);
    }

    #[test]
    fn test_build_skips_dotgit() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join(".git/config"), "git config content here").unwrap();
        fs::write(root.join("real.txt"), "hello world").unwrap();

        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        let data = fs::read(&index_path).unwrap();
        let header = crate::index::reader::read_header(&data[..Header::SIZE]);
        assert_eq!(header.file_count, 1);
    }

    #[test]
    fn test_build_with_cache_creates_cache_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("hello.txt"), "hello world").unwrap();

        let index_path = root.join("index.xgrep");
        let cache_path = cache_path_for(&index_path);
        build_index_with_cache(root, &index_path, Some(&cache_path)).unwrap();

        assert!(index_path.exists());
        assert!(cache_path.exists());

        // The sidecar belongs to the index and has one entry per file.
        let side = SidecarView::open(&cache_path).unwrap();
        let reader = crate::index::reader::IndexReader::open(&index_path).unwrap();
        assert_eq!(side.len(), 1);
        assert_eq!(side.sidecar_id, reader.sidecar_id());
        let expect: Vec<u32> = trigram::extract_grams(b"hello world")
            .iter()
            .map(|g| g.key())
            .collect();
        assert_eq!(side.trigrams(0).unwrap(), expect);
    }

    #[test]
    fn test_build_with_cache_incremental_produces_same_index() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world foo bar").unwrap();
        fs::write(root.join("b.txt"), "another file content here").unwrap();

        let index_path = root.join("index.xgrep");
        let cache_path = cache_path_for(&index_path);

        // First build (no cache)
        build_index_with_cache(root, &index_path, Some(&cache_path)).unwrap();
        let index_data_1 = fs::read(&index_path).unwrap();

        // Second build (with cache, no file changes)
        build_index_with_cache(root, &index_path, Some(&cache_path)).unwrap();
        let index_data_2 = fs::read(&index_path).unwrap();

        // Verify index contents are identical
        assert_eq!(index_data_1, index_data_2);
    }

    #[test]
    fn test_build_with_cache_after_file_change() {
        use crate::index::reader::IndexReader;

        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world").unwrap();
        fs::write(root.join("b.txt"), "unchanged content here").unwrap();

        let index_path = root.join("index.xgrep");
        let cache_path = cache_path_for(&index_path);

        // First build
        build_index_with_cache(root, &index_path, Some(&cache_path)).unwrap();

        let reader1 = IndexReader::open(&index_path).unwrap();
        assert_eq!(reader1.file_count(), 2);

        // Modify a.txt
        // Wait briefly to ensure mtime changes
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(root.join("a.txt"), "modified content xyz").unwrap();

        // Incremental build (b.txt cache hit, a.txt re-read)
        build_index_with_cache(root, &index_path, Some(&cache_path)).unwrap();

        let reader2 = IndexReader::open(&index_path).unwrap();
        assert_eq!(reader2.file_count(), 2);

        // Verify "xyz" trigram is found
        let posting = reader2.lookup_trigram(*b"xyz");
        assert!(
            !posting.is_empty(),
            "changed file content should be indexed"
        );
    }

    #[test]
    fn test_build_with_cache_file_added() {
        use crate::index::reader::IndexReader;

        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world").unwrap();

        let index_path = root.join("index.xgrep");
        let cache_path = cache_path_for(&index_path);

        build_index_with_cache(root, &index_path, Some(&cache_path)).unwrap();
        let reader1 = IndexReader::open(&index_path).unwrap();
        assert_eq!(reader1.file_count(), 1);

        // Add a new file
        fs::write(root.join("b.txt"), "new file zqx").unwrap();

        build_index_with_cache(root, &index_path, Some(&cache_path)).unwrap();
        let reader2 = IndexReader::open(&index_path).unwrap();
        assert_eq!(reader2.file_count(), 2);
    }

    #[test]
    fn test_concurrent_build_lock() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello").unwrap();
        let index_path = root.join("index.xgrep");

        // Hold the lock (simulating a concurrent build).
        let guard = acquire_lock(&index_path).unwrap();

        // Build should fail because the lock is held
        let result = build_index(root, &index_path);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("already in progress"));

        // Release lock
        drop(guard);

        // Now build should succeed
        let result = build_index(root, &index_path);
        assert!(result.is_ok());
    }

    #[test]
    #[cfg(unix)]
    fn test_stale_lock_recovery() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello").unwrap();
        let index_path = root.join("index.xgrep");

        // Create a lock file with a non-existent PID
        let lock_path = index_path.with_extension("lock");
        fs::write(&lock_path, "999999999").unwrap();

        // Build should succeed (stale lock recovered via PID check)
        let result = build_index(root, &index_path);
        assert!(result.is_ok());

        // Lock file should be cleaned up
        assert!(!lock_path.exists());
    }

    #[test]
    fn test_build_with_cache_file_deleted() {
        use crate::index::reader::IndexReader;

        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world").unwrap();
        fs::write(root.join("b.txt"), "goodbye world").unwrap();

        let index_path = root.join("index.xgrep");
        let cache_path = cache_path_for(&index_path);

        build_index_with_cache(root, &index_path, Some(&cache_path)).unwrap();
        let reader1 = IndexReader::open(&index_path).unwrap();
        assert_eq!(reader1.file_count(), 2);

        // Delete b.txt
        fs::remove_file(root.join("b.txt")).unwrap();

        build_index_with_cache(root, &index_path, Some(&cache_path)).unwrap();
        let reader2 = IndexReader::open(&index_path).unwrap();
        assert_eq!(reader2.file_count(), 1);

        // Verify it was also removed from the sidecar
        let side = SidecarView::open(&cache_path).unwrap();
        assert_eq!(side.len(), 1);
        assert_eq!(side.sidecar_id, reader2.sidecar_id());
    }

    // ----------------------------------------------------------------
    // Fingerprint / early-return tests
    // ----------------------------------------------------------------

    #[test]
    fn test_second_init_unchanged_returns_false() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world").unwrap();
        fs::write(root.join("b.txt"), "another file").unwrap();

        let index_path = root.join("index.xgrep");

        let first = build_index(root, &index_path).unwrap();
        assert!(first, "first build should return true (rebuilt)");

        let second = build_index(root, &index_path).unwrap();
        assert!(
            !second,
            "second build with no changes should return false (up to date)"
        );
    }

    #[test]
    fn test_init_after_file_modification_returns_true() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world").unwrap();

        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        // Wait for mtime to advance, then modify the file.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(root.join("a.txt"), "modified content xyz").unwrap();

        let rebuilt = build_index(root, &index_path).unwrap();
        assert!(
            rebuilt,
            "build after file change should return true (rebuilt)"
        );
    }

    #[test]
    fn test_init_after_new_file_returns_true() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world").unwrap();

        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        fs::write(root.join("b.txt"), "new file content").unwrap();

        let rebuilt = build_index(root, &index_path).unwrap();
        assert!(rebuilt, "build after new file should return true (rebuilt)");
    }

    #[test]
    fn test_init_after_file_deletion_returns_true() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world").unwrap();
        fs::write(root.join("b.txt"), "will be deleted").unwrap();

        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        fs::remove_file(root.join("b.txt")).unwrap();

        let rebuilt = build_index(root, &index_path).unwrap();
        assert!(
            rebuilt,
            "build after file deletion should return true (rebuilt)"
        );
    }

    #[test]
    fn test_fingerprint_file_created_after_build() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello").unwrap();

        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        let fp_path = fingerprint_path(&index_path);
        assert!(
            fp_path.exists(),
            "fingerprint file should be created after build"
        );
        assert_eq!(
            fs::read(&fp_path).unwrap().len(),
            16,
            "fingerprint file should be 16 bytes (fingerprint + build start)"
        );
    }

    // ----------------------------------------------------------------
    // Diff update tests
    // ----------------------------------------------------------------

    #[test]
    fn test_diff_update_modify_file_eq_full_rebuild() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world foo").unwrap();
        fs::write(root.join("b.txt"), "another content here").unwrap();
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(root.join("b.txt"), "completely different xyz").unwrap();
        build_index_with_cache(root, &index_path, None).unwrap();

        assert_diff_eq_full(root, &index_path);
    }

    #[test]
    fn test_diff_update_add_file_eq_full_rebuild() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world foo").unwrap();
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        fs::write(root.join("b.txt"), "new file content xyz").unwrap();
        build_index_with_cache(root, &index_path, None).unwrap();

        assert_diff_eq_full(root, &index_path);
    }

    #[test]
    fn test_diff_update_delete_file_eq_full_rebuild() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world foo").unwrap();
        fs::write(root.join("b.txt"), "another content here").unwrap();
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        fs::remove_file(root.join("b.txt")).unwrap();
        build_index_with_cache(root, &index_path, None).unwrap();

        assert_diff_eq_full(root, &index_path);
    }

    #[test]
    fn test_diff_update_double_modify_eq_full_rebuild() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world foo").unwrap();
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(root.join("a.txt"), "first modification abc").unwrap();
        build_index_with_cache(root, &index_path, None).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(root.join("a.txt"), "second modification def").unwrap();
        build_index_with_cache(root, &index_path, None).unwrap();

        assert_diff_eq_full(root, &index_path);
    }

    #[test]
    fn test_diff_update_all_files_changed_uses_fallback() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world foo").unwrap();
        fs::write(root.join("b.txt"), "another content here").unwrap();
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(root.join("a.txt"), "changed a content xyz").unwrap();
        fs::write(root.join("b.txt"), "changed b content qrs").unwrap();

        // Returns Ok(true) even when fallback (full build) is triggered.
        let result = build_index_with_cache(root, &index_path, None).unwrap();
        assert!(result);

        // Verify the index is correctly updated.
        use crate::index::reader::IndexReader;
        let reader = IndexReader::open(&index_path).unwrap();
        assert!(!reader.lookup_trigram(*b"xyz").is_empty());
    }

    #[test]
    fn test_diff_update_noop_trigrams_eq_full_rebuild() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // File change that does not alter the trigram set (e.g. short content, same bytes rewritten).
        fs::write(root.join("a.txt"), "abcdef").unwrap();
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1100));
        // Write the same content so the trigrams are identical (only mtime changes).
        // The diff update detects a mtime change but the trigrams are the same.
        fs::write(root.join("a.txt"), "abcdef").unwrap();
        build_index_with_cache(root, &index_path, None).unwrap();

        assert_diff_eq_full(root, &index_path);
    }

    #[test]
    fn test_diff_update_last_file_for_trigram_deleted() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // Only b.txt contains the "zzz" trigram.
        fs::write(root.join("a.txt"), "hello world foo").unwrap();
        fs::write(root.join("b.txt"), "zzz unique trigram").unwrap();
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        use crate::index::reader::IndexReader;
        {
            let reader = IndexReader::open(&index_path).unwrap();
            assert!(!reader.lookup_trigram(*b"zzz").is_empty());
        }

        fs::remove_file(root.join("b.txt")).unwrap();
        build_index_with_cache(root, &index_path, None).unwrap();

        let reader = IndexReader::open(&index_path).unwrap();
        // Must not panic and must return an empty list.
        let result = reader.lookup_trigram(*b"zzz");
        assert!(result.is_empty());
    }

    // --- bincache correctness tests ---
    //
    // bincache invariant: the index after a diff update must be identical to a full rebuild.
    // The bincache code path (binary classification of new_candidates) follows the same rule.
    //
    // Known limitation: a file that changes from binary to text while keeping the same mtime
    // and size will be incorrectly skipped by bincache (same assumption as OS mtime precision).
    // This is the same trade-off git uses with `--assume-unchanged`.

    // Verify that diff update does not crash and produces a result matching full rebuild
    // even when bincache is absent or corrupted.
    #[test]
    fn test_bincache_missing_does_not_crash() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // text + binary
        fs::write(root.join("text.txt"), "hello world").unwrap();
        fs::write(root.join("binary.bin"), b"data\x00null").unwrap();

        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        // Diff update without bincache.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(root.join("text.txt"), "hello world updated").unwrap();
        build_index_with_cache(root, &index_path, None).unwrap();

        assert_diff_eq_full(root, &index_path);
    }

    // When the mtime of a binary file recorded in bincache changes,
    // it becomes a cache miss and is re-peeked. Still binary → still excluded from index.
    #[test]
    fn test_bincache_binary_mtime_changed_still_excluded() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("text.txt"), "hello world unique_abc").unwrap();
        fs::write(root.join("binary.bin"), b"data\x00null").unwrap();

        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        // 1st diff: binary.bin is a new_candidate → recorded in bincache.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(root.join("text.txt"), "hello world unique_abc v2").unwrap();
        build_index_with_cache(root, &index_path, None).unwrap();

        // Change binary.bin's mtime (triggers a bincache miss).
        std::thread::sleep(std::time::Duration::from_millis(1100));
        // Content remains binary; only mtime is updated.
        fs::write(root.join("binary.bin"), b"data\x00null").unwrap();
        build_index_with_cache(root, &index_path, None).unwrap();

        // Binary is still excluded from index → matches full rebuild.
        assert_diff_eq_full(root, &index_path);
    }

    // When a file changes from binary to text and mtime is updated,
    // bincache miss → re-peek → classified as text → correctly included in index.
    #[test]
    fn test_bincache_binary_to_text_reindexed() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("text.txt"), "hello world").unwrap();
        fs::write(root.join("became_text.bin"), b"was\x00binary").unwrap();

        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        // 1st diff: became_text.bin is a new_candidate → recorded in bincache.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(root.join("text.txt"), "hello world v2").unwrap();
        build_index_with_cache(root, &index_path, None).unwrap();

        // Change became_text.bin to text content (mtime also updated).
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(root.join("became_text.bin"), "now text content qzx").unwrap();
        build_index_with_cache(root, &index_path, None).unwrap();

        // Now text, so "qzx" should be searchable → matches full rebuild.
        use crate::index::reader::IndexReader;
        let reader = IndexReader::open(&index_path).unwrap();
        let qzx = reader.lookup_trigram(*b"qzx");
        assert!(
            !qzx.is_empty(),
            "content after binary→text conversion must be searchable"
        );
        drop(reader);

        assert_diff_eq_full(root, &index_path);
    }

    // When a file changes from text to binary and mtime is updated,
    // old trigrams are removed from posting lists; when the last file for a trigram
    // is excluded, the posting list becomes empty.
    #[test]
    fn test_bincache_text_to_binary_posting_list_emptied() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // Only became_binary.txt contains the "qqq" trigram.
        fs::write(root.join("became_binary.txt"), "qqq unique trigram").unwrap();
        fs::write(root.join("other.txt"), "hello world").unwrap();

        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        use crate::index::reader::IndexReader;
        {
            let reader = IndexReader::open(&index_path).unwrap();
            assert!(
                !reader.lookup_trigram(*b"qqq").is_empty(),
                "qqq must exist after initial build"
            );
        }

        // Change became_binary.txt to binary content.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(root.join("became_binary.txt"), b"binary\x00content").unwrap();
        build_index_with_cache(root, &index_path, None).unwrap();

        let reader = IndexReader::open(&index_path).unwrap();
        let result = reader.lookup_trigram(*b"qqq");
        assert!(
            result.is_empty(),
            "after text→binary, the qqq posting list must be empty"
        );
        drop(reader);

        assert_diff_eq_full(root, &index_path);
    }

    // ----------------------------------------------------------------
    // Racy mtime (same-second edits), issue 1-3
    // ----------------------------------------------------------------

    fn paths_with_trigram(index_path: &Path, t: [u8; 3]) -> Vec<String> {
        let reader = crate::index::reader::IndexReader::open(index_path).unwrap();
        let mut v: Vec<String> = reader
            .lookup_trigram(t)
            .iter()
            .map(|&f| reader.file_path(f).to_string())
            .collect();
        v.sort();
        v
    }

    fn set_mtime(path: &Path, t: std::time::SystemTime) {
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    #[test]
    fn test_same_second_edit_with_different_size_is_indexed() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "original content").unwrap();
        fs::write(root.join("other.txt"), "unrelated file").unwrap();
        let index_path = root.join("index.xgrep");
        assert!(build_index(root, &index_path).unwrap());

        // Immediately (same second) rewrite with different size and content.
        fs::write(root.join("a.txt"), "qzx brand new and longer content").unwrap();
        assert!(build_index(root, &index_path).unwrap());

        assert_eq!(paths_with_trigram(&index_path, *b"qzx"), vec!["a.txt"]);
        assert!(paths_with_trigram(&index_path, *b"rig").is_empty());
        assert_diff_eq_full(root, &index_path);
    }

    #[test]
    fn test_racy_edit_with_identical_mtime_and_size_is_indexed() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let a = root.join("a.txt");
        fs::write(&a, "aaaa content").unwrap();
        for i in 0..30 {
            fs::write(root.join(format!("f{i}.txt")), format!("filler {i}")).unwrap();
        }
        // An mtime at/after the build start makes the file racily clean.
        let m = std::time::SystemTime::now() + std::time::Duration::from_secs(30);
        set_mtime(&a, m);
        let index_path = root.join("index.xgrep");
        assert!(build_index(root, &index_path).unwrap());

        // Same size, same mtime, different content: stat data alone cannot tell.
        fs::write(&a, "qzxq content").unwrap();
        set_mtime(&a, m);
        assert!(
            build_index(root, &index_path).unwrap(),
            "edit must be detected"
        );
        assert_eq!(paths_with_trigram(&index_path, *b"qzx"), vec!["a.txt"]);
        assert!(paths_with_trigram(&index_path, *b"aaa").is_empty());
        assert_diff_eq_full(root, &index_path);

        // Re-verification by hash finds nothing new: no rebuild.
        assert!(!build_index(root, &index_path).unwrap());
    }

    #[test]
    fn test_racy_edit_not_served_from_trigram_cache() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let a = root.join("a.txt");
        fs::write(&a, "aaaa content").unwrap();
        let m = std::time::SystemTime::now() + std::time::Duration::from_secs(30);
        set_mtime(&a, m);
        let index_path = root.join("index.xgrep");
        let cache_path = cache_path_for(&index_path);
        build_index_with_cache(root, &index_path, Some(&cache_path)).unwrap();

        // Single file changed => over the diff threshold => full build with cache.
        fs::write(&a, "qzxq content").unwrap();
        set_mtime(&a, m);
        build_index_with_cache(root, &index_path, Some(&cache_path)).unwrap();
        assert_eq!(paths_with_trigram(&index_path, *b"qzx"), vec!["a.txt"]);
        assert!(paths_with_trigram(&index_path, *b"aaa").is_empty());
    }

    #[test]
    fn test_fingerprint_records_build_start() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello").unwrap();
        let index_path = root.join("index.xgrep");
        let before = now_ns();
        build_index(root, &index_path).unwrap();
        let (_, start) = read_fingerprint(&fingerprint_path(&index_path)).unwrap();
        let start = start.unwrap();
        assert!(start >= before && start <= now_ns());
    }

    // ----------------------------------------------------------------
    // Max file size, issue 2-13
    // ----------------------------------------------------------------

    #[test]
    fn test_parse_size() {
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("10M"), Some(10 << 20));
        assert_eq!(parse_size("512k"), Some(512 << 10));
        assert_eq!(parse_size(" 2G "), Some(2 << 30));
        assert_eq!(parse_size("0"), Some(0));
        assert_eq!(parse_size("none"), Some(0));
        assert_eq!(parse_size("unlimited"), Some(0));
        assert_eq!(parse_size("abc"), None);
        assert_eq!(parse_size(""), None);
    }

    #[test]
    fn test_max_filesize_skips_large_files() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("small.txt"), "small qzx").unwrap();
        fs::write(root.join("big.txt"), "big qzx ".repeat(200)).unwrap();
        let index_path = root.join("index.xgrep");
        build_index_with_limit(root, &index_path, None, Some(100)).unwrap();
        assert_eq!(paths_with_trigram(&index_path, *b"qzx"), vec!["small.txt"]);

        // Unlimited: the setting is part of the fingerprint, so this rebuilds.
        assert!(build_index_with_limit(root, &index_path, None, None).unwrap());
        assert_eq!(
            paths_with_trigram(&index_path, *b"qzx"),
            vec!["big.txt", "small.txt"]
        );
    }

    #[test]
    fn test_oversize_files_are_recorded() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("small.txt"), "small qzx").unwrap();
        fs::write(root.join("big.txt"), "big qzx ".repeat(200)).unwrap();
        fs::write(root.join("bigger.log"), "x".repeat(5000)).unwrap();
        let index_path = root.join("index.xgrep");
        build_index_with_limit(root, &index_path, None, Some(100)).unwrap();
        let info = read_oversize(&index_path).unwrap();
        assert_eq!(info.limit, 100);
        assert_eq!(info.count, 2);
        assert_eq!(info.examples, vec!["bigger.log", "big.txt"]); // largest first
                                                                  // The record is not part of the corpus.
        assert!(!build_index_with_limit(root, &index_path, None, Some(100)).unwrap());

        // Diff update: one more large file.
        fs::write(root.join("new.bin.txt"), "y".repeat(300)).unwrap();
        build_index_with_limit(root, &index_path, None, Some(100)).unwrap();
        assert_eq!(read_oversize(&index_path).unwrap().count, 3);

        // Unlimited: nothing skipped, the record goes away.
        build_index_with_limit(root, &index_path, None, None).unwrap();
        assert_eq!(read_oversize(&index_path), None);
        assert!(!oversize_path(&index_path).exists());

        // An index built before the record existed gets it on the fast path.
        build_index_with_limit(root, &index_path, None, Some(100)).unwrap();
        fs::remove_file(oversize_path(&index_path)).unwrap();
        assert!(!build_index_with_limit(root, &index_path, None, Some(100)).unwrap());
        assert_eq!(read_oversize(&index_path).unwrap().count, 3);
    }

    #[test]
    fn test_oversize_hint_and_sizes() {
        assert_eq!(human_size(10 << 20), "10MB");
        assert_eq!(human_size(512 << 10), "512KB");
        assert_eq!(human_size(3 << 19), "1.5MB");
        assert_eq!(human_size(100), "100B");
        let info = OversizeInfo {
            limit: 10 << 20,
            count: 2,
            examples: vec!["data/dump.sql".into()],
        };
        let h = info.hint("search them with `xg PATTERN FILE`");
        assert_eq!(
            h,
            "2 files over 10MB are not indexed (e.g. data/dump.sql), so indexed searches cannot find matches in them; set XGREP_MAX_FILESIZE=0 (or a larger size) and rebuild the index, or search them with `xg PATTERN FILE`"
        );
    }

    #[test]
    fn test_diff_update_drops_file_that_grew_over_limit() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        for i in 0..30 {
            fs::write(root.join(format!("f{i}.txt")), format!("filler {i}")).unwrap();
        }
        fs::write(root.join("grow.txt"), "qzx small").unwrap();
        let index_path = root.join("index.xgrep");
        build_index_with_limit(root, &index_path, None, Some(100)).unwrap();
        assert_eq!(paths_with_trigram(&index_path, *b"qzx"), vec!["grow.txt"]);

        fs::write(root.join("grow.txt"), "qzx ".repeat(100)).unwrap();
        assert!(build_index_with_limit(root, &index_path, None, Some(100)).unwrap());
        assert!(paths_with_trigram(&index_path, *b"qzx").is_empty());
        let reader = crate::index::reader::IndexReader::open(&index_path).unwrap();
        assert_eq!(reader.file_count(), 30);
    }

    // ----------------------------------------------------------------
    // Binary detection consistency (full vs diff)
    // ----------------------------------------------------------------

    #[test]
    fn test_binary_rule_same_for_full_and_diff() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        for i in 0..30 {
            fs::write(root.join(format!("f{i}.txt")), format!("filler {i}")).unwrap();
        }
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        // New file whose only NUL byte is beyond the first 8 KiB: binary.
        let mut late_nul = b"qzx text ".repeat(1000);
        late_nul.push(0);
        fs::write(root.join("late_nul.dat"), &late_nul).unwrap();
        assert!(!build_index(root, &index_path).unwrap());
        assert!(paths_with_trigram(&index_path, *b"qzx").is_empty());
        assert_diff_eq_full(root, &index_path);
    }

    #[test]
    fn test_text_to_binary_removes_file_like_full_build() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        for i in 0..30 {
            fs::write(root.join(format!("f{i}.txt")), format!("filler {i}")).unwrap();
        }
        fs::write(root.join("x.txt"), "qzx text").unwrap();
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        fs::write(root.join("x.txt"), b"now\x00binary!").unwrap();
        assert!(build_index(root, &index_path).unwrap());
        let reader = crate::index::reader::IndexReader::open(&index_path).unwrap();
        assert_eq!(
            reader.file_count(),
            30,
            "binary file must leave the file table"
        );
        drop(reader);
        assert_diff_eq_full(root, &index_path);
    }

    // ----------------------------------------------------------------
    // Diff update: parallel phase 2, stats of changed files
    // ----------------------------------------------------------------

    #[test]
    fn test_diff_update_records_new_size_and_hash() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        for i in 0..30 {
            fs::write(root.join(format!("f{i}.txt")), format!("filler {i}")).unwrap();
        }
        fs::write(root.join("a.txt"), "short").unwrap();
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        let content = "a considerably longer replacement";
        fs::write(root.join("a.txt"), content).unwrap();
        assert!(build_index(root, &index_path).unwrap());
        let reader = crate::index::reader::IndexReader::open(&index_path).unwrap();
        let fid = (0..reader.file_count())
            .find(|&f| reader.file_path(f) == "a.txt")
            .unwrap();
        let fe = reader.file_entry(fid).unwrap();
        let size = fe.size;
        let hash = fe.content_hash;
        assert_eq!(size, content.len() as u64);
        assert_eq!(hash, xxhash_rust::xxh64::xxh64(content.as_bytes(), 0));
    }

    #[test]
    fn test_diff_update_many_files_eq_full_rebuild() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        for i in 0..200 {
            fs::write(
                root.join(format!("f{i:03}.txt")),
                format!("file number {i} body {}", i * 7),
            )
            .unwrap();
        }
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        // 2 edits, 2 deletions, 2 additions (3% of files).
        fs::write(root.join("f010.txt"), "edited qzx one").unwrap();
        fs::write(root.join("f150.txt"), "edited two vvw").unwrap();
        fs::remove_file(root.join("f020.txt")).unwrap();
        fs::remove_file(root.join("f199.txt")).unwrap();
        fs::write(root.join("new1.txt"), "added qzx").unwrap();
        fs::write(root.join("new2.txt"), "added again").unwrap();
        assert!(build_index(root, &index_path).unwrap());
        assert_eq!(
            paths_with_trigram(&index_path, *b"qzx"),
            vec!["f010.txt", "new1.txt"]
        );
        assert_diff_eq_full(root, &index_path);
    }

    // ----------------------------------------------------------------
    // Paths, locking, overflow, crash safety
    // ----------------------------------------------------------------

    #[test]
    #[cfg(target_os = "linux")]
    fn test_non_utf8_path_is_skipped() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempdir().unwrap();
        let root = dir.path();
        let bad = std::ffi::OsStr::from_bytes(b"bad\xff.txt");
        fs::write(root.join(bad), "qzx in a badly named file").unwrap();
        fs::write(root.join("good.txt"), "qzx in a good file").unwrap();
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();
        assert_eq!(paths_with_trigram(&index_path, *b"qzx"), vec!["good.txt"]);
    }

    #[test]
    #[cfg(unix)]
    fn test_lock_is_exclusive_and_released() {
        let dir = tempdir().unwrap();
        let index_path = dir.path().join("index.xgrep");
        let g = acquire_lock(&index_path).unwrap();
        assert!(acquire_lock(&index_path).is_err());
        drop(g);
        assert!(!index_path.with_extension("lock").exists());
        let g2 = acquire_lock(&index_path).unwrap();
        drop(g2);
    }

    #[test]
    fn test_range_offset_overflow_is_an_error() {
        let g = |tri| Gram {
            tri,
            loc: 1,
            next: 0,
        };
        let file_trigrams = vec![vec![g(*b"abc"), g(*b"abd")]];
        let dt = DenseTrigrams::build(&file_trigrams);
        let mut counts = vec![u32::MAX, 1];
        let err = build_range(0, &mut counts, &dt, &file_trigrams)
            .err()
            .expect("overflow must be reported");
        assert!(err.to_string().contains("too large"));
    }

    #[test]
    fn test_old_version_index_is_rebuilt_even_if_fingerprint_matches() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "Hello World").unwrap();
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();
        // Pretend the index was written by format v3 (same fingerprint).
        let mut data = fs::read(&index_path).unwrap();
        data[4..8].copy_from_slice(&3u32.to_le_bytes());
        fs::write(&index_path, &data).unwrap();
        assert!(crate::index::reader::IndexReader::open(&index_path).is_err());
        assert!(build_index(root, &index_path).unwrap());
        assert_eq!(
            crate::index::reader::index_version(&index_path),
            Some(VERSION)
        );
        assert_eq!(paths_with_trigram(&index_path, *b"hel"), vec!["a.txt"]);
    }

    #[test]
    fn test_mismatched_sidecar_falls_back_to_full_build() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        for i in 0..40 {
            fs::write(root.join(format!("f{i:02}.txt")), format!("file {i}")).unwrap();
        }
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();
        // Corrupt the sidecar: the next update must not trust it.
        let side = sidecar_path(&index_path, None);
        fs::write(&side, b"XGCH garbage").unwrap();
        fs::write(root.join("f05.txt"), "changed qzx").unwrap();
        assert!(build_index(root, &index_path).unwrap());
        assert_eq!(paths_with_trigram(&index_path, *b"qzx"), vec!["f05.txt"]);
        let v = crate::index::cache::SidecarView::open(&side).unwrap();
        let reader = crate::index::reader::IndexReader::open(&index_path).unwrap();
        assert_eq!(v.sidecar_id, reader.sidecar_id());
        drop(reader);
        assert_diff_eq_full(root, &index_path);
    }

    #[test]
    fn test_failed_write_leaves_no_temp_file() {
        let dir = tempdir().unwrap();
        let index_path = dir.path().join("index.xgrep");
        let res = write_index_atomic(
            &index_path,
            1,
            &[],
            7,
            |_, _| Ok(0), // pushes no entries: internal mismatch error
        );
        assert!(res.is_err());
        assert!(!index_path.exists());
        let leftovers: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert!(leftovers.is_empty(), "temp file must be cleaned up");
    }
}

#[cfg(test)]
mod diff_proptests {
    use super::*;
    use proptest::prelude::*;

    fn content() -> impl Strategy<Value = String> {
        "[abAB c\n_x]{0,60}"
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]

        /// A diff update (edits, deletions, additions; few enough to stay
        /// under the fallback threshold) yields exactly the postings and
        /// masks of a full rebuild.
        #[test]
        fn prop_diff_update_eq_full_rebuild(
            initial in prop::collection::vec(content(), 40),
            edits in prop::collection::vec((0usize..3, 0usize..48, content()), 1..6),
        ) {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            for (i, c) in initial.iter().enumerate() {
                fs::write(root.join(format!("f{i:02}.txt")), c).unwrap();
            }
            let index_path = root.join("index.xgrep");
            build_index(root, &index_path).unwrap();
            for (kind, n, c) in &edits {
                let p = root.join(format!("f{n:02}.txt"));
                match kind {
                    0 => fs::write(&p, c).unwrap(),
                    1 => {
                        let _ = fs::remove_file(&p);
                    }
                    _ => fs::write(root.join(format!("new{n:02}.txt")), c).unwrap(),
                }
            }
            build_index(root, &index_path).unwrap();
            // Sidecar and index stay paired.
            let reader = crate::index::reader::IndexReader::open(&index_path).unwrap();
            let side = crate::index::cache::SidecarView::open(&sidecar_path(&index_path, None)).unwrap();
            prop_assert_eq!(side.sidecar_id, reader.sidecar_id());
            prop_assert_eq!(side.len(), reader.file_count() as usize);
            drop(reader);
            tests::assert_diff_eq_full(root, &index_path);
        }
    }
}
