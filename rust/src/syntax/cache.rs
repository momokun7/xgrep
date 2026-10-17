//! Sidecar cache of per-file [`FileTags`], stored next to the index as
//! `<index>.tags`. Entries are keyed by relative path and validated by
//! mtime + size, filled lazily (only the files a tool asks about) and parsed
//! in parallel. The main index format is not involved.
//!
//! The file is rewritten atomically (temp file + rename) when new entries were
//! parsed. Any read error or version mismatch discards the cache.

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::UNIX_EPOCH;

use rayon::prelude::*;

use super::{lang_for_path, tags_for_source, FileTags, Kind, Symbol, MAX_PARSE_BYTES};

const MAGIC: &[u8; 8] = b"XGTAGS\0\0";
/// Bump when the extraction logic or the encoding changes.
const VERSION: u32 = 1;

#[derive(Clone)]
struct Entry {
    mtime_ns: u64,
    size: u64,
    tags: Arc<FileTags>,
}

#[derive(Default)]
struct State {
    loaded: bool,
    map: HashMap<String, Entry>,
    dirty: bool,
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
            st.map = load(&self.file).unwrap_or_default();
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
        let mut misses: Vec<usize> = Vec::new();
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
                        out[i] = Some(e.tags.clone());
                        stats.hits += 1;
                    }
                    _ => misses.push(i),
                }
            }
        }
        if misses.is_empty() {
            return (out, stats);
        }
        let parsed: Vec<(usize, Option<Entry>)> = misses
            .par_iter()
            .map(|&i| {
                let (mtime, size) = stats_now[i].unwrap_or((0, 0));
                let entry = (|| {
                    let lang = lang_for_path(files[i])?;
                    let bytes = std::fs::read(root.join(files[i])).ok()?;
                    if memchr::memchr(0, &bytes).is_some() {
                        return None;
                    }
                    let src = String::from_utf8_lossy(&bytes);
                    let tags = tags_for_source(lang, &src)?;
                    Some(Entry {
                        mtime_ns: mtime,
                        size,
                        tags: Arc::new(tags),
                    })
                })();
                (i, entry)
            })
            .collect();
        stats.parsed = parsed.len();
        let mut st = self.lock();
        for (i, e) in parsed {
            if let Some(e) = e {
                out[i] = Some(e.tags.clone());
                st.map.insert(files[i].to_string(), e);
                st.dirty = true;
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
            st.dirty = true;
        }
    }

    /// Write the cache if anything changed. Errors are ignored by callers
    /// (the cache is an optimisation).
    pub fn save(&self) -> io::Result<()> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !st.dirty {
            return Ok(());
        }
        let buf = encode(&st.map);
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let dir = self.file.parent().unwrap_or(Path::new("."));
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        tmp.write_all(&buf)?;
        tmp.persist(&self.file).map_err(|e| e.error)?;
        st.dirty = false;
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

fn encode(map: &HashMap<String, Entry>) -> Vec<u8> {
    let mut b = Vec::with_capacity(map.len() * 512);
    b.extend_from_slice(MAGIC);
    put_u32(&mut b, VERSION);
    put_u32(&mut b, map.len() as u32);
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    for k in keys {
        let e = &map[k];
        put_str(&mut b, k);
        put_u64(&mut b, e.mtime_ns);
        put_u64(&mut b, e.size);
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

fn decode(b: &[u8]) -> Option<HashMap<String, Entry>> {
    let mut c = Cur { b, p: 0 };
    if c.take(8)? != MAGIC || c.u32()? != VERSION {
        return None;
    }
    let n = c.u32()? as usize;
    let mut map = HashMap::with_capacity(n.min(1 << 20));
    for _ in 0..n {
        let path = c.str()?;
        let mtime_ns = c.u64()?;
        let size = c.u64()?;
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
        map.insert(
            path,
            Entry {
                mtime_ns,
                size,
                tags: Arc::new(FileTags { defs, refs, lines }),
            },
        );
    }
    Some(map)
}

fn load(file: &Path) -> Option<HashMap<String, Entry>> {
    let mut f = std::fs::File::open(file).ok()?;
    let mut b = Vec::new();
    f.read_to_end(&mut b).ok()?;
    decode(&b)
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
    fn corrupt_cache_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x.tags");
        std::fs::write(&f, b"XGTAGS\0\0garbage").unwrap();
        assert!(load(&f).is_none());
    }
}
