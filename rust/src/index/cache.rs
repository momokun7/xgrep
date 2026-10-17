//! Trigram cache for incremental index builds.

use std::collections::HashMap;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::error::Result;

/// Magic bytes identifying the xgrep cache format.
const CACHE_MAGIC: &[u8; 4] = b"XGCH";

/// Current cache format version.
///
/// v2: nanosecond mtime + size + ctime + inode per entry, plus the build start
/// time in the header (used for racy-clean detection). v1 caches (second-resolution
/// mtime only) are ignored.
const CACHE_VERSION: u32 = 2;

/// Header: magic(4) + version(4) + build_start_ns(8) + count(4).
const HEADER_LEN: usize = 20;
/// Fixed part of an entry after the path: 5 × u64 + trigram_count u32.
const ENTRY_FIXED_LEN: usize = 8 * 5 + 4;

/// File metadata used to decide whether a file may have changed.
///
/// `ctime_ns` and `ino` are 0 on platforms where they are unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct FileStat {
    /// Modification time in nanoseconds since the UNIX epoch.
    pub mtime_ns: u64,
    pub size: u64,
    /// Inode change time in nanoseconds since the UNIX epoch (unix only).
    pub ctime_ns: u64,
    /// Inode number (unix only).
    pub ino: u64,
}

impl FileStat {
    pub fn from_metadata(meta: &fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let ns = |s: i64, n: i64| -> u64 {
                if s < 0 {
                    0
                } else {
                    (s as u64)
                        .saturating_mul(1_000_000_000)
                        .saturating_add(n.max(0) as u64)
                }
            };
            Self {
                mtime_ns: ns(meta.mtime(), meta.mtime_nsec()),
                size: meta.len(),
                ctime_ns: ns(meta.ctime(), meta.ctime_nsec()),
                ino: meta.ino(),
            }
        }
        #[cfg(not(unix))]
        {
            let mtime_ns = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos().min(u64::MAX as u128) as u64)
                .unwrap_or(0);
            Self {
                mtime_ns,
                size: meta.len(),
                ctime_ns: 0,
                ino: 0,
            }
        }
    }

    /// "Racily clean" check (same idea as git's racy-git handling).
    ///
    /// A file whose mtime falls in the same second as (or after) the moment a
    /// build started may have been modified again after it was read without its
    /// mtime/size changing (coarse filesystem timestamps). Such files must not be
    /// trusted on stat data alone and are re-verified by content hash.
    pub fn is_racy(&self, build_start_ns: u64) -> bool {
        self.mtime_ns / 1_000_000_000 >= build_start_ns / 1_000_000_000
    }
}

/// Cached trigram information for a file.
pub(crate) struct CachedFile {
    pub stat: FileStat,
    pub content_hash: u64,
    /// Sorted, deduplicated trigrams.
    pub trigrams: Vec<[u8; 3]>,
}

/// Cache mapping file paths to trigram information.
pub struct TrigramCache {
    pub(crate) entries: HashMap<String, CachedFile>,
    /// Start time (ns since epoch) of the build that produced this cache.
    /// Entries that are racy relative to it are not trusted.
    pub(crate) build_start_ns: u64,
}

impl Default for TrigramCache {
    fn default() -> Self {
        Self::new()
    }
}

fn rd_u64(data: &[u8], pos: usize) -> u64 {
    u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap())
}

impl TrigramCache {
    /// Create an empty cache.
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            build_start_ns: 0,
        }
    }

    /// Load cache from file. Returns an empty cache if the file does not exist,
    /// is corrupted, or was written by another cache format version.
    pub fn load(path: &Path) -> Self {
        let data = match fs::read(path) {
            Ok(d) => d,
            Err(_) => return Self::new(),
        };
        if data.len() < HEADER_LEN || &data[0..4] != CACHE_MAGIC {
            return Self::new();
        }
        let version = u32::from_le_bytes(data[4..8].try_into().unwrap());
        if version != CACHE_VERSION {
            return Self::new();
        }
        let build_start_ns = rd_u64(&data, 8);
        let count = u32::from_le_bytes(data[16..20].try_into().unwrap()) as usize;
        let mut pos = HEADER_LEN;

        let mut entries = HashMap::with_capacity(count.min(1 << 20));
        for _ in 0..count {
            if pos + 2 > data.len() {
                break;
            }
            let path_len = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
            pos += 2;
            if pos + path_len + ENTRY_FIXED_LEN > data.len() {
                break;
            }
            let path_str = match std::str::from_utf8(&data[pos..pos + path_len]) {
                Ok(s) => s.to_string(),
                Err(_) => break,
            };
            pos += path_len;
            let stat = FileStat {
                mtime_ns: rd_u64(&data, pos),
                size: rd_u64(&data, pos + 8),
                ctime_ns: rd_u64(&data, pos + 16),
                ino: rd_u64(&data, pos + 24),
            };
            let content_hash = rd_u64(&data, pos + 32);
            let trigram_count =
                u32::from_le_bytes(data[pos + 40..pos + 44].try_into().unwrap()) as usize;
            pos += ENTRY_FIXED_LEN;
            let Some(end) = trigram_count
                .checked_mul(3)
                .and_then(|n| n.checked_add(pos))
                .filter(|&e| e <= data.len())
            else {
                break;
            };
            let mut trigrams: Vec<[u8; 3]> = data[pos..end]
                .chunks_exact(3)
                .map(|c| [c[0], c[1], c[2]])
                .collect();
            pos = end;
            // The builder relies on sorted, unique trigram lists.
            if !trigrams.is_sorted_by(|a, b| a < b) {
                trigrams.sort_unstable();
                trigrams.dedup();
            }
            entries.insert(
                path_str,
                CachedFile {
                    stat,
                    content_hash,
                    trigrams,
                },
            );
        }
        Self {
            entries,
            build_start_ns,
        }
    }

    /// Save the cache to a file (atomically via a temp file + rename).
    ///
    /// Streams entries through a buffered writer instead of materializing the
    /// whole file in memory first.
    pub fn save(&self, path: &Path) -> Result<()> {
        let valid: Vec<(&String, &CachedFile)> = self
            .entries
            .iter()
            .filter(|(p, cf)| {
                p.len() <= u16::MAX as usize && cf.trigrams.len() <= u32::MAX as usize
            })
            .collect();
        if valid.len() > u32::MAX as usize {
            return Ok(());
        }

        let parent = path.parent().unwrap_or(std::path::Path::new("."));
        let temp_path = parent.join(format!(".xgrep_cache_tmp_{}", std::process::id()));
        let result = (|| -> Result<()> {
            let mut w = BufWriter::with_capacity(256 * 1024, fs::File::create(&temp_path)?);
            w.write_all(CACHE_MAGIC)?;
            w.write_all(&CACHE_VERSION.to_le_bytes())?;
            w.write_all(&self.build_start_ns.to_le_bytes())?;
            w.write_all(&(valid.len() as u32).to_le_bytes())?;
            for (path_str, cf) in &valid {
                let path_bytes = path_str.as_bytes();
                w.write_all(&(path_bytes.len() as u16).to_le_bytes())?;
                w.write_all(path_bytes)?;
                w.write_all(&cf.stat.mtime_ns.to_le_bytes())?;
                w.write_all(&cf.stat.size.to_le_bytes())?;
                w.write_all(&cf.stat.ctime_ns.to_le_bytes())?;
                w.write_all(&cf.stat.ino.to_le_bytes())?;
                w.write_all(&cf.content_hash.to_le_bytes())?;
                w.write_all(&(cf.trigrams.len() as u32).to_le_bytes())?;
                for t in &cf.trigrams {
                    w.write_all(t)?;
                }
            }
            w.flush()?;
            Ok(())
        })();
        if let Err(e) = result {
            let _ = fs::remove_file(&temp_path);
            return Err(e);
        }
        fs::rename(&temp_path, path)?;
        Ok(())
    }
}

/// Return the cache file path for a given index path.
pub fn cache_path_for(index_path: &Path) -> PathBuf {
    index_path.with_extension("cache")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_old_format_returns_empty() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(&0u32.to_le_bytes()).unwrap();
        f.flush().unwrap();
        let cache = TrigramCache::load(f.path());
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn test_wrong_version_returns_empty() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"XGCH").unwrap();
        f.write_all(&99u32.to_le_bytes()).unwrap();
        f.write_all(&0u64.to_le_bytes()).unwrap();
        f.write_all(&0u32.to_le_bytes()).unwrap();
        f.flush().unwrap();
        let cache = TrigramCache::load(f.path());
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn test_v1_cache_is_ignored() {
        // v1 layout: magic, version=1, count, then {path_len, path, mtime, hash, n, trigrams}
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"XGCH").unwrap();
        f.write_all(&1u32.to_le_bytes()).unwrap();
        f.write_all(&1u32.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap();
        f.write_all(b"a").unwrap();
        f.write_all(&[0u8; 20]).unwrap();
        f.flush().unwrap();
        let cache = TrigramCache::load(f.path());
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn test_save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.cache");
        let mut cache = TrigramCache::new();
        cache.build_start_ns = 42;
        let stat = FileStat {
            mtime_ns: 1_234_567_890_123_456_789,
            size: 77,
            ctime_ns: 5,
            ino: 9,
        };
        cache.entries.insert(
            "src/main.rs".to_string(),
            CachedFile {
                stat,
                content_hash: 0xdeadbeef,
                trigrams: vec![*b"fn ", *b"mai"],
            },
        );
        cache.save(&path).unwrap();
        let loaded = TrigramCache::load(&path);
        assert_eq!(loaded.build_start_ns, 42);
        assert_eq!(loaded.entries.len(), 1);
        let e = loaded.entries.get("src/main.rs").unwrap();
        assert_eq!(e.stat, stat);
        assert_eq!(e.content_hash, 0xdeadbeef);
        assert_eq!(e.trigrams, vec![*b"fn ", *b"mai"]);
    }

    #[test]
    fn test_magic_bytes_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.cache");
        TrigramCache::new().save(&path).unwrap();
        let data = fs::read(&path).unwrap();
        assert_eq!(&data[0..4], b"XGCH");
        assert_eq!(
            u32::from_le_bytes(data[4..8].try_into().unwrap()),
            CACHE_VERSION
        );
        assert_eq!(u32::from_le_bytes(data[16..20].try_into().unwrap()), 0);
    }

    #[test]
    fn test_is_racy() {
        let s = |mtime_ns| FileStat {
            mtime_ns,
            ..Default::default()
        };
        let start = 10_500_000_000; // 10.5 s
        assert!(!s(9_999_999_999).is_racy(start));
        assert!(s(10_000_000_000).is_racy(start)); // same second, before start
        assert!(s(10_900_000_000).is_racy(start));
        assert!(s(11_000_000_000).is_racy(start));
    }
}
