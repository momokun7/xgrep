use serde_json::Value;
use std::io::{self, BufRead, Write};

/// JSON-RPCリクエスト/通知
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

/// MCPサーバーのメインループ。stdinから1行ずつ読み、ハンドラに渡してstdoutに返す。
pub fn run_server(handler: impl Fn(&Message) -> Option<Value>) {
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
}
