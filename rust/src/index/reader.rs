use std::fs::File;
use std::path::Path;

use crate::error::{Result, XgrepError};
#[cfg(unix)]
use libc;
use memmap2::Mmap;

use crate::index::format::*;
use crate::trigram::{fold, fold_trigram};

pub fn read_header(data: &[u8]) -> Header {
    let sidecar_id = if data.len() >= 32 {
        u64::from_le_bytes(data[24..32].try_into().unwrap())
    } else {
        0
    };
    Header {
        magic: [data[0], data[1], data[2], data[3]],
        version: u32::from_le_bytes([data[4], data[5], data[6], data[7]]),
        trigram_count: u32::from_le_bytes([data[8], data[9], data[10], data[11]]),
        file_count: u32::from_le_bytes([data[12], data[13], data[14], data[15]]),
        posting_total_bytes: u64::from_le_bytes(data[16..24].try_into().unwrap()),
        sidecar_id,
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

/// Format version of the index file at `path`, read from its header without
/// mapping the file. `None` if the file is missing or not an xgrep index.
pub fn index_version(path: &Path) -> Option<u32> {
    use std::io::Read;
    let mut buf = [0u8; 8];
    File::open(path).ok()?.read_exact(&mut buf).ok()?;
    if &buf[0..4] != MAGIC.as_slice() {
        return None;
    }
    Some(u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]))
}

/// True if an index file exists at `path` but cannot be used as is (older
/// format version or not an xgrep index) and therefore must be rebuilt.
pub fn needs_rebuild(path: &Path) -> bool {
    path.exists() && index_version(path) != Some(VERSION)
}

/// A decoded posting list: file IDs plus the raw `[loc, next]` mask bytes.
pub(crate) struct Posting<'a> {
    pub ids: Vec<u32>,
    masks: &'a [u8],
}

impl Posting<'_> {
    /// `loc` mask of entry `i`. Missing (corrupt) masks read as "all bits",
    /// which can only add candidates, never drop them.
    #[inline]
    pub fn loc(&self, i: usize) -> u8 {
        self.masks.get(2 * i).copied().unwrap_or(0xFF)
    }

    /// `next` mask of entry `i` (see [`Posting::loc`] for missing data).
    #[inline]
    pub fn next(&self, i: usize) -> u8 {
        self.masks.get(2 * i + 1).copied().unwrap_or(0xFF)
    }

    /// Packed masks (`loc | next << 8`) of entry `i`.
    #[inline]
    pub fn packed(&self, i: usize) -> u16 {
        pack_masks(self.loc(i), self.next(i))
    }
}

/// Ask the kernel to prefetch the first `hot_len` bytes of the mapping (the
/// trigram table and the posting lists, which every indexed search touches).
/// The file table / path pool are left to normal demand paging.
///
/// Measured on a warm page cache (13.8K files, 48 MB index), WILLNEED on
/// table + postings, on the whole file, MADV_RANDOM and no advice were all
/// within noise (≈2 ms per search process), so the prefetch is kept for the
/// cold-cache case where it overlaps the table/posting reads.
fn advise(mmap: &Mmap, hot_len: usize) {
    #[cfg(unix)]
    {
        let len = hot_len.min(mmap.len());
        if len == 0 {
            return;
        }
        // SAFETY: the range lies within the live mapping; madvise only gives
        // the kernel a hint and does not modify memory contents.
        let _ret =
            unsafe { libc::madvise(mmap.as_ptr() as *mut libc::c_void, len, libc::MADV_WILLNEED) };
        #[cfg(debug_assertions)]
        if _ret != 0 {
            eprintln!(
                "xgrep: madvise warning: {}",
                std::io::Error::last_os_error()
            );
        }
    }
    #[cfg(not(unix))]
    let _ = (mmap, hot_len);
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
        // locking during index builds (and builds replace the file by rename).
        let mmap = unsafe { Mmap::map(&file)? };

        if mmap.len() < Header::SIZE {
            return Err(XgrepError::IndexError("index file too small".to_string()));
        }

        let header = read_header(&mmap[..Header::SIZE]);
        if header.magic != MAGIC {
            return Err(XgrepError::IndexError("invalid index magic".to_string()));
        }
        if header.version != VERSION {
            return Err(XgrepError::IndexError(format!(
                "unsupported index version: {} (expected {}; rebuild with `xg init`)",
                header.version, VERSION
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

        let posting_lists_total_bytes = usize::try_from(header.posting_total_bytes)
            .map_err(|_| XgrepError::IndexError("header overflow".to_string()))?;
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

        // Prefetch the trigram table and the posting lists (what every
        // indexed search touches); file table / paths are read on demand.
        advise(&mmap, file_table_start);

        Ok(Self {
            mmap,
            cached_header: header,
            posting_lists_start,
            file_table_start,
            string_pool_start,
        })
    }

    #[cfg(test)]
    pub(crate) fn header(&self) -> Header {
        self.cached_header
    }

    pub fn file_count(&self) -> u32 {
        self.cached_header.file_count
    }

    /// Id tying this index to its sidecar (see `index::cache`).
    pub(crate) fn sidecar_id(&self) -> u64 {
        self.cached_header.sidecar_id
    }

    fn trigram_count(&self) -> usize {
        self.cached_header.trigram_count as usize
    }

    /// Trigram table entry `i` (`i < trigram_count`, checked at open).
    #[inline]
    fn entry_at(&self, i: usize) -> TrigramEntry {
        let offset = Header::SIZE + i * TrigramEntry::SIZE;
        read_trigram_entry(&self.mmap[offset..offset + TrigramEntry::SIZE])
    }

    /// Index of the first table entry whose trigram is `>= target`.
    fn lower_bound(&self, target: [u8; 3]) -> usize {
        let (mut lo, mut hi) = (0usize, self.trigram_count());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.entry_at(mid).trigram < target {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Posting bytes of a table entry (`None` if out of bounds = corrupt).
    fn entry_postings(&self, entry: &TrigramEntry) -> Option<&[u8]> {
        let start = self
            .posting_lists_start
            .checked_add(usize::try_from(entry.posting_offset).ok()?)?;
        let end = start.checked_add(entry.posting_len as usize)?;
        if end > self.file_table_start {
            return None;
        }
        Some(&self.mmap[start..end])
    }

    /// Raw posting bytes of an exact (already folded) trigram.
    ///
    /// The length of the returned slice is the planning cost estimate: it is
    /// known from the trigram table without decoding anything.
    pub(crate) fn posting_raw(&self, folded: [u8; 3]) -> Option<&[u8]> {
        let i = self.lower_bound(folded);
        if i >= self.trigram_count() {
            return None;
        }
        let entry = self.entry_at(i);
        if entry.trigram != folded {
            return None;
        }
        self.entry_postings(&entry)
    }

    /// File IDs containing trigram `target`, compared case-insensitively
    /// (ASCII): the index stores folded trigrams, so this is the superset of
    /// files containing `target` byte-for-byte.
    pub fn lookup_trigram(&self, target: [u8; 3]) -> Vec<u32> {
        self.posting_raw(fold_trigram(target))
            .map(Self::decode_posting_list)
            .unwrap_or_default()
    }

    /// Decode the file IDs of a posting list (masks are not decoded).
    pub fn decode_posting_list(data: &[u8]) -> Vec<u32> {
        Self::decode_ids(data).0
    }

    /// Decode IDs; returns them plus the byte offset just after the last ID.
    fn decode_ids(data: &[u8]) -> (Vec<u32>, usize) {
        if data.is_empty() {
            return (vec![], 0);
        }
        let (count, mut pos) = decode_varint(data);
        let count = count as usize;
        // Sanity check: count exceeding data length indicates corrupt data
        if count > data.len() {
            return (vec![], data.len());
        }
        let mut result = Vec::with_capacity(count);
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
        (result, pos)
    }

    /// Decode a posting list with its masks.
    pub(crate) fn decode_posting(data: &[u8]) -> Posting<'_> {
        let (ids, pos) = Self::decode_ids(data);
        let end = pos.saturating_add(ids.len() * 2).min(data.len());
        Posting {
            masks: &data[pos.min(end)..end],
            ids,
        }
    }

    /// Union of the posting lists of all trigrams starting with `prefix`
    /// (folded), i.e. every file containing the 2-byte string `prefix` in any
    /// letter case. Because each file is indexed with a trailing sentinel
    /// byte, even an occurrence at the very end of a file produces such a
    /// trigram, so a single contiguous range of the sorted table suffices.
    pub fn lookup_trigram_prefix(&self, prefix: [u8; 2]) -> Vec<u32> {
        let p = [fold(prefix[0]), fold(prefix[1])];
        let lo = self.lower_bound([p[0], p[1], 0]);
        let n = self.file_count() as usize;
        let mut bits = vec![0u64; n.div_ceil(64)];
        for i in lo..self.trigram_count() {
            let entry = self.entry_at(i);
            if entry.trigram[..2] != p {
                break;
            }
            let Some(data) = self.entry_postings(&entry) else {
                continue; // skip corrupted entry
            };
            for id in Self::decode_posting_list(data) {
                if let Some(w) = bits.get_mut(id as usize / 64) {
                    *w |= 1u64 << (id % 64);
                }
            }
        }
        let mut result = Vec::new();
        for (w, &word) in bits.iter().enumerate() {
            let mut x = word;
            while x != 0 {
                result.push((w as u32) * 64 + x.trailing_zeros());
                x &= x - 1;
            }
        }
        result
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

    #[allow(dead_code)]
    pub(crate) fn all_trigrams(&self) -> Vec<[u8; 3]> {
        (0..self.trigram_count())
            .map(|i| self.entry_at(i).trigram)
            .collect()
    }

    /// Returns all trigrams with their raw posting bytes in sorted order.
    /// Used for byte-copy optimization: unaffected posting lists can be
    /// copied directly without decode/re-encode.
    pub(crate) fn all_trigram_entries_raw(&self) -> Vec<([u8; 3], &[u8])> {
        let mut result = Vec::with_capacity(self.trigram_count());
        for i in 0..self.trigram_count() {
            let entry = self.entry_at(i);
            let Some(raw) = self.entry_postings(&entry) else {
                break;
            };
            result.push((entry.trigram, raw));
        }
        result
    }

    pub(crate) fn file_entry(&self, file_id: u32) -> Option<FileEntry> {
        if file_id >= self.cached_header.file_count {
            return None;
        }
        let offset = self.file_table_start + file_id as usize * FileEntry::SIZE;
        if offset + FileEntry::SIZE > self.mmap.len() {
            return None;
        }
        Some(read_file_entry(
            &self.mmap[offset..offset + FileEntry::SIZE],
        ))
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
        fs::write(&path, b"BADMxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx").unwrap();
        assert!(IndexReader::open(&path).is_err());
    }

    #[test]
    fn test_open_file_too_small() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tiny.xgrep");
        fs::write(&path, b"XGR").unwrap(); // only 3 bytes, need 32
        assert!(IndexReader::open(&path).is_err());
    }

    #[test]
    fn test_open_invalid_version() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("badver.xgrep");
        let mut data = vec![0u8; 32];
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
        let mut data = vec![0u8; 36]; // Header(32) + only 4 bytes
        data[0..4].copy_from_slice(b"XGRP");
        data[4..8].copy_from_slice(&VERSION.to_le_bytes());
        data[8..12].copy_from_slice(&9999u32.to_le_bytes()); // trigram_count = 9999 (huge)
        data[12..16].copy_from_slice(&0u32.to_le_bytes()); // file_count = 0
        data[16..24].copy_from_slice(&0u64.to_le_bytes()); // posting_total_bytes = 0
        data[24..32].copy_from_slice(&0u64.to_le_bytes()); // sidecar_id = 0
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

    #[test]
    fn test_old_versions_rejected_and_need_rebuild() {
        let (dir, index_path) = build_test_index();
        assert!(!needs_rebuild(&index_path));
        assert_eq!(index_version(&index_path), Some(VERSION));
        assert!(!needs_rebuild(&dir.path().join("missing.xgrep")));
        for old in [2u32, 3] {
            let mut data = fs::read(&index_path).unwrap();
            data[4..8].copy_from_slice(&old.to_le_bytes());
            let p = dir.path().join(format!("v{old}.xgrep"));
            fs::write(&p, &data).unwrap();
            assert!(IndexReader::open(&p).is_err());
            assert!(needs_rebuild(&p));
            assert_eq!(index_version(&p), Some(old));
        }
    }

    #[test]
    fn test_lookup_is_case_folded() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "HashMap").unwrap();
        fs::write(root.join("b.txt"), "hashmap").unwrap();
        fs::write(root.join("c.txt"), "unrelated").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        for t in [*b"Has", *b"has", *b"HAS"] {
            assert_eq!(reader.lookup_trigram(t).len(), 2, "{:?}", t);
        }
        // Only folded trigrams are stored.
        assert!(reader
            .all_trigrams()
            .iter()
            .all(|t| t.iter().all(|b| !b.is_ascii_uppercase())));
    }

    #[test]
    fn test_prefix_lookup_finds_pair_at_end_of_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("end.txt"), "xyzAB").unwrap(); // "ab" only at EOF
        fs::write(root.join("two.txt"), "ab").unwrap(); // 2-byte file
        fs::write(root.join("mid.txt"), "zabz").unwrap();
        fs::write(root.join("none.txt"), "a b").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        let mut got: Vec<&str> = reader
            .lookup_trigram_prefix(*b"ab")
            .into_iter()
            .map(|id| reader.file_path(id))
            .collect();
        got.sort();
        assert_eq!(got, vec!["end.txt", "mid.txt", "two.txt"]);
    }

    #[test]
    fn test_decode_posting_masks() {
        let mut buf = Vec::new();
        encode_posting(&mut buf, &[3, 7, 100], &[0x0102, 0x0304, 0xFF00]);
        let p = IndexReader::decode_posting(&buf);
        assert_eq!(p.ids, vec![3, 7, 100]);
        assert_eq!((p.loc(0), p.next(0)), (0x02, 0x01));
        assert_eq!((p.loc(1), p.next(1)), (0x04, 0x03));
        assert_eq!(p.packed(2), 0xFF00);
        // Truncated masks read as "all bits" (never drops a file).
        let p = IndexReader::decode_posting(&buf[..buf.len() - 2]);
        assert_eq!((p.loc(2), p.next(2)), (0xFF, 0xFF));
        assert_eq!(IndexReader::decode_posting_list(&buf), vec![3, 7, 100]);
    }
}
