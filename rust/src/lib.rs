//! Ultra-fast indexed code search engine using trigram inverted index.
//!
//! # Example
//!
//! ```no_run
//! use xgrep_search::{Xgrep, SearchOptions};
//!
//! let xg = Xgrep::open(".").unwrap();
//! xg.build_index().unwrap();
//! let results = xg.search("fn main", &SearchOptions::default()).unwrap();
//! for r in &results {
//!     println!("{}:{}: {}", r.file, r.line_number, r.line);
//! }
//! ```

pub(crate) mod candidates;
pub(crate) mod filetype;
pub(crate) mod git;
pub mod hints;
pub(crate) mod index;
pub(crate) mod mcp;
pub(crate) mod mcp_tools;
pub mod output;
pub mod search;
pub(crate) mod trigram;
pub(crate) mod trigram_query;

/// Re-exports for fuzz testing. Not part of the public API.
#[cfg(feature = "fuzz")]
pub mod fuzz_exports {
    pub use crate::index::format::{decode_varint, encode_varint};
    pub use crate::index::reader::IndexReader;
}

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

pub use filetype::extensions_for_type;
pub use filetype::list_all_types;
pub use search::SearchResult;

/// Return git changed files (unstaged + staged) relative to the given root.
///
/// Returns paths relative to `root`. Includes unstaged changes, staged changes,
/// and untracked files. Returns an error if `root` is not inside a git repository.
pub fn git_changed_files(root: &Path) -> Result<Vec<PathBuf>> {
    if !git::is_git_repo(root) {
        bail!("--changed requires a git repository");
    }
    git::changed_files(root)
}

/// Search options.
///
/// `Default::default()` creates a case-sensitive literal string search with no filters.
#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    /// Case-insensitive search (ASCII folding only)
    pub case_insensitive: bool,
    /// Treat pattern as regex instead of literal string
    pub regex: bool,
    /// Filter results by file extension (e.g., "rs", "py", "js")
    pub file_type: Option<String>,
    /// Maximum number of results to return
    pub max_count: Option<usize>,
    /// Only search files with uncommitted git changes
    pub changed_only: bool,
    /// Only search files changed within a time duration (e.g., "1h", "2d", "3.commits")
    pub since: Option<String>,
    /// Filter results by path substring match (e.g., "src/auth", "tests/")
    pub path_pattern: Option<String>,
    /// Check index freshness and use hybrid search for changed files.
    /// When false (default), uses existing index as-is for maximum speed.
    pub fresh: bool,
}

/// Main entry point for the search engine.
///
/// Use `open()` to specify a directory, then `search()` to execute queries.
/// Index auto-build, freshness checks, and hybrid search are handled internally.
pub struct Xgrep {
    root: PathBuf,
    index_path: PathBuf,
}

impl Xgrep {
    /// Open a directory. Index path is auto-resolved (.xgrep/index or ~/.cache/xgrep/<hash>/index).
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let index_path = resolve_index_path(&root)?;
        Ok(Self { root, index_path })
    }

    /// Open with a local index (.xgrep/) explicitly.
    pub fn open_local(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let index_path = root.join(".xgrep").join("index");
        Ok(Self { root, index_path })
    }

    /// Returns the root directory path.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the index file path.
    pub fn index_path(&self) -> &Path {
        &self.index_path
    }

    /// Build (or rebuild) the search index.
    pub fn build_index(&self) -> Result<()> {
        if let Some(parent) = self.index_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let cache = index::cache::cache_path_for(&self.index_path);
        index::builder::build_index_with_cache(&self.root, &self.index_path, Some(&cache))?;
        index::updater::save_meta(&self.root, &self.index_path)?;
        Ok(())
    }

    /// Execute a search. Auto-build, hybrid search, and git-changed-file search are handled internally.
    pub fn search(&self, pattern: &str, opts: &SearchOptions) -> Result<Vec<SearchResult>> {
        let mut results = if opts.changed_only || opts.since.is_some() {
            self.search_changed(pattern, opts)?
        } else {
            self.search_indexed(pattern, opts)?
        };

        // file_type filter
        if let Some(ref ft) = opts.file_type {
            if let Some(exts) = filetype::extensions_for_type(ft) {
                results.retain(|r| {
                    Path::new(&r.file)
                        .extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| exts.contains(&e))
                });
            } else if !crate::mcp::is_mcp_mode() {
                eprintln!("warning: unknown file type '{}', showing all results", ft);
            }
        }

        // path_pattern filter
        if let Some(ref pp) = opts.path_pattern {
            results.retain(|r| r.file.contains(pp));
        }

        // max_count
        if let Some(max) = opts.max_count {
            results.truncate(max);
        }

        Ok(results)
    }

    /// Index-based search. When `opts.fresh` is true, checks index freshness
    /// and uses hybrid search for changed files. When false (default), uses
    /// existing index as-is for maximum speed.
    fn search_indexed(&self, pattern: &str, opts: &SearchOptions) -> Result<Vec<SearchResult>> {
        if !opts.fresh {
            // Fast path: use index as-is without freshness check
            if self.index_path.exists() {
                let reader = index::reader::IndexReader::open(&self.index_path)?;
                let results = if opts.regex {
                    search::search_regex(&reader, &self.root, pattern, opts.case_insensitive)
                } else {
                    search::search(&reader, &self.root, pattern, opts.case_insensitive)
                };
                // Background rebuild: spawn a detached process to update the index
                // The current search uses the existing index; next search will use the updated one
                self.spawn_background_rebuild();
                return results;
            }
            // Index doesn't exist, fall through to auto-build
        }

        let status = index::updater::check_index_status(&self.root, &self.index_path)?;

        match status {
            index::updater::IndexStatus::Fresh => {
                let reader = index::reader::IndexReader::open(&self.index_path)?;
                if opts.regex {
                    search::search_regex(&reader, &self.root, pattern, opts.case_insensitive)
                } else {
                    search::search(&reader, &self.root, pattern, opts.case_insensitive)
                }
            }
            index::updater::IndexStatus::Stale { changed_files } => {
                let reader = index::reader::IndexReader::open(&self.index_path)?;

                // Search from index (results for changed files may be stale)
                let mut index_results = if opts.regex {
                    search::search_regex(&reader, &self.root, pattern, opts.case_insensitive)?
                } else {
                    search::search(&reader, &self.root, pattern, opts.case_insensitive)?
                };

                // Exclude results from changed files (may be stale data)
                let changed_set: std::collections::HashSet<String> = changed_files
                    .iter()
                    .map(|p| p.to_string_lossy().to_string())
                    .collect();
                index_results.retain(|r| !changed_set.contains(&r.file));

                // Directly scan changed files
                let direct_results = if opts.regex {
                    search::search_files_regex(
                        &self.root,
                        &changed_files,
                        pattern,
                        opts.case_insensitive,
                    )?
                } else {
                    search::search_files(
                        &self.root,
                        &changed_files,
                        pattern,
                        opts.case_insensitive,
                    )?
                };

                // Merge, sort, and deduplicate
                index_results.extend(direct_results);
                index_results
                    .sort_by(|a, b| a.file.cmp(&b.file).then(a.line_number.cmp(&b.line_number)));
                index_results.dedup_by(|a, b| a.file == b.file && a.line_number == b.line_number);
                Ok(index_results)
            }
            index::updater::IndexStatus::NeedsFullBuild => {
                // No index, full build required
                if !crate::mcp::is_mcp_mode() {
                    eprintln!("[indexing...]");
                }
                self.build_index()?;
                if !crate::mcp::is_mcp_mode() {
                    eprintln!("[done]");
                }

                let reader = index::reader::IndexReader::open(&self.index_path)?;
                if opts.regex {
                    search::search_regex(&reader, &self.root, pattern, opts.case_insensitive)
                } else {
                    search::search(&reader, &self.root, pattern, opts.case_insensitive)
                }
            }
        }
    }

    /// Spawn a detached background process to rebuild the index.
    /// Skips if: lock file exists, or index was built within the last 30 seconds.
    fn spawn_background_rebuild(&self) {
        // Skip if lock file exists (another rebuild in progress)
        if self.index_path.with_extension("lock").exists() {
            return;
        }
        // Skip if index is fresh enough (built within last 30 seconds)
        if let Ok(meta) = std::fs::metadata(&self.index_path) {
            if let Ok(modified) = meta.modified() {
                if modified.elapsed().unwrap_or_default().as_secs() < 30 {
                    return;
                }
            }
        }
        // Get the current executable path
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(_) => return,
        };
        // Spawn detached: `xg init` in the background
        let _ = std::process::Command::new(exe)
            .arg("init")
            .current_dir(&self.root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }

    /// Search only git-changed files. Returns error if not a git repository.
    fn search_changed(&self, pattern: &str, opts: &SearchOptions) -> Result<Vec<SearchResult>> {
        if !git::is_git_repo(&self.root) {
            bail!("--changed/--since requires a git repository");
        }

        let mut files = Vec::new();
        if opts.changed_only {
            files.extend(git::changed_files(&self.root)?);
        }
        if let Some(ref since) = opts.since {
            files.extend(git::since_files(&self.root, since)?);
        }
        files.sort();
        files.dedup();

        if opts.regex {
            search::search_files_regex(&self.root, &files, pattern, opts.case_insensitive)
        } else {
            search::search_files(&self.root, &files, pattern, opts.case_insensitive)
        }
    }

    /// Find files matching a glob or substring pattern.
    /// Returns a list of relative file paths from the index.
    pub fn find_files(&self, pattern: &str) -> Result<Vec<String>> {
        if !self.index_path.exists() {
            if !crate::mcp::is_mcp_mode() {
                eprintln!("[indexing...]");
            }
            self.build_index()?;
            if !crate::mcp::is_mcp_mode() {
                eprintln!("[done]");
            }
        }

        let reader = index::reader::IndexReader::open(&self.index_path)?;
        let file_count = reader.file_count();
        let mut matched = Vec::new();

        let is_glob = pattern.contains('*') || pattern.contains('?') || pattern.contains('[');

        if is_glob {
            let glob = glob::Pattern::new(pattern)
                .map_err(|e| anyhow::anyhow!("invalid glob pattern: {}", e))?;
            for fid in 0..file_count {
                let path = reader.file_path(fid);
                if glob.matches(path) {
                    matched.push(path.to_string());
                }
            }
        } else {
            let pattern_lower = pattern.to_lowercase();
            for fid in 0..file_count {
                let path = reader.file_path(fid);
                if path.to_lowercase().contains(&pattern_lower) {
                    matched.push(path.to_string());
                }
            }
        }

        matched.sort();
        Ok(matched)
    }

    /// Return index status information.
    pub fn index_status(&self) -> Result<String> {
        let status = index::updater::check_index_status(&self.root, &self.index_path)?;
        let status_str = match &status {
            index::updater::IndexStatus::Fresh => "fresh".to_string(),
            index::updater::IndexStatus::Stale { changed_files } => {
                format!("stale ({} changed files)", changed_files.len())
            }
            index::updater::IndexStatus::NeedsFullBuild => "no index".to_string(),
        };

        let index_info = if self.index_path.exists() {
            let meta = std::fs::metadata(&self.index_path).ok();
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let reader = index::reader::IndexReader::open(&self.index_path).ok();
            let file_count = reader.map(|r| r.file_count()).unwrap_or(0);
            format!(
                "Status: {}\nIndexed files: {}\nIndex size: {} bytes\nIndex path: {}",
                status_str,
                file_count,
                size,
                self.index_path.display()
            )
        } else {
            format!(
                "Status: {}\nIndex path: {}",
                status_str,
                self.index_path.display()
            )
        };
        Ok(index_info)
    }
}

/// Start the MCP server (stdio transport).
pub fn start_mcp_server(xg: Xgrep) {
    mcp::start(xg);
}

fn resolve_index_path(root: &Path) -> Result<PathBuf> {
    let local = root.join(".xgrep").join("index");
    if local.exists() {
        return Ok(local);
    }
    let hash = xxhash_rust::xxh64::xxh64(root.to_string_lossy().as_bytes(), 0);
    let cache_dir = dirs_next::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("xgrep")
        .join(format!("{:016x}", hash));
    Ok(cache_dir.join("index"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_index_path_prefers_local() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Create .xgrep/index
        std::fs::create_dir_all(root.join(".xgrep")).unwrap();
        std::fs::write(root.join(".xgrep/index"), "dummy").unwrap();

        let path = resolve_index_path(root).unwrap();
        assert!(path.ends_with(".xgrep/index"));
    }

    #[test]
    fn test_resolve_index_path_falls_back_to_cache() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // No .xgrep/index exists

        let path = resolve_index_path(root).unwrap();
        assert!(path.to_string_lossy().contains("xgrep"));
        assert!(path.ends_with("index"));
    }

    /// Regression test for GitHub Issue #15:
    /// When xgrep root is a subdirectory of the git repository root,
    /// --fresh search must not double the path (e.g., /repo/sub/sub/file).
    #[test]
    fn test_fresh_search_in_git_subdirectory_no_path_doubling() {
        use std::process::Command;

        let dir = tempfile::tempdir().unwrap();
        let git_root = dir.path();

        // Initialize git repo at top level
        Command::new("git")
            .args(["init"])
            .current_dir(git_root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(git_root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "test"])
            .current_dir(git_root)
            .output()
            .unwrap();

        // Create subdirectory with a file
        let sub = git_root.join("subdir");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("hello.rs"), "pub fn hello() { }").unwrap();

        // Initial commit
        Command::new("git")
            .args(["add", "."])
            .current_dir(git_root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(git_root)
            .output()
            .unwrap();

        // Build index rooted at the subdirectory (not git root)
        let xg = Xgrep::open_local(&sub).unwrap();
        xg.build_index().unwrap();

        // Modify the file to make index stale
        std::fs::write(sub.join("hello.rs"), "pub fn hello_world() { }").unwrap();

        // Search with fresh=true — this is the scenario that caused path doubling
        let opts = SearchOptions {
            fresh: true,
            ..Default::default()
        };
        let results = xg.search("hello_world", &opts).unwrap();

        // Must find the changed content (not fail with file-not-found)
        assert!(
            !results.is_empty(),
            "fresh search in git subdirectory should find changed file content"
        );
        // Path must be relative to xgrep root, not contain the subdirectory prefix twice
        for r in &results {
            assert!(
                !r.file.contains("subdir/subdir"),
                "path should not be doubled: got '{}'",
                r.file
            );
        }
    }

    /// Regression test: --fresh with --changed in a git subdirectory.
    /// Ensures search_changed also uses correct paths.
    #[test]
    fn test_changed_search_in_git_subdirectory() {
        use std::process::Command;

        let dir = tempfile::tempdir().unwrap();
        let git_root = dir.path();

        Command::new("git")
            .args(["init"])
            .current_dir(git_root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(git_root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "test"])
            .current_dir(git_root)
            .output()
            .unwrap();

        let sub = git_root.join("pkg");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("lib.rs"), "fn original() {}").unwrap();

        Command::new("git")
            .args(["add", "."])
            .current_dir(git_root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(git_root)
            .output()
            .unwrap();

        // Modify the file (uncommitted change)
        std::fs::write(sub.join("lib.rs"), "fn modified_unique_marker() {}").unwrap();

        // Search changed files from subdirectory root
        let xg = Xgrep::open_local(&sub).unwrap();
        xg.build_index().unwrap();

        let opts = SearchOptions {
            changed_only: true,
            ..Default::default()
        };
        let results = xg.search("modified_unique_marker", &opts).unwrap();

        assert!(
            !results.is_empty(),
            "--changed search in subdirectory should find modified content"
        );
        for r in &results {
            assert!(
                !r.file.contains("pkg/pkg"),
                "path should not be doubled: got '{}'",
                r.file
            );
        }
    }

    #[test]
    fn test_find_files_glob() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn hello() {}").unwrap();
        std::fs::write(root.join("src/util.py"), "def hello(): pass").unwrap();
        std::fs::write(root.join("README.md"), "# readme").unwrap();

        let xg = Xgrep::open_local(root).unwrap();
        xg.build_index().unwrap();

        let rs_files = xg.find_files("*.rs").unwrap();
        assert_eq!(rs_files.len(), 2);
        assert!(rs_files.iter().all(|f| f.ends_with(".rs")));

        let py_files = xg.find_files("*.py").unwrap();
        assert_eq!(py_files.len(), 1);
        assert_eq!(py_files[0], "src/util.py");

        let md_files = xg.find_files("*.md").unwrap();
        assert_eq!(md_files.len(), 1);
    }

    #[test]
    fn test_find_files_substring() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/config.rs"), "// config").unwrap();
        std::fs::write(root.join("src/app_config.toml"), "key = 1").unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();

        let xg = Xgrep::open_local(root).unwrap();
        xg.build_index().unwrap();

        let config_files = xg.find_files("config").unwrap();
        assert_eq!(config_files.len(), 2);
        assert!(config_files.iter().all(|f| f.contains("config")));
    }

    #[test]
    fn test_find_files_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        std::fs::write(root.join("hello.rs"), "fn hello() {}").unwrap();

        let xg = Xgrep::open_local(root).unwrap();
        xg.build_index().unwrap();

        let results = xg.find_files("*.py").unwrap();
        assert!(results.is_empty());

        let results = xg.find_files("nonexistent").unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_find_files_case_insensitive_substring() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        std::fs::write(root.join("Makefile"), "all:").unwrap();
        std::fs::write(root.join("makefile.bak"), "old").unwrap();

        let xg = Xgrep::open_local(root).unwrap();
        xg.build_index().unwrap();

        let results = xg.find_files("makefile").unwrap();
        assert_eq!(results.len(), 2);
    }
}
