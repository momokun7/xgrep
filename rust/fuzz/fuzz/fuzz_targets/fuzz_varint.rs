#![no_main]
use libfuzzer_sys::fuzz_target;
use xgrep_search::fuzz_exports::{decode_varint, encode_varint};

fuzz_target!(|data: &[u8]| {
    // 任意バイト列のデコード: パニックしないことを検証
    let (value, bytes_read) = decode_varint(data);
    assert!(bytes_read <= data.len());

    // 有効なデコード結果のラウンドトリップ検証
    if bytes_read > 0 && bytes_read <= 5 {
        let mut encoded = Vec::new();
        encode_varint(&mut encoded, value);
        let (decoded, _) = decode_varint(&encoded);
        assert_eq!(decoded, value);
    }
});
