pub mod filetype;
pub mod git;
pub mod index;
pub mod output;
pub mod search;
pub mod trigram;
pub mod trigram_query;

use std::path::{Path, PathBuf};

use anyhow::Result;

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
    /// ディレクトリを開く。インデックスパスは自動解決（.xgrep/index → ~/.cache/xgrep/<hash>/index）
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

    /// 検索を実行（Task 1: インデックス検索のみ、ハイブリッドはTask 2で追加）
    pub fn search(&self, pattern: &str, opts: &SearchOptions) -> Result<Vec<SearchResult>> {
        let reader = index::reader::IndexReader::open(&self.index_path)?;
        let mut results = if opts.regex {
            search::search_regex(&reader, &self.root, pattern, opts.case_insensitive)?
        } else {
            search::search(&reader, &self.root, pattern, opts.case_insensitive)?
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

        // max_count
        if let Some(max) = opts.max_count {
            results.truncate(max);
        }

        Ok(results)
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
