use serde_json::Value;

use crate::{output, SearchOptions, Xgrep};

/// MCPツール定義を返す
pub fn tools_list() -> Vec<Value> {
    vec![
        serde_json::json!({
            "name": "search",
            "description": "Search for a pattern in the codebase using trigram index. Returns matching lines with context.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Search pattern (literal string or regex if regex=true)"
                    },
                    "regex": {
                        "type": "boolean",
                        "description": "Treat pattern as regex (default: false)"
                    },
                    "case_insensitive": {
                        "type": "boolean",
                        "description": "Case-insensitive search (default: false)"
                    },
                    "file_type": {
                        "type": "string",
                        "description": "Filter by file type (e.g., rs, py, js)"
                    },
                    "path_pattern": {
                        "type": "string",
                        "description": "Filter by path substring"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum number of results (default: 20)"
                    },
                    "context_lines": {
                        "type": "integer",
                        "description": "Number of context lines around each match (default: 3)"
                    },
                    "max_tokens": {
                        "type": "integer",
                        "description": "Maximum output tokens (default: 4000 for MCP, unlimited for CLI)"
                    }
                },
                "required": ["pattern"]
            }
        }),
        serde_json::json!({
            "name": "find_definitions",
            "description": "Find definitions of a symbol (function, struct, class, etc.) in the codebase.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": {
                        "type": "string",
                        "description": "Symbol name to find definitions for"
                    },
                    "file_type": {
                        "type": "string",
                        "description": "Filter by file type (e.g., rs, py, js)"
                    },
                    "path_pattern": {
                        "type": "string",
                        "description": "Filter by path substring"
                    }
                },
                "required": ["symbol"]
            }
        }),
        serde_json::json!({
            "name": "index_status",
            "description": "Check the status of the search index (freshness, file count).",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
        serde_json::json!({
            "name": "build_index",
            "description": "Build or rebuild the search index for the codebase.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
        serde_json::json!({
            "name": "read_file",
            "description": "Read the contents of a file. Use after search to see full file context. Returns file content with line numbers.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative file path (from project root)"},
                    "start_line": {"type": "integer", "description": "Start line number (1-based, optional)"},
                    "end_line": {"type": "integer", "description": "End line number (inclusive, optional)"}
                },
                "required": ["path"]
            }
        }),
    ]
}

/// search ツールのハンドラ
pub fn handle_search(xg: &Xgrep, params: &Value) -> (String, bool) {
    let pattern = match params.get("pattern").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return ("Missing required parameter: pattern".to_string(), true),
    };

    let max_results = params
        .get("max_results")
        .and_then(|v| v.as_u64())
        .unwrap_or(20) as usize;
    let context_lines = params
        .get("context_lines")
        .and_then(|v| v.as_u64())
        .unwrap_or(3) as usize;
    let max_tokens = params
        .get("max_tokens")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(4000);

    let opts = SearchOptions {
        case_insensitive: params
            .get("case_insensitive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        regex: params
            .get("regex")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        file_type: params
            .get("file_type")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        max_count: Some(max_results),
        path_pattern: params
            .get("path_pattern")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        ..Default::default()
    };

    match xg.search(pattern, &opts) {
        Ok(results) => {
            let file_count = {
                let mut files = results.iter().map(|r| &r.file).collect::<Vec<_>>();
                files.sort();
                files.dedup();
                files.len()
            };
            let total = results.len();
            let header = if total == max_results {
                format!(
                    "Found {}+ matches in {} files (limited to {})\n\n",
                    total, file_count, max_results
                )
            } else {
                format!("Found {} matches in {} files\n\n", total, file_count)
            };
            match output::format_llm(&results, xg.root(), context_lines, Some(max_tokens)) {
                Ok(body) => (format!("{}{}", header, body), false),
                Err(e) => (format!("Format error: {}", e), true),
            }
        }
        Err(e) => (format!("Search error: {}", e), true),
    }
}

/// find_definitions ツールのハンドラ
pub fn handle_find_definitions(xg: &Xgrep, params: &Value) -> (String, bool) {
    let symbol = match params.get("symbol").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return ("Missing required parameter: symbol".to_string(), true),
    };

    let pattern = definition_regex(symbol);

    let opts = SearchOptions {
        regex: true,
        file_type: params
            .get("file_type")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        path_pattern: params
            .get("path_pattern")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        max_count: Some(20),
        ..Default::default()
    };

    match xg.search(&pattern, &opts) {
        Ok(results) => {
            let file_count = {
                let mut files = results.iter().map(|r| &r.file).collect::<Vec<_>>();
                files.sort();
                files.dedup();
                files.len()
            };
            let header = format!(
                "Found {} definitions of '{}' in {} files\n\n",
                results.len(),
                symbol,
                file_count
            );
            match output::format_llm(&results, xg.root(), 3, None) {
                Ok(body) => (format!("{}{}", header, body), false),
                Err(e) => (format!("Format error: {}", e), true),
            }
        }
        Err(e) => (format!("Search error: {}", e), true),
    }
}

/// build_index ツールのハンドラ
pub fn handle_build_index(xg: &Xgrep) -> (String, bool) {
    let start = std::time::Instant::now();
    match xg.build_index() {
        Ok(()) => {
            let elapsed = start.elapsed().as_secs_f64();
            let size = std::fs::metadata(xg.index_path())
                .map(|m| m.len())
                .unwrap_or(0);
            (
                format!(
                    "Index built successfully in {:.2}s ({} bytes)",
                    elapsed, size
                ),
                false,
            )
        }
        Err(e) => (format!("Build error: {}", e), true),
    }
}

/// index_status ツールのハンドラ
pub fn handle_index_status(xg: &Xgrep) -> (String, bool) {
    match xg.index_status() {
        Ok(msg) => (msg, false),
        Err(e) => (format!("Status check error: {}", e), true),
    }
}

/// read_file ツールのハンドラ
pub fn handle_read_file(xg: &Xgrep, params: &Value) -> (String, bool) {
    let path = match params.get("path").and_then(|p| p.as_str()) {
        Some(p) => p,
        None => return ("Missing required parameter: path".to_string(), true),
    };

    let full_path = xg.root().join(path);

    // Security: prevent path traversal
    let canonical = match full_path.canonicalize() {
        Ok(p) => p,
        Err(e) => return (format!("Cannot read file '{}': {}", path, e), true),
    };
    let root_canonical = match xg.root().canonicalize() {
        Ok(p) => p,
        Err(e) => return (format!("Cannot resolve root: {}", e), true),
    };
    if !canonical.starts_with(&root_canonical) {
        return (
            "Error: path traversal detected, file is outside project root".to_string(),
            true,
        );
    }

    let content = match std::fs::read_to_string(&canonical) {
        Ok(c) => c,
        Err(e) => return (format!("Cannot read file '{}': {}", path, e), true),
    };

    let lines: Vec<&str> = content.lines().collect();

    if lines.is_empty() {
        return (format!("## {}\n\nFile is empty.\n", path), false);
    }

    let start = params
        .get("start_line")
        .and_then(|v| v.as_u64())
        .unwrap_or(1) as usize;
    let end = params
        .get("end_line")
        .and_then(|v| v.as_u64())
        .unwrap_or(lines.len() as u64) as usize;

    let start = start.max(1).min(lines.len());
    let end = end.max(start).min(lines.len());

    let lang = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(output::lang_from_ext)
        .unwrap_or("");

    let mut output = format!("## {}:{}-{}\n\n```{}\n", path, start, end, lang);
    for (i, line) in lines[start - 1..end].iter().enumerate() {
        output.push_str(&format!("{:4} | {}\n", start + i, line));
    }
    output.push_str("```\n");

    (output, false)
}

/// シンボル名から定義パターンの正規表現を生成
pub fn definition_regex(symbol: &str) -> String {
    let escaped = regex::escape(symbol);
    format!(
        r"(?:pub\s+)?(?:fn|struct|enum|trait|type|impl|class|def|function|func|fun|const|let|var|val|interface)\s+{}\b",
        escaped
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_tools_list() {
        let tools = tools_list();
        assert_eq!(tools.len(), 5);

        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"search"));
        assert!(names.contains(&"find_definitions"));
        assert!(names.contains(&"index_status"));
        assert!(names.contains(&"build_index"));
        assert!(names.contains(&"read_file"));
    }

    fn setup_test_repo() -> (tempfile::TempDir, Xgrep) {
        let dir = tempdir().unwrap();
        let root = dir.path();

        // git init
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(root)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(root)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "test"])
            .current_dir(root)
            .output()
            .unwrap();

        fs::write(root.join(".gitignore"), ".xgrep/\n").unwrap();
        fs::write(
            root.join("hello.rs"),
            "fn hello() {\n    println!(\"hello\");\n}\n\nstruct Foo {\n    x: i32,\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("world.py"),
            "def world():\n    print(\"world\")\n\nclass Bar:\n    pass\n",
        )
        .unwrap();

        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();

        let xg = Xgrep::open_local(root).unwrap();
        xg.build_index().unwrap();
        (dir, xg)
    }

    #[test]
    fn test_handle_search() {
        let (_dir, xg) = setup_test_repo();
        let params = serde_json::json!({"pattern": "hello"});
        let (output, is_error) = handle_search(&xg, &params);
        assert!(!is_error, "output was: {}", output);
        assert!(output.contains("hello"));
        assert!(output.contains("Found"));
    }

    #[test]
    fn test_handle_find_definitions() {
        let (_dir, xg) = setup_test_repo();
        let params = serde_json::json!({"symbol": "hello"});
        let (output, is_error) = handle_find_definitions(&xg, &params);
        assert!(!is_error);
        assert!(output.contains("hello"));
        assert!(output.contains("definitions"));
    }

    #[test]
    fn test_handle_build_index() {
        let (_dir, xg) = setup_test_repo();
        let (output, is_error) = handle_build_index(&xg);
        assert!(!is_error);
        assert!(output.contains("Index built successfully"));
        assert!(output.contains("bytes"));
    }

    #[test]
    fn test_handle_search_missing_pattern() {
        let (_dir, xg) = setup_test_repo();
        let params = serde_json::json!({});
        let (output, is_error) = handle_search(&xg, &params);
        assert!(is_error);
        assert!(output.contains("Missing required parameter: pattern"));
    }

    #[test]
    fn test_handle_search_with_max_tokens() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        // git init
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(root)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(root)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "test"])
            .current_dir(root)
            .output()
            .unwrap();

        fs::write(root.join(".gitignore"), ".xgrep/\n").unwrap();
        // Create file with many matches
        let content: String = (1..=30)
            .map(|i| format!("fn handler_{i}() {{}}\n"))
            .collect();
        fs::write(root.join("a.rs"), &content).unwrap();

        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();

        let xg = Xgrep::open_local(root).unwrap();
        xg.build_index().unwrap();

        // Very low token limit should truncate
        let params = serde_json::json!({"pattern": "handler", "max_tokens": 100});
        let (text, is_error) = handle_search(&xg, &params);
        assert!(!is_error);
        assert!(text.contains("handler"));
        // With 100 tokens, output should be truncated
        assert!(text.contains("truncated") || text.len() < 1000);
    }

    #[test]
    fn test_definition_regex() {
        let re_str = definition_regex("Foo");
        let re = regex::Regex::new(&re_str).unwrap();

        assert!(re.is_match("fn Foo("));
        assert!(re.is_match("pub fn Foo("));
        assert!(re.is_match("struct Foo {"));
        assert!(re.is_match("pub struct Foo {"));
        assert!(re.is_match("enum Foo {"));
        assert!(re.is_match("trait Foo {"));
        assert!(re.is_match("class Foo:"));
        assert!(re.is_match("def Foo("));
        assert!(re.is_match("interface Foo {"));
        assert!(re.is_match("func Foo(")); // Go/Swift
        assert!(re.is_match("fun Foo(")); // Kotlin
        assert!(re.is_match("val Foo =")); // Kotlin/Scala

        // Should NOT match FooBar (word boundary)
        assert!(!re.is_match("fn FooBar("));
        assert!(!re.is_match("struct FooBar {"));
    }

    #[test]
    fn test_handle_read_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("hello.rs"), "line1\nline2\nline3\nline4\nline5").unwrap();

        let xg = crate::Xgrep::open(root).unwrap();

        let params = serde_json::json!({"path": "hello.rs"});
        let (text, is_error) = handle_read_file(&xg, &params);
        assert!(!is_error);
        assert!(text.contains("line1"));
        assert!(text.contains("line5"));
    }

    #[test]
    fn test_handle_read_file_line_range() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("hello.rs"), "line1\nline2\nline3\nline4\nline5").unwrap();

        let xg = crate::Xgrep::open(root).unwrap();

        let params = serde_json::json!({"path": "hello.rs", "start_line": 2, "end_line": 4});
        let (text, is_error) = handle_read_file(&xg, &params);
        assert!(!is_error);
        assert!(text.contains("line2"));
        assert!(text.contains("line4"));
        assert!(!text.contains("line1"));
        assert!(!text.contains("line5"));
    }

    #[test]
    fn test_handle_read_file_path_traversal() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("hello.rs"), "safe").unwrap();

        let xg = crate::Xgrep::open(root).unwrap();

        let params = serde_json::json!({"path": "../../etc/passwd"});
        let (_, is_error) = handle_read_file(&xg, &params);
        assert!(is_error);
    }

    #[test]
    fn test_handle_read_file_empty() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("empty.txt"), "").unwrap();

        let xg = crate::Xgrep::open(root).unwrap();

        let params = serde_json::json!({"path": "empty.txt"});
        let (text, is_error) = handle_read_file(&xg, &params);
        assert!(!is_error);
        assert!(text.contains("empty"));
    }
}
