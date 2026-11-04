//! CLI regressions: multiple PATH arguments, --exclude vs --max-count
//! ordering, and default-search freshness.

use std::path::Path;
use std::process::Command;

fn xg_bin(cache: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_xg"));
    // Never touch the user's real index cache.
    cmd.env("XGREP_CACHE_DIR", cache).env("XGREP_NO_HINTS", "1");
    cmd
}

/// Output lines with path separators normalized to `/` (Windows prints `\`).
fn stdout_lines(out: &std::process::Output) -> Vec<String> {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.replace('\\', "/"))
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

    // A single PATH prefixes its results too, as ripgrep does.
    let out = xg_bin(cache.path())
        .args(["multi_needle", "a"])
        .current_dir(root)
        .output()
        .unwrap();
    assert_eq!(stdout_lines(&out), vec!["a/x.txt:1:multi_needle in a"]);

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

/// Result paths are printed ripgrep style: the PATH as written joined with
/// the file's path inside it, so they can be opened from the current
/// directory. A leading `./` is dropped.
#[test]
fn test_result_paths_are_shown_as_written() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("a/sub")).unwrap();
    std::fs::create_dir_all(root.join("b")).unwrap();
    std::fs::write(root.join("a/sub/x.txt"), "pre\nshown_needle\npost\n").unwrap();
    std::fs::write(root.join("b/y.txt"), "shown_needle\n").unwrap();

    let run = |cwd: &Path, args: &[&str]| {
        let out = xg_bin(cache.path())
            .arg("shown_needle")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        stdout_lines(&out)
    };
    let x = "a/sub/x.txt:2:shown_needle";
    assert_eq!(run(root, &["a"]), vec![x]);
    assert_eq!(run(root, &["./a"]), vec![x]);
    assert_eq!(run(root, &["a/"]), vec![x]);
    assert_eq!(run(root, &["a/sub/x.txt"]), vec![x]);
    assert_eq!(run(root, &["./a/sub/x.txt"]), vec![x]);
    // `.` prints the same as no PATH (no `./` prefix).
    assert_eq!(run(root, &["."]), run(root, &[]));
    assert!(run(root, &["."]).contains(&x.to_string()));
    // Paths above the current directory stay as written.
    assert_eq!(
        run(&root.join("a/sub"), &["../../b"]),
        vec!["../../b/y.txt:1:shown_needle"]
    );
    assert_eq!(
        run(&root.join("a"), &["sub", "../b"]),
        vec!["sub/x.txt:2:shown_needle", "../b/y.txt:1:shown_needle"]
    );

    // Every output format uses the same paths, and context is still read
    // from the right file.
    assert_eq!(run(root, &["-l", "a"]), vec!["a/sub/x.txt"]);
    assert_eq!(run(root, &["-c", "a"]), vec!["a/sub/x.txt:1"]);
    assert_eq!(
        run(root, &["-C1", "a"]),
        vec![
            "a/sub/x.txt-1-pre",
            "a/sub/x.txt:2:shown_needle",
            "a/sub/x.txt-3-post"
        ]
    );
    // Parse the JSON: `stdout_lines` would turn its escaped `\\` into `//`.
    let out = xg_bin(cache.path())
        .args(["shown_needle", "--json", "a"])
        .current_dir(root)
        .output()
        .unwrap();
    let first = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .unwrap();
    assert_eq!(
        first["file"].as_str().map(|f| f.replace('\\', "/")),
        Some("a/sub/x.txt".to_string()),
        "{first}"
    );
    let llm = run(root, &["--format", "llm", "a"]);
    assert!(
        llm.iter().any(|l| l.starts_with("## a/sub/x.txt:")),
        "{llm:?}"
    );
    assert!(llm.iter().any(|l| l.contains("pre")), "{llm:?}");

    // --exclude matches the printed path.
    let out = xg_bin(cache.path())
        .args(["shown_needle", "a", "--exclude", "a/sub"])
        .current_dir(root)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    // Separators are kept as typed, like ripgrep (only a leading `./` goes).
    #[cfg(unix)]
    assert_eq!(run(root, &["a//sub"]), vec!["a//sub/x.txt:2:shown_needle"]);

    // --absolute-paths is unchanged.
    let abs = run(root, &["--absolute-paths", "a"]);
    let canonical = std::fs::canonicalize(root).unwrap();
    let want = canonical
        .join("a/sub/x.txt")
        .to_string_lossy()
        .replace('\\', "/");
    assert_eq!(abs, vec![format!("{want}:2:shown_needle")]);
}

#[test]
fn test_find_paths_are_shown_as_written() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("a/sub")).unwrap();
    std::fs::write(root.join("a/sub/found_me.txt"), "x\n").unwrap();
    let out = xg_bin(cache.path())
        .args(["--find", "found_me", "./a"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(stdout_lines(&out), vec!["a/sub/found_me.txt"]);

    // --exclude matches the printed path here too.
    let out = xg_bin(cache.path())
        .args(["--find", "found_me", "a", "--exclude", "a/sub"])
        .current_dir(root)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
}
