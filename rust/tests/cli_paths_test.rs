//! CLI regressions: multiple PATH arguments, --exclude vs --max-count
//! ordering, and default-search freshness.

use std::path::Path;
use std::process::Command;

fn xg_bin(cache: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_xg"));
    // Never touch the user's real index cache.
    cmd.env("XDG_CACHE_HOME", cache).env("XGREP_NO_HINTS", "1");
    cmd
}

fn stdout_lines(out: &std::process::Output) -> Vec<String> {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn test_multiple_paths_are_searched_and_prefixed() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("a")).unwrap();
    std::fs::create_dir_all(root.join("b/sub")).unwrap();
    std::fs::write(root.join("a/x.txt"), "multi_needle in a\n").unwrap();
    std::fs::write(root.join("b/sub/y.txt"), "multi_needle in b\n").unwrap();
    std::fs::write(root.join("top.txt"), "multi_needle top\n").unwrap();

    let out = xg_bin(cache.path())
        .args(["multi_needle", "a", "b/sub/y.txt"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        stdout_lines(&out),
        vec![
            "a/x.txt:1:multi_needle in a".to_string(),
            "b/sub/y.txt:1:multi_needle in b".to_string(),
        ]
    );

    // A single PATH keeps paths relative to that PATH (unchanged behaviour).
    let out = xg_bin(cache.path())
        .args(["multi_needle", "a"])
        .current_dir(root)
        .output()
        .unwrap();
    assert_eq!(stdout_lines(&out), vec!["x.txt:1:multi_needle in a"]);

    // Subcommands still parse.
    let out = xg_bin(cache.path())
        .args(["status", "a"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
}

#[test]
fn test_exclude_applies_before_max_count() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a_skip.txt"), "maxneedle 1\nmaxneedle 2\n").unwrap();
    std::fs::write(root.join("b_keep.txt"), "maxneedle 3\nmaxneedle 4\n").unwrap();

    let out = xg_bin(cache.path())
        .args(["maxneedle", "--exclude", "a_skip", "--max-count", "1"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(stdout_lines(&out), vec!["b_keep.txt:1:maxneedle 3"]);
}

#[test]
fn test_default_search_sees_new_file_without_reindex() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("old.txt"), "nothing here\n").unwrap();
    let init = xg_bin(cache.path())
        .arg("init")
        .current_dir(root)
        .output()
        .unwrap();
    assert!(init.status.success(), "{init:?}");

    std::fs::write(root.join("new.txt"), "fresh_needle_cli\n").unwrap();
    let out = xg_bin(cache.path())
        .arg("fresh_needle_cli")
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(stdout_lines(&out), vec!["new.txt:1:fresh_needle_cli"]);
}

#[test]
fn test_word_regexp_with_non_word_edge() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("c.txt"), "call foo( x)\nxfoo( y\n").unwrap();
    let out = xg_bin(cache.path())
        .args(["-w", "foo("])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(stdout_lines(&out), vec!["c.txt:1:call foo( x)"]);
}
