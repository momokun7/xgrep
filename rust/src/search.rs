use crate::index::reader::IndexReader;
use crate::trigram;
use crate::trigram_query;
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

    let search_pattern = if case_insensitive {
        pattern.to_lowercase()
    } else {
        pattern.to_string()
    };
    let pattern_bytes = search_pattern.as_bytes();
    let trigrams = trigram::extract_trigrams(pattern_bytes);

    let candidate_ids: Vec<u32> = if case_insensitive {
        if trigrams.is_empty() {
            // パターンが3文字未満: 全ファイルスキャン（不可避）
            (0..reader.file_count()).collect()
        } else {
            // バリアント展開のコストを事前計算し、閾値超過なら全ファイルスキャンにフォールバック
            let total_lookups: usize = trigrams.iter().map(|t| case_variants(*t).len()).sum();
            if total_lookups > 64 {
                // 全アルファベットの長いパターン等: posting list lookupが多すぎるため全スキャンが安い
                (0..reader.file_count()).collect()
            } else {
                // Zoekt方式: 各trigramのケースバリアントを列挙してunion → trigram間でintersect
                let mut trigram_candidates: Vec<Vec<u32>> = Vec::new();
                let mut any_empty = false;

                for t in &trigrams {
                    let variants = case_variants(*t);
                    let mut union_set = std::collections::BTreeSet::new();
                    for v in &variants {
                        for fid in reader.lookup_trigram(*v) {
                            union_set.insert(fid);
                        }
                    }
                    if union_set.is_empty() {
                        any_empty = true;
                        break;
                    }
                    trigram_candidates.push(union_set.into_iter().collect());
                }

                if any_empty {
                    // そのtrigramのどのcase variantも存在しない = マッチなし
                    vec![]
                } else {
                    let refs: Vec<&[u32]> =
                        trigram_candidates.iter().map(|v| v.as_slice()).collect();
                    intersect_postings(&refs)
                }
            }
        }
    } else if trigrams.is_empty() {
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
        let posting_lists: Vec<Vec<u32>> =
            trigrams.iter().map(|t| reader.lookup_trigram(*t)).collect();
        let refs: Vec<&[u32]> = posting_lists.iter().map(|v| v.as_slice()).collect();
        intersect_postings(&refs)
    };

    let mut results: Vec<SearchResult> = candidate_ids
        .par_iter()
        .flat_map(|&file_id| {
            let rel_path = reader.file_path(file_id);
            let full_path = root.join(rel_path);

            let content = match fs::read(&full_path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("xgrep: {}: {}", full_path.display(), e);
                    return vec![];
                }
            };

            let mut file_results = Vec::new();

            if case_insensitive {
                let pattern_lower = search_pattern.as_str();
                let content_str = String::from_utf8_lossy(&content);
                for (i, line) in content_str.lines().enumerate() {
                    if contains_case_insensitive(line, pattern_lower) {
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
                    let line_start = content[..abs_pos]
                        .iter()
                        .rposition(|&b| b == b'\n')
                        .map_or(0, |p| p + 1);
                    let line_end = content[abs_pos..]
                        .iter()
                        .position(|&b| b == b'\n')
                        .map_or(content.len(), |p| abs_pos + p);
                    let line =
                        std::str::from_utf8(&content[line_start..line_end]).unwrap_or("<binary>");

                    file_results.push(SearchResult {
                        file: rel_path.to_string(),
                        line_number: line_num,
                        line: line.to_string(),
                    });

                    pos = line_end + 1;
                    if pos >= content.len() {
                        break;
                    }
                }
            }

            file_results
        })
        .collect();

    results.sort_by(|a, b| a.file.cmp(&b.file).then(a.line_number.cmp(&b.line_number)));

    Ok(results)
}

/// Search specified files directly without using the index
pub fn search_files(
    root: &Path,
    files: &[PathBuf],
    pattern: &str,
    case_insensitive: bool,
) -> Result<Vec<SearchResult>> {
    let pattern_lower = if case_insensitive {
        pattern.to_lowercase()
    } else {
        String::new()
    };

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
                    if contains_case_insensitive(line, &pattern_lower) {
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
                    let line_start = content[..abs_pos]
                        .iter()
                        .rposition(|&b| b == b'\n')
                        .map_or(0, |p| p + 1);
                    let line_end = content[abs_pos..]
                        .iter()
                        .position(|&b| b == b'\n')
                        .map_or(content.len(), |p| abs_pos + p);
                    let line =
                        std::str::from_utf8(&content[line_start..line_end]).unwrap_or("<binary>");
                    file_results.push(SearchResult {
                        file: rel_str.clone(),
                        line_number: line_num,
                        line: line.to_string(),
                    });
                    pos = line_end + 1;
                    if pos >= content.len() {
                        break;
                    }
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
pub fn search_regex(
    reader: &IndexReader,
    root: &Path,
    pattern: &str,
    case_insensitive: bool,
) -> Result<Vec<SearchResult>> {
    let re = RegexBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .build()?;

    // Use trigram query for candidate filtering (Zoekt-style AST-based approach)
    let candidate_ids = if case_insensitive {
        // Case-insensitive: parse query, then expand each trigram into case variants
        let query = trigram_query::regex_to_query(pattern);
        if query.is_all() {
            eprintln!(
                "xgrep: warning: regex '{}' cannot be optimized with trigram index (full scan)",
                pattern
            );
        }
        let query = expand_case_variants_query(query);
        query.evaluate(reader)
    } else {
        let query = trigram_query::regex_to_query(pattern);
        if query.is_all() {
            eprintln!(
                "xgrep: warning: regex '{}' cannot be optimized with trigram index (full scan)",
                pattern
            );
        }
        query.evaluate(reader)
    };

    // Verify with regex on candidate files
    let mut results: Vec<SearchResult> = candidate_ids
        .par_iter()
        .flat_map(|&file_id| {
            let rel_path = reader.file_path(file_id);
            let full_path = root.join(rel_path);
            let content_bytes = match std::fs::read(&full_path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("xgrep: {}: {}", full_path.display(), e);
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

/// TrigramQueryツリー内の各Trigramノードをケースバリアントに展開する
fn expand_case_variants_query(query: trigram_query::TrigramQuery) -> trigram_query::TrigramQuery {
    match query {
        trigram_query::TrigramQuery::Trigram(t) => {
            let variants = case_variants(t);
            if variants.len() == 1 {
                trigram_query::TrigramQuery::Trigram(variants[0])
            } else {
                trigram_query::TrigramQuery::Or(
                    variants
                        .into_iter()
                        .map(trigram_query::TrigramQuery::Trigram)
                        .collect(),
                )
            }
        }
        trigram_query::TrigramQuery::And(qs) => trigram_query::TrigramQuery::And(
            qs.into_iter().map(expand_case_variants_query).collect(),
        ),
        trigram_query::TrigramQuery::Or(qs) => trigram_query::TrigramQuery::Or(
            qs.into_iter().map(expand_case_variants_query).collect(),
        ),
        other => other,
    }
}

/// trigramの各バイトについてASCII文字であれば大文字/小文字の両バリアントを生成する。
/// 最大8バリアント（3バイト全てがASCII文字の場合）。
fn case_variants(trigram: [u8; 3]) -> Vec<[u8; 3]> {
    let cases: [Vec<u8>; 3] = [
        if trigram[0].is_ascii_alphabetic() {
            vec![
                trigram[0].to_ascii_lowercase(),
                trigram[0].to_ascii_uppercase(),
            ]
        } else {
            vec![trigram[0]]
        },
        if trigram[1].is_ascii_alphabetic() {
            vec![
                trigram[1].to_ascii_lowercase(),
                trigram[1].to_ascii_uppercase(),
            ]
        } else {
            vec![trigram[1]]
        },
        if trigram[2].is_ascii_alphabetic() {
            vec![
                trigram[2].to_ascii_lowercase(),
                trigram[2].to_ascii_uppercase(),
            ]
        } else {
            vec![trigram[2]]
        },
    ];

    let mut variants = Vec::with_capacity(cases[0].len() * cases[1].len() * cases[2].len());
    for &b0 in &cases[0] {
        for &b1 in &cases[1] {
            for &b2 in &cases[2] {
                variants.push([b0, b1, b2]);
            }
        }
    }
    variants
}

pub fn intersect_postings(lists: &[&[u32]]) -> Vec<u32> {
    if lists.is_empty() {
        return vec![];
    }
    if lists.len() == 1 {
        return lists[0].to_vec();
    }

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
        let a = vec![1u32, 2, 3];
        let result = intersect_postings(&[&a, &[]]);
        assert_eq!(result, Vec::<u32>::new());
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
        assert_eq!(extract_literals("a.b"), ""); // single chars between specials
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
        // 異なるケースの同一内容が複数ファイルにあるとき、-iで全てヒットすること
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
        // upper.txt, lower.txt, mixed.txt の3ファイルがヒット
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
    fn test_case_variants() {
        let variants = case_variants(*b"hel");
        assert_eq!(variants.len(), 8);
        assert!(variants.contains(b"hel"));
        assert!(variants.contains(b"HEL"));
        assert!(variants.contains(b"Hel"));
        assert!(variants.contains(b"hEL"));
    }

    #[test]
    fn test_case_variants_non_letter() {
        // 数字や記号はバリアントを生成しない
        let variants = case_variants(*b"h1!");
        assert_eq!(variants.len(), 2); // h/H のみ
        assert!(variants.contains(b"h1!"));
        assert!(variants.contains(b"H1!"));
    }

    #[test]
    fn test_case_variants_all_non_letter() {
        let variants = case_variants(*b"123");
        assert_eq!(variants.len(), 1);
        assert!(variants.contains(b"123"));
    }

    #[test]
    fn test_search_case_insensitive_uses_index() {
        // case-insensitive検索がインデックスを使って候補を絞れることを確認
        let dir = tempdir().unwrap();
        let root = dir.path();
        // 多数のファイルを作成し、パターンを含むのは1つだけ
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
        // .* pattern can't use trigram index
        let results = search_regex(&reader, root, ".*", false).unwrap();
        assert!(results.len() >= 1);
    }

    #[test]
    fn test_case_insensitive_long_pattern_fallback() {
        // 長い全アルファベットパターンでバリアント展開が閾値を超える場合、
        // フォールバックにより高速に完了することを確認
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

        // 長い全アルファベットパターン: フォールバックで高速に完了すべき
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
        // "xyznonexistent" doesn't exist in any case variant
        let results = search(&reader, root, "xyznonexistent", true).unwrap();
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_contains_case_insensitive() {
        // needleは常にlowercase前提（呼び出し側でto_lowercase済み）
        assert!(contains_case_insensitive("Hello World", "hello"));
        assert!(contains_case_insensitive("HELLO WORLD", "hello world"));
        assert!(contains_case_insensitive("hello", "hello"));
        assert!(!contains_case_insensitive("hello", "xyz"));
        assert!(contains_case_insensitive("anything", ""));
        assert!(!contains_case_insensitive("hi", "longer"));
    }
}
