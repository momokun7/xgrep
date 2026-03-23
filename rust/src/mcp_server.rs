use serde_json::Value;

use crate::mcp::{self, Message};
use crate::mcp_tools;
use crate::Xgrep;

/// MCPサーバーを起動する（stdio transport）
pub fn start(xg: Xgrep) {
    mcp::run_server(|msg| handle_message(&xg, msg));
}

/// メッセージをディスパッチしてレスポンスを返す
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
                "instructions": "xgrep is an ultra-fast indexed code search engine. Use 'search' to find patterns, 'find_definitions' to locate symbol definitions, 'index_status' to check index health, and 'build_index' to rebuild the index."
            });
            Some(mcp::success_response(id, result))
        }
        "tools/list" => {
            let tools = mcp_tools::tools_list();
            let result = serde_json::json!({ "tools": tools });
            Some(mcp::success_response(id, result))
        }
        "tools/call" => {
            let tool_name = msg.params.get("name").and_then(|v| v.as_str()).unwrap_or("");
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
                _ => {
                    return Some(mcp::error_response(
                        id,
                        -32602,
                        &format!("Unknown tool: {}", tool_name),
                    ));
                }
            };

            Some(mcp::success_response(id, mcp::tool_result(&text, is_error)))
        }
        _ => Some(mcp::error_response(id, -32601, "Method not found")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::parse_message;
    use std::fs;
    use tempfile::tempdir;

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
        assert_eq!(tools.len(), 4);
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
        let msg = parse_message(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        )
        .unwrap();

        let resp = handle_message(&xg, &msg);
        assert!(resp.is_none());
    }

    #[test]
    fn test_unknown_method() {
        let (_dir, xg) = setup_test_repo();
        let msg = parse_message(
            r#"{"jsonrpc":"2.0","id":5,"method":"unknown/method","params":{}}"#,
        )
        .unwrap();

        let resp = handle_message(&xg, &msg).unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }
}
