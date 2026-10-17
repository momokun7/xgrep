// MCP integration tests for the syntax-aware tools: outline,
// find_definitions (tree-sitter backend), find_references, search
// expand:"block" and repo_map. Uses a small multi-language fixture repo.
#![cfg(feature = "syntax")]

use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

const INIT: &str = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#;

fn send(root: &std::path::Path, messages: &[String]) -> Vec<serde_json::Value> {
    let binary = env!("CARGO_BIN_EXE_xg");
    let cache = root.join(".test-cache");
    let mut child = Command::new(binary)
        .args(["serve", "--root", &root.to_string_lossy()])
        .env("XDG_CACHE_HOME", &cache)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start xg serve");
    let stdin = child.stdin.as_mut().unwrap();
    for m in messages {
        writeln!(stdin, "{}", m).unwrap();
    }
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

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
    let responses = send(root, &msgs);
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

fn call(root: &std::path::Path, name: &str, args: serde_json::Value) -> (String, bool) {
    call_tools(root, &[(name, args)]).remove(0)
}

fn write(root: &std::path::Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path();
    write(
        r,
        "src/server.rs",
        r#"pub struct Server {
    name: String,
}

impl Server {
    pub fn handle_login(&self, token: &str) -> bool {
        let ok = validate_token(token);
        ok && validate_token("again")
    }
}

/// Checks a token.
pub fn validate_token(token: &str) -> bool {
    !token.is_empty()
}

const HELP: &str = "
fn validate_token() is documented here, but this is a string
";
"#,
    );
    write(
        r,
        "src/lib.rs",
        "pub mod server;\nuse crate::server::validate_token;\n\npub fn boot() -> bool {\n    validate_token(\"x\")\n}\n",
    );
    write(
        r,
        "tests/server_test.rs",
        "#[test]\nfn token_ok() {\n    assert!(xg_fixture::server::validate_token(\"y\"));\n}\n",
    );
    write(
        r,
        "docs/guide.md",
        "# Guide\n\n## Auth\n\nCall validate_token before anything else.\n",
    );
    write(
        r,
        "app/auth.py",
        "class AuthService:\n    def login(self, user, pw):\n        if not check_password(pw):\n            return False\n        return check_password(pw + user)\n\n\ndef check_password(pw):\n    return len(pw) > 3\n",
    );
    write(
        r,
        "web/api.ts",
        "export interface User { id: string }\n\nexport class Api {\n  fetchUser(id: string): User {\n    return request(id);\n  }\n}\n\nexport function request(id: string): User {\n  return { id };\n}\n",
    );
    write(
        r,
        "go/server.go",
        "package srv\n\ntype Server struct{}\n\nfunc (s *Server) HandleLogin() bool {\n\treturn verify()\n}\n\nfunc verify() bool { return true }\n",
    );
    write(
        r,
        "java/Svc.java",
        "public class Svc {\n  public Svc() {}\n  public int compute(int a) {\n    return helper(a);\n  }\n  private int helper(int a) { return a; }\n}\n",
    );
    write(
        r,
        "c/util.c",
        "#define LIMIT 3\nint add(int a, int b);\nint add(int a, int b) {\n  return a + b;\n}\n",
    );
    write(
        r,
        "rb/thing.rb",
        "module Things\n  class Widget\n    def spin(n)\n      n * 2\n    end\n  end\nend\n",
    );
    dir
}

#[test]
fn tools_are_listed() {
    let dir = fixture();
    let responses = send(
        dir.path(),
        &[
            INIT.to_string(),
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#.to_string(),
        ],
    );
    let tools = responses[1]["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for n in ["outline", "find_references", "repo_map"] {
        assert!(names.contains(&n), "missing {n}: {names:?}");
    }
    let search = tools.iter().find(|t| t["name"] == "search").unwrap();
    assert!(search["inputSchema"]["properties"]["expand"].is_object());
}

#[test]
fn outline_files_dirs_and_errors() {
    let dir = fixture();
    let r = call_tools(
        dir.path(),
        &[
            ("outline", serde_json::json!({"path": "src/server.rs"})),
            ("outline", serde_json::json!({"path": "app/auth.py"})),
            (
                "outline",
                serde_json::json!({"path": ["web/api.ts", "go/server.go"]}),
            ),
            ("outline", serde_json::json!({"path": "java/Svc.java"})),
            ("outline", serde_json::json!({"path": "c/util.c"})),
            ("outline", serde_json::json!({"path": "rb/thing.rb"})),
            ("outline", serde_json::json!({"path": "docs/guide.md"})),
            ("outline", serde_json::json!({"path": "src"})),
            ("outline", serde_json::json!({"path": "nope.rs"})),
            ("outline", serde_json::json!({"path": "../x.rs"})),
        ],
    );
    let (rs, err) = &r[0];
    assert!(!err, "{rs}");
    assert!(rs.starts_with("## src/server.rs (rust, "), "{rs}");
    assert!(rs.contains("L1-3 pub struct Server"), "{rs}");
    assert!(rs.contains("L5-10 impl Server"), "{rs}");
    assert!(
        rs.contains("\n  L6-9 pub fn handle_login(&self, token: &str) -> bool\n"),
        "{rs}"
    );
    assert!(
        rs.contains("L13-15 pub fn validate_token(token: &str) -> bool"),
        "{rs}"
    );
    assert!(
        !rs.contains("!token.is_empty()"),
        "bodies must not be shown: {rs}"
    );

    let py = &r[1].0;
    assert!(py.contains("L1-5 class AuthService"), "{py}");
    assert!(py.contains("\n  L2-5 def login(self, user, pw)\n"), "{py}");
    assert!(py.contains("L8-9 def check_password(pw)"), "{py}");

    let multi = &r[2].0;
    assert!(multi.contains("## web/api.ts (typescript"), "{multi}");
    assert!(multi.contains("export class Api"), "{multi}");
    assert!(multi.contains("fetchUser(id: string): User"), "{multi}");
    assert!(multi.contains("## go/server.go (go"), "{multi}");
    assert!(
        multi.contains("func (s *Server) HandleLogin() bool"),
        "{multi}"
    );

    assert!(r[3].0.contains("public int compute(int a)"), "{}", r[3].0);
    assert!(r[3].0.contains("  L2 public Svc()"), "{}", r[3].0);
    assert!(
        r[4].0.contains("macro LIMIT") || r[4].0.contains("#define LIMIT"),
        "{}",
        r[4].0
    );
    assert!(r[4].0.contains("L3-5 int add(int a, int b)"), "{}", r[4].0);
    assert!(r[5].0.contains("module Things"), "{}", r[5].0);
    assert!(r[5].0.contains("    L3-5 def spin(n)"), "{}", r[5].0);

    let md = &r[6].0;
    assert!(md.contains("L1-5 # Guide"), "{md}");
    assert!(md.contains("  L3-5 ## Auth"), "{md}");

    // directory: every supported file, top-level only by default
    let d = &r[7].0;
    assert!(d.contains("## src/lib.rs"), "{d}");
    assert!(d.contains("## src/server.rs"), "{d}");
    assert!(!d.contains("handle_login"), "{d}");
    assert!(d.contains("nested deeper than max_depth"), "{d}");

    assert!(r[8].1, "missing file is an error: {}", r[8].0);
    assert!(r[9].1, "path outside root is an error: {}", r[9].0);
}

#[test]
fn outline_unsupported_and_budget() {
    let dir = fixture();
    write(dir.path(), "data/x.unknownext", "hello\n");
    let mut big = String::new();
    for i in 0..400 {
        big.push_str(&format!(
            "pub fn function_number_{i}(argument: u32) -> u32 {{ argument }}\n"
        ));
    }
    write(dir.path(), "src/big.rs", &big);
    let r = call_tools(
        dir.path(),
        &[
            ("outline", serde_json::json!({"path": "data/x.unknownext"})),
            (
                "outline",
                serde_json::json!({"path": "src/big.rs", "max_tokens": 300}),
            ),
        ],
    );
    assert!(r[0].1);
    assert!(r[0].0.contains("Supported:"), "{}", r[0].0);
    assert!(!r[1].1);
    assert!(
        r[1].0.contains("outline cut at max_tokens=300"),
        "{}",
        r[1].0
    );
    assert!(r[1].0.len() < 300 * 5, "{}", r[1].0.len());
}

#[test]
fn find_definitions_uses_tree_sitter() {
    let dir = fixture();
    let r = call_tools(
        dir.path(),
        &[
            (
                "find_definitions",
                serde_json::json!({"symbol": "validate_token"}),
            ),
            (
                "find_definitions",
                serde_json::json!({"symbol": "Api.fetchUser"}),
            ),
            ("find_definitions", serde_json::json!({"symbol": "helper"})),
            ("find_definitions", serde_json::json!({"symbol": "Guide"})),
        ],
    );
    let (t, err) = &r[0];
    assert!(!err, "{t}");
    // The fake `fn validate_token()` line inside a string literal is not a
    // definition (the regex heuristic would have reported it).
    assert!(t.contains("Found 1 definition of `validate_token`"), "{t}");
    assert!(t.contains("tree-sitter backend"), "{t}");
    assert!(t.contains("src/server.rs:13"), "{t}");
    assert!(r[1].0.contains("web/api.ts:4"), "{}", r[1].0);
    assert!(r[2].0.contains("java/Svc.java:6"), "{}", r[2].0);
    assert!(r[3].0.contains("No definitions"), "{}", r[3].0);
}

#[test]
fn find_references_excludes_defs_imports_and_names_callers() {
    let dir = fixture();
    let (t, err) = call(
        dir.path(),
        "find_references",
        serde_json::json!({"symbol": "validate_token"}),
    );
    assert!(!err, "{t}");
    assert!(
        t.contains("excluded 1 definition line and 1 import line"),
        "{t}"
    );
    assert!(
        t.contains("in Server.handle_login: let ok = validate_token(token);"),
        "{t}"
    );
    assert!(t.contains("in boot: validate_token(\"x\")"), "{t}");
    assert!(t.contains("in token_ok:"), "{t}");
    assert!(t.contains("Callers ("), "{t}");
    assert!(!t.contains("use crate::server::validate_token"), "{t}");
    // ranking: src before tests before docs
    let src = t.find("src/server.rs").unwrap();
    let tests = t.find("tests/server_test.rs").unwrap();
    let docs = t.find("docs/guide.md").unwrap();
    assert!(src < tests && tests < docs, "{t}");
    // Markdown mentions are annotated with their section.
    assert!(t.contains("in heading Auth"), "{t}");

    let (t2, _) = call(
        dir.path(),
        "find_references",
        serde_json::json!({"symbol": "validate_token", "include_imports": true, "include_definitions": true, "max_results": 2}),
    );
    assert!(t2.contains("next_offset=2"), "{t2}");

    let (py, _) = call(
        dir.path(),
        "find_references",
        serde_json::json!({"symbol": "check_password"}),
    );
    assert!(py.contains("Found 2 references"), "{py}");
    assert!(py.contains("in AuthService.login"), "{py}");
}

#[test]
fn search_expand_block_groups_matches() {
    let dir = fixture();
    let r = call_tools(
        dir.path(),
        &[
            (
                "search",
                serde_json::json!({"pattern": "check_password", "expand": "block"}),
            ),
            (
                "search",
                serde_json::json!({"pattern": "check_password", "expand": "block"}),
            ),
            (
                "search",
                serde_json::json!({"pattern": "x", "expand": "sideways"}),
            ),
            (
                "search",
                serde_json::json!({"pattern": "x", "expand": "block", "output_mode": "files"}),
            ),
            (
                "search",
                serde_json::json!({"pattern": "validate_token", "expand": "block", "max_results": 1}),
            ),
        ],
    );
    let (t, err) = &r[0];
    assert!(!err, "{t}");
    assert!(t.contains("grouped into 2 blocks"), "{t}");
    assert!(
        t.contains("## app/auth.py:2-5 method AuthService.login (2 matches)"),
        "{t}"
    );
    assert!(t.contains("3>         if not check_password(pw):"), "{t}");
    assert!(t.contains("2      def login(self, user, pw):"), "{t}");
    assert!(
        t.contains("## app/auth.py:8-9 fn check_password (1 match)"),
        "{t}"
    );
    // second call: the blocks were already shown in this session
    assert!(
        r[1].0.contains("already shown: app/auth.py:L2-5"),
        "{}",
        r[1].0
    );
    assert!(r[2].1 && r[2].0.contains("Unknown expand"), "{}", r[2].0);
    assert!(r[3].1, "{}", r[3].0);
    assert!(r[4].0.contains("Showing blocks 1-1 of"), "{}", r[4].0);
    assert!(r[4].0.contains("next_offset=1"), "{}", r[4].0);
}

#[test]
fn repo_map_ranks_and_respects_budget() {
    let dir = fixture();
    let r = call_tools(
        dir.path(),
        &[
            ("repo_map", serde_json::json!({})),
            ("repo_map", serde_json::json!({"max_tokens": 120})),
            ("repo_map", serde_json::json!({"focus": "app/auth.py"})),
            ("repo_map", serde_json::json!({"focus": "nope/"})),
            ("repo_map", serde_json::json!({"bogus": 1})),
        ],
    );
    let (t, err) = &r[0];
    assert!(!err, "{t}");
    assert!(t.starts_with("Repo map: "), "{t}");
    assert!(t.contains("src/server.rs:\n"), "{t}");
    assert!(
        t.contains("L13 pub fn validate_token(token: &str) -> bool"),
        "{t}"
    );
    // validate_token is the most referenced symbol
    let first_file = t
        .lines()
        .find(|l| l.ends_with(':') && !l.contains(' '))
        .unwrap();
    assert_eq!(first_file, "src/server.rs:", "{t}");
    assert!(!t.contains("docs/guide.md"), "{t}");

    let small = &r[1].0;
    let body: String = small.lines().skip(3).collect::<Vec<_>>().join("\n");
    assert!(body.len() / 4 <= 130, "{small}");

    assert!(r[2].0.contains("personalized to app/auth.py"), "{}", r[2].0);
    assert!(r[2].0.contains("check_password"), "{}", r[2].0);
    assert!(
        r[3].0.contains("matches no supported source file"),
        "{}",
        r[3].0
    );
    // Unknown parameters are ignored with a note.
    assert!(!r[4].1, "{}", r[4].0);
    assert!(
        r[4].0
            .starts_with("note: ignored unknown argument(s): bogus\nRepo map: "),
        "{}",
        r[4].0
    );
}

#[test]
fn cli_outline_and_expand_block() {
    let dir = fixture();
    let bin = env!("CARGO_BIN_EXE_xg");
    let out = Command::new(bin)
        .args(["--outline", "src/server.rs"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("L5-10 impl Server"), "{s}");

    let out = Command::new(bin)
        .args(["check_password", "--expand", "block"])
        .current_dir(dir.path())
        .env("XDG_CACHE_HOME", dir.path().join(".test-cache"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("## app/auth.py:2-5 method AuthService.login (2 matches)"),
        "{s}"
    );

    let out = Command::new(bin)
        .args(["--outline", "data.unknownext"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn tags_cache_is_written_next_to_index() {
    let dir = fixture();
    let (_, err) = call(dir.path(), "repo_map", serde_json::json!({}));
    assert!(!err);
    let cache = dir.path().join(".test-cache");
    let found = walk(&cache)
        .into_iter()
        .any(|p| p.to_string_lossy().ends_with(".tags"));
    assert!(found, "no .tags sidecar under {}", cache.display());
}

fn walk(p: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(p) {
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}
