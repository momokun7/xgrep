//! # xgrep Index Binary Format (Version 4)
//!
//! An index consists of two files:
//!
//! * the **main index** (`index`), memory-mapped by every search. It holds only
//!   what a search needs: the trigram table, the posting lists and the file
//!   table / path string pool.
//! * the **sidecar** (`index.cache`, see `index::cache`), read only by diff
//!   updates. It stores each file's sorted folded trigram list (in file ID
//!   order), so an update can find the posting lists a changed or deleted
//!   file appears in. (Up to v3 this information lived in a "per-file
//!   section" at the end of the main index *and* in a separate trigram cache:
//!   stored twice, and 75 % of the mmap'd index.)
//!
//! The main index header carries a `sidecar_id` (a hash of the indexed file
//! list) that is also written into the sidecar; a diff update only trusts a
//! sidecar with the same id and entry count, otherwise it rebuilds fully.
//!
//! ## Trigrams
//!
//! Trigrams are **ASCII case-folded** (see `trigram::fold`): case-sensitive
//! searches look up the folded trigrams of the pattern and verify candidates
//! by matching, and case-insensitive searches need no case-variant expansion.
//! Each file is indexed as if a NUL sentinel byte followed its last byte, so
//! every occurrence of a 2-byte string `ab` yields a trigram with prefix `ab`
//! (2-byte searches are a prefix range of the sorted trigram table).
//!
//! ## Masks (per trigram and file)
//!
//! Every posting carries two bytes (after Cursor's local regex index):
//!
//! * `loc`: bit `p % 8` set for every start offset `p` of the trigram.
//! * `next`: 8-bit bloom filter of the folded bytes following the trigram
//!   (bit `trigram::next_bit(b)`).
//!
//! For a literal, a file can only match at start offset `s` if for every
//! query trigram at query offset `o`, bit `(s + o) % 8` is set in that
//! trigram's `loc` mask and the following query byte is in its `next` mask.
//! Both checks only ever reject files that cannot contain the literal (masks
//! are a superset of the true positions/followers), so they never cause
//! false negatives.
//!
//! ```text
//! ┌──────────────────────────────────────────┐
//! │ Header (32 bytes)                        │
//! │   magic: [u8; 4]        = "XGRP"        │
//! │   version: u32           = 4             │
//! │   trigram_count: u32                     │
//! │   file_count: u32                        │
//! │   posting_total_bytes: u64               │
//! │   sidecar_id: u64                        │
//! ├──────────────────────────────────────────┤
//! │ Trigram Table (16 bytes × trigram_count) │
//! │   trigram: [u8; 3]   (folded, sorted)    │
//! │   _padding: u8                           │
//! │   posting_offset: u64                    │
//! │   posting_len: u32   (bytes)             │
//! ├──────────────────────────────────────────┤
//! │ Posting Lists (variable length)          │
//! │   Per trigram:                           │
//! │     count: varint                        │
//! │     file_ids: varint[count] (delta)      │
//! │     masks: [loc: u8, next: u8][count]    │
//! ├──────────────────────────────────────────┤
//! │ File Table (28 bytes × file_count)       │
//! │   path_offset: u32                       │
//! │   mtime: u64  (ns)                       │
//! │   size: u64                              │
//! │   content_hash: u64                      │
//! ├──────────────────────────────────────────┤
//! │ String Pool (variable length)            │
//! │   Null-terminated file paths             │
//! └──────────────────────────────────────────┘
//! ```
//!
//! All integers are little-endian. The trigram table is sorted by trigram
//! bytes for binary search; `posting_len` (known without decoding) is used as
//! the cost estimate for query planning. Posting lists use LEB128 varint
//! encoding with delta compression (file IDs stored as differences from the
//! previous one); the masks follow the IDs so ID-only decoding can stop early.
//!
//! Older indexes (v2/v3) are rejected by the reader and rebuilt from scratch.

pub const MAGIC: [u8; 4] = *b"XGRP";
pub const VERSION: u32 = 4;
/// Oldest index version that is recognised (and migrated by a rebuild).
pub const VERSION_V2: u32 = 2;

/// Header: 32 bytes
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Header {
    pub magic: [u8; 4],           // 4
    pub version: u32,             // 4
    pub trigram_count: u32,       // 4
    pub file_count: u32,          // 4
    pub posting_total_bytes: u64, // 8
    pub sidecar_id: u64,          // 8
}

/// Trigram Table entry: 16 bytes
/// trigram(3) + padding(1) + posting_offset(8) + posting_len(4) = 16
///
/// `packed` is required here: without it `repr(C)` inserts 4 bytes of alignment
/// padding before `posting_offset`, making `SIZE` 24 instead of 16.
/// All access goes through `to_bytes()` — never cast a pointer to this struct.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct TrigramEntry {
    pub trigram: [u8; 3],
    pub _padding: u8,
    pub posting_offset: u64,
    pub posting_len: u32,
}

/// File Table entry: 28 bytes
/// path_offset(4) + mtime(8) + size(8) + content_hash(8) = 28
///
/// `packed` is required here: without it `repr(C)` inserts 4 bytes of alignment
/// padding after `path_offset`, making `SIZE` 32 instead of 28.
/// All access goes through `to_bytes()` — never cast a pointer to this struct.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct FileEntry {
    pub path_offset: u32,
    pub mtime: u64,
    pub size: u64,
    pub content_hash: u64,
}

impl Header {
    pub const SIZE: usize = std::mem::size_of::<Self>();

    #[allow(clippy::wrong_self_convention)]
    pub fn to_bytes(&self) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        bytes[0..4].copy_from_slice(&self.magic);
        bytes[4..8].copy_from_slice(&self.version.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.trigram_count.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.file_count.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.posting_total_bytes.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.sidecar_id.to_le_bytes());
        bytes
    }
}

impl TrigramEntry {
    pub const SIZE: usize = std::mem::size_of::<Self>();

    #[allow(clippy::wrong_self_convention)]
    pub fn to_bytes(&self) -> [u8; 16] {
        let mut bytes = [0u8; 16];
        bytes[0..3].copy_from_slice(&self.trigram);
        bytes[3] = self._padding;
        bytes[4..12].copy_from_slice(&self.posting_offset.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.posting_len.to_le_bytes());
        bytes
    }
}

impl FileEntry {
    pub const SIZE: usize = std::mem::size_of::<Self>();

    #[allow(clippy::wrong_self_convention)]
    pub fn to_bytes(&self) -> [u8; 28] {
        let mut bytes = [0u8; 28];
        bytes[0..4].copy_from_slice(&self.path_offset.to_le_bytes());
        bytes[4..12].copy_from_slice(&self.mtime.to_le_bytes());
        bytes[12..20].copy_from_slice(&self.size.to_le_bytes());
        bytes[20..28].copy_from_slice(&self.content_hash.to_le_bytes());
        bytes
    }
}

/// Convert trigram [u8;3] to u32
pub fn trigram_to_u32(t: [u8; 3]) -> u32 {
    (t[0] as u32) << 16 | (t[1] as u32) << 8 | t[2] as u32
}

pub fn u32_to_trigram(v: u32) -> [u8; 3] {
    [(v >> 16) as u8, (v >> 8) as u8, v as u8]
}

#[cfg(test)]
/// Convert Vec<[u8;3]> to a sorted unique u32 list
pub fn trigrams_to_sorted_u32(trigrams: &[[u8; 3]]) -> Vec<u32> {
    let mut v: Vec<u32> = trigrams.iter().map(|&t| trigram_to_u32(t)).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Encode u32 as LEB128 varint
pub fn encode_varint(buf: &mut Vec<u8>, mut value: u32) {
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if value == 0 {
            break;
        }
    }
}

/// Decode LEB128 varint, returns (value, bytes_consumed)
pub fn decode_varint(data: &[u8]) -> (u32, usize) {
    let mut result: u32 = 0;
    let mut shift: u32 = 0;
    for (i, &byte) in data.iter().enumerate() {
        if shift >= 35 {
            // Overflow: u32 requires at most 5 bytes (5*7=35bit). Beyond this is malformed
            return (result, i + 1);
        }
        if shift == 28 && (byte & 0x70) != 0 {
            // Varint overflow: byte 5 has bits that don't fit in u32
            return (result, i + 1);
        }
        result |= ((byte & 0x7F) as u32) << shift;
        if byte & 0x80 == 0 {
            return (result, i + 1);
        }
        shift += 7;
    }
    (result, data.len())
}

/// Pack a posting's masks into the u16 form used in memory (`loc | next << 8`).
#[inline]
pub fn pack_masks(loc: u8, next: u8) -> u16 {
    loc as u16 | (next as u16) << 8
}

/// Encode one posting list: `count`, delta-encoded IDs, then `[loc, next]`
/// per entry. `ids` must be sorted ascending; `masks[i]` belongs to `ids[i]`.
pub fn encode_posting(buf: &mut Vec<u8>, ids: &[u32], masks: &[u16]) {
    debug_assert_eq!(ids.len(), masks.len());
    encode_varint(buf, ids.len() as u32);
    let mut prev = 0u32;
    for &fid in ids {
        encode_varint(buf, fid - prev);
        prev = fid;
    }
    buf.reserve(masks.len() * 2);
    for &m in masks {
        buf.extend_from_slice(&m.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_size() {
        assert_eq!(Header::SIZE, 32);
    }

    #[test]
    fn test_trigram_entry_size() {
        assert_eq!(TrigramEntry::SIZE, 16);
    }

    #[test]
    fn test_file_entry_size() {
        assert_eq!(FileEntry::SIZE, 28);
    }

    #[test]
    fn test_varint_roundtrip() {
        let values = [0u32, 1, 127, 128, 300, 16383, 16384, 100_000, u32::MAX];
        for &v in &values {
            let mut buf = Vec::new();
            encode_varint(&mut buf, v);
            let (decoded, bytes_read) = decode_varint(&buf);
            assert_eq!(decoded, v, "roundtrip failed for {}", v);
            assert_eq!(bytes_read, buf.len());
        }
    }

    #[test]
    fn test_varint_sizes() {
        let mut buf = Vec::new();
        encode_varint(&mut buf, 0);
        assert_eq!(buf.len(), 1);

        buf.clear();
        encode_varint(&mut buf, 127);
        assert_eq!(buf.len(), 1);

        buf.clear();
        encode_varint(&mut buf, 128);
        assert_eq!(buf.len(), 2);

        buf.clear();
        encode_varint(&mut buf, 16383);
        assert_eq!(buf.len(), 2);

        buf.clear();
        encode_varint(&mut buf, 16384);
        assert_eq!(buf.len(), 3);
    }

    #[test]
    fn test_varint_zero() {
        let mut buf = Vec::new();
        encode_varint(&mut buf, 0);
        assert_eq!(buf, vec![0]);
        let (val, bytes) = decode_varint(&buf);
        assert_eq!(val, 0);
        assert_eq!(bytes, 1);
    }

    #[test]
    fn test_varint_max_u32() {
        let mut buf = Vec::new();
        encode_varint(&mut buf, u32::MAX);
        let (val, bytes) = decode_varint(&buf);
        assert_eq!(val, u32::MAX);
        assert_eq!(bytes, buf.len());
    }

    #[test]
    fn test_varint_boundary_127() {
        let mut buf = Vec::new();
        encode_varint(&mut buf, 127);
        assert_eq!(buf.len(), 1); // fits in 1 byte
        let (val, _) = decode_varint(&buf);
        assert_eq!(val, 127);
    }

    #[test]
    fn test_varint_boundary_128() {
        let mut buf = Vec::new();
        encode_varint(&mut buf, 128);
        assert_eq!(buf.len(), 2); // needs 2 bytes
        let (val, _) = decode_varint(&buf);
        assert_eq!(val, 128);
    }

    #[test]
    fn test_decode_varint_empty() {
        let (val, bytes) = decode_varint(&[]);
        assert_eq!(val, 0);
        assert_eq!(bytes, 0);
    }

    #[test]
    fn test_decode_varint_overflow_all_continuation_bits() {
        // Invalid data: all bytes have continuation bit set
        let data = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        let (_, bytes_read) = decode_varint(&data);
        // Overflow detected at byte 5 (shift=35), returns up to byte 6
        assert!(bytes_read > 0);
        assert!(bytes_read <= 6);
    }

    #[test]
    fn test_decode_varint_overflow_byte4() {
        // Byte 4 with upper bits set (overflow for u32)
        let data = [0x80, 0x80, 0x80, 0x80, 0xFF]; // 5th byte has all bits set
        let (_val, bytes_read) = decode_varint(&data);
        // Should not produce a value with the overflowed bits
        assert!(bytes_read <= 5);
        // Value should be capped/truncated rather than wrapping
    }

    #[test]
    fn test_decode_varint_exactly_5_continuation_bytes() {
        // All 5 bytes have continuation bit set: overflow at shift=35
        let data = [0x80, 0x80, 0x80, 0x80, 0x80];
        let (_, bytes_read) = decode_varint(&data);
        assert!(bytes_read > 0);
    }

    #[test]
    fn test_trigram_to_u32_roundtrip() {
        let t = *b"hel";
        let v = trigram_to_u32(t);
        assert_eq!(u32_to_trigram(v), t);
    }

    #[test]
    fn test_trigrams_to_sorted_u32_dedup() {
        let trigrams = vec![*b"abc", *b"xyz", *b"abc"];
        let sorted = trigrams_to_sorted_u32(&trigrams);
        assert_eq!(sorted.len(), 2);
        assert!(sorted[0] < sorted[1]);
    }
}

#[cfg(test)]
mod proptest_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn varint_roundtrip(value in 0u32..=u32::MAX) {
            let mut buf = Vec::new();
            encode_varint(&mut buf, value);
            let (decoded, bytes_read) = decode_varint(&buf);
            prop_assert_eq!(decoded, value);
            prop_assert!(bytes_read > 0);
            prop_assert!(bytes_read <= 5);
            prop_assert_eq!(bytes_read, buf.len());
        }

        #[test]
        fn varint_encoding_is_compact(value in 0u32..=u32::MAX) {
            let mut buf = Vec::new();
            encode_varint(&mut buf, value);
            prop_assert!(buf.len() <= 5);
            if value < 128 { prop_assert_eq!(buf.len(), 1); }
            if value < 16384 { prop_assert!(buf.len() <= 2); }
        }
    }
}
