use std::path::Path;
use std::process::{Command, Output};

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

/// Create a repo directory with the given files and build its index.
fn setup(files: &[(&str, &[u8])]) -> (tempfile::TempDir, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    for (name, content) in files {
        std::fs::write(dir.path().join(name), content).unwrap();
    }
    let init = xg(dir.path(), cache.path(), &["init"]);
    assert!(init.status.success(), "xg init failed: {:?}", init);
    (dir, cache)
}

fn stdout(o: &Output) -> String {
    String::from_utf8(o.stdout.clone()).expect("stdout must be UTF-8")
}

#[test]
fn test_context_and_llm_output_survive_non_utf8_file() {
    let (dir, cache) = setup(&[
        ("good.txt", b"before\nneedle here\nafter\n"),
        ("bad.txt", b"\xff\xfe\nneedle \xc3 bad\n\xfe\n"),
    ]);
    for args in [
        &["-C", "1", "needle"][..],
        &["--format", "llm", "needle"][..],
    ] {
        let o = xg(dir.path(), cache.path(), args);
        assert!(o.status.success(), "{:?}: {:?}", args, o);
        let s = stdout(&o);
        assert!(s.contains("needle here"), "{:?}: {}", args, s);
        assert!(s.contains("needle \u{fffd} bad"), "{:?}: {}", args, s);
    }
}

#[test]
fn test_llm_output_has_line_numbers_and_markers() {
    let (dir, cache) = setup(&[("a.rs", b"one\ntwo\nfn needle() {}\nfour\n")]);
    let o = xg(
        dir.path(),
        cache.path(),
        &["--format", "llm", "-C", "1", "needle"],
    );
    assert!(o.status.success(), "{:?}", o);
    assert_eq!(
        stdout(&o),
        "## a.rs:2-4\n\n```rust\n2  two\n3> fn needle() {}\n4  four\n```\n"
    );
}

#[test]
fn test_max_tokens_limits_llm_output() {
    let content: String = (1..=3000).map(|i| format!("needle {}\n", i)).collect();
    let (dir, cache) = setup(&[("big.txt", content.as_bytes())]);
    let o = xg(
        dir.path(),
        cache.path(),
        &["--format", "llm", "--max-tokens", "300", "needle"],
    );
    assert!(o.status.success(), "{:?}", o);
    let s = stdout(&o);
    assert!(s.len() <= 300 * 4, "len = {}", s.len());
    assert!(s.contains("... (truncated,"), "{}", s);

    // Without --max-tokens the CLI is unlimited.
    let o = xg(dir.path(), cache.path(), &["--format", "llm", "needle"]);
    assert!(!stdout(&o).contains("truncated"));
}

#[test]
fn test_max_tokens_requires_llm_format() {
    let (dir, cache) = setup(&[("a.txt", b"needle\n")]);
    let o = xg(dir.path(), cache.path(), &["--max-tokens", "100", "needle"]);
    assert_eq!(o.status.code(), Some(2), "{:?}", o);
}

#[test]
fn test_json_output_is_ndjson_with_summary() {
    let (dir, cache) = setup(&[
        ("a.txt", b"needle one\nnope\nneedle two\n"),
        ("b.txt", b"needle \"three\"\n"),
    ]);
    let o = xg(dir.path(), cache.path(), &["--json", "needle"]);
    assert!(o.status.success(), "{:?}", o);
    let lines: Vec<serde_json::Value> = stdout(&o)
        .lines()
        .map(|l| serde_json::from_str(l).expect("each line is one JSON object"))
        .collect();
    assert_eq!(lines.len(), 4);
    assert_eq!(lines[0]["type"], "match");
    assert_eq!(lines[0]["file"], "a.txt");
    assert_eq!(lines[1]["line_number"], 3);
    assert_eq!(lines[1]["line"], "needle two");
    assert_eq!(lines[2]["line"], "needle \"three\"");
    assert_eq!(lines[3]["type"], "summary");
    assert_eq!(lines[3]["total_matches"], 3);
    assert_eq!(lines[3]["total_files"], 2);
    assert_eq!(lines[3]["truncated"], false);

    let o = xg(
        dir.path(),
        cache.path(),
        &["--json", "--max-count", "2", "needle"],
    );
    let last: serde_json::Value = serde_json::from_str(stdout(&o).lines().last().unwrap()).unwrap();
    assert_eq!(last["total_matches"], 2);
    assert_eq!(last["truncated"], true);
}

#[test]
fn test_max_columns() {
    let long = format!("needle{}\n", "z".repeat(1000));
    let (dir, cache) = setup(&[("long.txt", long.as_bytes())]);

    // Plain output: off by default, enabled with --max-columns.
    let o = xg(dir.path(), cache.path(), &["needle"]);
    assert!(stdout(&o).len() > 1000);
    let o = xg(dir.path(), cache.path(), &["--max-columns", "10", "needle"]);
    assert_eq!(stdout(&o), "long.txt:1:needlezzzz…[+996 chars]\n");

    // LLM output: 300 characters by default, 0 disables the limit.
    let o = xg(dir.path(), cache.path(), &["--format", "llm", "needle"]);
    assert!(stdout(&o).contains("…[+706 chars]"), "{}", stdout(&o));
    let o = xg(
        dir.path(),
        cache.path(),
        &["--format", "llm", "--max-columns", "0", "needle"],
    );
    assert!(!stdout(&o).contains("chars]"));
}
