mod common;

use criterion::{criterion_group, criterion_main, Criterion};
use tempfile::TempDir;
use xgrep_search::{Config, SearchOptions, Xgrep};

/// Set up a corpus with index built.
fn setup_corpus(file_count: usize) -> (TempDir, Xgrep) {
    let tmp = TempDir::new().unwrap();
    common::create_corpus(tmp.path(), file_count);
    let xg = Xgrep::open_local(tmp.path())
        .unwrap()
        .with_config(Config { quiet: true });
    xg.build_index().unwrap();
    (tmp, xg)
}

fn bench_literal_search(c: &mut Criterion) {
    let (_tmp, xg) = setup_corpus(100);

    c.bench_function("search_literal", |b| {
        b.iter(|| {
            let results = xg
                .search("process_data", &SearchOptions::default())
                .unwrap();
            criterion::black_box(results);
        });
    });
}

fn bench_regex_search(c: &mut Criterion) {
    let (_tmp, xg) = setup_corpus(100);

    let opts = SearchOptions {
        regex: true,
        ..Default::default()
    };

    c.bench_function("search_regex", |b| {
        b.iter(|| {
            let results = xg.search(r"fn\s+\w+_\d+", &opts).unwrap();
            criterion::black_box(results);
        });
    });
}

fn bench_case_insensitive_search(c: &mut Criterion) {
    let (_tmp, xg) = setup_corpus(100);

    let opts = SearchOptions {
        case_insensitive: true,
        ..Default::default()
    };

    c.bench_function("search_case_insensitive", |b| {
        b.iter(|| {
            let results = xg.search("Config", &opts).unwrap();
            criterion::black_box(results);
        });
    });
}

criterion_group!(
    benches,
    bench_literal_search,
    bench_regex_search,
    bench_case_insensitive_search,
);
criterion_main!(benches);
