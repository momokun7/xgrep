//! Sidecar cache of per-file [`FileTags`], stored next to the index as
//! `<index>.tags`. Entries are keyed by relative path and validated by
//! mtime + size, filled lazily (only the files a tool asks about) and parsed
//! in parallel. The main index format is not involved.
//!
//! Like the index builder, entries are "racily clean"-aware: an entry whose
//! file mtime falls in the same second as (or after) the moment its content
//! was read is not trusted on stat data alone, because a same-size edit
//! within one timestamp tick (coarse filesystems such as WSL drvfs) leaves
//! mtime + size unchanged. Such entries are re-verified by content hash.
//!
//! New entries are appended to a journal (`<index>.tags.log`) so a call that
//! parses a handful of files does not rewrite a large cache; the journal is
//! folded into the main file (atomic temp file + rename) once it grows past a
//! quarter of the cache. Any read error or version mismatch discards the
//! cache (a damaged journal tail is ignored).

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::UNIX_EPOCH;

use rayon::prelude::*;

use super::{lang_for_path, tags_for_source, FileTags, Kind, Symbol, MAX_PARSE_BYTES};

const MAGIC: &[u8; 8] = b"XGTAGS\0\0";
/// Bump when the extraction logic or the encoding changes.
const VERSION: u32 = 4;

#[derive(Clone)]
struct Entry {
    mtime_ns: u64,
    size: u64,
    /// xxh64 of the file content the tags were computed from.
    hash: u64,
    /// When the content was read (ns since the epoch), for the racy check.
    verified_ns: u64,
    tags: Arc<FileTags>,
}

impl Entry {
    /// Stat data alone cannot prove the file unchanged since it was read
    /// (same rule as [`crate::index::cache::FileStat::is_racy`]).
    fn is_racy(&self) -> bool {
        self.mtime_ns / 1_000_000_000 >= self.verified_ns / 1_000_000_000
    }
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[derive(Default)]
struct State {
    loaded: bool,
    map: HashMap<String, Entry>,
    /// Keys parsed since the last save (journal candidates).
    pending: Vec<String>,
    /// Entries were removed: the next save must rewrite the main file.
    rewrite: bool,
    /// Entries currently in the journal file.
    journal_len: usize,
}

/// Tag cache for one index.
pub struct TagStore {
    file: PathBuf,
    state: Mutex<State>,
}

/// Statistics of one [`TagStore::tags`] call.
#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub hits: usize,
    pub parsed: usize,
}

/// Sidecar path for an index file.
pub fn sidecar_path(index_path: &Path) -> PathBuf {
    let mut s = index_path.as_os_str().to_owned();
    s.push(".tags");
    PathBuf::from(s)
}

/// Process-wide store for an index (shared by MCP calls and the CLI).
pub fn store_for(index_path: &Path) -> Arc<TagStore> {
    static STORES: OnceLock<Mutex<HashMap<PathBuf, Arc<TagStore>>>> = OnceLock::new();
    let file = sidecar_path(index_path);
    let mut m = STORES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    m.entry(file.clone())
        .or_insert_with(|| {
            Arc::new(TagStore {
                file,
                state: Mutex::new(State::default()),
            })
        })
        .clone()
}

fn stat(path: &Path) -> Option<(u64, u64)> {
    let m = std::fs::metadata(path).ok()?;
    if !m.is_file() {
        return None;
    }
    let mtime = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    Some((mtime, m.len()))
}

impl TagStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !st.loaded {
            st.loaded = true;
            let (map, journal_len) = load(&self.file).unwrap_or_default();
            st.map = map;
            st.journal_len = journal_len;
        }
        st
    }

    /// Tags for each of `files` (paths relative to `root`), in order. `None`
    /// for unsupported languages, missing, oversized or non-UTF-8 files.
    pub fn tags(&self, root: &Path, files: &[&str]) -> (Vec<Option<Arc<FileTags>>>, Stats) {
        let stats_now: Vec<Option<(u64, u64)>> = files
            .par_iter()
            .map(|f| {
                lang_for_path(f)?;
                stat(&root.join(f))
            })
            .collect();
        let mut out: Vec<Option<Arc<FileTags>>> = vec![None; files.len()];
        // (index, racy entry with matching stat to verify by content hash)
        let mut misses: Vec<(usize, Option<(u64, Arc<FileTags>)>)> = Vec::new();
        let mut stats = Stats::default();
        {
            let st = self.lock();
            for (i, s) in stats_now.iter().enumerate() {
                let Some((mtime, size)) = *s else { continue };
                if size > MAX_PARSE_BYTES {
                    continue;
                }
                match st.map.get(files[i]) {
                    Some(e) if e.mtime_ns == mtime && e.size == size => {
                        if e.is_racy() {
                            misses.push((i, Some((e.hash, e.tags.clone()))));
                        } else {
                            out[i] = Some(e.tags.clone());
                            stats.hits += 1;
                        }
                    }
                    _ => misses.push((i, None)),
                }
            }
        }
        if misses.is_empty() {
            return (out, stats);
        }
        // (index, entry, parsed?)
        let fresh: Vec<(usize, Option<Entry>, bool)> = misses
            .into_par_iter()
            .map(|(i, prev)| {
                let (mtime, size) = stats_now[i].unwrap_or((0, 0));
                let mut parsed = false;
                let entry = (|| {
                    let lang = lang_for_path(files[i])?;
                    // Taken before reading: an edit after this instant in
                    // the same second keeps the entry racy.
                    let verified_ns = now_ns();
                    let bytes = std::fs::read(root.join(files[i])).ok()?;
                    if memchr::memchr(0, &bytes).is_some() {
                        return None;
                    }
                    let hash = xxhash_rust::xxh64::xxh64(&bytes, 0);
                    let tags = match prev {
                        Some((h, tags)) if h == hash => tags,
                        _ => {
                            parsed = true;
                            let src = String::from_utf8_lossy(&bytes);
                            Arc::new(tags_for_source(lang, &src)?)
                        }
                    };
                    Some(Entry {
                        mtime_ns: mtime,
                        size,
                        hash,
                        verified_ns,
                        tags,
                    })
                })();
                (i, entry, parsed)
            })
            .collect();
        let mut st = self.lock();
        for (i, e, parsed) in fresh {
            if parsed {
                stats.parsed += 1;
            } else if e.is_some() {
                stats.hits += 1;
            }
            if let Some(e) = e {
                out[i] = Some(e.tags.clone());
                st.map.insert(files[i].to_string(), e);
                st.pending.push(files[i].to_string());
            }
        }
        (out, stats)
    }

    /// Drop code-file entries not in `keep` (call with the full list of
    /// indexed source files, e.g. from `repo_map`). Docs entries are kept.
    pub fn retain_code(&self, keep: &HashSet<&str>) {
        let mut st = self.lock();
        let before = st.map.len();
        st.map.retain(|k, _| {
            keep.contains(k.as_str()) || !lang_for_path(k).is_some_and(|l| l.is_code())
        });
        if st.map.len() != before {
            st.rewrite = true;
        }
    }

    /// Write the cache if anything changed. Errors are ignored by callers
    /// (the cache is an optimisation).
    pub fn save(&self) -> io::Result<()> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.pending.is_empty() && !st.rewrite {
            return Ok(());
        }
        let log = journal_path(&self.file);
        let small = (st.journal_len + st.pending.len()) * 4 < st.map.len();
        if !st.rewrite && small && self.file.exists() {
            let mut keys = std::mem::take(&mut st.pending);
            keys.sort();
            keys.dedup();
            let buf = encode(keys.iter().filter_map(|k| st.map.get_key_value(k)));
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log)?;
            f.write_all(&buf)?;
            st.journal_len += keys.len();
            return Ok(());
        }
        let buf = encode(st.map.iter());
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let dir = self.file.parent().unwrap_or(Path::new("."));
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        tmp.write_all(&buf)?;
        tmp.persist(&self.file).map_err(|e| e.error)?;
        let _ = std::fs::remove_file(&log);
        st.pending.clear();
        st.rewrite = false;
        st.journal_len = 0;
        Ok(())
    }

    /// Path of the sidecar file.
    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.file
    }
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

fn put_u32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(b: &mut Vec<u8>, v: u64) {
    b.extend_from_slice(&v.to_le_bytes());
}

fn put_str(b: &mut Vec<u8>, s: &str) {
    put_u32(b, s.len() as u32);
    b.extend_from_slice(s.as_bytes());
}

fn journal_path(file: &Path) -> PathBuf {
    let mut s = file.as_os_str().to_owned();
    s.push(".log");
    PathBuf::from(s)
}

/// One chunk: header + entries. The main file is one chunk; the journal is
/// a sequence of chunks.
fn encode<'a>(entries: impl Iterator<Item = (&'a String, &'a Entry)>) -> Vec<u8> {
    let mut entries: Vec<(&String, &Entry)> = entries.collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let mut b = Vec::with_capacity(entries.len() * 512 + 16);
    b.extend_from_slice(MAGIC);
    put_u32(&mut b, VERSION);
    put_u32(&mut b, entries.len() as u32);
    for (k, e) in entries {
        put_str(&mut b, k);
        put_u64(&mut b, e.mtime_ns);
        put_u64(&mut b, e.size);
        put_u64(&mut b, e.hash);
        put_u64(&mut b, e.verified_ns);
        let t = &e.tags;
        put_u32(&mut b, t.lines);
        put_u32(&mut b, t.defs.len() as u32);
        for s in &t.defs {
            b.push(s.kind as u8);
            put_str(&mut b, &s.name);
            put_u32(&mut b, s.line);
            put_u32(&mut b, s.start);
            put_u32(&mut b, s.end);
            put_u32(&mut b, s.parent.map_or(0, |p| p + 1));
            put_str(&mut b, &s.sig);
        }
        put_u32(&mut b, t.refs.len() as u32);
        for (n, c) in &t.refs {
            put_str(&mut b, n);
            put_u32(&mut b, *c);
        }
    }
    b
}

struct Cur<'a> {
    b: &'a [u8],
    p: usize,
}

impl Cur<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let s = self.b.get(self.p..self.p.checked_add(n)?)?;
        self.p += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn str(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).ok()
    }
}

/// Decode one chunk at the cursor into `map`.
fn decode_chunk(c: &mut Cur, map: &mut HashMap<String, Entry>) -> Option<usize> {
    if c.take(8)? != MAGIC || c.u32()? != VERSION {
        return None;
    }
    let n = c.u32()? as usize;
    map.reserve(n.min(1 << 20));
    let mut staged = Vec::with_capacity(n.min(1 << 20));
    for _ in 0..n {
        let path = c.str()?;
        let mtime_ns = c.u64()?;
        let size = c.u64()?;
        let hash = c.u64()?;
        let verified_ns = c.u64()?;
        let lines = c.u32()?;
        let nd = c.u32()? as usize;
        let mut defs = Vec::with_capacity(nd.min(1 << 16));
        for _ in 0..nd {
            let kind = Kind::from_u8(c.u8()?)?;
            let name = c.str()?;
            let line = c.u32()?;
            let start = c.u32()?;
            let end = c.u32()?;
            let parent = match c.u32()? {
                0 => None,
                p => Some(p - 1),
            };
            let sig = c.str()?;
            defs.push(Symbol {
                kind,
                name,
                line,
                start,
                end,
                parent,
                sig,
            });
        }
        let nr = c.u32()? as usize;
        let mut refs = Vec::with_capacity(nr.min(1 << 16));
        for _ in 0..nr {
            let name = c.str()?;
            let cnt = c.u32()?;
            refs.push((name, cnt));
        }
        staged.push((
            path,
            Entry {
                mtime_ns,
                size,
                hash,
                verified_ns,
                tags: Arc::new(FileTags { defs, refs, lines }),
            },
        ));
    }
    // A chunk applies all-or-nothing (a torn journal append is ignored).
    map.extend(staged);
    Some(n)
}

fn read_all(file: &Path) -> Option<Vec<u8>> {
    let mut f = std::fs::File::open(file).ok()?;
    let mut b = Vec::new();
    f.read_to_end(&mut b).ok()?;
    Some(b)
}

/// Main file plus journal. Returns the map and the number of journal entries.
fn load(file: &Path) -> Option<(HashMap<String, Entry>, usize)> {
    let b = read_all(file)?;
    let mut map = HashMap::new();
    decode_chunk(&mut Cur { b: &b, p: 0 }, &mut map)?;
    let mut journal = 0usize;
    if let Some(j) = read_all(&journal_path(file)) {
        let mut c = Cur { b: &j, p: 0 };
        while c.p < j.len() {
            match decode_chunk(&mut c, &mut map) {
                Some(n) => journal += n,
                None => break,
            }
        }
    }
    Some((map, journal))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_invalidation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.md"), "# Title\n## Sub\n").unwrap();
        std::fs::write(root.join("b.xyz"), "nothing").unwrap();
        let idx = root.join("idx").join("index.bin");
        let store = TagStore {
            file: sidecar_path(&idx),
            state: Mutex::new(State::default()),
        };
        let (t, s) = store.tags(root, &["a.md", "b.xyz", "missing.md"]);
        assert_eq!(s.parsed, 1);
        assert_eq!(t[0].as_ref().unwrap().defs.len(), 2);
        assert!(t[1].is_none() && t[2].is_none());
        store.save().unwrap();
        assert!(store.path().exists());

        // A fresh store reads the sidecar back.
        let again = TagStore {
            file: sidecar_path(&idx),
            state: Mutex::new(State::default()),
        };
        let (t2, s2) = again.tags(root, &["a.md"]);
        assert_eq!(s2.hits, 1);
        assert_eq!(t2[0], t[0]);

        // Changing the file (size) invalidates the entry.
        std::fs::write(root.join("a.md"), "# Title\n## Sub\n### Deeper\n").unwrap();
        let (t3, s3) = again.tags(root, &["a.md"]);
        assert_eq!(s3.parsed, 1);
        assert_eq!(t3[0].as_ref().unwrap().defs.len(), 3);
    }

    #[test]
    fn small_updates_go_to_the_journal() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let names: Vec<String> = (0..10).map(|i| format!("d{}.md", i)).collect();
        for n in &names {
            std::fs::write(root.join(n), "# T\n").unwrap();
        }
        let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        let file = sidecar_path(&root.join("index"));
        let store = TagStore {
            file: file.clone(),
            state: Mutex::new(State::default()),
        };
        store.tags(root, &refs);
        store.save().unwrap();
        assert!(!journal_path(&file).exists());
        // one new file: appended to the journal, main file untouched
        std::fs::write(root.join("new.md"), "# N\n## M\n").unwrap();
        let before = std::fs::metadata(&file).unwrap().len();
        store.tags(root, &["new.md"]);
        store.save().unwrap();
        assert_eq!(std::fs::metadata(&file).unwrap().len(), before);
        assert!(journal_path(&file).exists());
        // a new process sees both
        let again = TagStore {
            file: file.clone(),
            state: Mutex::new(State::default()),
        };
        let (t, s) = again.tags(root, &["new.md", "d3.md"]);
        assert_eq!(s.hits, 2);
        assert_eq!(t[0].as_ref().unwrap().defs.len(), 2);
        // a torn journal tail is ignored
        let mut j = std::fs::OpenOptions::new()
            .append(true)
            .open(journal_path(&file))
            .unwrap();
        j.write_all(b"XGTAGS\0\0\x04\0\0\0\x05").unwrap();
        let third = TagStore {
            file,
            state: Mutex::new(State::default()),
        };
        assert_eq!(third.tags(root, &["new.md"]).1.hits, 1);
    }

    #[test]
    fn racy_same_size_edit_is_not_served_stale() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let f = root.join("a.md");
        std::fs::write(&f, "# Alpha\n").unwrap();
        let mtime = std::fs::metadata(&f).unwrap().modified().unwrap();
        let file = sidecar_path(&root.join("index"));
        let store = TagStore {
            file: file.clone(),
            state: Mutex::new(State::default()),
        };
        let (t, s) = store.tags(root, &["a.md"]);
        assert_eq!(s.parsed, 1);
        assert_eq!(t[0].as_ref().unwrap().defs[0].name, "Alpha");
        store.save().unwrap();
        // Same-size edit within the same timestamp tick: mtime + size equal.
        std::fs::write(&f, "# Omega\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        let (t, _) = store.tags(root, &["a.md"]);
        assert_eq!(t[0].as_ref().unwrap().defs[0].name, "Omega");
        // Also when the entry comes back from disk in a new process.
        std::fs::write(&f, "# Gamma\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        let again = TagStore {
            file,
            state: Mutex::new(State::default()),
        };
        let (t, _) = again.tags(root, &["a.md"]);
        assert_eq!(t[0].as_ref().unwrap().defs[0].name, "Gamma");
    }

    #[test]
    fn old_files_are_trusted_on_stat_and_racy_ones_verified() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let f = root.join("a.md");
        std::fs::write(&f, "# Alpha\n").unwrap();
        let store = TagStore {
            file: sidecar_path(&root.join("index")),
            state: Mutex::new(State::default()),
        };
        // Racy (just written) but unchanged: verified by hash, not re-parsed.
        assert_eq!(store.tags(root, &["a.md"]).1.parsed, 1);
        let s = store.tags(root, &["a.md"]).1;
        assert_eq!((s.hits, s.parsed), (1, 0));
        // An old mtime is trusted on stat data alone.
        let old = UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        std::fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert_eq!(store.tags(root, &["a.md"]).1.parsed, 1);
        let st = store.lock();
        assert!(!st.map["a.md"].is_racy());
    }

    #[test]
    fn corrupt_cache_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x.tags");
        std::fs::write(&f, b"XGTAGS\0\0garbage").unwrap();
        assert!(load(&f).is_none());
    }
}
