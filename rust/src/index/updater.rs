use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use ignore::WalkBuilder;

/// Index freshness check result.
#[derive(Debug)]
pub enum IndexStatus {
    /// Index is up-to-date, no changes.
    Fresh,
    /// Index exists but some files have changed. Use index search + direct scan of changed files.
    Stale { changed_files: Vec<PathBuf> },
    /// Index does not exist, full build required.
    NeedsFullBuild,
}

/// Metadata stored alongside the index.
#[derive(Debug)]
struct IndexMeta {
    commit_hash: Option<String>,
}

impl IndexMeta {
    fn path_for(index_path: &Path) -> PathBuf {
        index_path.with_extension("meta")
    }

    fn load(index_path: &Path) -> Option<Self> {
        let meta_path = Self::path_for(index_path);
        let content = fs::read_to_string(&meta_path).ok()?;
        let commit_hash = content.lines().next().map(|s| s.trim().to_string());
        Some(IndexMeta { commit_hash })
    }

    fn save(index_path: &Path, commit_hash: Option<&str>) -> Result<()> {
        let meta_path = Self::path_for(index_path);
        let content = commit_hash.unwrap_or("");
        fs::write(&meta_path, content)?;
        Ok(())
    }
}

/// Get the git repository root directory (via `git rev-parse --show-toplevel`).
fn git_toplevel(root: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(root)
        .output()
        .ok()?;
    if output.status.success() {
        Some(PathBuf::from(
            String::from_utf8_lossy(&output.stdout).trim(),
        ))
    } else {
        None
    }
}

/// Compute the relative prefix from `git_toplevel` to `root`.
///
/// For example, if git_toplevel is `/repo` and root is `/repo/sub/dir`,
/// returns `Some("sub/dir")`. If root is not under git_toplevel, returns `None`.
///
/// Both paths are canonicalized to handle symlinks (e.g. /var -> /private/var on macOS).
fn root_prefix_in_git(git_toplevel: &Path, root: &Path) -> Option<PathBuf> {
    let canon_toplevel = fs::canonicalize(git_toplevel).unwrap_or_else(|_| git_toplevel.into());
    let canon_root = fs::canonicalize(root).unwrap_or_else(|_| root.into());
    canon_root
        .strip_prefix(&canon_toplevel)
        .ok()
        .map(PathBuf::from)
}

/// Convert a git-root-relative path to an xgrep-root-relative path.
///
/// `prefix` is the relative path from git_toplevel to xgrep root (computed
/// by `root_prefix_in_git`). Git commands return paths relative to the
/// repository root; this function strips the prefix so that the resulting
/// path is relative to `root`.
///
/// Returns `None` if the path is outside of `root` (belongs to a sibling
/// subdirectory in the same git repo).
fn to_root_relative(prefix: &Path, git_rel_path: &str) -> Option<PathBuf> {
    let git_path = Path::new(git_rel_path);
    git_path.strip_prefix(prefix).ok().map(PathBuf::from)
}

/// Convert a git-root-relative path using the computed prefix.
///
/// If `prefix` is `None` (root == git toplevel), the path is used as-is.
/// If `prefix` is `Some`, the prefix is stripped; paths outside root return `None`.
fn convert_git_path(prefix: &Option<PathBuf>, git_rel_path: &str) -> Option<PathBuf> {
    match prefix {
        Some(pfx) if !pfx.as_os_str().is_empty() => to_root_relative(pfx, git_rel_path),
        _ => Some(PathBuf::from(git_rel_path)),
    }
}

/// Get the current git HEAD commit hash.
fn current_commit_hash(root: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

/// Get the newest file mtime in the directory (UNIX epoch seconds).
fn newest_file_mtime(root: &Path) -> Option<u64> {
    let mut newest = 0u64;
    for entry in WalkBuilder::new(root).build().flatten() {
        if entry.file_type().is_some_and(|ft| ft.is_file()) {
            // Skip index-related files (.xgrep, .meta, .cache) so they don't
            // affect freshness detection for the source tree.
            if let Some(name) = entry.path().file_name().and_then(|n| n.to_str()) {
                if name.ends_with(".xgrep")
                    || name.ends_with(".xgrep.meta")
                    || name.ends_with(".xgrep.cache")
                {
                    continue;
                }
            }
            if let Ok(meta) = entry.metadata() {
                if let Ok(mtime) = meta.modified() {
                    let secs = mtime
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    if secs > newest {
                        newest = secs;
                    }
                }
            }
        }
    }
    if newest > 0 {
        Some(newest)
    } else {
        None
    }
}

/// Extract file paths from a `git status --porcelain` line.
///
/// Format: "XY filename" or "XY \"filename with spaces\"" or "XY old -> new"
/// For renames, returns both old and new paths (to also exclude stale entries for old path).
fn parse_status_paths(line: &str) -> Vec<String> {
    if line.len() < 4 {
        return vec![];
    }
    let path_part = &line[3..];

    let clean = |p: &str| -> String {
        let p = if p.starts_with('"') && p.ends_with('"') {
            &p[1..p.len() - 1]
        } else {
            p
        };
        p.to_string()
    };

    if let Some(arrow_pos) = path_part.find(" -> ") {
        // Rename: return both old and new paths
        let old = &path_part[..arrow_pos];
        let new = &path_part[arrow_pos + 4..];
        let old = clean(old);
        let new = clean(new);
        let mut result = vec![];
        if !old.is_empty() {
            result.push(old);
        }
        if !new.is_empty() {
            result.push(new);
        }
        result
    } else {
        let path = clean(path_part);
        if path.is_empty() {
            vec![]
        } else {
            vec![path]
        }
    }
}

/// Common helper to collect uncommitted changes (staged + unstaged) and untracked files.
///
/// Returned paths are relative to `root`, not to the git repository root.
/// Paths outside of `root` are filtered out.
fn collect_uncommitted_changes(root: &Path) -> Result<std::collections::HashSet<PathBuf>> {
    let prefix = git_toplevel(root).and_then(|tl| root_prefix_in_git(&tl, root));
    let mut changed = std::collections::HashSet::new();

    // Staged + unstaged changes (tracked files only)
    // -uno excludes untracked files to prevent hangs in large repositories
    let output = std::process::Command::new("git")
        .args(["status", "--porcelain", "-uno"])
        .current_dir(root)
        .output()?;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        for path in parse_status_paths(line) {
            if let Some(p) = convert_git_path(&prefix, &path) {
                changed.insert(p);
            }
        }
    }

    // Untracked files (fast enumeration respecting .gitignore)
    // Using ls-files --others instead of git status --porcelain
    // to stay fast even with large numbers of untracked files like node_modules
    let output = std::process::Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard"])
        .current_dir(root)
        .output()?;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim();
        if !line.is_empty() {
            if let Some(p) = convert_git_path(&prefix, line) {
                changed.insert(p);
            }
        }
    }

    Ok(changed)
}

/// Get files changed between two commits.
///
/// Returned paths are relative to `root`, not to the git repository root.
/// Paths outside of `root` are filtered out.
fn changed_files_since(root: &Path, old_hash: &str) -> Result<Vec<PathBuf>> {
    let prefix = git_toplevel(root).and_then(|tl| root_prefix_in_git(&tl, root));
    let mut files = std::collections::HashSet::new();

    // Committed changes: old_hash..HEAD
    let output = std::process::Command::new("git")
        .args([
            "diff-tree",
            "-r",
            "--name-only",
            "--no-commit-id",
            old_hash,
            "HEAD",
        ])
        .current_dir(root)
        .output()?;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim();
        if !line.is_empty() {
            if let Some(p) = convert_git_path(&prefix, line) {
                files.insert(p);
            }
        }
    }

    let mut result: Vec<PathBuf> = files.into_iter().collect();
    result.sort();
    Ok(result)
}

/// Save index metadata (call after build_index).
pub fn save_meta(root: &Path, index_path: &Path) -> Result<()> {
    let hash = current_commit_hash(root);
    IndexMeta::save(index_path, hash.as_deref())
}

/// Check index freshness and return changed file list (does not rebuild).
pub fn check_index_status(root: &Path, index_path: &Path) -> Result<IndexStatus> {
    if !index_path.exists() {
        return Ok(IndexStatus::NeedsFullBuild);
    }

    let meta = IndexMeta::load(index_path);
    let current_hash = current_commit_hash(root);

    let mut changed = std::collections::HashSet::new();

    match (&meta, &current_hash) {
        (Some(m), Some(curr)) => {
            let hash_changed = m.commit_hash.as_deref() != Some(curr.as_str());

            if hash_changed {
                // Commit changed: check diff-tree + status + all untracked files
                let old_hash = m.commit_hash.as_deref().unwrap_or("");
                if let Ok(files) = changed_files_since(root, old_hash) {
                    for f in files {
                        changed.insert(f);
                    }
                }
                changed.extend(collect_uncommitted_changes(root)?);
            } else {
                // Same commit: only check staged/unstaged changes (fast path)
                // Skip git ls-files --others to save ~170ms
                let prefix = git_toplevel(root).and_then(|tl| root_prefix_in_git(&tl, root));
                let output = std::process::Command::new("git")
                    .args(["status", "--porcelain", "-uno"])
                    .current_dir(root)
                    .output()?;
                for line in String::from_utf8_lossy(&output.stdout).lines() {
                    for path in parse_status_paths(line) {
                        if let Some(p) = convert_git_path(&prefix, &path) {
                            changed.insert(p);
                        }
                    }
                }
            }
        }
        _ => {
            // Non-git repo or no metadata: determine freshness via mtime
            let index_mtime = fs::metadata(index_path)?
                .modified()?
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            if let Some(newest) = newest_file_mtime(root) {
                if index_mtime >= newest {
                    return Ok(IndexStatus::Fresh);
                }
            }
            return Ok(IndexStatus::NeedsFullBuild);
        }
    }

    if changed.is_empty() {
        Ok(IndexStatus::Fresh)
    } else if changed.len() > 500 {
        // Too many changes: full rebuild is more efficient
        Ok(IndexStatus::NeedsFullBuild)
    } else {
        let mut files: Vec<PathBuf> = changed.into_iter().collect();
        files.sort();
        Ok(IndexStatus::Stale {
            changed_files: files,
        })
    }
}

/// Build the index with cache (incremental update).
#[allow(dead_code)]
fn build_with_cache(root: &Path, index_path: &Path) -> Result<()> {
    let cache_path = crate::index::cache::cache_path_for(index_path);
    crate::index::builder::build_index_with_cache(root, index_path, Some(&cache_path))
}

/// Check if the index is up-to-date and rebuild if necessary.
#[allow(dead_code)]
pub fn ensure_fresh_index(root: &Path, index_path: &Path) -> Result<()> {
    if !index_path.exists() {
        // Index does not exist: full build (with cache creation)
        eprintln!("[indexing...]");
        build_with_cache(root, index_path)?;
        save_meta(root, index_path)?;
        eprintln!("[done]");
        return Ok(());
    }

    // Index exists: check if update is needed
    let meta = IndexMeta::load(index_path);
    let current_hash = current_commit_hash(root);

    match (&meta, &current_hash) {
        (Some(m), Some(curr)) if m.commit_hash.as_deref() == Some(curr.as_str()) => {
            // Same commit. Check for uncommitted changes
            let uncommitted = collect_uncommitted_changes(root)?;
            if uncommitted.is_empty() {
                // No changes, index is up-to-date
                return Ok(());
            }
            // Uncommitted changes found, incremental rebuild with cache
            eprintln!("[updating index...]");
            build_with_cache(root, index_path)?;
            IndexMeta::save(index_path, Some(curr))?;
            eprintln!("[done]");
        }
        (Some(m), Some(curr)) => {
            // Different commit
            let old_hash = m.commit_hash.as_deref().unwrap_or("");
            let changed = changed_files_since(root, old_hash)?;

            if changed.is_empty() {
                // No file changes (e.g. merge commit)
                IndexMeta::save(index_path, Some(curr))?;
                return Ok(());
            }

            // Changes detected, incremental rebuild with cache
            eprintln!("[updating index ({} files changed)...]", changed.len());
            build_with_cache(root, index_path)?;
            IndexMeta::save(index_path, Some(curr))?;
            eprintln!("[done]");
        }
        _ => {
            // Non-git repo or no metadata: determine freshness via mtime
            let index_mtime = fs::metadata(index_path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);

            let needs_rebuild = match newest_file_mtime(root) {
                Some(newest) => index_mtime < newest,
                None => true,
            };

            if needs_rebuild {
                eprintln!("[updating index...]");
                build_with_cache(root, index_path)?;
                save_meta(root, index_path)?;
                eprintln!("[done]");
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::tempdir;

    fn init_git_repo(dir: &Path) {
        Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "test"])
            .current_dir(dir)
            .output()
            .unwrap();
    }

    #[test]
    fn test_ensure_fresh_index_creates_new() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("hello.txt"), "hello world").unwrap();
        let index_path = root.join("test.xgrep");

        ensure_fresh_index(root, &index_path).unwrap();

        assert!(index_path.exists());
    }

    #[test]
    fn test_ensure_fresh_index_no_rebuild_if_unchanged() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        init_git_repo(root);
        // Add index files to .gitignore so git status does not detect them
        fs::write(root.join(".gitignore"), "*.xgrep\n*.meta\n*.cache\n").unwrap();
        fs::write(root.join("hello.txt"), "hello world").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();

        let index_path = root.join("test.xgrep");
        ensure_fresh_index(root, &index_path).unwrap();

        let mtime1 = fs::metadata(&index_path).unwrap().modified().unwrap();

        // Re-run without changes
        std::thread::sleep(std::time::Duration::from_millis(100));
        ensure_fresh_index(root, &index_path).unwrap();

        let mtime2 = fs::metadata(&index_path).unwrap().modified().unwrap();
        assert_eq!(mtime1, mtime2); // Index was not rebuilt
    }

    #[test]
    fn test_ensure_fresh_index_rebuilds_after_commit() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        init_git_repo(root);
        fs::write(root.join("hello.txt"), "hello world").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();

        let index_path = root.join("test.xgrep");
        ensure_fresh_index(root, &index_path).unwrap();

        let mtime1 = fs::metadata(&index_path).unwrap().modified().unwrap();

        // Create a new commit
        std::thread::sleep(std::time::Duration::from_millis(100));
        fs::write(root.join("new_file.txt"), "new content").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "add file"])
            .current_dir(root)
            .output()
            .unwrap();

        ensure_fresh_index(root, &index_path).unwrap();

        let mtime2 = fs::metadata(&index_path).unwrap().modified().unwrap();
        assert_ne!(mtime1, mtime2); // Index was rebuilt
    }

    #[test]
    fn test_meta_save_load() {
        let dir = tempdir().unwrap();
        let index_path = dir.path().join("test.xgrep");
        fs::write(&index_path, "dummy").unwrap();

        IndexMeta::save(&index_path, Some("abc123")).unwrap();
        let meta = IndexMeta::load(&index_path).unwrap();
        assert_eq!(meta.commit_hash, Some("abc123".to_string()));
    }

    #[test]
    fn test_meta_load_missing() {
        let dir = tempdir().unwrap();
        let index_path = dir.path().join("nonexistent.xgrep");
        assert!(IndexMeta::load(&index_path).is_none());
    }

    #[test]
    fn test_ensure_fresh_index_rebuilds_on_uncommitted_changes() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        init_git_repo(root);
        fs::write(root.join("hello.txt"), "hello world").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();

        let index_path = root.join("test.xgrep");
        ensure_fresh_index(root, &index_path).unwrap();

        let mtime1 = fs::metadata(&index_path).unwrap().modified().unwrap();

        // Make uncommitted changes (same commit, dirty working tree)
        std::thread::sleep(std::time::Duration::from_millis(100));
        fs::write(root.join("hello.txt"), "changed content").unwrap();

        ensure_fresh_index(root, &index_path).unwrap();

        let mtime2 = fs::metadata(&index_path).unwrap().modified().unwrap();
        assert_ne!(mtime1, mtime2); // Index was rebuilt due to uncommitted changes
    }

    #[test]
    fn test_ensure_fresh_index_rebuilds_on_new_untracked_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        init_git_repo(root);
        fs::write(root.join("hello.txt"), "hello world").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();

        let index_path = root.join("test.xgrep");
        ensure_fresh_index(root, &index_path).unwrap();

        let mtime1 = fs::metadata(&index_path).unwrap().modified().unwrap();

        // Add a new untracked file
        std::thread::sleep(std::time::Duration::from_millis(100));
        fs::write(root.join("new_file.txt"), "new content").unwrap();

        ensure_fresh_index(root, &index_path).unwrap();

        let mtime2 = fs::metadata(&index_path).unwrap().modified().unwrap();
        assert_ne!(mtime1, mtime2); // Index was rebuilt due to new untracked file
    }

    #[test]
    fn test_parse_status_paths_simple() {
        assert_eq!(
            parse_status_paths(" M hello.txt"),
            vec!["hello.txt".to_string()]
        );
    }

    #[test]
    fn test_parse_status_paths_rename() {
        let paths = parse_status_paths("R  old.txt -> new.txt");
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&"old.txt".to_string()));
        assert!(paths.contains(&"new.txt".to_string()));
    }

    #[test]
    fn test_parse_status_paths_quoted() {
        assert_eq!(
            parse_status_paths(" M \"file with spaces.txt\""),
            vec!["file with spaces.txt".to_string()]
        );
    }

    #[test]
    fn test_parse_status_paths_short() {
        assert!(parse_status_paths("M").is_empty());
    }

    #[test]
    fn test_parse_status_paths_empty_line() {
        assert!(parse_status_paths("?? ").is_empty());
    }

    #[test]
    fn test_check_index_status_fresh() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        init_git_repo(root);
        fs::write(root.join(".gitignore"), "*.xgrep\n*.meta\n*.cache\n").unwrap();
        fs::write(root.join("hello.txt"), "hello world").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();

        let index_path = root.join("test.xgrep");
        ensure_fresh_index(root, &index_path).unwrap();

        let status = check_index_status(root, &index_path).unwrap();
        assert!(matches!(status, IndexStatus::Fresh));
    }

    #[test]
    fn test_check_index_status_stale() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        init_git_repo(root);
        fs::write(root.join(".gitignore"), "*.xgrep\n*.meta\n*.cache\n").unwrap();
        fs::write(root.join("hello.txt"), "hello world").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();

        let index_path = root.join("test.xgrep");
        ensure_fresh_index(root, &index_path).unwrap();

        // Modify a file
        fs::write(root.join("hello.txt"), "changed").unwrap();

        let status = check_index_status(root, &index_path).unwrap();
        match status {
            IndexStatus::Stale { changed_files } => {
                assert!(!changed_files.is_empty());
            }
            other => panic!("expected Stale, got {:?}", other),
        }
    }

    #[test]
    fn test_check_index_status_no_index() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let index_path = root.join("nonexistent.xgrep");

        let status = check_index_status(root, &index_path).unwrap();
        assert!(matches!(status, IndexStatus::NeedsFullBuild));
    }

    #[test]
    fn test_check_index_status_stale_new_untracked_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        init_git_repo(root);
        fs::write(root.join(".gitignore"), "*.xgrep\n*.meta\n*.cache\n").unwrap();
        fs::write(root.join("hello.txt"), "hello world").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();

        let index_path = root.join("test.xgrep");
        ensure_fresh_index(root, &index_path).unwrap();

        // Add a new untracked file
        fs::write(root.join("new_file.txt"), "new content").unwrap();

        let status = check_index_status(root, &index_path).unwrap();
        // When commit hash is the same, fast path does not detect untracked files (perf optimization)
        // Untracked files will be detected after the next commit or full rebuild
        assert!(matches!(status, IndexStatus::Fresh));
    }

    #[test]
    fn test_non_git_mtime_freshness() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello").unwrap();

        let index_path = root.join("index.xgrep");
        crate::index::builder::build_index(root, &index_path).unwrap();
        save_meta(root, &index_path).unwrap();

        // Index was just built, should be Fresh
        let status = check_index_status(root, &index_path).unwrap();
        assert!(
            matches!(status, IndexStatus::Fresh),
            "expected Fresh, got {:?}",
            status
        );

        // Wait for filesystem timestamp to advance (Windows NTFS needs >1s)
        std::thread::sleep(std::time::Duration::from_secs(2));
        fs::write(root.join("b.txt"), "world").unwrap();

        // Index should be NeedsFullBuild now
        let status = check_index_status(root, &index_path).unwrap();
        assert!(
            matches!(status, IndexStatus::NeedsFullBuild),
            "expected NeedsFullBuild, got {:?}",
            status
        );
    }

    #[test]
    fn test_non_git_ensure_fresh_skips_rebuild_when_fresh() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello").unwrap();

        let index_path = root.join("test.xgrep");
        ensure_fresh_index(root, &index_path).unwrap();

        let mtime1 = fs::metadata(&index_path).unwrap().modified().unwrap();

        // Re-run without changes
        std::thread::sleep(std::time::Duration::from_millis(100));
        ensure_fresh_index(root, &index_path).unwrap();

        let mtime2 = fs::metadata(&index_path).unwrap().modified().unwrap();
        assert_eq!(mtime1, mtime2); // Not rebuilt
    }

    #[test]
    fn test_check_index_status_stale_after_commit() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        init_git_repo(root);
        fs::write(root.join(".gitignore"), "*.xgrep\n*.meta\n*.cache\n").unwrap();
        fs::write(root.join("hello.txt"), "hello world").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();

        let index_path = root.join("test.xgrep");
        ensure_fresh_index(root, &index_path).unwrap();

        // New commit
        fs::write(root.join("new_file.txt"), "new content").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "add file"])
            .current_dir(root)
            .output()
            .unwrap();

        let status = check_index_status(root, &index_path).unwrap();
        match status {
            IndexStatus::Stale { changed_files } => {
                assert!(!changed_files.is_empty());
            }
            other => panic!("expected Stale, got {:?}", other),
        }
    }

    #[test]
    fn test_to_root_relative_subdir() {
        let prefix = PathBuf::from("sub/dir");

        // Path inside root
        let result = to_root_relative(&prefix, "sub/dir/file.rs");
        assert_eq!(result, Some(PathBuf::from("file.rs")));

        // Path outside root (sibling directory)
        let result = to_root_relative(&prefix, "other/file.rs");
        assert!(result.is_none());

        // Nested path inside root
        let result = to_root_relative(&prefix, "sub/dir/nested/deep.rs");
        assert_eq!(result, Some(PathBuf::from("nested/deep.rs")));
    }

    #[test]
    fn test_to_root_relative_empty_prefix() {
        let prefix = PathBuf::from("");

        let result = to_root_relative(&prefix, "src/main.rs");
        assert_eq!(result, Some(PathBuf::from("src/main.rs")));
    }

    #[test]
    fn test_convert_git_path_no_prefix() {
        // When prefix is None (root == git toplevel), path is used as-is
        let result = convert_git_path(&None, "src/main.rs");
        assert_eq!(result, Some(PathBuf::from("src/main.rs")));
    }

    #[test]
    fn test_convert_git_path_with_prefix() {
        let prefix = Some(PathBuf::from("sub/dir"));

        let result = convert_git_path(&prefix, "sub/dir/file.rs");
        assert_eq!(result, Some(PathBuf::from("file.rs")));

        // Outside root
        let result = convert_git_path(&prefix, "other/file.rs");
        assert!(result.is_none());
    }

    #[test]
    fn test_collect_uncommitted_changes_subdir() {
        // Set up a git repo with a subdirectory structure
        let dir = tempdir().unwrap();
        let git_root = dir.path();
        init_git_repo(git_root);

        // Create subdirectory structure
        let sub_dir = git_root.join("sub").join("dir");
        fs::create_dir_all(&sub_dir).unwrap();
        fs::write(git_root.join(".gitignore"), "*.xgrep\n*.meta\n*.cache\n").unwrap();
        fs::write(sub_dir.join("file.rs"), "fn main() {}").unwrap();
        fs::write(git_root.join("root_file.txt"), "root").unwrap();

        Command::new("git")
            .args(["add", "."])
            .current_dir(git_root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(git_root)
            .output()
            .unwrap();

        // Modify a file in the subdirectory
        fs::write(sub_dir.join("file.rs"), "fn main() { changed }").unwrap();
        // Also modify a file outside the subdirectory
        fs::write(git_root.join("root_file.txt"), "changed root").unwrap();

        // Collect changes from the subdirectory (xgrep root = sub/dir)
        let changes = collect_uncommitted_changes(&sub_dir).unwrap();

        // Should contain file.rs (relative to sub/dir), NOT sub/dir/file.rs
        assert!(
            changes.contains(&PathBuf::from("file.rs")),
            "expected file.rs in changes, got: {:?}",
            changes
        );

        // Should NOT contain root_file.txt (outside of sub/dir)
        assert!(
            !changes.contains(&PathBuf::from("root_file.txt")),
            "root_file.txt should be filtered out, got: {:?}",
            changes
        );

        // Should NOT contain git-root-relative path
        assert!(
            !changes.contains(&PathBuf::from("sub/dir/file.rs")),
            "should not contain git-root-relative path, got: {:?}",
            changes
        );
    }

    #[test]
    fn test_check_index_status_subdir_stale() {
        // When xgrep root is a subdirectory of git root, changed_files
        // should be relative to xgrep root, not git root.
        let dir = tempdir().unwrap();
        let git_root = dir.path();
        init_git_repo(git_root);

        let sub_dir = git_root.join("sub").join("dir");
        fs::create_dir_all(&sub_dir).unwrap();
        fs::write(git_root.join(".gitignore"), "*.xgrep\n*.meta\n*.cache\n").unwrap();
        fs::write(sub_dir.join("file.rs"), "fn main() {}").unwrap();

        Command::new("git")
            .args(["add", "."])
            .current_dir(git_root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(git_root)
            .output()
            .unwrap();

        // Build index from the subdirectory
        let index_path = sub_dir.join("test.xgrep");
        ensure_fresh_index(&sub_dir, &index_path).unwrap();

        // Modify a file in the subdirectory
        fs::write(sub_dir.join("file.rs"), "fn main() { changed }").unwrap();

        let status = check_index_status(&sub_dir, &index_path).unwrap();
        match status {
            IndexStatus::Stale { changed_files } => {
                // Paths should be relative to sub/dir, not git root
                assert!(
                    changed_files.contains(&PathBuf::from("file.rs")),
                    "expected file.rs, got: {:?}",
                    changed_files
                );
                assert!(
                    !changed_files.contains(&PathBuf::from("sub/dir/file.rs")),
                    "should not contain git-root-relative path: {:?}",
                    changed_files
                );
            }
            other => panic!("expected Stale, got {:?}", other),
        }
    }

    #[test]
    fn test_changed_files_since_subdir() {
        let dir = tempdir().unwrap();
        let git_root = dir.path();
        init_git_repo(git_root);

        let sub_dir = git_root.join("sub");
        fs::create_dir_all(&sub_dir).unwrap();
        fs::write(sub_dir.join("a.txt"), "hello").unwrap();
        fs::write(git_root.join("root.txt"), "root").unwrap();

        Command::new("git")
            .args(["add", "."])
            .current_dir(git_root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(git_root)
            .output()
            .unwrap();

        let old_hash = current_commit_hash(git_root).unwrap();

        // New commit with changes in both locations
        fs::write(sub_dir.join("a.txt"), "changed").unwrap();
        fs::write(git_root.join("root.txt"), "changed root").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(git_root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "changes"])
            .current_dir(git_root)
            .output()
            .unwrap();

        // Query from subdirectory
        let files = changed_files_since(&sub_dir, &old_hash).unwrap();

        // Should contain a.txt (relative to sub/), not sub/a.txt
        assert!(
            files.contains(&PathBuf::from("a.txt")),
            "expected a.txt, got: {:?}",
            files
        );

        // Should NOT contain root.txt (outside of sub/)
        assert!(
            !files.contains(&PathBuf::from("root.txt")),
            "root.txt should be filtered out: {:?}",
            files
        );
    }
}
