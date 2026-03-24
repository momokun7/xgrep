use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

/// キャッシュされたファイルのtrigram情報
pub(crate) struct CachedFile {
    pub mtime: u64,
    pub content_hash: u64,
    pub trigrams: Vec<[u8; 3]>,
}

/// ファイルパス→trigram情報のキャッシュ
pub struct TrigramCache {
    pub entries: HashMap<String, CachedFile>,
}

impl TrigramCache {
    /// 空のキャッシュを作成する
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// キャッシュファイルを読み込む。存在しない or 破損している場合は空キャッシュを返す。
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
        if data.len() < 4 {
            return Self { entries };
        }
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

    /// キャッシュをファイルに保存する
    pub fn save(&self, path: &Path) -> Result<()> {
        let mut buf = Vec::new();
        if self.entries.len() > u32::MAX as usize {
            // Too many entries for cache format, skip saving
            return Ok(());
        }
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

/// キャッシュファイルのパスを返す
pub fn cache_path_for(index_path: &Path) -> PathBuf {
    index_path.with_extension("cache")
}
