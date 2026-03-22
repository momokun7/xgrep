use std::collections::HashMap;
use std::fs;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use ignore::WalkBuilder;
use rayon::prelude::*;

use crate::index::format::*;
use crate::trigram;

pub fn build_index(root: &Path, index_path: &Path) -> Result<()> {
    // Phase 1: ファイルパス収集 (逐次、高速)
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
        if entry.file_type().map_or(true, |ft| !ft.is_file()) {
            continue;
        }
        file_paths.push(entry.path().to_path_buf());
    }

    // Phase 2: チャンク処理でファイル読み込み & trigram抽出 (メモリ削減)
    struct FileData {
        relative_path: String,
        mtime: u64,
        size: u64,
        content_hash: u64,
        trigrams: Vec<[u8; 3]>,
    }

    let mut files: Vec<(String, u64, u64, u64)> = Vec::new();
    let mut trigram_to_files: HashMap<[u8; 3], Vec<u32>> = HashMap::new();

    const CHUNK_SIZE: usize = 1000;

    for chunk in file_paths.chunks(CHUNK_SIZE) {
        // チャンク内を rayon で並列処理
        let chunk_data: Vec<FileData> = chunk
            .par_iter()
            .filter_map(|path| {
                let content = fs::read(path).ok()?;

                // バイナリファイルスキップ (NULバイト検出)
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

                Some(FileData {
                    relative_path: relative,
                    mtime,
                    size,
                    content_hash: hash,
                    trigrams,
                })
            })
            .collect();

        // チャンク結果を共有ステートにマージ (逐次)
        for fd in chunk_data {
            let file_id = files.len() as u32;
            files.push((fd.relative_path, fd.mtime, fd.size, fd.content_hash));
            for t in fd.trigrams {
                trigram_to_files.entry(t).or_default().push(file_id);
            }
        }
        // chunk_data はここで drop され、trigram Vec とファイル内容が解放される
    }

    // posting list のメモリを最小化
    for list in trigram_to_files.values_mut() {
        list.shrink_to_fit();
    }

    // Phase 2: trigramをソート
    let mut sorted_trigrams: Vec<[u8; 3]> = trigram_to_files.keys().copied().collect();
    sorted_trigrams.sort();

    // Phase 3: ストリーミング書き出し（Vec<u8>バッファを使わず直接ファイルに書く）
    if let Some(parent) = index_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = fs::File::create(index_path)?;
    let mut writer = BufWriter::with_capacity(256 * 1024, file);

    // Header 書き出し
    let header = Header {
        magic: MAGIC,
        version: VERSION,
        trigram_count: sorted_trigrams.len() as u32,
        file_count: files.len() as u32,
    };
    writer.write_all(&header.to_bytes())?;

    // Trigram Table のプレースホルダー（後で seek して上書き）
    let trigram_table_size = sorted_trigrams.len() * TrigramEntry::SIZE;
    writer.write_all(&vec![0u8; trigram_table_size])?;

    // Posting Lists を書き出し、オフセットを記録
    let mut trigram_entries: Vec<TrigramEntry> = Vec::with_capacity(sorted_trigrams.len());
    let mut posting_buf: Vec<u8> = Vec::with_capacity(4096);
    let mut current_posting_offset: u64 = 0;

    for t in &sorted_trigrams {
        // remove で取り出すことで、書き出し済みの posting list を即座に解放
        let file_ids = trigram_to_files.remove(t).unwrap_or_default();

        posting_buf.clear();

        // count を varint で書き出し
        encode_varint(&mut posting_buf, file_ids.len() as u32);

        // delta-encoded file_ids を varint で書き出し
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
        // file_ids はここで drop され、メモリが解放される
    }

    // HashMap を明示的に解放（File Table 書き出し前にメモリを空ける）
    drop(trigram_to_files);

    // File Table 書き出し
    let mut string_pool = Vec::new();

    for (rel_path, mtime, size, hash) in &files {
        let path_offset = string_pool.len() as u32;
        string_pool.extend_from_slice(rel_path.as_bytes());
        string_pool.push(0);

        let entry = FileEntry {
            path_offset,
            mtime: *mtime,
            size: *size,
            content_hash: *hash,
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
        fs::write(
            root.join("ignored_dir/secret.txt"),
            "should be ignored",
        )
        .unwrap();
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
