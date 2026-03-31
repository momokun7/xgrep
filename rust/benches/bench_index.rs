use criterion::{criterion_group, criterion_main, Criterion};
use std::fs;
use std::path::Path;
use tempfile::TempDir;
use xgrep_search::{Config, Xgrep};

/// Generate a corpus of Rust-like source files for benchmarking.
fn create_corpus(dir: &Path, file_count: usize) {
    let src_dir = dir.join("src");
    fs::create_dir_all(&src_dir).unwrap();

    let snippets = [
        "use std::collections::HashMap;\n",
        "use std::io::{self, Read, Write};\n",
        "pub struct Config {\n    pub name: String,\n    pub value: u64,\n}\n",
        "impl Config {\n    pub fn new(name: &str) -> Self {\n        Self { name: name.to_string(), value: 0 }\n    }\n}\n",
        "pub fn process_data(input: &[u8]) -> Vec<u8> {\n    input.iter().map(|b| b ^ 0xff).collect()\n}\n",
        "fn helper_function(x: i32, y: i32) -> i32 {\n    x * y + 42\n}\n",
        "#[derive(Debug, Clone)]\npub enum Status {\n    Active,\n    Inactive,\n    Pending,\n}\n",
        "pub trait Searchable {\n    fn search(&self, query: &str) -> Vec<String>;\n}\n",
        "impl Searchable for Vec<String> {\n    fn search(&self, query: &str) -> Vec<String> {\n        self.iter().filter(|s| s.contains(query)).cloned().collect()\n    }\n}\n",
        "pub fn calculate_hash(data: &[u8]) -> u64 {\n    let mut hash: u64 = 5381;\n    for &b in data {\n        hash = hash.wrapping_mul(33).wrapping_add(b as u64);\n    }\n    hash\n}\n",
        "mod tests {\n    use super::*;\n    #[test]\n    fn test_process() {\n        let input = b\"hello\";\n        let output = process_data(input);\n        assert_eq!(output.len(), 5);\n    }\n}\n",
        "const MAX_BUFFER_SIZE: usize = 4096;\nstatic GLOBAL_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);\n",
    ];

    for i in 0..file_count {
        let mut content = format!("// File {}\n", i);
        for j in 0..8 {
            content.push_str(snippets[(i + j) % snippets.len()]);
            content.push('\n');
        }
        content.push_str(&format!("pub fn unique_fn_{}() -> usize {{ {} }}\n", i, i));
        let filename = format!("module_{:03}.rs", i);
        fs::write(src_dir.join(&filename), &content).unwrap();
    }
}

fn bench_index_build_100(c: &mut Criterion) {
    let tmp = TempDir::new().unwrap();
    create_corpus(tmp.path(), 100);

    c.bench_function("index_build_100_files", |b| {
        b.iter(|| {
            let xg = Xgrep::open_local(tmp.path())
                .unwrap()
                .with_config(Config { quiet: true });
            xg.build_index().unwrap();
        });
    });
}

fn bench_index_build_500(c: &mut Criterion) {
    let tmp = TempDir::new().unwrap();
    create_corpus(tmp.path(), 500);

    c.bench_function("index_build_500_files", |b| {
        b.iter(|| {
            let xg = Xgrep::open_local(tmp.path())
                .unwrap()
                .with_config(Config { quiet: true });
            xg.build_index().unwrap();
        });
    });
}

criterion_group!(benches, bench_index_build_100, bench_index_build_500,);
criterion_main!(benches);
