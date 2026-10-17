use std::io::{Read, Write};
use std::process::{Command, Output, Stdio};

fn xg_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_xg"))
}

/// Run xg in `dir` with `input` piped to standard input.
fn run_with_stdin(dir: &std::path::Path, args: &[&str], input: &str) -> Output {
    let cache = tempfile::tempdir().unwrap();
    let mut child = xg_bin()
        .args(args)
        .current_dir(dir)
        .env("XDG_CACHE_HOME", cache.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run xg");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

const INPUT: &str = "apple pie\nbanana\napricot jam\n";

#[test]
fn piped_stdin_is_searched_when_no_path_is_given() {
    let dir = tempfile::tempdir().unwrap();
    // A file in the directory must not be searched instead of the input.
    std::fs::write(dir.path().join("a.txt"), "apricot in a file\n").unwrap();

    let out = run_with_stdin(dir.path(), &["apricot"], INPUT);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "<stdin>:3:apricot jam\n");

    let out = run_with_stdin(dir.path(), &["-c", "ap"], INPUT);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "<stdin>:2\n");

    let out = run_with_stdin(dir.path(), &["-C", "1", "banana"], INPUT);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "<stdin>-1-apple pie\n<stdin>:2:banana\n<stdin>-3-apricot jam\n"
    );

    let out = run_with_stdin(dir.path(), &["zzz_absent"], INPUT);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn dash_reads_stdin_and_cannot_be_mixed_with_paths() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_with_stdin(dir.path(), &["jam", "-"], INPUT);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "<stdin>:3:apricot jam\n");

    let out = run_with_stdin(dir.path(), &["jam", "-", "."], INPUT);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("standard input"));
}

#[test]
fn null_stdin_searches_the_directory() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "needle_qzx\n").unwrap();
    let out = xg_bin()
        .args(["-l", "needle_qzx"])
        .current_dir(dir.path())
        .env("XDG_CACHE_HOME", cache.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "a.txt\n", "{out:?}");
}

#[test]
fn closed_stdout_pipe_exits_quietly() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let line = "needle_qzx line\n".repeat(20_000);
    std::fs::write(dir.path().join("a.txt"), &line).unwrap();
    let mut child = xg_bin()
        .args(["needle_qzx"])
        .current_dir(dir.path())
        .env("XDG_CACHE_HOME", cache.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Read a little, then close the pipe like `| head -1`.
    let mut stdout = child.stdout.take().unwrap();
    let mut buf = [0u8; 16];
    stdout.read_exact(&mut buf).unwrap();
    drop(stdout);
    let out = child.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("error"), "{err}");
    assert!(!err.contains("panicked"), "{err}");
    // Unix stops through SIGPIPE (no exit code); elsewhere xg exits with 0.
    if let Some(code) = out.status.code() {
        assert_eq!(code, 0, "{err}");
    }
}
