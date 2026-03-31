use std::process::Command;

fn xg_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_xg"))
}

#[test]
fn test_find_glob_rs_files() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("main.rs"), "fn main() {}").unwrap();
    std::fs::write(src.join("lib.rs"), "pub fn hello() {}").unwrap();
    std::fs::write(src.join("util.py"), "def hello(): pass").unwrap();

    let init = xg_bin()
        .args(["init", "--local"])
        .current_dir(dir.path())
        .output()
        .expect("failed to run xg init");
    assert!(init.status.success());

    let output = xg_bin()
        .args(["--find", "*.rs"])
        .current_dir(dir.path())
        .output()
        .expect("failed to run xg --find");
    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines.len(), 2, "expected 2 .rs files, got: {:?}", lines);
    assert!(lines.iter().all(|l| l.ends_with(".rs")));
}

#[test]
fn test_find_substring() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), "key = 1").unwrap();
    std::fs::write(dir.path().join("app_config.json"), "{}").unwrap();
    std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();

    let init = xg_bin()
        .args(["init", "--local"])
        .current_dir(dir.path())
        .output()
        .expect("failed to run xg init");
    assert!(init.status.success());

    let output = xg_bin()
        .args(["--find", "config"])
        .current_dir(dir.path())
        .output()
        .expect("failed to run xg --find");
    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines.len(), 2, "expected 2 config files, got: {:?}", lines);
    assert!(lines.iter().all(|l| l.contains("config")));
}

#[test]
fn test_find_no_match_exits_1() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.rs"), "fn hello() {}").unwrap();

    let init = xg_bin()
        .args(["init", "--local"])
        .current_dir(dir.path())
        .output()
        .expect("failed to run xg init");
    assert!(init.status.success());

    let output = xg_bin()
        .args(["--find", "*.py"])
        .current_dir(dir.path())
        .output()
        .expect("failed to run xg --find");
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn test_find_with_max_count() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..10 {
        std::fs::write(dir.path().join(format!("file_{}.rs", i)), "content").unwrap();
    }

    let init = xg_bin()
        .args(["init", "--local"])
        .current_dir(dir.path())
        .output()
        .expect("failed to run xg init");
    assert!(init.status.success());

    let output = xg_bin()
        .args(["--find", "*.rs", "--max-count", "3"])
        .current_dir(dir.path())
        .output()
        .expect("failed to run xg --find");
    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(
        lines.len(),
        3,
        "expected 3 files with --max-count, got: {:?}",
        lines
    );
}

#[test]
fn test_find_with_json_output() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
    std::fs::write(dir.path().join("lib.rs"), "pub fn lib() {}").unwrap();

    let init = xg_bin()
        .args(["init", "--local"])
        .current_dir(dir.path())
        .output()
        .expect("failed to run xg init");
    assert!(init.status.success());

    let output = xg_bin()
        .args(["--find", "*.rs", "--json"])
        .current_dir(dir.path())
        .output()
        .expect("failed to run xg --find --json");
    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("invalid JSON");
    let arr = parsed.as_array().expect("expected JSON array");
    assert_eq!(arr.len(), 2);
}

#[test]
fn test_find_with_count() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..5 {
        std::fs::write(dir.path().join(format!("f{}.rs", i)), "content").unwrap();
    }

    let init = xg_bin()
        .args(["init", "--local"])
        .current_dir(dir.path())
        .output()
        .expect("failed to run xg init");
    assert!(init.status.success());

    let output = xg_bin()
        .args(["--find", "*.rs", "-c"])
        .current_dir(dir.path())
        .output()
        .expect("failed to run xg --find -c");
    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "5");
}

#[test]
fn test_find_with_absolute_paths() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();

    let init = xg_bin()
        .args(["init", "--local"])
        .current_dir(dir.path())
        .output()
        .expect("failed to run xg init");
    assert!(init.status.success());

    let output = xg_bin()
        .args(["--find", "*.rs", "--absolute-paths"])
        .current_dir(dir.path())
        .output()
        .expect("failed to run xg --find --absolute-paths");
    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.trim();
    assert!(
        std::path::Path::new(line).is_absolute(),
        "expected absolute path, got: {}",
        line
    );
    assert!(line.ends_with("main.rs"));
}

#[test]
fn test_find_with_path_argument() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.rs"), "fn hello() {}").unwrap();
    std::fs::write(dir.path().join("world.py"), "def world(): pass").unwrap();

    let init = xg_bin()
        .args(["init", "--local"])
        .current_dir(dir.path())
        .output()
        .expect("failed to run xg init");
    assert!(init.status.success());

    // Use PATH argument instead of cwd
    let output = xg_bin()
        .args(["--find", "*.rs", &dir.path().to_string_lossy()])
        .output()
        .expect("failed to run xg --find with path");
    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].ends_with(".rs"));
}

#[test]
fn test_find_with_type_filter() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
    std::fs::write(dir.path().join("lib.py"), "def lib(): pass").unwrap();
    std::fs::write(dir.path().join("config.toml"), "key = 1").unwrap();

    let init = xg_bin()
        .args(["init", "--local"])
        .current_dir(dir.path())
        .output()
        .expect("failed to run xg init");
    assert!(init.status.success());

    // --find "*" -t rs should only return .rs files
    let output = xg_bin()
        .args(["--find", "*", "-t", "rs"])
        .current_dir(dir.path())
        .output()
        .expect("failed to run xg --find with -t");
    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines.len(), 1, "expected 1 .rs file, got: {:?}", lines);
    assert!(lines[0].ends_with(".rs"));
}

#[test]
fn test_find_with_exclude() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    let vendor = dir.path().join("vendor");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&vendor).unwrap();
    std::fs::write(src.join("main.rs"), "fn main() {}").unwrap();
    std::fs::write(vendor.join("dep.rs"), "fn dep() {}").unwrap();

    let init = xg_bin()
        .args(["init", "--local"])
        .current_dir(dir.path())
        .output()
        .expect("failed to run xg init");
    assert!(init.status.success());

    let output = xg_bin()
        .args(["--find", "*.rs", "--exclude", "vendor"])
        .current_dir(dir.path())
        .output()
        .expect("failed to run xg --find --exclude");
    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "expected 1 file after exclude, got: {:?}",
        lines
    );
    assert!(lines[0].contains("src"));
    assert!(!lines[0].contains("vendor"));
}

#[test]
fn test_find_exclude_empty_string_no_effect() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();

    let init = xg_bin()
        .args(["init", "--local"])
        .current_dir(dir.path())
        .output()
        .expect("failed to run xg init");
    assert!(init.status.success());

    // --exclude "" should NOT filter anything
    let output = xg_bin()
        .args(["--find", "*.rs", "--exclude", ""])
        .current_dir(dir.path())
        .output()
        .expect("failed to run xg --find --exclude ''");
    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "empty --exclude should not filter, got: {:?}",
        lines
    );
}

#[test]
fn test_search_with_exclude() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    let tests = dir.path().join("tests");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&tests).unwrap();
    std::fs::write(src.join("main.rs"), "fn hello() {}").unwrap();
    std::fs::write(tests.join("test.rs"), "fn hello() {}").unwrap();

    let init = xg_bin()
        .args(["init", "--local"])
        .current_dir(dir.path())
        .output()
        .expect("failed to run xg init");
    assert!(init.status.success());

    let output = xg_bin()
        .args(["hello", "--exclude", "tests"])
        .current_dir(dir.path())
        .output()
        .expect("failed to run xg with --exclude");
    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("src/main.rs") || stdout.contains("src\\main.rs"),
        "expected src/main.rs in output, got: {}",
        stdout
    );
    assert!(
        !stdout.contains("tests/") && !stdout.contains("tests\\"),
        "expected tests/ to be excluded, got: {}",
        stdout
    );
}

#[test]
fn test_status_with_index() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.rs"), "fn hello() {}").unwrap();

    let init = xg_bin()
        .args(["init", "--local"])
        .current_dir(dir.path())
        .output()
        .expect("failed to run xg init");
    assert!(init.status.success());

    let output = xg_bin()
        .args(["status"])
        .current_dir(dir.path())
        .output()
        .expect("failed to run xg status");
    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Indexed files:"));
    assert!(stdout.contains("Index size:"));
    assert!(stdout.contains("Last built:"));
}

#[test]
fn test_status_without_index() {
    let dir = tempfile::tempdir().unwrap();

    let output = xg_bin()
        .args(["status"])
        .current_dir(dir.path())
        .output()
        .expect("failed to run xg status");
    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("No index found"));
}
