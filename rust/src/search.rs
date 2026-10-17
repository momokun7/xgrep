use crate::candidates::{resolve_literal_candidates, resolve_regex_candidates};
use crate::error::{Result, XgrepError};
use crate::globfilter::GlobFilter;
use crate::index::reader::IndexReader;
use crate::trigram;
use memchr::memmem;
use rayon::prelude::*;
use regex::bytes::RegexBuilder;
use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

thread_local! {
    static LOWER_BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// ASCII-only case-insensitive contains check (for testing).
/// Uses memmem::Finder internally to verify with the same algorithm as production code.
#[cfg(test)]
#[allow(dead_code)]
fn contains_case_insensitive(haystack: &str, needle: &str) -> bool {
    let mut lowered = haystack.as_bytes().to_vec();
    lowered.make_ascii_lowercase();
    let needle_lower = needle.to_ascii_lowercase();
    memmem::Finder::new(needle_lower.as_bytes())
        .find(&lowered)
        .is_some()
}

/// Maps non-decreasing byte positions to (line number, line start, line end)
/// incrementally, without building a full line-offset table.
struct LineCursor<'a> {
    content: &'a [u8],
    /// Start of the line containing the last located position.
    line_start: usize,
    /// 1-based line number of `line_start`.
    line: usize,
}

impl<'a> LineCursor<'a> {
    fn new(content: &'a [u8]) -> Self {
        Self {
            content,
            line_start: 0,
            line: 1,
        }
    }

    fn locate(&mut self, pos: usize) -> (usize, usize, usize) {
        let content = self.content;
        let ls = memchr::memrchr(b'\n', &content[self.line_start..pos])
            .map_or(self.line_start, |i| self.line_start + i + 1);
        self.line += memchr::memchr_iter(b'\n', &content[self.line_start..ls]).count();
        self.line_start = ls;
        let le = memchr::memchr(b'\n', &content[pos..]).map_or(content.len(), |i| pos + i);
        (self.line, ls, le)
    }
}

/// Collect one result per matching line.
///
/// `find(hay, from)` returns the next match `(start, end)` at or after `from`.
/// After a hit, searching resumes at the next line, so each line is searched
/// at most once. Line text is taken from `content`, which must have the same
/// layout as `hay` (e.g. `hay` may be an ASCII-lowercased copy).
fn collect_lines(
    hay: &[u8],
    content: &[u8],
    rel_path: &str,
    mut find: impl FnMut(&[u8], usize) -> Option<(usize, usize)>,
) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let mut file: Option<Arc<str>> = None;
    let mut cursor = LineCursor::new(hay);
    let mut from = 0usize;
    while from <= hay.len() {
        let Some((start, end)) = find(hay, from) else {
            break;
        };
        // An empty match after a trailing newline is not a line.
        if start == hay.len() && start > 0 && hay[start - 1] == b'\n' {
            break;
        }
        let (line_number, ls, le) = cursor.locate(start);
        let line = std::str::from_utf8(&content[ls..le]).unwrap_or("<binary>");
        let file = file.get_or_insert_with(|| Arc::from(rel_path));
        results.push(SearchResult {
            file: Arc::clone(file),
            line_number,
            line: line.to_string(),
        });
        from = end.max(le + 1);
    }
    results
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub file: Arc<str>,
    pub line_number: usize,
    pub line: String,
}

// ---------------------------------------------------------------------------
// Matcher trait: unified interface for three matching strategies
// ---------------------------------------------------------------------------

trait Matcher: Send + Sync {
    fn find_matches(&self, content: &[u8], rel_path: &str) -> Vec<SearchResult>;
}

/// Case-sensitive literal string matcher (using memmem SIMD search).
struct LiteralMatcher {
    finder: memmem::Finder<'static>,
}

impl LiteralMatcher {
    fn new(pattern: &[u8]) -> Self {
        Self {
            finder: memmem::Finder::new(pattern).into_owned(),
        }
    }
}

impl Matcher for LiteralMatcher {
    fn find_matches(&self, content: &[u8], rel_path: &str) -> Vec<SearchResult> {
        let len = self.finder.needle().len();
        collect_lines(content, content, rel_path, |hay, from| {
            self.finder
                .find(&hay[from..])
                .map(|p| (from + p, from + p + len))
        })
    }
}

/// Case-insensitive (ASCII folding) literal string matcher.
/// Searches an ASCII-lowercased copy of the whole file, reusing a
/// thread-local buffer to avoid repeated allocation.
struct CaseInsensitiveMatcher {
    finder: memmem::Finder<'static>,
}

impl CaseInsensitiveMatcher {
    fn new(pattern_lower: &[u8]) -> Self {
        Self {
            finder: memmem::Finder::new(pattern_lower).into_owned(),
        }
    }
}

impl Matcher for CaseInsensitiveMatcher {
    fn find_matches(&self, content: &[u8], rel_path: &str) -> Vec<SearchResult> {
        let len = self.finder.needle().len();
        LOWER_BUF.with(|cell| {
            let mut buf = cell.borrow_mut();
            buf.clear();
            buf.extend_from_slice(content);
            buf.make_ascii_lowercase();
            collect_lines(&buf, content, rel_path, |hay, from| {
                self.finder
                    .find(&hay[from..])
                    .map(|p| (from + p, from + p + len))
            })
        })
    }
}

/// Literal matcher; uses the copy-free case-sensitive path when case folding
/// cannot change the result (no ASCII letters in the pattern).
fn literal_matcher(pattern: &str, case_insensitive: bool) -> Box<dyn Matcher> {
    let lower = pattern.to_ascii_lowercase();
    if case_insensitive && lower.bytes().any(|b| b.is_ascii_lowercase()) {
        Box::new(CaseInsensitiveMatcher::new(lower.as_bytes()))
    } else {
        Box::new(LiteralMatcher::new(pattern.as_bytes()))
    }
}

/// Regex matcher.
struct RegexMatcher {
    re: regex::bytes::Regex,
}

impl Matcher for RegexMatcher {
    fn find_matches(&self, content: &[u8], rel_path: &str) -> Vec<SearchResult> {
        collect_lines(content, content, rel_path, |hay, from| {
            self.re.find_at(hay, from).map(|m| (m.start(), m.end()))
        })
    }
}

// ---------------------------------------------------------------------------
// Path filters and limits (applied before files are read)
// ---------------------------------------------------------------------------

/// Path-based filters and a result limit, applied to candidate files before
/// they are read.
#[derive(Default)]
pub(crate) struct ScanFilter {
    /// Keep only files with one of these extensions (`-t`).
    pub(crate) extensions: Option<Vec<&'static str>>,
    /// Keep only paths containing this substring.
    pub(crate) path_substring: Option<String>,
    /// Include/exclude globs (`-g`).
    pub(crate) globs: Option<GlobFilter>,
    /// Stop once this many results are collected. Results are the first
    /// `max_count` in (file, line) order.
    pub(crate) max_count: Option<usize>,
}

impl ScanFilter {
    pub(crate) fn accepts(&self, path: &str) -> bool {
        if let Some(exts) = &self.extensions {
            let ext = Path::new(path).extension().and_then(|e| e.to_str());
            if !ext.is_some_and(|e| exts.contains(&e)) {
                return false;
            }
        }
        if let Some(sub) = &self.path_substring {
            if !path.contains(sub.as_str()) {
                return false;
            }
        }
        if let Some(globs) = &self.globs {
            if !globs.matches(path) {
                return false;
            }
        }
        true
    }

    fn has_path_filter(&self) -> bool {
        self.extensions.is_some() || self.path_substring.is_some() || self.globs.is_some()
    }
}

thread_local! {
    static SCOPED_FILTER: RefCell<Option<ScanFilter>> = const { RefCell::new(None) };
}

/// Run `f` with `filter` applied to every `search*` call it makes on this
/// thread. Lets callers that cannot pass a filter explicitly still get
/// filter pushdown.
pub(crate) fn with_scan_filter<R>(filter: ScanFilter, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<ScanFilter>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let prev = self.0.take();
            SCOPED_FILTER.with(|c| *c.borrow_mut() = prev);
        }
    }
    let prev = SCOPED_FILTER.with(|c| c.borrow_mut().replace(filter));
    let _restore = Restore(prev);
    f()
}

/// Run `f` with the scoped filter (or the default). The filter is taken out
/// while `f` runs so that work stolen onto this thread does not see it.
fn with_active_filter<R>(f: impl FnOnce(&ScanFilter) -> R) -> R {
    struct PutBack(Option<ScanFilter>);
    impl Drop for PutBack {
        fn drop(&mut self) {
            if let Some(filter) = self.0.take() {
                SCOPED_FILTER.with(|c| *c.borrow_mut() = Some(filter));
            }
        }
    }
    let guard = PutBack(SCOPED_FILTER.with(|c| c.borrow_mut().take()));
    match &guard.0 {
        Some(filter) => f(filter),
        None => f(&ScanFilter::default()),
    }
}

// ---------------------------------------------------------------------------
// Scan functions
// ---------------------------------------------------------------------------

/// Below this many files, scan sequentially: starting the rayon pool costs
/// more than it saves.
const PARALLEL_THRESHOLD: usize = 64;
/// First chunk size when scanning with a result limit; doubles per chunk.
const LIMIT_CHUNK: usize = 64;

/// A file to scan: its path relative to the root and its display name.
struct Target<'a> {
    path: &'a Path,
    name: std::borrow::Cow<'a, str>,
}

fn scan_one<M: Matcher + ?Sized>(
    root: &Path,
    target: &Target<'_>,
    matcher: &M,
    skip_binary: bool,
    quiet: bool,
) -> Vec<SearchResult> {
    let full_path = root.join(target.path);
    let content = match fs::read(&full_path) {
        Ok(c) => c,
        Err(e) => {
            if !quiet {
                eprintln!("xgrep: {}: {}", full_path.display(), e);
            }
            return vec![];
        }
    };
    if skip_binary && memchr::memchr(0, &content).is_some() {
        return vec![];
    }
    matcher.find_matches(&content, &target.name)
}

fn scan_many<M: Matcher + ?Sized>(
    root: &Path,
    targets: &[Target<'_>],
    matcher: &M,
    skip_binary: bool,
    quiet: bool,
) -> Vec<Vec<SearchResult>> {
    let one = |t: &Target<'_>| scan_one(root, t, matcher, skip_binary, quiet);
    if targets.len() < PARALLEL_THRESHOLD {
        targets.iter().map(one).collect()
    } else {
        targets.par_iter().map(one).collect()
    }
}

/// Filter targets by path, scan them in path order and return results
/// sorted by (file, line). With a limit, files are scanned in growing
/// chunks and scanning stops once enough results are collected.
fn scan<M: Matcher + ?Sized>(
    root: &Path,
    mut targets: Vec<Target<'_>>,
    matcher: &M,
    filter: &ScanFilter,
    skip_binary: bool,
    quiet: bool,
) -> Vec<SearchResult> {
    if filter.has_path_filter() {
        targets.retain(|t| filter.accepts(&t.name));
    }
    targets.sort_unstable_by(|a, b| a.name.cmp(&b.name));

    let Some(limit) = filter.max_count else {
        return scan_many(root, &targets, matcher, skip_binary, quiet)
            .into_iter()
            .flatten()
            .collect();
    };

    let mut results = Vec::new();
    let mut rest = &targets[..];
    let mut chunk = LIMIT_CHUNK;
    while results.len() < limit && !rest.is_empty() {
        let (head, tail) = rest.split_at(chunk.min(rest.len()));
        for file_results in scan_many(root, head, matcher, skip_binary, quiet) {
            results.extend(file_results);
            if results.len() >= limit {
                break;
            }
        }
        rest = tail;
        chunk *= 2;
    }
    results.truncate(limit);
    results
}

/// Scan files from an index candidate ID list.
fn scan_indexed<M: Matcher + ?Sized>(
    reader: &IndexReader,
    root: &Path,
    candidate_ids: &[u32],
    matcher: &M,
    filter: &ScanFilter,
    quiet: bool,
) -> Vec<SearchResult> {
    let targets = candidate_ids
        .iter()
        .map(|&fid| {
            let name = reader.file_path(fid);
            Target {
                path: Path::new(name),
                name: name.into(),
            }
        })
        .collect();
    scan(root, targets, matcher, filter, false, quiet)
}

/// Scan files directly from a file path list (skipping binary files).
fn scan_direct<M: Matcher + ?Sized>(
    root: &Path,
    files: &[PathBuf],
    matcher: &M,
    filter: &ScanFilter,
    quiet: bool,
) -> Vec<SearchResult> {
    let targets = files
        .iter()
        .map(|p| Target {
            path: p.as_path(),
            name: p.to_string_lossy(),
        })
        .collect();
    scan(root, targets, matcher, filter, true, quiet)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Index-based search with path filters and a result limit pushed down
/// into the scan. Dispatches to literal or regex matching.
pub(crate) fn search_with_filter(
    reader: &IndexReader,
    root: &Path,
    pattern: &str,
    regex: bool,
    case_insensitive: bool,
    quiet: bool,
    filter: &ScanFilter,
) -> Result<Vec<SearchResult>> {
    if regex {
        search_regex_filtered(reader, root, pattern, case_insensitive, quiet, filter)
    } else {
        search_literal_filtered(reader, root, pattern, case_insensitive, quiet, filter)
    }
}

/// Direct (non-index) search over `files` with path filters and a limit.
pub(crate) fn search_files_with_filter(
    root: &Path,
    files: &[PathBuf],
    pattern: &str,
    regex: bool,
    case_insensitive: bool,
    quiet: bool,
    filter: &ScanFilter,
) -> Result<Vec<SearchResult>> {
    if regex {
        let matcher = RegexMatcher {
            re: build_regex(pattern, case_insensitive)?,
        };
        return Ok(scan_direct(root, files, &matcher, filter, quiet));
    }
    if case_insensitive && pattern.bytes().any(|b| b > 127) && !quiet {
        eprintln!(
            "xgrep: warning: case-insensitive search with non-ASCII pattern '{}' uses ASCII-only folding",
            pattern
        );
    }
    let matcher = literal_matcher(pattern, case_insensitive);
    Ok(scan_direct(root, files, &*matcher, filter, quiet))
}

/// Literal search. Honors a filter installed with [`with_scan_filter`].
pub(crate) fn search(
    reader: &IndexReader,
    root: &Path,
    pattern: &str,
    case_insensitive: bool,
    quiet: bool,
) -> Result<Vec<SearchResult>> {
    with_active_filter(|f| {
        search_with_filter(reader, root, pattern, false, case_insensitive, quiet, f)
    })
}

fn search_literal_filtered(
    reader: &IndexReader,
    root: &Path,
    pattern: &str,
    case_insensitive: bool,
    quiet: bool,
    filter: &ScanFilter,
) -> Result<Vec<SearchResult>> {
    let pattern_bytes = pattern.as_bytes();
    if pattern_bytes.len() < 3 && !pattern_bytes.is_empty() && !quiet {
        eprintln!(
            "xgrep: warning: pattern '{}' is shorter than 3 characters, index not used (full scan)",
            pattern
        );
    }

    if case_insensitive && pattern.bytes().any(|b| b > 127) && !quiet {
        eprintln!(
            "xgrep: warning: case-insensitive search with non-ASCII pattern '{}' uses ASCII-only folding",
            pattern
        );
    }

    let perf = std::env::var_os("XGREP_PERF_TIMING").is_some();
    let t0 = perf.then(std::time::Instant::now);

    // ASCII-only, matching the content folding in CaseInsensitiveMatcher.
    let search_pattern = if case_insensitive {
        pattern.to_ascii_lowercase()
    } else {
        pattern.to_string()
    };
    let pattern_bytes = search_pattern.as_bytes();
    let trigrams = trigram::extract_trigrams(pattern_bytes);

    let candidate_ids = resolve_literal_candidates(
        reader,
        pattern,
        &search_pattern,
        &trigrams,
        case_insensitive,
    );

    if let Some(t) = t0 {
        eprintln!(
            "[perf] index_phase: {:.1}ms  candidates={} / total_files={}",
            t.elapsed().as_secs_f64() * 1000.0,
            candidate_ids.len(),
            reader.file_count(),
        );
    }
    let t1 = perf.then(std::time::Instant::now);

    let matcher = literal_matcher(pattern, case_insensitive);
    let results = scan_indexed(reader, root, &candidate_ids, &*matcher, filter, quiet);

    if let Some(t) = t1 {
        eprintln!(
            "[perf] scan_phase:  {:.1}ms  results={}",
            t.elapsed().as_secs_f64() * 1000.0,
            results.len(),
        );
    }

    Ok(results)
}

/// Search specified files directly without using the index.
/// Honors a filter installed with [`with_scan_filter`].
pub(crate) fn search_files(
    root: &Path,
    files: &[PathBuf],
    pattern: &str,
    case_insensitive: bool,
    quiet: bool,
) -> Result<Vec<SearchResult>> {
    with_active_filter(|f| {
        search_files_with_filter(root, files, pattern, false, case_insensitive, quiet, f)
    })
}

/// Compile a search regex. `^`/`$` match at line boundaries (LF or CRLF),
/// and matching runs on raw bytes so non-UTF-8 files keep exact offsets.
fn build_regex(pattern: &str, case_insensitive: bool) -> Result<regex::bytes::Regex> {
    RegexBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .multi_line(true)
        .crlf(true)
        .build()
        .map_err(|e| XgrepError::InvalidPattern(e.to_string()))
}

/// Regex search: extract literal trigrams from pattern for index lookup,
/// then verify with full regex on candidate files.
/// Honors a filter installed with [`with_scan_filter`].
pub(crate) fn search_regex(
    reader: &IndexReader,
    root: &Path,
    pattern: &str,
    case_insensitive: bool,
    quiet: bool,
) -> Result<Vec<SearchResult>> {
    with_active_filter(|f| {
        search_with_filter(reader, root, pattern, true, case_insensitive, quiet, f)
    })
}

fn search_regex_filtered(
    reader: &IndexReader,
    root: &Path,
    pattern: &str,
    case_insensitive: bool,
    quiet: bool,
    filter: &ScanFilter,
) -> Result<Vec<SearchResult>> {
    let re = build_regex(pattern, case_insensitive)?;
    let candidate_ids = resolve_regex_candidates(reader, pattern, case_insensitive, quiet);
    let matcher = RegexMatcher { re };
    Ok(scan_indexed(
        reader,
        root,
        &candidate_ids,
        &matcher,
        filter,
        quiet,
    ))
}

/// Also add search_files_regex for --changed/--since with regex.
/// Honors a filter installed with [`with_scan_filter`].
pub(crate) fn search_files_regex(
    root: &Path,
    files: &[PathBuf],
    pattern: &str,
    case_insensitive: bool,
    quiet: bool,
) -> Result<Vec<SearchResult>> {
    with_active_filter(|f| {
        search_files_with_filter(root, files, pattern, true, case_insensitive, quiet, f)
    })
}

/// Extract literal substrings from a regex pattern (simple heuristic)
/// Finds the longest run of non-special characters (must be >= 2 chars to be useful)
#[cfg(test)]
fn extract_literals(pattern: &str) -> String {
    let special = [
        '[', ']', '(', ')', '{', '}', '.', '*', '+', '?', '|', '^', '$', '\\',
    ];
    let mut best = String::new();
    let mut current = String::new();

    for c in pattern.chars() {
        if special.contains(&c) {
            if current.len() > best.len() {
                best = current.clone();
            }
            current.clear();
        } else {
            current.push(c);
        }
    }
    if current.len() > best.len() {
        best = current;
    }
    // Single character literals are not useful for trigram filtering
    if best.len() <= 1 {
        return String::new();
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::builder;
    use tempfile::tempdir;

    #[test]
    fn test_search_finds_match() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.rs"), "fn handle_auth() {}\nfn other() {}").unwrap();
        fs::write(root.join("b.rs"), "fn main() {}").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        let results = search(&reader, root, "handle_auth", false, false).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].file.contains("a.rs"));
        assert_eq!(results[0].line_number, 1);
    }

    #[test]
    fn test_search_no_match() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.rs"), "fn main() {}").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        let results = search(&reader, root, "nonexistent_xyz", false, false).unwrap();
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_search_short_pattern() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "ok then").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        let results = search(&reader, root, "ok", false, false).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_search_2char_prefix_optimization() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("has_fn.rs"), "fn hello() {}\nfn world() {}").unwrap();
        fs::write(root.join("no_match.txt"), "xyz abc def ghi jkl").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();

        let results = search(&reader, root, "fn", false, false).unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.file.contains("has_fn.rs")));
    }

    #[test]
    fn test_search_2char_no_prefix_match_fallback() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "zq is rare").unwrap();
        fs::write(root.join("b.txt"), "no match here").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        let results = search(&reader, root, "zq", false, false).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].file.contains("a.txt"));
    }

    #[test]
    fn test_search_2char_no_match_returns_empty() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        let results = search(&reader, root, "zq", false, false).unwrap();
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_lookup_trigram_prefix_returns_subset() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.rs"), "fn hello() {}").unwrap();
        fs::write(root.join("b.txt"), "xyz abc").unwrap();
        fs::write(root.join("c.txt"), "qrs tuv").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();

        let candidates = reader.lookup_trigram_prefix(*b"fn");
        assert!(!candidates.is_empty());
        assert!(candidates.len() < reader.file_count() as usize);
    }

    #[test]
    fn test_search_case_insensitive() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.rs"), "fn HandleAuth() {}\nfn other() {}").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        let results = search(&reader, root, "handleauth", true, false).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].line.contains("HandleAuth"));
    }

    #[test]
    fn test_search_files_direct() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.rs"), "fn hello() {}\nfn world() {}").unwrap();
        fs::write(root.join("b.rs"), "fn other() {}").unwrap();
        let files = vec![PathBuf::from("a.rs")];
        let results = search_files(root, &files, "hello", false, false).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].file.contains("a.rs"));
    }

    #[test]
    fn test_search_files_case_insensitive() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.rs"), "fn HandleAuth() {}").unwrap();
        let files = vec![PathBuf::from("a.rs")];
        let results = search_files(root, &files, "handleauth", true, false).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_extract_literals() {
        assert_eq!(extract_literals("handle[A-Z]\\w+"), "handle");
        assert_eq!(extract_literals(".*foo"), "foo");
        assert_eq!(extract_literals("hello"), "hello");
        assert_eq!(extract_literals("a.b"), "");
    }

    #[test]
    fn test_search_regex() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("a.rs"),
            "fn handle_auth() {}\nfn handle_user() {}\nfn other() {}",
        )
        .unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        let results = search_regex(&reader, root, "handle_\\w+", false, false).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_search_regex_case_insensitive() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.rs"), "fn HandleAuth() {}\nfn other() {}").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        let results = search_regex(&reader, root, "handleauth", true, false).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_search_empty_pattern() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        let results = search(&reader, root, "", false, false).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_search_pattern_longer_than_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hi").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        let results = search(
            &reader,
            root,
            "this is much longer than the file content",
            false,
            false,
        )
        .unwrap();
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_search_multiple_matches_same_line() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "foo bar foo baz foo").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        let results = search(&reader, root, "foo", false, false).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].line_number, 1);
    }

    #[test]
    fn test_search_multiline_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "line1\nline2\nline3\nline4\nline5").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        let results = search(&reader, root, "line3", false, false).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].line_number, 3);
    }

    #[test]
    fn test_search_special_characters() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "price is $100.00\nanother line").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        let results = search(&reader, root, "$100.00", false, false).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_search_regex_invalid_pattern() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        let result = search_regex(&reader, root, "[invalid", false, false);
        assert!(result.is_err());
    }

    #[test]
    fn test_search_regex_empty_match() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello\nworld").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        let results = search_regex(&reader, root, ".*", false, false).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_search_deleted_file_after_index() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world").unwrap();
        fs::write(root.join("b.txt"), "hello earth").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        fs::remove_file(root.join("a.txt")).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        let results = search(&reader, root, "hello", false, false).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].file.contains("b.txt"));
    }

    #[test]
    fn test_search_utf8_pattern() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("a.txt"),
            "this has some Japanese: テスト\nand more: テスト2",
        )
        .unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        let results = search(&reader, root, "テスト", false, false).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_search_case_insensitive_fallback_all_uppercase() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "THIS IS UPPERCASE\nlowercase here").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        let results = search(&reader, root, "THIS IS UPPERCASE", true, false).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_search_files_nonexistent_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let files = vec![PathBuf::from("nonexistent.txt")];
        let results = search_files(root, &files, "hello", false, false).unwrap();
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_search_files_empty_list() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let files: Vec<PathBuf> = vec![];
        let results = search_files(root, &files, "hello", false, false).unwrap();
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_search_files_regex() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("a.rs"),
            "fn handle_auth() {}\nfn handle_user() {}",
        )
        .unwrap();
        let files = vec![PathBuf::from("a.rs")];
        let results = search_files_regex(root, &files, "handle_\\w+", false, false).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_search_case_insensitive_mixed_case_content() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("upper.txt"), "HELLO WORLD").unwrap();
        fs::write(root.join("lower.txt"), "hello world").unwrap();
        fs::write(root.join("mixed.txt"), "HeLLo WoRLd").unwrap();
        fs::write(root.join("none.txt"), "goodbye earth").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        let results = search(&reader, root, "hello world", true, false).unwrap();
        assert_eq!(results.len(), 3);
        let mut files: Vec<&str> = results.iter().map(|r| r.file.as_ref()).collect();
        files.sort();
        assert!(files.iter().any(|f| f.contains("upper.txt")));
        assert!(files.iter().any(|f| f.contains("lower.txt")));
        assert!(files.iter().any(|f| f.contains("mixed.txt")));
    }

    #[test]
    fn test_search_files_regex_invalid() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.rs"), "hello").unwrap();
        let files = vec![PathBuf::from("a.rs")];
        let result = search_files_regex(root, &files, "[invalid", false, false);
        assert!(result.is_err());
    }

    #[test]
    fn test_search_case_insensitive_uses_index() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        for i in 0..20 {
            fs::write(
                root.join(format!("file{}.rs", i)),
                format!("content number {}", i),
            )
            .unwrap();
        }
        fs::write(root.join("target.rs"), "fn HandleAuth() {}\nfn other() {}").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();

        let results = search(&reader, root, "handleauth", true, false).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].line.contains("HandleAuth"));
    }

    #[test]
    fn test_search_single_char_still_works() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "x marks the spot").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        let results = search(&reader, root, "x", false, false).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_regex_dot_star_still_works() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        let results = search_regex(&reader, root, ".*", false, false).unwrap();
        assert!(!results.is_empty());
    }

    #[test]
    fn test_case_insensitive_long_pattern_fallback() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("a.txt"),
            "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz",
        )
        .unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();

        let start = std::time::Instant::now();
        let results = search(&reader, root, "abcdefghijklmnop", true, false).unwrap();
        let elapsed = start.elapsed();

        assert!(
            elapsed.as_secs() < 1,
            "Case-insensitive search took too long: {:?}",
            elapsed
        );
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_case_insensitive_no_match_returns_empty() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        let results = search(&reader, root, "xyznonexistent", true, false).unwrap();
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_contains_case_insensitive() {
        assert!(contains_case_insensitive("Hello World", "hello"));
        assert!(contains_case_insensitive("HELLO WORLD", "hello world"));
        assert!(contains_case_insensitive("hello", "hello"));
        assert!(!contains_case_insensitive("hello", "xyz"));
        assert!(contains_case_insensitive("anything", ""));
        assert!(!contains_case_insensitive("hi", "longer"));
    }

    #[test]
    fn test_search_files_skips_binary() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("binary.bin"), b"hello\x00world").unwrap();
        fs::write(root.join("text.txt"), "hello world").unwrap();

        let files = vec![PathBuf::from("binary.bin"), PathBuf::from("text.txt")];
        let results = search_files(root, &files, "hello", false, false).unwrap();

        assert_eq!(results.len(), 1);
        assert!(results[0].file.contains("text.txt"));
    }

    #[test]
    fn test_search_files_regex_skips_binary() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("binary.bin"), b"hello\x00world").unwrap();
        fs::write(root.join("text.txt"), "hello world").unwrap();

        let files = vec![PathBuf::from("binary.bin"), PathBuf::from("text.txt")];
        let results = search_files_regex(root, &files, "hello", false, false).unwrap();

        assert_eq!(results.len(), 1);
        assert!(results[0].file.contains("text.txt"));
    }

    #[test]
    fn test_case_insensitive_ascii_only_still_works() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "Hello WORLD").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        let results = search(&reader, root, "hello world", true, false).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_regex_anchors_match_per_line() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("a.rs"),
            "// header\nuse std::fs;\nfoo();\nbar();\n",
        )
        .unwrap();
        fs::write(root.join("b.rs"), "x\r\nuse std::io;\r\nbaz();\r\n").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();

        let results = search_regex(&reader, root, "^use std", false, false).unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.line_number == 2));

        let results = search_regex(&reader, root, r"\);$", false, false).unwrap();
        let got: Vec<(&str, usize)> = results
            .iter()
            .map(|r| (r.file.as_ref(), r.line_number))
            .collect();
        assert_eq!(got, vec![("a.rs", 3), ("a.rs", 4), ("b.rs", 3)]);

        let files = vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")];
        let results = search_files_regex(root, &files, "^use std", false, false).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_regex_non_utf8_line_numbers() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("a.txt"),
            b"\xff\xff\xff\xff\xff\xff\nab\ncd\nef\ntarget here\nzz\n",
        )
        .unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        let results = search_regex(&reader, root, "target", false, false).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].line_number, 5);
        assert_eq!(results[0].line, "target here");
    }

    #[test]
    fn test_line_cursor() {
        let content = b"line1\nline2\nline3";
        let mut c = LineCursor::new(content);
        assert_eq!(c.locate(0), (1, 0, 5));
        assert_eq!(c.locate(3), (1, 0, 5));
        assert_eq!(c.locate(6), (2, 6, 11));
        assert_eq!(c.locate(8), (2, 6, 11));
        assert_eq!(c.locate(12), (3, 12, 17));
        assert_eq!(c.locate(17), (3, 12, 17));
    }

    #[test]
    fn test_collect_lines_one_result_per_line() {
        let m = LiteralMatcher::new(b"ab");
        let r = m.find_matches(b"ab ab\nxx\n\nab\nabab", "f");
        let lines: Vec<(usize, &str)> =
            r.iter().map(|r| (r.line_number, r.line.as_str())).collect();
        assert_eq!(lines, vec![(1, "ab ab"), (4, "ab"), (5, "abab")]);
    }

    #[test]
    fn test_case_insensitive_matcher_lines() {
        let m = CaseInsensitiveMatcher::new(b"hello");
        let content = b"x\nHeLLo there hello\nnope\nsay HELLO";
        let r = m.find_matches(content, "f");
        let lines: Vec<(usize, &str)> =
            r.iter().map(|r| (r.line_number, r.line.as_str())).collect();
        assert_eq!(lines, vec![(2, "HeLLo there hello"), (4, "say HELLO")]);
    }

    #[test]
    fn test_case_insensitive_without_letters_uses_literal_path() {
        let m = literal_matcher("123", true);
        let r = m.find_matches(b"a\nx123\n", "f");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].line_number, 2);
    }

    #[test]
    fn test_regex_empty_match_ignores_trailing_newline() {
        let re = build_regex(".*", false).unwrap();
        let m = RegexMatcher { re };
        let r = m.find_matches(b"hello\nworld\n", "f");
        assert_eq!(r.len(), 2);
        let r = m.find_matches(b"a\n\nb", "f");
        assert_eq!(r.len(), 3);
    }

    struct CountingMatcher {
        inner: LiteralMatcher,
        files: std::sync::Mutex<Vec<String>>,
    }

    impl Matcher for CountingMatcher {
        fn find_matches(&self, content: &[u8], rel_path: &str) -> Vec<SearchResult> {
            self.files.lock().unwrap().push(rel_path.to_string());
            self.inner.find_matches(content, rel_path)
        }
    }

    fn counting(pattern: &str) -> CountingMatcher {
        CountingMatcher {
            inner: LiteralMatcher::new(pattern.as_bytes()),
            files: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn many_files(n: usize) -> (tempfile::TempDir, IndexReader) {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("docs")).unwrap();
        for i in 0..n {
            let body = format!("needle one {i}\nother\nneedle two {i}\n");
            fs::write(root.join(format!("src/f{i:03}.rs")), &body).unwrap();
            fs::write(root.join(format!("docs/d{i:03}.md")), &body).unwrap();
        }
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        (dir, reader)
    }

    #[test]
    fn test_path_filters_skip_files_before_reading() {
        let (dir, reader) = many_files(100);
        let root = dir.path();
        let ids: Vec<u32> = (0..reader.file_count()).collect();

        let filter = ScanFilter {
            extensions: Some(vec!["md"]),
            ..Default::default()
        };
        let m = counting("needle");
        let results = scan_indexed(&reader, root, &ids, &m, &filter, true);
        assert_eq!(results.len(), 200);
        let read = m.files.lock().unwrap();
        assert_eq!(read.len(), 100);
        assert!(read.iter().all(|f| f.ends_with(".md")));

        let filter = ScanFilter {
            path_substring: Some("f00".to_string()),
            globs: Some(GlobFilter::new(&["*.rs".to_string(), "!*f009.rs".to_string()]).unwrap()),
            ..Default::default()
        };
        let m = counting("needle");
        let results = scan_indexed(&reader, root, &ids, &m, &filter, true);
        assert_eq!(m.files.lock().unwrap().len(), 9);
        assert_eq!(results.len(), 18);
    }

    #[test]
    fn test_max_count_stops_early_and_keeps_order() {
        let (dir, reader) = many_files(150);
        let root = dir.path();
        let ids: Vec<u32> = (0..reader.file_count()).rev().collect();

        let full = scan_indexed(
            &reader,
            root,
            &ids,
            &counting("needle"),
            &ScanFilter::default(),
            true,
        );
        assert_eq!(full.len(), 600);
        let sorted = full
            .windows(2)
            .all(|w| (&w[0].file, w[0].line_number) < (&w[1].file, w[1].line_number));
        assert!(sorted, "results must be sorted by (file, line)");

        for limit in [0usize, 1, 3, 130, 599, 600, 1000] {
            let filter = ScanFilter {
                max_count: Some(limit),
                ..Default::default()
            };
            let m = counting("needle");
            let limited = scan_indexed(&reader, root, &ids, &m, &filter, true);
            let expected: Vec<(String, usize)> = full
                .iter()
                .take(limit)
                .map(|r| (r.file.to_string(), r.line_number))
                .collect();
            let got: Vec<(String, usize)> = limited
                .iter()
                .map(|r| (r.file.to_string(), r.line_number))
                .collect();
            assert_eq!(got, expected, "limit {limit}");
            if limit <= 3 {
                assert!(m.files.lock().unwrap().len() <= LIMIT_CHUNK);
            }
        }
    }

    #[test]
    fn test_scoped_filter_applies_to_legacy_entry_points() {
        let (dir, reader) = many_files(10);
        let root = dir.path();
        let filter = ScanFilter {
            extensions: Some(vec!["rs"]),
            max_count: Some(3),
            ..Default::default()
        };
        let results = with_scan_filter(filter, || {
            let a = search(&reader, root, "needle", false, true).unwrap();
            let b = search_regex(&reader, root, "needle", false, true).unwrap();
            let files = vec![PathBuf::from("docs/d000.md"), PathBuf::from("src/f000.rs")];
            let c = search_files(root, &files, "needle", false, true).unwrap();
            (a, b, c)
        });
        assert_eq!(results.0.len(), 3);
        assert_eq!(results.1.len(), 3);
        assert_eq!(results.2.len(), 2);
        assert!(results.0.iter().all(|r| r.file.ends_with(".rs")));
        assert!(results.2.iter().all(|r| r.file.ends_with(".rs")));
        // Scope ended: no filter applies.
        assert_eq!(
            search(&reader, root, "needle", false, true).unwrap().len(),
            40
        );
    }
}
