use std::process::Command;

fn xg_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_xg"))
}

#[test]
fn test_oversize_files_in_status_and_zero_result_hint() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello world").unwrap();
    std::fs::write(dir.path().join("big.txt"), "needle_qzx ".repeat(50)).unwrap();
    let run = |args: &[&str]| {
        xg_bin()
            .args(args)
            .current_dir(dir.path())
            .env("XDG_CACHE_HOME", cache.path())
            .env("XGREP_MAX_FILESIZE", "100")
            .env_remove("XGREP_NO_HINTS")
            .output()
            .expect("failed to run xg")
    };
    assert!(run(&["init"]).status.success());

    let status = run(&["status"]);
    let out = String::from_utf8_lossy(&status.stdout);
    assert!(
        out.contains("Not indexed (over 100B): 1 file, e.g. big.txt"),
        "{out}"
    );

    let miss = run(&["needle_qzx"]);
    assert_eq!(miss.status.code(), Some(1));
    let err = String::from_utf8_lossy(&miss.stderr);
    assert!(
        err.contains("hint: 1 file over 100B is not indexed (e.g. big.txt)")
            && err.contains("XGREP_MAX_FILESIZE=0"),
        "{err}"
    );
    // --no-hints silences it.
    let quiet = run(&["--no-hints", "needle_qzx"]);
    assert!(quiet.stderr.is_empty(), "{:?}", quiet);

    // The suggested direct file search finds it.
    let direct = run(&["needle_qzx", "big.txt"]);
    assert_eq!(direct.status.code(), Some(0), "{:?}", direct);
}

#[test]
fn test_no_pattern_exits_with_2() {
    // Running xg without a pattern should exit with code 2 (usage error)
    let output = xg_bin().output().expect("failed to run xg");
    assert_eq!(
        output.status.code(),
        Some(2),
        "expected exit code 2 for missing pattern, got {:?}",
        output.status.code()
    );
}

#[test]
fn test_no_match_exits_with_1() {
    // Searching for a pattern that doesn't match should exit with code 1
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello world").unwrap();

    // First, init the index
    let init = xg_bin()
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("failed to run xg init");
    assert!(init.status.success(), "xg init failed: {:?}", init);

    // Search for non-existent pattern
    let output = xg_bin()
        .arg("nonexistent_pattern_xyz")
        .current_dir(dir.path())
        .output()
        .expect("failed to run xg");
    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit code 1 for no match, got {:?}",
        output.status.code()
    );
}

#[test]
fn test_match_exits_with_0() {
    // Searching for a pattern that matches should exit with code 0
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello world").unwrap();

    let init = xg_bin()
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("failed to run xg init");
    assert!(init.status.success(), "xg init failed: {:?}", init);

    let output = xg_bin()
        .arg("hello")
        .current_dir(dir.path())
        .output()
        .expect("failed to run xg");
    assert_eq!(
        output.status.code(),
        Some(0),
        "expected exit code 0 for match, got {:?}",
        output.status.code()
    );
}

#[test]
fn test_mutually_exclusive_flags_exits_with_2() {
    let output = xg_bin()
        .args(["-c", "-l", "pattern"])
        .output()
        .expect("failed to run xg");
    assert_eq!(
        output.status.code(),
        Some(2),
        "expected exit code 2 for mutually exclusive flags, got {:?}",
        output.status.code()
    );
}

#[test]
fn test_json_with_format_exits_with_2() {
    let output = xg_bin()
        .args(["--json", "--format", "llm", "pattern"])
        .output()
        .expect("failed to run xg");
    assert_eq!(
        output.status.code(),
        Some(2),
        "expected exit code 2 for --json with --format, got {:?}",
        output.status.code()
    );
}
