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
pub mod cli;
pub(crate) mod defs;
pub mod error;
pub(crate) mod filetype;
pub(crate) mod fsinfo;
pub(crate) mod git;
pub(crate) mod globfilter;
pub mod hints;
pub(crate) mod history;
pub(crate) mod index;
pub(crate) mod mcp;
pub(crate) mod mcp_render;
pub(crate) mod mcp_tools;
pub mod output;
pub(crate) mod rank;
pub mod search;
pub(crate) mod session;
pub(crate) mod syntax;
pub(crate) mod trigram;
pub(crate) mod trigram_query;
pub(crate) mod watch;

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
pub use history::{CommitMatch, DiffMatch, DiffSide};
pub use search::{FileMatches, OutputMode, SearchResult};
pub use watch::WatchStatus;

/// Returns true if the pattern contains an uppercase letter.
///
/// Used to implement smart-case: a pattern with no uppercase letters is
/// searched case-insensitively unless the caller forces sensitivity.
/// In regex mode, characters that form an escape sequence (e.g. `\W`, `\D`)
/// are not treated as uppercase literals. `\\D` (escaped backslash followed
/// by `D`) does count.
///
/// Non-ASCII uppercase letters (e.g. `Ä`) count too, so such patterns stay
/// case-sensitive: literal search only folds ASCII, and the regex engine
/// would otherwise fold them with Unicode rules. In regex mode a trailing
/// backslash is treated as an incomplete escape and contributes no uppercase.
///
/// # Examples
///
/// ```
/// use xgrep_search::pattern_has_uppercase;
/// assert!(!pattern_has_uppercase("hello", false));
/// assert!(pattern_has_uppercase("Hello", false));
/// assert!(pattern_has_uppercase("Äbc", false));
/// assert!(!pattern_has_uppercase(r"\W+foo", true));
/// ```
pub fn pattern_has_uppercase(pattern: &str, regex: bool) -> bool {
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if regex && c == '\\' {
            // Skip the escape sequence (backslash + next char)
            chars.next();
            continue;
        }
        if c.is_uppercase() {
            return true;
        }
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
    /// File-level AND: a file is reported only if it also matches every one
    /// of these patterns (anywhere in the file). Interpreted like the main
    /// pattern (`regex`, `word`, `case_insensitive`). Used to narrow index
    /// candidates by posting-list intersection.
    pub and_patterns: Vec<String>,
    /// File-level NOT: a file is skipped if it matches any of these patterns.
    /// Interpreted like the main pattern. Verified on file contents.
    pub not_patterns: Vec<String>,
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

    /// Require files to also match `pattern` (file-level AND). Repeatable.
    pub fn with_and(mut self, pattern: impl Into<String>) -> Self {
        self.and_patterns.push(pattern.into());
        self
    }

    /// Skip files that match `pattern` (file-level NOT). Repeatable.
    pub fn with_not(mut self, pattern: impl Into<String>) -> Self {
        self.not_patterns.push(pattern.into());
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

/// Index freshness observed by a search, returned by
/// [`Xgrep::last_search_freshness`].
///
/// Default searches check which files changed since the index was built
/// (via `git status`, or by comparing mtimes outside git) and read those
/// files directly, so results include new code even before the index is
/// rebuilt. The [`Display`](std::fmt::Display) impl renders a short note
/// such as `index stale: 3 files rescanned`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Freshness {
    /// Files that differ from the index (modified, deleted, or new).
    pub changed_files: usize,
    /// Files read directly from disk and merged into the results.
    pub rescanned_files: usize,
    /// True when too many files changed to rescan them all; the results
    /// then come from the stale index alone and may be incomplete.
    pub truncated: bool,
}

impl Freshness {
    /// True if any file differs from the index.
    pub fn is_stale(&self) -> bool {
        self.changed_files > 0 || self.truncated
    }
}

fn files(n: usize) -> String {
    if n == 1 {
        "1 file".to_string()
    } else {
        format!("{n} files")
    }
}

impl std::fmt::Display for Freshness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if !self.is_stale() {
            write!(f, "index: fresh")
        } else if self.truncated {
            write!(
                f,
                "index stale: {} changed, not rescanned (rebuild pending)",
                files(self.changed_files)
            )
        } else {
            write!(f, "index stale: {} rescanned", files(self.rescanned_files))
        }
    }
}

/// Maximum number of changed files read directly by a default search.
/// Beyond this, the search uses the index alone, warns, and relies on the
/// background rebuild (`--fresh` rebuilds synchronously instead).
const OVERLAY_MAX_FILES: usize = 500;

/// Default time budget for the freshness probe of a default search.
/// Override with `XGREP_OVERLAY_TIMEOUT_MS`.
const OVERLAY_PROBE_BUDGET_MS: u64 = 200;

/// After a probe exceeds its budget, skip probing for this long.
const OVERLAY_PROBE_BACKOFF_SECS: u64 = 600;

fn overlay_probe_budget() -> std::time::Duration {
    let ms = std::env::var("XGREP_OVERLAY_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(OVERLAY_PROBE_BUDGET_MS);
    std::time::Duration::from_millis(ms)
}

/// Default reuse window for a persisted probe result (see
/// `index::updater::compute_overlay_cached`), set with
/// `XGREP_PROBE_CACHE_MS`.
///
/// Off by default: a cached result hides in-place edits made within the
/// window (search, edit, search again within a second is exactly what
/// coding agents do), and nothing cheaper than a stat per file can detect
/// those. Scripts issuing many searches against a tree they do not modify
/// can opt in (e.g. `XGREP_PROBE_CACHE_MS=1000`).
const PROBE_CACHE_TTL_MS: u64 = 0;

fn probe_cache_ttl() -> std::time::Duration {
    let ms = std::env::var("XGREP_PROBE_CACHE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(PROBE_CACHE_TTL_MS);
    std::time::Duration::from_millis(ms)
}

/// Skip background rebuilds if one was attempted within this many seconds.
const REBUILD_COOLDOWN_SECS: u64 = 30;

/// When true, background rebuilds spawn `<current_exe> init` as a detached
/// process. Only the `xg` CLI sets this: in library contexts (Python, Node)
/// `current_exe()` is the host interpreter, which must never be launched.
static SPAWN_REBUILD_PROCESS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Choose how background index rebuilds run (process-wide).
///
/// `true` spawns a detached `xg init` child (the current executable must be
/// the `xg` binary); used by short-lived CLI invocations so the search can
/// exit immediately. `false` (default) rebuilds on a background thread
/// inside the current process, which is what libraries and the MCP server
/// want: no foreign processes and no zombies.
#[doc(hidden)]
pub fn set_background_rebuild_process(enabled: bool) {
    SPAWN_REBUILD_PROCESS.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

/// True if the freshness overlay is disabled via `XGREP_NO_OVERLAY=1`.
fn overlay_disabled() -> bool {
    std::env::var_os("XGREP_NO_OVERLAY").is_some_and(|v| v == "1" || v == "true")
}

/// Build/update the index at `index_path` and record its metadata.
///
/// Metadata is saved even when the corpus fingerprint matched (no rebuild),
/// so the "verified" timestamp advances and freshness checks stay cheap.
fn rebuild_index_files(root: &Path, index_path: &Path) -> Result<bool> {
    let started = std::time::SystemTime::now();
    let cache = index::cache::cache_path_for(index_path);
    let rebuilt = index::builder::build_index_with_cache(root, index_path, Some(&cache))?;
    index::updater::save_meta(root, index_path, Some(started))?;
    Ok(rebuilt)
}

/// Main entry point for the search engine.
///
/// Use `open()` to specify a directory, then `search()` to execute queries.
/// Index auto-build, freshness checks, and hybrid search are handled internally.
pub struct Xgrep {
    root: PathBuf,
    index_path: PathBuf,
    config: Config,
    /// Freshness observed by the most recent index search.
    last_freshness: std::sync::Mutex<Option<Freshness>>,
    /// Set while an in-process background rebuild thread is running.
    rebuild_running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Serializes index builds within this process (explicit builds,
    /// background threads and the watcher), so they wait for each other
    /// instead of failing on the build lock file.
    build_mutex: std::sync::Arc<std::sync::Mutex<()>>,
    /// File watcher, when enabled with [`Xgrep::start_watching`].
    watcher: std::sync::Mutex<Option<watch::WatchHandle>>,
    /// Polling mode ([`WatchStatus::Polling`]): `start_watching` on a
    /// filesystem without reliable change events.
    polling: std::sync::atomic::AtomicBool,
    /// Probe result reused for `watch::remote_probe_ttl()` in polling mode.
    remote_probe: std::sync::Mutex<Option<(std::time::Instant, index::updater::Overlay)>>,
    /// Where the last search's overlay came from ("probe", "probe-cache",
    /// "watcher", or "none"); for diagnostics and tests.
    last_overlay_source: std::sync::Mutex<&'static str>,
}

impl Xgrep {
    /// Open a directory. Index path is auto-resolved (`.xgrep/index` or `~/.cache/xgrep/<hash>/index`).
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let index_path = resolve_index_path(&root)?;
        Ok(Self::with_paths(root, index_path))
    }

    /// Open with a local index (.xgrep/) explicitly.
    pub fn open_local(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let index_path = root.join(".xgrep").join("index");
        Ok(Self::with_paths(root, index_path))
    }

    fn with_paths(root: PathBuf, index_path: PathBuf) -> Self {
        Self {
            root,
            index_path,
            config: Config::default(),
            last_freshness: std::sync::Mutex::new(None),
            rebuild_running: Default::default(),
            build_mutex: Default::default(),
            watcher: std::sync::Mutex::new(None),
            polling: Default::default(),
            remote_probe: std::sync::Mutex::new(None),
            last_overlay_source: std::sync::Mutex::new("none"),
        }
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
    ///
    /// Returns `true` if the index was (re)built, `false` if the corpus
    /// fingerprint matched and no rebuild was needed.
    pub fn build_index(&self) -> Result<bool> {
        if let Some(parent) = self.index_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Older format (v2/v3) → current: emit a one-time message so users
        // understand why the first run after upgrade is slower than usual.
        if !self.config.quiet {
            use crate::index::format::{VERSION, VERSION_V2};
            if let Some(ver) = index::reader::index_version(&self.index_path) {
                if (VERSION_V2..VERSION).contains(&ver) {
                    eprintln!("migrating index to v{VERSION} format (one-time rebuild)...");
                }
            }
        }

        let _serial = self.build_mutex.lock().unwrap_or_else(|e| e.into_inner());
        let started = std::time::Instant::now();
        let rebuilt = rebuild_index_files(&self.root, &self.index_path)?;
        if let Some(w) = self.watcher_guard().as_ref() {
            w.mark_rebuilt(started);
        }
        Ok(rebuilt)
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
            // ripgrep semantics: the match must be preceded by start-of-line
            // or a non-word character, and followed by end-of-line or a
            // non-word character. Plain `\b...\b` fails for patterns whose
            // edges are non-word characters (`-w 'foo('` would never match
            // `call foo( x)`). The regex crate has no lookaround, so the
            // neighbouring character is consumed instead; results are
            // line-based, so this does not change which lines match.
            // `\n` is excluded so a match never starts on the previous line.
            // Capture group 1 holds the user's pattern (the match column).
            (
                format!(r"(?:(?m:^)|[^\w\n])({})(?:(?m:$)|[^\w\n])", inner).into(),
                true,
            )
        } else {
            (pattern.into(), opts.regex)
        }
    }

    /// The executable pattern (after word wrapping) for one user pattern.
    fn pattern_spec(pattern: &str, opts: &SearchOptions) -> search::PatternSpec {
        let (p, regex) = Self::effective_pattern(pattern, opts);
        search::PatternSpec {
            pattern: p.into_owned(),
            regex,
            word: opts.word,
        }
    }

    /// Compile the main pattern plus `opts.and_patterns` / `opts.not_patterns`.
    fn compile_query(&self, pattern: &str, opts: &SearchOptions) -> Result<search::CompiledQuery> {
        let spec = search::QuerySpec {
            main: Self::pattern_spec(pattern, opts),
            and: opts
                .and_patterns
                .iter()
                .map(|p| Self::pattern_spec(p, opts))
                .collect(),
            not: opts
                .not_patterns
                .iter()
                .map(|p| Self::pattern_spec(p, opts))
                .collect(),
            case_insensitive: opts.case_insensitive,
        };
        search::CompiledQuery::new(spec, self.config.quiet)
    }

    /// Execute a search. Auto-build, hybrid search, and git-changed-file search are handled internally.
    ///
    /// Results are sorted by (file, line). For large result sets prefer
    /// [`search_stream`](Self::search_stream), which does not hold every
    /// result in memory.
    pub fn search(&self, pattern: &str, opts: &SearchOptions) -> Result<Vec<SearchResult>> {
        let mut results = Vec::new();
        self.search_stream(
            pattern,
            opts,
            OutputMode::Lines,
            search::collect_into(&mut results),
        )?;
        Ok(results)
    }

    /// Streaming search: calls `sink` once per matching file, in path order,
    /// as soon as that file's results are complete.
    ///
    /// Files are scanned in parallel; only a bounded window of finished
    /// files waits for earlier ones, so memory does not grow with the total
    /// output. `mode` selects what is computed per file: every line
    /// ([`OutputMode::Lines`]), only the number of matching lines
    /// ([`OutputMode::Count`], no line text is built) or only whether the
    /// file matches ([`OutputMode::FilesOnly`], stops at the first match).
    ///
    /// `opts.max_count` limits the total number of matching lines delivered.
    /// Return `false` from `sink` to stop the search early.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use xgrep_search::{OutputMode, SearchOptions, Xgrep};
    ///
    /// let xg = Xgrep::open(".").unwrap();
    /// xg.search_stream("TODO", &SearchOptions::new(), OutputMode::Count, |fm| {
    ///     println!("{}:{}", fm.file, fm.count);
    ///     true
    /// })
    /// .unwrap();
    /// ```
    pub fn search_stream(
        &self,
        pattern: &str,
        opts: &SearchOptions,
        mode: OutputMode,
        mut sink: impl FnMut(FileMatches) -> bool,
    ) -> Result<()> {
        let q = self.compile_query(pattern, opts)?;
        let filter = self.scan_filter(opts)?;
        if opts.changed_only || opts.since.is_some() {
            self.search_changed(&q, opts, &filter, mode, &mut sink)
        } else {
            self.search_indexed(&q, opts, &filter, mode, &mut sink)
        }
    }

    /// Path filters and limit for `opts` (applied before files are read).
    fn scan_filter(&self, opts: &SearchOptions) -> Result<search::ScanFilter> {
        let extensions = match opts.file_type.as_deref() {
            Some(ft) => {
                let exts = filetype::extensions_for_type(ft);
                if exts.is_none() && !self.config.quiet {
                    eprintln!("warning: unknown file type '{}', showing all results", ft);
                }
                exts
            }
            None => None,
        };
        let globs = if opts.globs.is_empty() {
            None
        } else {
            Some(globfilter::GlobFilter::new(&opts.globs)?)
        };
        Ok(search::ScanFilter {
            extensions,
            path_substring: opts.path_pattern.clone(),
            globs,
            exclude_paths: None,
            max_count: opts.max_count,
        })
    }

    /// Index-based search with a freshness overlay.
    ///
    /// Before using the index, a cheap probe finds files that changed since
    /// the index was built (stat of every indexed file and directory,
    /// compared with the index entries), or, while a file watcher is active
    /// ([`Xgrep::start_watching`]), the watcher's tracked change set is used
    /// instead. Index results for those files are dropped and
    /// the files are scanned directly, so new and edited code is found even
    /// before the index is rebuilt. The rebuild itself happens in the
    /// background.
    ///
    /// If more than [`OVERLAY_MAX_FILES`] files changed, the default search
    /// falls back to the index alone (with a warning), while `opts.fresh`
    /// rebuilds the index synchronously first.
    fn search_indexed(
        &self,
        q: &search::CompiledQuery,
        opts: &SearchOptions,
        filter: &search::ScanFilter,
        mode: OutputMode,
        sink: &mut dyn FnMut(FileMatches) -> bool,
    ) -> Result<()> {
        self.set_last_freshness(None);

        // A missing index, or one in an older format version, is (re)built.
        if !self.index_path.exists() || index::reader::needs_rebuild(&self.index_path) {
            if !self.config.quiet {
                eprintln!("[indexing...]");
            }
            self.build_index()?;
            if !self.config.quiet {
                eprintln!("[done]");
            }
            let reader = index::reader::IndexReader::open(&self.index_path)?;
            self.set_last_freshness(Some(Freshness::default()));
            return self.search_reader(&reader, q, filter, &[], mode, sink);
        }

        let perf = std::env::var_os("XGREP_PERF_TIMING").is_some();
        let t0 = perf.then(std::time::Instant::now);
        // The watcher's change set must be read before the index is opened
        // (see `watch::Snapshot`).
        let watched = if opts.fresh || overlay_disabled() {
            None
        } else {
            self.watcher_guard().as_ref().map(|w| w.snapshot())
        };
        let reader = index::reader::IndexReader::open(&self.index_path)?;
        let mut source = "probe";
        let overlay = if opts.fresh {
            index::updater::compute_overlay(&self.root, &self.index_path, &reader, None)
        } else if overlay_disabled() {
            None
        } else if let Some(snap) = watched.filter(|s| s.usable) {
            // Watching: the tracked change set replaces the probe.
            source = "watcher";
            Some(index::updater::overlay_from_paths(
                &self.root,
                &self.index_path,
                &reader,
                snap.paths,
            ))
        } else if self.polling.load(std::sync::atomic::Ordering::Relaxed) {
            source = "probe-polling";
            self.cached_remote_probe(&reader)
        } else if self.probe_backoff_active() {
            None
        } else {
            let deadline = std::time::Instant::now() + overlay_probe_budget();
            let ov = index::updater::compute_overlay_cached(
                &self.root,
                &self.index_path,
                &reader,
                Some(deadline),
                probe_cache_ttl(),
            );
            self.record_probe_outcome(ov.is_some());
            ov.map(|(ov, cached)| {
                if cached {
                    source = "probe-cache";
                }
                ov
            })
        };
        if let Ok(mut g) = self.last_overlay_source.lock() {
            *g = if overlay.is_some() { source } else { "none" };
        }
        if let (Some(t), Some(ov)) = (t0, overlay.as_ref()) {
            eprintln!(
                "[perf] overlay_probe: {:.2}ms  changed={}{}  source={}",
                t.elapsed().as_secs_f64() * 1000.0,
                ov.changed.len(),
                if ov.truncated { "+" } else { "" },
                source
            );
        }
        let Some(overlay) = overlay else {
            // Probe disabled, backed off, or timed out: use the index as-is
            // (freshness unknown).
            self.search_reader(&reader, q, filter, &[], mode, sink)?;
            self.spawn_background_rebuild();
            return Ok(());
        };

        let changed = overlay.changed.len();
        if changed == 0 && !overlay.truncated {
            self.set_last_freshness(Some(Freshness::default()));
            if overlay.meta_outdated {
                self.spawn_background_rebuild();
            }
            return self.search_reader(&reader, q, filter, &[], mode, sink);
        }

        if overlay.truncated || changed > OVERLAY_MAX_FILES {
            if opts.fresh {
                // Too many changes to rescan: incremental rebuild, then search.
                drop(reader);
                if !self.config.quiet {
                    eprintln!("[updating index ({} files changed)...]", changed);
                }
                self.build_index()?;
                if !self.config.quiet {
                    eprintln!("[done]");
                }
                let reader = index::reader::IndexReader::open(&self.index_path)?;
                self.set_last_freshness(Some(Freshness::default()));
                return self.search_reader(&reader, q, filter, &[], mode, sink);
            }
            let info = Freshness {
                changed_files: changed,
                rescanned_files: 0,
                truncated: true,
            };
            if !self.config.quiet {
                eprintln!(
                    "xgrep: warning: {}; results may be incomplete. Run `xg init` or use --fresh.",
                    info
                );
            }
            self.set_last_freshness(Some(info));
            self.search_reader(&reader, q, filter, &[], mode, sink)?;
            self.spawn_background_rebuild();
            return Ok(());
        }

        // Overlay: skip changed files in the index scan and scan them
        // directly. Excluding them up front (rather than dropping their hits
        // afterwards) keeps the pushed-down max_count correct.
        let changed_keys: std::collections::HashSet<String> = overlay
            .changed
            .iter()
            .map(|p| index::updater::index_key(p))
            .collect();
        let index_filter = search::ScanFilter {
            exclude_paths: Some(changed_keys),
            ..filter.clone()
        };
        let scan: Vec<PathBuf> = overlay
            .changed
            .into_iter()
            .filter(|p| self.root.join(p).is_file())
            .collect();
        // One merged, path-ordered scan of index candidates and changed files.
        let mut targets = search::index_targets(&reader, q, &index_filter, self.config.quiet);
        targets.extend(search::file_targets(&scan, filter));
        search::run(
            &self.root,
            targets,
            q,
            filter.max_count,
            mode,
            self.config.quiet,
            sink,
        );
        drop(reader);

        self.set_last_freshness(Some(Freshness {
            changed_files: changed,
            rescanned_files: scan.len(),
            truncated: false,
        }));
        self.spawn_background_rebuild();
        Ok(())
    }

    /// Run a query against an open index reader, plus `direct` files read
    /// from disk, streaming per-file results to `sink` in path order.
    fn search_reader(
        &self,
        reader: &index::reader::IndexReader,
        q: &search::CompiledQuery,
        filter: &search::ScanFilter,
        direct: &[PathBuf],
        mode: OutputMode,
        sink: &mut dyn FnMut(FileMatches) -> bool,
    ) -> Result<()> {
        let mut targets = search::index_targets(reader, q, filter, self.config.quiet);
        targets.extend(search::file_targets(direct, filter));
        search::run(
            &self.root,
            targets,
            q,
            filter.max_count,
            mode,
            self.config.quiet,
            sink,
        );
        Ok(())
    }

    /// Marker written when a freshness probe exceeds its budget (e.g. git on
    /// a slow network or WSL `/mnt/c` filesystem).
    fn probe_slow_marker(&self) -> PathBuf {
        self.index_path.with_extension("probe-slow")
    }

    /// True while a recent probe timeout says probing is not worth it.
    fn probe_backoff_active(&self) -> bool {
        std::fs::metadata(self.probe_slow_marker())
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age.as_secs() < OVERLAY_PROBE_BACKOFF_SECS)
    }

    fn record_probe_outcome(&self, completed: bool) {
        let marker = self.probe_slow_marker();
        if completed {
            if marker.exists() {
                let _ = std::fs::remove_file(marker);
            }
        } else {
            let _ = std::fs::write(marker, b"freshness probe timed out\n");
            if !self.config.quiet {
                eprintln!(
                    "xgrep: note: index freshness check took over {}ms; skipping it for {} minutes \
                     (results may miss recent edits; use --fresh to force)",
                    overlay_probe_budget().as_millis(),
                    OVERLAY_PROBE_BACKOFF_SECS / 60
                );
            }
        }
    }

    /// Probe result in polling mode, reused for `watch::remote_probe_ttl()`.
    /// Honors the slow-probe backoff.
    fn cached_remote_probe(
        &self,
        reader: &index::reader::IndexReader,
    ) -> Option<index::updater::Overlay> {
        let mut cache = self.remote_probe.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, ov)) = cache.as_ref() {
            if at.elapsed() < watch::remote_probe_ttl() {
                return Some(ov.clone());
            }
        }
        if self.probe_backoff_active() {
            return None;
        }
        let deadline = std::time::Instant::now() + overlay_probe_budget();
        let ov =
            index::updater::compute_overlay(&self.root, &self.index_path, reader, Some(deadline));
        self.record_probe_outcome(ov.is_some());
        if let Some(ov) = &ov {
            *cache = Some((std::time::Instant::now(), ov.clone()));
        }
        ov
    }

    fn watcher_guard(&self) -> std::sync::MutexGuard<'_, Option<watch::WatchHandle>> {
        self.watcher.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Watch the root for file changes, for long-lived processes (the MCP
    /// server enables this; the CLI does not).
    ///
    /// While watching, index searches need no freshness probe: changed,
    /// created and deleted files are tracked in memory (respecting ignore
    /// files), read directly by searches, and folded into the index by a
    /// background rebuild once changes have been quiet for ~2 s. Until the
    /// watcher is in sync, after lost events (queue overflow), or if it fails
    /// to start, searches fall back to the probe.
    ///
    /// Filesystems without reliable change events (WSL `/mnt/<drive>`,
    /// NFS/SMB/FUSE mounts) are not watched; instead the probe result is
    /// reused for ~2 s ([`WatchStatus::Polling`], `XGREP_REMOTE_PROBE_CACHE_MS`),
    /// so edits there may take that long to show up.
    ///
    /// Idempotent. Setup runs on a background thread; see
    /// [`watch_status`](Self::watch_status). Stopped by
    /// [`stop_watching`](Self::stop_watching) or when `self` is dropped.
    pub fn start_watching(&self) -> Result<()> {
        self.start_watching_with(watch::WatchConfig::default())
    }

    pub(crate) fn start_watching_with(&self, cfg: watch::WatchConfig) -> Result<()> {
        let mut guard = self.watcher_guard();
        if guard.is_some() || self.polling.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(());
        }
        if fsinfo::is_remote_fs(&self.root) {
            self.polling
                .store(true, std::sync::atomic::Ordering::Relaxed);
            return Ok(());
        }
        let root = self.root.clone();
        let index_path = self.index_path.clone();
        let build_mutex = std::sync::Arc::clone(&self.build_mutex);
        let rebuild: watch::RebuildFn = Box::new(move || {
            let _serial = build_mutex.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(parent) = index_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            rebuild_index_files(&root, &index_path)
        });
        *guard = Some(watch::WatchHandle::start(
            &self.root,
            &self.index_path,
            rebuild,
            cfg,
        )?);
        Ok(())
    }

    /// Stop the file watcher (no-op if not watching). Waits for an index
    /// rebuild the watcher is running to finish.
    pub fn stop_watching(&self) {
        self.polling
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let handle = self.watcher_guard().take();
        drop(handle);
    }

    /// Current state of the file watcher.
    pub fn watch_status(&self) -> WatchStatus {
        if self.polling.load(std::sync::atomic::Ordering::Relaxed) {
            return WatchStatus::Polling;
        }
        self.watcher_guard()
            .as_ref()
            .map_or(WatchStatus::Off, |w| w.status())
    }

    /// Block until the watcher is in sync (searches then skip the probe), up
    /// to `timeout`. Returns `false` on timeout, or if not watching or the
    /// watcher failed.
    pub fn wait_for_watcher(&self, timeout: std::time::Duration) -> bool {
        // Wait in short steps so the watcher mutex is not held for long
        // (searches need it).
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let step = deadline
                .saturating_duration_since(std::time::Instant::now())
                .min(std::time::Duration::from_millis(100));
            let synced = match self.watcher_guard().as_ref() {
                None => return false,
                Some(w) => w.wait_synced(step),
            };
            if synced {
                return true;
            }
            if std::time::Instant::now() >= deadline
                || matches!(self.watch_status(), WatchStatus::Failed(_))
            {
                return false;
            }
        }
    }

    /// One-line freshness note for tool output (e.g. MCP responses), such as
    /// `index: fresh; watching: yes` or
    /// `index stale: 3 files rescanned; watching: no`.
    ///
    /// The first part describes the most recent [`search`](Self::search)
    /// (see [`last_search_freshness`](Self::last_search_freshness)); the
    /// second the file watcher ([`watch_status`](Self::watch_status)).
    pub fn freshness_note(&self) -> String {
        let index = match self.last_search_freshness() {
            Some(f) => f.to_string(),
            None => "index: freshness not checked".to_string(),
        };
        format!("{index}; {}", self.watch_status())
    }

    fn set_last_freshness(&self, value: Option<Freshness>) {
        if let Ok(mut guard) = self.last_freshness.lock() {
            *guard = value;
        }
    }

    /// Freshness observed by the most recent [`search`](Self::search).
    ///
    /// `None` if no index search has run yet, or the last search did not use
    /// the index (`changed_only`, `since`). When the index was stale, the
    /// returned value tells how many changed files were rescanned directly
    /// (its `Display` renders e.g. `index stale: 3 files rescanned`).
    pub fn last_search_freshness(&self) -> Option<Freshness> {
        self.last_freshness.lock().ok().and_then(|g| g.clone())
    }

    /// Files whose contents may differ from the index (modified, deleted,
    /// or new — including untracked files in a git work tree), relative to
    /// the root. Empty when the index is fresh or has not been built.
    pub fn stale_files(&self) -> Result<Vec<PathBuf>> {
        match index::updater::check_index_status(&self.root, &self.index_path)? {
            index::updater::IndexStatus::Stale { changed_files } => Ok(changed_files),
            _ => Ok(Vec::new()),
        }
    }

    /// Start a background index rebuild without blocking the caller.
    ///
    /// Skips if a build lock exists, a rebuild is already running in this
    /// process, or a rebuild was attempted within the last
    /// [`REBUILD_COOLDOWN_SECS`]. Attempts are recorded in a `.rebuild`
    /// stamp file next to the index, touched even when the build turns out
    /// to be a no-op, so repeated searches do not re-walk the tree.
    ///
    /// In the `xg` CLI (see [`set_background_rebuild_process`]) a detached
    /// `xg init` child is spawned so the search process can exit at once.
    /// Everywhere else (MCP server, Python/Node bindings) the rebuild runs on
    /// a background thread: `current_exe()` is not `xg` there, and a thread
    /// leaves no zombie processes behind.
    fn spawn_background_rebuild(&self) {
        use std::sync::atomic::Ordering;

        // With a watcher, its worker owns rebuild scheduling.
        if let Some(w) = self.watcher_guard().as_ref() {
            if w.is_running() {
                w.request_rebuild();
                return;
            }
        }
        if self.index_path.with_extension("lock").exists()
            || self.rebuild_running.load(Ordering::Acquire)
        {
            return;
        }
        let recent = |p: &Path| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age.as_secs() < REBUILD_COOLDOWN_SECS)
        };
        let stamp = self.index_path.with_extension("rebuild");
        if recent(&stamp) || recent(&self.index_path) {
            return;
        }
        // Record the attempt first so concurrent searches don't pile up.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if std::fs::write(&stamp, now.to_string()).is_err() {
            return;
        }

        if SPAWN_REBUILD_PROCESS.load(Ordering::Relaxed) && current_exe_is_xg() {
            self.spawn_rebuild_process();
        } else {
            self.spawn_rebuild_thread();
        }
    }

    /// Spawn a detached `xg init` for this root (CLI only).
    ///
    /// The child is not waited for: the CLI exits right after printing
    /// results, and the orphaned child is reaped by init. It is placed in
    /// its own process group so Ctrl-C on the search does not kill it.
    fn spawn_rebuild_process(&self) {
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let root = std::fs::canonicalize(&self.root).unwrap_or_else(|_| self.root.clone());
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("init");
        if self.index_path == self.root.join(".xgrep").join("index") {
            cmd.arg("--local");
        }
        cmd.arg(&root)
            .current_dir(&root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let _ = cmd.spawn();
    }

    /// Rebuild on a background thread inside this process.
    fn spawn_rebuild_thread(&self) {
        use std::sync::atomic::Ordering;

        if self.rebuild_running.swap(true, Ordering::AcqRel) {
            return;
        }
        let root = self.root.clone();
        let index_path = self.index_path.clone();
        let running = std::sync::Arc::clone(&self.rebuild_running);
        let build_mutex = std::sync::Arc::clone(&self.build_mutex);
        let spawned = std::thread::Builder::new()
            .name("xgrep-rebuild".into())
            .spawn(move || {
                let _serial = build_mutex.lock().unwrap_or_else(|e| e.into_inner());
                let _ = rebuild_index_files(&root, &index_path);
                running.store(false, Ordering::Release);
            });
        if spawned.is_err() {
            self.rebuild_running.store(false, Ordering::Release);
        }
    }

    /// Wait (bounded) until an in-process background rebuild finishes.
    #[cfg(test)]
    fn wait_background_rebuild(&self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while self
            .rebuild_running
            .load(std::sync::atomic::Ordering::Acquire)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
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
        let mut results = Vec::new();
        self.search_files_stream(
            files,
            pattern,
            opts,
            OutputMode::Lines,
            search::collect_into(&mut results),
        )?;
        Ok(results)
    }

    /// Streaming variant of [`search_files`](Self::search_files); see
    /// [`search_stream`](Self::search_stream) for `mode` and `sink`.
    pub fn search_files_stream(
        &self,
        files: &[PathBuf],
        pattern: &str,
        opts: &SearchOptions,
        mode: OutputMode,
        mut sink: impl FnMut(FileMatches) -> bool,
    ) -> Result<()> {
        let q = self.compile_query(pattern, opts)?;
        let filter = search::ScanFilter {
            globs: if opts.globs.is_empty() {
                None
            } else {
                Some(globfilter::GlobFilter::new(&opts.globs)?)
            },
            max_count: opts.max_count,
            ..Default::default()
        };
        let targets = search::file_targets(files, &filter);
        search::run(
            &self.root,
            targets,
            &q,
            filter.max_count,
            mode,
            self.config.quiet,
            &mut sink,
        );
        Ok(())
    }

    /// Search only git-changed files. Returns error if not a git repository.
    fn search_changed(
        &self,
        q: &search::CompiledQuery,
        opts: &SearchOptions,
        filter: &search::ScanFilter,
        mode: OutputMode,
        sink: &mut dyn FnMut(FileMatches) -> bool,
    ) -> Result<()> {
        self.set_last_freshness(None);
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

        let targets = search::file_targets(&files, filter);
        search::run(
            &self.root,
            targets,
            q,
            filter.max_count,
            mode,
            self.config.quiet,
            sink,
        );
        Ok(())
    }

    /// Search the lines added and removed by `git diff <range>`.
    ///
    /// `range` is anything `git diff` accepts as a revision argument, e.g.
    /// `HEAD~3..HEAD`, `main...feature` or `HEAD` (working tree against
    /// HEAD). Only `+`/`-` lines are searched; added lines carry their line
    /// number in the new file, removed lines in the old file. Paths are
    /// relative to the search root, and only changes under the root are
    /// searched. Results are in diff order (by file, then hunk).
    ///
    /// Honoured options: `regex`, `word`, `case_insensitive`, `file_type`,
    /// `path_pattern`, `globs`, `max_count`. `and_patterns` / `not_patterns`
    /// apply per file: a file's lines are reported only if its added and
    /// removed lines (together) match every AND pattern and no NOT pattern.
    pub fn search_diff(
        &self,
        range: &str,
        pattern: &str,
        opts: &SearchOptions,
    ) -> Result<Vec<DiffMatch>> {
        let filter = self.scan_filter(opts)?;
        let quiet = self.config.quiet;
        let ci = opts.case_insensitive;
        let main = search::LineMatcher::new(&Self::pattern_spec(pattern, opts), ci, quiet)?;
        let compile_all = |pats: &[String]| -> Result<Vec<search::LineMatcher>> {
            pats.iter()
                .map(|p| search::LineMatcher::new(&Self::pattern_spec(p, opts), ci, quiet))
                .collect()
        };
        let gates = history::FileGates {
            and: compile_all(&opts.and_patterns)?,
            not: compile_all(&opts.not_patterns)?,
        };
        history::search_diff(&self.root, range, &main, &gates, &filter)
    }

    /// Search commit messages (subject and body) of the history reachable
    /// from HEAD, newest first. Each matching commit is reported once, with
    /// its first matching message line.
    ///
    /// Only commits touching files under the search root are considered when
    /// the root is a subdirectory of the repository. Honoured options:
    /// `regex`, `word`, `case_insensitive`, `max_count`, and `since`
    /// (`"2d"`, `"1w"`, `"20.commits"`, ...).
    pub fn search_commits(&self, pattern: &str, opts: &SearchOptions) -> Result<Vec<CommitMatch>> {
        let matcher = search::LineMatcher::new(
            &Self::pattern_spec(pattern, opts),
            opts.case_insensitive,
            self.config.quiet,
        )?;
        history::search_commits(&self.root, &matcher, opts.since.as_deref(), opts.max_count)
    }

    /// Find files matching a glob or substring pattern.
    /// Returns a list of relative file paths from the index.
    pub fn find_files(&self, pattern: &str) -> Result<Vec<String>> {
        // A missing index, or one in an older format version, is (re)built.
        if !self.index_path.exists() || index::reader::needs_rebuild(&self.index_path) {
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

/// True if the running executable is the `xg` CLI binary (not a host
/// interpreter such as python or node that loaded xgrep as a library).
fn current_exe_is_xg() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .is_some_and(|stem| stem == "xg" || stem == "xgrep")
}

/// Resolve where the index for `root` lives.
///
/// The root is canonicalized first so that relative paths, `..`
/// components, and symlinked paths to the same directory share one index.
fn resolve_index_path(root: &Path) -> Result<PathBuf> {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
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
        // Verify that parse_duration in git.rs returns InvalidArgument.
        // Tested indirectly via since_files().
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

    fn git(root: &Path, args: &[&str]) {
        let out = crate::git::git_cmd()
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {:?} failed: {:?}", args, out);
    }

    fn init_repo_with(root: &Path, files: &[(&str, &str)]) {
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "t@t"]);
        git(root, &["config", "user.name", "t"]);
        std::fs::write(root.join(".gitignore"), ".xgrep/\n").unwrap();
        for (name, content) in files {
            let p = root.join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, content).unwrap();
        }
        git(root, &["add", "."]);
        git(root, &["commit", "-q", "-m", "init"]);
    }

    fn lines_of(results: &[SearchResult]) -> Vec<(String, usize)> {
        results
            .iter()
            .map(|r| (r.file.to_string(), r.line_number))
            .collect()
    }

    /// Regression (P2): `-w 'foo('` must match `call foo( x)` like ripgrep.
    #[test]
    fn test_word_boundary_non_word_edges() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("a.txt"),
            "call foo( x)\nfoo(\nxfoo( y\nfoo(x\n(bar) end\nx(bar)y\n",
        )
        .unwrap();
        let xg = Xgrep::open_local(root).unwrap();
        xg.build_index().unwrap();
        let opts = SearchOptions::new().with_word(true);

        let r = xg.search("foo(", &opts).unwrap();
        assert_eq!(
            lines_of(&r),
            vec![("a.txt".into(), 1), ("a.txt".into(), 2)],
            "{r:?}"
        );

        let r = xg.search("(bar)", &opts).unwrap();
        assert_eq!(lines_of(&r), vec![("a.txt".into(), 5)], "{r:?}");
    }

    /// The consumed boundary character must never attribute a match to the
    /// previous line, and regex patterns must work too.
    #[test]
    fn test_word_boundary_line_attribution_and_regex() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "abc\nfoo bar\nfoobar\n").unwrap();
        let xg = Xgrep::open_local(root).unwrap();
        xg.build_index().unwrap();

        let r = xg
            .search("foo", &SearchOptions::new().with_word(true))
            .unwrap();
        assert_eq!(lines_of(&r), vec![("a.txt".into(), 2)], "{r:?}");
        assert_eq!(r[0].line, "foo bar");

        let r = xg
            .search(
                "fo+|bar",
                &SearchOptions::new().with_word(true).with_regex(true),
            )
            .unwrap();
        assert_eq!(lines_of(&r), vec![("a.txt".into(), 2)], "{r:?}");
    }

    /// Regression (1-7): the default (non-fresh) search must not silently
    /// miss edited or untracked files, and must drop stale index hits.
    #[test]
    fn test_default_search_overlays_git_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo_with(
            root,
            &[
                ("a.rs", "fn old_name() {}\n"),
                ("gone.rs", "fn old_name_gone() {}\n"),
            ],
        );
        let xg = Xgrep::open_local(root)
            .unwrap()
            .with_config(Config { quiet: true });
        xg.build_index().unwrap();

        let opts = SearchOptions::new();
        let r = xg.search("old_name", &opts).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(xg.last_search_freshness(), Some(Freshness::default()));

        std::fs::write(root.join("a.rs"), "fn brand_new_sym() {}\n").unwrap();
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        std::fs::write(root.join("src/deep/b.rs"), "fn another_new_sym() {}\n").unwrap();
        std::fs::write(root.join(".hidden.rs"), "fn brand_new_sym_hidden() {}\n").unwrap();
        std::fs::remove_file(root.join("gone.rs")).unwrap();

        let r = xg.search("brand_new_sym", &opts).unwrap();
        assert_eq!(lines_of(&r), vec![("a.rs".into(), 1)], "{r:?}");
        let r = xg.search("another_new_sym", &opts).unwrap();
        assert_eq!(lines_of(&r), vec![("src/deep/b.rs".into(), 1)], "{r:?}");
        // Stale hits for the edited and the deleted file are dropped.
        let r = xg.search("old_name", &opts).unwrap();
        assert!(r.is_empty(), "{r:?}");

        let f = xg.last_search_freshness().unwrap();
        assert!(f.is_stale());
        assert!(!f.truncated);
        // a.rs (modified), gone.rs (deleted), src/deep/b.rs (untracked)
        assert_eq!(f.changed_files, 3, "{f:?}");
        assert_eq!(f.rescanned_files, 2, "{f:?}");
        assert_eq!(f.to_string(), "index stale: 2 files rescanned");

        let stale = xg.stale_files().unwrap();
        assert_eq!(
            stale,
            vec![
                PathBuf::from("a.rs"),
                PathBuf::from("gone.rs"),
                PathBuf::from("src/deep/b.rs")
            ]
        );
        xg.wait_background_rebuild();
    }

    /// After a commit, files changed between the indexed commit and HEAD
    /// are rescanned even though `git status` is clean.
    #[test]
    fn test_default_search_overlays_new_commits() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo_with(root, &[("a.rs", "fn first() {}\n")]);
        let xg = Xgrep::open_local(root)
            .unwrap()
            .with_config(Config { quiet: true });
        xg.build_index().unwrap();

        std::fs::write(root.join("a.rs"), "fn committed_change() {}\n").unwrap();
        git(root, &["commit", "-q", "-am", "second"]);

        let r = xg
            .search("committed_change", &SearchOptions::new())
            .unwrap();
        assert_eq!(lines_of(&r), vec![("a.rs".into(), 1)], "{r:?}");
        xg.wait_background_rebuild();
    }

    /// Same-size edit right after a build (same mtime second) is still seen.
    #[test]
    fn test_default_search_racy_same_size_edit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo_with(root, &[("a.rs", "x\n")]);
        std::fs::write(root.join("u.rs"), "fn aaaa() {}\n").unwrap();
        let xg = Xgrep::open_local(root)
            .unwrap()
            .with_config(Config { quiet: true });
        xg.build_index().unwrap();
        // Untracked file rewritten with identical size immediately.
        std::fs::write(root.join("u.rs"), "fn bbbb() {}\n").unwrap();
        let r = xg.search("bbbb", &SearchOptions::new()).unwrap();
        assert_eq!(lines_of(&r), vec![("u.rs".into(), 1)], "{r:?}");
        xg.wait_background_rebuild();
    }

    /// Non-git trees: modified, deleted, and new files (including files in
    /// new directories) are detected via mtimes.
    #[test]
    fn test_default_search_overlays_non_git_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(!crate::git::is_git_repo(root), "tempdir must not be in git");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("a.txt"), "needle_old\n").unwrap();
        std::fs::write(root.join("sub/c.txt"), "needle_old too\n").unwrap();
        let xg = Xgrep::open_local(root)
            .unwrap()
            .with_config(Config { quiet: true });
        xg.build_index().unwrap();
        assert!(xg.stale_files().unwrap().is_empty());

        std::fs::write(root.join("a.txt"), "needle_new here\n").unwrap();
        std::fs::remove_file(root.join("sub/c.txt")).unwrap();
        std::fs::create_dir_all(root.join("newdir/inner")).unwrap();
        std::fs::write(root.join("newdir/inner/n.txt"), "needle_new deep\n").unwrap();
        std::fs::write(root.join("sub/d.txt"), "needle_new sub\n").unwrap();
        std::fs::write(root.join(".hidden"), "needle_new hidden\n").unwrap();

        let opts = SearchOptions::new();
        let r = xg.search("needle_new", &opts).unwrap();
        assert_eq!(
            lines_of(&r),
            vec![
                ("a.txt".into(), 1),
                ("newdir/inner/n.txt".into(), 1),
                ("sub/d.txt".into(), 1)
            ],
            "{r:?}"
        );
        let r = xg.search("needle_old", &opts).unwrap();
        assert!(r.is_empty(), "{r:?}");
        let f = xg.last_search_freshness().unwrap();
        assert_eq!(f.changed_files, 4, "{f:?}");
        assert_eq!(f.rescanned_files, 3, "{f:?}");
        xg.wait_background_rebuild();
    }

    /// max_count with changed files: the limited index scan must not be
    /// filled by stale hits of changed files that are dropped afterwards.
    #[test]
    fn test_overlay_respects_max_count() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "needle\nneedle\n").unwrap();
        std::fs::write(root.join("b.txt"), "needle\n").unwrap();
        std::fs::write(root.join("c.txt"), "needle\n").unwrap();
        let xg = Xgrep::open_local(root)
            .unwrap()
            .with_config(Config { quiet: true });
        xg.build_index().unwrap();

        // a.txt no longer matches; its stale index hits must not use up the limit.
        std::fs::write(root.join("a.txt"), "gone\n").unwrap();
        let r = xg
            .search("needle", &SearchOptions::new().with_max_count(2))
            .unwrap();
        assert_eq!(
            lines_of(&r),
            vec![("b.txt".into(), 1), ("c.txt".into(), 1)],
            "{r:?}"
        );
        xg.wait_background_rebuild();
    }

    /// Too many changed files: the default search falls back to the index
    /// and reports it; `fresh` rebuilds synchronously instead.
    #[test]
    fn test_overlay_cap_and_fresh_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("seed.txt"), "seed content\n").unwrap();
        let xg = Xgrep::open_local(root)
            .unwrap()
            .with_config(Config { quiet: true });
        xg.build_index().unwrap();
        // Keep the background rebuild from racing the assertions.
        std::fs::write(xg.index_path().with_extension("rebuild"), "0").unwrap();

        for i in 0..(OVERLAY_MAX_FILES + 1) {
            std::fs::write(root.join(format!("f{i}.txt")), "capneedle\n").unwrap();
        }
        let r = xg.search("capneedle", &SearchOptions::new()).unwrap();
        assert!(r.is_empty());
        let f = xg.last_search_freshness().unwrap();
        assert!(f.truncated, "{f:?}");
        assert_eq!(f.changed_files, OVERLAY_MAX_FILES + 1);

        let r = xg
            .search("capneedle", &SearchOptions::new().with_fresh(true))
            .unwrap();
        assert_eq!(r.len(), OVERLAY_MAX_FILES + 1);
        assert_eq!(xg.last_search_freshness(), Some(Freshness::default()));
    }

    /// Regression (1-8/2-12): library contexts rebuild on a thread (never by
    /// spawning `current_exe()`), and the cooldown stamp is honoured even
    /// when the rebuild finds nothing to change.
    #[test]
    fn test_background_rebuild_thread_and_cooldown() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "alpha_one\n").unwrap();
        let xg = Xgrep::open_local(root)
            .unwrap()
            .with_config(Config { quiet: true });
        xg.build_index().unwrap();
        let stamp = xg.index_path().with_extension("rebuild");
        let age = |p: &Path| {
            let old = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
            std::fs::File::options()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(old)
                .unwrap();
        };
        age(xg.index_path());

        std::fs::write(root.join("a.txt"), "alpha_two\n").unwrap();
        let r = xg.search("alpha_two", &SearchOptions::new()).unwrap();
        assert_eq!(r.len(), 1);
        assert!(stamp.exists(), "rebuild attempt must be stamped");
        xg.wait_background_rebuild();
        assert!(
            xg.stale_files().unwrap().is_empty(),
            "background thread should have refreshed the index"
        );

        // Within the cooldown, no further rebuild starts.
        age(xg.index_path());
        std::fs::write(root.join("a.txt"), "alpha_three\n").unwrap();
        let r = xg.search("alpha_three", &SearchOptions::new()).unwrap();
        assert_eq!(r.len(), 1);
        assert!(!xg
            .rebuild_running
            .load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(xg.stale_files().unwrap(), vec![PathBuf::from("a.txt")]);
    }

    /// Regression (P2): non-ASCII paths work with --fresh (git -z parsing).
    #[test]
    fn test_fresh_search_non_ascii_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo_with(root, &[("日本.txt", "old text\n")]);
        let xg = Xgrep::open_local(root)
            .unwrap()
            .with_config(Config { quiet: true });
        xg.build_index().unwrap();
        std::fs::write(root.join("日本.txt"), "brand new text\n").unwrap();

        for fresh in [true, false] {
            let r = xg
                .search("brand new", &SearchOptions::new().with_fresh(fresh))
                .unwrap();
            assert_eq!(lines_of(&r), vec![("日本.txt".into(), 1)], "{r:?}");
        }
        match xg.index_status().unwrap().state {
            IndexState::Stale { changed_files } => assert_eq!(changed_files, 1),
            other => panic!("expected stale, got {other:?}"),
        }
        xg.wait_background_rebuild();
    }

    /// Regression (P2): relative, `..`, and symlinked spellings of the same
    /// root share one index.
    #[test]
    fn test_resolve_index_path_canonicalizes_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let canonical = resolve_index_path(&root).unwrap();
        assert_eq!(
            resolve_index_path(&root.join("sub").join("..")).unwrap(),
            canonical
        );
        #[cfg(unix)]
        {
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(&root, &link).unwrap();
            assert_eq!(resolve_index_path(&link).unwrap(), canonical);
        }
    }
}
