//! Regression tests for the search core (matching, smart-case, filters).

use std::fs;
use tempfile::tempdir;
use xgrep_search::{pattern_has_uppercase, Config, SearchOptions, Xgrep};

fn open(root: &std::path::Path) -> Xgrep {
    let xg = Xgrep::open_local(root)
        .unwrap()
        .with_config(Config { quiet: true });
    xg.build_index().unwrap();
    xg
}

fn smart_case(pattern: &str, regex: bool) -> SearchOptions {
    SearchOptions::new()
        .with_regex(regex)
        .with_case_insensitive(!pattern_has_uppercase(pattern, regex))
}

#[test]
fn smart_case_finds_non_ascii_literal() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("a.txt"), "first line\nÄbc here\n").unwrap();
    fs::write(root.join("b.txt"), "äbc lower\n").unwrap();
    let xg = open(root);

    // Non-ASCII uppercase keeps the search case-sensitive.
    let results = xg.search("Äbc", &smart_case("Äbc", false)).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].line_number, 2);

    // Forced ASCII-only case-insensitive search must still find the literal.
    let opts = SearchOptions::new().with_case_insensitive(true);
    let results = xg.search("Äbc", &opts).unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].file.contains("a.txt"));
    let results = xg.search("ÄBC", &opts).unwrap();
    assert_eq!(results.len(), 1);

    let results = xg.search_files(&["a.txt".into()], "Äbc", &opts).unwrap();
    assert_eq!(results.len(), 1);
}

#[test]
fn smart_case_detects_non_ascii_uppercase() {
    assert!(pattern_has_uppercase("Äbc", false));
    assert!(pattern_has_uppercase("Äbc", true));
    assert!(!pattern_has_uppercase("äbc", false));
    assert!(!pattern_has_uppercase(r"\Wäbc", true));
}

fn corpus(root: &std::path::Path) {
    fs::create_dir_all(root.join("src/auth")).unwrap();
    fs::create_dir_all(root.join("docs")).unwrap();
    for i in 0..120 {
        let body = format!("fn target_{i}() {{}}\n// target again\nother\n");
        fs::write(root.join(format!("src/m{i:03}.rs")), &body).unwrap();
        fs::write(root.join(format!("docs/n{i:03}.md")), &body).unwrap();
        if i % 10 == 0 {
            fs::write(root.join(format!("src/auth/a{i:03}.py")), &body).unwrap();
        }
    }
}

fn key(r: &xgrep_search::SearchResult) -> (String, usize) {
    (r.file.to_string(), r.line_number)
}

type PathPred = Box<dyn Fn(&str) -> bool>;

#[test]
fn filters_and_max_count_match_post_filter_semantics() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    corpus(root);
    let xg = open(root);

    let all = xg.search("target", &SearchOptions::new()).unwrap();
    assert_eq!(all.len(), 2 * (120 + 120 + 12));

    let cases: Vec<(SearchOptions, PathPred)> = vec![
        (
            SearchOptions::new().with_file_type("rs"),
            Box::new(|f: &str| f.ends_with(".rs")),
        ),
        (
            SearchOptions::new().with_path_pattern("src/auth"),
            Box::new(|f: &str| f.contains("src/auth")),
        ),
        (
            SearchOptions::new()
                .with_glob("*.md")
                .with_glob("!docs/n00*"),
            Box::new(|f: &str| f.ends_with(".md") && !f.starts_with("docs/n00")),
        ),
        (SearchOptions::new(), Box::new(|_: &str| true)),
    ];
    for (base, pred) in cases {
        let expected: Vec<_> = all.iter().filter(|r| pred(&r.file)).map(key).collect();
        for regex in [false, true] {
            for max in [None, Some(1), Some(7), Some(150), Some(10_000)] {
                let mut opts = base.clone().with_regex(regex);
                opts.max_count = max;
                let got: Vec<_> = xg
                    .search("target", &opts)
                    .unwrap()
                    .iter()
                    .map(key)
                    .collect();
                let want: Vec<_> = expected
                    .iter()
                    .take(max.unwrap_or(usize::MAX))
                    .cloned()
                    .collect();
                assert_eq!(got, want, "opts={opts:?}");
            }
        }
    }
}

#[test]
fn unknown_file_type_keeps_all_results() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    corpus(root);
    let xg = open(root);
    let all = xg.search("target", &SearchOptions::new()).unwrap();
    let unknown = xg
        .search("target", &SearchOptions::new().with_file_type("nope"))
        .unwrap();
    assert_eq!(all.len(), unknown.len());
}
