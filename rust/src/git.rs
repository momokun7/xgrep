use anyhow::{bail, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Check if the directory is a git repository.
pub fn is_git_repo(root: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(root)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Get the list of uncommitted changed files (staged + unstaged).
pub fn changed_files(root: &Path) -> Result<Vec<PathBuf>> {
    if !is_git_repo(root) {
        bail!("not a git repository");
    }

    let mut files = HashSet::new();

    // unstaged changes
    let output = Command::new("git")
        .args(["diff", "--name-only"])
        .current_dir(root)
        .output()?;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim();
        if !line.is_empty() {
            let path = root.join(line);
            if path.exists() {
                files.insert(PathBuf::from(line));
            }
        }
    }

    // staged changes
    let output = Command::new("git")
        .args(["diff", "--cached", "--name-only"])
        .current_dir(root)
        .output()?;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim();
        if !line.is_empty() {
            let path = root.join(line);
            if path.exists() {
                files.insert(PathBuf::from(line));
            }
        }
    }

    let mut result: Vec<PathBuf> = files.into_iter().collect();
    result.sort();
    Ok(result)
}

/// Get the list of files changed within a specified duration.
pub fn since_files(root: &Path, duration: &str) -> Result<Vec<PathBuf>> {
    if !is_git_repo(root) {
        bail!("not a git repository");
    }

    let output = if let Some(since_str) = parse_duration(duration)? {
        Command::new("git")
            .args([
                "log",
                &format!("--since={since_str}"),
                "--name-only",
                "--pretty=format:",
            ])
            .current_dir(root)
            .output()?
    } else {
        // commits mode: "3.commits" -> git log -3
        let n: &str = duration.split('.').next().unwrap();
        Command::new("git")
            .args(["log", &format!("-{n}"), "--name-only", "--pretty=format:"])
            .current_dir(root)
            .output()?
    };

    let mut files = HashSet::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim();
        if !line.is_empty() {
            let path = root.join(line);
            if path.exists() {
                files.insert(PathBuf::from(line));
            }
        }
    }

    let mut result: Vec<PathBuf> = files.into_iter().collect();
    result.sort();
    Ok(result)
}

fn parse_duration(duration: &str) -> Result<Option<String>> {
    if duration.ends_with(".commits") {
        let n = duration.strip_suffix(".commits").unwrap();
        if n.parse::<u32>().is_err() {
            bail!("invalid commit count: {}", n);
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
        bail!(
            "invalid duration format: {}. Use Nh, Nm, Nd, Nw, or N.commits",
            duration
        );
    };

    let n: u32 = num_str
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid number: {}", num_str))?;
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
}
