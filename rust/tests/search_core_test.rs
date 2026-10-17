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
