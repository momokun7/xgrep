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

pub use search::SearchResult;

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
            } else {
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
                return if opts.regex {
                    search::search_regex(&reader, &self.root, pattern, opts.case_insensitive)
                } else {
                    search::search(&reader, &self.root, pattern, opts.case_insensitive)
                };
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

                // インデックスから検索（変更ファイルの結果は古い可能性あり）
                let mut index_results = if opts.regex {
                    search::search_regex(&reader, &self.root, pattern, opts.case_insensitive)?
                } else {
                    search::search(&reader, &self.root, pattern, opts.case_insensitive)?
                };

                // 変更ファイルの結果を除外（古いデータの可能性があるため）
                let changed_set: std::collections::HashSet<String> = changed_files
                    .iter()
                    .map(|p| p.to_string_lossy().to_string())
                    .collect();
                index_results.retain(|r| !changed_set.contains(&r.file));

                // 変更ファイルを直接スキャン
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

                // マージしてソート、重複除去
                index_results.extend(direct_results);
                index_results
                    .sort_by(|a, b| a.file.cmp(&b.file).then(a.line_number.cmp(&b.line_number)));
                index_results.dedup_by(|a, b| a.file == b.file && a.line_number == b.line_number);
                Ok(index_results)
            }
            index::updater::IndexStatus::NeedsFullBuild => {
                // インデックスなし、フルビルド
                eprintln!("[indexing...]");
                self.build_index()?;
                eprintln!("[done]");

                let reader = index::reader::IndexReader::open(&self.index_path)?;
                if opts.regex {
                    search::search_regex(&reader, &self.root, pattern, opts.case_insensitive)
                } else {
                    search::search(&reader, &self.root, pattern, opts.case_insensitive)
                }
            }
        }
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

    /// インデックスのステータス情報を返す
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
}
