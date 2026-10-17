#![no_main]
use libfuzzer_sys::fuzz_target;
use xgrep_search::fuzz_exports::{decode_varint, encode_varint};

fuzz_target!(|data: &[u8]| {
    // Decode arbitrary bytes: must not panic.
    let (value, bytes_read) = decode_varint(data);
    assert!(bytes_read <= data.len());

    // A valid decode round-trips through encode.
    if bytes_read > 0 && bytes_read <= 5 {
        let mut encoded = Vec::new();
        encode_varint(&mut encoded, value);
        let (decoded, _) = decode_varint(&encoded);
        assert_eq!(decoded, value);
    }
});
