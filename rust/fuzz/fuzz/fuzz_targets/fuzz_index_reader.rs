#![no_main]
use libfuzzer_sys::fuzz_target;
use std::io::Write;

fuzz_target!(|data: &[u8]| {
    // 任意バイト列をインデックスファイルとして開く: パニックしないことを検証
    let dir = tempfile::tempdir().unwrap();
    let index_path = dir.path().join("index.xgrep");
    {
        let mut f = std::fs::File::create(&index_path).unwrap();
        f.write_all(data).unwrap();
    }

    // 不正データに対してはErrを返し、パニックしないこと
    let _ = xgrep::fuzz_exports::IndexReader::open(&index_path);
});
