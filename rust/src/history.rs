//! Search in git history: lines added or removed by a diff, and commit
//! messages. Uses the `git` CLI.

use crate::error::{Result, XgrepError};
use crate::git;
use crate::search::{LineMatcher, ScanFilter};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::Arc;

/// Which side of a diff a line belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffSide {
    /// A `+` line; its line number refers to the new file.
    Added,
    /// A `-` line; its line number refers to the old file.
    Removed,
}

impl DiffSide {
    /// `'+'` or `'-'`.
    pub fn marker(self) -> char {
        match self {
            DiffSide::Added => '+',
            DiffSide::Removed => '-',
        }
    }

    /// `"added"` or `"removed"`.
    pub fn as_str(self) -> &'static str {
        match self {
            DiffSide::Added => "added",
            DiffSide::Removed => "removed",
        }
    }
}

/// A matching added or removed line, returned by
/// [`Xgrep::search_diff`](crate::Xgrep::search_diff).
#[derive(Debug, Clone)]
pub struct DiffMatch {
    /// Path relative to the search root: the new path for added lines, the
    /// old path for removed lines (they differ for renames).
    pub file: Arc<str>,
    /// Added or removed.
    pub side: DiffSide,
    /// 1-based line number in the new file (added) or old file (removed).
    pub line_number: usize,
    /// Line text without the `+`/`-` marker.
    pub line: String,
    /// Byte offset of the first match within `line` (0-based).
    pub column: usize,
}

/// A commit whose message matches, returned by
/// [`Xgrep::search_commits`](crate::Xgrep::search_commits).
#[derive(Debug, Clone)]
pub struct CommitMatch {
    /// Full commit hash.
    pub hash: String,
    /// Author date, ISO 8601 (`2026-10-17T12:34:56+09:00`).
    pub date: String,
    /// Author name.
    pub author: String,
    /// Subject (first line of the message).
    pub subject: String,
    /// 1-based line within the message of the first match (1 = subject).
    pub line_number: usize,
    /// Text of that message line.
    pub line: String,
    /// Byte offset of the first match within `line` (0-based).
    pub column: usize,
}

/// File-level AND / NOT patterns for diff search.
pub(crate) struct FileGates {
    pub(crate) and: Vec<LineMatcher>,
    pub(crate) not: Vec<LineMatcher>,
}

impl FileGates {
    fn is_empty(&self) -> bool {
        self.and.is_empty() && self.not.is_empty()
    }
}

/// Reject revision arguments that git would parse as options.
fn check_revs(range: &str) -> Result<Vec<&str>> {
    let revs: Vec<&str> = range.split_whitespace().collect();
    if revs.is_empty() {
        return Err(XgrepError::InvalidArgument(
            "empty revision range".to_string(),
        ));
    }
    if let Some(bad) = revs.iter().find(|r| r.starts_with('-')) {
        return Err(XgrepError::InvalidArgument(format!(
            "invalid revision range: {bad}"
        )));
    }
    Ok(revs)
}

/// Wait for `child` and turn a failed exit into an error carrying stderr.
fn finish(mut child: Child, what: &str, stopped_early: bool) -> Result<()> {
    if stopped_early {
        let _ = child.kill();
        let _ = child.wait();
        return Ok(());
    }
    let mut err = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.read_to_string(&mut err);
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(XgrepError::InvalidArgument(format!(
            "{} failed: {}",
            what,
            err.trim()
        )))
    }
}

/// Run `git diff` for `range` under `root` and search its +/- lines.
pub(crate) fn search_diff(
    root: &Path,
    range: &str,
    matcher: &LineMatcher,
    gates: &FileGates,
    filter: &ScanFilter,
) -> Result<Vec<DiffMatch>> {
    let revs = check_revs(range)?;
    if !git::is_git_repo(root) {
        return Err(XgrepError::NotGitRepo);
    }
    let mut child = git::git_cmd()
        .args(["-c", "core.quotePath=false", "diff"])
        .args([
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--unified=0",
            "--relative",
            "--src-prefix=a/",
            "--dst-prefix=b/",
        ])
        .args(&revs)
        .arg("--")
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let mut out = Vec::new();
    let complete = parse_diff(
        BufReader::new(stdout),
        matcher,
        gates,
        filter,
        filter.max_count.unwrap_or(usize::MAX),
        &mut out,
    )?;
    finish(child, "git diff", !complete)?;
    Ok(out)
}

/// Matches collected for the file currently being parsed.
#[derive(Default)]
struct DiffFile {
    old_path: Option<String>,
    new_path: Option<String>,
    /// Skip this file's lines (path filtered out).
    skip: bool,
    matches: Vec<DiffMatch>,
    and_seen: Vec<bool>,
    not_seen: bool,
}

impl DiffFile {
    fn path_for(&self, side: DiffSide) -> Option<&str> {
        match side {
            DiffSide::Added => self.new_path.as_deref().or(self.old_path.as_deref()),
            DiffSide::Removed => self.old_path.as_deref().or(self.new_path.as_deref()),
        }
    }
}

/// Parse a unified diff (`--unified=0` or any context size) and collect
/// matching `+`/`-` lines into `out`. Returns false if it stopped early
/// because `limit` was reached.
fn parse_diff(
    mut reader: impl BufRead,
    matcher: &LineMatcher,
    gates: &FileGates,
    filter: &ScanFilter,
    limit: usize,
    out: &mut Vec<DiffMatch>,
) -> Result<bool> {
    if limit == 0 {
        return Ok(false);
    }
    let mut file = DiffFile::default();
    let mut paths: [Option<Arc<str>>; 2] = [None, None];
    let (mut old_line, mut new_line) = (0usize, 0usize);
    let (mut old_rem, mut new_rem) = (0usize, 0usize);
    let mut buf = Vec::new();

    // Move a finished file's matches to `out`; true if the limit is reached.
    let flush = |file: &mut DiffFile, out: &mut Vec<DiffMatch>| -> bool {
        let pass = file.and_seen.iter().all(|&s| s) && !file.not_seen;
        if pass {
            let room = limit - out.len();
            out.extend(file.matches.drain(..).take(room));
        }
        out.len() >= limit
    };

    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        if buf.last() == Some(&b'\n') {
            buf.pop();
        }
        let in_hunk = old_rem > 0 || new_rem > 0;
        if !in_hunk {
            if buf.starts_with(b"diff ") {
                if flush(&mut file, out) {
                    return Ok(false);
                }
                file = DiffFile {
                    and_seen: vec![false; gates.and.len()],
                    ..Default::default()
                };
                paths = [None, None];
            } else if let Some(rest) = buf.strip_prefix(b"--- ") {
                file.old_path = parse_diff_path(rest, b"a/");
            } else if let Some(rest) = buf.strip_prefix(b"+++ ") {
                file.new_path = parse_diff_path(rest, b"b/");
                let primary = file.new_path.as_deref().or(file.old_path.as_deref());
                file.skip = !primary.is_some_and(|p| filter.accepts(p));
            } else if buf.starts_with(b"@@ ") {
                if let Some((os, oc, ns, nc)) = parse_hunk_header(&buf) {
                    old_line = os;
                    old_rem = oc;
                    new_line = ns;
                    new_rem = nc;
                }
            }
            continue;
        }

        let Some((&marker, content)) = buf.split_first() else {
            // An empty line inside a hunk: treat as empty context.
            old_rem = old_rem.saturating_sub(1);
            new_rem = new_rem.saturating_sub(1);
            old_line += 1;
            new_line += 1;
            continue;
        };
        let side = match marker {
            b'+' => {
                new_rem = new_rem.saturating_sub(1);
                new_line += 1;
                Some((DiffSide::Added, new_line - 1))
            }
            b'-' => {
                old_rem = old_rem.saturating_sub(1);
                old_line += 1;
                Some((DiffSide::Removed, old_line - 1))
            }
            b'\\' => None, // "\ No newline at end of file"
            _ => {
                old_rem = old_rem.saturating_sub(1);
                new_rem = new_rem.saturating_sub(1);
                old_line += 1;
                new_line += 1;
                None
            }
        };
        let Some((side, line_number)) = side else {
            continue;
        };
        if file.skip {
            continue;
        }
        let content = content.strip_suffix(b"\r").unwrap_or(content);
        if !gates.is_empty() {
            for (seen, m) in file.and_seen.iter_mut().zip(&gates.and) {
                if !*seen && m.find(content).is_some() {
                    *seen = true;
                }
            }
            if !file.not_seen && gates.not.iter().any(|m| m.find(content).is_some()) {
                file.not_seen = true;
            }
        }
        if let Some(column) = matcher.find(content) {
            let slot = (side == DiffSide::Removed) as usize;
            let path = match &paths[slot] {
                Some(p) => Arc::clone(p),
                None => {
                    let p: Arc<str> = Arc::from(file.path_for(side).unwrap_or(""));
                    paths[slot] = Some(Arc::clone(&p));
                    p
                }
            };
            file.matches.push(DiffMatch {
                file: path,
                side,
                line_number,
                line: String::from_utf8_lossy(content).into_owned(),
                column,
            });
            // Without gates, results can be released as soon as the limit
            // is reached.
            if gates.is_empty() && out.len() + file.matches.len() >= limit {
                flush(&mut file, out);
                return Ok(false);
            }
        }
    }
    flush(&mut file, out);
    Ok(true)
}

/// Parse the path of a `---`/`+++` line: `/dev/null` is `None`, quoted
/// paths are unquoted, the `a/`/`b/` prefix is removed.
fn parse_diff_path(rest: &[u8], prefix: &[u8]) -> Option<String> {
    let raw: Vec<u8> = if rest.first() == Some(&b'"') {
        unquote_c(rest)
    } else {
        // git appends a tab to names containing spaces.
        rest.strip_suffix(b"\t").unwrap_or(rest).to_vec()
    };
    if raw == b"/dev/null" {
        return None;
    }
    let path = raw.strip_prefix(prefix).unwrap_or(&raw);
    Some(String::from_utf8_lossy(path).into_owned())
}

/// Undo git's C-style quoting (`"a/t\303\244b\tx"`).
fn unquote_c(s: &[u8]) -> Vec<u8> {
    let inner = s.strip_prefix(b"\"").unwrap_or(s);
    let mut out = Vec::with_capacity(inner.len());
    let mut i = 0;
    while i < inner.len() {
        let c = inner[i];
        if c == b'"' {
            break;
        }
        if c != b'\\' || i + 1 >= inner.len() {
            out.push(c);
            i += 1;
            continue;
        }
        let e = inner[i + 1];
        i += 2;
        match e {
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'r' => out.push(b'\r'),
            b'a' => out.push(7),
            b'b' => out.push(8),
            b'f' => out.push(12),
            b'v' => out.push(11),
            b'0'..=b'7' => {
                let mut v = u32::from(e - b'0');
                let mut n = 1;
                while n < 3 && i < inner.len() && (b'0'..=b'7').contains(&inner[i]) {
                    v = v * 8 + u32::from(inner[i] - b'0');
                    i += 1;
                    n += 1;
                }
                out.push(v as u8);
            }
            other => out.push(other),
        }
    }
    out
}

/// Parse `@@ -a[,b] +c[,d] @@ ...` into (a, b, c, d).
fn parse_hunk_header(line: &[u8]) -> Option<(usize, usize, usize, usize)> {
    let s = std::str::from_utf8(line).ok()?;
    let mut parts = s.split(' ');
    parts.next()?; // "@@"
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let range = |r: &str| -> Option<(usize, usize)> {
        match r.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((r.parse().ok()?, 1)),
        }
    };
    let (os, oc) = range(old)?;
    let (ns, nc) = range(new)?;
    Some((os, oc, ns, nc))
}

/// Run `git log` under `root` and search commit messages.
pub(crate) fn search_commits(
    root: &Path,
    matcher: &LineMatcher,
    since: Option<&str>,
    max_count: Option<usize>,
) -> Result<Vec<CommitMatch>> {
    if !git::is_git_repo(root) {
        return Err(XgrepError::NotGitRepo);
    }
    let mut cmd = git::git_cmd();
    cmd.args([
        "log",
        "-z",
        "--no-color",
        "--format=%H%x1f%aI%x1f%an%x1f%s%x1f%B",
    ]);
    if let Some(since) = since {
        match git::parse_duration(since)? {
            Some(s) => {
                cmd.arg(format!("--since={s}"));
            }
            None => {
                let n = since.trim_end_matches(".commits");
                cmd.arg(format!("-{n}"));
            }
        }
    }
    // Limit to the root's subtree when it is not the repository root.
    let canon = std::fs::canonicalize(root).ok();
    let top = git::git_toplevel(root).and_then(|t| std::fs::canonicalize(t).ok());
    if canon.is_some() && canon != top {
        cmd.args(["--", "."]);
    }
    let mut child = cmd
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let limit = max_count.unwrap_or(usize::MAX);
    let mut out = Vec::new();
    let complete = parse_log(BufReader::new(stdout), matcher, limit, &mut out)?;
    finish(child, "git log", !complete)?;
    Ok(out)
}

/// Parse `git log -z --format=%H%x1f%aI%x1f%an%x1f%s%x1f%B` output. Returns
/// false if it stopped early because `limit` was reached.
fn parse_log(
    mut reader: impl BufRead,
    matcher: &LineMatcher,
    limit: usize,
    out: &mut Vec<CommitMatch>,
) -> Result<bool> {
    if limit == 0 {
        return Ok(false);
    }
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if reader.read_until(0, &mut buf)? == 0 {
            return Ok(true);
        }
        if buf.last() == Some(&0) {
            buf.pop();
        }
        let rec = buf.strip_prefix(b"\n").unwrap_or(&buf);
        let mut fields = rec.splitn(5, |&b| b == 0x1f);
        let (Some(hash), Some(date), Some(author), Some(subject), Some(body)) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            continue;
        };
        let hit = body
            .split(|&b| b == b'\n')
            .map(|l| l.strip_suffix(b"\r").unwrap_or(l))
            .enumerate()
            .find_map(|(i, l)| matcher.find(l).map(|c| (i + 1, l, c)));
        if let Some((line_number, line, column)) = hit {
            let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
            out.push(CommitMatch {
                hash: text(hash),
                date: text(date),
                author: text(author),
                subject: text(subject),
                line_number,
                line: text(line),
                column,
            });
            if out.len() >= limit {
                return Ok(false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::PatternSpec;

    fn lit(p: &str) -> LineMatcher {
        let spec = PatternSpec {
            pattern: p.to_string(),
            regex: false,
            word: false,
        };
        LineMatcher::new(&spec, false, true).unwrap()
    }

    fn no_gates() -> FileGates {
        FileGates {
            and: vec![],
            not: vec![],
        }
    }

    const DIFF: &str = "\
diff --git a/src/a.rs b/src/a.rs
index 111..222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -3 +3 @@ fn x()
-let foo = 1;
+let foo = 2;
@@ -10,0 +11,2 @@
+// added foo
+bar
diff --git a/old.txt b/new.txt
similarity index 90%
rename from old.txt
rename to new.txt
--- a/old.txt
+++ b/new.txt
@@ -1,2 +0,0 @@
--- removed dashes foo
-+++ removed pluses
diff --git a/gone.md b/gone.md
deleted file mode 100644
--- a/gone.md
+++ /dev/null
@@ -1 +0,0 @@
-foo in deleted file
diff --git a/bin.png b/bin.png
Binary files a/bin.png and b/bin.png differ
diff --git \"a/sp\\303\\244ce.txt\" \"b/sp\\303\\244ce.txt\"
--- \"a/sp\\303\\244ce.txt\"
+++ \"b/sp\\303\\244ce.txt\"
@@ -0,0 +1 @@
+foo quoted
\\ No newline at end of file
";

    fn run(m: &LineMatcher, gates: &FileGates, filter: &ScanFilter, limit: usize) -> Vec<String> {
        let mut out = Vec::new();
        parse_diff(DIFF.as_bytes(), m, gates, filter, limit, &mut out).unwrap();
        out.iter()
            .map(|d| {
                format!(
                    "{}:{}{}:{}:{}",
                    d.file,
                    d.side.marker(),
                    d.line_number,
                    d.column,
                    d.line
                )
            })
            .collect()
    }

    #[test]
    fn test_parse_diff_sides_and_line_numbers() {
        let got = run(&lit("foo"), &no_gates(), &ScanFilter::default(), usize::MAX);
        assert_eq!(
            got,
            vec![
                "src/a.rs:-3:4:let foo = 1;",
                "src/a.rs:+3:4:let foo = 2;",
                "src/a.rs:+11:9:// added foo",
                "old.txt:-1:18:-- removed dashes foo",
                "gone.md:-1:0:foo in deleted file",
                "späce.txt:+1:0:foo quoted",
            ]
        );
        let got = run(&lit("+++"), &no_gates(), &ScanFilter::default(), usize::MAX);
        assert_eq!(got, vec!["old.txt:-2:0:+++ removed pluses"]);
    }

    #[test]
    fn test_parse_diff_limit_filter_and_gates() {
        let got = run(&lit("foo"), &no_gates(), &ScanFilter::default(), 2);
        assert_eq!(got.len(), 2);

        let filter = ScanFilter {
            extensions: Some(vec!["rs"]),
            ..Default::default()
        };
        let got = run(&lit("foo"), &no_gates(), &filter, usize::MAX);
        assert_eq!(got.len(), 3);
        assert!(got.iter().all(|l| l.starts_with("src/a.rs:")));

        // AND: only files whose changed lines also contain "bar".
        let gates = FileGates {
            and: vec![lit("bar")],
            not: vec![],
        };
        let got = run(&lit("foo"), &gates, &ScanFilter::default(), usize::MAX);
        assert_eq!(got.len(), 3);
        // NOT: drop files whose changed lines contain "added".
        let gates = FileGates {
            and: vec![],
            not: vec![lit("added")],
        };
        let got = run(&lit("foo"), &gates, &ScanFilter::default(), usize::MAX);
        assert!(got.iter().all(|l| !l.starts_with("src/a.rs:")));
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn test_parse_hunk_header_and_unquote() {
        assert_eq!(parse_hunk_header(b"@@ -3 +3 @@"), Some((3, 1, 3, 1)));
        assert_eq!(
            parse_hunk_header(b"@@ -10,0 +11,2 @@ fn x"),
            Some((10, 0, 11, 2))
        );
        assert_eq!(parse_hunk_header(b"@@ bogus"), None);
        assert_eq!(unquote_c(b"\"a\\tb\\\"c\\\\\""), b"a\tb\"c\\".to_vec());
        assert_eq!(
            parse_diff_path(b"a/with space.txt\t", b"a/").as_deref(),
            Some("with space.txt")
        );
        assert_eq!(parse_diff_path(b"/dev/null", b"a/"), None);
    }

    #[test]
    fn test_check_revs() {
        assert!(check_revs("--output=/tmp/x").is_err());
        assert!(check_revs("  ").is_err());
        assert_eq!(check_revs("main HEAD").unwrap(), vec!["main", "HEAD"]);
    }

    #[test]
    fn test_parse_log() {
        let log = "aaa\x1f2026-01-02T03:04:05+00:00\x1fAlice\x1ffix: parser\x1ffix: parser\n\nDetails about Foo\n\0\
                   \nbbb\x1f2026-01-01T00:00:00+00:00\x1fBob\x1ffeat: other\x1ffeat: other\n\0";
        let m = LineMatcher::new(
            &PatternSpec {
                pattern: "foo".into(),
                regex: false,
                word: false,
            },
            true,
            true,
        )
        .unwrap();
        let mut out = Vec::new();
        assert!(parse_log(log.as_bytes(), &m, usize::MAX, &mut out).unwrap());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].hash, "aaa");
        assert_eq!(out[0].author, "Alice");
        assert_eq!(out[0].subject, "fix: parser");
        assert_eq!(out[0].line_number, 3);
        assert_eq!(out[0].line, "Details about Foo");
        assert_eq!(out[0].column, 14);

        let mut out = Vec::new();
        assert!(!parse_log(log.as_bytes(), &lit(":"), 1, &mut out).unwrap());
        assert_eq!(out.len(), 1);
    }
}
