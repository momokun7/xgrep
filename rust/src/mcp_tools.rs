//! MCP tool handlers for xgrep search operations.

use serde::Deserialize;
use serde_json::Value;

use crate::{output, SearchOptions, Xgrep};

fn default_max_results() -> usize {
    20
}
fn default_context_lines() -> usize {
    3
}
fn default_max_tokens() -> usize {
    4000
}

#[derive(Deserialize)]
struct SearchParams {
    pattern: String,
    #[serde(default)]
    regex: bool,
    #[serde(default)]
    case_insensitive: bool,
    file_type: Option<String>,
    path_pattern: Option<String>,
    #[serde(default = "default_max_results")]
    max_results: usize,
    #[serde(default = "default_context_lines")]
    context_lines: usize,
    #[serde(default = "default_max_tokens")]
    max_tokens: usize,
}

#[derive(Deserialize)]
struct FindDefinitionsParams {
    symbol: String,
    file_type: Option<String>,
    path_pattern: Option<String>,
    #[serde(default = "default_max_results")]
    max_results: usize,
}

#[derive(Deserialize)]
struct ReadFileParams {
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
}

/// Return MCP tool definitions.
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
            "description": "Find likely definitions of a symbol using regex heuristics (fn/struct/class/def patterns). May include false positives — not AST-based.",
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
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum number of results (default: 20)"
                    }
                },
                "required": ["symbol"]
            }
        }),
        serde_json::json!({
            "name": "index_status",
            "description": "Check the status of the search index. Returns a JSON object: state (\"fresh\", \"stale\", or \"missing\"), changed_files (count, present only when state is \"stale\"), indexed_files (count), index_size_bytes, and index_path.",
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

/// Handler for the `search` tool.
pub fn handle_search(xg: &Xgrep, params: &Value) -> (String, bool) {
    let p: SearchParams = match serde_json::from_value(params.clone()) {
        Ok(v) => v,
        Err(e) => return (format!("Invalid parameters: {}", e), true),
    };

    let opts = SearchOptions {
        case_insensitive: p.case_insensitive,
        regex: p.regex,
        file_type: p.file_type,
        max_count: Some(p.max_results),
        path_pattern: p.path_pattern,
        ..Default::default()
    };

    match xg.search(&p.pattern, &opts) {
        Ok(results) => {
            let file_count = {
                let mut files = results.iter().map(|r| &r.file).collect::<Vec<_>>();
                files.sort();
                files.dedup();
                files.len()
            };
            let total = results.len();
            let header = if total == p.max_results {
                format!(
                    "Found {}+ matches in {} files (limited to {})\n\n",
                    total, file_count, p.max_results
                )
            } else {
                format!("Found {} matches in {} files\n\n", total, file_count)
            };
            match output::format_llm(
                &results,
                xg.root(),
                p.context_lines,
                p.context_lines,
                Some(p.max_tokens),
                false,
            ) {
                Ok(body) => (format!("{}{}", header, body), false),
                Err(e) => (format!("Format error: {}", e), true),
            }
        }
        Err(e) => (format!("Search error: {}", e), true),
    }
}

/// Handler for the `find_definitions` tool.
pub fn handle_find_definitions(xg: &Xgrep, params: &Value) -> (String, bool) {
    let p: FindDefinitionsParams = match serde_json::from_value(params.clone()) {
        Ok(v) => v,
        Err(e) => return (format!("Invalid parameters: {}", e), true),
    };

    let pattern = definition_regex(&p.symbol);

    let opts = SearchOptions {
        regex: true,
        file_type: p.file_type,
        path_pattern: p.path_pattern,
        max_count: Some(p.max_results),
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
            let total = results.len();
            let header = if total == p.max_results {
                format!(
                    "Found {}+ definitions of '{}' in {} files (limited to {}, pass max_results to increase)\n\n",
                    total, p.symbol, file_count, p.max_results
                )
            } else {
                format!(
                    "Found {} definitions of '{}' in {} files\n\n",
                    total, p.symbol, file_count
                )
            };
            match output::format_llm(&results, xg.root(), 3, 3, None, false) {
                Ok(body) => (format!("{}{}", header, body), false),
                Err(e) => (format!("Format error: {}", e), true),
            }
        }
        Err(e) => (format!("Search error: {}", e), true),
    }
}

/// Handler for the `build_index` tool.
pub fn handle_build_index(xg: &Xgrep) -> (String, bool) {
    let start = std::time::Instant::now();
    match xg.build_index() {
        Ok(rebuilt) => {
            let elapsed = start.elapsed().as_secs_f64();
            let size = std::fs::metadata(xg.index_path())
                .map(|m| m.len())
                .unwrap_or(0);
            let msg = if rebuilt {
                format!(
                    "Index built successfully in {:.2}s ({} bytes)",
                    elapsed, size
                )
            } else {
                format!("Index is up to date ({:.2}s, {} bytes)", elapsed, size)
            };
            (msg, false)
        }
        Err(e) => (format!("Build error: {}", e), true),
    }
}

/// Handler for the `index_status` tool. Returns a structured JSON object so
/// agents can branch on the index state without parsing prose.
pub fn handle_index_status(xg: &Xgrep) -> (String, bool) {
    use crate::IndexState;
    match xg.index_status() {
        Ok(info) => {
            let (state, changed_files) = match info.state {
                IndexState::Fresh => ("fresh", None),
                IndexState::Stale { changed_files } => ("stale", Some(changed_files)),
                IndexState::Missing => ("missing", None),
            };
            let mut json = serde_json::json!({
                "state": state,
                "indexed_files": info.indexed_files,
                "index_size_bytes": info.index_size_bytes,
                "index_path": info.index_path.to_string_lossy(),
            });
            if let Some(c) = changed_files {
                json["changed_files"] = c.into();
            }
            (json.to_string(), false)
        }
        Err(e) => (format!("Status check error: {}", e), true),
    }
}

/// Handler for the `read_file` tool.
pub fn handle_read_file(xg: &Xgrep, params: &Value) -> (String, bool) {
    let p: ReadFileParams = match serde_json::from_value(params.clone()) {
        Ok(v) => v,
        Err(e) => return (format!("Invalid parameters: {}", e), true),
    };

    let full_path = xg.root().join(&p.path);

    // Security: prevent path traversal
    let canonical = match full_path.canonicalize() {
        Ok(c) => c,
        Err(e) => return (format!("Cannot read file '{}': {}", p.path, e), true),
    };
    let root_canonical = match xg.root().canonicalize() {
        Ok(c) => c,
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
        Err(e) => return (format!("Cannot read file '{}': {}", p.path, e), true),
    };

    let lines: Vec<&str> = content.lines().collect();

    if lines.is_empty() {
        return (format!("## {}\n\nFile is empty.\n", p.path), false);
    }

    let start = p.start_line.unwrap_or(1).max(1).min(lines.len());
    let end = p
        .end_line
        .unwrap_or(lines.len())
        .max(start)
        .min(lines.len());

    let lang = std::path::Path::new(&p.path)
        .extension()
        .and_then(|e| e.to_str())
        .map(output::lang_from_ext)
        .unwrap_or("");

    let mut output = format!("## {}:{}-{}\n\n```{}\n", p.path, start, end, lang);
    for (i, line) in lines[start - 1..end].iter().enumerate() {
        output.push_str(&format!("{:4} | {}\n", start + i, line));
    }
    output.push_str("```\n");

    (output, false)
}

/// Generate a regex pattern for symbol definitions from a symbol name.
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
        crate::git::git_cmd()
            .args(["init"])
            .current_dir(root)
            .output()
            .unwrap();
        crate::git::git_cmd()
            .args(["config", "user.email", "test@test.com"])
            .current_dir(root)
            .output()
            .unwrap();
        crate::git::git_cmd()
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

        crate::git::git_cmd()
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        crate::git::git_cmd()
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
    fn test_handle_find_definitions_missing_symbol() {
        let (_dir, xg) = setup_test_repo();
        let params = serde_json::json!({});
        let (output, is_error) = handle_find_definitions(&xg, &params);
        assert!(is_error);
        assert!(output.contains("Invalid parameters"));
        assert!(output.contains("symbol"));
    }

    #[test]
    fn test_handle_find_definitions_no_truncation() {
        let (_dir, xg) = setup_test_repo();
        // max_results=100 >> actual results (1), so no truncation signal
        let params = serde_json::json!({"symbol": "hello", "max_results": 100});
        let (output, is_error) = handle_find_definitions(&xg, &params);
        assert!(!is_error, "unexpected error: {}", output);
        assert!(output.contains("Found"));
        assert!(output.contains("definitions"));
        assert!(!output.contains("limited to"));
    }

    #[test]
    fn test_handle_find_definitions_truncation_signal() {
        let (dir, xg) = setup_test_repo();
        // Add extra files so there are many fn/struct matches
        fs::write(
            dir.path().join("extra.rs"),
            "fn hello() {}\nfn hello_world() {}\n",
        )
        .unwrap();
        crate::git::git_cmd()
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .unwrap();
        crate::git::git_cmd()
            .args(["commit", "-m", "add extra"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        xg.build_index().unwrap();

        // max_results=1 forces truncation when hello matches in both files
        let params = serde_json::json!({"symbol": "hello", "max_results": 1});
        let (output, is_error) = handle_find_definitions(&xg, &params);
        assert!(!is_error, "unexpected error: {}", output);
        assert!(
            output.contains("limited to"),
            "expected truncation signal in: {}",
            output
        );
        assert!(
            output.contains("max_results"),
            "expected max_results hint in: {}",
            output
        );
    }

    #[test]
    fn test_handle_index_status_structured_json() {
        let (_dir, xg) = setup_test_repo();
        let (output, is_error) = handle_index_status(&xg);
        assert!(!is_error, "output was: {}", output);
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["state"], "fresh");
        assert!(json["indexed_files"].as_u64().unwrap() >= 1);
        assert!(json["index_size_bytes"].as_u64().unwrap() > 0);
        assert!(json["index_path"].as_str().unwrap().contains("index"));
        // Fresh index has no changed_files field.
        assert!(json.get("changed_files").is_none());
    }

    #[test]
    fn test_handle_index_status_stale_reports_changed_files() {
        let (dir, xg) = setup_test_repo();
        // Modify a tracked file after the index was built so the index goes stale.
        fs::write(
            dir.path().join("hello.rs"),
            "fn hello() {\n    println!(\"changed\");\n}\n",
        )
        .unwrap();
        let (output, is_error) = handle_index_status(&xg);
        assert!(!is_error, "output was: {}", output);
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        // Agents branch on the bare state string, not prose.
        assert_eq!(json["state"], "stale");
        assert!(json["changed_files"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn test_handle_build_index() {
        let (_dir, xg) = setup_test_repo();
        let (output, is_error) = handle_build_index(&xg);
        assert!(!is_error);
        // setup_test_repo already built the index, so this call may return
        // "up to date" rather than "built successfully". Both are valid.
        assert!(
            output.contains("Index built successfully") || output.contains("Index is up to date"),
            "unexpected output: {output}"
        );
        assert!(output.contains("bytes"));
    }

    #[test]
    fn test_handle_search_missing_pattern() {
        let (_dir, xg) = setup_test_repo();
        let params = serde_json::json!({});
        let (output, is_error) = handle_search(&xg, &params);
        assert!(is_error);
        assert!(output.contains("Invalid parameters"));
        assert!(output.contains("pattern"));
    }

    #[test]
    fn test_handle_search_with_max_tokens() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        // git init
        crate::git::git_cmd()
            .args(["init"])
            .current_dir(root)
            .output()
            .unwrap();
        crate::git::git_cmd()
            .args(["config", "user.email", "test@test.com"])
            .current_dir(root)
            .output()
            .unwrap();
        crate::git::git_cmd()
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

        crate::git::git_cmd()
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        crate::git::git_cmd()
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

    #[test]
    fn test_handle_search_invalid_max_results() {
        let (_dir, xg) = setup_test_repo();
        let params = serde_json::json!({"pattern": "hello", "max_results": "not_a_number"});
        let (output, is_error) = handle_search(&xg, &params);
        assert!(is_error);
        assert!(output.contains("Invalid parameters"));
    }

    #[test]
    fn test_handle_search_invalid_bool() {
        let (_dir, xg) = setup_test_repo();
        let params = serde_json::json!({"pattern": "hello", "regex": "yes"});
        let (output, is_error) = handle_search(&xg, &params);
        assert!(is_error);
        assert!(output.contains("Invalid parameters"));
    }

    #[test]
    fn test_handle_search_negative_max_results() {
        let (_dir, xg) = setup_test_repo();
        let params = serde_json::json!({"pattern": "hello", "max_results": -5});
        let (output, is_error) = handle_search(&xg, &params);
        assert!(is_error);
        assert!(output.contains("Invalid parameters"));
    }
}
