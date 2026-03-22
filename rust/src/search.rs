use std::fs;
use std::path::{Path, PathBuf};
use anyhow::Result;
use memchr::memmem;
use rayon::prelude::*;
use regex::RegexBuilder;
use crate::index::reader::IndexReader;
use crate::trigram;

pub struct SearchResult {
    pub file: String,
    pub line_number: usize,
    pub line: String,
}

pub fn search(reader: &IndexReader, root: &Path, pattern: &str, case_insensitive: bool) -> Result<Vec<SearchResult>> {
    let search_pattern = if case_insensitive { pattern.to_lowercase() } else { pattern.to_string() };
    let pattern_bytes = search_pattern.as_bytes();
    let trigrams = trigram::extract_trigrams(pattern_bytes);

    let candidate_ids: Vec<u32> = if trigrams.is_empty() {
        if pattern_bytes.len() == 2 {
            // 2文字パターン: プレフィックスに一致する全trigramのunionで候補を絞る
            let prefix = [pattern_bytes[0], pattern_bytes[1]];
            let candidates = reader.lookup_trigram_prefix(prefix);
            // 候補が空（プレフィックスに一致するtrigramが存在しない）なら全スキャン
            if candidates.is_empty() {
                (0..reader.file_count()).collect()
            } else {
                candidates
            }
        } else {
            // 0-1文字パターン: 全ファイルスキャン
            (0..reader.file_count()).collect()
        }
    } else {
        let posting_lists: Vec<Vec<u32>> = trigrams
            .iter()
            .map(|t| reader.lookup_trigram(*t))
            .collect();
        // case-insensitive時、lowercase trigramでポスティングが空なら全ファイルスキャンにフォールバック
        if case_insensitive && posting_lists.iter().any(|l| l.is_empty()) {
            (0..reader.file_count()).collect()
        } else {
            let refs: Vec<&[u32]> = posting_lists.iter().map(|v| v.as_slice()).collect();
            intersect_postings(&refs)
        }
    };

    let mut results: Vec<SearchResult> = candidate_ids
        .par_iter()
        .flat_map(|&file_id| {
            let rel_path = reader.file_path(file_id);
            let full_path_str = format!("{}/{}", root.display(), rel_path);

            let content = match fs::read(&full_path_str) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("xgrep: {}: {}", full_path_str, e);
                    return vec![];
                }
            };

            let mut file_results = Vec::new();

            if case_insensitive {
                let pattern_lower = search_pattern.as_str();
                let content_str = String::from_utf8_lossy(&content);
                for (i, line) in content_str.lines().enumerate() {
                    if line.to_lowercase().contains(pattern_lower) {
                        file_results.push(SearchResult {
                            file: rel_path.to_string(),
                            line_number: i + 1,
                            line: line.to_string(),
                        });
                    }
                }
            } else {
                let finder = memmem::Finder::new(pattern.as_bytes());
                let mut pos = 0;

                while let Some(match_pos) = finder.find(&content[pos..]) {
                    let abs_pos = pos + match_pos;
                    let line_num = content[..abs_pos].iter().filter(|&&b| b == b'\n').count() + 1;
                    let line_start = content[..abs_pos].iter().rposition(|&b| b == b'\n').map_or(0, |p| p + 1);
                    let line_end = content[abs_pos..].iter().position(|&b| b == b'\n').map_or(content.len(), |p| abs_pos + p);
                    let line = std::str::from_utf8(&content[line_start..line_end]).unwrap_or("<binary>");

                    file_results.push(SearchResult {
                        file: rel_path.to_string(),
                        line_number: line_num,
                        line: line.to_string(),
                    });

                    pos = line_end + 1;
                    if pos >= content.len() { break; }
                }
            }

            file_results
        })
        .collect();

    results.sort_by(|a, b| a.file.cmp(&b.file).then(a.line_number.cmp(&b.line_number)));

    Ok(results)
}

/// Search specified files directly without using the index
pub fn search_files(root: &Path, files: &[PathBuf], pattern: &str, case_insensitive: bool) -> Result<Vec<SearchResult>> {
    let pattern_lower = if case_insensitive { pattern.to_lowercase() } else { String::new() };

    let mut results: Vec<SearchResult> = files
        .par_iter()
        .flat_map(|rel_path| {
            let full_path = root.join(rel_path);
            let content = match fs::read(&full_path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("xgrep: {}: {}", full_path.display(), e);
                    return vec![];
                }
            };

            let rel_str = rel_path.to_string_lossy().to_string();
            let mut file_results = Vec::new();

            if case_insensitive {
                let content_str = String::from_utf8_lossy(&content);
                for (i, line) in content_str.lines().enumerate() {
                    if line.to_lowercase().contains(&pattern_lower) {
                        file_results.push(SearchResult {
                            file: rel_str.clone(),
                            line_number: i + 1,
                            line: line.to_string(),
                        });
                    }
                }
            } else {
                let finder = memmem::Finder::new(pattern.as_bytes());
                let mut pos = 0;
                while let Some(match_pos) = finder.find(&content[pos..]) {
                    let abs_pos = pos + match_pos;
                    let line_num = content[..abs_pos].iter().filter(|&&b| b == b'\n').count() + 1;
                    let line_start = content[..abs_pos].iter().rposition(|&b| b == b'\n').map_or(0, |p| p + 1);
                    let line_end = content[abs_pos..].iter().position(|&b| b == b'\n').map_or(content.len(), |p| abs_pos + p);
                    let line = std::str::from_utf8(&content[line_start..line_end]).unwrap_or("<binary>");
                    file_results.push(SearchResult {
                        file: rel_str.clone(),
                        line_number: line_num,
                        line: line.to_string(),
                    });
                    pos = line_end + 1;
                    if pos >= content.len() { break; }
                }
            }
            file_results
        })
        .collect();

    results.sort_by(|a, b| a.file.cmp(&b.file).then(a.line_number.cmp(&b.line_number)));
    Ok(results)
}

/// Regex search: extract literal trigrams from pattern for index lookup,
/// then verify with full regex on candidate files
pub fn search_regex(reader: &IndexReader, root: &Path, pattern: &str, case_insensitive: bool) -> Result<Vec<SearchResult>> {
    let re = RegexBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .build()?;

    // Extract literal portions from regex for trigram lookup
    // Simple heuristic: find longest literal substring in the pattern
    let literals = extract_literals(pattern);

    let candidate_ids: Vec<u32> = if literals.len() >= 3 {
        // Use trigram index with literals
        let trigrams = trigram::extract_trigrams(literals.as_bytes());
        if trigrams.is_empty() {
            (0..reader.file_count()).collect()
        } else {
            let posting_lists: Vec<Vec<u32>> = trigrams
                .iter()
                .map(|t| reader.lookup_trigram(*t))
                .collect();
            // If any posting list is empty, fall back to full scan
            if posting_lists.iter().any(|p| p.is_empty()) {
                (0..reader.file_count()).collect()
            } else {
                let refs: Vec<&[u32]> = posting_lists.iter().map(|v| v.as_slice()).collect();
                intersect_postings(&refs)
            }
        }
    } else {
        // No usable literals, full file scan
        (0..reader.file_count()).collect()
    };

    // Verify with regex on candidate files
    let mut results: Vec<SearchResult> = candidate_ids
        .par_iter()
        .flat_map(|&file_id| {
            let rel_path = reader.file_path(file_id);
            let full_path = format!("{}/{}", root.display(), rel_path);
            let content_bytes = match std::fs::read(&full_path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("xgrep: {}: {}", full_path, e);
                    return vec![];
                }
            };
            let content = String::from_utf8_lossy(&content_bytes);

            let mut file_results = Vec::new();
            for (i, line) in content.lines().enumerate() {
                if re.is_match(line) {
                    file_results.push(SearchResult {
                        file: rel_path.to_string(),
                        line_number: i + 1,
                        line: line.to_string(),
                    });
                }
            }
            file_results
        })
        .collect();

    results.sort_by(|a, b| a.file.cmp(&b.file).then(a.line_number.cmp(&b.line_number)));
    Ok(results)
}

/// Extract literal substrings from a regex pattern (simple heuristic)
/// Finds the longest run of non-special characters (must be >= 2 chars to be useful)
fn extract_literals(pattern: &str) -> String {
    let special = ['[', ']', '(', ')', '{', '}', '.', '*', '+', '?', '|', '^', '$', '\\'];
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

/// Also add search_files_regex for --changed/--since with regex
pub fn search_files_regex(root: &Path, files: &[PathBuf], pattern: &str, case_insensitive: bool) -> Result<Vec<SearchResult>> {
    let re = RegexBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .build()?;

    let mut results: Vec<SearchResult> = files
        .par_iter()
        .flat_map(|rel_path| {
            let full_path = root.join(rel_path);
            let content_bytes = match std::fs::read(&full_path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("xgrep: {}: {}", full_path.display(), e);
                    return vec![];
                }
            };
            let content = String::from_utf8_lossy(&content_bytes);
            let rel_str = rel_path.to_string_lossy().to_string();
            let mut file_results = Vec::new();
            for (i, line) in content.lines().enumerate() {
                if re.is_match(line) {
                    file_results.push(SearchResult {
                        file: rel_str.clone(),
                        line_number: i + 1,
                        line: line.to_string(),
                    });
                }
            }
            file_results
        })
        .collect();

    results.sort_by(|a, b| a.file.cmp(&b.file).then(a.line_number.cmp(&b.line_number)));
    Ok(results)
}

pub fn intersect_postings(lists: &[&[u32]]) -> Vec<u32> {
    if lists.is_empty() { return vec![]; }
    if lists.len() == 1 { return lists[0].to_vec(); }

    // Sort by length to start from shortest list
    let mut sorted_lists: Vec<&[u32]> = lists.to_vec();
    sorted_lists.sort_by_key(|l| l.len());

    let mut result: Vec<u32> = sorted_lists[0].to_vec();
    for list in &sorted_lists[1..] {
        let mut new_result = Vec::new();
        let mut i = 0;
        let mut j = 0;
        while i < result.len() && j < list.len() {
            match result[i].cmp(&list[j]) {
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => {
                    new_result.push(result[i]);
                    i += 1;
                    j += 1;
                }
            }
        }
        result = new_result;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::builder;
    use tempfile::tempdir;

    #[test]
    fn test_intersect_postings() {
        let a = vec![1, 3, 5, 7, 9];
        let b = vec![2, 3, 5, 8];
        let c = vec![3, 5, 6];
        let result = intersect_postings(&[&a, &b, &c]);
        assert_eq!(result, vec![3, 5]);
    }

    #[test]
    fn test_intersect_empty() {
        let a = vec![1, 2, 3];
        let result = intersect_postings(&[&a, &[]]);
        assert_eq!(result, vec![]);
    }

    #[test]
    fn test_intersect_single() {
        let a = vec![1, 2, 3];
        let result = intersect_postings(&[&a]);
        assert_eq!(result, vec![1, 2, 3]);
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
        // 2文字パターン "fn" はtrigramプレフィックス検索で候補を絞れることを確認
        let dir = tempdir().unwrap();
        let root = dir.path();
        // "fn" を含むファイルと含まないファイルを用意
        fs::write(root.join("has_fn.rs"), "fn hello() {}\nfn world() {}").unwrap();
        fs::write(root.join("no_match.txt"), "xyz abc def ghi jkl").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();

        let results = search(&reader, root, "fn", false).unwrap();
        // "fn" を含む行が2行ヒットすること
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.file.contains("has_fn.rs")));
    }

    #[test]
    fn test_lookup_trigram_prefix_returns_subset() {
        // lookup_trigram_prefix が全ファイル数より少ない候補を返すことを確認
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.rs"), "fn hello() {}").unwrap();
        fs::write(root.join("b.txt"), "xyz abc").unwrap();
        fs::write(root.join("c.txt"), "qrs tuv").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();

        // "fn" プレフィックスにヒットする候補はa.rsのみのはず
        let candidates = reader.lookup_trigram_prefix(*b"fn");
        assert!(!candidates.is_empty());
        // 全ファイル数(3)より少ないこと（プレフィックスで絞り込めていること）
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
        assert_eq!(extract_literals("a.b"), "");  // single chars between specials
    }

    #[test]
    fn test_search_regex() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.rs"), "fn handle_auth() {}\nfn handle_user() {}\nfn other() {}").unwrap();
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
        // Empty pattern should match every line (same as grep "")
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
        let results = search(&reader, root, "this is much longer than the file content", false).unwrap();
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
        // Should return the line once (not 3 times)
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
        // Fixed string search: $ and . are literal
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
        // Invalid regex should return error
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
        // .* matches everything
        let results = search_regex(&reader, root, ".*", false).unwrap();
        assert_eq!(results.len(), 2); // both lines match
    }

    #[test]
    fn test_search_deleted_file_after_index() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world").unwrap();
        fs::write(root.join("b.txt"), "hello earth").unwrap();
        let index_path = root.join("index.xgrep");
        builder::build_index(root, &index_path).unwrap();
        // Delete a file after indexing
        fs::remove_file(root.join("a.txt")).unwrap();
        let reader = IndexReader::open(&index_path).unwrap();
        // Should still work, just skip the deleted file
        let results = search(&reader, root, "hello", false).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].file.contains("b.txt"));
    }

    #[test]
    fn test_search_utf8_pattern() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "this has some Japanese: テスト\nand more: テスト2").unwrap();
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
        // All-uppercase pattern with -i: trigrams may not be in index
        // Should fallback to full scan and still find the match
        let results = search(&reader, root, "THIS IS UPPERCASE", true).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_search_files_nonexistent_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let files = vec![PathBuf::from("nonexistent.txt")];
        // Should not panic, just return empty
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
    fn test_intersect_postings_all_same() {
        let a = vec![1, 2, 3, 4, 5];
        let b = vec![1, 2, 3, 4, 5];
        let result = intersect_postings(&[&a, &b]);
        assert_eq!(result, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_intersect_postings_no_overlap() {
        let a = vec![1, 3, 5];
        let b = vec![2, 4, 6];
        let result = intersect_postings(&[&a, &b]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_intersect_postings_empty_input() {
        let result = intersect_postings(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_search_files_regex() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.rs"), "fn handle_auth() {}\nfn handle_user() {}").unwrap();
        let files = vec![PathBuf::from("a.rs")];
        let results = search_files_regex(root, &files, "handle_\\w+", false).unwrap();
        assert_eq!(results.len(), 2);
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
}
