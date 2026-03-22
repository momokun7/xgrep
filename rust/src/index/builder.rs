use std::collections::HashMap;
use std::fs;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::Result;
use ignore::WalkBuilder;
use rayon::prelude::*;

use crate::index::format::*;
use crate::trigram;

pub fn build_index(root: &Path, index_path: &Path) -> Result<()> {
    // ============================================================
    // Pass 1: ファイルパス収集、メタデータ取得、trigram出現数カウント
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
        file_paths.push(entry.path().to_path_buf());
    }

    let mut files: Vec<FileInfo> = Vec::new();
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
        }

        let chunk_data: Vec<ChunkResult> = chunk
            .par_iter()
            .filter_map(|path| {
                let content = fs::read(path).ok()?;
                if memchr::memchr(0, &content).is_some() {
                    return None;
                }
                let relative = path.strip_prefix(root).ok()?.to_string_lossy().to_string();
                let meta = fs::metadata(path).ok()?;
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let size = meta.len();
                let hash = xxhash_rust::xxh64::xxh64(&content, 0);
                let trigrams = trigram::extract_trigrams(&content);
                Some(ChunkResult {
                    relative_path: relative,
                    mtime,
                    size,
                    content_hash: hash,
                    trigrams,
                })
            })
            .collect();

        for cr in chunk_data {
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
        }
    }

    // ============================================================
    // オフセットテーブル計算 (prefix sum)
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

    let write_positions: Vec<AtomicU32> = offset_table
        .iter()
        .map(|&off| AtomicU32::new(off))
        .collect();

    // ============================================================
    // テンポラリファイル作成 (posting data用)
    // ============================================================
    if total_pairs == 0 {
        // trigramが1つもない場合はmmapなしで直接書き出し
        return write_index_no_postings(index_path, &sorted_trigrams, &files);
    }

    let temp_dir = tempfile::tempdir()?;
    let temp_path = temp_dir.path().join("postings.tmp");
    {
        let f = fs::File::create(&temp_path)?;
        f.set_len((total_pairs * 4) as u64)?;
    }

    let temp_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&temp_path)?;
    let mut temp_mmap = unsafe { memmap2::MmapMut::map_mut(&temp_file)? };

    // ============================================================
    // Pass 2: ファイル再読み込み、file_idをmmapに配置
    // ============================================================
    let temp_mmap_ptr = temp_mmap.as_mut_ptr();
    let temp_mmap_len = temp_mmap.len();

    let file_count = files.len();
    let file_indices: Vec<usize> = (0..file_count).collect();

    for chunk in file_indices.chunks(CHUNK_SIZE) {
        let chunk_trigrams: Vec<(u32, Vec<[u8; 3]>)> = chunk
            .par_iter()
            .filter_map(|&file_id| {
                let full_path = root.join(&files[file_id].relative_path);
                let content = fs::read(&full_path).ok()?;
                if memchr::memchr(0, &content).is_some() {
                    return None;
                }
                let trigrams = trigram::extract_trigrams(&content);
                Some((file_id as u32, trigrams))
            })
            .collect();

        for (file_id, trigrams) in chunk_trigrams {
            for t in &trigrams {
                if let Some(&idx) = trigram_to_index.get(t) {
                    let pos = write_positions[idx].fetch_add(1, Ordering::Relaxed) as usize;
                    if pos * 4 + 4 <= temp_mmap_len {
                        let bytes = file_id.to_ne_bytes();
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                bytes.as_ptr(),
                                temp_mmap_ptr.add(pos * 4),
                                4,
                            );
                        }
                    }
                }
            }
        }
    }

    temp_mmap.flush()?;

    // ============================================================
    // 最終インデックスファイル書き出し (mmapから読み取り)
    // ============================================================
    if let Some(parent) = index_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let out_file = fs::File::create(index_path)?;
    let mut writer = BufWriter::with_capacity(256 * 1024, out_file);

    let header = Header {
        magic: MAGIC,
        version: VERSION,
        trigram_count: sorted_trigrams.len() as u32,
        file_count: files.len() as u32,
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
            let fid = u32::from_ne_bytes([
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

    // mmapとテンポラリファイルを解放
    drop(temp_mmap);
    drop(temp_file);
    drop(temp_dir);

    // File Table 書き出し
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

    // String Pool 書き出し
    writer.write_all(&string_pool)?;

    // Trigram Table を seek して上書き
    writer.flush()?;
    let mut file = writer.into_inner()?;
    file.seek(SeekFrom::Start(Header::SIZE as u64))?;
    let mut trig_writer = BufWriter::with_capacity(64 * 1024, file);
    for entry in &trigram_entries {
        trig_writer.write_all(&entry.to_bytes())?;
    }
    trig_writer.flush()?;

    Ok(())
}

/// trigramが1つもない場合の書き出し (空ファイルや3バイト未満のファイルのみ)
fn write_index_no_postings(
    index_path: &Path,
    sorted_trigrams: &[[u8; 3]],
    files: &[FileInfo],
) -> Result<()> {
    if let Some(parent) = index_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let out_file = fs::File::create(index_path)?;
    let mut writer = BufWriter::with_capacity(256 * 1024, out_file);

    let header = Header {
        magic: MAGIC,
        version: VERSION,
        trigram_count: sorted_trigrams.len() as u32,
        file_count: files.len() as u32,
    };
    writer.write_all(&header.to_bytes())?;

    let trigram_table_size = sorted_trigrams.len() * TrigramEntry::SIZE;
    writer.write_all(&vec![0u8; trigram_table_size])?;

    // File Table 書き出し
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
    use crate::index::format::*;
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
        let header: Header = unsafe { std::ptr::read_unaligned(data.as_ptr() as *const Header) };
        assert_eq!(&header.magic, b"XGRP");
        assert_eq!(header.version, 1);
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
        let header: Header = unsafe { std::ptr::read_unaligned(data.as_ptr() as *const Header) };
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
        let header: Header = unsafe { std::ptr::read_unaligned(data.as_ptr() as *const Header) };
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
        let header: Header = unsafe { std::ptr::read_unaligned(data.as_ptr() as *const Header) };
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
        let header: Header = unsafe { std::ptr::read_unaligned(data.as_ptr() as *const Header) };
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
        let header: Header = unsafe { std::ptr::read_unaligned(data.as_ptr() as *const Header) };
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
        let header: Header = unsafe { std::ptr::read_unaligned(data.as_ptr() as *const Header) };
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
        let header: Header = unsafe { std::ptr::read_unaligned(data.as_ptr() as *const Header) };
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
        let header: Header = unsafe { std::ptr::read_unaligned(data.as_ptr() as *const Header) };
        assert_eq!(header.file_count, 1);
    }
}
