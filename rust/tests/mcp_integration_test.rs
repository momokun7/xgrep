// rust/tests/mcp_integration_test.rs
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};
use tempfile::tempdir;

struct McpOutput {
    responses: Vec<serde_json::Value>,
    stderr: String,
}

fn send_mcp_messages(root: &std::path::Path, messages: &[&str]) -> Vec<serde_json::Value> {
    send_mcp_messages_full(root, messages).responses
}

fn send_mcp_messages_full(root: &std::path::Path, messages: &[&str]) -> McpOutput {
    let binary = env!("CARGO_BIN_EXE_xg");
    // Keep any cache-dir index out of the user's real ~/.cache.
    let cache = root.join(".test-cache");
    let mut child = Command::new(binary)
        .args(["serve", "--root", &root.to_string_lossy()])
        .env("XDG_CACHE_HOME", &cache)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to start xg serve");

    let stdin = child.stdin.as_mut().unwrap();
    for msg in messages {
        writeln!(stdin, "{}", msg).unwrap();
    }
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("Failed to read output");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    McpOutput {
        responses: stdout
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect(),
        stderr,
    }
}

#[test]
fn test_mcp_initialize_and_search() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::write(
        root.join("hello.rs"),
        "fn main() {\n    println!(\"hello\");\n}",
    )
    .unwrap();

    let responses = send_mcp_messages(
        root,
        &[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search","arguments":{"pattern":"hello"}}}"#,
        ],
    );

    // initialize response
    assert_eq!(responses[0]["id"], 1);
    assert!(responses[0]["result"]["protocolVersion"].is_string());

    // tools/list response (notification has no response, so index 1)
    assert_eq!(responses[1]["id"], 2);
    let tools = responses[1]["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 7);

    // search response
    assert_eq!(responses[2]["id"], 3);
    let text = responses[2]["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(text.contains("hello"));
}

#[test]
fn test_mcp_find_definitions() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::write(
        root.join("lib.rs"),
        "pub fn calculate(x: i32) -> i32 { x * 2 }",
    )
    .unwrap();

    let responses = send_mcp_messages(
        root,
        &[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"find_definitions","arguments":{"symbol":"calculate"}}}"#,
        ],
    );

    assert_eq!(responses[1]["id"], 2);
    let text = responses[1]["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(text.contains("calculate"));
}

#[test]
fn test_mcp_build_index() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("a.rs"), "fn hello() {}").unwrap();

    let responses = send_mcp_messages(
        root,
        &[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"build_index","arguments":{}}}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"index_status","arguments":{}}}"#,
        ],
    );

    // build_index
    assert_eq!(responses[1]["id"], 2);
    let text = responses[1]["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(text.contains("Index built"));

    // index_status returns structured JSON
    assert_eq!(responses[2]["id"], 3);
    let status_text = responses[2]["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    let status: serde_json::Value = serde_json::from_str(status_text).unwrap();
    assert!(status["state"].as_str().unwrap().contains("fresh"));
    assert!(status["indexed_files"].as_u64().unwrap() >= 1);
    assert!(status["index_path"].as_str().unwrap().contains("index"));
}

#[test]
fn test_mcp_unknown_tool() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("a.rs"), "hello").unwrap();

    let responses = send_mcp_messages(
        root,
        &[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"nonexistent","arguments":{}}}"#,
        ],
    );

    assert_eq!(responses[1]["id"], 2);
    assert!(responses[1]["error"]["code"].as_i64().unwrap() == -32602);
}

#[test]
fn test_mcp_input_validation_error() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("a.rs"), "fn hello() {}").unwrap();

    let responses = send_mcp_messages(
        root,
        &[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search","arguments":{"pattern":"hello","max_results":"not_a_number"}}}"#,
        ],
    );

    assert_eq!(responses[1]["id"], 2);
    let text = responses[1]["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(text.contains("Invalid parameters"));
    assert_eq!(responses[1]["result"]["isError"], true);
}

#[test]
fn test_mcp_stderr_suppressed() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    // Short pattern (< 3 chars) normally triggers a stderr warning
    fs::write(root.join("a.rs"), "fn ab() {}").unwrap();

    let output = send_mcp_messages_full(
        root,
        &[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search","arguments":{"pattern":"ab"}}}"#,
        ],
    );

    // Search should succeed
    assert_eq!(output.responses[1]["id"], 2);
    assert_eq!(output.responses[1]["result"]["isError"], false);
    // stderr should be empty in MCP mode (no "pattern shorter than 3" warning)
    assert!(
        output.stderr.is_empty(),
        "Expected no stderr in MCP mode, got: {}",
        output.stderr
    );
}

// ---------------------------------------------------------------------------
// Helpers for tool-level tests
// ---------------------------------------------------------------------------

const INIT: &str = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#;

/// Run tool calls in one server session; returns (text, is_error) per call.
fn call_tools(root: &std::path::Path, calls: &[(&str, serde_json::Value)]) -> Vec<(String, bool)> {
    let mut msgs = vec![INIT.to_string()];
    for (i, (name, args)) in calls.iter().enumerate() {
        msgs.push(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": i + 1,
                "method": "tools/call",
                "params": {"name": name, "arguments": args}
            })
            .to_string(),
        );
    }
    let refs: Vec<&str> = msgs.iter().map(|s| s.as_str()).collect();
    let responses = send_mcp_messages(root, &refs);
    responses[1..]
        .iter()
        .map(|r| {
            (
                r["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                r["result"]["isError"].as_bool().unwrap_or(true),
            )
        })
        .collect()
}

fn call_one(root: &std::path::Path, name: &str, args: serde_json::Value) -> (String, bool) {
    call_tools(root, &[(name, args)]).remove(0)
}

fn write(root: &std::path::Path, rel: &str, content: &str) {
    let p = root.join(rel);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(p, content).unwrap();
}

// ---------------------------------------------------------------------------
// read_file (4-2)
// ---------------------------------------------------------------------------

#[test]
fn test_mcp_read_file_caps_binary_and_ranges() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let big: String = (1..=3000).map(|i| format!("row {i}\n")).collect();
    write(root, "big.txt", &big);
    fs::write(root.join("blob.bin"), b"head\0tail").unwrap();
    write(root, "min.js", &"x".repeat(300_000));
    write(root, "small.py", "a\nb\nc\n");
    fs::create_dir_all(root.join(".git")).unwrap();
    write(root, ".git/config", "[core]\n");

    let out = call_tools(
        root,
        &[
            ("read_file", serde_json::json!({"path": "big.txt"})),
            (
                "read_file",
                serde_json::json!({"path": "big.txt", "start_line": 2001}),
            ),
            ("read_file", serde_json::json!({"path": "blob.bin"})),
            ("read_file", serde_json::json!({"path": "min.js"})),
            (
                "read_file",
                serde_json::json!({"path": "small.py", "start_line": 50}),
            ),
            (
                "read_file",
                serde_json::json!({"path": "small.py", "start_line": 3, "end_line": 1}),
            ),
            ("read_file", serde_json::json!({"path": ".git/config"})),
        ],
    );
    let (t, e) = &out[0];
    assert!(!e);
    assert!(t.contains("3000 lines total"), "{t}");
    assert!(
        t.contains("next start_line=2001"),
        "{}",
        &t[t.len() - 120..]
    );
    assert!(!t.contains("row 2001\n"));
    let (t, e) = &out[1];
    assert!(!e);
    assert!(
        t.contains("2001 | row 2001") && t.contains("3000 | row 3000"),
        "{t}"
    );
    assert!(out[2].1 && out[2].0.contains("binary"), "{:?}", out[2]);
    let (t, e) = &out[3];
    assert!(!e);
    assert!(t.len() < 2_000, "minified line not shortened: {}", t.len());
    assert!(t.contains("chars]"));
    assert!(
        out[4].1 && out[4].0.contains("past the end"),
        "{:?}",
        out[4]
    );
    assert!(
        out[5].1 && out[5].0.contains("before start_line"),
        "{:?}",
        out[5]
    );
    assert!(out[6].1 && out[6].0.contains(".git"), "{:?}", out[6]);
}

// ---------------------------------------------------------------------------
// Argument validation and zero-result hints (4-6)
// ---------------------------------------------------------------------------

#[test]
fn test_mcp_validation_and_hints() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/auth.rs",
        "fn handleLogin(x: i32) {}\nfn main() { handleLogin(1); }\n",
    );
    write(root, "notes.md", "handleLogin is documented here\n");

    let out = call_tools(
        root,
        &[
            (
                "search",
                serde_json::json!({"pattern": "handleLogin", "file_type": "xyz"}),
            ),
            (
                "search",
                serde_json::json!({"pattern": "handleLogin", "file_type": ".rs", "include_seen": true}),
            ),
            (
                "search",
                serde_json::json!({"pattern": "handleLogin", "path_pattern": "*.rs", "include_seen": true}),
            ),
            ("search", serde_json::json!({"pattern": "handleLogin\\("})),
            ("search", serde_json::json!({"pattern": "HANDLELOGIN"})),
            (
                "search",
                serde_json::json!({"pattern": "handlelogin", "include_seen": true}),
            ),
            (
                "search",
                serde_json::json!({"pattern": "handlelogin", "case_insensitive": false}),
            ),
            (
                "search",
                serde_json::json!({"pattern": "handleLogin", "file_type": "py"}),
            ),
            (
                "search",
                serde_json::json!({"pattern": "x", "pathpattern": "src"}),
            ),
        ],
    );
    let (t, e) = &out[0];
    assert!(*e, "{t}");
    assert!(
        t.contains("Unknown file_type 'xyz'") && t.contains("rust"),
        "{t}"
    );
    let (t, e) = &out[1];
    assert!(!e);
    assert!(
        t.contains("read as 'rs'") && t.contains("Found 2 matches in 1 file"),
        "{t}"
    );
    let (t, e) = &out[2];
    assert!(!e);
    assert!(
        t.contains("applied as a glob") && t.contains("Found 2 matches"),
        "{t}"
    );
    let (t, e) = &out[3];
    assert!(!e);
    assert!(
        t.contains("No matches") && t.contains("Unescaped `handleLogin(` has 2 matches"),
        "{t}"
    );
    let (t, _) = &out[4];
    assert!(t.contains("Case-insensitive search finds 3 matches"), "{t}");
    // smart-case: all-lowercase pattern is case-insensitive by default
    let (t, _) = &out[5];
    assert!(
        t.contains("Found 3 matches") && t.contains("smart-case: case-insensitive"),
        "{t}"
    );
    // explicit override wins
    let (t, _) = &out[6];
    assert!(t.contains("No matches"), "{t}");
    let (t, _) = &out[7];
    assert!(t.contains("3 matches exist outside"), "{t}");
    let (t, e) = &out[8];
    assert!(*e && t.contains("unknown field `pathpattern`"), "{t}");
}

// ---------------------------------------------------------------------------
// CLI parity: glob/word/output_mode/context/find_files/changed_only (4-7)
// ---------------------------------------------------------------------------

#[test]
fn test_mcp_glob_word_modes_and_find_files() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/a.rs",
        "let id = 1;\nlet width = 2;\nlet id2 = id;\n",
    );
    write(root, "src/b.py", "id = 3\n");
    write(root, "tests/t.rs", "let id = 4;\n");
    write(root, "src/mcp_tools.rs", "fn x() {}\n");

    let out = call_tools(
        root,
        &[
            (
                "search",
                serde_json::json!({"pattern": "id", "word": true, "glob": "*.rs", "exclude": ["tests/**"], "output_mode": "count"}),
            ),
            (
                "search",
                serde_json::json!({"pattern": "id", "word": true, "output_mode": "files"}),
            ),
            (
                "search",
                serde_json::json!({"pattern": "id", "word": true, "mode": "locations"}),
            ),
            (
                "search",
                serde_json::json!({"pattern": "width", "context_lines": 1}),
            ),
            ("find_files", serde_json::json!({"pattern": "mcp_tools"})),
            ("find_files", serde_json::json!({"pattern": "*.rs"})),
            (
                "find_files",
                serde_json::json!({"pattern": "*.rs", "exclude": "tests/**"}),
            ),
            (
                "search",
                serde_json::json!({"pattern": "id", "output_mode": "nope"}),
            ),
        ],
    );
    let (t, _) = &out[0];
    assert!(
        t.contains("src/a.rs:2") && !t.contains("tests/t.rs") && !t.contains("b.py"),
        "{t}"
    );
    let (t, _) = &out[1];
    assert!(
        t.contains("src/a.rs (2 matches, first L1)") && t.contains("src/b.py (1 match"),
        "{t}"
    );
    let (t, _) = &out[2];
    assert!(t.contains("src/a.rs: L1,L3"), "{t}");
    let (t, _) = &out[3];
    assert!(
        t.contains("let id = 1;") && t.contains("let id2 = id;"),
        "context_lines:1 {t}"
    );
    let (t, _) = &out[4];
    assert!(
        t.contains("Found 1 file") && t.contains("src/mcp_tools.rs"),
        "{t}"
    );
    let (t, _) = &out[5];
    assert!(t.contains("Found 3 files"), "{t}");
    let (t, _) = &out[6];
    assert!(
        t.contains("Found 2 files") && !t.contains("tests/t.rs"),
        "{t}"
    );
    assert!(out[7].1 && out[7].0.contains("Unknown output_mode"));
}

#[test]
fn test_mcp_changed_only() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let vcs = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
    };
    vcs(&["init", "-q"]);
    vcs(&["config", "user.email", "t@t"]);
    vcs(&["config", "user.name", "t"]);
    write(root, ".gitignore", ".test-cache/\n");
    write(root, "a.rs", "fn needle_one() {}\n");
    write(root, "b.rs", "fn needle_two() {}\n");
    vcs(&["add", "."]);
    vcs(&["commit", "-qm", "init"]);
    write(root, "b.rs", "fn needle_two() {}\nfn needle_three() {}\n");

    let (t, e) = call_one(
        root,
        "search",
        serde_json::json!({"pattern": "needle", "changed_only": true}),
    );
    assert!(!e, "{t}");
    assert!(
        t.contains("Found 2 matches in 1 file") && t.contains("b.rs") && !t.contains("a.rs"),
        "{t}"
    );
}

// ---------------------------------------------------------------------------
// Pagination (4-5)
// ---------------------------------------------------------------------------

#[test]
fn test_mcp_pagination_exact_total_and_next_offset() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let body: String = (1..=50).map(|i| format!("marker {i}\n\n\n")).collect();
    write(root, "a.txt", &body);

    let out = call_tools(
        root,
        &[
            (
                "search",
                serde_json::json!({"pattern": "marker", "max_results": 20}),
            ),
            (
                "search",
                serde_json::json!({"pattern": "marker", "max_results": 20, "offset": 40}),
            ),
            (
                "search",
                serde_json::json!({"pattern": "marker", "max_results": 500, "max_tokens": 200, "include_seen": true}),
            ),
            (
                "search",
                serde_json::json!({"pattern": "marker", "offset": 99}),
            ),
        ],
    );
    let (t, _) = &out[0];
    assert!(t.contains("Found 50 matches"), "{t}");
    assert!(
        t.contains("Showing results 1-20 of 50") && t.contains("next_offset=20"),
        "{t}"
    );
    let (t, _) = &out[1];
    assert!(
        t.contains("Showing results 41-50 of 50") && t.contains("end of results"),
        "{t}"
    );
    assert!(
        t.contains("marker 41") && t.contains("marker 50") && !t.contains("marker 40\n"),
        "{t}"
    );
    // token budget shrinks the page but next_offset stays exact
    let (t, _) = &out[2];
    let shown: Vec<usize> = t
        .lines()
        // Lines look like `4> marker 2` (line-number gutter + match marker).
        .filter_map(|l| l.split_once("> ").map(|(_, rest)| rest))
        .filter_map(|l| l.strip_prefix("marker "))
        .map(|n| n.parse().unwrap())
        .collect();
    assert!(!shown.is_empty() && shown.len() < 50, "{t}");
    assert!(
        t.contains(&format!("next_offset={}", shown.len())),
        "next_offset should equal shown count {}: {t}",
        shown.len()
    );
    assert!(t.len() < 200 * 4 + 400, "budget exceeded: {}", t.len());
    assert!(out[3].0.contains("past the end"), "{}", out[3].0);
}

// ---------------------------------------------------------------------------
// Ranking and dedupe (4-3)
// ---------------------------------------------------------------------------

#[test]
fn test_mcp_ranking_definition_and_source_first() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, "CHANGELOG.md", "- renamed WidgetConfig\n");
    write(root, "docs/guide.md", "Use WidgetConfig to configure.\n");
    write(root, "benches/b.rs", "use crate::WidgetConfig;\n");
    write(
        root,
        "src/aaa.rs",
        "// WidgetConfig is mentioned in a comment\nfn f(c: WidgetConfig) {}\n",
    );
    write(
        root,
        "src/lib.rs",
        "pub struct WidgetConfig {\n    x: i32,\n}\n",
    );
    write(root, "tests/t.rs", "let c = WidgetConfig { x: 1 };\n");
    // identical copies are collapsed into the higher-ranked one
    write(root, "src/copy_a.rs", "fn uses(c: WidgetConfig) {}\n");
    write(root, "src/copy_b.rs", "fn uses(c: WidgetConfig) {}\n");

    let (t, _) = call_one(
        root,
        "search",
        serde_json::json!({"pattern": "WidgetConfig"}),
    );
    let order: Vec<&str> = t.lines().filter_map(|l| l.strip_prefix("## ")).collect();
    assert_eq!(order.first().copied(), Some("src/lib.rs:1"), "{t}");
    let pos = |p: &str| order.iter().position(|l| l.starts_with(p)).unwrap();
    assert!(pos("src/aaa.rs") < pos("tests/t.rs"), "{t}");
    assert!(pos("tests/t.rs") < pos("docs/guide.md"), "{t}");
    assert!(pos("docs/guide.md") < pos("CHANGELOG.md"), "{t}");
    assert!(
        t.contains("Collapsed 1 match in 1 file with content identical"),
        "{t}"
    );
    assert!(
        t.contains("identical content also in: src/copy_b.rs"),
        "{t}"
    );

    // A comment-only mention ranks below code mentions in other source files.
    let (t, _) = call_one(
        root,
        "search",
        serde_json::json!({"pattern": "WidgetConfig", "path_pattern": "src/", "output_mode": "locations"}),
    );
    let files: Vec<&str> = t.lines().filter(|l| l.starts_with("src/")).collect();
    assert_eq!(files.first().copied(), Some("src/lib.rs: L1"), "{t}");
}

// ---------------------------------------------------------------------------
// find_definitions heuristics (4-4)
// ---------------------------------------------------------------------------

#[test]
fn test_mcp_find_definitions_languages() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "x.go",
        "package x\n\nfunc (s *Server) HandleLogin(w int) {\n}\n",
    );
    write(
        root,
        "x.c",
        "static int compute(int a) {\n  return a;\n}\n#define MAXV 10\nint y = compute(1);\n",
    );
    write(
        root,
        "x.ts",
        "export class Foo {\n  async handleLogin(req: string): Promise<void> {\n  }\n}\nhandleLogin(1);\n",
    );
    write(
        root,
        "x.rs",
        "impl<T> Display for Wrapper<T> {}\nmacro_rules! my_macro { () => {} }\nfn main() {\n    let compute = 3;\n}\n",
    );
    write(
        root,
        "x.py",
        "class Server:\n    def handle(self):\n        pass\n",
    );
    write(root, "doc.md", "```\nfn compute() {}\n```\n");

    let out = call_tools(
        root,
        &[
            (
                "find_definitions",
                serde_json::json!({"symbol": "HandleLogin"}),
            ),
            (
                "find_definitions",
                serde_json::json!({"symbol": "compute", "include_seen": true}),
            ),
            (
                "find_definitions",
                serde_json::json!({"symbol": "MAXV", "include_seen": true}),
            ),
            (
                "find_definitions",
                serde_json::json!({"symbol": "handleLogin", "include_seen": true}),
            ),
            (
                "find_definitions",
                serde_json::json!({"symbol": "Wrapper", "include_seen": true}),
            ),
            (
                "find_definitions",
                serde_json::json!({"symbol": "my_macro", "include_seen": true}),
            ),
            (
                "find_definitions",
                serde_json::json!({"symbol": "Server.handle", "include_seen": true}),
            ),
            (
                "find_definitions",
                serde_json::json!({"symbol": "compute", "max_tokens": 60, "context_lines": 40, "include_seen": true}),
            ),
        ],
    );
    assert!(out[0].0.contains("x.go:3"), "{}", out[0].0);
    let (t, _) = &out[1];
    assert!(
        t.contains("Found 1 definition") && t.contains("x.c:1"),
        "locals/docs/calls must be excluded: {t}"
    );
    assert!(out[2].0.contains("x.c:4"), "{}", out[2].0);
    let (t, _) = &out[3];
    assert!(
        t.contains("Found 1 definition") && t.contains("x.ts:2"),
        "{t}"
    );
    assert!(out[4].0.contains("x.rs:1"), "{}", out[4].0);
    assert!(out[5].0.contains("Found 1 definition"), "{}", out[5].0);
    let (t, _) = &out[6];
    assert!(
        t.contains("read as 'handle'") && t.contains("x.py:2"),
        "{t}"
    );
    let (t, e) = &out[7];
    assert!(!e && t.len() < 700, "max_tokens ignored ({}): {t}", t.len());
}

// ---------------------------------------------------------------------------
// Multi-query and locations mode (F5)
// ---------------------------------------------------------------------------

#[test]
fn test_mcp_multi_query_and_locations() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "a.rs",
        "fn alpha() {}\nfn beta() {}\nfn alpha_two() {}\n",
    );
    write(root, "b.rs", "fn gamma() { alpha(); }\n");

    let out = call_tools(
        root,
        &[
            (
                "search",
                serde_json::json!({
                    "queries": [{"pattern": "alpha"}, "beta", {"pattern": "gamma", "output_mode": "files", "mode": "files"}],
                    "mode": "locations"
                }),
            ),
            (
                "search",
                serde_json::json!({"pattern": "x", "queries": ["y"]}),
            ),
            (
                "search",
                serde_json::json!({"queries": [{"pattern": "(", "regex": true}]}),
            ),
        ],
    );
    let (t, e) = &out[0];
    assert!(!e, "{t}");
    assert!(t.contains("3 queries"), "{t}");
    assert!(
        t.contains("### Query 1: `alpha`") && t.contains("a.rs: L1,L3") && t.contains("b.rs: L1"),
        "{t}"
    );
    assert!(
        t.contains("### Query 2: `beta`") && t.contains("a.rs: L2"),
        "{t}"
    );
    assert!(
        t.contains("### Query 3: `gamma`") && t.contains("b.rs (1 match"),
        "{t}"
    );
    assert!(out[1].1 && out[1].0.contains("not both"), "{:?}", out[1]);
    assert!(out[2].1 && out[2].0.contains("queries[0]"), "{:?}", out[2]);
}

// ---------------------------------------------------------------------------
// Session dedup (F6)
// ---------------------------------------------------------------------------

#[test]
fn test_mcp_session_dedup_and_reset() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "a.rs",
        "fn target() {}\n\n\n\n\n\n\n\n\n\nfn other() { target(); }\n",
    );

    let out = call_tools(
        root,
        &[
            ("search", serde_json::json!({"pattern": "target"})),
            ("search", serde_json::json!({"pattern": "target"})),
            (
                "search",
                serde_json::json!({"pattern": "target", "include_seen": true}),
            ),
            ("session_reset", serde_json::json!({})),
            ("search", serde_json::json!({"pattern": "target"})),
            ("session_reset", serde_json::json!({})),
            (
                "read_file",
                serde_json::json!({"path": "a.rs", "start_line": 1, "end_line": 5}),
            ),
            ("find_definitions", serde_json::json!({"symbol": "target"})),
        ],
    );
    assert!(out[0].0.contains("fn target() {}"), "{}", out[0].0);
    let (t, _) = &out[1];
    assert!(t.contains("already shown: a.rs:L1,L11 (2 matches)"), "{t}");
    assert!(!t.contains("fn target() {}"), "{t}");
    assert!(
        out[2].0.contains("fn target() {}"),
        "include_seen: {}",
        out[2].0
    );
    assert!(out[3].0.contains("Session reset"), "{}", out[3].0);
    assert!(
        out[4].0.contains("fn target() {}"),
        "after reset: {}",
        out[4].0
    );
    // read_file ranges count as shown
    let (t, _) = &out[7];
    assert!(t.contains("already shown: a.rs:L1-5"), "{t}");
}
