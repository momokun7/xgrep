//! Result ranking for MCP responses.
//!
//! The index returns matches in path order, which puts CHANGELOGs, docs and
//! vendored copies ahead of the real source. This module scores each match
//! (definition lines, source vs. tests vs. docs, filename hits, comment lines)
//! and orders results so the most useful ones survive `max_results` /
//! `max_tokens` truncation. It also collapses files with identical content.

use std::collections::HashMap;
use std::path::Path;

use crate::search::SearchResult;

/// Maximum number of matches per file in the first ranking tier. Further
/// matches of the same file are pushed behind the first tier of every other
/// file so one noisy file cannot fill a whole page.
pub const PER_FILE_TIER: usize = 5;

/// Coarse classification of a path by its role in the repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathClass {
    Source,
    Test,
    Example,
    Docs,
    Changelog,
    Vendor,
    Other,
}

const SOURCE_EXTS: &[&str] = &[
    "rs", "py", "pyi", "js", "mjs", "cjs", "jsx", "ts", "tsx", "mts", "go", "rb", "java", "c", "h",
    "cc", "cpp", "cxx", "hpp", "hxx", "cs", "kt", "kts", "swift", "scala", "dart", "php", "lua",
    "zig", "ex", "exs", "hs", "sh", "bash", "zsh", "vue", "svelte", "sql", "proto", "m", "mm",
];

const DOC_EXTS: &[&str] = &["md", "markdown", "rst", "txt", "adoc", "org"];

/// Classify a relative path.
pub fn path_class(path: &str) -> PathClass {
    let lower = path.to_ascii_lowercase();
    let segments: Vec<&str> = lower.split('/').collect();
    let file = segments.last().copied().unwrap_or("");
    let dirs = &segments[..segments.len().saturating_sub(1)];

    if dirs.iter().any(|d| {
        matches!(
            *d,
            "vendor" | "node_modules" | "third_party" | "third-party" | "external" | "dist"
        )
    }) || file.ends_with(".min.js")
    {
        return PathClass::Vendor;
    }
    if file.starts_with("changelog") || file.starts_with("changes") || file.starts_with("history") {
        return PathClass::Changelog;
    }
    let ext = Path::new(file)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if DOC_EXTS.contains(&ext) || dirs.iter().any(|d| matches!(*d, "docs" | "doc")) {
        return PathClass::Docs;
    }
    let stem = file.split('.').next().unwrap_or("");
    if dirs.iter().any(|d| {
        matches!(
            *d,
            "tests" | "test" | "__tests__" | "spec" | "testdata" | "fixtures"
        )
    }) || stem.starts_with("test_")
        || stem.ends_with("_test")
        || stem.ends_with("_tests")
        || file.contains(".test.")
        || file.contains(".spec.")
        || stem == "tests"
    {
        return PathClass::Test;
    }
    if dirs.iter().any(|d| {
        matches!(
            *d,
            "examples" | "example" | "benches" | "bench" | "benchmarks"
        )
    }) {
        return PathClass::Example;
    }
    if SOURCE_EXTS.contains(&ext) {
        return PathClass::Source;
    }
    PathClass::Other
}

/// Score contribution of the path role.
pub fn path_score(path: &str) -> i32 {
    match path_class(path) {
        PathClass::Source => 20,
        PathClass::Other => 0,
        PathClass::Test => -10,
        PathClass::Example => -15,
        PathClass::Docs => -30,
        PathClass::Changelog => -45,
        PathClass::Vendor => -40,
    }
}

fn ext_of(path: &str) -> &str {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
}

/// True when the line is (most likely) a comment in the file's language.
pub fn is_comment_line(path: &str, line: &str) -> bool {
    let t = line.trim_start();
    if t.starts_with("//") || t.starts_with("/*") || t.starts_with("<!--") {
        return true;
    }
    // `* foo` inside a block comment (but not `*ptr = x` / `**kwargs`).
    if t == "*" || t.starts_with("* ") || t.starts_with("*/") {
        return true;
    }
    let ext = ext_of(path);
    let hash_comment = matches!(
        ext,
        "py" | "pyi"
            | "rb"
            | "sh"
            | "bash"
            | "zsh"
            | "yaml"
            | "yml"
            | "toml"
            | "pl"
            | "r"
            | "R"
            | "ex"
            | "exs"
            | "tf"
            | "cmake"
            | "mk"
            | "dockerfile"
    ) || path.ends_with("Makefile");
    if hash_comment && t.starts_with('#') && !t.starts_with("#!") {
        return true;
    }
    if matches!(ext, "lua" | "sql" | "hs") && t.starts_with("--") {
        return true;
    }
    if matches!(ext, "py" | "pyi") && (t.starts_with("\"\"\"") || t.starts_with("'''")) {
        return true;
    }
    false
}

/// Score inputs that are constant for one query.
pub struct QueryTerm {
    /// Lowercased literal term used for filename boosts (None for regex queries).
    pub term_lower: Option<String>,
}

impl QueryTerm {
    pub fn new(pattern: &str, regex: bool) -> Self {
        let term_lower = if regex || pattern.trim().len() < 3 {
            None
        } else {
            Some(pattern.trim().to_ascii_lowercase())
        };
        Self { term_lower }
    }

    /// Bonus when the file name (not directories) contains the query term.
    pub fn filename_bonus(&self, path: &str) -> i32 {
        let Some(term) = &self.term_lower else {
            return 0;
        };
        let file = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
        let stem = file.split('.').next().unwrap_or("");
        // Compare on alphanumerics so `SearchOptions` boosts `search_options.rs`.
        let norm = |s: &str| -> String {
            s.chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
        };
        let (stem_n, term_n) = (norm(stem), norm(term));
        if term_n.is_empty() {
            return 0;
        }
        if stem_n == term_n {
            30
        } else if stem_n.contains(&term_n) || (stem_n.len() >= 3 && term_n.contains(&stem_n)) {
            15
        } else {
            0
        }
    }
}

/// A result paired with its score.
#[derive(Debug, Clone)]
pub struct Scored {
    pub result: SearchResult,
    pub score: i32,
}

/// Order scored matches: files by their best score (ties by path), and within
/// a file by score then line. Matches beyond [`PER_FILE_TIER`] in a file are
/// moved behind the first tier of all files.
pub fn order(mut items: Vec<Scored>) -> Vec<SearchResult> {
    // file -> (best score, match count). Ties on the best score go to the
    // file with more matches (the "home" of a symbol tends to use it most).
    let mut file_best: HashMap<std::sync::Arc<str>, (i32, usize)> = HashMap::new();
    for s in &items {
        let e = file_best
            .entry(s.result.file.clone())
            .or_insert((i32::MIN, 0));
        e.0 = e.0.max(s.score);
        e.1 += 1;
    }
    // Within-file order: score desc, line asc.
    items.sort_by(|a, b| {
        a.result
            .file
            .cmp(&b.result.file)
            .then(b.score.cmp(&a.score))
            .then(a.result.line_number.cmp(&b.result.line_number))
    });
    let mut keyed: Vec<(usize, (i32, usize), Scored)> = Vec::with_capacity(items.len());
    let mut idx_in_file = 0usize;
    let mut prev: Option<std::sync::Arc<str>> = None;
    for s in items {
        if prev.as_ref() != Some(&s.result.file) {
            idx_in_file = 0;
            prev = Some(s.result.file.clone());
        }
        let tier = idx_in_file / PER_FILE_TIER;
        idx_in_file += 1;
        let fb = file_best[&s.result.file];
        keyed.push((tier, fb, s));
    }
    keyed.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(b.1 .0.cmp(&a.1 .0))
            .then(b.1 .1.cmp(&a.1 .1))
            .then(a.2.result.file.cmp(&b.2.result.file))
            .then(b.2.score.cmp(&a.2.score))
            .then(a.2.result.line_number.cmp(&b.2.result.line_number))
    });
    keyed.into_iter().map(|(_, _, s)| s.result).collect()
}

/// Result of collapsing identical files.
#[derive(Debug, Default)]
pub struct Dedup {
    /// kept path -> duplicate paths whose matches were dropped
    pub duplicates: HashMap<String, Vec<String>>,
    /// number of matches dropped
    pub dropped_matches: usize,
    /// number of files dropped
    pub dropped_files: usize,
}

/// Drop matches from files whose content is byte-identical (xxh64) to a
/// higher-ranked file. Only files that share a size with another matched file
/// are hashed, so the common case costs one `stat` per file.
pub fn dedupe_identical(root: &Path, ranked: Vec<SearchResult>) -> (Vec<SearchResult>, Dedup) {
    let mut order: Vec<std::sync::Arc<str>> = Vec::new();
    let mut seen_files: std::collections::HashSet<std::sync::Arc<str>> = Default::default();
    for r in &ranked {
        if seen_files.insert(r.file.clone()) {
            order.push(r.file.clone());
        }
    }
    let mut dedup = Dedup::default();
    if order.len() < 2 {
        return (ranked, dedup);
    }
    let mut by_size: HashMap<u64, Vec<std::sync::Arc<str>>> = HashMap::new();
    for f in &order {
        if let Ok(m) = std::fs::metadata(root.join(&**f)) {
            if m.len() > 0 {
                by_size.entry(m.len()).or_default().push(f.clone());
            }
        }
    }
    // file -> canonical (kept) file
    let mut alias: HashMap<std::sync::Arc<str>, std::sync::Arc<str>> = HashMap::new();
    for group in by_size.values().filter(|g| g.len() > 1) {
        let mut by_hash: HashMap<u64, std::sync::Arc<str>> = HashMap::new();
        // `group` preserves rank order because `order` does.
        for f in group {
            let Ok(bytes) = std::fs::read(root.join(&**f)) else {
                continue;
            };
            let h = xxhash_rust::xxh64::xxh64(&bytes, 0);
            match by_hash.get(&h) {
                Some(kept) => {
                    alias.insert(f.clone(), kept.clone());
                }
                None => {
                    by_hash.insert(h, f.clone());
                }
            }
        }
    }
    if alias.is_empty() {
        return (ranked, dedup);
    }
    for (dup, kept) in &alias {
        dedup
            .duplicates
            .entry(kept.to_string())
            .or_default()
            .push(dup.to_string());
    }
    for v in dedup.duplicates.values_mut() {
        v.sort();
    }
    dedup.dropped_files = alias.len();
    let before = ranked.len();
    let kept: Vec<SearchResult> = ranked
        .into_iter()
        .filter(|r| !alias.contains_key(&r.file))
        .collect();
    dedup.dropped_matches = before - kept.len();
    (kept, dedup)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn r(file: &str, line: usize) -> SearchResult {
        SearchResult {
            file: Arc::from(file),
            line_number: line,
            line: String::new(),
        }
    }

    #[test]
    fn classifies_paths() {
        assert_eq!(path_class("rust/src/lib.rs"), PathClass::Source);
        assert_eq!(path_class("rust/tests/api_test.rs"), PathClass::Test);
        assert_eq!(path_class("src/foo.test.ts"), PathClass::Test);
        assert_eq!(path_class("CHANGELOG.md"), PathClass::Changelog);
        assert_eq!(path_class("docs/agents.md"), PathClass::Docs);
        assert_eq!(path_class("README.md"), PathClass::Docs);
        assert_eq!(path_class("vendor/x/y.go"), PathClass::Vendor);
        assert_eq!(path_class("web/app.min.js"), PathClass::Vendor);
        assert_eq!(path_class("rust/benches/b.rs"), PathClass::Example);
    }

    #[test]
    fn detects_comments() {
        assert!(is_comment_line("a.rs", "    /// doc"));
        assert!(is_comment_line("a.c", " * block"));
        assert!(is_comment_line("a.py", "# note"));
        assert!(!is_comment_line("a.c", "#define X 1"));
        assert!(!is_comment_line("a.rs", "#[derive(Debug)]"));
        assert!(!is_comment_line("a.rs", "fn x() {}"));
        assert!(!is_comment_line("a.py", "#!/usr/bin/env python"));
    }

    #[test]
    fn filename_bonus_normalizes() {
        let q = QueryTerm::new("SearchOptions", false);
        assert!(q.filename_bonus("src/search_options.rs") > 0);
        assert_eq!(q.filename_bonus("src/lib.rs"), 0);
        assert_eq!(QueryTerm::new("a.*b", true).filename_bonus("a.rs"), 0);
    }

    #[test]
    fn order_puts_best_file_first_and_tiers() {
        let mut items = vec![Scored {
            result: r("a.md", 1),
            score: -30,
        }];
        for i in 1..=7 {
            items.push(Scored {
                result: r("b.rs", i),
                score: 20,
            });
        }
        items.push(Scored {
            result: r("c.rs", 3),
            score: 120,
        });
        let out = order(items);
        assert_eq!(&*out[0].file, "c.rs");
        // first tier of b.rs (5), then a.md, then the remaining b.rs matches
        assert_eq!(&*out[1].file, "b.rs");
        assert_eq!(&*out[6].file, "a.md");
        assert_eq!(&*out[7].file, "b.rs");
        assert_eq!(out[7].line_number, 6);
    }

    #[test]
    fn dedupe_drops_identical_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn x() {}\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn x() {}\n").unwrap();
        std::fs::write(dir.path().join("c.rs"), "fn y() {}\n").unwrap();
        let ranked = vec![r("a.rs", 1), r("b.rs", 1), r("c.rs", 1)];
        let (kept, d) = dedupe_identical(dir.path(), ranked);
        assert_eq!(kept.len(), 2);
        assert_eq!(d.dropped_files, 1);
        assert_eq!(d.duplicates["a.rs"], vec!["b.rs".to_string()]);
    }
}
