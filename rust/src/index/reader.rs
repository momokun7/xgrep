use std::fs::File;
use std::path::Path;

use crate::error::{Result, XgrepError};
#[cfg(unix)]
use libc;
use memmap2::Mmap;

use crate::index::format::*;

pub fn read_header(data: &[u8]) -> Header {
    Header {
        magic: [data[0], data[1], data[2], data[3]],
        version: u32::from_le_bytes([data[4], data[5], data[6], data[7]]),
        trigram_count: u32::from_le_bytes([data[8], data[9], data[10], data[11]]),
        file_count: u32::from_le_bytes([data[12], data[13], data[14], data[15]]),
        posting_total_bytes: u64::from_le_bytes(data[16..24].try_into().unwrap()),
    }
}

fn read_trigram_entry(data: &[u8]) -> TrigramEntry {
    TrigramEntry {
        trigram: [data[0], data[1], data[2]],
        _padding: data[3],
        posting_offset: u64::from_le_bytes(data[4..12].try_into().unwrap()),
        posting_len: u32::from_le_bytes(data[12..16].try_into().unwrap()),
    }
}

fn read_file_entry(data: &[u8]) -> FileEntry {
    FileEntry {
        path_offset: u32::from_le_bytes(data[0..4].try_into().unwrap()),
        mtime: u64::from_le_bytes(data[4..12].try_into().unwrap()),
        size: u64::from_le_bytes(data[12..20].try_into().unwrap()),
        content_hash: u64::from_le_bytes(data[20..28].try_into().unwrap()),
    }
}

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
        // SAFETY: `file` is a valid, open file descriptor. The mapped memory may become
        // invalid if the file is modified externally; we mitigate this with advisory
        // locking during index builds.
        let mmap = unsafe { Mmap::map(&file)? };

        #[cfg(unix)]
        {
            // SAFETY: `mmap.as_ptr()` and `mmap.len()` are valid (mmap created above).
            // MADV_WILLNEED is an advisory hint that does not modify memory contents.
            let ret = unsafe {
                libc::madvise(
                    mmap.as_ptr() as *mut libc::c_void,
                    mmap.len(),
                    libc::MADV_WILLNEED,
                )
            };
            if ret != 0 && !crate::mcp::is_quiet() {
                eprintln!(
                    "xgrep: madvise warning: {}",
                    std::io::Error::last_os_error()
                );
            }
        }

        if mmap.len() < Header::SIZE {
            return Err(XgrepError::IndexError("index file too small".to_string()));
        }

        let header = read_header(&mmap[..Header::SIZE]);
        if &header.magic != b"XGRP" {
            return Err(XgrepError::IndexError("invalid index magic".to_string()));
        }
        if header.version != VERSION {
            return Err(XgrepError::IndexError(format!(
                "unsupported index version: {}",
                header.version
            )));
        }

        let trigram_table_size = (header.trigram_count as usize)
            .checked_mul(TrigramEntry::SIZE)
            .ok_or_else(|| {
                XgrepError::IndexError("header overflow: trigram_count too large".to_string())
            })?;
        let posting_lists_start = Header::SIZE
            .checked_add(trigram_table_size)
            .ok_or_else(|| XgrepError::IndexError("header overflow".to_string()))?;

        // Verify trigram table end is within mmap bounds
        if posting_lists_start > mmap.len() {
            return Err(XgrepError::IndexError(format!(
                "index file is truncated or corrupt (trigram table exceeds file size: need {} bytes, got {})",
                posting_lists_start, mmap.len()
            )));
        }

        let posting_lists_total_bytes = header.posting_total_bytes as usize;
        let file_table_start = posting_lists_start
            .checked_add(posting_lists_total_bytes)
            .ok_or_else(|| {
                XgrepError::IndexError("header overflow: posting_total_bytes too large".to_string())
            })?;
        let file_table_size = (header.file_count as usize)
            .checked_mul(FileEntry::SIZE)
            .ok_or_else(|| {
                XgrepError::IndexError("header overflow: file_count too large".to_string())
            })?;
        let string_pool_start = file_table_start
            .checked_add(file_table_size)
            .ok_or_else(|| XgrepError::IndexError("header overflow".to_string()))?;

        // Verify all offsets are within mmap bounds
        if string_pool_start > mmap.len() {
            return Err(XgrepError::IndexError(format!(
                "index file is truncated or corrupt (expected at least {} bytes, got {})",
                string_pool_start,
                mmap.len()
            )));
        }

        Ok(Self {
            mmap,
            cached_header: header,
            posting_lists_start,
            file_table_start,
            string_pool_start,
        })
    }

    pub fn header(&self) -> Header {
        self.cached_header
    }

    pub fn file_count(&self) -> u32 {
        self.cached_header.file_count
    }

    pub fn lookup_trigram(&self, target: [u8; 3]) -> Vec<u32> {
        let count = self.cached_header.trigram_count as usize;
        if count == 0 {
            return vec![];
        }

        let trigram_table_start = Header::SIZE;
        let mut lo = 0usize;
        let mut hi = count;

        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let offset = trigram_table_start + mid * TrigramEntry::SIZE;
            if offset + TrigramEntry::SIZE > self.mmap.len() {
                return vec![];
            }
            let entry = read_trigram_entry(&self.mmap[offset..offset + TrigramEntry::SIZE]);

            match entry.trigram.cmp(&target) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    let pl_start = self.posting_lists_start + entry.posting_offset as usize;
                    let pl_end = pl_start + entry.posting_len as usize;
                    if pl_end > self.mmap.len() {
                        return vec![];
                    }
                    let data = &self.mmap[pl_start..pl_end];
                    return Self::decode_posting_list(data);
                }
            }
        }
        vec![]
    }

    pub fn decode_posting_list(data: &[u8]) -> Vec<u32> {
        if data.is_empty() {
            return vec![];
        }
        let (count, mut pos) = decode_varint(data);
        let count = count as usize;
        // Sanity check: count exceeding data length indicates corrupt data
        if count > data.len() {
            return vec![];
        }
        let mut result = Vec::with_capacity(count.min(1024));
        let mut prev: u32 = 0;
        for _ in 0..count {
            if pos >= data.len() {
                break; // Data is truncated
            }
            let (delta, bytes_read) = decode_varint(&data[pos..]);
            if bytes_read == 0 {
                break; // Prevent infinite loop when no progress
            }
            pos += bytes_read;
            prev = prev.saturating_add(delta);
            result.push(prev);
        }
        result
    }

    /// Return the union of posting lists for all trigrams matching a 2-byte prefix.
    /// Uses binary search on the sorted trigram table to find the range.
    pub fn lookup_trigram_prefix(&self, prefix: [u8; 2]) -> Vec<u32> {
        let count = self.cached_header.trigram_count as usize;
        if count == 0 {
            return vec![];
        }

        let trigram_table_start = Header::SIZE;

        // lower bound: prefix[0], prefix[1], 0x00
        let lo_target = [prefix[0], prefix[1], 0u8];
        let lo_idx = {
            let mut lo = 0usize;
            let mut hi = count;
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                let offset = trigram_table_start + mid * TrigramEntry::SIZE;
                let entry = read_trigram_entry(&self.mmap[offset..offset + TrigramEntry::SIZE]);
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
                let entry = read_trigram_entry(&self.mmap[offset..offset + TrigramEntry::SIZE]);
                if entry.trigram <= hi_target {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            lo
        };

        if lo_idx >= hi_idx {
            return vec![];
        }

        let mut seen = std::collections::BTreeSet::new();
        for i in lo_idx..hi_idx {
            let offset = trigram_table_start + i * TrigramEntry::SIZE;
            let entry = read_trigram_entry(&self.mmap[offset..offset + TrigramEntry::SIZE]);
            // Sanity check: only process entries matching the prefix
            if entry.trigram[0] != prefix[0] || entry.trigram[1] != prefix[1] {
                continue;
            }
            let pl_start = self.posting_lists_start + entry.posting_offset as usize;
            let pl_byte_len = entry.posting_len as usize;
            let pl_end = pl_start + pl_byte_len;
            if pl_end > self.mmap.len() {
                continue; // skip corrupted entry
            }
            let data = &self.mmap[pl_start..pl_end];
            for fid in Self::decode_posting_list(data) {
                seen.insert(fid);
            }
        }
        seen.into_iter().collect()
    }

    pub fn file_path(&self, file_id: u32) -> &str {
        if file_id >= self.cached_header.file_count {
            return "<invalid file_id>";
        }
        let offset = self.file_table_start + file_id as usize * FileEntry::SIZE;
        if offset + FileEntry::SIZE > self.mmap.len() {
            return "<invalid file_id>";
        }
        let entry = read_file_entry(&self.mmap[offset..offset + FileEntry::SIZE]);
        let str_start = self.string_pool_start + entry.path_offset as usize;
        if str_start >= self.mmap.len() {
            return "<invalid>";
        }
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
    fn test_open_empty_index() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // Build empty index
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        assert_eq!(reader.file_count(), 0);
        assert_eq!(reader.header().trigram_count, 0);
        // Lookup should return empty
        let posting = reader.lookup_trigram(*b"abc");
        assert!(posting.is_empty());
    }

    #[test]
    fn test_open_invalid_magic() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.xgrep");
        fs::write(&path, b"BADMxxxxxxxxxxxxxxxx").unwrap();
        assert!(IndexReader::open(&path).is_err());
    }

    #[test]
    fn test_open_file_too_small() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tiny.xgrep");
        fs::write(&path, b"XGR").unwrap(); // only 3 bytes, need 20
        assert!(IndexReader::open(&path).is_err());
    }

    #[test]
    fn test_open_invalid_version() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("badver.xgrep");
        let mut data = vec![0u8; 24];
        data[0..4].copy_from_slice(b"XGRP");
        // version = 99 (invalid)
        data[4..8].copy_from_slice(&99u32.to_le_bytes());
        fs::write(&path, &data).unwrap();
        assert!(IndexReader::open(&path).is_err());
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

    #[test]
    fn test_open_truncated_index() {
        // Header is valid but trigram_count is huge with insufficient actual data
        let dir = tempdir().unwrap();
        let path = dir.path().join("truncated.xgrep");
        let mut data = vec![0u8; 28]; // Header(24) + only 4 bytes
        data[0..4].copy_from_slice(b"XGRP");
        data[4..8].copy_from_slice(&VERSION.to_le_bytes());
        data[8..12].copy_from_slice(&9999u32.to_le_bytes()); // trigram_count = 9999 (huge)
        data[12..16].copy_from_slice(&0u32.to_le_bytes()); // file_count = 0
        data[16..24].copy_from_slice(&0u64.to_le_bytes()); // posting_total_bytes = 0
        fs::write(&path, &data).unwrap();
        let result = IndexReader::open(&path);
        assert!(result.is_err());
    }

    #[test]
    fn test_file_path_invalid_id() {
        let (_dir, index_path) = build_test_index();
        let reader = IndexReader::open(&index_path).unwrap();
        // Pass an ID >= file_count
        let path = reader.file_path(9999);
        assert_eq!(path, "<invalid file_id>");
    }

    #[test]
    fn test_decode_posting_list_empty() {
        let result = IndexReader::decode_posting_list(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_lookup_trigram_prefix_corrupt_posting() {
        // Verify that corrupted posting offsets don't panic
        // Just verify the function doesn't crash with a valid but minimal index
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "abcdef").unwrap();
        let index_path = root.join("index.xgrep");
        crate::index::builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        // Search for prefix that may or may not exist - should not panic
        let _ = reader.lookup_trigram_prefix(*b"zz");
        let _ = reader.lookup_trigram_prefix(*b"ab");
    }

    #[test]
    fn test_decode_posting_list_malformed() {
        // count is 100 but only 2 bytes of data
        let mut data = Vec::new();
        crate::index::format::encode_varint(&mut data, 100);
        // No data for deltas
        let result = IndexReader::decode_posting_list(&data);
        // Should not infinite-loop; returns empty or partial result
        assert!(result.len() < 100);
    }
}
