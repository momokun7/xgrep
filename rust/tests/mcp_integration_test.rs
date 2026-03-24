// rust/tests/mcp_integration_test.rs
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};
use tempfile::tempdir;

fn send_mcp_messages(root: &std::path::Path, messages: &[&str]) -> Vec<serde_json::Value> {
    let binary = env!("CARGO_BIN_EXE_xg");
    let mut child = Command::new(binary)
        .args(["serve", "--root", &root.to_string_lossy()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("Failed to start xg serve");

    let stdin = child.stdin.as_mut().unwrap();
    for msg in messages {
        writeln!(stdin, "{}", msg).unwrap();
    }
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("Failed to read output");
    let stdout = String::from_utf8_lossy(&output.stdout);

    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
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
    assert_eq!(tools.len(), 5);

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

    // index_status
    assert_eq!(responses[2]["id"], 3);
    let status_text = responses[2]["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(status_text.contains("Index path:"));
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
