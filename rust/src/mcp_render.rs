//! Rendering of ranked MCP results: pagination, token budgets, session
//! collapsing of already-shown ranges and the compact list modes.
//!
//! Content blocks are produced by [`crate::output::format_llm`] (one call per
//! file so the ranked file order is preserved); this module only decides
//! *which* matches go on the page so the response fits `max_tokens` and so
//! `next_offset` is exact.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use regex::Regex;

use crate::output;
use crate::search::SearchResult;
use crate::session::Session;

/// Lines longer than this are rendered as a window around the match instead
/// of being passed to the Markdown formatter (minified JS, data blobs).
pub const LONG_LINE_CHARS: usize = 500;
/// Width of the window shown for a long line.
const WINDOW_CHARS: usize = 300;

/// What the agent asked to receive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// Matching lines with context, Markdown code blocks.
    Content,
    /// File paths only (like `rg -l`), with match counts.
    Files,
    /// Per-file match counts (like `rg -c`).
    Count,
    /// `path: L10, L20-22` summaries without line text.
    Locations,
}

impl OutputMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "content" => Some(Self::Content),
            "files" | "files_with_matches" => Some(Self::Files),
            "count" => Some(Self::Count),
            "locations" => Some(Self::Locations),
            _ => None,
        }
    }
}

/// Estimate token count. ASCII ~4 bytes/token, non-ASCII ~2 bytes/token.
/// (Same heuristic as the CLI formatter.)
pub fn estimate_tokens(text: &str) -> usize {
    let ascii = text.bytes().filter(|b| b.is_ascii()).count();
    (ascii / 4) + ((text.len() - ascii) / 2) + 1
}

/// Rendering configuration.
pub struct RenderCfg<'a> {
    pub root: &'a Path,
    pub before: usize,
    pub after: usize,
    pub max_results: usize,
    pub offset: usize,
    pub max_tokens: usize,
    pub include_seen: bool,
    /// Used to centre the window on very long lines.
    pub highlight: Option<&'a Regex>,
    /// kept file -> identical files that were collapsed into it
    pub duplicates: &'a HashMap<String, Vec<String>>,
}

/// One rendered page.
#[derive(Debug, Default)]
pub struct Page {
    pub body: String,
    /// Index (into the ranked list) of the first shown unit.
    pub start: usize,
    /// Number of units (matches, or files for list modes) on this page.
    pub shown: usize,
    /// Total units available.
    pub total: usize,
    /// Offset to pass for the next page, if any.
    pub next_offset: Option<usize>,
    /// Number of matches collapsed as already shown.
    pub collapsed_seen: usize,
    /// True when even a single unit did not fit and the body was cut.
    pub hard_truncated: bool,
}

/// Pick a Markdown fence that cannot be closed by the content.
pub fn fence_for(content: &str) -> String {
    let mut longest = 0usize;
    let mut run = 0usize;
    for c in content.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat((longest + 1).max(3))
}

/// Cut a line to at most `max` chars, appending `…[+N chars]`.
pub fn truncate_line(line: &str, max: usize) -> String {
    let n = line.chars().count();
    if n <= max {
        return line.to_string();
    }
    let cut: String = line.chars().take(max).collect();
    format!("{}…[+{} chars]", cut, n - max)
}

/// A window of a long line centred on the first match.
fn window(line: &str, highlight: Option<&Regex>) -> String {
    let chars: Vec<char> = line.chars().collect();
    let total = chars.len();
    let byte_pos = highlight
        .and_then(|re| re.find(line))
        .map(|m| m.start())
        .unwrap_or(0);
    let pos = line[..byte_pos].chars().count();
    let start = pos.saturating_sub(WINDOW_CHARS / 3);
    let end = (start + WINDOW_CHARS).min(total);
    let mut s = String::new();
    if start > 0 {
        s.push_str(&format!("…[{} chars before] ", start));
    }
    s.extend(&chars[start..end]);
    if end < total {
        s.push_str(&format!(" …[+{} chars]", total - end));
    }
    s
}

fn lang_of(path: &str) -> &'static str {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(output::lang_from_ext)
        .unwrap_or("")
}

/// Fallback renderer: match lines only (no context), long lines windowed.
fn render_plain(file: &str, items: &[&SearchResult], highlight: Option<&Regex>) -> String {
    let mut out = String::new();
    for (i, r) in items.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let text = if r.line.chars().count() > LONG_LINE_CHARS {
            window(&r.line, highlight)
        } else {
            r.line.clone()
        };
        let fence = fence_for(&text);
        out.push_str(&format!(
            "## {}:{}\n\n{}{}\n{}\n{}\n",
            file,
            r.line_number,
            fence,
            lang_of(file),
            text,
            fence
        ));
    }
    out
}

struct Renderer<'a> {
    cfg: &'a RenderCfg<'a>,
    session: &'a Session,
    memo: HashMap<(Arc<str>, Vec<usize>), String>,
}

struct Rendered {
    text: String,
    marks: Vec<(Arc<str>, usize, usize)>,
    collapsed_seen: usize,
}

impl<'a> Renderer<'a> {
    fn render_file(&mut self, file: &Arc<str>, items: &[&SearchResult]) -> String {
        let key = (file.clone(), items.iter().map(|r| r.line_number).collect());
        if let Some(s) = self.memo.get(&key) {
            return s.clone();
        }
        let long = items
            .iter()
            .any(|r| r.line.chars().count() > LONG_LINE_CHARS);
        let text = if long {
            render_plain(file, items, self.cfg.highlight)
        } else {
            let owned: Vec<SearchResult> = items.iter().map(|r| (*r).clone()).collect();
            match output::format_llm(
                &owned,
                self.cfg.root,
                self.cfg.before,
                self.cfg.after,
                None,
                false,
            ) {
                Ok(s) if !s.is_empty() => s,
                _ => render_plain(file, items, self.cfg.highlight),
            }
        };
        self.memo.insert(key, text.clone());
        text
    }

    /// Render the first `p` ranked items.
    fn render(&mut self, items: &[SearchResult]) -> Rendered {
        let mut groups: Vec<(Arc<str>, Vec<&SearchResult>)> = Vec::new();
        let mut index: HashMap<Arc<str>, usize> = HashMap::new();
        for r in items {
            let i = *index.entry(r.file.clone()).or_insert_with(|| {
                groups.push((r.file.clone(), Vec::new()));
                groups.len() - 1
            });
            groups[i].1.push(r);
        }
        let mut out = Rendered {
            text: String::new(),
            marks: Vec::new(),
            collapsed_seen: 0,
        };
        for (file, mut group) in groups {
            group.sort_by_key(|r| r.line_number);
            group.dedup_by_key(|r| r.line_number);
            let (seen, fresh): (Vec<&SearchResult>, Vec<&SearchResult>) = if self.cfg.include_seen {
                (Vec::new(), group)
            } else {
                group
                    .into_iter()
                    .partition(|r| self.session.seen_range(&file, r.line_number).is_some())
            };
            if !out.text.is_empty() {
                out.text.push('\n');
            }
            if !fresh.is_empty() {
                out.text.push_str(&self.render_file(&file, &fresh));
                for r in &fresh {
                    out.marks.push((
                        file.clone(),
                        r.line_number.saturating_sub(self.cfg.before).max(1),
                        r.line_number + self.cfg.after,
                    ));
                }
            }
            if !seen.is_empty() {
                let mut ranges: Vec<(usize, usize)> = seen
                    .iter()
                    .filter_map(|r| self.session.seen_range(&file, r.line_number))
                    .collect();
                ranges.sort_unstable();
                ranges.dedup();
                let list: Vec<String> = ranges
                    .iter()
                    .map(|(s, e)| {
                        if s == e {
                            format!("L{}", s)
                        } else {
                            format!("L{}-{}", s, e)
                        }
                    })
                    .collect();
                out.text.push_str(&format!(
                    "already shown: {}:{} ({} match{})\n",
                    file,
                    list.join(","),
                    seen.len(),
                    if seen.len() == 1 { "" } else { "es" }
                ));
                out.collapsed_seen += seen.len();
            }
            if let Some(d) = self.cfg.duplicates.get(&*file) {
                out.text
                    .push_str(&format!("(identical content also in: {})\n", d.join(", ")));
            }
        }
        out
    }
}

/// Render matches (content mode) choosing the largest prefix of
/// `items[offset..]` that fits `max_tokens`, then record it in the session.
pub fn render_content(items: &[SearchResult], cfg: &RenderCfg, session: &mut Session) -> Page {
    let total = items.len();
    let start = cfg.offset.min(total);
    let avail = &items[start..];
    let pmax = cfg.max_results.max(1).min(avail.len());
    let mut page = Page {
        start,
        total,
        ..Default::default()
    };
    if pmax == 0 {
        return page;
    }
    let mut r = Renderer {
        cfg,
        session,
        memo: HashMap::new(),
    };
    let fits = |s: &str| estimate_tokens(s) <= cfg.max_tokens;
    let mut best = r.render(&avail[..pmax]);
    let mut p = pmax;
    if !fits(&best.text) {
        // Binary search the largest prefix that fits.
        let (mut lo, mut hi) = (0usize, pmax); // lo fits (0 trivially), hi does not
        let mut lo_render: Option<Rendered> = None;
        while hi - lo > 1 {
            let mid = (lo + hi) / 2;
            let rr = r.render(&avail[..mid]);
            if fits(&rr.text) {
                lo = mid;
                lo_render = Some(rr);
            } else {
                hi = mid;
            }
        }
        match lo_render {
            Some(rr) => {
                p = lo;
                best = rr;
            }
            None => {
                // Not even one match fits: show the first one, cut to budget.
                p = 1;
                best = r.render(&avail[..1]);
                if let Some(emitted) = hard_cut(&mut best.text, cfg.max_tokens) {
                    // Only the lines that survived the cut count as shown;
                    // the rest must stay visible to follow-up searches.
                    let file = avail[0].file.clone();
                    best.marks = emitted
                        .into_iter()
                        .map(|(s, e)| (file.clone(), s, e))
                        .collect();
                    page.hard_truncated = true;
                }
            }
        }
    }
    drop(r);
    for (f, s, e) in &best.marks {
        session.mark(f, *s, *e);
    }
    page.body = best.text;
    page.shown = p;
    page.collapsed_seen = best.collapsed_seen;
    if start + p < total {
        page.next_offset = Some(start + p);
    }
    page
}

/// Cut `text` to roughly `max_tokens` (appending a notice). Returns `None`
/// when the text already fits, otherwise the line ranges whose numbered
/// lines (`{n}> text` / `{n}  text`) were emitted in full before the cut, so
/// only those are recorded in the session.
fn hard_cut(text: &mut String, max_tokens: usize) -> Option<Vec<(usize, usize)>> {
    let max_chars = max_tokens.saturating_mul(4).max(200);
    if text.len() <= max_chars {
        return None;
    }
    let mut cut = max_chars;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
    let emitted = emitted_line_ranges(text);
    text.push_str(&format!(
        "\n…[block cut at max_tokens={}; use read_file with start_line/end_line]\n",
        max_tokens
    ));
    Some(emitted)
}

/// Line numbers of the complete numbered lines in `text`, as merged ranges.
/// A trailing line without its newline (cut mid-line) does not count.
fn emitted_line_ranges(text: &str) -> Vec<(usize, usize)> {
    let complete = match text.rfind('\n') {
        Some(i) => &text[..i],
        None => return Vec::new(),
    };
    let mut nums: Vec<usize> = complete
        .lines()
        .filter_map(|l| {
            let t = l.trim_start_matches(' ');
            let digits = t.bytes().take_while(u8::is_ascii_digit).count();
            if digits == 0 {
                return None;
            }
            match &t.as_bytes()[digits..] {
                [b'>' | b' ', b' ', ..] => t[..digits].parse().ok(),
                _ => None,
            }
        })
        .collect();
    nums.sort_unstable();
    nums.dedup();
    let mut out: Vec<(usize, usize)> = Vec::new();
    for n in nums {
        match out.last_mut() {
            Some(last) if n == last.1 + 1 => last.1 = n,
            _ => out.push((n, n)),
        }
    }
    out
}

/// Per-file aggregation in ranked order.
pub struct FileHits {
    pub file: Arc<str>,
    pub lines: Vec<usize>,
}

/// Group ranked matches by file, preserving the order of first appearance.
pub fn group_files(items: &[SearchResult]) -> Vec<FileHits> {
    let mut out: Vec<FileHits> = Vec::new();
    let mut index: HashMap<Arc<str>, usize> = HashMap::new();
    for r in items {
        let i = *index.entry(r.file.clone()).or_insert_with(|| {
            out.push(FileHits {
                file: r.file.clone(),
                lines: Vec::new(),
            });
            out.len() - 1
        });
        out[i].lines.push(r.line_number);
    }
    for f in &mut out {
        f.lines.sort_unstable();
        f.lines.dedup();
    }
    out
}

/// Compress sorted line numbers into `L3,L10-12` form (at most `cap` entries).
pub fn line_ranges(lines: &[usize], cap: usize) -> String {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for &l in lines {
        match ranges.last_mut() {
            Some(last) if l == last.1 + 1 => last.1 = l,
            _ => ranges.push((l, l)),
        }
    }
    let mut parts: Vec<String> = ranges
        .iter()
        .take(cap)
        .map(|(s, e)| {
            if s == e {
                format!("L{}", s)
            } else {
                format!("L{}-{}", s, e)
            }
        })
        .collect();
    if ranges.len() > cap {
        parts.push(format!("…(+{} more ranges)", ranges.len() - cap));
    }
    parts.join(",")
}

/// Render the list modes (files / count / locations). Pagination unit: files.
pub fn render_list(items: &[SearchResult], mode: OutputMode, cfg: &RenderCfg) -> Page {
    let files = group_files(items);
    let total = files.len();
    let start = cfg.offset.min(total);
    let mut page = Page {
        start,
        total,
        ..Default::default()
    };
    let mut used = 0usize;
    for f in files.iter().skip(start).take(cfg.max_results.max(1)) {
        let n = f.lines.len();
        let mut line = match mode {
            OutputMode::Files => format!(
                "{} ({} match{}, first L{})",
                f.file,
                n,
                if n == 1 { "" } else { "es" },
                f.lines[0]
            ),
            OutputMode::Count => format!("{}:{}", f.file, n),
            _ => format!("{}: {}", f.file, line_ranges(&f.lines, 40)),
        };
        if let Some(d) = cfg.duplicates.get(&*f.file) {
            line.push_str(&format!(" (identical: {})", d.join(", ")));
        }
        line.push('\n');
        let t = estimate_tokens(&line);
        if page.shown > 0 && used + t > cfg.max_tokens {
            break;
        }
        used += t;
        page.body.push_str(&line);
        page.shown += 1;
    }
    if start + page.shown < total {
        page.next_offset = Some(start + page.shown);
    }
    page
}

/// Blocks up to this many lines are shown whole; longer ones are elided to
/// the signature, windows around the matches and the closing line.
pub const BLOCK_FULL_LINES: usize = 80;
/// Lines shown around each match inside an elided block.
const BLOCK_WINDOW: usize = 2;
/// Line length cap inside blocks.
const BLOCK_LINE_CHARS: usize = 300;

/// Render one block (or its collapsed pointer). Returns text and shown ranges.
fn render_block(
    b: &crate::syntax::tools::Block,
    lines: &[String],
    duplicates: Option<&Vec<String>>,
) -> (String, Vec<(usize, usize)>) {
    let last = lines.len().max(1);
    let start = b.start.clamp(1, last);
    let end = b.end.clamp(start, last.max(start));
    let mut ranges: Vec<(usize, usize)> = if end - start < BLOCK_FULL_LINES {
        vec![(start, end)]
    } else {
        let mut r = vec![(start, b.head_end.clamp(start, (start + 5).min(end)))];
        for &m in &b.matches {
            r.push((
                m.saturating_sub(BLOCK_WINDOW).max(start),
                (m + BLOCK_WINDOW).min(end),
            ));
        }
        r.push((end, end));
        r.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for (s, e) in r {
            match merged.last_mut() {
                Some(l) if s <= l.1 + 1 => l.1 = l.1.max(e),
                _ => merged.push((s, e)),
            }
        }
        merged
    };
    ranges.retain(|(s, e)| s <= e);
    let body_lines: Vec<String> = {
        let width = end.to_string().len();
        let mut v = Vec::new();
        let mut prev: Option<usize> = None;
        for &(s, e) in &ranges {
            if let Some(p) = prev {
                if s > p + 1 {
                    v.push(format!("{:>width$}  ⋮ ({} lines)", "", s - p - 1));
                }
            }
            for n in s..=e {
                let text = lines.get(n - 1).map(|l| l.as_str()).unwrap_or("");
                let marker = if b.matches.binary_search(&n).is_ok() {
                    '>'
                } else {
                    ' '
                };
                v.push(format!(
                    "{:>width$}{} {}",
                    n,
                    marker,
                    truncate_line(text, BLOCK_LINE_CHARS)
                ));
            }
            prev = Some(e);
        }
        v
    };
    let body = body_lines.join("\n");
    let fence = fence_for(&body);
    let n = b.matches.len();
    let mut out = format!(
        "## {}:{}-{}{} ({} match{}{})\n\n{}{}\n{}\n{}\n",
        b.file,
        start,
        end,
        b.label
            .as_ref()
            .map(|l| format!(" {}", l))
            .unwrap_or_default(),
        n,
        if n == 1 { "" } else { "es" },
        if ranges.len() > 1 { ", elided" } else { "" },
        fence,
        lang_of(&b.file),
        body,
        fence
    );
    if let Some(d) = duplicates {
        out.push_str(&format!("(identical content also in: {})\n", d.join(", ")));
    }
    (out, ranges)
}

/// Render `expand:"block"` results: units are blocks, greedily packed into
/// `max_tokens`; blocks already shown in this session collapse to a pointer.
pub fn render_blocks(
    blocks: &[crate::syntax::tools::Block],
    cfg: &RenderCfg,
    session: &mut Session,
) -> Page {
    let total = blocks.len();
    let start = cfg.offset.min(total);
    let mut page = Page {
        start,
        total,
        ..Default::default()
    };
    let mut files: HashMap<Arc<str>, Vec<String>> = HashMap::new();
    let mut used = 0usize;
    let mut dup_noted: std::collections::HashSet<Arc<str>> = Default::default();
    for b in blocks.iter().skip(start).take(cfg.max_results.max(1)) {
        let collapsed = !cfg.include_seen
            && session
                .seen_range(&b.file, b.start)
                .is_some_and(|(_, e)| e >= b.end);
        let (mut text, mut marks) = if collapsed {
            (
                format!(
                    "already shown: {}:L{}-{} ({} match{})\n",
                    b.file,
                    b.start,
                    b.end,
                    b.matches.len(),
                    if b.matches.len() == 1 { "" } else { "es" }
                ),
                Vec::new(),
            )
        } else {
            let lines = files.entry(b.file.clone()).or_insert_with(|| {
                let bytes = std::fs::read(cfg.root.join(&*b.file)).unwrap_or_default();
                String::from_utf8_lossy(&bytes)
                    .lines()
                    .map(|l| l.to_string())
                    .collect()
            });
            let dups = if dup_noted.insert(b.file.clone()) {
                cfg.duplicates.get(&*b.file)
            } else {
                None
            };
            render_block(b, lines, dups)
        };
        if page.shown > 0 {
            text.insert(0, '\n');
        }
        let t = estimate_tokens(&text);
        if page.shown > 0 && used + t > cfg.max_tokens {
            break;
        }
        if page.shown == 0 && t > cfg.max_tokens {
            if let Some(emitted) = hard_cut(&mut text, cfg.max_tokens) {
                if !collapsed {
                    // Only the lines that survived the cut count as shown.
                    marks = emitted;
                }
                page.hard_truncated = true;
            }
        }
        used += estimate_tokens(&text);
        if collapsed {
            page.collapsed_seen += b.matches.len();
        }
        for (s, e) in marks {
            session.mark(&b.file, s, e);
        }
        page.body.push_str(&text);
        page.shown += 1;
        if page.hard_truncated {
            break;
        }
    }
    if start + page.shown < total {
        page.next_offset = Some(start + page.shown);
    }
    page
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fence_grows_with_content() {
        assert_eq!(fence_for("abc"), "```");
        assert_eq!(fence_for("x ``` y"), "````");
    }

    #[test]
    fn truncate_line_marks_rest() {
        assert_eq!(truncate_line("abcdef", 3), "abc…[+3 chars]");
        assert_eq!(truncate_line("abc", 3), "abc");
    }

    #[test]
    fn window_centres_on_match() {
        let line = format!("{}needle{}", "a".repeat(1000), "b".repeat(1000));
        let re = Regex::new("needle").unwrap();
        let w = window(&line, Some(&re));
        assert!(w.contains("needle"));
        assert!(w.len() < 400);
        assert!(w.starts_with("…["));
    }

    #[test]
    fn line_ranges_compress() {
        assert_eq!(line_ranges(&[1, 2, 3, 7, 9, 10], 10), "L1-3,L7,L9-10");
        assert_eq!(line_ranges(&[1, 3, 5], 2), "L1,L3,…(+1 more ranges)");
    }

    fn block(start: usize, end: usize, matches: Vec<usize>) -> crate::syntax::tools::Block {
        crate::syntax::tools::Block {
            file: Arc::from("a.rs"),
            start,
            end,
            head_end: start,
            label: Some("fn f".into()),
            matches,
        }
    }

    #[test]
    fn small_block_is_whole_and_marked() {
        let lines: Vec<String> = (1..=10).map(|i| format!("line {}", i)).collect();
        let (t, r) = render_block(&block(2, 5, vec![3]), &lines, None);
        assert!(t.starts_with("## a.rs:2-5 fn f (1 match)"), "{}", t);
        assert!(t.contains("3> line 3"), "{}", t);
        assert!(t.contains("2  line 2"), "{}", t);
        assert_eq!(r, vec![(2, 5)]);
    }

    #[test]
    fn long_block_is_elided() {
        let lines: Vec<String> = (1..=300).map(|i| format!("l{}", i)).collect();
        let (t, r) = render_block(&block(1, 300, vec![150]), &lines, None);
        assert!(t.contains("elided"), "{}", t);
        assert!(t.contains("150> l150"));
        assert!(t.contains("⋮"));
        assert!(!t.contains("l100\n"));
        assert_eq!(r, vec![(1, 1), (148, 152), (300, 300)]);
    }

    #[test]
    fn render_blocks_collapses_seen_and_pages() {
        let dir = tempfile::tempdir().unwrap();
        let content: String = (1..=20).map(|i| format!("x{}\n", i)).collect();
        std::fs::write(dir.path().join("a.rs"), content).unwrap();
        let blocks = vec![block(1, 5, vec![2]), block(10, 15, vec![12])];
        let dups = HashMap::new();
        let cfg = RenderCfg {
            root: dir.path(),
            before: 0,
            after: 0,
            max_results: 1,
            offset: 0,
            max_tokens: 4000,
            include_seen: false,
            highlight: None,
            duplicates: &dups,
        };
        let mut session = Session::new();
        let p = render_blocks(&blocks, &cfg, &mut session);
        assert_eq!(p.shown, 1);
        assert_eq!(p.next_offset, Some(1));
        let again = render_blocks(&blocks, &cfg, &mut session);
        assert!(
            again.body.starts_with("already shown: a.rs:L1-5"),
            "{}",
            again.body
        );
        assert_eq!(again.collapsed_seen, 1);
    }

    fn padded_file(dir: &Path, n: usize) {
        let content: String = (1..=n)
            .map(|i| format!("line{:03} {}\n", i, "p".repeat(30)))
            .collect();
        std::fs::write(dir.join("a.rs"), content).unwrap();
    }

    #[test]
    fn hard_cut_block_marks_only_emitted_lines() {
        let dir = tempfile::tempdir().unwrap();
        padded_file(dir.path(), 60);
        let blocks = vec![block(1, 60, vec![2])];
        let dups = HashMap::new();
        let cfg = RenderCfg {
            root: dir.path(),
            before: 0,
            after: 0,
            max_results: 1,
            offset: 0,
            max_tokens: 100,
            include_seen: false,
            highlight: None,
            duplicates: &dups,
        };
        let mut session = Session::new();
        let p = render_blocks(&blocks, &cfg, &mut session);
        assert!(p.hard_truncated, "{}", p.body);
        let (s, e) = session.seen_range("a.rs", 1).expect("head marked");
        assert_eq!(s, 1);
        assert!(e > 1 && e < 60, "marked up to {}", e);
        // The last emitted line is complete in the body; the next one is not.
        let full = |n: usize| format!("line{:03} {}\n", n, "p".repeat(30));
        assert!(p.body.contains(&full(e)), "{}", p.body);
        assert!(!p.body.contains(&full(e + 1)), "{}", p.body);
        assert_eq!(session.seen_range("a.rs", 60), None);
        // The block is not collapsed on a follow-up search.
        let again = render_blocks(&blocks, &cfg, &mut session);
        assert!(again.body.starts_with("## a.rs"), "{}", again.body);
        assert_eq!(again.collapsed_seen, 0);
    }

    #[test]
    fn hard_cut_content_marks_only_emitted_lines() {
        let dir = tempfile::tempdir().unwrap();
        padded_file(dir.path(), 60);
        let lines: Vec<String> = std::fs::read_to_string(dir.path().join("a.rs"))
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        let hit = |n: usize| SearchResult {
            file: Arc::from("a.rs"),
            line_number: n,
            line: lines[n - 1].clone(),
            column: 0,
        };
        let dups = HashMap::new();
        let mut cfg = RenderCfg {
            root: dir.path(),
            before: 29,
            after: 30,
            max_results: 1,
            offset: 0,
            max_tokens: 100,
            include_seen: false,
            highlight: None,
            duplicates: &dups,
        };
        let mut session = Session::new();
        let p = render_content(&[hit(30)], &cfg, &mut session);
        assert!(p.hard_truncated, "{}", p.body);
        assert!(session.seen_range("a.rs", 1).is_some());
        assert_eq!(session.seen_range("a.rs", 55), None);
        cfg.before = 0;
        cfg.after = 0;
        cfg.max_tokens = 4000;
        let again = render_content(&[hit(55)], &cfg, &mut session);
        assert!(!again.body.contains("already shown"), "{}", again.body);
        assert!(again.body.contains("line055"), "{}", again.body);
    }

    #[test]
    fn emitted_line_ranges_skips_partial_tail() {
        let t = "## a.rs:1-9\n\n```rust\n1  a\n2> b\n3  c\n4  cu";
        assert_eq!(emitted_line_ranges(t), vec![(1, 3)]);
        assert_eq!(emitted_line_ranges("no numbers\n"), Vec::new());
    }
}
