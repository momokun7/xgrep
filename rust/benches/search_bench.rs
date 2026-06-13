/// Criterion benchmarks for xgrep's core search hotpaths.
///
/// Three cases are measured:
///   (a) literal search       – the common fast path through the trigram index
///   (b) regex search         – same pipeline but with the regex engine for verification
///   (c) find_files (--find)  – file-name lookup against the index
///
/// Index build is performed once in the setup phase (outside the iter loop)
/// so that only the search / find step is measured.
use criterion::{criterion_group, criterion_main, Criterion};
use std::fs;
use tempfile::TempDir;
use xgrep_search::{Config, SearchOptions, Xgrep};

// ---------------------------------------------------------------------------
// Corpus helpers
// ---------------------------------------------------------------------------

/// Create a deterministic corpus of synthetic Rust-like source files.
/// Each file gets a unique function and a mix of recurring patterns so that
/// both high-selectivity (rare needle) and low-selectivity (common keyword)
/// searches can be exercised.
fn build_corpus(file_count: usize) -> (TempDir, Xgrep) {
    let tmp = TempDir::new().expect("tempdir");
    let src = tmp.path().join("src");
    fs::create_dir_all(&src).expect("create src dir");

    let snippets = [
        "use std::collections::HashMap;\n",
        "use std::sync::{Arc, Mutex};\n",
        "pub struct Config { pub name: String, pub value: u64 }\n",
        "impl Config { pub fn new(name: &str) -> Self { Self { name: name.to_string(), value: 0 } } }\n",
        "pub fn process_data(input: &[u8]) -> Vec<u8> { input.iter().map(|b| b ^ 0xff).collect() }\n",
        "fn helper(x: i32, y: i32) -> i32 { x * y + 42 }\n",
        "#[derive(Debug, Clone)]\npub enum Status { Active, Inactive, Pending }\n",
        "pub trait Searchable { fn search(&self, query: &str) -> Vec<String>; }\n",
        "pub fn calculate_hash(data: &[u8]) -> u64 { data.iter().fold(5381u64, |h, &b| h.wrapping_mul(33).wrapping_add(b as u64)) }\n",
        "const MAX_SIZE: usize = 4096;\n",
    ];

    for i in 0..file_count {
        let mut content = format!("// module {}\n", i);
        for j in 0..8 {
            content.push_str(snippets[(i + j) % snippets.len()]);
        }
        // Unique function per file — used for exact-match search benchmarks.
        content.push_str(&format!(
            "pub fn bench_target_fn_{}() -> usize {{ {} }}\n",
            i, i
        ));
        fs::write(src.join(format!("module_{:03}.rs", i)), &content).expect("write file");
    }

    // A few non-Rust files so that --find has something diverse to search.
    fs::write(tmp.path().join("README.md"), "# xgrep bench\n").expect("write readme");
    fs::write(tmp.path().join("build.sh"), "#!/bin/sh\ncargo build\n").expect("write build.sh");

    let xg = Xgrep::open_local(tmp.path())
        .expect("open_local")
        .with_config(Config { quiet: true });
    xg.build_index().expect("build_index");

    (tmp, xg)
}

// ---------------------------------------------------------------------------
// (a) Literal search
// ---------------------------------------------------------------------------

fn bench_literal_search(c: &mut Criterion) {
    // Setup: build index once outside the iter loop.
    let (_tmp, xg) = build_corpus(200);

    c.bench_function("search/literal", |b| {
        b.iter(|| {
            let results = xg
                .search("process_data", &SearchOptions::default())
                .expect("search");
            criterion::black_box(results);
        });
    });
}

// ---------------------------------------------------------------------------
// (b) Regex search
// ---------------------------------------------------------------------------

fn bench_regex_search(c: &mut Criterion) {
    let (_tmp, xg) = build_corpus(200);

    let opts = SearchOptions::new().with_regex(true);

    c.bench_function("search/regex", |b| {
        b.iter(|| {
            // Pattern: function definitions with a numeric suffix (e.g. fn bench_target_fn_0)
            let results = xg
                .search(r"pub fn \w+_\d+\(\)", &opts)
                .expect("regex search");
            criterion::black_box(results);
        });
    });
}

// ---------------------------------------------------------------------------
// (c) find_files (--find equivalent)
// ---------------------------------------------------------------------------

fn bench_find_files(c: &mut Criterion) {
    let (_tmp, xg) = build_corpus(200);

    c.bench_function("find/glob_rs", |b| {
        b.iter(|| {
            let files = xg.find_files("*.rs").expect("find_files");
            criterion::black_box(files);
        });
    });
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_literal_search,
    bench_regex_search,
    bench_find_files
);
criterion_main!(benches);
