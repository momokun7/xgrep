mod common;

use criterion::{criterion_group, criterion_main, Criterion};
use tempfile::TempDir;
use xgrep_search::{Config, Xgrep};

fn bench_index_build_100(c: &mut Criterion) {
    let tmp = TempDir::new().unwrap();
    common::create_corpus(tmp.path(), 100);

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
    common::create_corpus(tmp.path(), 500);

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
