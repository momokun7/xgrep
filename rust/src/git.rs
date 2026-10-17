use crate::error::{Result, XgrepError};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Create a git Command with hook-injected GIT_DIR/GIT_INDEX_FILE cleared.
///
/// Pre-commit hooks set GIT_DIR and GIT_INDEX_FILE in the environment. Without
/// this cleanup, git subprocesses spawned from tests during the hook would use
/// the hook's repository instead of the test's own temp git repo.
pub(crate) fn git_cmd() -> Command {
    let mut cmd = Command::new("git");
    cmd.env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR");
    cmd
}

/// Check if the directory is a git repository.
pub fn is_git_repo(root: &Path) -> bool {
    git_cmd()
        .args(["rev-parse", "--git-dir"])
        .current_dir(root)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Get the git repository root directory.
pub(crate) fn git_toplevel(root: &Path) -> Option<PathBuf> {
    let output = git_cmd()
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}

/// Convert a git-root-relative path to a root-relative path.
/// When root is a subdirectory of the git repo, strips the prefix.
/// Returns None if the path is outside root's scope.
fn to_root_relative(root: &Path, git_root: &Path, git_rel_path: &str) -> Option<PathBuf> {
    let canon_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let canon_git = std::fs::canonicalize(git_root).unwrap_or_else(|_| git_root.to_path_buf());

    if canon_root == canon_git {
        // root IS the git root, no conversion needed
        return Some(PathBuf::from(git_rel_path));
    }

    // root is a subdirectory of git root: compute the prefix to strip
    let prefix = canon_root.strip_prefix(&canon_git).ok()?;
    let rel = PathBuf::from(git_rel_path);
    rel.strip_prefix(prefix).ok().map(|p| p.to_path_buf())
}

/// Split NUL-separated git output (`-z`) into path tokens.
///
/// Empty tokens are skipped. A leading newline (emitted by `git log -z`
/// between commits) is stripped. Paths are neither quoted nor escaped in
/// `-z` mode, so non-ASCII names such as `日本.txt` survive intact.
pub(crate) fn split_nul(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.split(|&b| b == 0).filter_map(|tok| {
        let tok = tok.strip_prefix(b"\n").unwrap_or(tok);
        if tok.is_empty() {
            None
        } else {
            Some(tok)
        }
    })
}

/// Convert raw path bytes from git into a `PathBuf` without lossy decoding
/// on Unix (git paths are byte strings).
pub(crate) fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

/// Locate the git working-tree root by walking up from `root` looking for a
/// `.git` entry (directory, or file for worktrees/submodules).
///
/// This avoids spawning `git rev-parse --show-toplevel` on hot paths. The
/// result is canonicalized. Callers must still handle git commands failing
/// (e.g. a stray `.git` that is not a repository).
pub(crate) fn find_toplevel_fs(root: &Path) -> Option<PathBuf> {
    let canon = std::fs::canonicalize(root).ok()?;
    let mut dir: &Path = &canon;
    loop {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
}

/// Collect git-changed file paths from `-z` output, converting to root-relative paths.
fn collect_git_paths(root: &Path, git_root: &Option<PathBuf>, out: &[u8]) -> HashSet<PathBuf> {
    let mut files = HashSet::new();
    for tok in split_nul(out) {
        let rel = match git_root {
            Some(gr) => match to_root_relative(root, gr, &String::from_utf8_lossy(tok)) {
                Some(p) => p,
                None => continue, // path outside root's scope
            },
            None => bytes_to_path(tok),
        };
        let full = root.join(&rel);
        if full.exists() {
            files.insert(rel);
        }
    }
    files
}

/// Get the list of uncommitted changed files (staged + unstaged).
pub fn changed_files(root: &Path) -> Result<Vec<PathBuf>> {
    if !is_git_repo(root) {
        return Err(XgrepError::NotGitRepo);
    }

    let gr = git_toplevel(root);
    let mut files = HashSet::new();

    // unstaged changes
    let output = git_cmd()
        .args(["diff", "--name-only", "-z"])
        .current_dir(root)
        .output()?;
    files.extend(collect_git_paths(root, &gr, &output.stdout));

    // staged changes
    let output = git_cmd()
        .args(["diff", "--cached", "--name-only", "-z"])
        .current_dir(root)
        .output()?;
    files.extend(collect_git_paths(root, &gr, &output.stdout));

    let mut result: Vec<PathBuf> = files.into_iter().collect();
    result.sort();
    Ok(result)
}

/// Get the list of files changed within a specified duration.
pub fn since_files(root: &Path, duration: &str) -> Result<Vec<PathBuf>> {
    if !is_git_repo(root) {
        return Err(XgrepError::NotGitRepo);
    }

    let output = if let Some(since_str) = parse_duration(duration)? {
        git_cmd()
            .args([
                "log",
                &format!("--since={since_str}"),
                "--name-only",
                "-z",
                "--pretty=format:",
            ])
            .current_dir(root)
            .output()?
    } else {
        // commits mode: "3.commits" -> git log -3
        let n: &str = duration.split('.').next().unwrap();
        git_cmd()
            .args([
                "log",
                "-z",
                &format!("-{n}"),
                "--name-only",
                "--pretty=format:",
            ])
            .current_dir(root)
            .output()?
    };

    let gr = git_toplevel(root);
    let files = collect_git_paths(root, &gr, &output.stdout);

    let mut result: Vec<PathBuf> = files.into_iter().collect();
    result.sort();
    Ok(result)
}

fn parse_duration(duration: &str) -> Result<Option<String>> {
    if duration.ends_with(".commits") {
        let n = duration.strip_suffix(".commits").unwrap();
        if n.parse::<u32>().is_err() {
            return Err(XgrepError::InvalidArgument(format!(
                "invalid commit count: {}",
                n
            )));
        }
        return Ok(None);
    }

    let (num_str, unit) = if let Some(stripped) = duration.strip_suffix('h') {
        (stripped, "hour")
    } else if let Some(stripped) = duration.strip_suffix('m') {
        (stripped, "minute")
    } else if let Some(stripped) = duration.strip_suffix('d') {
        (stripped, "day")
    } else if let Some(stripped) = duration.strip_suffix('w') {
        (stripped, "week")
    } else {
        return Err(XgrepError::InvalidArgument(format!(
            "invalid duration format: {}. Use Nh, Nm, Nd, Nw, or N.commits",
            duration
        )));
    };

    let n: u32 = num_str
        .parse()
        .map_err(|_| XgrepError::InvalidArgument(format!("invalid number: {}", num_str)))?;
    let plural = if n == 1 { "" } else { "s" };
    Ok(Some(format!("{} {}{} ago", n, unit, plural)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_duration_hours() {
        let result = parse_duration("1h").unwrap();
        assert_eq!(result, Some("1 hour ago".to_string()));
    }

    #[test]
    fn test_parse_duration_minutes() {
        let result = parse_duration("30m").unwrap();
        assert_eq!(result, Some("30 minutes ago".to_string()));
    }

    #[test]
    fn test_parse_duration_days() {
        let result = parse_duration("2d").unwrap();
        assert_eq!(result, Some("2 days ago".to_string()));
    }

    #[test]
    fn test_parse_duration_weeks() {
        let result = parse_duration("1w").unwrap();
        assert_eq!(result, Some("1 week ago".to_string()));
    }

    #[test]
    fn test_parse_duration_commits() {
        let result = parse_duration("3.commits").unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_duration_invalid() {
        assert!(parse_duration("abc").is_err());
    }

    #[test]
    fn test_is_git_repo() {
        let cwd = std::env::current_dir().unwrap();
        let mut dir = cwd.as_path();
        while !dir.join(".git").exists() {
            if let Some(parent) = dir.parent() {
                dir = parent;
            } else {
                break;
            }
        }
        assert!(is_git_repo(dir));
    }

    #[test]
    fn test_parse_duration_zero() {
        let result = parse_duration("0h").unwrap();
        assert_eq!(result, Some("0 hours ago".to_string()));
    }

    #[test]
    fn test_parse_duration_large_number() {
        let result = parse_duration("999d").unwrap();
        assert_eq!(result, Some("999 days ago".to_string()));
    }

    #[test]
    fn test_parse_duration_empty() {
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn test_parse_duration_no_number() {
        assert!(parse_duration("h").is_err());
    }

    #[test]
    fn test_parse_duration_invalid_commits() {
        assert!(parse_duration("abc.commits").is_err());
    }

    #[test]
    fn test_is_git_repo_nonexistent_dir() {
        assert!(!is_git_repo(std::path::Path::new("/nonexistent/path")));
    }

    #[test]
    fn test_is_git_repo_non_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_git_repo(dir.path()));
    }

    /// Regression: non-ASCII names come back raw with `-z` (not quoted as
    /// `"\346\227\245..."`), so they resolve to existing files.
    #[test]
    fn test_changed_and_since_files_non_ascii() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = |args: &[&str]| {
            assert!(git_cmd()
                .args(args)
                .current_dir(root)
                .output()
                .unwrap()
                .status
                .success());
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(root.join("日本.txt"), "one").unwrap();
        std::fs::write(root.join("a b.txt"), "one").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "init"]);

        let since = since_files(root, "1.commits").unwrap();
        assert!(since.contains(&PathBuf::from("日本.txt")), "{since:?}");
        assert!(since.contains(&PathBuf::from("a b.txt")), "{since:?}");

        std::fs::write(root.join("日本.txt"), "two").unwrap();
        let changed = changed_files(root).unwrap();
        assert_eq!(changed, vec![PathBuf::from("日本.txt")]);
    }

    #[test]
    fn test_split_nul_skips_empty_and_log_newlines() {
        let toks: Vec<&[u8]> = split_nul(b"a.txt\0\0\nb.txt\0").collect();
        assert_eq!(toks, vec![&b"a.txt"[..], &b"b.txt"[..]]);
    }
}
