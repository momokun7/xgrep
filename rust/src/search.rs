use crate::candidates::{resolve_literal_candidates, resolve_regex_candidates};
use crate::error::{Result, XgrepError};
use crate::index::reader::IndexReader;
use crate::trigram;
use memchr::memmem;
use rayon::prelude::*;
use regex::RegexBuilder;
use std::fs;
use std::path::{Path, PathBuf};

/// ASCII-only case-insensitive contains check (for testing).
/// Uses memmem::Finder internally to verify with the same algorithm as production code.
#[cfg(test)]
#[allow(dead_code)]
fn contains_case_insensitive(haystack: &str, needle: &str) -> bool {
    let mut lowered = haystack.as_bytes().to_vec();
    lowered.make_ascii_lowercase();
    let needle_lower = needle.to_lowercase();
    memmem::Finder::new(needle_lower.as_bytes())
        .find(&lowered)
        .is_some()
}

/// Build a table of byte offsets for the start of each line.
/// line_offsets[i] = byte offset of the start of line i+1.
fn build_line_offsets(content: &[u8]) -> Vec<usize> {
    let mut offsets = vec![0]; // Line 1 starts at offset 0
    for (i, &b) in content.iter().enumerate() {
        if b == b'\n' {
            offsets.push(i + 1);
        }
    }
    offsets
}

/// Find the line number (1-based) for a byte position using binary search on precomputed offsets.
fn line_number_at(line_offsets: &[usize], pos: usize) -> usize {
    match line_offsets.binary_search(&pos) {
        Ok(i) => i + 1,
        Err(i) => i, // pos is within line i (0-indexed), so line number is i
    }
}

/// Get the byte offset for the start of a line number (1-based).
fn line_start(line_offsets: &[usize], line_num: usize) -> usize {
    if line_num <= 1 {
        0
    } else {
        line_offsets.get(line_num - 1).copied().unwrap_or(0)
    }
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub file: String,
    pub line_number: usize,
    pub line: String,
}

// ---------------------------------------------------------------------------
// Matcher trait: unified interface for three matching strategies
// ---------------------------------------------------------------------------

trait Matcher: Send + Sync {
    fn find_matches(&self, content: &[u8], rel_path: &str) -> Vec<SearchResult>;
}

/// Case-sensitive literal string matcher (using memmem::Finder).
struct LiteralMatcher {
    pattern: Vec<u8>,
}

impl Matcher for LiteralMatcher {
    fn find_matches(&self, content: &[u8], rel_path: &str) -> Vec<SearchResult> {
        let finder = memmem::Finder::new(&self.pattern);

        // Early return if no match at all
        if finder.find(content).is_none() {
            return vec![];
        }

        // Only build line offsets when we know there's at least one match
        let line_offsets = build_line_offsets(content);
        let mut results = Vec::new();
        let mut pos = 0;

        while let Some(match_pos) = finder.find(&content[pos..]) {
            let abs_pos = pos + match_pos;
            let line_num = line_number_at(&line_offsets, abs_pos);
            let ls = line_start(&line_offsets, line_num);
            let line_end = content[abs_pos..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(content.len(), |p| abs_pos + p);
            let line = std::str::from_utf8(&content[ls..line_end]).unwrap_or("<binary>");

            results.push(SearchResult {
                file: rel_path.to_string(),
                line_number: line_num,
                line: line.to_string(),
            });

            pos = line_end + 1;
            if pos >= content.len() {
                break;
            }
        }

        results
    }
}

/// Case-insensitive literal string matcher (ASCII-only folding + memmem SIMD search).
struct CaseInsensitiveMatcher {
    pattern_lower: String,
}

impl Matcher for CaseInsensitiveMatcher {
    fn find_matches(&self, content: &[u8], rel_path: &str) -> Vec<SearchResult> {
        let pattern_bytes = self.pattern_lower.as_bytes();

        // Early rejection: check if first byte of pattern exists (either case)
        if !pattern_bytes.is_empty() {
            let first = pattern_bytes[0];
            let first_upper = first.to_ascii_uppercase();
            if first == first_upper {
                // Non-alphabetic first byte
                if memchr::memchr(first, content).is_none() {
                    return vec![];
                }
            } else if memchr::memchr2(first, first_upper, content).is_none() {
                return vec![];
            }
        }

        // Now do the full lowercase + memmem search
        let mut lowered = content.to_vec();
        lowered.make_ascii_lowercase();

        let finder = memmem::Finder::new(pattern_bytes);
        if finder.find(&lowered).is_none() {
            return vec![];
        }

        let line_offsets = build_line_offsets(content);
        let mut results = Vec::new();
        let mut pos = 0;

        while let Some(match_pos) = finder.find(&lowered[pos..]) {
            let abs_pos = pos + match_pos;
            let line_num = line_number_at(&line_offsets, abs_pos);
            let ls = line_start(&line_offsets, line_num);
            let line_end = content[abs_pos..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(content.len(), |p| abs_pos + p);
            let line = std::str::from_utf8(&content[ls..line_end]).unwrap_or("<binary>");

            results.push(SearchResult {
                file: rel_path.to_string(),
                line_number: line_num,
                line: line.to_string(),
            });

            // Skip to next line to avoid duplicate matches on the same line
            pos = line_end + 1;
            if pos >= content.len() {
                break;
            }
        }
        results
    }
}

/// Regex matcher.
struct RegexMatcher {
    re: regex::Regex,
}

impl Matcher for RegexMatcher {
    fn find_matches(&self, content: &[u8], rel_path: &str) -> Vec<SearchResult> {
        let content_str = String::from_utf8_lossy(content);

        // Early return if no match
        if !self.re.is_match(&content_str) {
            return vec![];
        }

        let mut results = Vec::new();
        for (i, line) in content_str.lines().enumerate() {
            if self.re.is_match(line) {
                results.push(SearchResult {
                    file: rel_path.to_string(),
                    line_number: i + 1,
                    line: line.to_string(),
                });
            }
        }
        results
    }
}

// ---------------------------------------------------------------------------
// Scan functions
// ---------------------------------------------------------------------------

/// Scan files directly from index candidate ID list (no intermediate Vec).
fn scan_indexed<M: Matcher>(
    reader: &IndexReader,
    root: &Path,
    candidate_ids: &[u32],
    matcher: &M,
) -> Vec<SearchResult> {
    let mut results: Vec<SearchResult> = candidate_ids
        .par_iter()
        .flat_map(|&fid| {
            let rel_path = reader.file_path(fid);
            let full_path = root.join(rel_path);
            let content = match fs::read(&full_path) {
                Ok(c) => c,
                Err(e) => {
                    if !crate::mcp::is_mcp_mode() {
                        eprintln!("xgrep: {}: {}", full_path.display(), e);
                    }
                    return vec![];
                }
            };
            matcher.find_matches(&content, rel_path)
        })
        .collect();
    results.sort_by(|a, b| a.file.cmp(&b.file).then(a.line_number.cmp(&b.line_number)));
    results
}

/// Scan files directly from a file path list (skipping binary files).
fn scan_direct<M: Matcher>(root: &Path, files: &[PathBuf], matcher: &M) -> Vec<SearchResult> {
    let mut results: Vec<SearchResult> = files
        .par_iter()
        .flat_map(|rel_path| {
            let full_path = root.join(rel_path);
            let content = match fs::read(&full_path) {
                Ok(c) => c,
                Err(e) => {
                    if !crate::mcp::is_mcp_mode() {
                        eprintln!("xgrep: {}: {}", full_path.display(), e);
                    }
                    return vec![];
                }
            };
            // Skip binary files
            if memchr::memchr(0, &content).is_some() {
                return vec![];
            }
            let rel_str = rel_path.to_string_lossy();
            matcher.find_matches(&content, &rel_str)
        })
        .collect();
    results.sort_by(|a, b| a.file.cmp(&b.file).then(a.line_number.cmp(&b.line_number)));
    results
}

// ---------------------------------------------------------------------------
// Public API (signature preserved)
// ---------------------------------------------------------------------------

pub fn search(
    reader: &IndexReader,
    root: &Path,
    pattern: &str,
    case_insensitive: bool,
) -> Result<Vec<SearchResult>> {
    let pattern_bytes = pattern.as_bytes();
    if pattern_bytes.len() < 3 && !pattern_bytes.is_empty() && !crate::mcp::is_mcp_mode() {
        eprintln!(
            "xgrep: warning: pattern '{}' is shorter than 3 characters, index not used (full scan)",
            pattern
        );
    }

    if case_insensitive && pattern.bytes().any(|b| b > 127) && !crate::mcp::is_mcp_mode() {
        eprintln!(
            "xgrep: warning: case-insensitive search with non-ASCII pattern '{}' uses ASCII-only folding",
            pattern
        );
    }

    let search_pattern = if case_insensitive {
        pattern.to_lowercase()
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

    let results = if case_insensitive {
        let matcher = CaseInsensitiveMatcher {
            pattern_lower: search_pattern,
        };
        scan_indexed(reader, root, &candidate_ids, &matcher)
    } else {
        let matcher = LiteralMatcher {
            pattern: pattern.as_bytes().to_vec(),
        };
        scan_indexed(reader, root, &candidate_ids, &matcher)
    };

    Ok(results)
}

/// Search specified files directly without using the index
pub fn search_files(
    root: &Path,
    files: &[PathBuf],
    pattern: &str,
    case_insensitive: bool,
) -> Result<Vec<SearchResult>> {
    if case_insensitive && pattern.bytes().any(|b| b > 127) && !crate::mcp::is_mcp_mode() {
        eprintln!(
            "xgrep: warning: case-insensitive search with non-ASCII pattern '{}' uses ASCII-only folding",
            pattern
        );
    }

    let results = if case_insensitive {
        let matcher = CaseInsensitiveMatcher {
            pattern_lower: pattern.to_lowercase(),
        };
        scan_direct(root, files, &matcher)
    } else {
        let matcher = LiteralMatcher {
            pattern: pattern.as_bytes().to_vec(),
        };
        scan_direct(root, files, &matcher)
    };

    Ok(results)
}

/// Regex search: extract literal trigrams from pattern for index lookup,
/// then verify with full regex on candidate files
pub fn search_regex(
    reader: &IndexReader,
    root: &Path,
    pattern: &str,
    case_insensitive: bool,
) -> Result<Vec<SearchResult>> {
    let re = RegexBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .build()
        .map_err(|e| XgrepError::InvalidPattern(e.to_string()))?;

    let candidate_ids = resolve_regex_candidates(reader, pattern, case_insensitive);

    let matcher = RegexMatcher { re };
    let results = scan_indexed(reader, root, &candidate_ids, &matcher);

    Ok(results)
}

/// Also add search_files_regex for --changed/--since with regex
pub fn search_files_regex(
    root: &Path,
    files: &[PathBuf],
    pattern: &str,
    case_insensitive: bool,
) -> Result<Vec<SearchResult>> {
    let re = RegexBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .build()
        .map_err(|e| XgrepError::InvalidPattern(e.to_string()))?;

    let matcher = RegexMatcher { re };
    let results = scan_direct(root, files, &matcher);

    Ok(results)
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
        let results = search(&reader, root, "handle_auth", false).unwrap();
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
        let results = search(&reader, root, "nonexistent_xyz", false).unwrap();
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
        let results = search(&reader, root, "ok", false).unwrap();
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

        let results = search(&reader, root, "fn", false).unwrap();
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
        let results = search(&reader, root, "zq", false).unwrap();
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
        let results = search(&reader, root, "zq", false).unwrap();
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
        let results = search(&reader, root, "handleauth", true).unwrap();
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
        let results = search_files(root, &files, "hello", false).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].file.contains("a.rs"));
    }

    #[test]
    fn test_search_files_case_insensitive() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.rs"), "fn HandleAuth() {}").unwrap();
        let files = vec![PathBuf::from("a.rs")];
        let results = search_files(root, &files, "handleauth", true).unwrap();
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
        let results = search_regex(&reader, root, "handle_\\w+", false).unwrap();
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
        let results = search_regex(&reader, root, "handleauth", true).unwrap();
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
        let results = search(&reader, root, "", false).unwrap();
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
        let results = search(&reader, root, "foo", false).unwrap();
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
        let results = search(&reader, root, "line3", false).unwrap();
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
        let results = search(&reader, root, "$100.00", false).unwrap();
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
        let result = search_regex(&reader, root, "[invalid", false);
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
        let results = search_regex(&reader, root, ".*", false).unwrap();
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
        let results = search(&reader, root, "hello", false).unwrap();
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
        let results = search(&reader, root, "テスト", false).unwrap();
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
        let results = search(&reader, root, "THIS IS UPPERCASE", true).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_search_files_nonexistent_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let files = vec![PathBuf::from("nonexistent.txt")];
        let results = search_files(root, &files, "hello", false).unwrap();
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_search_files_empty_list() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let files: Vec<PathBuf> = vec![];
        let results = search_files(root, &files, "hello", false).unwrap();
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
        let results = search_files_regex(root, &files, "handle_\\w+", false).unwrap();
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
        let results = search(&reader, root, "hello world", true).unwrap();
        assert_eq!(results.len(), 3);
        let mut files: Vec<&str> = results.iter().map(|r| r.file.as_str()).collect();
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
        let result = search_files_regex(root, &files, "[invalid", false);
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

        let results = search(&reader, root, "handleauth", true).unwrap();
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
        let results = search(&reader, root, "x", false).unwrap();
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
        let results = search_regex(&reader, root, ".*", false).unwrap();
        assert!(results.len() >= 1);
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
        let results = search(&reader, root, "abcdefghijklmnop", true).unwrap();
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
        let results = search(&reader, root, "xyznonexistent", true).unwrap();
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
        let results = search_files(root, &files, "hello", false).unwrap();

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
        let results = search_files_regex(root, &files, "hello", false).unwrap();

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
        let results = search(&reader, root, "hello world", true).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_build_line_offsets() {
        let content = b"line1\nline2\nline3";
        let offsets = build_line_offsets(content);
        assert_eq!(offsets, vec![0, 6, 12]);
    }

    #[test]
    fn test_line_number_at() {
        let offsets = vec![0, 6, 12];
        assert_eq!(line_number_at(&offsets, 0), 1); // start of line 1
        assert_eq!(line_number_at(&offsets, 3), 1); // middle of line 1
        assert_eq!(line_number_at(&offsets, 6), 2); // start of line 2
        assert_eq!(line_number_at(&offsets, 8), 2); // middle of line 2
        assert_eq!(line_number_at(&offsets, 12), 3); // start of line 3
        assert_eq!(line_number_at(&offsets, 15), 3); // end of line 3
    }
}
