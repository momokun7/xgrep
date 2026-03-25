//! Trigram cache for incremental index builds.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

/// Magic bytes identifying the xgrep cache format.
const CACHE_MAGIC: &[u8; 4] = b"XGCH";

/// Current cache format version.
const CACHE_VERSION: u32 = 1;

/// Cached trigram information for a file.
pub(crate) struct CachedFile {
    pub mtime: u64,
    pub content_hash: u64,
    pub trigrams: Vec<[u8; 3]>,
}

/// Cache mapping file paths to trigram information.
pub struct TrigramCache {
    pub entries: HashMap<String, CachedFile>,
}

impl TrigramCache {
    /// Create an empty cache.
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Load cache from file. Returns an empty cache if the file does not exist or is corrupted.
    pub fn load(path: &Path) -> Self {
        let data = match fs::read(path) {
            Ok(d) => d,
            Err(_) => {
                return Self {
                    entries: HashMap::new(),
                }
            }
        };
        let mut entries = HashMap::new();
        let mut pos = 0;

        // Need at least 12 bytes: 4 magic + 4 version + 4 count
        if data.len() < 12 {
            return Self { entries };
        }

        // Check magic bytes
        if &data[0..4] != CACHE_MAGIC {
            return Self { entries };
        }
        pos += 4;

        // Check version
        let version = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        if version != CACHE_VERSION {
            return Self { entries };
        }
        pos += 4;

        let count =
            u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;

        for _ in 0..count {
            if pos + 2 > data.len() {
                break;
            }
            let path_len = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
            pos += 2;
            if pos + path_len > data.len() {
                break;
            }
            let path_str = match std::str::from_utf8(&data[pos..pos + path_len]) {
                Ok(s) => s.to_string(),
                Err(_) => break,
            };
            pos += path_len;
            if pos + 20 > data.len() {
                break;
            }
            let mtime = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
            pos += 8;
            let content_hash = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
            pos += 8;
            let trigram_count = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
            pos += 4;
            if pos + trigram_count * 3 > data.len() {
                break;
            }
            let mut trigrams = Vec::with_capacity(trigram_count);
            for _ in 0..trigram_count {
                trigrams.push([data[pos], data[pos + 1], data[pos + 2]]);
                pos += 3;
            }
            entries.insert(
                path_str,
                CachedFile {
                    mtime,
                    content_hash,
                    trigrams,
                },
            );
        }
        Self { entries }
    }

    /// Save the cache to a file.
    pub fn save(&self, path: &Path) -> Result<()> {
        let mut buf = Vec::new();
        if self.entries.len() > u32::MAX as usize {
            return Ok(());
        }

        // Write header
        buf.extend_from_slice(CACHE_MAGIC);
        buf.extend_from_slice(&CACHE_VERSION.to_le_bytes());

        let valid_entries = self
            .entries
            .iter()
            .filter(|(p, _)| p.len() <= u16::MAX as usize)
            .count() as u32;
        buf.extend_from_slice(&valid_entries.to_le_bytes());
        for (path_str, cf) in &self.entries {
            let path_bytes = path_str.as_bytes();
            if path_bytes.len() > u16::MAX as usize {
                continue;
            }
            buf.extend_from_slice(&(path_bytes.len() as u16).to_le_bytes());
            buf.extend_from_slice(path_bytes);
            buf.extend_from_slice(&cf.mtime.to_le_bytes());
            buf.extend_from_slice(&cf.content_hash.to_le_bytes());
            let trigram_count: u32 = cf.trigrams.len().min(u32::MAX as usize) as u32;
            buf.extend_from_slice(&trigram_count.to_le_bytes());
            for t in &cf.trigrams {
                buf.extend_from_slice(t);
            }
        }
        let parent = path.parent().unwrap_or(std::path::Path::new("."));
        let temp_path = parent.join(format!(".xgrep_cache_tmp_{}", std::process::id()));
        fs::write(&temp_path, &buf)?;
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
        f.write_all(&0u32.to_le_bytes()).unwrap();
        f.flush().unwrap();
        let cache = TrigramCache::load(f.path());
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn test_save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.cache");
        let mut cache = TrigramCache::new();
        cache.entries.insert(
            "src/main.rs".to_string(),
            CachedFile {
                mtime: 1234567890,
                content_hash: 0xdeadbeef,
                trigrams: vec![[b'f', b'n', b' '], [b'm', b'a', b'i']],
            },
        );
        cache.save(&path).unwrap();
        let loaded = TrigramCache::load(&path);
        assert_eq!(loaded.entries.len(), 1);
        let e = loaded.entries.get("src/main.rs").unwrap();
        assert_eq!(e.mtime, 1234567890);
        assert_eq!(e.content_hash, 0xdeadbeef);
        assert_eq!(e.trigrams.len(), 2);
    }

    #[test]
    fn test_magic_bytes_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.cache");
        TrigramCache::new().save(&path).unwrap();
        let data = fs::read(&path).unwrap();
        assert_eq!(&data[0..4], b"XGCH");
        assert_eq!(u32::from_le_bytes(data[4..8].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(data[8..12].try_into().unwrap()), 0);
    }
}
