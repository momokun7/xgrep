// rust/tests/watch_serve_test.rs
//
// `xg serve` watches the root: edits are visible to the next search, and a
// long session leaks neither threads nor zombie processes.
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

struct Serve {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Serve {
    fn start(root: &std::path::Path, cache: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_xg"))
            .args(["serve", "--root", &root.to_string_lossy()])
            .env("XDG_CACHE_HOME", cache)
            .env_remove("XGREP_NO_WATCH")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to start xg serve");
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut s = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        s.request(
            "initialize",
            serde_json::json!({"protocolVersion":"2025-03-26","capabilities":{},
                "clientInfo":{"name":"test","version":"1.0"}}),
        );
        s
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        let msg = serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).expect("json response");
        assert_eq!(v["id"], id);
        v
    }

    fn search(&mut self, pattern: &str) -> String {
        let v = self.request(
            "tools/call",
            serde_json::json!({"name":"search","arguments":{"pattern":pattern,"include_seen":true}}),
        );
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    /// Close stdin and wait (bounded) for a clean exit.
    fn shutdown(mut self) -> std::process::ExitStatus {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                return st;
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("xg serve did not exit after stdin closed");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[cfg(target_os = "linux")]
fn thread_count(pid: u32) -> usize {
    fs::read_dir(format!("/proc/{pid}/task")).unwrap().count()
}

/// Child processes of `pid` that are zombies (exited but not reaped).
#[cfg(target_os = "linux")]
fn zombie_children(pid: u32) -> Vec<String> {
    let mut out = Vec::new();
    for task in fs::read_dir(format!("/proc/{pid}/task")).unwrap().flatten() {
        let children = fs::read_to_string(task.path().join("children")).unwrap_or_default();
        for c in children.split_whitespace() {
            let stat = fs::read_to_string(format!("/proc/{c}/stat")).unwrap_or_default();
            // Field 3 (after the parenthesised command name) is the state.
            let state = stat
                .rsplit(')')
                .next()
                .unwrap_or("")
                .split_whitespace()
                .next();
            if state == Some("Z") {
                out.push(c.to_string());
            }
        }
    }
    out
}

#[test]
fn test_serve_sees_edits_and_does_not_leak() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("proj");
    let cache = dir.path().join("cache");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("main.rs"), "fn main_original() {}\n").unwrap();

    let mut serve = Serve::start(&root, &cache);
    let first = serve.search("main_original");
    assert!(first.contains("main.rs"), "{first}");

    #[cfg(target_os = "linux")]
    let pid = serve.child.id();
    #[cfg(target_os = "linux")]
    let threads_after_first = thread_count(pid);

    for i in 0..25 {
        // New file, in-place edit, and a deletion between searches.
        let sym = format!("fresh_symbol_{i}_xyz");
        fs::write(root.join(format!("f{i}.rs")), format!("fn {sym}() {{}}\n")).unwrap();
        fs::write(root.join("main.rs"), format!("fn main_v{i}() {{}}\n")).unwrap();
        if i > 0 {
            fs::remove_file(root.join(format!("f{}.rs", i - 1))).unwrap();
        }

        let text = serve.search(&sym);
        assert!(text.contains(&format!("f{i}.rs")), "iteration {i}: {text}");
        let text = serve.search(&format!("main_v{i}"));
        assert!(text.contains("main.rs"), "iteration {i}: {text}");
        if i > 0 {
            let gone = serve.search(&format!("fresh_symbol_{}_xyz", i - 1));
            assert!(!gone.contains(&format!("f{}.rs", i - 1)), "{gone}");
        }
        if i % 8 == 0 {
            // Let the watcher's quiet-period rebuild run now and then.
            std::thread::sleep(Duration::from_millis(2500));
        }

        #[cfg(target_os = "linux")]
        {
            let z = zombie_children(pid);
            assert!(z.is_empty(), "zombie children after iteration {i}: {z:?}");
        }
    }

    #[cfg(target_os = "linux")]
    {
        // A one-off rebuild thread may be running, but nothing accumulates.
        let now = thread_count(pid);
        assert!(
            now <= threads_after_first + 2,
            "threads grew from {threads_after_first} to {now}"
        );
    }

    let status = serve.shutdown();
    assert!(status.success(), "{status:?}");
}
