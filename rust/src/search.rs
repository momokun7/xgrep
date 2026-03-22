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
        // 3文字未満 → 全ファイルスキャン
        (0..reader.file_count()).collect()
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
                Err(_) => return vec![],
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
                Err(_) => return vec![],
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
            let content = match std::fs::read_to_string(&full_path) {
                Ok(c) => c,
                Err(_) => return vec![],
            };

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
            let content = match std::fs::read_to_string(&full_path) {
                Ok(c) => c,
                Err(_) => return vec![],
            };
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
}
