//! Content search: compiled queries, per-file matching and an ordered,
//! streaming scan engine.
//!
//! Candidate files are scanned in parallel but handed to the caller one
//! file at a time in path order, so results can be written as soon as each
//! file completes. Memory is bounded by the files in flight, not by the
//! total output.

use crate::candidates::{intersect_postings, resolve_literal_candidates, resolve_regex_candidates};
use crate::error::{Result, XgrepError};
use crate::globfilter::GlobFilter;
use crate::index::reader::IndexReader;
use memchr::memmem;
use regex::bytes::RegexBuilder;
use std::borrow::Cow;
use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

thread_local! {
    static LOWER_BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static READ_BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
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

/// One matching line.
#[derive(Debug, Clone)]
pub struct SearchResult {
    /// File path relative to the search root.
    pub file: Arc<str>,
    /// 1-based line number.
    pub line_number: usize,
    /// Line text (without the line terminator).
    pub line: String,
    /// Byte offset of the first match within `line` (0-based).
    pub column: usize,
}

impl SearchResult {
    /// Create a result for a match at the start of `line` (column 0).
    pub fn new(file: impl Into<Arc<str>>, line_number: usize, line: impl Into<String>) -> Self {
        Self {
            file: file.into(),
            line_number,
            line: line.into(),
            column: 0,
        }
    }
}

/// What a streaming search computes for each matching file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OutputMode {
    /// Every matching line, with its text (the default).
    #[default]
    Lines,
    /// Only the number of matching lines; no line text is built.
    Count,
    /// Only whether the file matches; scanning stops at its first match.
    FilesOnly,
}

/// Results for one file, delivered in path order by the streaming search
/// APIs ([`Xgrep::search_stream`](crate::Xgrep::search_stream)).
#[derive(Debug, Clone)]
pub struct FileMatches {
    /// File path relative to the search root.
    pub file: Arc<str>,
    /// Number of matching lines delivered for this file. In
    /// [`OutputMode::FilesOnly`] this is a lower bound (normally 1).
    pub count: usize,
    /// Matching lines in line order ([`OutputMode::Lines`] only; empty otherwise).
    pub lines: Vec<SearchResult>,
}

// ---------------------------------------------------------------------------
// Matchers
// ---------------------------------------------------------------------------

/// File contents prepared for matching: the raw bytes, plus an
/// ASCII-lowercased copy with the same layout (only filled when a
/// case-insensitive literal matcher needs it; otherwise equal to `raw`).
#[derive(Clone, Copy)]
struct Hay<'a> {
    raw: &'a [u8],
    lower: &'a [u8],
}

trait Matcher: Send + Sync {
    /// True if `find_at` reads `Hay::lower`.
    fn needs_lower(&self) -> bool {
        false
    }

    /// Next match `(start, end)` at or after `from`.
    fn find_at(&self, hay: &Hay<'_>, from: usize) -> Option<(usize, usize)>;

    /// Byte column (within the line starting at `line_start`) of the match
    /// found at `start`.
    fn column(&self, _hay: &Hay<'_>, start: usize, line_start: usize) -> usize {
        start - line_start
    }
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
    fn find_at(&self, hay: &Hay<'_>, from: usize) -> Option<(usize, usize)> {
        let len = self.finder.needle().len();
        self.finder
            .find(&hay.raw[from..])
            .map(|p| (from + p, from + p + len))
    }
}

/// Case-insensitive (ASCII folding) literal string matcher. Searches the
/// ASCII-lowercased copy of the file.
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
    fn needs_lower(&self) -> bool {
        true
    }

    fn find_at(&self, hay: &Hay<'_>, from: usize) -> Option<(usize, usize)> {
        let len = self.finder.needle().len();
        self.finder
            .find(&hay.lower[from..])
            .map(|p| (from + p, from + p + len))
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
    /// Capture group holding the user's pattern (word mode wraps the pattern
    /// with boundary characters that are part of the match).
    column_group: Option<usize>,
}

impl Matcher for RegexMatcher {
    fn find_at(&self, hay: &Hay<'_>, from: usize) -> Option<(usize, usize)> {
        self.re.find_at(hay.raw, from).map(|m| (m.start(), m.end()))
    }

    fn column(&self, hay: &Hay<'_>, start: usize, line_start: usize) -> usize {
        let pos = self.column_group.and_then(|g| {
            self.re
                .captures_at(hay.raw, start)
                .and_then(|c| c.get(g))
                .map(|m| m.start())
        });
        pos.unwrap_or(start).max(line_start) - line_start
    }
}

/// Call `f(start, line_end)` for the first match on each matching line, in
/// order, until `f` returns false.
///
/// After a hit, searching resumes at the next line, so each line is searched
/// at most once.
fn for_each_match_line(hay: &Hay<'_>, m: &dyn Matcher, mut f: impl FnMut(usize, usize) -> bool) {
    let raw = hay.raw;
    let mut from = 0usize;
    while from <= raw.len() {
        let Some((start, end)) = m.find_at(hay, from) else {
            break;
        };
        // An empty match after a trailing newline is not a line.
        if start == raw.len() && start > 0 && raw[start - 1] == b'\n' {
            break;
        }
        let le = memchr::memchr(b'\n', &raw[start..]).map_or(raw.len(), |i| start + i);
        if !f(start, le) {
            break;
        }
        from = end.max(le + 1);
    }
}

fn is_match(hay: &Hay<'_>, m: &dyn Matcher) -> bool {
    let mut found = false;
    for_each_match_line(hay, m, |_, _| {
        found = true;
        false
    });
    found
}

fn count_lines(hay: &Hay<'_>, m: &dyn Matcher) -> usize {
    let mut n = 0;
    for_each_match_line(hay, m, |_, _| {
        n += 1;
        true
    });
    n
}

fn collect_lines(hay: &Hay<'_>, m: &dyn Matcher, file: &Arc<str>) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let mut cursor = LineCursor::new(hay.raw);
    for_each_match_line(hay, m, |start, _| {
        let (line_number, ls, le) = cursor.locate(start);
        let line = std::str::from_utf8(&hay.raw[ls..le]).unwrap_or("<binary>");
        results.push(SearchResult {
            file: Arc::clone(file),
            line_number,
            line: line.to_string(),
            column: m.column(hay, start, ls),
        });
        true
    });
    results
}

/// Run `f` with `content` prepared for matching (lowercased copy only when
/// `lower` is requested, reusing a thread-local buffer).
fn with_hay<R>(content: &[u8], lower: bool, f: impl FnOnce(&Hay<'_>) -> R) -> R {
    if !lower {
        return f(&Hay {
            raw: content,
            lower: content,
        });
    }
    LOWER_BUF.with(|cell| {
        let mut buf = cell.borrow_mut();
        buf.clear();
        buf.extend_from_slice(content);
        buf.make_ascii_lowercase();
        let r = f(&Hay {
            raw: content,
            lower: &buf,
        });
        release_if_large(&mut buf);
        r
    })
}

/// Per-thread scratch buffers larger than this are freed after use, so a
/// few huge files do not pin memory in every worker thread.
const SCRATCH_KEEP_BYTES: usize = 1 << 20;

fn release_if_large(buf: &mut Vec<u8>) {
    if buf.capacity() > SCRATCH_KEEP_BYTES {
        *buf = Vec::new();
    }
}

// ---------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------

/// One pattern of a query, as executed (after word-boundary wrapping).
#[derive(Debug, Clone)]
pub(crate) struct PatternSpec {
    pub(crate) pattern: String,
    pub(crate) regex: bool,
    /// The pattern was wrapped for word matching; capture group 1 holds the
    /// user's pattern (used for the match column).
    pub(crate) word: bool,
}

/// A file-level boolean query: files must match `main` and every `and`
/// pattern, and no `not` pattern. Only `main` produces result lines.
#[derive(Debug, Clone)]
pub(crate) struct QuerySpec {
    pub(crate) main: PatternSpec,
    pub(crate) and: Vec<PatternSpec>,
    pub(crate) not: Vec<PatternSpec>,
    pub(crate) case_insensitive: bool,
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

fn compile_pattern(
    spec: &PatternSpec,
    case_insensitive: bool,
    quiet: bool,
) -> Result<Box<dyn Matcher>> {
    if spec.regex {
        return Ok(Box::new(RegexMatcher {
            re: build_regex(&spec.pattern, case_insensitive)?,
            column_group: spec.word.then_some(1),
        }));
    }
    if case_insensitive && spec.pattern.bytes().any(|b| b > 127) && !quiet {
        eprintln!(
            "xgrep: warning: case-insensitive search with non-ASCII pattern '{}' uses ASCII-only folding",
            spec.pattern
        );
    }
    Ok(literal_matcher(&spec.pattern, case_insensitive))
}

/// A query ready to run against file contents.
pub(crate) struct CompiledQuery {
    spec: QuerySpec,
    main: Box<dyn Matcher>,
    and: Vec<Box<dyn Matcher>>,
    not: Vec<Box<dyn Matcher>>,
    needs_lower: bool,
}

impl CompiledQuery {
    pub(crate) fn new(spec: QuerySpec, quiet: bool) -> Result<Self> {
        let ci = spec.case_insensitive;
        let main = compile_pattern(&spec.main, ci, quiet)?;
        let and = spec
            .and
            .iter()
            .map(|p| compile_pattern(p, ci, quiet))
            .collect::<Result<Vec<_>>>()?;
        let not = spec
            .not
            .iter()
            .map(|p| compile_pattern(p, ci, quiet))
            .collect::<Result<Vec<_>>>()?;
        let needs_lower = std::iter::once(&main)
            .chain(&and)
            .chain(&not)
            .any(|m| m.needs_lower());
        Ok(Self {
            spec,
            main,
            and,
            not,
            needs_lower,
        })
    }

    /// Candidate file IDs: the intersection of the main pattern's and every
    /// AND pattern's posting-list candidates. NOT patterns cannot prune by
    /// trigrams (a file may contain all of a pattern's trigrams without
    /// containing the pattern); they are verified on file contents.
    pub(crate) fn candidates(&self, reader: &IndexReader, quiet: bool) -> Vec<u32> {
        let ci = self.spec.case_insensitive;
        let main = pattern_candidates(reader, &self.spec.main, ci, quiet);
        if self.spec.and.is_empty() || main.is_empty() {
            return main;
        }
        let mut lists = vec![main];
        for p in &self.spec.and {
            let c = pattern_candidates(reader, p, ci, true);
            if c.is_empty() {
                return c;
            }
            lists.push(c);
        }
        let refs: Vec<&[u32]> = lists.iter().map(|v| v.as_slice()).collect();
        intersect_postings(&refs)
    }

    /// Evaluate the query on one file.
    fn eval(&self, content: &[u8], file: &str, mode: OutputMode) -> Option<FileMatches> {
        with_hay(content, self.needs_lower, |hay| {
            if !self.and.iter().all(|m| is_match(hay, &**m)) {
                return None;
            }
            // Check the main pattern before scanning whole files for NOT
            // patterns (a NOT pattern that is absent is the expensive case).
            if !self.not.is_empty()
                && (!is_match(hay, &*self.main) || self.not.iter().any(|m| is_match(hay, &**m)))
            {
                return None;
            }
            let file: Arc<str> = Arc::from(file);
            match mode {
                OutputMode::Lines => {
                    let lines = collect_lines(hay, &*self.main, &file);
                    if lines.is_empty() {
                        return None;
                    }
                    Some(FileMatches {
                        file,
                        count: lines.len(),
                        lines,
                    })
                }
                OutputMode::Count => {
                    let count = count_lines(hay, &*self.main);
                    (count > 0).then(|| FileMatches {
                        file,
                        count,
                        lines: Vec::new(),
                    })
                }
                OutputMode::FilesOnly => is_match(hay, &*self.main).then(|| FileMatches {
                    file,
                    count: 1,
                    lines: Vec::new(),
                }),
            }
        })
    }
}

/// Candidate file IDs for one pattern.
fn pattern_candidates(
    reader: &IndexReader,
    spec: &PatternSpec,
    case_insensitive: bool,
    quiet: bool,
) -> Vec<u32> {
    if spec.regex {
        return resolve_regex_candidates(reader, &spec.pattern, case_insensitive, quiet);
    }
    let pattern = spec.pattern.as_str();
    if let Some(w) = short_literal_warning(pattern) {
        if !quiet {
            eprintln!("{}", w);
        }
    }
    // ASCII-only, matching the content folding in CaseInsensitiveMatcher.
    let search_pattern = if case_insensitive {
        pattern.to_ascii_lowercase()
    } else {
        pattern.to_string()
    };
    resolve_literal_candidates(reader, &search_pattern)
}

/// Warning for literals the index cannot narrow: only 1-byte patterns scan
/// every file (2-byte literals use the trigram prefix range).
fn short_literal_warning(pattern: &str) -> Option<String> {
    (pattern.len() == 1).then(|| {
        format!(
            "xgrep: warning: pattern '{}' is a single character, index not used (full scan)",
            pattern
        )
    })
}

/// Matches single lines (e.g. diff lines or commit message lines) with the
/// same semantics as file search.
pub(crate) struct LineMatcher {
    m: Box<dyn Matcher>,
}

impl LineMatcher {
    pub(crate) fn new(spec: &PatternSpec, case_insensitive: bool, quiet: bool) -> Result<Self> {
        Ok(Self {
            m: compile_pattern(spec, case_insensitive, quiet)?,
        })
    }

    /// Byte column of the first match in `line` (which must not contain a
    /// newline), or `None`.
    pub(crate) fn find(&self, line: &[u8]) -> Option<usize> {
        with_hay(line, self.m.needs_lower(), |hay| {
            let mut col = None;
            for_each_match_line(hay, &*self.m, |start, _| {
                col = Some(self.m.column(hay, start, 0));
                false
            });
            col
        })
    }
}

// ---------------------------------------------------------------------------
// Path filters and limits (applied before files are read)
// ---------------------------------------------------------------------------

/// Path-based filters and a result limit, applied to candidate files before
/// they are read.
#[derive(Default, Clone)]
pub(crate) struct ScanFilter {
    /// Keep only files with one of these extensions (`-t`).
    pub(crate) extensions: Option<Vec<&'static str>>,
    /// Keep only paths containing this substring.
    pub(crate) path_substring: Option<String>,
    /// Include/exclude globs (`-g`).
    pub(crate) globs: Option<GlobFilter>,
    /// Skip these exact paths (e.g. files rescanned outside the index).
    pub(crate) exclude_paths: Option<std::collections::HashSet<String>>,
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
        if let Some(excluded) = &self.exclude_paths {
            if excluded.contains(path) {
                return false;
            }
        }
        true
    }

    fn has_path_filter(&self) -> bool {
        self.extensions.is_some()
            || self.path_substring.is_some()
            || self.globs.is_some()
            || self.exclude_paths.is_some()
    }
}

// ---------------------------------------------------------------------------
// Scan engine
// ---------------------------------------------------------------------------

/// Below this many files, scan sequentially on the calling thread: starting
/// worker threads costs more than it saves.
const PARALLEL_THRESHOLD: usize = 64;
/// Maximum number of items scanned ahead of the next one to emit. Large, so
/// one slow file does not stall the other workers.
const MAX_AHEAD: usize = 4096;
/// Approximate bytes of finished-but-unemitted results above which workers
/// pause (except for the item the consumer waits for). This bounds the
/// memory of a streaming search independently of the total output size.
const PENDING_BUDGET_BYTES: usize = 32 << 20;

/// A file to scan: its path relative to the root and its display name.
pub(crate) struct Target<'a> {
    path: Cow<'a, Path>,
    name: Cow<'a, str>,
    /// Skip files containing NUL bytes (files not vetted by the indexer).
    skip_binary: bool,
}

/// Index candidates for `q` that pass `filter`'s path filters.
pub(crate) fn index_targets<'a>(
    reader: &'a IndexReader,
    q: &CompiledQuery,
    filter: &ScanFilter,
    quiet: bool,
) -> Vec<Target<'a>> {
    let perf = std::env::var_os("XGREP_PERF_TIMING").is_some();
    let t0 = perf.then(std::time::Instant::now);
    let ids = q.candidates(reader, quiet);
    if let Some(t) = t0 {
        eprintln!(
            "[perf] index_phase: {:.1}ms  candidates={} / total_files={}",
            t.elapsed().as_secs_f64() * 1000.0,
            ids.len(),
            reader.file_count(),
        );
    }
    ids.into_iter()
        .map(|fid| reader.file_path(fid))
        .filter(|name| !filter.has_path_filter() || filter.accepts(name))
        .map(|name| Target {
            path: Cow::Borrowed(Path::new(name)),
            name: Cow::Borrowed(name),
            skip_binary: false,
        })
        .collect()
}

/// Files read directly (not through the index) that pass `filter`'s path
/// filters. Binary files are skipped when scanned.
pub(crate) fn file_targets<'a>(files: &'a [PathBuf], filter: &ScanFilter) -> Vec<Target<'a>> {
    files
        .iter()
        .map(|p| Target {
            path: Cow::Borrowed(p.as_path()),
            name: p.to_string_lossy(),
            skip_binary: true,
        })
        .filter(|t| !filter.has_path_filter() || filter.accepts(&t.name))
        .collect()
}

fn scan_one(
    root: &Path,
    target: &Target<'_>,
    q: &CompiledQuery,
    mode: OutputMode,
    quiet: bool,
) -> Option<FileMatches> {
    use std::io::Read;

    let full_path = root.join(&target.path);
    // Read into a reused per-thread buffer: allocating a fresh buffer per
    // file leaves freed memory cached in every worker's allocator heap.
    READ_BUF.with(|cell| {
        let mut content = cell.borrow_mut();
        content.clear();
        let read = fs::File::open(&full_path).and_then(|mut f| f.read_to_end(&mut content));
        let result = match read {
            Err(e) => {
                if !quiet {
                    eprintln!("xgrep: {}: {}", full_path.display(), e);
                }
                None
            }
            Ok(_) if target.skip_binary && memchr::memchr(0, &content).is_some() => None,
            Ok(_) => q.eval(&content, &target.name, mode),
        };
        release_if_large(&mut content);
        result
    })
}

/// Scan `targets` and deliver each matching file to `sink` in path order.
///
/// Targets are sorted by name (duplicates dropped, direct reads preferred
/// over index entries). `max_count` limits the total number of matching
/// lines delivered; scanning stops as soon as it is reached or `sink`
/// returns false.
pub(crate) fn run(
    root: &Path,
    mut targets: Vec<Target<'_>>,
    q: &CompiledQuery,
    max_count: Option<usize>,
    mode: OutputMode,
    quiet: bool,
    sink: &mut dyn FnMut(FileMatches) -> bool,
) {
    // Direct reads (skip_binary) sort first among equal names and win dedup.
    targets.sort_unstable_by(|a, b| a.name.cmp(&b.name).then(b.skip_binary.cmp(&a.skip_binary)));
    targets.dedup_by(|later, earlier| later.name == earlier.name);

    let mut remaining = max_count.unwrap_or(usize::MAX);
    if remaining == 0 {
        return;
    }
    // A global line limit needs exact counts, which FilesOnly does not compute.
    let scan_mode = if mode == OutputMode::FilesOnly && max_count.is_some() {
        OutputMode::Count
    } else {
        mode
    };
    let mut emit = |mut fm: FileMatches| -> bool {
        if fm.lines.len() > remaining {
            fm.lines.truncate(remaining);
        }
        fm.count = fm.count.min(remaining);
        remaining -= fm.count;
        sink(fm) && remaining > 0
    };
    // Only line results can be large. Reserve budget for them up front from
    // the file size, so several huge files are not expanded at once.
    let lines_mode = scan_mode == OutputMode::Lines;
    let cost = |t: &Target<'_>| {
        if !lines_mode {
            return 0;
        }
        fs::metadata(root.join(&t.path)).map_or(0, |m| {
            usize::try_from(m.len())
                .unwrap_or(usize::MAX)
                .saturating_mul(2)
        })
    };
    ordered_for_each(
        &targets,
        |t| scan_one(root, t, q, scan_mode, quiet),
        cost,
        FileMatches::approx_bytes,
        &mut emit,
    );
}

impl FileMatches {
    /// Approximate heap + inline size, for the pending-results budget.
    fn approx_bytes(&self) -> usize {
        // Inline struct plus the line's heap allocation (with allocator
        // rounding).
        let per_line = std::mem::size_of::<SearchResult>() + 16;
        64 + self.file.len()
            + self
                .lines
                .iter()
                .map(|l| per_line + l.line.len())
                .sum::<usize>()
    }
}

/// Default cap on scan workers. Scans are bound by I/O and by the ordered
/// consumer well before this; more threads mostly add allocator memory
/// (each worker heap caches freed result buffers).
const MAX_DEFAULT_WORKERS: usize = 12;

/// Number of worker threads for parallel scans (`RAYON_NUM_THREADS` is
/// honoured for consistency with the rest of xgrep).
fn worker_threads() -> usize {
    std::env::var("RAYON_NUM_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .min(MAX_DEFAULT_WORKERS)
        })
}

struct Reorder<R> {
    /// Finished items waiting to be emitted: index -> (result, weight).
    done: std::collections::BTreeMap<usize, (Option<R>, usize)>,
    /// The consumer sleeps until the next item to emit arrives.
    consumer_waiting: bool,
    /// Workers sleeping until the look-ahead window moves.
    window_waiting: usize,
    /// Workers sleeping until pending results drop below half the budget.
    budget_waiting: usize,
    /// Set when the consumer stops early or a worker panicked.
    stop: bool,
}

/// Sets `stop` if a worker unwinds, so the consumer never waits forever.
struct PanicGuard<'a, R> {
    state: &'a Mutex<Reorder<R>>,
    ready: &'a Condvar,
    room: &'a Condvar,
}

impl<R> Drop for PanicGuard<'_, R> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            if let Ok(mut st) = self.state.lock() {
                st.stop = true;
            }
            self.ready.notify_all();
            self.room.notify_all();
        }
    }
}

/// Run `work` on every item in parallel and call `emit` with the results in
/// item order, while later items are still being processed.
///
/// Memory is bounded: before starting an item, a worker reserves its
/// estimated `cost`; it pauses while the reservations plus the `weight` of
/// finished-but-unemitted results would exceed [`PENDING_BUDGET_BYTES`], or
/// when it is [`MAX_AHEAD`] items ahead of the next item to emit. The worker
/// of the item the consumer waits for never pauses, so progress is
/// guaranteed (and one item larger than the budget still runs, alone at the
/// head). Stops early when `emit` returns false.
///
/// Wake-ups are targeted to keep context switches low: a worker wakes the
/// consumer only when it delivers the item the consumer waits for; paused
/// workers are woken when the budget drops below half (hysteresis), when the
/// window moved a quarter of its size, or before the consumer sleeps.
fn ordered_for_each<T: Sync, R: Send>(
    items: &[T],
    work: impl Fn(&T) -> Option<R> + Sync,
    cost: impl Fn(&T) -> usize + Sync,
    weight: impl Fn(&R) -> usize + Sync,
    emit: &mut dyn FnMut(R) -> bool,
) {
    let n = items.len();
    let workers = worker_threads().min(n);
    if n < PARALLEL_THRESHOLD || workers <= 1 {
        for item in items {
            if let Some(r) = work(item) {
                if !emit(r) {
                    return;
                }
            }
        }
        return;
    }

    let state = Mutex::new(Reorder {
        done: std::collections::BTreeMap::new(),
        consumer_waiting: false,
        window_waiting: 0,
        budget_waiting: 0,
        stop: false,
    });
    let ready = Condvar::new();
    let room = Condvar::new();
    let next = AtomicUsize::new(0);
    // Index of the next item to emit (only changed with `state` locked), and
    // the reserved + finished weight not yet emitted. Read lock-free on the
    // fast path.
    let emitted = AtomicUsize::new(0);
    let pending = AtomicUsize::new(0);
    let over_window = |i: usize| i >= emitted.load(Ordering::Acquire) + MAX_AHEAD;
    let over_budget = |i: usize, c: usize, limit: usize| {
        i > emitted.load(Ordering::Acquire)
            && pending.load(Ordering::Acquire).saturating_add(c) > limit
    };

    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                let _guard = PanicGuard {
                    state: &state,
                    ready: &ready,
                    room: &room,
                };
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    let c = cost(&items[i]);
                    if over_window(i) || over_budget(i, c, PENDING_BUDGET_BYTES) {
                        let mut st = state.lock().unwrap();
                        let mut limit = PENDING_BUDGET_BYTES;
                        loop {
                            if st.stop {
                                return;
                            }
                            if over_window(i) {
                                st.window_waiting += 1;
                                st = room.wait(st).unwrap();
                                st.window_waiting -= 1;
                            } else if over_budget(i, c, limit) {
                                // Resume only once half the budget is free.
                                limit = PENDING_BUDGET_BYTES / 2;
                                st.budget_waiting += 1;
                                st = room.wait(st).unwrap();
                                st.budget_waiting -= 1;
                            } else {
                                break;
                            }
                        }
                    }
                    pending.fetch_add(c, Ordering::AcqRel);
                    let r = work(&items[i]);
                    let w = r.as_ref().map_or(0, &weight);
                    let mut st = state.lock().unwrap();
                    if st.stop {
                        break;
                    }
                    st.done.insert(i, (r, w));
                    // Swap the reservation for the actual weight.
                    pending.fetch_add(w, Ordering::AcqRel);
                    pending.fetch_sub(c, Ordering::AcqRel);
                    let wake = st.consumer_waiting && i == emitted.load(Ordering::Acquire);
                    drop(st);
                    if wake {
                        ready.notify_one();
                    }
                }
            });
        }

        let mut batch: Vec<Option<R>> = Vec::new();
        let mut last_window_notify = 0usize;
        loop {
            {
                let mut st = state.lock().unwrap();
                loop {
                    let mut e = emitted.load(Ordering::Acquire);
                    let mut freed = 0;
                    while let Some((r, w)) = st.done.remove(&e) {
                        batch.push(r);
                        freed += w;
                        e += 1;
                    }
                    emitted.store(e, Ordering::Release);
                    let left = pending.fetch_sub(freed, Ordering::AcqRel) - freed;
                    if !batch.is_empty() || e >= n || st.stop {
                        let wake = (st.budget_waiting > 0 && left <= PENDING_BUDGET_BYTES / 2)
                            || (st.window_waiting > 0 && e - last_window_notify >= MAX_AHEAD / 4);
                        if wake {
                            last_window_notify = e;
                            room.notify_all();
                        }
                        break;
                    }
                    // About to sleep: let paused workers re-check first, so
                    // the item we wait for is never held up.
                    if st.window_waiting + st.budget_waiting > 0 {
                        last_window_notify = e;
                        room.notify_all();
                    }
                    st.consumer_waiting = true;
                    st = ready.wait(st).unwrap();
                    st.consumer_waiting = false;
                }
            }
            if batch.is_empty() {
                break; // all items emitted, or a worker panicked (re-raised by the scope)
            }
            for r in batch.drain(..).flatten() {
                if !emit(r) {
                    let mut st = state.lock().unwrap();
                    st.stop = true;
                    st.done.clear();
                    drop(st);
                    room.notify_all();
                    return;
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Convenience wrappers
// ---------------------------------------------------------------------------

/// Collect streamed file results into one `Vec` of lines.
pub(crate) fn collect_into(out: &mut Vec<SearchResult>) -> impl FnMut(FileMatches) -> bool + '_ {
    |fm| {
        out.extend(fm.lines);
        true
    }
}

/// Literal/regex query without boolean parts.
#[cfg(test)]
pub(crate) fn simple_query(
    pattern: &str,
    regex: bool,
    case_insensitive: bool,
    quiet: bool,
) -> Result<CompiledQuery> {
    CompiledQuery::new(
        QuerySpec {
            main: PatternSpec {
                pattern: pattern.to_string(),
                regex,
                word: false,
            },
            and: Vec::new(),
            not: Vec::new(),
            case_insensitive,
        },
        quiet,
    )
}

/// Index-based search with path filters and a result limit pushed down
/// into the scan. Dispatches to literal or regex matching.
#[cfg(test)]
pub(crate) fn search_with_filter(
    reader: &IndexReader,
    root: &Path,
    pattern: &str,
    regex: bool,
    case_insensitive: bool,
    quiet: bool,
    filter: &ScanFilter,
) -> Result<Vec<SearchResult>> {
    let q = simple_query(pattern, regex, case_insensitive, quiet)?;
    let targets = index_targets(reader, &q, filter, quiet);
    let mut out = Vec::new();
    run(
        root,
        targets,
        &q,
        filter.max_count,
        OutputMode::Lines,
        quiet,
        &mut collect_into(&mut out),
    );
    Ok(out)
}

/// Direct (non-index) search over `files` with path filters and a limit.
#[cfg(test)]
pub(crate) fn search_files_with_filter(
    root: &Path,
    files: &[PathBuf],
    pattern: &str,
    regex: bool,
    case_insensitive: bool,
    quiet: bool,
    filter: &ScanFilter,
) -> Result<Vec<SearchResult>> {
    let q = simple_query(pattern, regex, case_insensitive, quiet)?;
    let targets = file_targets(files, filter);
    let mut out = Vec::new();
    run(
        root,
        targets,
        &q,
        filter.max_count,
        OutputMode::Lines,
        quiet,
        &mut collect_into(&mut out),
    );
    Ok(out)
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
    fn test_short_literal_warning_only_for_single_byte() {
        assert!(short_literal_warning("x")
            .unwrap()
            .contains("index not used (full scan)"));
        // 2-byte literals use the trigram prefix range: no full-scan claim.
        assert_eq!(short_literal_warning("fn"), None);
        assert_eq!(short_literal_warning("あ"), None); // 3 bytes
        assert_eq!(short_literal_warning(""), None);
        assert_eq!(short_literal_warning("abc"), None);
    }

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

    fn find_matches(m: &dyn Matcher, content: &[u8]) -> Vec<SearchResult> {
        with_hay(content, m.needs_lower(), |hay| {
            collect_lines(hay, m, &Arc::from("f"))
        })
    }

    #[test]
    fn test_collect_lines_one_result_per_line() {
        let m = LiteralMatcher::new(b"ab");
        let r = find_matches(&m, b"ab ab\nxx\n\nab\nabab");
        let lines: Vec<(usize, &str, usize)> = r
            .iter()
            .map(|r| (r.line_number, r.line.as_str(), r.column))
            .collect();
        assert_eq!(lines, vec![(1, "ab ab", 0), (4, "ab", 0), (5, "abab", 0)]);
    }

    #[test]
    fn test_case_insensitive_matcher_lines() {
        let m = CaseInsensitiveMatcher::new(b"hello");
        let content = b"x\nHeLLo there hello\nnope\nsay HELLO";
        let r = find_matches(&m, content);
        let lines: Vec<(usize, &str, usize)> = r
            .iter()
            .map(|r| (r.line_number, r.line.as_str(), r.column))
            .collect();
        assert_eq!(
            lines,
            vec![(2, "HeLLo there hello", 0), (4, "say HELLO", 4)]
        );
    }

    #[test]
    fn test_case_insensitive_without_letters_uses_literal_path() {
        let m = literal_matcher("123", true);
        assert!(!m.needs_lower());
        let r = find_matches(&*m, b"a\nx123\n");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].line_number, 2);
        assert_eq!(r[0].column, 1);
    }

    #[test]
    fn test_regex_empty_match_ignores_trailing_newline() {
        let m = RegexMatcher {
            re: build_regex(".*", false).unwrap(),
            column_group: None,
        };
        assert_eq!(find_matches(&m, b"hello\nworld\n").len(), 2);
        assert_eq!(find_matches(&m, b"a\n\nb").len(), 3);
    }

    #[test]
    fn test_columns_regex_and_word_group() {
        let m = RegexMatcher {
            re: build_regex(r"b+", false).unwrap(),
            column_group: None,
        };
        let r = find_matches(&m, b"aaabbb\nb\nxxxxxb");
        let cols: Vec<usize> = r.iter().map(|r| r.column).collect();
        assert_eq!(cols, vec![3, 0, 5]);
        // Word mode: the boundary character is part of the match, the
        // column points at the user's pattern (group 1).
        let m = RegexMatcher {
            re: build_regex(r"(?:(?m:^)|[^\w\n])(foo)(?:(?m:$)|[^\w\n])", false).unwrap(),
            column_group: Some(1),
        };
        let r = find_matches(&m, b"call foo(1)\nfoo\nfood foo");
        let cols: Vec<(usize, usize)> = r.iter().map(|r| (r.line_number, r.column)).collect();
        assert_eq!(cols, vec![(1, 5), (2, 0), (3, 5)]);
    }

    #[test]
    fn test_count_and_is_match_agree_with_lines() {
        let content = b"foo\nbar foo foo\n\nnone\nfoo";
        let m = LiteralMatcher::new(b"foo");
        let hay = Hay {
            raw: content,
            lower: content,
        };
        assert_eq!(count_lines(&hay, &m), 3);
        assert!(is_match(&hay, &m));
        let m = LiteralMatcher::new(b"zzz");
        assert_eq!(count_lines(&hay, &m), 0);
        assert!(!is_match(&hay, &m));
    }

    #[test]
    fn test_line_matcher() {
        let spec = PatternSpec {
            pattern: "world".into(),
            regex: false,
            word: false,
        };
        let m = LineMatcher::new(&spec, true, true).unwrap();
        assert_eq!(m.find(b"hello WORLD"), Some(6));
        assert_eq!(m.find(b"nothing"), None);
        let spec = PatternSpec {
            pattern: r"\d+".into(),
            regex: true,
            word: false,
        };
        let m = LineMatcher::new(&spec, false, true).unwrap();
        assert_eq!(m.find(b"abc 42"), Some(4));
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
        let (_dir, reader) = many_files(100);
        let q = simple_query("needle", false, false, true).unwrap();

        let filter = ScanFilter {
            extensions: Some(vec!["md"]),
            ..Default::default()
        };
        let targets = index_targets(&reader, &q, &filter, true);
        assert_eq!(targets.len(), 100);
        assert!(targets.iter().all(|t| t.name.ends_with(".md")));

        let filter = ScanFilter {
            path_substring: Some("f00".to_string()),
            globs: Some(GlobFilter::new(&["*.rs".to_string(), "!*f009.rs".to_string()]).unwrap()),
            ..Default::default()
        };
        let targets = index_targets(&reader, &q, &filter, true);
        assert_eq!(targets.len(), 9);
    }

    #[test]
    fn test_max_count_stops_early_and_keeps_order() {
        let (dir, reader) = many_files(150);
        let root = dir.path();

        let full = search(&reader, root, "needle", false, true).unwrap();
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
            let limited =
                search_with_filter(&reader, root, "needle", false, false, true, &filter).unwrap();
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
        }
    }

    #[test]
    fn test_ordered_for_each_keeps_order_and_stops_early() {
        let items: Vec<usize> = (0..5000).collect();
        let processed = AtomicUsize::new(0);
        let mut got = Vec::new();
        ordered_for_each(
            &items,
            |&i| {
                processed.fetch_add(1, Ordering::Relaxed);
                // Uneven work so items finish out of order.
                if i % 7 == 0 {
                    std::thread::sleep(std::time::Duration::from_micros(200));
                }
                (i % 3 != 0).then_some(i)
            },
            |_| 0,
            |_| 1,
            &mut |i| {
                got.push(i);
                true
            },
        );
        let expected: Vec<usize> = items.iter().copied().filter(|i| i % 3 != 0).collect();
        assert_eq!(got, expected);
        assert_eq!(processed.load(Ordering::Relaxed), 5000);

        // Stopping after 3 results leaves most items unprocessed (the
        // look-ahead window bounds how far workers run ahead).
        let items: Vec<usize> = (0..20_000).collect();
        let workers = worker_threads().min(items.len());
        let processed = AtomicUsize::new(0);
        let mut got = Vec::new();
        ordered_for_each(
            &items,
            |&i| {
                processed.fetch_add(1, Ordering::Relaxed);
                Some(i)
            },
            |_| 0,
            |_| 1,
            &mut |i| {
                got.push(i);
                got.len() < 3
            },
        );
        assert_eq!(got, vec![0, 1, 2]);
        // The consumer may have drained a batch past the stop point.
        let bound = 2 * MAX_AHEAD + workers;
        let n = processed.load(Ordering::Relaxed);
        assert!(n <= bound, "processed {} > {}", n, bound);
    }

    #[test]
    fn test_ordered_for_each_budget_bounds_pending_results() {
        // Every result weighs half the budget: at most a few results may be
        // pending at once, yet everything is emitted, in order.
        let items: Vec<usize> = (0..3000).collect();
        let workers = worker_threads().min(items.len());
        let in_flight = AtomicUsize::new(0);
        let max_seen = AtomicUsize::new(0);
        let mut got = Vec::new();
        ordered_for_each(
            &items,
            |&i| {
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                max_seen.fetch_max(now, Ordering::SeqCst);
                if i % 5 == 0 {
                    std::thread::sleep(std::time::Duration::from_micros(100));
                }
                Some(i)
            },
            |_| 0,
            |_| PENDING_BUDGET_BYTES / 2,
            &mut |i| {
                in_flight.fetch_sub(1, Ordering::SeqCst);
                got.push(i);
                true
            },
        );
        assert_eq!(got, items);
        // Produced-but-unemitted results: those that passed the budget
        // check before it filled up (one per worker) plus a couple pending.
        let peak = max_seen.load(Ordering::SeqCst);
        assert!(peak <= 2 * workers + 3, "peak {} workers {}", peak, workers);
    }

    #[test]
    fn test_ordered_for_each_reserved_cost_serializes_huge_items() {
        // Every item reserves more than the budget: only the item at the
        // head may run, so at most one runs at a time.
        let items: Vec<usize> = (0..300).collect();
        let running = AtomicUsize::new(0);
        let max_running = AtomicUsize::new(0);
        let mut got = Vec::new();
        ordered_for_each(
            &items,
            |&i| {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                max_running.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_micros(50));
                running.fetch_sub(1, Ordering::SeqCst);
                Some(i)
            },
            |_| PENDING_BUDGET_BYTES + 1,
            |_| 1,
            &mut |i| {
                got.push(i);
                true
            },
        );
        assert_eq!(got, items);
        // A worker may pass the check just as the head advances, so allow
        // a little slack, but far below the worker count.
        assert!(max_running.load(Ordering::SeqCst) <= 3);
    }

    #[test]
    fn test_output_modes_count_and_files_only() {
        let (dir, reader) = many_files(80);
        let root = dir.path();
        let q = simple_query("needle", false, false, true).unwrap();
        let run_mode = |mode: OutputMode, max: Option<usize>| {
            let targets = index_targets(&reader, &q, &ScanFilter::default(), true);
            let mut out = Vec::new();
            run(root, targets, &q, max, mode, true, &mut |fm| {
                out.push((fm.file.to_string(), fm.count, fm.lines.len()));
                true
            });
            out
        };
        let lines = run_mode(OutputMode::Lines, None);
        let counts = run_mode(OutputMode::Count, None);
        let files = run_mode(OutputMode::FilesOnly, None);
        assert_eq!(lines.len(), 160);
        assert!(lines.iter().all(|(_, c, n)| *c == 2 && *n == 2));
        assert!(counts.iter().all(|(_, c, n)| *c == 2 && *n == 0));
        assert!(files.iter().all(|(_, c, n)| *c == 1 && *n == 0));
        let names =
            |v: &[(String, usize, usize)]| v.iter().map(|x| x.0.clone()).collect::<Vec<_>>();
        assert_eq!(names(&lines), names(&counts));
        assert_eq!(names(&lines), names(&files));

        // A global line limit truncates the last file's count.
        let limited = run_mode(OutputMode::Count, Some(5));
        assert_eq!(
            limited.iter().map(|x| x.1).collect::<Vec<_>>(),
            vec![2, 2, 1]
        );
        let limited = run_mode(OutputMode::FilesOnly, Some(5));
        assert_eq!(limited.len(), 3);
    }

    #[test]
    fn test_boolean_query_and_not() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.rs"), "fn foo() {}\nbar();\n").unwrap();
        fs::write(root.join("b.rs"), "fn foo() {}\nbaz();\n").unwrap();
        fs::write(root.join("c.rs"), "fn foo() {}\nbar();\n// test\n").unwrap();
        fs::write(root.join("d.rs"), "nothing here\nbar();\n").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        let pat = |p: &str, regex: bool| PatternSpec {
            pattern: p.to_string(),
            regex,
            word: false,
        };
        let files = |spec: QuerySpec| {
            let q = CompiledQuery::new(spec, true).unwrap();
            let targets = index_targets(&reader, &q, &ScanFilter::default(), true);
            let mut out = Vec::new();
            run(
                root,
                targets,
                &q,
                None,
                OutputMode::Lines,
                true,
                &mut |fm| {
                    assert!(fm.lines.iter().all(|l| l.line.contains("foo")));
                    out.push(fm.file.to_string());
                    true
                },
            );
            out
        };
        let spec = QuerySpec {
            main: pat("foo", false),
            and: vec![pat("bar", false)],
            not: vec![],
            case_insensitive: false,
        };
        assert_eq!(files(spec.clone()), vec!["a.rs", "c.rs"]);
        // AND prunes candidates at the posting-list level.
        let q = CompiledQuery::new(spec.clone(), true).unwrap();
        assert_eq!(q.candidates(&reader, true).len(), 2);

        let spec = QuerySpec {
            not: vec![pat("test", false)],
            ..spec
        };
        assert_eq!(files(spec.clone()), vec!["a.rs"]);

        // Regex-aware AND / NOT, case-insensitive.
        let spec = QuerySpec {
            main: pat("foo", false),
            and: vec![pat(r"ba[rz]\(", true)],
            not: vec![pat("baz", false)],
            case_insensitive: true,
        };
        assert_eq!(files(spec), vec!["a.rs", "c.rs"]);
    }

    #[test]
    fn test_overlay_targets_dedup_prefers_direct() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "needle old\n").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        fs::write(root.join("a.txt"), "needle new\n").unwrap();
        let q = simple_query("needle", false, false, true).unwrap();
        let files = vec![PathBuf::from("a.txt")];
        let mut targets = index_targets(&reader, &q, &ScanFilter::default(), true);
        targets.extend(file_targets(&files, &ScanFilter::default()));
        assert_eq!(targets.len(), 2);
        let mut out = Vec::new();
        run(
            root,
            targets,
            &q,
            None,
            OutputMode::Lines,
            true,
            &mut collect_into(&mut out),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].line, "needle new");
    }

    #[test]
    fn test_exclude_paths_skips_files_before_limit() {
        let (dir, reader) = many_files(10);
        let root = dir.path();
        let all = search(&reader, root, "needle", false, true).unwrap();
        let first = all[0].file.to_string();
        let filter = ScanFilter {
            exclude_paths: Some(std::iter::once(first.clone()).collect()),
            max_count: Some(3),
            ..Default::default()
        };
        let got = search_with_filter(&reader, root, "needle", false, false, true, &filter).unwrap();
        assert_eq!(got.len(), 3);
        assert!(got.iter().all(|r| *r.file != *first));
    }

    fn search(
        reader: &IndexReader,
        root: &Path,
        pattern: &str,
        case_insensitive: bool,
        quiet: bool,
    ) -> Result<Vec<SearchResult>> {
        let f = ScanFilter::default();
        search_with_filter(reader, root, pattern, false, case_insensitive, quiet, &f)
    }

    fn search_regex(
        reader: &IndexReader,
        root: &Path,
        pattern: &str,
        case_insensitive: bool,
        quiet: bool,
    ) -> Result<Vec<SearchResult>> {
        let f = ScanFilter::default();
        search_with_filter(reader, root, pattern, true, case_insensitive, quiet, &f)
    }

    fn search_files(
        root: &Path,
        files: &[PathBuf],
        pattern: &str,
        case_insensitive: bool,
        quiet: bool,
    ) -> Result<Vec<SearchResult>> {
        let f = ScanFilter::default();
        search_files_with_filter(root, files, pattern, false, case_insensitive, quiet, &f)
    }

    fn search_files_regex(
        root: &Path,
        files: &[PathBuf],
        pattern: &str,
        case_insensitive: bool,
        quiet: bool,
    ) -> Result<Vec<SearchResult>> {
        let f = ScanFilter::default();
        search_files_with_filter(root, files, pattern, true, case_insensitive, quiet, &f)
    }
}
