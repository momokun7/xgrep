use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::Result;

use crate::search::SearchResult;

/// ファイル拡張子からMarkdownコードブロック用の言語名を返す
pub fn lang_from_ext(ext: &str) -> &'static str {
    match ext {
        "rs" => "rust",
        "py" => "python",
        "js" => "javascript",
        "ts" => "typescript",
        "go" => "go",
        "rb" => "ruby",
        "java" => "java",
        "c" | "h" => "c",
        "cpp" | "cc" | "hpp" => "cpp",
        "sh" | "bash" => "bash",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "md" => "markdown",
        "html" => "html",
        "css" => "css",
        "sql" => "sql",
        _ => "",
    }
}

/// ripgrep互換のdefault形式で出力
pub fn format_default(results: &[SearchResult]) -> String {
    results
        .iter()
        .map(|r| format!("{}:{}:{}", r.file, r.line_number, r.line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// JSON形式で出力
pub fn format_json(results: &[SearchResult]) -> String {
    let json_results: Vec<serde_json::Value> = results
        .iter()
        .map(|r| {
            serde_json::json!({
                "file": r.file,
                "line_number": r.line_number,
                "line": r.line
            })
        })
        .collect();
    serde_json::to_string_pretty(&json_results).unwrap_or_else(|_| "[]".to_string())
}

/// LLM向けMarkdownコードブロック形式で出力（コンテキスト行付き）
pub fn format_llm(
    results: &[SearchResult],
    root: &Path,
    context_lines: usize,
    max_tokens: Option<usize>,
) -> Result<String> {
    if results.is_empty() {
        return Ok(String::new());
    }

    // Group by file
    let mut by_file: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for r in results {
        by_file.entry(&r.file).or_default().push(r.line_number);
    }

    let mut output = String::new();
    let mut first_block = true;
    let mut files_shown = 0;
    let total_files = by_file.len();

    for (file, line_numbers) in &by_file {
        let full_path = root.join(file);
        let content = fs::read_to_string(&full_path)?;
        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        let lang = Path::new(file)
            .extension()
            .and_then(|e| e.to_str())
            .map(lang_from_ext)
            .unwrap_or("");

        // Calculate context ranges and merge overlapping
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        for &line_num in line_numbers {
            let start = line_num.saturating_sub(context_lines).max(1);
            let end = (line_num + context_lines).min(total_lines);

            if let Some(last) = ranges.last_mut() {
                if start <= last.1 + 1 {
                    last.1 = last.1.max(end);
                    continue;
                }
            }
            ranges.push((start, end));
        }

        for (start, end) in ranges {
            if !first_block {
                output.push('\n');
            }
            first_block = false;

            if start == end {
                output.push_str(&format!("## {}:{}\n\n", file, start));
            } else {
                output.push_str(&format!("## {}:{}-{}\n\n", file, start, end));
            }

            output.push_str(&format!("```{}\n", lang));
            for i in start..=end {
                if i <= lines.len() {
                    output.push_str(lines[i - 1]);
                    output.push('\n');
                }
            }
            output.push_str("```\n");
        }

        files_shown += 1;

        // Check token limit after each file
        if let Some(max) = max_tokens {
            if output.len() / 4 >= max {
                let remaining_files = total_files - files_shown;
                let remaining_matches: usize = by_file
                    .iter()
                    .skip(files_shown)
                    .map(|(_, lines)| lines.len())
                    .sum();
                if remaining_files > 0 || remaining_matches > 0 {
                    output.push_str(&format!(
                        "\n... (truncated, {} more matches in {} more files)\n",
                        remaining_matches, remaining_files
                    ));
                }
                break;
            }
        }
    }

    Ok(output)
}

/// ripgrep互換のコンテキスト付き出力（`--`セパレータ）
pub fn format_default_context(
    results: &[SearchResult],
    root: &Path,
    context_lines: usize,
) -> Result<String> {
    if results.is_empty() {
        return Ok(String::new());
    }

    let mut by_file: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for r in results {
        by_file.entry(&r.file).or_default().push(r.line_number);
    }

    let mut parts: Vec<String> = Vec::new();

    for (file, line_numbers) in &by_file {
        let full_path = root.join(file);
        let content = fs::read_to_string(&full_path)?;
        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        let mut ranges: Vec<(usize, usize, Vec<usize>)> = Vec::new();
        for &ln in line_numbers {
            let start = ln.saturating_sub(context_lines).max(1);
            let end = (ln + context_lines).min(total_lines);
            if let Some(last) = ranges.last_mut() {
                if start <= last.1 + 1 {
                    last.1 = last.1.max(end);
                    last.2.push(ln);
                    continue;
                }
            }
            ranges.push((start, end, vec![ln]));
        }

        for (start, end, match_lines) in ranges {
            let mut block = String::new();
            for i in start..=end {
                if i <= lines.len() {
                    if match_lines.contains(&i) {
                        block.push_str(&format!("{}:{}:{}\n", file, i, lines[i - 1]));
                    } else {
                        block.push_str(&format!("{}-{}-{}\n", file, i, lines[i - 1]));
                    }
                }
            }
            parts.push(block.trim_end().to_string());
        }
    }

    // Separator between context groups (ripgrep uses -- for both intra-file and inter-file)
    Ok(parts.join("\n--\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_format_json() {
        let results = vec![SearchResult {
            file: "src/main.rs".to_string(),
            line_number: 42,
            line: "fn handle_auth() {}".to_string(),
        }];
        let json = format_json(&results);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed[0]["file"], "src/main.rs");
        assert_eq!(parsed[0]["line_number"], 42);
        assert_eq!(parsed[0]["line"], "fn handle_auth() {}");
    }

    #[test]
    fn test_format_json_empty() {
        let json = format_json(&[]);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.as_array().unwrap().is_empty());
    }

    #[test]
    fn test_format_json_multiple() {
        let results = vec![
            SearchResult {
                file: "a.rs".to_string(),
                line_number: 1,
                line: "foo".to_string(),
            },
            SearchResult {
                file: "b.rs".to_string(),
                line_number: 2,
                line: "bar".to_string(),
            },
        ];
        let json = format_json(&results);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 2);
        assert_eq!(parsed[1]["file"], "b.rs");
    }

    #[test]
    fn test_format_default() {
        let results = vec![SearchResult {
            file: "src/main.rs".to_string(),
            line_number: 42,
            line: "    fn handle_auth() {}".to_string(),
        }];
        let output = format_default(&results);
        assert_eq!(output, "src/main.rs:42:    fn handle_auth() {}");
    }

    #[test]
    fn test_format_default_multiple() {
        let results = vec![
            SearchResult {
                file: "a.rs".to_string(),
                line_number: 1,
                line: "foo".to_string(),
            },
            SearchResult {
                file: "b.rs".to_string(),
                line_number: 10,
                line: "bar".to_string(),
            },
        ];
        let output = format_default(&results);
        assert_eq!(output, "a.rs:1:foo\nb.rs:10:bar");
    }

    #[test]
    fn test_lang_from_ext() {
        assert_eq!(lang_from_ext("rs"), "rust");
        assert_eq!(lang_from_ext("py"), "python");
        assert_eq!(lang_from_ext("js"), "javascript");
        assert_eq!(lang_from_ext("ts"), "typescript");
        assert_eq!(lang_from_ext("go"), "go");
        assert_eq!(lang_from_ext("c"), "c");
        assert_eq!(lang_from_ext("h"), "c");
        assert_eq!(lang_from_ext("cpp"), "cpp");
        assert_eq!(lang_from_ext("xyz"), "");
    }

    #[test]
    fn test_format_llm_single_match() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("test.rs"),
            "line1\nline2\nfn hello() {}\nline4\nline5\nline6\nline7",
        )
        .unwrap();
        let results = vec![SearchResult {
            file: "test.rs".to_string(),
            line_number: 3,
            line: "fn hello() {}".to_string(),
        }];
        let output = format_llm(&results, root, 2, None).unwrap();
        assert!(output.contains("## test.rs:"));
        assert!(output.contains("```rust"));
        assert!(output.contains("fn hello() {}"));
        assert!(output.contains("line2"));
        assert!(output.contains("line4"));
    }

    #[test]
    fn test_format_llm_merge_context() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("test.rs"), "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8").unwrap();
        let results = vec![
            SearchResult {
                file: "test.rs".to_string(),
                line_number: 3,
                line: "l3".to_string(),
            },
            SearchResult {
                file: "test.rs".to_string(),
                line_number: 5,
                line: "l5".to_string(),
            },
        ];
        let output = format_llm(&results, root, 1, None).unwrap();
        let block_count = output.matches("```rust").count();
        assert_eq!(block_count, 1); // merged into one block
    }

    #[test]
    fn test_format_default_context() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("test.rs"),
            "line1\nline2\nmatch_line\nline4\nline5",
        )
        .unwrap();
        let results = vec![SearchResult {
            file: "test.rs".to_string(),
            line_number: 3,
            line: "match_line".to_string(),
        }];
        let output = format_default_context(&results, root, 1).unwrap();
        assert!(output.contains("test.rs-2-line2")); // context line uses -
        assert!(output.contains("test.rs:3:match_line")); // match line uses :
        assert!(output.contains("test.rs-4-line4")); // context line uses -
    }

    #[test]
    fn test_format_default_empty() {
        let output = format_default(&[]);
        assert_eq!(output, "");
    }

    #[test]
    fn test_format_llm_empty() {
        let dir = tempdir().unwrap();
        let output = format_llm(&[], dir.path(), 3, None).unwrap();
        assert_eq!(output, "");
    }

    #[test]
    fn test_format_llm_no_extension() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Makefile"), "all:\n\techo hello\n\techo done").unwrap();
        let results = vec![SearchResult {
            file: "Makefile".to_string(),
            line_number: 2,
            line: "\techo hello".to_string(),
        }];
        let output = format_llm(&results, root, 1, None).unwrap();
        // No extension = empty language
        assert!(output.contains("```\n")); // no language after ```
        assert!(output.contains("echo hello"));
    }

    #[test]
    fn test_format_llm_context_zero() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("test.rs"), "line1\nline2\nline3\nline4\nline5").unwrap();
        let results = vec![SearchResult {
            file: "test.rs".to_string(),
            line_number: 3,
            line: "line3".to_string(),
        }];
        let output = format_llm(&results, root, 0, None).unwrap();
        assert!(output.contains("line3"));
        assert!(!output.contains("line2")); // no context
        assert!(!output.contains("line4"));
    }

    #[test]
    fn test_format_default_context_empty() {
        let dir = tempdir().unwrap();
        let output = format_default_context(&[], dir.path(), 3).unwrap();
        assert_eq!(output, "");
    }

    #[test]
    fn test_format_llm_max_tokens() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // Create multiple files so truncation can kick in between files
        let content: String = (1..=20).map(|i| format!("line number {}\n", i)).collect();
        fs::write(root.join("a.rs"), &content).unwrap();
        fs::write(root.join("b.rs"), &content).unwrap();
        fs::write(root.join("c.rs"), &content).unwrap();

        let mut results: Vec<SearchResult> = Vec::new();
        for file in &["a.rs", "b.rs", "c.rs"] {
            for i in (2..=10).step_by(2) {
                results.push(SearchResult {
                    file: file.to_string(),
                    line_number: i,
                    line: format!("line number {}", i),
                });
            }
        }

        // With very low token limit, should truncate after first file
        let output = format_llm(&results, root, 1, Some(50)).unwrap();
        assert!(output.contains("truncated"));
        assert!(output.contains("more matches"));
        assert!(output.contains("more files"));
    }

    #[test]
    fn test_format_llm_no_token_limit() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.rs"), "line1\nline2\nline3").unwrap();

        let results = vec![SearchResult {
            file: "a.rs".to_string(),
            line_number: 2,
            line: "line2".to_string(),
        }];

        let output = format_llm(&results, root, 1, None).unwrap();
        assert!(!output.contains("truncated"));
        assert!(output.contains("line2"));
    }
}
