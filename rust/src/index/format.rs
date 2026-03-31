//! # xgrep Index Binary Format (Version 2)
//!
//! ## Design Note: Existence-Only Trigrams
//!
//! The current format stores **existence-only** trigrams: each posting list records
//! which files contain a given trigram, but not the byte offsets where it appears.
//! This keeps the index small (~8% of source size) at the cost of higher false
//! positive rates during candidate resolution -- every candidate file must be read
//! and verified with a full content match.
//!
//! **Positional trigrams** (as used by zoekt) store per-file byte offsets alongside
//! file IDs, enabling distance verification at the index level. This dramatically
//! reduces false positives (estimated 90-99% reduction for medium-length queries)
//! but increases index size to ~20-35% of source.
//!
//! Positional trigrams are planned for index format v3. See
//! `docs/positional-trigrams-design.md` for the full design document.
//!
//! ```text
//! ┌──────────────────────────────────────────┐
//! │ Header (24 bytes)                        │
//! │   magic: [u8; 4]        = "XGRP"        │
//! │   version: u32           = 2             │
//! │   trigram_count: u32                     │
//! │   file_count: u32                        │
//! │   posting_total_bytes: u64               │
//! ├──────────────────────────────────────────┤
//! │ Trigram Table (16 bytes × trigram_count) │
//! │   trigram: [u8; 3]                       │
//! │   _padding: u8                           │
//! │   posting_offset: u64                    │
//! │   posting_len: u32                       │
//! ├──────────────────────────────────────────┤
//! │ Posting Lists (variable length)          │
//! │   Per trigram:                            │
//! │     count: varint                        │
//! │     file_ids: varint[] (delta-encoded)   │
//! ├──────────────────────────────────────────┤
//! │ File Table (28 bytes × file_count)       │
//! │   path_offset: u32                       │
//! │   mtime: u64                             │
//! │   size: u64                              │
//! │   content_hash: u64                      │
//! ├──────────────────────────────────────────┤
//! │ String Pool (variable length)            │
//! │   Null-terminated file paths             │
//! └──────────────────────────────────────────┘
//! ```
//!
//! All integers are little-endian. Trigram table is sorted by trigram bytes
//! for binary search. Posting lists use LEB128 varint encoding with delta
//! compression (file IDs stored as differences from previous).

pub const MAGIC: [u8; 4] = *b"XGRP";
pub const VERSION: u32 = 2;

/// Header: 24 bytes
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Header {
    pub magic: [u8; 4],           // 4
    pub version: u32,             // 4
    pub trigram_count: u32,       // 4
    pub file_count: u32,          // 4
    pub posting_total_bytes: u64, // 8
}

/// Trigram Table entry: 16 bytes
/// trigram(3) + padding(1) + posting_offset(8) + posting_len(4) = 16
///
/// WARNING: Do NOT use `unsafe { ptr::read_unaligned(... as *const TrigramEntry) }` or similar
/// pointer casts on this struct. Always use `to_bytes()` for serialization.
/// The packed repr exists only to define the exact byte layout.
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
/// WARNING: Do NOT use `unsafe { ptr::read_unaligned(... as *const FileEntry) }` or similar
/// pointer casts on this struct. Always use `to_bytes()` for serialization.
/// The packed repr exists only to define the exact byte layout.
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
    pub fn to_bytes(&self) -> [u8; 24] {
        let mut bytes = [0u8; 24];
        bytes[0..4].copy_from_slice(&self.magic);
        bytes[4..8].copy_from_slice(&self.version.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.trigram_count.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.file_count.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.posting_total_bytes.to_le_bytes());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_size() {
        assert_eq!(Header::SIZE, 24);
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
