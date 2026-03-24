pub const MAGIC: [u8; 4] = *b"XGRP";
pub const VERSION: u32 = 1;

/// Header: 16 bytes
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Header {
    pub magic: [u8; 4],     // 4
    pub version: u32,       // 4
    pub trigram_count: u32, // 4
    pub file_count: u32,    // 4
}

/// Trigram Table entry: 16 bytes
/// trigram(3) + padding(1) + posting_offset(8) + posting_len(4) = 16
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

    pub fn to_bytes(&self) -> [u8; 16] {
        let mut bytes = [0u8; 16];
        bytes[0..4].copy_from_slice(&self.magic);
        bytes[4..8].copy_from_slice(&self.version.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.trigram_count.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.file_count.to_le_bytes());
        bytes
    }
}

impl TrigramEntry {
    pub const SIZE: usize = std::mem::size_of::<Self>();

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
            // Overflow: u32は最大5バイト (5*7=35bit)。これ以上はmalformed
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
        assert_eq!(Header::SIZE, 16);
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
        // 全バイトにcontinuation bitが設定されている不正データ
        let data = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        let (_, bytes_read) = decode_varint(&data);
        // 5バイト目(shift=35)でoverflow検出して6バイト目を返す
        assert!(bytes_read > 0);
        assert!(bytes_read <= 6);
    }

    #[test]
    fn test_decode_varint_exactly_5_continuation_bytes() {
        // 5バイト全てcontinuation bit付き: shift=35でoverflow
        let data = [0x80, 0x80, 0x80, 0x80, 0x80];
        let (_, bytes_read) = decode_varint(&data);
        assert!(bytes_read > 0);
    }
}
