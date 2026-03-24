use serde_json::Value;
use std::io::{self, BufRead, Write};

use crate::mcp_tools;
use crate::Xgrep;

/// JSON-RPC request/notification.
pub struct Message {
    pub id: Option<Value>,
    pub method: String,
    pub params: Value,
}

pub fn parse_message(line: &str) -> Result<Message, String> {
    let v: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;

    // Validate jsonrpc version
    match v.get("jsonrpc").and_then(|v| v.as_str()) {
        Some("2.0") => {}
        _ => return Err("invalid or missing jsonrpc version (expected \"2.0\")".to_string()),
    }

    let method = v
        .get("method")
        .and_then(|m| m.as_str())
        .ok_or("missing method")?
        .to_string();
    let id = v.get("id").cloned();
    let params = v
        .get("params")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    Ok(Message { id, method, params })
}

pub fn success_response(id: Value, result: Value) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
}

pub fn error_response(id: Value, code: i32, message: &str) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message
        }
    })
}

pub fn tool_result(text: &str, is_error: bool) -> Value {
    serde_json::json!({
        "content": [{"type": "text", "text": text}],
        "isError": is_error
    })
}

/// Start the MCP server (stdio transport).
pub fn start(xg: Xgrep) {
    run_server(|msg| handle_message(&xg, msg));
}

/// Dispatch a message and return the response.
pub fn handle_message(xg: &Xgrep, msg: &Message) -> Option<Value> {
    let id = match &msg.id {
        Some(id) => id.clone(),
        None => return None, // notification
    };

    match msg.method.as_str() {
        "initialize" => {
            let result = serde_json::json!({
                "protocolVersion": "2025-03-26",
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "xgrep",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "instructions": "xgrep is an ultra-fast indexed code search engine. Use 'search' to find patterns, 'find_definitions' to locate symbol definitions, 'read_file' to view full file contents, 'index_status' to check index health, and 'build_index' to rebuild the index."
            });
            Some(success_response(id, result))
        }
        "tools/list" => {
            let tools = mcp_tools::tools_list();
            let result = serde_json::json!({ "tools": tools });
            Some(success_response(id, result))
        }
        "tools/call" => {
            let tool_name = msg
                .params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let arguments = msg
                .params
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::json!({}));

            let (text, is_error) = match tool_name {
                "search" => mcp_tools::handle_search(xg, &arguments),
                "find_definitions" => mcp_tools::handle_find_definitions(xg, &arguments),
                "build_index" => mcp_tools::handle_build_index(xg),
                "index_status" => mcp_tools::handle_index_status(xg),
                "read_file" => mcp_tools::handle_read_file(xg, &arguments),
                _ => {
                    return Some(error_response(
                        id,
                        -32602,
                        &format!("Unknown tool: {}", tool_name),
                    ));
                }
            };

            Some(success_response(id, tool_result(&text, is_error)))
        }
        _ => Some(error_response(id, -32601, "Method not found")),
    }
}

/// MCP server main loop. Reads lines from stdin, dispatches to handler, writes responses to stdout.
fn run_server(handler: impl Fn(&Message) -> Option<Value>) {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }

        let msg = match parse_message(&line) {
            Ok(m) => m,
            Err(e) => {
                let resp = error_response(Value::Null, -32700, &format!("Parse error: {}", e));
                let _ = writeln!(out, "{}", resp);
                let _ = out.flush();
                continue;
            }
        };

        // 通知（idなし）はレスポンスを返さない
        if msg.id.is_none() {
            handler(&msg);
            continue;
        }

        if let Some(resp) = handler(&msg) {
            let _ = writeln!(out, "{}", resp);
            let _ = out.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_parse_request() {
        let json = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let msg = parse_message(json).unwrap();
        assert_eq!(msg.method, "initialize");
        assert_eq!(msg.id, Some(serde_json::json!(1)));
    }

    #[test]
    fn test_parse_notification() {
        let json = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let msg = parse_message(json).unwrap();
        assert!(msg.id.is_none());
    }

    #[test]
    fn test_success_response() {
        let resp = success_response(serde_json::json!(1), serde_json::json!({"key": "value"}));
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains("\"result\""));
        assert!(!s.contains("\"error\""));
    }

    #[test]
    fn test_error_response() {
        let resp = error_response(serde_json::json!(1), -32601, "Method not found");
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains("\"error\""));
        assert!(s.contains("-32601"));
    }

    #[test]
    fn test_tool_result() {
        let resp = tool_result("hello world", false);
        assert_eq!(resp["content"][0]["text"], "hello world");
        assert_eq!(resp["isError"], false);
    }

    #[test]
    fn test_tool_result_error() {
        let resp = tool_result("something failed", true);
        assert_eq!(resp["isError"], true);
    }

    #[test]
    fn test_parse_invalid_json() {
        let result = parse_message("not json");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_missing_method() {
        let result = parse_message(r#"{"jsonrpc":"2.0","id":1}"#);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_missing_jsonrpc() {
        let result = parse_message(r#"{"method":"test","id":1}"#);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_id_null_is_request_not_notification() {
        // JSON-RPC 2.0: "id": null はリクエスト（通知ではない）
        let json = r#"{"jsonrpc":"2.0","id":null,"method":"test"}"#;
        let msg = parse_message(json).unwrap();
        assert_eq!(msg.id, Some(Value::Null)); // None ではない
        assert!(!msg.id.is_none()); // 通知ではなくリクエストとして扱う
    }

    #[test]
    fn test_parse_notification_has_no_id_field() {
        // JSON-RPC 2.0: id フィールドが存在しない = 通知
        let json = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let msg = parse_message(json).unwrap();
        assert!(msg.id.is_none()); // 通知
    }

    fn setup_test_repo() -> (tempfile::TempDir, Xgrep) {
        let dir = tempdir().unwrap();
        let root = dir.path();

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
            "fn hello() {\n    println!(\"hello\");\n}\n",
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
    fn test_initialize() {
        let (_dir, xg) = setup_test_repo();
        let msg = parse_message(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
        )
        .unwrap();

        let resp = handle_message(&xg, &msg).unwrap();
        assert_eq!(resp["result"]["protocolVersion"], "2025-03-26");
        assert!(resp["result"]["capabilities"]["tools"].is_object());
        assert_eq!(resp["result"]["serverInfo"]["name"], "xgrep");
        assert!(resp["result"]["instructions"].as_str().is_some());
    }

    #[test]
    fn test_tools_list() {
        let (_dir, xg) = setup_test_repo();
        let msg =
            parse_message(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#).unwrap();

        let resp = handle_message(&xg, &msg).unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), crate::mcp_tools::tools_list().len());
    }

    #[test]
    fn test_tools_call_search() {
        let (_dir, xg) = setup_test_repo();
        let msg = parse_message(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search","arguments":{"pattern":"hello"}}}"#,
        )
        .unwrap();

        let resp = handle_message(&xg, &msg).unwrap();
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("Found"));
        assert!(text.contains("hello"));
        assert_eq!(resp["result"]["isError"], false);
    }

    #[test]
    fn test_tools_call_unknown() {
        let (_dir, xg) = setup_test_repo();
        let msg = parse_message(
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"nonexistent","arguments":{}}}"#,
        )
        .unwrap();

        let resp = handle_message(&xg, &msg).unwrap();
        assert_eq!(resp["error"]["code"], -32602);
        assert!(resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Unknown tool"));
    }

    #[test]
    fn test_notification_returns_none() {
        let (_dir, xg) = setup_test_repo();
        let msg =
            parse_message(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).unwrap();

        let resp = handle_message(&xg, &msg);
        assert!(resp.is_none());
    }

    #[test]
    fn test_unknown_method() {
        let (_dir, xg) = setup_test_repo();
        let msg =
            parse_message(r#"{"jsonrpc":"2.0","id":5,"method":"unknown/method","params":{}}"#)
                .unwrap();

        let resp = handle_message(&xg, &msg).unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }
}
