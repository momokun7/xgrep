//! trigram逆引きインデックスによる高速コード検索エンジン。
//!
//! # Example
//!
//! ```no_run
//! use xgrep::{Xgrep, SearchOptions};
//!
//! let xg = Xgrep::open(".").unwrap();
//! xg.build_index().unwrap();
//! let results = xg.search("fn main", &SearchOptions::default()).unwrap();
//! for r in &results {
//!     println!("{}:{}: {}", r.file, r.line_number, r.line);
//! }
//! ```

pub(crate) mod filetype;
pub(crate) mod git;
pub mod index;
pub(crate) mod mcp;
pub mod mcp_server;
pub(crate) mod mcp_tools;
pub mod output;
pub mod search;
pub(crate) mod trigram;
pub(crate) mod trigram_query;

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

pub use search::SearchResult;

/// 検索オプション。
///
/// `Default::default()` で固定文字列・case-sensitive・フィルタなしの検索になる。
#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    pub case_insensitive: bool,
    pub regex: bool,
    pub file_type: Option<String>,
    pub max_count: Option<usize>,
    pub changed_only: bool,
    pub since: Option<String>,
    pub path_pattern: Option<String>,
}

/// 検索エンジンのメインエントリポイント。
///
/// `open()` でディレクトリを指定し、`search()` で検索を実行する。
/// インデックスの自動ビルド・鮮度チェック・ハイブリッド検索は内部で自動処理される。
pub struct Xgrep {
    root: PathBuf,
    index_path: PathBuf,
}

impl Xgrep {
    /// ディレクトリを開く。インデックスパスは自動解決（.xgrep/index → ~/.cache/xgrep/&lt;hash&gt;/index）
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let index_path = resolve_index_path(&root)?;
        Ok(Self { root, index_path })
    }

    /// ローカルインデックス(.xgrep/)を明示指定して開く
    pub fn open_local(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let index_path = root.join(".xgrep").join("index");
        Ok(Self { root, index_path })
    }

    /// ルートディレクトリのパスを返す
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// インデックスファイルのパスを返す
    pub fn index_path(&self) -> &Path {
        &self.index_path
    }

    /// インデックスをビルド（またはリビルド）
    pub fn build_index(&self) -> Result<()> {
        if let Some(parent) = self.index_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let cache = index::builder::cache_path_for(&self.index_path);
        index::builder::build_index_with_cache(&self.root, &self.index_path, Some(&cache))?;
        index::updater::save_meta(&self.root, &self.index_path)?;
        Ok(())
    }

    /// 検索を実行。インデックスの自動ビルド・ハイブリッド検索・Git変更ファイル検索を内部で処理する。
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

    /// インデックスベースの検索。IndexStatusに応じてハイブリッド検索・自動ビルドを行う。
    fn search_indexed(&self, pattern: &str, opts: &SearchOptions) -> Result<Vec<SearchResult>> {
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

    /// Git変更ファイルのみを対象に検索する。Gitリポジトリでない場合はエラー。
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
