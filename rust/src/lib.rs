//! Ultra-fast indexed code search engine using trigram inverted index.
//!
//! # Example
//!
//! ```no_run
//! use xgrep_search::{Xgrep, SearchOptions};
//!
//! let xg = Xgrep::open(".").unwrap();
//! xg.build_index().unwrap();
//!
//! // Build options fluently — no exhaustive struct literal required.
//! let opts = SearchOptions::new()
//!     .with_file_type("rs")
//!     .with_max_count(20);
//! let results = xg.search("fn main", &opts).unwrap();
//! for r in &results {
//!     println!("{}:{}: {}", r.file, r.line_number, r.line);
//! }
//! ```
//!
//! # Limitations
//!
//! Files smaller than 3 bytes contain no trigrams and are invisible to
//! content search (they still appear in `--find` results). This is an
//! intentional index design trade-off.

pub(crate) mod candidates;
pub mod error;
pub(crate) mod filetype;
pub(crate) mod git;
pub(crate) mod globfilter;
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

pub use error::{Result, XgrepError};
pub use filetype::extensions_for_type;
pub use filetype::list_all_types;
pub use search::SearchResult;

/// Returns true if the pattern contains an uppercase ASCII letter.
///
/// Used to implement smart-case: a pattern with no uppercase letters is
/// searched case-insensitively unless the caller forces sensitivity.
/// In regex mode, characters that form an escape sequence (e.g. `\W`, `\D`)
/// are not treated as uppercase literals. `\\D` (escaped backslash followed
/// by `D`) does count.
///
/// Non-ASCII letters never count as uppercase, matching the engine's
/// ASCII-only case folding. In regex mode a trailing backslash is treated as
/// an incomplete escape and contributes no uppercase.
///
/// # Examples
///
/// ```
/// use xgrep_search::pattern_has_uppercase;
/// assert!(!pattern_has_uppercase("hello", false));
/// assert!(pattern_has_uppercase("Hello", false));
/// assert!(!pattern_has_uppercase(r"\W+foo", true));
/// ```
pub fn pattern_has_uppercase(pattern: &str, regex: bool) -> bool {
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if regex && bytes[i] == b'\\' {
            // Skip the escape sequence (backslash + next byte)
            i += 2;
            continue;
        }
        if bytes[i].is_ascii_uppercase() {
            return true;
        }
        i += 1;
    }
    false
}

/// Return git changed files (unstaged + staged) relative to the given root.
///
/// Returns paths relative to `root`. Includes unstaged changes, staged changes,
/// and untracked files. Returns an error if `root` is not inside a git repository.
pub fn git_changed_files(root: &Path) -> Result<Vec<PathBuf>> {
    if !git::is_git_repo(root) {
        return Err(XgrepError::NotGitRepo);
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
    /// Match only at word boundaries (wraps the pattern in `\b(?:...)\b`).
    /// Note: enabling this always runs the regex engine, even for literal patterns.
    pub word: bool,
    /// Include/exclude result paths by glob (ripgrep -g compatible).
    /// Prefix a glob with `!` to exclude. Empty = no filtering.
    pub globs: Vec<String>,
}

impl SearchOptions {
    /// Create default search options (case-sensitive literal search, no filters).
    ///
    /// # Examples
    ///
    /// ```
    /// use xgrep_search::SearchOptions;
    /// let opts = SearchOptions::new()
    ///     .with_case_insensitive(true)
    ///     .with_file_type("rs")
    ///     .with_max_count(10);
    /// assert!(opts.case_insensitive);
    /// assert_eq!(opts.file_type.as_deref(), Some("rs"));
    /// assert_eq!(opts.max_count, Some(10));
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// Set case-insensitive search (ASCII folding only).
    pub fn with_case_insensitive(mut self, value: bool) -> Self {
        self.case_insensitive = value;
        self
    }

    /// Treat the pattern as a regex instead of a literal string.
    pub fn with_regex(mut self, value: bool) -> Self {
        self.regex = value;
        self
    }

    /// Filter results by file type (e.g. `"rs"`, `"py"`, `"js"`).
    pub fn with_file_type(mut self, file_type: impl Into<String>) -> Self {
        self.file_type = Some(file_type.into());
        self
    }

    /// Limit the number of results returned.
    pub fn with_max_count(mut self, max: usize) -> Self {
        self.max_count = Some(max);
        self
    }

    /// Match only at word boundaries (always runs the regex engine).
    pub fn with_word(mut self, value: bool) -> Self {
        self.word = value;
        self
    }

    /// Add an include/exclude glob (prefix with `!` to exclude). Repeatable.
    pub fn with_glob(mut self, glob: impl Into<String>) -> Self {
        self.globs.push(glob.into());
        self
    }

    /// Restrict the search to files with uncommitted git changes.
    pub fn with_changed_only(mut self, value: bool) -> Self {
        self.changed_only = value;
        self
    }

    /// Check index freshness and use hybrid search for changed files.
    pub fn with_fresh(mut self, value: bool) -> Self {
        self.fresh = value;
        self
    }

    /// Only search files changed within a time duration (e.g. `"1h"`, `"2d"`, `"3.commits"`).
    pub fn with_since(mut self, since: impl Into<String>) -> Self {
        self.since = Some(since.into());
        self
    }

    /// Filter results by path substring match (e.g. `"src/auth"`, `"tests/"`).
    pub fn with_path_pattern(mut self, pattern: impl Into<String>) -> Self {
        self.path_pattern = Some(pattern.into());
        self
    }
}

/// Configuration for the search engine.
///
/// Controls runtime behavior such as suppressing stderr output.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// Suppress diagnostic messages (warnings, progress) on stderr.
    /// Set to `true` when running as an MCP server or in any context
    /// where stderr output is undesirable.
    pub quiet: bool,
}

/// Index freshness state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexState {
    /// Index is up to date.
    Fresh,
    /// Index exists but some files changed since the last build.
    Stale {
        /// Number of files changed since the last index build.
        changed_files: usize,
    },
    /// No index has been built yet.
    Missing,
}

/// Structured index status, returned by [`Xgrep::index_status`].
///
/// The [`Display`](std::fmt::Display) impl renders the human-readable status
/// text used by the `xg status` CLI command.
///
/// # Examples
///
/// ```no_run
/// use xgrep_search::{Xgrep, IndexState};
///
/// let xg = Xgrep::open(".").unwrap();
/// let info = xg.index_status().unwrap();
/// match info.state {
///     IndexState::Fresh => println!("up to date ({} files)", info.indexed_files),
///     IndexState::Stale { changed_files } => println!("{} files changed", changed_files),
///     IndexState::Missing => println!("no index at {}", info.index_path.display()),
/// }
/// ```
#[derive(Debug, Clone)]
pub struct IndexStatusInfo {
    /// Freshness state of the index.
    pub state: IndexState,
    /// Number of files in the index (0 if missing).
    pub indexed_files: usize,
    /// Index file size in bytes (0 if missing).
    pub index_size_bytes: u64,
    /// Path to the index file.
    pub index_path: PathBuf,
}

impl std::fmt::Display for IndexStatusInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let status_str = match &self.state {
            IndexState::Fresh => "fresh".to_string(),
            IndexState::Stale { changed_files } => {
                format!("stale ({} changed files)", changed_files)
            }
            IndexState::Missing => "no index".to_string(),
        };
        if matches!(self.state, IndexState::Missing) {
            write!(
                f,
                "Status: {}\nIndex path: {}",
                status_str,
                self.index_path.display()
            )
        } else {
            write!(
                f,
                "Status: {}\nIndexed files: {}\nIndex size: {} bytes\nIndex path: {}",
                status_str,
                self.indexed_files,
                self.index_size_bytes,
                self.index_path.display()
            )
        }
    }
}

/// Main entry point for the search engine.
///
/// Use `open()` to specify a directory, then `search()` to execute queries.
/// Index auto-build, freshness checks, and hybrid search are handled internally.
pub struct Xgrep {
    root: PathBuf,
    index_path: PathBuf,
    config: Config,
}

impl Xgrep {
    /// Open a directory. Index path is auto-resolved (`.xgrep/index` or `~/.cache/xgrep/<hash>/index`).
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let index_path = resolve_index_path(&root)?;
        Ok(Self {
            root,
            index_path,
            config: Config::default(),
        })
    }

    /// Open with a local index (.xgrep/) explicitly.
    pub fn open_local(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let index_path = root.join(".xgrep").join("index");
        Ok(Self {
            root,
            index_path,
            config: Config::default(),
        })
    }

    /// Set the configuration using builder pattern.
    pub fn with_config(mut self, config: Config) -> Self {
        self.config = config;
        self
    }

    /// Returns the current configuration.
    pub fn config(&self) -> &Config {
        &self.config
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

    /// Apply word-boundary wrapping. Returns the effective pattern and
    /// whether it must be executed as a regex.
    ///
    /// Callees must use the returned bool instead of `opts.regex`.
    fn effective_pattern<'a>(
        pattern: &'a str,
        opts: &SearchOptions,
    ) -> (std::borrow::Cow<'a, str>, bool) {
        if opts.word {
            let inner = if opts.regex {
                pattern.to_string()
            } else {
                regex::escape(pattern)
            };
            (format!(r"\b(?:{})\b", inner).into(), true)
        } else {
            (pattern.into(), opts.regex)
        }
    }

    /// Execute a search. Auto-build, hybrid search, and git-changed-file search are handled internally.
    pub fn search(&self, pattern: &str, opts: &SearchOptions) -> Result<Vec<SearchResult>> {
        let (pattern, regex) = Self::effective_pattern(pattern, opts);
        let pattern = pattern.as_ref();
        let mut results = if opts.changed_only || opts.since.is_some() {
            self.search_changed(pattern, regex, opts)?
        } else {
            self.search_indexed(pattern, regex, opts)?
        };

        // file_type filter
        if let Some(ref ft) = opts.file_type {
            if let Some(exts) = filetype::extensions_for_type(ft) {
                results.retain(|r| {
                    Path::new(&*r.file)
                        .extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| exts.contains(&e))
                });
            } else if !self.config.quiet {
                eprintln!("warning: unknown file type '{}', showing all results", ft);
            }
        }

        // path_pattern filter
        if let Some(ref pp) = opts.path_pattern {
            results.retain(|r| r.file.contains(pp));
        }

        // glob filter (-g)
        if !opts.globs.is_empty() {
            let filter = globfilter::GlobFilter::new(&opts.globs)?;
            results.retain(|r| filter.matches(&r.file));
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
    fn search_indexed(
        &self,
        pattern: &str,
        regex: bool,
        opts: &SearchOptions,
    ) -> Result<Vec<SearchResult>> {
        if !opts.fresh {
            // Fast path: use index as-is without freshness check
            if self.index_path.exists() {
                let reader = index::reader::IndexReader::open(&self.index_path)?;
                let results = if regex {
                    search::search_regex(
                        &reader,
                        &self.root,
                        pattern,
                        opts.case_insensitive,
                        self.config.quiet,
                    )
                } else {
                    search::search(
                        &reader,
                        &self.root,
                        pattern,
                        opts.case_insensitive,
                        self.config.quiet,
                    )
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
                if regex {
                    search::search_regex(
                        &reader,
                        &self.root,
                        pattern,
                        opts.case_insensitive,
                        self.config.quiet,
                    )
                } else {
                    search::search(
                        &reader,
                        &self.root,
                        pattern,
                        opts.case_insensitive,
                        self.config.quiet,
                    )
                }
            }
            index::updater::IndexStatus::Stale { changed_files } => {
                let reader = index::reader::IndexReader::open(&self.index_path)?;

                // Search from index (results for changed files may be stale)
                let mut index_results = if regex {
                    search::search_regex(
                        &reader,
                        &self.root,
                        pattern,
                        opts.case_insensitive,
                        self.config.quiet,
                    )?
                } else {
                    search::search(
                        &reader,
                        &self.root,
                        pattern,
                        opts.case_insensitive,
                        self.config.quiet,
                    )?
                };

                // Exclude results from changed files (may be stale data)
                let changed_set: std::collections::HashSet<String> = changed_files
                    .iter()
                    .map(|p| p.to_string_lossy().to_string())
                    .collect();
                index_results.retain(|r| !changed_set.contains(r.file.as_ref()));

                // Directly scan changed files
                let direct_results = if regex {
                    search::search_files_regex(
                        &self.root,
                        &changed_files,
                        pattern,
                        opts.case_insensitive,
                        self.config.quiet,
                    )?
                } else {
                    search::search_files(
                        &self.root,
                        &changed_files,
                        pattern,
                        opts.case_insensitive,
                        self.config.quiet,
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
                if !self.config.quiet {
                    eprintln!("[indexing...]");
                }
                self.build_index()?;
                if !self.config.quiet {
                    eprintln!("[done]");
                }

                let reader = index::reader::IndexReader::open(&self.index_path)?;
                if regex {
                    search::search_regex(
                        &reader,
                        &self.root,
                        pattern,
                        opts.case_insensitive,
                        self.config.quiet,
                    )
                } else {
                    search::search(
                        &reader,
                        &self.root,
                        pattern,
                        opts.case_insensitive,
                        self.config.quiet,
                    )
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

    /// Search specific files directly without using the index.
    ///
    /// Useful when the caller already knows which files to search (e.g., a single file path).
    pub fn search_files(
        &self,
        files: &[PathBuf],
        pattern: &str,
        opts: &SearchOptions,
    ) -> Result<Vec<SearchResult>> {
        let (pattern, regex) = Self::effective_pattern(pattern, opts);
        let pattern = pattern.as_ref();
        let mut results = if regex {
            search::search_files_regex(
                &self.root,
                files,
                pattern,
                opts.case_insensitive,
                self.config.quiet,
            )?
        } else {
            search::search_files(
                &self.root,
                files,
                pattern,
                opts.case_insensitive,
                self.config.quiet,
            )?
        };

        // glob filter (-g)
        if !opts.globs.is_empty() {
            let filter = globfilter::GlobFilter::new(&opts.globs)?;
            results.retain(|r| filter.matches(&r.file));
        }

        if let Some(max) = opts.max_count {
            results.truncate(max);
        }

        Ok(results)
    }

    /// Search only git-changed files. Returns error if not a git repository.
    fn search_changed(
        &self,
        pattern: &str,
        regex: bool,
        opts: &SearchOptions,
    ) -> Result<Vec<SearchResult>> {
        if !git::is_git_repo(&self.root) {
            return Err(XgrepError::NotGitRepo);
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

        if regex {
            search::search_files_regex(
                &self.root,
                &files,
                pattern,
                opts.case_insensitive,
                self.config.quiet,
            )
        } else {
            search::search_files(
                &self.root,
                &files,
                pattern,
                opts.case_insensitive,
                self.config.quiet,
            )
        }
    }

    /// Find files matching a glob or substring pattern.
    /// Returns a list of relative file paths from the index.
    pub fn find_files(&self, pattern: &str) -> Result<Vec<String>> {
        if !self.index_path.exists() {
            if !self.config.quiet {
                eprintln!("[indexing...]");
            }
            self.build_index()?;
            if !self.config.quiet {
                eprintln!("[done]");
            }
        }

        let reader = index::reader::IndexReader::open(&self.index_path)?;
        let file_count = reader.file_count();
        let mut matched = Vec::new();

        let is_glob = pattern.contains('*') || pattern.contains('?') || pattern.contains('[');

        if is_glob {
            let glob = glob::Pattern::new(pattern)
                .map_err(|e| XgrepError::InvalidPattern(format!("invalid glob: {}", e)))?;
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

    /// Return structured index status information.
    ///
    /// Use the [`IndexStatusInfo`] fields directly, or its
    /// [`Display`](std::fmt::Display) impl for the human-readable text.
    pub fn index_status(&self) -> Result<IndexStatusInfo> {
        let status = index::updater::check_index_status(&self.root, &self.index_path)?;
        let state = match &status {
            index::updater::IndexStatus::Fresh => IndexState::Fresh,
            index::updater::IndexStatus::Stale { changed_files } => IndexState::Stale {
                changed_files: changed_files.len(),
            },
            index::updater::IndexStatus::NeedsFullBuild => IndexState::Missing,
        };

        if self.index_path.exists() {
            let size = std::fs::metadata(&self.index_path)
                .map(|m| m.len())
                .unwrap_or(0);
            let file_count = index::reader::IndexReader::open(&self.index_path)
                .map(|r| r.file_count() as usize)
                .unwrap_or(0);
            Ok(IndexStatusInfo {
                state,
                indexed_files: file_count,
                index_size_bytes: size,
                index_path: self.index_path.clone(),
            })
        } else {
            Ok(IndexStatusInfo {
                state: IndexState::Missing,
                indexed_files: 0,
                index_size_bytes: 0,
                index_path: self.index_path.clone(),
            })
        }
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
        let dir = tempfile::tempdir().unwrap();
        let git_root = dir.path();

        // Initialize git repo at top level
        crate::git::git_cmd()
            .args(["init"])
            .current_dir(git_root)
            .output()
            .unwrap();
        crate::git::git_cmd()
            .args(["config", "user.email", "test@test.com"])
            .current_dir(git_root)
            .output()
            .unwrap();
        crate::git::git_cmd()
            .args(["config", "user.name", "test"])
            .current_dir(git_root)
            .output()
            .unwrap();

        // Create subdirectory with a file
        let sub = git_root.join("subdir");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("hello.rs"), "pub fn hello() { }").unwrap();

        // Initial commit
        crate::git::git_cmd()
            .args(["add", "."])
            .current_dir(git_root)
            .output()
            .unwrap();
        crate::git::git_cmd()
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
        let dir = tempfile::tempdir().unwrap();
        let git_root = dir.path();

        crate::git::git_cmd()
            .args(["init"])
            .current_dir(git_root)
            .output()
            .unwrap();
        crate::git::git_cmd()
            .args(["config", "user.email", "test@test.com"])
            .current_dir(git_root)
            .output()
            .unwrap();
        crate::git::git_cmd()
            .args(["config", "user.name", "test"])
            .current_dir(git_root)
            .output()
            .unwrap();

        let sub = git_root.join("pkg");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("lib.rs"), "fn original() {}").unwrap();

        crate::git::git_cmd()
            .args(["add", "."])
            .current_dir(git_root)
            .output()
            .unwrap();
        crate::git::git_cmd()
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
        assert!(
            py_files[0] == "src/util.py" || py_files[0] == "src\\util.py",
            "expected src/util.py or src\\util.py, got: {}",
            py_files[0]
        );

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

    #[test]
    fn test_git_changed_files_returns_not_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        let err = git_changed_files(dir.path()).unwrap_err();
        assert!(matches!(err, XgrepError::NotGitRepo));
    }

    #[test]
    fn test_changed_search_returns_not_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "hello").unwrap();
        let xg = Xgrep::open_local(root).unwrap();
        xg.build_index().unwrap();
        let opts = SearchOptions {
            changed_only: true,
            ..Default::default()
        };
        let err = xg.search("hello", &opts).unwrap_err();
        assert!(matches!(err, XgrepError::NotGitRepo));
    }

    #[test]
    fn test_find_files_invalid_glob_returns_invalid_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "hello").unwrap();
        let xg = Xgrep::open_local(root).unwrap();
        xg.build_index().unwrap();
        let err = xg.find_files("[invalid").unwrap_err();
        assert!(matches!(err, XgrepError::InvalidPattern(_)));
    }

    #[test]
    fn test_index_reader_invalid_magic_returns_index_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.xgrep");
        std::fs::write(&path, b"BADMxxxxxxxxxxxxxxxxxxxxxxxx").unwrap();
        match index::reader::IndexReader::open(&path) {
            Err(XgrepError::IndexError(_)) => {}
            other => panic!("expected IndexError, got {:?}", other.err()),
        }
    }

    #[test]
    fn test_index_reader_too_small_returns_index_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiny.xgrep");
        std::fs::write(&path, b"XGR").unwrap();
        match index::reader::IndexReader::open(&path) {
            Err(XgrepError::IndexError(_)) => {}
            other => panic!("expected IndexError, got {:?}", other.err()),
        }
    }

    #[test]
    fn test_search_invalid_regex_returns_invalid_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "hello").unwrap();
        let xg = Xgrep::open_local(root).unwrap();
        xg.build_index().unwrap();
        let opts = SearchOptions {
            regex: true,
            ..Default::default()
        };
        let err = xg.search("[invalid", &opts).unwrap_err();
        assert!(matches!(err, XgrepError::InvalidPattern(_)));
    }

    #[test]
    fn test_invalid_duration_returns_invalid_argument() {
        // git.rs の parse_duration が InvalidArgument を返すことを確認
        // since_files() 経由でテストする
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        crate::git::git_cmd()
            .args(["init"])
            .current_dir(root)
            .output()
            .unwrap();
        let err = git::since_files(root, "badformat").unwrap_err();
        assert!(
            matches!(err, XgrepError::InvalidArgument(_)),
            "expected InvalidArgument, got {:?}",
            err
        );
    }

    /// Regression: a 2-char pattern occurring at EOF (no trailing byte) must be found.
    /// Trigram "ab?" does not exist for the final "ab" in "xxab", but "xab" does.
    /// A second file provides a non-empty prefix candidate set so the EOF case
    /// cannot be masked by the full-scan fallback.
    #[test]
    fn test_two_char_pattern_at_eof() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // No trailing newline: "ab" is the last 2 bytes (only "xab" trigram exists)
        std::fs::write(root.join("tail.txt"), b"xxab").unwrap();
        // Provides the "ab?" prefix trigram so the prefix lookup is non-empty
        std::fs::write(root.join("other.txt"), b"abc def\n").unwrap();
        let xg = Xgrep::open_local(root).unwrap();
        xg.build_index().unwrap();
        let results = xg.search("ab", &SearchOptions::default()).unwrap();
        let files: Vec<&str> = results.iter().map(|r| r.file.as_ref()).collect();
        assert!(
            files.iter().any(|f| f.ends_with("tail.txt")),
            "2-char pattern at EOF must be found, got {:?}",
            files
        );
        assert!(
            files.iter().any(|f| f.ends_with("other.txt")),
            "2-char pattern in prefix position must be found, got {:?}",
            files
        );
    }

    /// A 2-char pattern that exists nowhere must return no results
    /// (and must not fall back to a full scan).
    #[test]
    fn test_two_char_pattern_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), b"hello world\n").unwrap();
        let xg = Xgrep::open_local(root).unwrap();
        xg.build_index().unwrap();
        let results = xg.search("zq", &SearchOptions::default()).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_options_builder() {
        let opts = SearchOptions::new()
            .with_case_insensitive(true)
            .with_regex(true)
            .with_file_type("rs")
            .with_max_count(10)
            .with_word(true)
            .with_glob("*.rs")
            .with_glob("!*_test.rs");
        assert!(opts.case_insensitive);
        assert!(opts.regex);
        assert_eq!(opts.file_type.as_deref(), Some("rs"));
        assert_eq!(opts.max_count, Some(10));
        assert!(opts.word);
        assert_eq!(opts.globs.len(), 2);
    }

    #[test]
    fn test_index_status_structured() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "hello\n").unwrap();
        let xg = Xgrep::open_local(root).unwrap();
        xg.build_index().unwrap();
        let info = xg.index_status().unwrap();
        assert_eq!(info.state, IndexState::Fresh);
        assert_eq!(info.indexed_files, 1);
        assert!(info.index_size_bytes > 0);
        // Display preserves the human-readable format
        let text = info.to_string();
        assert!(text.contains("Status: fresh"));
        assert!(text.contains("Indexed files: 1"));
    }

    #[test]
    fn test_pattern_has_uppercase_literal() {
        assert!(!pattern_has_uppercase("hello", false));
        assert!(pattern_has_uppercase("Hello", false));
        assert!(!pattern_has_uppercase("123!", false));
        // Literal mode: backslash is a literal character, W counts as uppercase
        assert!(pattern_has_uppercase(r"\W", false));
    }

    #[test]
    fn test_pattern_has_uppercase_regex_skips_escapes() {
        // \W is a regex escape, not an uppercase literal
        assert!(!pattern_has_uppercase(r"\W+foo", true));
        assert!(pattern_has_uppercase(r"\W+Foo", true));
        assert!(!pattern_has_uppercase(r"foo\d", true));
        // Escaped backslash followed by uppercase: \\ is literal backslash, D is uppercase
        assert!(pattern_has_uppercase(r"\\D", true));
        // Trailing backslash is an incomplete escape and contributes no uppercase
        assert!(!pattern_has_uppercase("foo\\", true));
    }

    /// Smart-case behavior verified at the library level via explicit flag:
    /// the CLI maps (no -i, no -s, all-lowercase pattern) to case_insensitive=true.
    #[test]
    fn test_word_boundary_literal() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "cat\nconcatenate\nthe cat, sat\n").unwrap();
        let xg = Xgrep::open_local(root).unwrap();
        xg.build_index().unwrap();
        let opts = SearchOptions {
            word: true,
            ..Default::default()
        };
        let results = xg.search("cat", &opts).unwrap();
        // Matches line 1 ("cat") and line 3 ("the cat, sat"), not "concatenate"
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.line != "concatenate"));
    }

    #[test]
    fn test_word_boundary_with_regex_metachars_in_literal() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "foo.bar baz\nfooxbar\n").unwrap();
        let xg = Xgrep::open_local(root).unwrap();
        xg.build_index().unwrap();
        let opts = SearchOptions {
            word: true,
            ..Default::default()
        };
        // "." must be escaped: literal "foo.bar" must not match "fooxbar"
        let results = xg.search("foo.bar", &opts).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].line.contains("foo.bar"));
    }

    #[test]
    fn test_search_with_glob_filter() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "needle\n").unwrap();
        std::fs::write(root.join("src/b.py"), "needle\n").unwrap();
        let xg = Xgrep::open_local(root).unwrap();
        xg.build_index().unwrap();
        let opts = SearchOptions {
            globs: vec!["*.rs".to_string()],
            ..Default::default()
        };
        let results = xg.search("needle", &opts).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].file.ends_with("a.rs"));
    }

    #[test]
    fn test_search_case_insensitive_finds_mixed_case() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.rs"), "fn HandleAuth() {}\n").unwrap();
        let xg = Xgrep::open_local(root).unwrap();
        xg.build_index().unwrap();
        let opts = SearchOptions {
            case_insensitive: true,
            ..Default::default()
        };
        let results = xg.search("handleauth", &opts).unwrap();
        assert_eq!(results.len(), 1);
    }

    /// -g must be honored on the explicit-file search path too.
    #[test]
    fn test_search_files_applies_glob_filter() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.py"), "needle\n").unwrap();
        let xg = Xgrep::open_local(root).unwrap();
        xg.build_index().unwrap();
        let opts = SearchOptions {
            globs: vec!["*.rs".to_string()],
            ..Default::default()
        };
        let results = xg
            .search_files(&[std::path::PathBuf::from("a.py")], "needle", &opts)
            .unwrap();
        assert!(results.is_empty(), "-g '*.rs' must filter out a .py file");
    }
}
