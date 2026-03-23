use std::fs;
use tempfile::tempdir;
use xgrep::{SearchOptions, Xgrep};

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
fn test_accessors() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    let xg = Xgrep::open(root).unwrap();
    assert_eq!(xg.root(), root);
    assert!(xg.index_path().to_string_lossy().contains("xgrep"));
}
