//! Streaming search, boolean (AND / NOT) queries and match columns,
//! through the library API and the `xg` CLI.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use xgrep_search::{OutputMode, SearchOptions, Xgrep};

/// Run `xg` in `dir` with its index cache kept inside `cache`.
fn xg(dir: &Path, cache: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xg"))
        .args(args)
        .current_dir(dir)
        .env("XDG_CACHE_HOME", cache)
        .env("XGREP_NO_HINTS", "1")
        .output()
        .expect("failed to run xg")
}

fn stdout(o: &Output) -> String {
    String::from_utf8(o.stdout.clone()).expect("stdout must be UTF-8")
}

/// A corpus with enough files to take the parallel scan path.
fn corpus() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("tests")).unwrap();
    for i in 0..120 {
        let mut body = format!("// file {i}\nfn handler_{i}() {{}}\n");
        if i % 2 == 0 {
            body.push_str("let db = connect();\n");
        }
        if i % 3 == 0 {
            body.push_str("mock_handler();\n");
        }
        body.push_str(&format!("handler_{i}();\n"));
        fs::write(root.join(format!("src/f{i:03}.rs")), body).unwrap();
    }
    for i in 0..10 {
        fs::write(
            root.join(format!("tests/t{i}.rs")),
            format!("fn handler_test_{i}() {{}}\nlet db = connect();\n"),
        )
        .unwrap();
    }
    dir
}

#[test]
fn test_search_stream_modes_agree_with_search() {
    let dir = corpus();
    let xg = Xgrep::open_local(dir.path()).unwrap();
    xg.build_index().unwrap();
    let opts = SearchOptions::new();
    let all = xg.search("handler", &opts).unwrap();
    assert!(all.len() > 200);

    let mut streamed = Vec::new();
    let mut files = Vec::new();
    xg.search_stream("handler", &opts, OutputMode::Lines, |fm| {
        assert_eq!(fm.count, fm.lines.len());
        assert!(fm.lines.iter().all(|l| l.file == fm.file));
        files.push(fm.file.to_string());
        streamed.extend(fm.lines);
        true
    })
    .unwrap();
    let key = |r: &xgrep_search::SearchResult| (r.file.to_string(), r.line_number);
    assert_eq!(
        streamed.iter().map(key).collect::<Vec<_>>(),
        all.iter().map(key).collect::<Vec<_>>()
    );
    let mut sorted = files.clone();
    sorted.sort();
    assert_eq!(files, sorted, "files arrive in path order");

    let mut counts = Vec::new();
    xg.search_stream("handler", &opts, OutputMode::Count, |fm| {
        assert!(fm.lines.is_empty());
        counts.push((fm.file.to_string(), fm.count));
        true
    })
    .unwrap();
    assert_eq!(counts.len(), files.len());
    assert_eq!(counts.iter().map(|c| c.1).sum::<usize>(), all.len());

    let mut only = Vec::new();
    xg.search_stream("handler", &opts, OutputMode::FilesOnly, |fm| {
        only.push(fm.file.to_string());
        true
    })
    .unwrap();
    assert_eq!(only, files);

    // The sink can stop the search early.
    let mut seen = 0;
    xg.search_stream("handler", &opts, OutputMode::Lines, |_| {
        seen += 1;
        seen < 3
    })
    .unwrap();
    assert_eq!(seen, 3);

    // max_count is a global line limit, also in streaming modes.
    let limited = SearchOptions::new().with_max_count(5);
    let mut total = 0;
    xg.search_stream("handler", &limited, OutputMode::Count, |fm| {
        total += fm.count;
        true
    })
    .unwrap();
    assert_eq!(total, 5);
}

#[test]
fn test_and_not_patterns_library() {
    let dir = corpus();
    let xg = Xgrep::open_local(dir.path()).unwrap();
    xg.build_index().unwrap();

    let files = |opts: &SearchOptions| {
        let mut v: Vec<String> = xg
            .search("handler", opts)
            .unwrap()
            .into_iter()
            .map(|r| r.file.to_string())
            .collect();
        v.dedup();
        v
    };
    let with_db = files(&SearchOptions::new().with_and("connect()"));
    assert_eq!(with_db.len(), 60 + 10);
    let no_tests = files(
        &SearchOptions::new()
            .with_and("connect()")
            .with_not("handler_test"),
    );
    assert_eq!(no_tests.len(), 60);
    assert!(no_tests.iter().all(|f| f.starts_with("src/")));
    let no_mock = files(
        &SearchOptions::new()
            .with_regex(true)
            .with_and(r"let \w+ = connect")
            .with_not(r"mock_\w+\("),
    );
    // Even files without a mock call (i % 6 != 0 among the evens) + tests.
    assert_eq!(no_mock.len(), 40 + 10);
}

#[test]
fn test_result_columns() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("a.txt"),
        "xx Needle\n  needle()\nfoo needle_x\n",
    )
    .unwrap();
    let xg = Xgrep::open_local(dir.path()).unwrap();
    xg.build_index().unwrap();
    let cols = |opts: SearchOptions| -> Vec<(usize, usize)> {
        xg.search("needle", &opts)
            .unwrap()
            .iter()
            .map(|r| (r.line_number, r.column))
            .collect()
    };
    assert_eq!(cols(SearchOptions::new()), vec![(2, 2), (3, 4)]);
    assert_eq!(
        cols(SearchOptions::new().with_case_insensitive(true)),
        vec![(1, 3), (2, 2), (3, 4)]
    );
    // Word mode reports the column of the word, not the boundary character.
    assert_eq!(cols(SearchOptions::new().with_word(true)), vec![(2, 2)]);
}

#[test]
fn test_cli_count_files_only_and_max_count() {
    let dir = corpus();
    let cache = tempfile::tempdir().unwrap();
    let (root, cache) = (dir.path(), cache.path());
    let lines = stdout(&xg(root, cache, &["-s", "handler"]));
    let mut expected_counts: Vec<(String, usize)> = Vec::new();
    for l in lines.lines() {
        let file = l.split(':').next().unwrap().to_string();
        match expected_counts.last_mut() {
            Some((f, n)) if *f == file => *n += 1,
            _ => expected_counts.push((file, 1)),
        }
    }
    let counts = stdout(&xg(root, cache, &["-s", "-c", "handler"]));
    let want: String = expected_counts
        .iter()
        .map(|(f, n)| format!("{f}:{n}\n"))
        .collect();
    assert_eq!(counts, want);
    let files = stdout(&xg(root, cache, &["-s", "-l", "handler"]));
    let want: String = expected_counts
        .iter()
        .map(|(f, _)| format!("{f}\n"))
        .collect();
    assert_eq!(files, want);

    // --max-count is a global line limit for -c and -l as well.
    let c = stdout(&xg(
        root,
        cache,
        &["-s", "-c", "--max-count", "5", "handler"],
    ));
    let total: usize = c
        .lines()
        .map(|l| l.rsplit(':').next().unwrap().parse::<usize>().unwrap())
        .sum();
    assert_eq!(total, 5);
    let l = stdout(&xg(
        root,
        cache,
        &["-s", "-l", "--max-count", "5", "handler"],
    ));
    assert_eq!(l.lines().count(), c.lines().count());
}

#[test]
fn test_cli_and_not_flags() {
    let dir = corpus();
    let cache = tempfile::tempdir().unwrap();
    let o = xg(
        dir.path(),
        cache.path(),
        &[
            "-l",
            "handler",
            "--and",
            "connect()",
            "--not",
            "handler_test",
        ],
    );
    assert!(o.status.success(), "{:?}", o);
    let files = stdout(&o);
    assert_eq!(files.lines().count(), 60);
    assert!(files.lines().all(|f| f.starts_with("src/")));
    // Nothing matches -> exit 1.
    let o = xg(
        dir.path(),
        cache.path(),
        &["handler", "--and", "zzz_not_there"],
    );
    assert_eq!(o.status.code(), Some(1));
}

#[test]
fn test_cli_json_column_and_window_truncation() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let minified = format!("{}findme{}\n", "x".repeat(2000), "y".repeat(2000));
    fs::write(dir.path().join("app.min.js"), &minified).unwrap();
    let o = xg(dir.path(), cache.path(), &["--json", "findme"]);
    let first: serde_json::Value =
        serde_json::from_str(stdout(&o).lines().next().unwrap()).unwrap();
    assert_eq!(first["column"], 2001);
    let o = xg(dir.path(), cache.path(), &["-M", "40", "findme"]);
    let s = stdout(&o);
    assert!(s.contains("findme"), "{}", s);
    assert!(s.starts_with("app.min.js:1:[+1990 chars]…"), "{}", s);
}
