use std::fs::File;
use std::path::Path;

use anyhow::{Result, bail};
use memmap2::Mmap;
#[cfg(unix)]
use libc;

use crate::index::format::*;

pub struct IndexReader {
    mmap: Mmap,
    cached_header: Header,
    posting_lists_start: usize,
    file_table_start: usize,
    string_pool_start: usize,
}

impl IndexReader {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let mmap = unsafe { Mmap::map(&file)? };

        #[cfg(unix)]
        {
            unsafe {
                libc::madvise(
                    mmap.as_ptr() as *mut libc::c_void,
                    mmap.len(),
                    libc::MADV_WILLNEED,
                );
            }
        }

        if mmap.len() < Header::SIZE {
            bail!("Index file too small");
        }

        let header: Header = unsafe { std::ptr::read_unaligned(mmap.as_ptr() as *const Header) };
        if &header.magic != b"XGRP" {
            bail!("Invalid index magic");
        }
        if header.version != VERSION {
            bail!("Unsupported index version: {}", header.version);
        }

        let trigram_table_start = Header::SIZE;
        let posting_lists_start = trigram_table_start + (header.trigram_count as usize) * TrigramEntry::SIZE;

        // Calculate total posting list bytes from trigram entries
        let mut posting_lists_total_bytes = 0usize;
        for i in 0..header.trigram_count as usize {
            let entry_offset = trigram_table_start + i * TrigramEntry::SIZE;
            let entry: TrigramEntry = unsafe {
                std::ptr::read_unaligned(mmap[entry_offset..].as_ptr() as *const TrigramEntry)
            };
            let end = entry.posting_offset as usize + entry.posting_len as usize;
            if end > posting_lists_total_bytes {
                posting_lists_total_bytes = end;
            }
        }

        let file_table_start = posting_lists_start + posting_lists_total_bytes;
        let string_pool_start = file_table_start + (header.file_count as usize) * FileEntry::SIZE;

        Ok(Self { mmap, cached_header: header, posting_lists_start, file_table_start, string_pool_start })
    }

    pub fn header(&self) -> Header {
        self.cached_header
    }

    pub fn file_count(&self) -> u32 {
        self.cached_header.file_count
    }

    pub fn lookup_trigram(&self, target: [u8; 3]) -> Vec<u32> {
        let count = self.cached_header.trigram_count as usize;
        if count == 0 { return vec![]; }

        let trigram_table_start = Header::SIZE;
        let mut lo = 0usize;
        let mut hi = count;

        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let offset = trigram_table_start + mid * TrigramEntry::SIZE;
            let entry: TrigramEntry = unsafe {
                std::ptr::read_unaligned(self.mmap[offset..].as_ptr() as *const TrigramEntry)
            };

            match entry.trigram.cmp(&target) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    let pl_start = self.posting_lists_start + entry.posting_offset as usize;
                    let pl_byte_len = entry.posting_len as usize;
                    let data = &self.mmap[pl_start..pl_start + pl_byte_len];
                    return Self::decode_posting_list(data);
                }
            }
        }
        vec![]
    }

    fn decode_posting_list(data: &[u8]) -> Vec<u32> {
        let mut pos = 0;
        let (count, bytes_read) = decode_varint(&data[pos..]);
        pos += bytes_read;

        let mut result = Vec::with_capacity(count as usize);
        let mut prev: u32 = 0;
        for _ in 0..count {
            let (delta, bytes_read) = decode_varint(&data[pos..]);
            pos += bytes_read;
            prev += delta;
            result.push(prev);
        }
        result
    }

    /// 2バイトプレフィックスに一致する全trigramのposting listをunionして返す。
    /// trigram tableはソート済みなのでbinary searchで範囲を特定する。
    pub fn lookup_trigram_prefix(&self, prefix: [u8; 2]) -> Vec<u32> {
        let count = self.cached_header.trigram_count as usize;
        if count == 0 { return vec![]; }

        let trigram_table_start = Header::SIZE;

        // lower bound: prefix[0], prefix[1], 0x00
        let lo_target = [prefix[0], prefix[1], 0u8];
        let lo_idx = {
            let mut lo = 0usize;
            let mut hi = count;
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                let offset = trigram_table_start + mid * TrigramEntry::SIZE;
                let entry: TrigramEntry = unsafe {
                    std::ptr::read_unaligned(self.mmap[offset..].as_ptr() as *const TrigramEntry)
                };
                if entry.trigram < lo_target {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            lo
        };

        // upper bound: prefix[0], prefix[1], 0xFF (exclusive upper bound is next entry after 0xFF)
        let hi_target = [prefix[0], prefix[1], 0xFFu8];
        let hi_idx = {
            let mut lo = lo_idx;
            let mut hi = count;
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                let offset = trigram_table_start + mid * TrigramEntry::SIZE;
                let entry: TrigramEntry = unsafe {
                    std::ptr::read_unaligned(self.mmap[offset..].as_ptr() as *const TrigramEntry)
                };
                if entry.trigram <= hi_target {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            lo
        };

        if lo_idx >= hi_idx { return vec![]; }

        let mut seen = std::collections::BTreeSet::new();
        for i in lo_idx..hi_idx {
            let offset = trigram_table_start + i * TrigramEntry::SIZE;
            let entry: TrigramEntry = unsafe {
                std::ptr::read_unaligned(self.mmap[offset..].as_ptr() as *const TrigramEntry)
            };
            // sanity check: prefixが一致するエントリのみ処理
            if entry.trigram[0] != prefix[0] || entry.trigram[1] != prefix[1] { continue; }
            let pl_start = self.posting_lists_start + entry.posting_offset as usize;
            let pl_byte_len = entry.posting_len as usize;
            let data = &self.mmap[pl_start..pl_start + pl_byte_len];
            for fid in Self::decode_posting_list(data) {
                seen.insert(fid);
            }
        }
        seen.into_iter().collect()
    }

    pub fn file_path(&self, file_id: u32) -> &str {
        let offset = self.file_table_start + file_id as usize * FileEntry::SIZE;
        let entry: FileEntry = unsafe {
            std::ptr::read_unaligned(self.mmap[offset..].as_ptr() as *const FileEntry)
        };
        let str_start = self.string_pool_start + entry.path_offset as usize;
        let remaining = &self.mmap[str_start..];
        let len = memchr::memchr(0, remaining).unwrap_or(remaining.len());
        std::str::from_utf8(&remaining[..len]).unwrap_or("<invalid>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::builder;
    use std::fs;
    use tempfile::tempdir;

    fn build_test_index() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("hello.txt"), "hello world").unwrap();
        fs::write(root.join("foo.rs"), "fn hello_world() {}").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        (dir, index_path)
    }

    #[test]
    fn test_open_and_header() {
        let (_dir, index_path) = build_test_index();
        let reader = IndexReader::open(&index_path).unwrap();
        let h = reader.header();
        assert_eq!(&h.magic, b"XGRP");
        assert_eq!(h.file_count, 2);
    }

    #[test]
    fn test_lookup_trigram_found() {
        let (_dir, index_path) = build_test_index();
        let reader = IndexReader::open(&index_path).unwrap();
        let posting = reader.lookup_trigram(*b"hel");
        assert!(posting.len() >= 2);
    }

    #[test]
    fn test_lookup_missing_trigram() {
        let (_dir, index_path) = build_test_index();
        let reader = IndexReader::open(&index_path).unwrap();
        let posting = reader.lookup_trigram(*b"zzz");
        assert_eq!(posting.len(), 0);
    }

    #[test]
    fn test_file_path() {
        let (_dir, index_path) = build_test_index();
        let reader = IndexReader::open(&index_path).unwrap();
        let p0 = reader.file_path(0);
        let p1 = reader.file_path(1);
        let paths: Vec<&str> = vec![p0, p1];
        assert!(paths.iter().any(|p| p.ends_with(".txt")));
        assert!(paths.iter().any(|p| p.ends_with(".rs")));
    }
}
