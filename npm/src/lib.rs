use napi::bindgen_prelude::*;
use napi_derive::napi;

/// Search result returned from xgrep.
#[napi(object)]
pub struct SearchResult {
    /// File path relative to root.
    pub file: String,
    /// Line number (1-based).
    pub line_number: u32,
    /// The matching line content.
    pub line: String,
}

/// Search options for xgrep.
#[napi(object)]
pub struct SearchOptions {
    /// Case-insensitive search (ASCII-only).
    pub case_insensitive: Option<bool>,
    /// Treat pattern as regex.
    pub regex: Option<bool>,
    /// Filter by file type (e.g., "rs", "py", "js").
    pub file_type: Option<String>,
    /// Maximum number of results.
    pub max_count: Option<u32>,
    /// Only search git-changed files.
    pub changed_only: Option<bool>,
    /// Search files changed within duration (e.g., "1h", "2d").
    pub since: Option<String>,
    /// Filter by path substring.
    pub path_pattern: Option<String>,
    /// Check index freshness before searching.
    pub fresh: Option<bool>,
}

/// Ultra-fast indexed code search engine.
#[napi]
pub struct Xgrep {
    inner: xgrep_search::Xgrep,
}

#[napi]
impl Xgrep {
    /// Open a directory for searching. Index location is auto-resolved.
    #[napi(factory)]
    pub fn open(root: String) -> Result<Xgrep> {
        let inner = xgrep_search::Xgrep::open(&root)
            .map_err(|e| Error::from_reason(e.to_string()))?;
        Ok(Xgrep { inner })
    }

    /// Open a directory with local index storage (.xgrep/ in the project root).
    #[napi(factory)]
    pub fn open_local(root: String) -> Result<Xgrep> {
        let inner = xgrep_search::Xgrep::open_local(&root)
            .map_err(|e| Error::from_reason(e.to_string()))?;
        Ok(Xgrep { inner })
    }

    /// Build or rebuild the search index.
    #[napi]
    pub fn build_index(&self) -> Result<()> {
        self.inner
            .build_index()
            .map_err(|e| Error::from_reason(e.to_string()))
    }

    /// Search for a pattern in the indexed codebase.
    #[napi]
    pub fn search(
        &self,
        pattern: String,
        opts: Option<SearchOptions>,
    ) -> Result<Vec<SearchResult>> {
        let rust_opts = to_rust_opts(opts);
        let results = self
            .inner
            .search(&pattern, &rust_opts)
            .map_err(|e| Error::from_reason(e.to_string()))?;
        Ok(results.into_iter().map(to_js_result).collect())
    }

    /// Get the current index status.
    #[napi]
    pub fn index_status(&self) -> Result<String> {
        self.inner
            .index_status()
            .map_err(|e| Error::from_reason(e.to_string()))
    }

    /// Get the root directory path.
    #[napi(getter)]
    pub fn root(&self) -> String {
        self.inner.root().to_string_lossy().to_string()
    }

    /// Get the index file path.
    #[napi(getter)]
    pub fn index_path(&self) -> String {
        self.inner.index_path().to_string_lossy().to_string()
    }
}

fn to_rust_opts(opts: Option<SearchOptions>) -> xgrep_search::SearchOptions {
    match opts {
        None => xgrep_search::SearchOptions::default(),
        Some(o) => xgrep_search::SearchOptions {
            case_insensitive: o.case_insensitive.unwrap_or(false),
            regex: o.regex.unwrap_or(false),
            file_type: o.file_type,
            max_count: o.max_count.map(|n| n as usize),
            changed_only: o.changed_only.unwrap_or(false),
            since: o.since,
            path_pattern: o.path_pattern,
            fresh: o.fresh.unwrap_or(false),
        },
    }
}

fn to_js_result(r: xgrep_search::search::SearchResult) -> SearchResult {
    SearchResult {
        file: r.file,
        line_number: r.line_number as u32,
        line: r.line,
    }
}
