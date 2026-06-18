use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use tempfile::TempDir;
use xgrep_search::{Config, SearchOptions, Xgrep};

/// Generate a corpus where only a fraction of files contain the target pattern.
/// This lets us measure search performance with varying selectivity, which
/// reflects the index's candidate resolution effectiveness.
fn create_corpus_with_needle(dir: &Path, file_count: usize, needle_every_n: usize) {
    let src_dir = dir.join("src");
    fs::create_dir_all(&src_dir).unwrap();

    let filler = concat!(
        "use std::collections::HashMap;\n",
        "pub struct Widget {\n    pub id: u64,\n    pub label: String,\n}\n",
        "impl Widget {\n    pub fn new(id: u64) -> Self {\n",
        "        Self { id, label: String::new() }\n    }\n}\n",
        "pub fn transform(data: &[u8]) -> Vec<u8> {\n",
        "    data.iter().rev().cloned().collect()\n}\n",
        "fn internal_helper(x: f64) -> f64 { x * 2.0 + 1.0 }\n",
    );

    let needle = "pub fn xgrep_unique_target_pattern() -> bool { true }\n";

    for i in 0..file_count {
        let mut content = format!("// Generated file {}\n", i);
        content.push_str(filler);
        content.push_str(&format!(
            "pub fn generated_fn_{}() -> usize {{ {} }}\n",
            i, i
        ));
        if i % needle_every_n == 0 {
            content.push_str(needle);
        }
        let filename = format!("gen_{:04}.rs", i);
        fs::write(src_dir.join(&filename), &content).unwrap();
    }
}

fn bench_candidate_resolution(c: &mut Criterion) {
    // 200 files, needle in every 10th file -> 20 files should match
    let tmp = TempDir::new().unwrap();
    create_corpus_with_needle(tmp.path(), 200, 10);
    let xg = Xgrep::open_local(tmp.path())
        .unwrap()
        .with_config(Config { quiet: true });
    xg.build_index().unwrap();

    c.bench_function("candidates_vs_matches", |b| {
        b.iter(|| {
            let results = xg
                .search("xgrep_unique_target_pattern", &SearchOptions::default())
                .unwrap();
            let match_files: HashSet<&str> = results.iter().map(|r| r.file.as_str()).collect();
            criterion::black_box(match_files.len());
        });
    });

    // Print false positive stats once outside the benchmark loop
    let results = xg
        .search("xgrep_unique_target_pattern", &SearchOptions::default())
        .unwrap();
    let match_files: HashSet<&str> = results.iter().map(|r| r.file.as_str()).collect();
    let total_files = 200;
    let expected_matches = total_files / 10;
    eprintln!(
        "[bench_candidates] total_files={}, expected_matches={}, actual_matches={}",
        total_files,
        expected_matches,
        match_files.len(),
    );
}

/// Distributed pattern: needle appears in *every* file (worst-case for index filtering).
/// Simulates patterns like `devm_kzalloc` that appear across the entire codebase.
/// Reports candidate count and search time so we can see index_phase vs scan_phase ratio.
fn create_distributed_corpus(dir: &Path, file_count: usize) {
    let src_dir = dir.join("src");
    fs::create_dir_all(&src_dir).unwrap();

    // ubiquitous token present in every file (simulates widely-used kernel symbol)
    let ubiquitous = "pub fn xgrep_distributed_needle(x: usize) -> usize { x }\n";
    // also include a rare token present in 1/100 files
    let rare = "pub fn xgrep_rare_needle_unique() -> bool { true }\n";

    for i in 0..file_count {
        let mut content = format!("// Distributed corpus file {}\n", i);
        content.push_str("use std::fmt;\npub struct Item {{ id: usize }}\n");
        content.push_str(ubiquitous);
        content.push_str(&format!(
            "pub fn generated_fn_{}() -> usize {{ {} }}\n",
            i, i
        ));
        if i % 100 == 0 {
            content.push_str(rare);
        }
        let filename = format!("dist_{:04}.rs", i);
        fs::write(src_dir.join(&filename), &content).unwrap();
    }
}

fn bench_focused_vs_distributed(c: &mut Criterion) {
    let file_counts = [500usize, 1000];
    let mut group = c.benchmark_group("candidate_selectivity");

    for &n in &file_counts {
        // --- focused: rare needle, appears in 1/100 files ---
        {
            let tmp = TempDir::new().unwrap();
            create_distributed_corpus(tmp.path(), n);
            let xg = Xgrep::open_local(tmp.path())
                .unwrap()
                .with_config(Config { quiet: true });
            xg.build_index().unwrap();

            let expected_matches = n / 100;
            group.bench_with_input(BenchmarkId::new("focused_1pct", n), &n, |b, _| {
                b.iter(|| {
                    let results = xg
                        .search("xgrep_rare_needle_unique", &SearchOptions::default())
                        .unwrap();
                    criterion::black_box(results.len())
                });
            });
            // Print match stats once outside the bench loop
            let results = xg
                .search("xgrep_rare_needle_unique", &SearchOptions::default())
                .unwrap();
            eprintln!(
                "[focused_1pct n={}] expected≈{}, got={}",
                n,
                expected_matches,
                results.len()
            );
        }

        // --- distributed: ubiquitous needle, appears in every file ---
        {
            let tmp = TempDir::new().unwrap();
            create_distributed_corpus(tmp.path(), n);
            let xg = Xgrep::open_local(tmp.path())
                .unwrap()
                .with_config(Config { quiet: true });
            xg.build_index().unwrap();

            group.bench_with_input(BenchmarkId::new("distributed_100pct", n), &n, |b, _| {
                b.iter(|| {
                    let results = xg
                        .search("xgrep_distributed_needle", &SearchOptions::default())
                        .unwrap();
                    criterion::black_box(results.len())
                });
            });
            let results = xg
                .search("xgrep_distributed_needle", &SearchOptions::default())
                .unwrap();
            eprintln!("[distributed_100pct n={}] got={}", n, results.len());
        }
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_candidate_resolution,
    bench_focused_vs_distributed
);
criterion_main!(benches);
