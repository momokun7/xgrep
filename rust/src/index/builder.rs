use std::collections::HashMap;
use std::fs;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use ignore::WalkBuilder;
use rayon::prelude::*;

use crate::index::cache::{CachedFile, TrigramCache};
use crate::index::format::*;
use crate::trigram;

// ============================================================
// Lock Guard: advisory file lock to prevent concurrent builds
// ============================================================

struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn acquire_lock(index_path: &Path) -> Result<LockGuard> {
    acquire_lock_with_retry(index_path, 3)
}

fn acquire_lock_with_retry(index_path: &Path, retries: u32) -> Result<LockGuard> {
    if retries == 0 {
        bail!(
            "Failed to acquire lock after retries (lock: {})",
            index_path.with_extension("lock").display()
        );
    }
    let lock_path = index_path.with_extension("lock");
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
    {
        Ok(mut f) => {
            let _ = write!(f, "{}", std::process::id());
            Ok(LockGuard { path: lock_path })
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Check for stale lock (process no longer alive)
            if let Ok(pid_str) = fs::read_to_string(&lock_path) {
                if let Ok(pid) = pid_str.trim().parse::<u32>() {
                    #[cfg(unix)]
                    {
                        if unsafe { libc::kill(pid as i32, 0) } != 0 {
                            let _ = fs::remove_file(&lock_path);
                            return acquire_lock_with_retry(index_path, retries - 1);
                        }
                    }
                }
            }
            bail!(
                "Index build already in progress (lock: {})",
                lock_path.display()
            )
        }
        Err(e) => bail!("Failed to create lock file: {}", e),
    }
}

#[allow(dead_code)]
pub fn build_index(root: &Path, index_path: &Path) -> Result<()> {
    build_index_with_cache(root, index_path, None)
}

pub fn build_index_with_cache(
    root: &Path,
    index_path: &Path,
    cache_path: Option<&Path>,
) -> Result<()> {
    let _lock_guard = acquire_lock(index_path)?;
    let lock_path = index_path.with_extension("lock");
    let mut cache = cache_path
        .map(TrigramCache::load)
        .unwrap_or_else(TrigramCache::new);
    let mut cache_hits = 0usize;
    let mut cache_misses = 0usize;
    // ============================================================
    // Pass 1: collect file paths, fetch metadata, count trigram occurrences
    // ============================================================
    let mut file_paths: Vec<PathBuf> = Vec::new();
    for entry in WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            name != ".xgrep"
        })
        .build()
    {
        let entry = entry?;
        if entry.file_type().is_none_or(|ft| !ft.is_file()) {
            continue;
        }
        let path = entry.path().to_path_buf();
        if path == lock_path {
            continue;
        }
        file_paths.push(path);
    }

    let mut files: Vec<FileInfo> = Vec::new();
    let mut file_trigrams: Vec<Vec<[u8; 3]>> = Vec::new();
    let mut trigram_count: HashMap<[u8; 3], u32> = HashMap::new();
    let mut total_pairs: usize = 0;

    const CHUNK_SIZE: usize = 1000;

    for chunk in file_paths.chunks(CHUNK_SIZE) {
        struct ChunkResult {
            relative_path: String,
            mtime: u64,
            size: u64,
            content_hash: u64,
            trigrams: Vec<[u8; 3]>,
            from_cache: bool,
        }

        let chunk_data: Vec<ChunkResult> = chunk
            .par_iter()
            .filter_map(|path| {
                let relative = path.strip_prefix(root).ok()?.to_string_lossy().to_string();
                let meta = fs::metadata(path).ok()?;
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let size = meta.len();

                // Cache hit check: skip file read if path + mtime match
                if let Some(cached) = cache.entries.get(&relative) {
                    if cached.mtime == mtime {
                        return Some(ChunkResult {
                            relative_path: relative,
                            mtime,
                            size,
                            content_hash: cached.content_hash,
                            trigrams: cached.trigrams.clone(),
                            from_cache: true,
                        });
                    }
                }

                // Cache miss: read file and extract trigrams
                let content = fs::read(path).ok()?;
                if memchr::memchr(0, &content).is_some() {
                    return None;
                }
                let hash = xxhash_rust::xxh64::xxh64(&content, 0);
                let trigrams = trigram::extract_trigrams(&content);
                Some(ChunkResult {
                    relative_path: relative,
                    mtime,
                    size,
                    content_hash: hash,
                    trigrams,
                    from_cache: false,
                })
            })
            .collect();

        for cr in chunk_data {
            if cr.from_cache {
                cache_hits += 1;
            } else {
                cache_misses += 1;
            }
            files.push(FileInfo {
                relative_path: cr.relative_path,
                mtime: cr.mtime,
                size: cr.size,
                content_hash: cr.content_hash,
            });
            for &t in &cr.trigrams {
                *trigram_count.entry(t).or_insert(0) += 1;
                total_pairs += 1;
            }
            file_trigrams.push(cr.trigrams);
        }
    }

    // ============================================================
    // Offset table computation (prefix sum)
    // ============================================================
    let mut sorted_trigrams: Vec<[u8; 3]> = trigram_count.keys().copied().collect();
    sorted_trigrams.sort();

    let mut offset_table: Vec<u32> = Vec::with_capacity(sorted_trigrams.len());
    let mut cumulative: u32 = 0;
    for t in &sorted_trigrams {
        offset_table.push(cumulative);
        cumulative += trigram_count[t];
    }

    let mut trigram_to_index: HashMap<[u8; 3], usize> = HashMap::new();
    for (i, t) in sorted_trigrams.iter().enumerate() {
        trigram_to_index.insert(*t, i);
    }

    let mut write_positions: Vec<u32> = offset_table.clone();

    // ============================================================
    // Create temporary file (for posting data)
    // ============================================================
    if files.len() > u32::MAX as usize {
        bail!("too many files: {} (maximum {})", files.len(), u32::MAX);
    }
    if sorted_trigrams.len() > u32::MAX as usize {
        bail!(
            "too many unique trigrams: {} (maximum {})",
            sorted_trigrams.len(),
            u32::MAX
        );
    }

    if total_pairs == 0 {
        // No trigrams at all: write directly without mmap
        let result = write_index_no_postings(index_path, &sorted_trigrams, &files);
        if result.is_ok() {
            save_cache(&mut cache, &files, &file_trigrams, cache_path)?;
        }
        return result;
    }

    let temp_dir = tempfile::tempdir()?;
    let temp_path = temp_dir.path().join("postings.tmp");
    {
        let f = fs::File::create(&temp_path)?;
        let temp_size = total_pairs
            .checked_mul(4)
            .ok_or_else(|| anyhow::anyhow!("Index too large: total_pairs overflow"))?;
        f.set_len(temp_size as u64)?;
    }

    let temp_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&temp_path)?;
    let mut temp_mmap = unsafe { memmap2::MmapMut::map_mut(&temp_file)? };

    // ============================================================
    // Pass 2: place file_ids into mmap using trigrams collected in Pass 1
    // ============================================================
    for (file_id, trigrams) in file_trigrams.iter().enumerate() {
        let file_id = file_id as u32;
        for t in trigrams {
            if let Some(&idx) = trigram_to_index.get(t) {
                let pos = write_positions[idx] as usize;
                write_positions[idx] += 1;
                let byte_offset = pos * 4;
                if byte_offset + 4 <= temp_mmap.len() {
                    temp_mmap[byte_offset..byte_offset + 4].copy_from_slice(&file_id.to_le_bytes());
                }
            }
        }
    }

    temp_mmap.flush()?;

    // ============================================================
    // Write final index file (reading from mmap)
    // Atomic replacement: write to temp file, then rename
    // ============================================================
    let parent = index_path.parent().unwrap_or(std::path::Path::new("."));
    fs::create_dir_all(parent)?;
    let temp_index_path = parent.join(format!(".xgrep_tmp_{}", std::process::id()));
    let out_file = fs::File::create(&temp_index_path)?;
    let mut writer = BufWriter::with_capacity(256 * 1024, out_file);

    // Write placeholder header (posting_total_bytes will be updated after writing postings)
    let mut header = Header {
        magic: MAGIC,
        version: VERSION,
        trigram_count: sorted_trigrams.len() as u32,
        file_count: files.len() as u32,
        posting_total_bytes: 0,
    };
    writer.write_all(&header.to_bytes())?;

    let trigram_table_size = sorted_trigrams.len() * TrigramEntry::SIZE;
    writer.write_all(&vec![0u8; trigram_table_size])?;

    let mut trigram_entries: Vec<TrigramEntry> = Vec::with_capacity(sorted_trigrams.len());
    let mut posting_buf: Vec<u8> = Vec::with_capacity(4096);
    let mut current_posting_offset: u64 = 0;

    for (i, t) in sorted_trigrams.iter().enumerate() {
        let start = offset_table[i] as usize;
        let count = trigram_count[t] as usize;

        let mut file_ids: Vec<u32> = Vec::with_capacity(count);
        for j in 0..count {
            let pos = (start + j) * 4;
            let fid = u32::from_le_bytes([
                temp_mmap[pos],
                temp_mmap[pos + 1],
                temp_mmap[pos + 2],
                temp_mmap[pos + 3],
            ]);
            file_ids.push(fid);
        }
        file_ids.sort_unstable();

        posting_buf.clear();
        encode_varint(&mut posting_buf, file_ids.len() as u32);
        let mut prev: u32 = 0;
        for &fid in &file_ids {
            encode_varint(&mut posting_buf, fid - prev);
            prev = fid;
        }

        let offset = current_posting_offset;
        if posting_buf.len() > u32::MAX as usize {
            bail!("Posting list too large for index format (> 4GB)");
        }
        let len = posting_buf.len() as u32;
        writer.write_all(&posting_buf)?;
        current_posting_offset += len as u64;

        trigram_entries.push(TrigramEntry {
            trigram: *t,
            _padding: 0,
            posting_offset: offset,
            posting_len: len,
        });
    }

    // Release mmap and temporary file
    drop(temp_mmap);
    drop(temp_file);
    drop(temp_dir);

    // Write File Table
    let mut string_pool = Vec::new();
    for fi in &files {
        let path_offset = string_pool.len() as u32;
        string_pool.extend_from_slice(fi.relative_path.as_bytes());
        string_pool.push(0);
        let entry = FileEntry {
            path_offset,
            mtime: fi.mtime,
            size: fi.size,
            content_hash: fi.content_hash,
        };
        writer.write_all(&entry.to_bytes())?;
    }

    // Write String Pool
    writer.write_all(&string_pool)?;

    // Overwrite Header's posting_total_bytes with the final value
    header.posting_total_bytes = current_posting_offset;
    writer.flush()?;
    let mut file = writer.into_inner()?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&header.to_bytes())?;

    // Seek and overwrite Trigram Table
    file.seek(SeekFrom::Start(Header::SIZE as u64))?;
    let mut trig_writer = BufWriter::with_capacity(64 * 1024, file);
    for entry in &trigram_entries {
        trig_writer.write_all(&entry.to_bytes())?;
    }
    trig_writer.flush()?;
    drop(trig_writer);

    // Atomic replacement: rename temp file to final path
    fs::rename(&temp_index_path, index_path)?;

    // Update and save cache
    save_cache(&mut cache, &files, &file_trigrams, cache_path)?;

    if cache_hits > 0 {
        eprintln!("[cache: {} hits, {} misses]", cache_hits, cache_misses);
    }

    Ok(())
}

/// Update and save the cache.
fn save_cache(
    cache: &mut TrigramCache,
    files: &[FileInfo],
    file_trigrams: &[Vec<[u8; 3]>],
    cache_path: Option<&Path>,
) -> Result<()> {
    if let Some(cp) = cache_path {
        // Update cache with current file list (excluding deleted files)
        let mut new_entries = HashMap::with_capacity(files.len());
        for (i, fi) in files.iter().enumerate() {
            new_entries.insert(
                fi.relative_path.clone(),
                CachedFile {
                    mtime: fi.mtime,
                    content_hash: fi.content_hash,
                    trigrams: file_trigrams[i].clone(),
                },
            );
        }
        cache.entries = new_entries;
        cache.save(cp)?;
    }
    Ok(())
}

/// Write index when there are no trigrams (only empty files or files shorter than 3 bytes).
fn write_index_no_postings(
    index_path: &Path,
    sorted_trigrams: &[[u8; 3]],
    files: &[FileInfo],
) -> Result<()> {
    let parent = index_path.parent().unwrap_or(std::path::Path::new("."));
    fs::create_dir_all(parent)?;
    let temp_path = parent.join(format!(".xgrep_tmp_{}", std::process::id()));
    let out_file = fs::File::create(&temp_path)?;
    let mut writer = BufWriter::with_capacity(256 * 1024, out_file);

    let header = Header {
        magic: MAGIC,
        version: VERSION,
        trigram_count: sorted_trigrams.len() as u32,
        file_count: files.len() as u32,
        posting_total_bytes: 0,
    };
    writer.write_all(&header.to_bytes())?;

    let trigram_table_size = sorted_trigrams.len() * TrigramEntry::SIZE;
    writer.write_all(&vec![0u8; trigram_table_size])?;

    // Write File Table
    let mut string_pool = Vec::new();
    for fi in files {
        let path_offset = string_pool.len() as u32;
        string_pool.extend_from_slice(fi.relative_path.as_bytes());
        string_pool.push(0);
        let entry = FileEntry {
            path_offset,
            mtime: fi.mtime,
            size: fi.size,
            content_hash: fi.content_hash,
        };
        writer.write_all(&entry.to_bytes())?;
    }

    writer.write_all(&string_pool)?;
    writer.flush()?;
    drop(writer);

    // Atomic replacement: rename temp file to final path
    fs::rename(&temp_path, index_path)?;

    Ok(())
}

struct FileInfo {
    relative_path: String,
    mtime: u64,
    size: u64,
    content_hash: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::cache::cache_path_for;
    use std::fs;
    use tempfile::tempdir;

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
        std::process::Command::new("git")
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
        // File has no trigrams (< 3 bytes)
        assert_eq!(header.trigram_count, 0);
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
    fn test_trigram_cache_save_load_roundtrip() {
        let dir = tempdir().unwrap();
        let cache_path = dir.path().join("test.cache");

        let mut cache = TrigramCache {
            entries: HashMap::new(),
        };
        cache.entries.insert(
            "hello.txt".to_string(),
            CachedFile {
                mtime: 12345,
                content_hash: 99999,
                trigrams: vec![*b"hel", *b"ell", *b"llo"],
            },
        );
        cache.entries.insert(
            "foo.rs".to_string(),
            CachedFile {
                mtime: 67890,
                content_hash: 11111,
                trigrams: vec![*b"fn ", *b"n m", *b" ma"],
            },
        );
        cache.save(&cache_path).unwrap();

        let loaded = TrigramCache::load(&cache_path);
        assert_eq!(loaded.entries.len(), 2);

        let hello = loaded.entries.get("hello.txt").unwrap();
        assert_eq!(hello.mtime, 12345);
        assert_eq!(hello.content_hash, 99999);
        assert_eq!(hello.trigrams, vec![*b"hel", *b"ell", *b"llo"]);

        let foo = loaded.entries.get("foo.rs").unwrap();
        assert_eq!(foo.mtime, 67890);
        assert_eq!(foo.content_hash, 11111);
        assert_eq!(foo.trigrams, vec![*b"fn ", *b"n m", *b" ma"]);
    }

    #[test]
    fn test_trigram_cache_load_missing_file() {
        let cache = TrigramCache::load(Path::new("/nonexistent/path/test.cache"));
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn test_trigram_cache_load_corrupt_data() {
        let dir = tempdir().unwrap();
        let cache_path = dir.path().join("bad.cache");
        fs::write(&cache_path, b"xx").unwrap(); // Less than 4 bytes
        let cache = TrigramCache::load(&cache_path);
        assert!(cache.entries.is_empty());
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

        // Verify cache contains entries
        let cache = TrigramCache::load(&cache_path);
        assert_eq!(cache.entries.len(), 1);
        assert!(cache.entries.contains_key("hello.txt"));
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

        // Manually create a lock file with our PID (simulating a concurrent build)
        let lock_path = index_path.with_extension("lock");
        fs::write(&lock_path, format!("{}", std::process::id())).unwrap();

        // Build should fail because lock exists and our process is alive
        let result = build_index(root, &index_path);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("already in progress"));

        // Clean up lock
        fs::remove_file(&lock_path).unwrap();

        // Now build should succeed
        let result = build_index(root, &index_path);
        assert!(result.is_ok());
    }

    #[test]
    fn test_stale_lock_recovery() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello").unwrap();
        let index_path = root.join("index.xgrep");

        // Create a lock file with a non-existent PID
        let lock_path = index_path.with_extension("lock");
        fs::write(&lock_path, "999999999").unwrap();

        // Build should succeed (stale lock recovered)
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

        // Verify it was also removed from cache
        let cache = TrigramCache::load(&cache_path);
        assert_eq!(cache.entries.len(), 1);
        assert!(!cache.entries.contains_key("b.txt"));
    }
}
