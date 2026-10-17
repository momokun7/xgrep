use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use crate::error::Result;

use crate::search::SearchResult;

/// Default maximum characters shown per line in LLM output (`--format llm`, MCP).
pub const DEFAULT_LLM_MAX_LINE_CHARS: usize = 300;

/// Tokens kept in reserve for the closing fence and truncation notice when a
/// token budget is in effect, so the whole output stays within the budget.
const LLM_FOOTER_RESERVE_TOKENS: usize = 24;

/// Return the Markdown code block language name for a file extension.
pub fn lang_from_ext(ext: &str) -> &'static str {
    match ext {
        "rs" => "rust",
        "py" => "python",
        "js" => "javascript",
        "ts" => "typescript",
        "go" => "go",
        "rb" => "ruby",
        "java" => "java",
        "c" | "h" => "c",
        "cpp" | "cc" | "hpp" => "cpp",
        "sh" | "bash" => "bash",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "md" => "markdown",
        "html" => "html",
        "css" => "css",
        "sql" => "sql",
        _ => "",
    }
}

/// Options for the ripgrep-style text output (`write_default*`).
#[derive(Debug, Clone, Default)]
pub struct TextOptions {
    /// Lines of context before each match (context output only).
    pub before_context: usize,
    /// Lines of context after each match (context output only).
    pub after_context: usize,
    /// Print paths joined with the search root.
    pub absolute_paths: bool,
    /// Truncate lines longer than this many characters (`None` = no limit).
    pub max_columns: Option<usize>,
}

/// Options for the Markdown output aimed at LLMs (`write_llm`).
#[derive(Debug, Clone)]
pub struct LlmOptions {
    /// Lines of context before each match.
    pub before_context: usize,
    /// Lines of context after each match.
    pub after_context: usize,
    /// Approximate token budget for the whole output (`None` = unlimited).
    /// Checked line by line, so one huge file or line cannot exceed it.
    pub max_tokens: Option<usize>,
    /// Print paths joined with the search root.
    pub absolute_paths: bool,
    /// Truncate lines longer than this many characters (`None` = no limit).
    pub max_line_chars: Option<usize>,
}

impl Default for LlmOptions {
    fn default() -> Self {
        LlmOptions {
            before_context: 3,
            after_context: 3,
            max_tokens: None,
            absolute_paths: false,
            max_line_chars: Some(DEFAULT_LLM_MAX_LINE_CHARS),
        }
    }
}

/// What `write_llm` emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LlmSummary {
    /// Number of matched lines that were shown.
    pub shown_matches: usize,
    /// Number of matched lines in the input.
    pub total_matches: usize,
    /// True when output stopped early because of the token budget.
    pub truncated: bool,
}

/// Shorten `line` to at most `max_chars` characters, appending `…[+N chars]`
/// with the number of characters left out. `None` leaves the line untouched.
pub fn truncate_line(line: &str, max_chars: Option<usize>) -> Cow<'_, str> {
    let Some(max) = max_chars else {
        return Cow::Borrowed(line);
    };
    // A line of at most `max` bytes has at most `max` characters.
    if line.len() <= max {
        return Cow::Borrowed(line);
    }
    match line.char_indices().nth(max) {
        None => Cow::Borrowed(line),
        Some((cut, _)) => {
            let rest = line[cut..].chars().count();
            Cow::Owned(format!("{}…[+{} chars]", &line[..cut], rest))
        }
    }
}

/// Read a file for display, never failing on its contents.
///
/// Bytes that are not valid UTF-8 are replaced with U+FFFD. Line splitting
/// happens on `\n` just like the searcher, so line numbers stay aligned.
/// Returns `None` when the file cannot be read (deleted, permission denied,
/// ...); callers then fall back to the matched lines carried in the results.
fn read_display_text(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    Some(match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    })
}

/// Return the text of line `n` (1-based): from the file when available,
/// otherwise from the search result for that line (if it is a match).
fn line_text<'a>(
    lines: &[&'a str],
    matches: &BTreeMap<usize, &'a str>,
    n: usize,
) -> Option<&'a str> {
    match lines.get(n - 1) {
        Some(l) => Some(l),
        None => matches.get(&n).copied(),
    }
}

/// Group results by file (sorted), keeping each matched line's text.
fn group_by_file(results: &[SearchResult]) -> BTreeMap<&str, BTreeMap<usize, &str>> {
    let mut by_file: BTreeMap<&str, BTreeMap<usize, &str>> = BTreeMap::new();
    for r in results {
        by_file
            .entry(&r.file)
            .or_default()
            .insert(r.line_number, r.line.as_str());
    }
    by_file
}

/// Compute context ranges `(start, end)` (1-based, inclusive) around the
/// matches, merging ranges that overlap or touch.
fn context_ranges(
    match_lines: impl Iterator<Item = usize>,
    before: usize,
    after: usize,
    total_lines: usize,
) -> Vec<(usize, usize)> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for ln in match_lines {
        let start = ln.saturating_sub(before).max(1);
        // Never end before the match itself (file shrank or unreadable).
        let end = ln.saturating_add(after).min(total_lines).max(ln);
        if let Some(last) = ranges.last_mut() {
            if start <= last.1 + 1 {
                last.1 = last.1.max(end);
                continue;
            }
        }
        ranges.push((start, end));
    }
    ranges
}

fn display_path<'a>(root: &Path, file: &'a str, absolute: bool) -> Cow<'a, str> {
    if absolute {
        Cow::Owned(root.join(file).to_string_lossy().into_owned())
    } else {
        Cow::Borrowed(file)
    }
}

/// Render into a `String` with a writer-based formatter, dropping the final
/// newline (the string formatters historically had none).
fn render_to_string(f: impl FnOnce(&mut Vec<u8>) -> io::Result<()>) -> Result<String> {
    let mut buf = Vec::new();
    f(&mut buf)?;
    if buf.last() == Some(&b'\n') {
        buf.pop();
    }
    // Every piece written is a &str, so this cannot fail; stay lossless anyway.
    Ok(String::from_utf8(buf)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
}

/// Write matches in ripgrep-compatible `path:line:text` form, one per line.
pub fn write_default<W: Write>(
    w: &mut W,
    results: &[SearchResult],
    root: &Path,
    opts: &TextOptions,
) -> io::Result<()> {
    for r in results {
        writeln!(
            w,
            "{}:{}:{}",
            display_path(root, &r.file, opts.absolute_paths),
            r.line_number,
            truncate_line(&r.line, opts.max_columns)
        )?;
    }
    Ok(())
}

/// Format output in ripgrep-compatible default format.
pub fn format_default(results: &[SearchResult]) -> String {
    render_to_string(|buf| write_default(buf, results, Path::new(""), &TextOptions::default()))
        .unwrap_or_default()
}

/// Write matches with context in ripgrep-compatible form: `path:N:text` for
/// matched lines, `path-N-text` for context lines and `--` between groups.
pub fn write_default_context<W: Write>(
    w: &mut W,
    results: &[SearchResult],
    root: &Path,
    opts: &TextOptions,
) -> io::Result<()> {
    let mut first_group = true;
    for (file, matches) in &group_by_file(results) {
        let content = read_display_text(&root.join(file)).unwrap_or_default();
        let lines: Vec<&str> = content.lines().collect();
        let path = display_path(root, file, opts.absolute_paths);
        let ranges = context_ranges(
            matches.keys().copied(),
            opts.before_context,
            opts.after_context,
            lines.len(),
        );

        for (start, end) in ranges {
            // Separator between context groups (ripgrep uses -- for both
            // intra-file and inter-file).
            if !first_group {
                w.write_all(b"--\n")?;
            }
            first_group = false;
            // Both the lines and the match keys ascend, so walk them together
            // instead of searching the match list for every line.
            let mut match_cursor = matches.range(start..=end).map(|(&k, _)| k).peekable();
            for i in start..=end {
                let is_match = match_cursor.next_if_eq(&i).is_some();
                if let Some(text) = line_text(&lines, matches, i) {
                    let sep = if is_match { ':' } else { '-' };
                    writeln!(
                        w,
                        "{}{}{}{}{}",
                        path,
                        sep,
                        i,
                        sep,
                        truncate_line(text, opts.max_columns)
                    )?;
                }
            }
        }
    }
    Ok(())
}

/// Format output with context in ripgrep-compatible format (`--` separator).
pub fn format_default_context(
    results: &[SearchResult],
    root: &Path,
    before_context: usize,
    after_context: usize,
    absolute_paths: bool,
) -> Result<String> {
    let opts = TextOptions {
        before_context,
        after_context,
        absolute_paths,
        max_columns: None,
    };
    render_to_string(|buf| write_default_context(buf, results, root, &opts))
}

/// Write results as JSON Lines (NDJSON): one object per matched line,
///
/// `{"type":"match","file":"src/a.rs","line_number":3,"line":"..."}`
///
/// followed by one summary object:
///
/// `{"type":"summary","total_matches":N,"total_files":M,"truncated":false}`
///
/// `truncated` tells whether the results were cut short by a result limit.
/// Objects are written directly, without building a JSON tree in memory.
pub fn write_json_lines<W: Write>(
    w: &mut W,
    results: &[SearchResult],
    root: &Path,
    absolute_paths: bool,
    truncated: bool,
) -> io::Result<()> {
    let mut files: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for r in results {
        files.insert(&r.file);
        w.write_all(b"{\"type\":\"match\",\"file\":")?;
        serde_json::to_writer(&mut *w, &*display_path(root, &r.file, absolute_paths))?;
        write!(w, ",\"line_number\":{},\"line\":", r.line_number)?;
        serde_json::to_writer(&mut *w, &r.line)?;
        w.write_all(b"}\n")?;
    }
    writeln!(
        w,
        "{{\"type\":\"summary\",\"total_matches\":{},\"total_files\":{},\"truncated\":{}}}",
        results.len(),
        files.len(),
        truncated
    )
}

/// Format output as JSON Lines (see [`write_json_lines`]).
pub fn format_json(results: &[SearchResult]) -> String {
    render_to_string(|buf| write_json_lines(buf, results, Path::new(""), false, false))
        .unwrap_or_default()
}

/// Estimate token count. ASCII ~4 bytes/token, non-ASCII (CJK etc.) ~2 bytes/token.
pub fn estimate_tokens(text: &str) -> usize {
    let mut meter = TokenMeter::default();
    meter.add(text);
    meter.tokens()
}

/// Running token estimate of everything written so far (same rule as
/// `estimate_tokens`, applied to the concatenated output).
#[derive(Debug, Default, Clone, Copy)]
struct TokenMeter {
    ascii_bytes: usize,
    non_ascii_bytes: usize,
}

impl TokenMeter {
    fn add(&mut self, text: &str) {
        let ascii = text.bytes().filter(|b| b.is_ascii()).count();
        self.ascii_bytes += ascii;
        self.non_ascii_bytes += text.len() - ascii;
    }

    fn tokens(&self) -> usize {
        (self.ascii_bytes / 4) + (self.non_ascii_bytes / 2) + 1
    }

    /// Token estimate if all of `texts` were appended.
    fn tokens_with(&self, texts: &[&str]) -> usize {
        let mut m = *self;
        for t in texts {
            m.add(t);
        }
        m.tokens()
    }
}

/// Writer that keeps a token estimate of what passes through it.
struct MeteredWriter<'w, W: Write> {
    inner: &'w mut W,
    meter: TokenMeter,
}

impl<W: Write> MeteredWriter<'_, W> {
    fn put(&mut self, text: &str) -> io::Result<()> {
        self.meter.add(text);
        self.inner.write_all(text.as_bytes())
    }
}

/// Length of the longest run of consecutive backticks in `s`.
fn longest_backtick_run(s: &str) -> usize {
    let mut longest = 0;
    let mut run = 0;
    for b in s.bytes() {
        if b == b'`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    longest
}

/// Write results as Markdown code blocks for LLMs.
///
/// Each block is headed by `## path:start-end` and every line is prefixed by
/// its line number, with `>` marking matched lines (`181> ...`) and a space
/// marking context lines (`178  ...`). The code fence is longer than any
/// backtick run inside the block so content cannot close it early. When
/// `max_tokens` is set, the budget is checked before every line; output stops
/// with a `... (truncated, ...)` notice once the next line would not fit.
pub fn write_llm<W: Write>(
    w: &mut W,
    results: &[SearchResult],
    root: &Path,
    opts: &LlmOptions,
) -> io::Result<LlmSummary> {
    let by_file = group_by_file(results);
    let total_matches: usize = by_file.values().map(|m| m.len()).sum();
    let limit = match opts.max_tokens {
        Some(max) => max.saturating_sub(LLM_FOOTER_RESERVE_TOKENS),
        None => usize::MAX,
    };

    let mut out = MeteredWriter {
        inner: w,
        meter: TokenMeter::default(),
    };
    let mut shown_matches = 0;
    let mut shown_any_line = false;
    // (index of the file being written, matches shown from it) when truncated.
    let mut cut: Option<(usize, usize)> = None;

    'files: for (file_idx, (file, matches)) in by_file.iter().enumerate() {
        let content = read_display_text(&root.join(file)).unwrap_or_default();
        let lines: Vec<&str> = content.lines().collect();
        let path = display_path(root, file, opts.absolute_paths);
        let lang = Path::new(file)
            .extension()
            .and_then(|e| e.to_str())
            .map(lang_from_ext)
            .unwrap_or("");
        let ranges = context_ranges(
            matches.keys().copied(),
            opts.before_context,
            opts.after_context,
            lines.len(),
        );
        let mut shown_in_file = 0;

        for (start, end) in ranges {
            let mut match_cursor = matches.range(start..=end).map(|(&k, _)| k).peekable();
            let block: Vec<(usize, bool, Cow<str>)> = (start..=end)
                .filter_map(|i| {
                    let is_match = match_cursor.next_if_eq(&i).is_some();
                    line_text(&lines, matches, i)
                        .map(|t| (i, is_match, truncate_line(t, opts.max_line_chars)))
                })
                .collect();
            let fence_len = block
                .iter()
                .map(|(_, _, t)| longest_backtick_run(t) + 1)
                .max()
                .unwrap_or(0)
                .max(3);
            let fence = "`".repeat(fence_len);
            let width = end.to_string().len();
            let sep = if shown_any_line { "\n" } else { "" };
            let header = if start == end {
                format!("{}## {}:{}\n\n{}{}\n", sep, path, start, fence, lang)
            } else {
                format!(
                    "{}## {}:{}-{}\n\n{}{}\n",
                    sep, path, start, end, fence, lang
                )
            };
            let close = format!("{}\n", fence);

            let mut opened = false;
            for (i, is_match, text) in &block {
                let marker = if *is_match { '>' } else { ' ' };
                let mut line = format!("{:>width$}{} {}\n", i, marker, text);
                let pending_header = if opened { "" } else { header.as_str() };
                if out.meter.tokens_with(&[pending_header, &line, &close]) > limit {
                    if shown_any_line {
                        if opened {
                            out.put(&close)?;
                        }
                        cut = Some((file_idx, shown_in_file));
                        break 'files;
                    }
                    // Nothing shown yet: show a shortened first line rather
                    // than an empty answer.
                    let mut chars = limit.saturating_mul(4).max(16);
                    loop {
                        let short = truncate_line(text, Some(chars));
                        line = format!("{:>width$}{} {}\n", i, marker, short);
                        if chars <= 16
                            || out.meter.tokens_with(&[pending_header, &line, &close]) <= limit
                        {
                            break;
                        }
                        chars /= 2;
                    }
                    out.put(pending_header)?;
                    out.put(&line)?;
                    out.put(&close)?;
                    if *is_match {
                        shown_in_file += 1;
                        shown_matches += 1;
                    }
                    cut = Some((file_idx, shown_in_file));
                    break 'files;
                }
                if !opened {
                    out.put(&header)?;
                    opened = true;
                }
                out.put(&line)?;
                shown_any_line = true;
                if *is_match {
                    shown_in_file += 1;
                    shown_matches += 1;
                }
            }
            if opened {
                out.put(&close)?;
            }
        }
    }

    let truncated = cut.is_some();
    if let Some((file_idx, shown_in_file)) = cut {
        let remaining_matches = total_matches - shown_matches;
        let current_left = by_file
            .values()
            .nth(file_idx)
            .is_some_and(|m| m.len() > shown_in_file);
        let remaining_files = by_file.len() - file_idx - 1 + usize::from(current_left);
        let sep = if shown_any_line { "\n" } else { "" };
        if remaining_matches > 0 {
            out.put(&format!(
                "{}... (truncated, {} more matches in {} more files)\n",
                sep, remaining_matches, remaining_files
            ))?;
        } else {
            out.put(&format!("{}... (truncated)\n", sep))?;
        }
    }

    Ok(LlmSummary {
        shown_matches,
        total_matches,
        truncated,
    })
}

/// Format results as Markdown for LLMs using `opts`.
pub fn format_llm_with(results: &[SearchResult], root: &Path, opts: &LlmOptions) -> String {
    let mut buf = Vec::new();
    // Writing to a Vec cannot fail.
    let _ = write_llm(&mut buf, results, root, opts);
    String::from_utf8(buf).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// Format output as Markdown code blocks for LLM consumption (with context lines).
///
/// Lines are cut at [`DEFAULT_LLM_MAX_LINE_CHARS`]; see [`write_llm`].
pub fn format_llm(
    results: &[SearchResult],
    root: &Path,
    before_context: usize,
    after_context: usize,
    max_tokens: Option<usize>,
    absolute_paths: bool,
) -> Result<String> {
    let opts = LlmOptions {
        before_context,
        after_context,
        max_tokens,
        absolute_paths,
        ..LlmOptions::default()
    };
    Ok(format_llm_with(results, root, &opts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_format_json() {
        let results = vec![SearchResult {
            file: "src/main.rs".into(),
            line_number: 42,
            line: "fn handle_auth() {}".to_string(),
        }];
        let json = format_json(&results);
        let parsed = parse_ndjson(&json);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["type"], "match");
        assert_eq!(parsed[0]["file"], "src/main.rs");
        assert_eq!(parsed[0]["line_number"], 42);
        assert_eq!(parsed[0]["line"], "fn handle_auth() {}");
        assert_eq!(parsed[1]["type"], "summary");
        assert_eq!(parsed[1]["total_matches"], 1);
        assert_eq!(parsed[1]["total_files"], 1);
        assert_eq!(parsed[1]["truncated"], false);
    }

    fn parse_ndjson(s: &str) -> Vec<serde_json::Value> {
        s.lines()
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{}: {}", e, l)))
            .collect()
    }

    #[test]
    fn test_format_json_empty() {
        let json = format_json(&[]);
        let parsed = parse_ndjson(&json);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["type"], "summary");
        assert_eq!(parsed[0]["total_matches"], 0);
    }

    #[test]
    fn test_write_json_lines_escaping_absolute_and_truncated() {
        let results = vec![
            result("a \"q\".rs", 1, "tab\there \\ \"quoted\" \u{1} ünï"),
            result("a \"q\".rs", 2, "second"),
        ];
        let mut buf = Vec::new();
        write_json_lines(&mut buf, &results, Path::new("/root"), true, true).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.ends_with('\n'));
        let parsed = parse_ndjson(&text);
        assert_eq!(parsed.len(), 3);
        assert_eq!(
            parsed[0]["file"],
            Path::new("/root")
                .join("a \"q\".rs")
                .to_string_lossy()
                .as_ref()
        );
        assert_eq!(parsed[0]["line"], "tab\there \\ \"quoted\" \u{1} ünï");
        assert_eq!(parsed[2]["total_matches"], 2);
        assert_eq!(parsed[2]["total_files"], 1);
        assert_eq!(parsed[2]["truncated"], true);
    }

    #[test]
    fn test_format_json_multiple() {
        let results = vec![
            SearchResult {
                file: "a.rs".into(),
                line_number: 1,
                line: "foo".to_string(),
            },
            SearchResult {
                file: "b.rs".into(),
                line_number: 2,
                line: "bar".to_string(),
            },
        ];
        let json = format_json(&results);
        let parsed = parse_ndjson(&json);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[1]["file"], "b.rs");
        assert_eq!(parsed[2]["total_files"], 2);
    }

    #[test]
    fn test_format_default() {
        let results = vec![SearchResult {
            file: "src/main.rs".into(),
            line_number: 42,
            line: "    fn handle_auth() {}".to_string(),
        }];
        let output = format_default(&results);
        assert_eq!(output, "src/main.rs:42:    fn handle_auth() {}");
    }

    #[test]
    fn test_format_default_multiple() {
        let results = vec![
            SearchResult {
                file: "a.rs".into(),
                line_number: 1,
                line: "foo".to_string(),
            },
            SearchResult {
                file: "b.rs".into(),
                line_number: 10,
                line: "bar".to_string(),
            },
        ];
        let output = format_default(&results);
        assert_eq!(output, "a.rs:1:foo\nb.rs:10:bar");
    }

    #[test]
    fn test_lang_from_ext() {
        assert_eq!(lang_from_ext("rs"), "rust");
        assert_eq!(lang_from_ext("py"), "python");
        assert_eq!(lang_from_ext("js"), "javascript");
        assert_eq!(lang_from_ext("ts"), "typescript");
        assert_eq!(lang_from_ext("go"), "go");
        assert_eq!(lang_from_ext("c"), "c");
        assert_eq!(lang_from_ext("h"), "c");
        assert_eq!(lang_from_ext("cpp"), "cpp");
        assert_eq!(lang_from_ext("xyz"), "");
    }

    #[test]
    fn test_format_llm_single_match() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("test.rs"),
            "line1\nline2\nfn hello() {}\nline4\nline5\nline6\nline7",
        )
        .unwrap();
        let results = vec![SearchResult {
            file: "test.rs".into(),
            line_number: 3,
            line: "fn hello() {}".to_string(),
        }];
        let output = format_llm(&results, root, 2, 2, None, false).unwrap();
        assert!(output.contains("## test.rs:"));
        assert!(output.contains("```rust"));
        assert!(output.contains("fn hello() {}"));
        assert!(output.contains("line2"));
        assert!(output.contains("line4"));
    }

    #[test]
    fn test_format_llm_merge_context() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("test.rs"), "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8").unwrap();
        let results = vec![
            SearchResult {
                file: "test.rs".into(),
                line_number: 3,
                line: "l3".to_string(),
            },
            SearchResult {
                file: "test.rs".into(),
                line_number: 5,
                line: "l5".to_string(),
            },
        ];
        let output = format_llm(&results, root, 1, 1, None, false).unwrap();
        let block_count = output.matches("```rust").count();
        assert_eq!(block_count, 1); // merged into one block
    }

    #[test]
    fn test_format_default_context() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("test.rs"),
            "line1\nline2\nmatch_line\nline4\nline5",
        )
        .unwrap();
        let results = vec![SearchResult {
            file: "test.rs".into(),
            line_number: 3,
            line: "match_line".to_string(),
        }];
        let output = format_default_context(&results, root, 1, 1, false).unwrap();
        assert!(output.contains("test.rs-2-line2")); // context line uses -
        assert!(output.contains("test.rs:3:match_line")); // match line uses :
        assert!(output.contains("test.rs-4-line4")); // context line uses -
    }

    #[test]
    fn test_context_output_non_utf8_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("bin.rs"), b"ok1\n\xff\xfe target \xc3\n ok3\n").unwrap();
        let results = vec![SearchResult {
            file: "bin.rs".into(),
            line_number: 2,
            line: "\u{fffd}\u{fffd} target \u{fffd}".to_string(),
        }];
        let llm = format_llm(&results, root, 1, 1, None, false).unwrap();
        assert!(llm.contains("ok1"));
        assert!(llm.contains("\u{fffd}\u{fffd} target \u{fffd}"));
        assert!(llm.contains("ok3"));
        let ctx = format_default_context(&results, root, 1, 1, false).unwrap();
        assert!(ctx.contains("bin.rs-1-ok1"));
        assert!(ctx.contains("bin.rs:2:\u{fffd}\u{fffd} target \u{fffd}"));
    }

    #[test]
    fn test_context_output_unreadable_file_falls_back_to_matches() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("ok.rs"), "a\nhit\nb\n").unwrap();
        let results = vec![
            SearchResult {
                file: "gone.rs".into(),
                line_number: 5,
                line: "hit in deleted file".to_string(),
            },
            SearchResult {
                file: "ok.rs".into(),
                line_number: 2,
                line: "hit".to_string(),
            },
        ];
        let llm = format_llm(&results, root, 1, 1, None, false).unwrap();
        assert!(llm.contains("hit in deleted file"));
        assert!(llm.contains("## ok.rs:1-3"));
        let ctx = format_default_context(&results, root, 1, 1, false).unwrap();
        assert!(ctx.contains("gone.rs:5:hit in deleted file"));
        assert!(ctx.contains("ok.rs-1-a"));
    }

    #[test]
    fn test_format_default_context_many_matches_is_linear() {
        // 200k matching lines used to take seconds with -C 1 (O(n^2) lookup).
        let dir = tempdir().unwrap();
        let root = dir.path();
        let n = 200_000;
        let content: String = (1..=n).map(|i| format!("m{}\n", i)).collect();
        fs::write(root.join("big.txt"), content).unwrap();
        let results: Vec<SearchResult> = (1..=n)
            .map(|i| SearchResult {
                file: "big.txt".into(),
                line_number: i,
                line: format!("m{}", i),
            })
            .collect();
        let start = std::time::Instant::now();
        let output = format_default_context(&results, root, 1, 1, false).unwrap();
        let elapsed = start.elapsed();
        assert_eq!(output.lines().count(), n);
        assert!(output.lines().all(|l| l.starts_with("big.txt:")));
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "context output too slow: {:?}",
            elapsed
        );
    }

    #[test]
    fn test_format_default_context_marks_only_matches() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("t.rs"), "a\nm\nb\nc\nm\nd\n").unwrap();
        let results: Vec<SearchResult> = [2, 5]
            .iter()
            .map(|&i| SearchResult {
                file: "t.rs".into(),
                line_number: i,
                line: "m".to_string(),
            })
            .collect();
        let output = format_default_context(&results, root, 1, 1, false).unwrap();
        assert_eq!(
            output,
            "t.rs-1-a\nt.rs:2:m\nt.rs-3-b\nt.rs-4-c\nt.rs:5:m\nt.rs-6-d"
        );
    }

    fn result(file: &str, line_number: usize, line: &str) -> SearchResult {
        SearchResult {
            file: file.into(),
            line_number,
            line: line.to_string(),
        }
    }

    #[test]
    fn test_truncate_line() {
        assert_eq!(truncate_line("abcdef", None), "abcdef");
        assert_eq!(truncate_line("abcdef", Some(6)), "abcdef");
        assert_eq!(truncate_line("abcdef", Some(4)), "abcd…[+2 chars]");
        // Counts characters, not bytes, and never splits a character.
        assert_eq!(truncate_line("あいう", Some(3)), "あいう");
        assert_eq!(truncate_line("あいうえ", Some(2)), "あい…[+2 chars]");
        assert!(matches!(
            truncate_line("short", Some(300)),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn test_format_llm_line_numbers_and_match_marker() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let content: String = (1..=12).map(|i| format!("line{}\n", i)).collect();
        fs::write(root.join("t.rs"), content).unwrap();
        let results = vec![result("t.rs", 9, "line9"), result("t.rs", 10, "line10")];
        let output = format_llm(&results, root, 1, 1, None, false).unwrap();
        assert_eq!(
            output,
            "## t.rs:8-11\n\n```rust\n 8  line8\n 9> line9\n10> line10\n11  line11\n```\n"
        );
    }

    #[test]
    fn test_format_llm_fence_longer_than_backticks_in_content() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("doc.md"),
            "intro\n```rust\nlet target = 1;\n```\nmore ````` here\nend\n",
        )
        .unwrap();
        let results = vec![result("doc.md", 3, "let target = 1;")];
        let output = format_llm(&results, root, 2, 2, None, false).unwrap();
        // Longest run in the block is 5 backticks -> fence of 6.
        assert!(output.contains("\n``````markdown\n"), "{}", output);
        assert!(output.ends_with("\n``````\n"), "{}", output);

        // Without long runs in the shown block, the fence stays at 3.
        let output = format_llm(&results, root, 0, 0, None, false).unwrap();
        assert!(output.contains("\n```markdown\n3> let target = 1;\n```\n"));
    }

    #[test]
    fn test_format_llm_truncates_long_lines() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let minified = format!("var a=1;{}", "x".repeat(300_000));
        fs::write(root.join("app.min.js"), format!("{}\n", minified)).unwrap();
        let results = vec![result("app.min.js", 1, &minified)];
        let output = format_llm(&results, root, 3, 3, None, false).unwrap();
        assert!(output.len() < 1_000, "len = {}", output.len());
        assert!(output.contains(&format!("…[+{} chars]", 300_008 - 300)));

        // Explicitly unlimited keeps the whole line.
        let opts = LlmOptions {
            max_line_chars: None,
            ..LlmOptions::default()
        };
        let output = format_llm_with(&results, root, &opts);
        assert!(output.len() > 300_000);
    }

    #[test]
    fn test_format_llm_budget_holds_within_single_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let content: String = (1..=5000)
            .map(|i| format!("handler call number {}\n", i))
            .collect();
        fs::write(root.join("one.rs"), content).unwrap();
        let results: Vec<SearchResult> = (1..=5000)
            .map(|i| result("one.rs", i, &format!("handler call number {}", i)))
            .collect();
        let opts = LlmOptions {
            max_tokens: Some(500),
            ..LlmOptions::default()
        };
        let mut buf = Vec::new();
        let summary = write_llm(&mut buf, &results, root, &opts).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(
            estimate_tokens(&output) <= 500,
            "{}",
            estimate_tokens(&output)
        );
        assert!(summary.truncated);
        assert!(summary.shown_matches > 0);
        assert_eq!(summary.total_matches, 5000);
        assert!(output.contains(&format!(
            "... (truncated, {} more matches in 1 more files)",
            5000 - summary.shown_matches
        )));
        // The block that was cut is still closed.
        assert_eq!(output.matches("```").count() % 2, 0);
    }

    #[test]
    fn test_format_llm_budget_with_long_line_and_no_line_limit() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let minified = "y".repeat(300_000);
        fs::write(root.join("m.js"), &minified).unwrap();
        let results = vec![result("m.js", 1, &minified)];
        let opts = LlmOptions {
            max_tokens: Some(200),
            max_line_chars: None,
            ..LlmOptions::default()
        };
        let output = format_llm_with(&results, root, &opts);
        // The single line is shortened instead of blowing the budget.
        assert!(
            estimate_tokens(&output) <= 200,
            "{}",
            estimate_tokens(&output)
        );
        assert!(output.contains("1> yyy"));
        assert!(output.contains("chars]"));
    }

    #[test]
    fn test_format_llm_budget_not_truncated_when_fits() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.rs"), "x\nhit\ny\n").unwrap();
        let results = vec![result("a.rs", 2, "hit")];
        let opts = LlmOptions {
            max_tokens: Some(1000),
            ..LlmOptions::default()
        };
        let mut buf = Vec::new();
        let summary = write_llm(&mut buf, &results, root, &opts).unwrap();
        assert!(!summary.truncated);
        assert_eq!(summary.shown_matches, 1);
        assert!(!String::from_utf8(buf).unwrap().contains("truncated"));
    }

    #[test]
    fn test_write_default_max_columns_and_absolute() {
        let results = vec![result("a.rs", 3, "0123456789")];
        let opts = TextOptions {
            max_columns: Some(4),
            absolute_paths: true,
            ..TextOptions::default()
        };
        let mut buf = Vec::new();
        write_default(&mut buf, &results, Path::new("/r"), &opts).unwrap();
        let expected = format!(
            "{}:3:0123…[+6 chars]\n",
            Path::new("/r").join("a.rs").display()
        );
        assert_eq!(String::from_utf8(buf).unwrap(), expected);
    }

    #[test]
    fn test_write_default_context_max_columns() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("t.rs"), "context-line-long\nmatch-line-long\n").unwrap();
        let results = vec![result("t.rs", 2, "match-line-long")];
        let opts = TextOptions {
            before_context: 1,
            max_columns: Some(5),
            ..TextOptions::default()
        };
        let mut buf = Vec::new();
        write_default_context(&mut buf, &results, root, &opts).unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "t.rs-1-conte…[+12 chars]\nt.rs:2:match…[+10 chars]\n"
        );
    }

    #[test]
    fn test_format_default_empty() {
        let output = format_default(&[]);
        assert_eq!(output, "");
    }

    #[test]
    fn test_format_llm_empty() {
        let dir = tempdir().unwrap();
        let output = format_llm(&[], dir.path(), 3, 3, None, false).unwrap();
        assert_eq!(output, "");
    }

    #[test]
    fn test_format_llm_no_extension() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Makefile"), "all:\n\techo hello\n\techo done").unwrap();
        let results = vec![SearchResult {
            file: "Makefile".into(),
            line_number: 2,
            line: "\techo hello".to_string(),
        }];
        let output = format_llm(&results, root, 1, 1, None, false).unwrap();
        // No extension = empty language
        assert!(output.contains("```\n")); // no language after ```
        assert!(output.contains("echo hello"));
    }

    #[test]
    fn test_format_llm_context_zero() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("test.rs"), "line1\nline2\nline3\nline4\nline5").unwrap();
        let results = vec![SearchResult {
            file: "test.rs".into(),
            line_number: 3,
            line: "line3".to_string(),
        }];
        let output = format_llm(&results, root, 0, 0, None, false).unwrap();
        assert!(output.contains("line3"));
        assert!(!output.contains("line2")); // no context
        assert!(!output.contains("line4"));
    }

    #[test]
    fn test_format_default_context_empty() {
        let dir = tempdir().unwrap();
        let output = format_default_context(&[], dir.path(), 3, 3, false).unwrap();
        assert_eq!(output, "");
    }

    #[test]
    fn test_format_llm_max_tokens() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // Create multiple files so truncation can kick in between files
        let content: String = (1..=20).map(|i| format!("line number {}\n", i)).collect();
        fs::write(root.join("a.rs"), &content).unwrap();
        fs::write(root.join("b.rs"), &content).unwrap();
        fs::write(root.join("c.rs"), &content).unwrap();

        let mut results: Vec<SearchResult> = Vec::new();
        for file in &["a.rs", "b.rs", "c.rs"] {
            for i in (2..=10).step_by(2) {
                results.push(SearchResult {
                    file: (*file).into(),
                    line_number: i,
                    line: format!("line number {}", i),
                });
            }
        }

        // With very low token limit, should truncate after first file
        let output = format_llm(&results, root, 1, 1, Some(50), false).unwrap();
        assert!(output.contains("truncated"));
        assert!(output.contains("more matches"));
        assert!(output.contains("more files"));
    }

    #[test]
    fn test_estimate_tokens_ascii() {
        let text = "hello world this is a test";
        let tokens = estimate_tokens(text);
        // ~26 bytes / 4 = ~6-7 tokens
        assert!((5..=10).contains(&tokens));
    }

    #[test]
    fn test_estimate_tokens_cjk() {
        let text = "こんにちは世界";
        let tokens = estimate_tokens(text);
        // 21 bytes, all non-ASCII, / 2 = ~10-11 tokens
        assert!((8..=15).contains(&tokens));
    }

    #[test]
    fn test_estimate_tokens_mixed() {
        let text = "hello こんにちは";
        let tokens = estimate_tokens(text);
        assert!(tokens > 0);
    }

    #[test]
    fn test_estimate_tokens_empty() {
        assert_eq!(estimate_tokens(""), 1); // minimum 1
    }

    #[test]
    fn test_format_llm_no_token_limit() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.rs"), "line1\nline2\nline3").unwrap();

        let results = vec![SearchResult {
            file: "a.rs".into(),
            line_number: 2,
            line: "line2".to_string(),
        }];

        let output = format_llm(&results, root, 1, 1, None, false).unwrap();
        assert!(!output.contains("truncated"));
        assert!(output.contains("line2"));
    }

    #[test]
    fn test_format_default_context_asymmetric() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("test.rs"),
            "line1\nline2\nmatch_line\nline4\nline5",
        )
        .unwrap();
        let results = vec![SearchResult {
            file: "test.rs".into(),
            line_number: 3,
            line: "match_line".to_string(),
        }];
        // before=0, after=2: no lines before, two lines after
        let output = format_default_context(&results, root, 0, 2, false).unwrap();
        assert!(!output.contains("line2"));
        assert!(output.contains("test.rs:3:match_line"));
        assert!(output.contains("test.rs-4-line4"));
        assert!(output.contains("test.rs-5-line5"));
    }

    #[test]
    fn test_format_llm_asymmetric() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("test.rs"), "l1\nl2\nl3\nl4\nl5").unwrap();
        let results = vec![SearchResult {
            file: "test.rs".into(),
            line_number: 3,
            line: "l3".to_string(),
        }];
        // before=2, after=0
        let output = format_llm(&results, root, 2, 0, None, false).unwrap();
        assert!(output.contains("l1"));
        assert!(output.contains("l2"));
        assert!(output.contains("l3"));
        assert!(!output.contains("l4"));
    }

    #[test]
    fn test_format_llm_asymmetric_merge() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("test.rs"), "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8").unwrap();
        let results = vec![
            SearchResult {
                file: "test.rs".into(),
                line_number: 2,
                line: "l2".to_string(),
            },
            SearchResult {
                file: "test.rs".into(),
                line_number: 5,
                line: "l5".to_string(),
            },
        ];
        // before=0, after=3: match at 2 covers 2-5, match at 5 covers 5-8 -> one merged block 2-8
        let output = format_llm(&results, root, 0, 3, None, false).unwrap();
        let block_count = output.matches("```rust").count();
        assert_eq!(block_count, 1, "overlapping asymmetric ranges must merge");
        assert!(output.contains("## test.rs:2-8"));
        assert!(!output.contains("l1"));
    }
}
