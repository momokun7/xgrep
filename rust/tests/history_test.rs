//! Git history search (`--in-diff`, `--log`) through the library API and
//! the `xg` CLI.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use xgrep_search::{DiffSide, SearchOptions, Xgrep};

/// Run `xg` in `dir` with its index cache kept inside `cache`.
fn xg(dir: &Path, cache: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xg"))
        .args(args)
        .current_dir(dir)
        .env("XDG_CACHE_HOME", cache)
        .env("XGREP_NO_HINTS", "1")
        .output()
        .expect("failed to run xg")
}

fn stdout(o: &Output) -> String {
    String::from_utf8(o.stdout.clone()).expect("stdout must be UTF-8")
}

fn git(dir: &Path, args: &[&str]) {
    let o = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env("GIT_AUTHOR_DATE", "2026-01-02T03:04:05+00:00")
        .env("GIT_COMMITTER_DATE", "2026-01-02T03:04:05+00:00")
        .output()
        .unwrap();
    assert!(o.status.success(), "git {:?}: {:?}", args, o);
}

/// A git repo with two commits under `root/sub`, plus an uncommitted edit.
fn history_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Tester"]);
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(
        root.join("sub/a.rs"),
        "fn keep() {}\nfn old_needle() {}\nfn other() {}\n",
    )
    .unwrap();
    fs::write(root.join("top.rs"), "needle at top\n").unwrap();
    fs::write(root.join("sub/gone.md"), "needle in doc\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "init: first version"]);
    fs::write(
        root.join("sub/a.rs"),
        "fn keep() {}\nfn other() {}\nfn new_needle() {}\n",
    )
    .unwrap();
    fs::write(root.join("top.rs"), "needle at top changed\n").unwrap();
    fs::remove_file(root.join("sub/gone.md")).unwrap();
    git(root, &["add", "-A"]);
    git(
        root,
        &[
            "commit",
            "-q",
            "-m",
            "refactor: rename things\n\nThe Needle function moved.",
        ],
    );
    fs::write(
        root.join("sub/a.rs"),
        "fn keep() {}\nfn other() {}\nfn new_needle() {}\nfn wip_needle() {}\n",
    )
    .unwrap();
    dir
}

#[test]
fn test_search_diff_library() {
    let dir = history_repo();
    let xg = Xgrep::open_local(dir.path()).unwrap();
    let got: Vec<(String, DiffSide, usize, String)> = xg
        .search_diff("HEAD~1..HEAD", "needle", &SearchOptions::new())
        .unwrap()
        .into_iter()
        .map(|d| (d.file.to_string(), d.side, d.line_number, d.line))
        .collect();
    assert_eq!(
        got,
        vec![
            (
                "sub/a.rs".into(),
                DiffSide::Removed,
                2,
                "fn old_needle() {}".into()
            ),
            (
                "sub/a.rs".into(),
                DiffSide::Added,
                3,
                "fn new_needle() {}".into()
            ),
            (
                "sub/gone.md".into(),
                DiffSide::Removed,
                1,
                "needle in doc".into()
            ),
            (
                "top.rs".into(),
                DiffSide::Removed,
                1,
                "needle at top".into()
            ),
            (
                "top.rs".into(),
                DiffSide::Added,
                1,
                "needle at top changed".into()
            ),
        ]
    );

    // Working tree against HEAD, restricted to a subdirectory root.
    let sub = Xgrep::open_local(dir.path().join("sub")).unwrap();
    let wip = sub
        .search_diff("HEAD", "needle", &SearchOptions::new())
        .unwrap();
    assert_eq!(wip.len(), 1);
    assert_eq!(&*wip[0].file, "a.rs");
    assert_eq!(wip[0].line_number, 4);
    assert_eq!(wip[0].column, 7);

    // Filters and limits.
    let rs_only = xg
        .search_diff(
            "HEAD~1..HEAD",
            "needle",
            &SearchOptions::new().with_file_type("rs").with_max_count(2),
        )
        .unwrap();
    assert_eq!(rs_only.len(), 2);
    assert!(rs_only.iter().all(|d| d.file.ends_with(".rs")));

    assert!(xg
        .search_diff("--output=/tmp/x", "needle", &SearchOptions::new())
        .is_err());
    assert!(xg
        .search_diff("no-such-rev", "needle", &SearchOptions::new())
        .is_err());
}

#[test]
fn test_search_commits_library() {
    let dir = history_repo();
    let xg = Xgrep::open_local(dir.path()).unwrap();
    let ci = SearchOptions::new().with_case_insensitive(true);
    let hits = xg.search_commits("needle", &ci).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].subject, "refactor: rename things");
    assert_eq!(hits[0].line_number, 3);
    assert_eq!(hits[0].line, "The Needle function moved.");
    assert_eq!(hits[0].author, "Tester");
    assert!(hits[0].date.starts_with("2026-01-02"));
    assert_eq!(hits[0].hash.len(), 40);

    let all = xg
        .search_commits(r"^\w+:", &SearchOptions::new().with_regex(true))
        .unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].subject, "refactor: rename things", "newest first");
    let last = xg
        .search_commits(
            r"^\w+:",
            &SearchOptions::new()
                .with_regex(true)
                .with_since("1.commits"),
        )
        .unwrap();
    assert_eq!(last.len(), 1);
    let limited = xg
        .search_commits(
            r"^\w+:",
            &SearchOptions::new().with_regex(true).with_max_count(1),
        )
        .unwrap();
    assert_eq!(limited.len(), 1);
}

#[test]
fn test_cli_in_diff_and_log() {
    let dir = history_repo();
    let cache = tempfile::tempdir().unwrap();
    let (root, cache) = (dir.path(), cache.path());

    let o = xg(root, cache, &["--in-diff", "HEAD~1..HEAD", "needle"]);
    assert!(o.status.success(), "{:?}", o);
    assert_eq!(
        stdout(&o),
        "sub/a.rs:-2:fn old_needle() {}\n\
         sub/a.rs:+3:fn new_needle() {}\n\
         sub/gone.md:-1:needle in doc\n\
         top.rs:-1:needle at top\n\
         top.rs:+1:needle at top changed\n"
    );
    let o = xg(root, cache, &["--in-diff", "HEAD~1..HEAD", "-c", "needle"]);
    assert_eq!(stdout(&o), "sub/a.rs:2\nsub/gone.md:1\ntop.rs:2\n");
    let o = xg(
        root,
        cache,
        &["--in-diff", "HEAD~1..HEAD", "--json", "-t", "rs", "needle"],
    );
    let objs: Vec<serde_json::Value> = stdout(&o)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(objs.len(), 5);
    assert_eq!(objs[0]["side"], "removed");
    assert_eq!(objs[1]["side"], "added");
    assert_eq!(objs[4]["type"], "summary");
    let o = xg(root, cache, &["--in-diff", "HEAD~1..HEAD", "zzz_absent"]);
    assert_eq!(o.status.code(), Some(1));
    let o = xg(root, cache, &["--in-diff", "bogus-rev", "needle"]);
    assert_eq!(o.status.code(), Some(2));

    let o = xg(root, cache, &["--log", "-i", "needle"]);
    assert!(o.status.success(), "{:?}", o);
    let s = stdout(&o);
    let mut rows = s.lines();
    let head = rows.next().unwrap();
    assert!(
        head.ends_with(" 2026-01-02 refactor: rename things"),
        "{}",
        head
    );
    assert_eq!(rows.next(), Some("    L3: The Needle function moved."));
    let o = xg(root, cache, &["--log", "-c", "-e", r"^\w+:"]);
    assert_eq!(stdout(&o), "2\n");
}
