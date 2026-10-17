#![no_main]
use libfuzzer_sys::fuzz_target;
use xgrep_search::fuzz_exports::IndexReader;

fuzz_target!(|data: &[u8]| {
    // Decode arbitrary bytes: must not panic or loop forever.
    let result = IndexReader::decode_posting_list(data);
    // Decoded doc IDs never decrease.
    for window in result.windows(2) {
        assert!(window[0] <= window[1]);
    }
});
