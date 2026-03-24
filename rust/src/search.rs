use crate::candidates::{resolve_literal_candidates, resolve_regex_candidates};
use crate::index::reader::IndexReader;
use crate::trigram;
use anyhow::Result;
use memchr::memmem;
use rayon::prelude::*;
use regex::RegexBuilder;
use std::fs;
use std::path::{Path, PathBuf};

/// ASCII-only case-insensitive containsチェック。アロケーションなし。
/// needleは事前にlowercase化されている前提。
/// Unicode case foldingは非対応だが、コード検索ではASCIIで十分。
fn contains_case_insensitive(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let needle_bytes = needle.as_bytes();
    let haystack_bytes = haystack.as_bytes();
    if needle_bytes.len() > haystack_bytes.len() {
        return false;
    }
    'outer: for i in 0..=(haystack_bytes.len() - needle_bytes.len()) {
        for j in 0..needle_bytes.len() {
            if haystack_bytes[i + j].to_ascii_lowercase() != needle_bytes[j] {
                continue 'outer;
            }
        }
        return true;
    }
    false
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub file: String,
    pub line_number: usize,
    pub line: String,
}

// ---------------------------------------------------------------------------
// Matcher trait: 3つのマッチ戦略を統一するインターフェース
// ---------------------------------------------------------------------------

trait Matcher: Send + Sync {
    fn find_matches(&self, content: &[u8], rel_path: &str) -> Vec<SearchResult>;
}

/// case-sensitive固定文字列マッチ（memmem::Finder使用）
struct LiteralMatcher {
    pattern: Vec<u8>,
}

impl Matcher for LiteralMatcher {
    fn find_matches(&self, content: &[u8], rel_path: &str) -> Vec<SearchResult> {
        let finder = memmem::Finder::new(&self.pattern);
        let mut results = Vec::new();
        let mut pos = 0;

        while let Some(match_pos) = finder.find(&content[pos..]) {
            let abs_pos = pos + match_pos;
            let line_num = content[..abs_pos].iter().filter(|&&b| b == b'\n').count() + 1;
            let line_start = content[..abs_pos]
                .iter()
                .rposition(|&b| b == b'\n')
                .map_or(0, |p| p + 1);
            let line_end = content[abs_pos..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(content.len(), |p| abs_pos + p);
            let line = std::str::from_utf8(&content[line_start..line_end]).unwrap_or("<binary>");

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

/// case-insensitive固定文字列マッチ（ASCII-only folding）
struct CaseInsensitiveMatcher {
    pattern_lower: String,
}

impl Matcher for CaseInsensitiveMatcher {
    fn find_matches(&self, content: &[u8], rel_path: &str) -> Vec<SearchResult> {
        let content_str = String::from_utf8_lossy(content);
        let mut results = Vec::new();
        for (i, line) in content_str.lines().enumerate() {
            if contains_case_insensitive(line, &self.pattern_lower) {
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

/// 正規表現マッチ
struct RegexMatcher {
    re: regex::Regex,
}

impl Matcher for RegexMatcher {
    fn find_matches(&self, content: &[u8], rel_path: &str) -> Vec<SearchResult> {
        let content_str = String::from_utf8_lossy(content);
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
// 統一スキャン関数
// ---------------------------------------------------------------------------

/// 1チャンクあたりの最大ファイル数。メモリ使用量の上限を制御する。
const MAX_CHUNK_SIZE: usize = 10_000;

/// ファイル候補リストに対してMatcherでスキャンし、ソート済み結果を返す。
/// 候補をMAX_CHUNK_SIZEごとに分割し、チャンク単位で並列処理することで
/// 同時にメモリに載るファイル数を制限する。
fn scan_files<M: Matcher>(
    candidates: &[(String, PathBuf)],
    matcher: &M,
    skip_binary: bool,
) -> Vec<SearchResult> {
    let mut all_results = Vec::new();
    for chunk in candidates.chunks(MAX_CHUNK_SIZE) {
        let mut chunk_results: Vec<SearchResult> = chunk
            .par_iter()
            .flat_map(|(rel_path, full_path)| {
                let content = match fs::read(full_path) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("xgrep: {}: {}", full_path.display(), e);
                        return vec![];
                    }
                };
                if skip_binary && memchr::memchr(0, &content).is_some() {
                    return vec![];
                }
                matcher.find_matches(&content, rel_path)
            })
            .collect();
        all_results.append(&mut chunk_results);
    }
    all_results.sort_by(|a, b| a.file.cmp(&b.file).then(a.line_number.cmp(&b.line_number)));
    all_results
}

/// IndexReaderの候補IDリストから(rel_path, full_path)ペアを構築する。
fn candidates_from_index(
    reader: &IndexReader,
    root: &Path,
    candidate_ids: &[u32],
) -> Vec<(String, PathBuf)> {
    candidate_ids
        .iter()
        .map(|&fid| {
            let rel = reader.file_path(fid).to_string();
            let full = root.join(&rel);
            (rel, full)
        })
        .collect()
}

/// PathBufリストから(rel_path, full_path)ペアを構築する。
fn candidates_from_files(root: &Path, files: &[PathBuf]) -> Vec<(String, PathBuf)> {
    files
        .iter()
        .map(|rel| {
            let rel_str = rel.to_string_lossy().to_string();
            let full = root.join(rel);
            (rel_str, full)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 公開API（シグネチャ維持）
// ---------------------------------------------------------------------------

pub fn search(
    reader: &IndexReader,
    root: &Path,
    pattern: &str,
    case_insensitive: bool,
) -> Result<Vec<SearchResult>> {
    let pattern_bytes = pattern.as_bytes();
    if pattern_bytes.len() < 3 && !pattern_bytes.is_empty() {
        eprintln!(
            "xgrep: warning: pattern '{}' is shorter than 3 characters, index not used (full scan)",
            pattern
        );
    }

    if case_insensitive && pattern.bytes().any(|b| b > 127) {
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

    let candidates = candidates_from_index(reader, root, &candidate_ids);

    // インデックス経由の検索ではバイナリチェック不要（インデックスビルド時にスキップ済み）
    let results = if case_insensitive {
        let matcher = CaseInsensitiveMatcher {
            pattern_lower: search_pattern,
        };
        scan_files(&candidates, &matcher, false)
    } else {
        let matcher = LiteralMatcher {
            pattern: pattern.as_bytes().to_vec(),
        };
        scan_files(&candidates, &matcher, false)
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
    if case_insensitive && pattern.bytes().any(|b| b > 127) {
        eprintln!(
            "xgrep: warning: case-insensitive search with non-ASCII pattern '{}' uses ASCII-only folding",
            pattern
        );
    }

    let candidates = candidates_from_files(root, files);

    let results = if case_insensitive {
        let matcher = CaseInsensitiveMatcher {
            pattern_lower: pattern.to_lowercase(),
        };
        scan_files(&candidates, &matcher, true)
    } else {
        let matcher = LiteralMatcher {
            pattern: pattern.as_bytes().to_vec(),
        };
        scan_files(&candidates, &matcher, true)
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
        .build()?;

    let candidate_ids = resolve_regex_candidates(reader, pattern, case_insensitive);
    let candidates = candidates_from_index(reader, root, &candidate_ids);

    let matcher = RegexMatcher { re };
    let results = scan_files(&candidates, &matcher, false);

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
        .build()?;

    let candidates = candidates_from_files(root, files);
    let matcher = RegexMatcher { re };
    let results = scan_files(&candidates, &matcher, true);

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
}
