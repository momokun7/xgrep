#![no_main]
use libfuzzer_sys::fuzz_target;
use std::io::Write;

fuzz_target!(|data: &[u8]| {
    // Open arbitrary bytes as an index file: must not panic.
    let dir = tempfile::tempdir().unwrap();
    let index_path = dir.path().join("index.xgrep");
    {
        let mut f = std::fs::File::create(&index_path).unwrap();
        f.write_all(data).unwrap();
    }

    // Invalid data returns Err instead of panicking.
    let _ = xgrep_search::fuzz_exports::IndexReader::open(&index_path);
});
