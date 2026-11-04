//! XGREP_CACHE_DIR chooses where indexes live (issue #136), and an
//! unwritable index location is reported with its path.

use std::process::Command;

fn xg() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_xg"));
    cmd.env("XGREP_NO_HINTS", "1");
    cmd
}

fn files_under(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(files_under(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}

#[test]
fn xgrep_cache_dir_holds_the_index() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "cache_dir_needle\n").unwrap();

    let out = xg()
        .args(["cache_dir_needle", "."])
        .current_dir(dir.path())
        .env("XGREP_CACHE_DIR", cache.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("cache_dir_needle"));

    let files = files_under(cache.path());
    assert!(
        files
            .iter()
            .any(|p| p.file_name().is_some_and(|n| n == "index")),
        "no index under {}: {files:?}",
        cache.path().display()
    );
    // Used as is: <XGREP_CACHE_DIR>/<hash>/index, no extra `xgrep` level.
    assert!(!cache.path().join("xgrep").exists());
}

#[test]
fn relative_xgrep_cache_dir_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "needle\n").unwrap();

    let out = xg()
        .args(["needle", "."])
        .current_dir(dir.path())
        .env("XGREP_CACHE_DIR", "xgcache")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("must be an absolute path"), "{stderr}");
    // Nothing was written into the searched tree.
    assert!(!dir.path().join("xgcache").exists());
}

#[cfg(unix)]
#[test]
fn unwritable_cache_dir_error_names_the_path() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "needle\n").unwrap();
    std::fs::set_permissions(cache.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    // Root ignores directory permissions; nothing to check then.
    if std::fs::write(cache.path().join("probe"), b"").is_ok() {
        return;
    }

    let out = xg()
        .args(["needle", "."])
        .current_dir(dir.path())
        .env("XGREP_CACHE_DIR", cache.path())
        .output()
        .unwrap();
    std::fs::set_permissions(cache.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("cannot write index at"), "{stderr}");
    assert!(
        stderr.contains(&*cache.path().to_string_lossy()),
        "{stderr}"
    );
    assert!(stderr.contains("XGREP_CACHE_DIR"), "{stderr}");
}
