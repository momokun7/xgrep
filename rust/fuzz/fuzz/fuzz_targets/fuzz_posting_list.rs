#![no_main]
use libfuzzer_sys::fuzz_target;
use xgrep_search::fuzz_exports::IndexReader;

fuzz_target!(|data: &[u8]| {
    // 任意バイト列のデコード: パニック・無限ループしないことを検証
    let result = IndexReader::decode_posting_list(data);
    // デコード結果は単調増加であること
    for window in result.windows(2) {
        assert!(window[0] <= window[1]);
    }
});
