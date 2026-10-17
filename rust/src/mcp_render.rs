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
                let max_chars = cfg.max_tokens.saturating_mul(4).max(200);
                if best.text.len() > max_chars {
                    let mut cut = max_chars;
                    while !best.text.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    best.text.truncate(cut);
                    best.text.push_str(&format!(
                        "\n…[block cut at max_tokens={}; use read_file with start_line/end_line]\n",
                        cfg.max_tokens
                    ));
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
}
