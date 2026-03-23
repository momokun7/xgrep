use std::fs;
use tempfile::tempdir;
use xgrep::{self, SearchOptions, Xgrep};

#[test]
fn test_open_and_search_basic() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("a.rs"), "fn handle_auth() {}\nfn other() {}").unwrap();

    let xg = Xgrep::open(root).unwrap();
    xg.build_index().unwrap();

    let results = xg.search("handle_auth", &SearchOptions::default()).unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].file.contains("a.rs"));
    assert_eq!(results[0].line_number, 1);
}

#[test]
fn test_open_local() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("a.rs"), "fn hello() {}").unwrap();

    let xg = Xgrep::open_local(root).unwrap();
    xg.build_index().unwrap();

    let results = xg.search("hello", &SearchOptions::default()).unwrap();
    assert_eq!(results.len(), 1);

    // .xgrep/index にインデックスが作られていること
    assert!(root.join(".xgrep").join("index").exists());
}

#[test]
fn test_search_auto_builds_index() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("a.rs"), "fn hello() {}").unwrap();

    let xg = Xgrep::open(root).unwrap();
    // build_index()を呼ばずにsearch → 自動ビルドされる
    let results = xg.search("hello", &SearchOptions::default()).unwrap();
    assert_eq!(results.len(), 1);
}

#[test]
fn test_search_regex_via_api() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("a.rs"), "fn handle_auth() {}\nfn handle_user() {}").unwrap();

    let xg = Xgrep::open(root).unwrap();
    xg.build_index().unwrap();

    let opts = SearchOptions { regex: true, ..Default::default() };
    let results = xg.search("handle_\\w+", &opts).unwrap();
    assert_eq!(results.len(), 2);
}

#[test]
fn test_search_case_insensitive_via_api() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("a.rs"), "fn HandleAuth() {}").unwrap();

    let xg = Xgrep::open(root).unwrap();
    xg.build_index().unwrap();

    let opts = SearchOptions { case_insensitive: true, ..Default::default() };
    let results = xg.search("handleauth", &opts).unwrap();
    assert_eq!(results.len(), 1);
}

#[test]
fn test_search_file_type_filter() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("a.rs"), "fn hello() {}").unwrap();
    fs::write(root.join("b.py"), "def hello(): pass").unwrap();

    let xg = Xgrep::open(root).unwrap();
    xg.build_index().unwrap();

    let opts = SearchOptions { file_type: Some("rs".to_string()), ..Default::default() };
    let results = xg.search("hello", &opts).unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].file.contains(".rs"));
}

#[test]
fn test_search_max_count() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("a.rs"), "hello\nhello\nhello\nhello\nhello").unwrap();

    let xg = Xgrep::open(root).unwrap();
    xg.build_index().unwrap();

    let opts = SearchOptions { max_count: Some(2), ..Default::default() };
    let results = xg.search("hello", &opts).unwrap();
    assert_eq!(results.len(), 2);
}

#[test]
fn test_search_changed_requires_git() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("a.rs"), "fn hello() {}").unwrap();

    let xg = Xgrep::open(root).unwrap();
    xg.build_index().unwrap();

    let opts = SearchOptions { changed_only: true, ..Default::default() };
    let result = xg.search("hello", &opts);
    assert!(result.is_err());
}

#[test]
fn test_search_result_debug_clone() {
    let r = xgrep::SearchResult {
        file: "a.rs".to_string(),
        line_number: 1,
        line: "hello".to_string(),
    };
    let _ = format!("{:?}", r);
    let _cloned = r.clone();
}

#[test]
fn test_accessors() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    let xg = Xgrep::open(root).unwrap();
    assert_eq!(xg.root(), root);
    assert!(xg.index_path().to_string_lossy().contains("xgrep"));
}
