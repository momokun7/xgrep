//! MCP handlers for the syntax tools (`outline`, `find_references`,
//! `repo_map`) and the block computation behind `search` `expand:"block"`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

use super::cache::{store_for, Stats};
use super::repo_map::{build_map, MapOptions};
use super::{display_sig, lang_for_path, line_span, supported_languages, FileTags, Kind};
use crate::defs::{DefinitionBackend, HeuristicBackend};
use crate::globfilter::GlobFilter;
use crate::mcp_tools::{
    build_filters, check_max_results, normalize_symbol, parse, plural, resolve_project_path,
    Filters, StrOrList,
};
use crate::rank::{self, is_comment_line, Scored};
use crate::{filetype, SearchOptions, SearchResult, Xgrep};

const OUTLINE_DEFAULT_TOKENS: usize = 4000;
const REFS_DEFAULT_MAX: usize = 50;
const REFS_DEFAULT_TOKENS: usize = 4000;
const MAP_DEFAULT_TOKENS: usize = 1500;
/// Context lines around a match that has no enclosing definition.
const FALLBACK_CONTEXT: usize = 3;
/// Reference line text cap.
const REF_LINE_CHARS: usize = 200;

/// Tags for `files` (relative to the root), persisting newly parsed entries.
pub fn tags_for(xg: &Xgrep, files: &[&str]) -> (Vec<Option<Arc<FileTags>>>, Stats) {
    let store = store_for(xg.index_path());
    let (t, s) = store.tags(xg.root(), files);
    if s.parsed > 0 {
        let _ = store.save();
    }
    (t, s)
}

/// Tags keyed by file for the files of `results`.
pub fn tags_by_file(xg: &Xgrep, results: &[SearchResult]) -> HashMap<Arc<str>, Arc<FileTags>> {
    let mut files: Vec<Arc<str>> = results.iter().map(|r| r.file.clone()).collect();
    files.sort_unstable();
    files.dedup();
    let refs: Vec<&str> = files.iter().map(|f| &**f).collect();
    let (tags, _) = tags_for(xg, &refs);
    files
        .into_iter()
        .zip(tags)
        .filter_map(|(f, t)| t.map(|t| (f, t)))
        .collect()
}

// ---------------------------------------------------------------------------
// Block expansion
// ---------------------------------------------------------------------------

/// A code block covering one or more matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub file: Arc<str>,
    pub start: usize,
    pub end: usize,
    /// Last line of the block head (signature) kept when the block is elided.
    pub head_end: usize,
    /// `fn name` style label; None for plain context windows.
    pub label: Option<String>,
    /// Matched lines (sorted, unique).
    pub matches: Vec<usize>,
}

/// Group ranked matches into the smallest enclosing definition blocks
/// (function, method, class, impl, ...). Matches outside any definition, or
/// in files without syntax support, get a ±3 line window. Blocks keep the
/// order of their best-ranked match.
pub fn expand_blocks(xg: &Xgrep, ranked: &[SearchResult]) -> (Vec<Block>, usize) {
    let tags = tags_by_file(xg, ranked);
    let mut blocks: Vec<Block> = Vec::new();
    let mut by_key: HashMap<(Arc<str>, usize, usize), usize> = HashMap::new();
    let mut syntax_files: HashSet<Arc<str>> = HashSet::new();
    for r in ranked {
        let line = r.line_number;
        let def = tags.get(&r.file).and_then(|t| {
            t.enclosing(line as u32, |s| s.kind != Kind::Declaration)
                .map(|i| (t, i))
        });
        let (start, end, head_end, label) = match def {
            Some((t, i)) => {
                syntax_files.insert(r.file.clone());
                let s = &t.defs[i];
                (
                    s.start as usize,
                    s.end as usize,
                    s.line as usize,
                    Some(format!("{} {}", s.kind.label(), t.qualified(i))),
                )
            }
            None => {
                let max = tags
                    .get(&r.file)
                    .map_or(usize::MAX, |t| t.lines.max(1) as usize);
                let s = line.saturating_sub(FALLBACK_CONTEXT).max(1);
                let e = (line + FALLBACK_CONTEXT).min(max.max(line));
                // merge into an overlapping window of the same file
                if let Some(b) = blocks.iter_mut().find(|b| {
                    b.label.is_none() && b.file == r.file && s <= b.end + 1 && b.start <= e + 1
                }) {
                    b.start = b.start.min(s);
                    b.end = b.end.max(e);
                    b.head_end = b.start;
                    if !b.matches.contains(&line) {
                        b.matches.push(line);
                        b.matches.sort_unstable();
                    }
                    continue;
                }
                (s, e, s, None)
            }
        };
        let key = (r.file.clone(), start, end);
        match by_key.get(&key) {
            Some(&i) => {
                let b = &mut blocks[i];
                if !b.matches.contains(&line) {
                    b.matches.push(line);
                    b.matches.sort_unstable();
                }
            }
            None => {
                by_key.insert(key, blocks.len());
                blocks.push(Block {
                    file: r.file.clone(),
                    start,
                    end,
                    head_end,
                    label,
                    matches: vec![line],
                });
            }
        }
    }
    (blocks, syntax_files.len())
}

// ---------------------------------------------------------------------------
// outline
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OutlineParams {
    path: StrOrList,
    max_depth: Option<usize>,
    max_tokens: Option<usize>,
}

/// Outline text for one file's tags.
pub fn render_outline(display: &str, lang: &str, t: &FileTags, max_depth: Option<usize>) -> String {
    let mut out = format!(
        "## {} ({}, {}, {})\n",
        display,
        lang,
        plural(t.lines as usize, "line"),
        plural(t.defs.len(), "symbol")
    );
    if t.defs.is_empty() {
        out.push_str("(no definitions found)\n");
        return out;
    }
    let mut hidden = 0usize;
    for (i, s) in t.defs.iter().enumerate() {
        let d = t.depth(i);
        if max_depth.is_some_and(|m| d > m) {
            hidden += 1;
            continue;
        }
        out.push_str(&format!(
            "{}{} {}\n",
            "  ".repeat(d),
            line_span(s.start.min(s.line), s.end),
            display_sig(s)
        ));
    }
    if hidden > 0 {
        out.push_str(&format!(
            "({} nested deeper than max_depth not shown)\n",
            plural(hidden, "symbol")
        ));
    }
    out
}

/// Handler for the `outline` tool.
pub fn handle_outline(xg: &Xgrep, params: &Value) -> (String, bool) {
    let p: OutlineParams = match parse(params) {
        Ok(v) => v,
        Err(e) => return (e, true),
    };
    let budget = p.max_tokens.unwrap_or(OUTLINE_DEFAULT_TOKENS).max(100);
    let paths = match p.path {
        StrOrList::One(s) => vec![s],
        StrOrList::Many(v) => v,
    };
    if paths.is_empty() {
        return ("path must not be empty".to_string(), true);
    }
    // Expand directories to their indexed, supported files.
    let mut files: Vec<String> = Vec::new();
    let mut dir_mode = false;
    for raw in &paths {
        let (canonical, display) = match resolve_project_path(xg, raw) {
            Ok(v) => v,
            Err(e) => return (e, true),
        };
        if canonical.is_dir() {
            dir_mode = true;
            let prefix = if display.is_empty() {
                String::new()
            } else {
                format!("{}/", display)
            };
            match xg.find_files("") {
                Ok(all) => files.extend(
                    all.into_iter()
                        .filter(|f| f.starts_with(&prefix) && lang_for_path(f).is_some()),
                ),
                Err(e) => return (format!("Find error: {}", e), true),
            }
        } else {
            if lang_for_path(&display).is_none() {
                return (
                    format!(
                        "outline does not support '{}'. Supported: {}. Use read_file or search instead.",
                        display,
                        supported_languages()
                    ),
                    true,
                );
            }
            files.push(display);
        }
    }
    if files.is_empty() {
        return (
            format!(
                "No files with syntax support under {}. Supported: {}.",
                paths.join(", "),
                supported_languages()
            ),
            false,
        );
    }
    // Directories default to top-level symbols only.
    let max_depth = p.max_depth.or(if dir_mode { Some(0) } else { None });
    let refs: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
    let (tags, _) = tags_for(xg, &refs);
    let mut out = String::new();
    let mut used = 0usize;
    let mut shown = 0usize;
    for (f, t) in files.iter().zip(&tags) {
        let Some(t) = t else {
            continue;
        };
        let lang = lang_for_path(f).map(|l| l.name()).unwrap_or("");
        let mut section = render_outline(f, lang, t, max_depth);
        let cost = crate::mcp_render::estimate_tokens(&section);
        if used + cost > budget {
            if shown == 0 {
                // Cut the single section at a line boundary.
                let mut acc = 0usize;
                let mut cut = String::new();
                for l in section.lines() {
                    let c = crate::mcp_render::estimate_tokens(l);
                    if acc + c > budget {
                        break;
                    }
                    acc += c;
                    cut.push_str(l);
                    cut.push('\n');
                }
                let kept = cut.lines().count().saturating_sub(1);
                cut.push_str(&format!(
                    "…[outline cut at max_tokens={}: {} of {} symbols shown; pass max_depth:0 for top-level only or a larger max_tokens]\n",
                    budget,
                    kept,
                    t.defs.len()
                ));
                section = cut;
                out.push_str(&section);
                shown += 1;
            }
            break;
        }
        used += cost;
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&section);
        shown += 1;
    }
    if shown < files.len() {
        out.push_str(&format!(
            "\n…[{} more files not shown (max_tokens={}); outline a narrower path]\n",
            files.len() - shown,
            budget
        ));
    }
    (out, false)
}

// ---------------------------------------------------------------------------
// find_references
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RefsParams {
    symbol: String,
    file_type: Option<String>,
    path_pattern: Option<String>,
    glob: Option<StrOrList>,
    exclude: Option<StrOrList>,
    #[serde(default)]
    include_definitions: bool,
    #[serde(default)]
    include_imports: bool,
    max_results: Option<usize>,
    #[serde(default)]
    offset: usize,
    max_tokens: Option<usize>,
    #[serde(default)]
    fresh: bool,
}

/// True for import / include / use lines.
pub fn is_import_line(path: &str, line: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r#"^\s*(?:(?:pub(?:\s*\([^)]*\))?\s+)?use\s|import\s|from\s+\S+\s+import\s|#\s*include\b|#\s*import\b|require(?:_relative)?[\s(]|using\s+[\w.:]+\s*;|extern\s+crate\s|package\s|export\s+(?:\*|\{[^}]*\})\s*from\s|(?:const|let|var)\s+[\w{}\s,:]+=\s*require\()"#,
        )
        .expect("valid import regex")
    });
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if matches!(ext, "md" | "markdown" | "rst" | "txt") {
        return false;
    }
    re.is_match(line)
}

struct RefHit {
    result: SearchResult,
    caller: Option<String>,
    comment: bool,
}

fn window_line(line: &str, re: &Regex) -> String {
    let t = line.trim();
    let n = t.chars().count();
    if n <= REF_LINE_CHARS {
        return t.to_string();
    }
    let pos = re
        .find(t)
        .map(|m| t[..m.start()].chars().count())
        .unwrap_or(0);
    let start = pos.saturating_sub(REF_LINE_CHARS / 3);
    let s: String = t.chars().skip(start).take(REF_LINE_CHARS).collect();
    format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        s,
        if start + REF_LINE_CHARS < n {
            "…"
        } else {
            ""
        }
    )
}

/// Handler for the `find_references` tool.
pub fn handle_find_references(xg: &Xgrep, params: &Value) -> (String, bool) {
    let p: RefsParams = match parse(params) {
        Ok(v) => v,
        Err(e) => return (e, true),
    };
    let mut notes = Vec::new();
    let symbol = match normalize_symbol(&p.symbol, &mut notes) {
        Ok(s) => s,
        Err(e) => return (e, true),
    };
    let filters = match build_filters(p.file_type, p.path_pattern, p.glob, p.exclude) {
        Ok(f) => f,
        Err(e) => return (e, true),
    };
    notes.extend(filters.notes.iter().cloned());
    let max_results = match check_max_results(p.max_results, REFS_DEFAULT_MAX) {
        Ok(v) => v,
        Err(e) => return (e, true),
    };
    let budget = p.max_tokens.unwrap_or(REFS_DEFAULT_TOKENS).max(100);
    let pattern = format!(r"\b{}\b", regex::escape(&symbol));
    let opts = SearchOptions {
        regex: true,
        file_type: filters.file_type.clone(),
        path_pattern: filters.path_pattern.clone(),
        globs: filters.globs.clone(),
        fresh: p.fresh,
        ..Default::default()
    };
    let results = match xg.search(&pattern, &opts) {
        Ok(r) => r,
        Err(e) => return (format!("Search error: {}", e), true),
    };
    let tags = tags_by_file(xg, &results);
    let heuristic = HeuristicBackend.compile(&symbol, false);
    let mut defs_skipped = 0usize;
    let mut imports_skipped = 0usize;
    let mut hits: Vec<RefHit> = Vec::new();
    for r in results {
        let t = tags.get(&r.file);
        let is_def = match t {
            Some(t) => t.defs.iter().any(|s| {
                s.name == symbol && s.line as usize == r.line_number && s.kind != Kind::Heading
            }),
            None => heuristic.classify(&r.file, &r.line).is_some(),
        };
        if is_def && !p.include_definitions {
            defs_skipped += 1;
            continue;
        }
        if !p.include_imports && is_import_line(&r.file, &r.line) {
            imports_skipped += 1;
            continue;
        }
        let caller = t.and_then(|t| {
            let line = r.line_number as u32;
            t.enclosing(line, |s| {
                s.kind.is_callable() && !(s.line == line && s.name == symbol)
            })
            .map(|i| t.qualified(i))
            .or_else(|| {
                t.enclosing(line, |s| s.kind.is_container() || s.kind == Kind::Heading)
                    .map(|i| format!("{} {}", t.defs[i].kind.label(), t.defs[i].name))
            })
        });
        let comment = is_comment_line(&r.file, &r.line);
        hits.push(RefHit {
            result: r,
            caller,
            comment,
        });
    }

    let mut head = String::new();
    for n in &notes {
        head.push_str(&format!("note: {}\n", n));
    }
    let excluded = format!(
        "excluded {} and {}",
        plural(defs_skipped, "definition line"),
        plural(imports_skipped, "import line")
    );
    if hits.is_empty() {
        head.push_str(&format!(
            "No references to `{}` ({}; case-sensitive whole-word match).\n",
            symbol, excluded
        ));
        head.push_str("hint: pass include_definitions/include_imports to see those lines; use search with case_insensitive:true for other spellings; retry with fresh:true for code written moments ago.\n");
        return (head, false);
    }

    // Caller summary (before ranking reorders things).
    let mut callers: HashMap<(String, Arc<str>), usize> = HashMap::new();
    for h in &hits {
        if let Some(c) = &h.caller {
            *callers
                .entry((c.clone(), h.result.file.clone()))
                .or_insert(0) += 1;
        }
    }
    let callers_n = callers.len();
    let mut callers: Vec<((String, Arc<str>), usize)> = callers.into_iter().collect();
    callers.sort_by(|a, b| {
        rank::path_score(&b.0 .1)
            .cmp(&rank::path_score(&a.0 .1))
            .then(b.1.cmp(&a.1))
            .then(a.0.cmp(&b.0))
    });

    let total = hits.len();
    let mut meta: HashMap<(Arc<str>, usize), (Option<String>, bool)> = HashMap::new();
    let scored: Vec<Scored> = hits
        .into_iter()
        .map(|h| {
            let mut score = rank::path_score(&h.result.file);
            if h.comment {
                score -= 30;
            }
            if h.caller.is_some() {
                score += 5;
            }
            meta.insert(
                (h.result.file.clone(), h.result.line_number),
                (h.caller, h.comment),
            );
            Scored {
                result: h.result,
                score,
            }
        })
        .collect();
    let files_n = {
        let mut f: Vec<&str> = scored.iter().map(|s| &*s.result.file).collect();
        f.sort_unstable();
        f.dedup();
        f.len()
    };
    let ranked = rank::order(scored);
    let hl = Regex::new(&pattern).expect("escaped pattern");

    // Page: group by file in ranked order, one line per reference.
    let start = p.offset.min(total);
    let mut body = String::new();
    let mut used = 0usize;
    let mut shown = 0usize;
    let mut cur_file: Option<Arc<str>> = None;
    for r in ranked.iter().skip(start).take(max_results) {
        let (caller, comment) = meta
            .get(&(r.file.clone(), r.line_number))
            .cloned()
            .unwrap_or((None, false));
        let mut chunk = String::new();
        if cur_file.as_ref() != Some(&r.file) {
            chunk.push_str(&format!("\n{}\n", r.file));
        }
        let ctx = match (&caller, comment) {
            (Some(c), false) => format!(" in {}", c),
            (Some(c), true) => format!(" in {} (comment)", c),
            (None, true) => " (comment)".to_string(),
            (None, false) => String::new(),
        };
        chunk.push_str(&format!(
            "  L{}{}: {}\n",
            r.line_number,
            ctx,
            window_line(&r.line, &hl)
        ));
        let c = crate::mcp_render::estimate_tokens(&chunk);
        if shown > 0 && used + c > budget {
            break;
        }
        used += c;
        body.push_str(&chunk);
        cur_file = Some(r.file.clone());
        shown += 1;
    }
    head.push_str(&format!(
        "Found {} to `{}` in {} ({}; whole-word, case-sensitive). Ranked: source, then tests, then docs.\n",
        plural(total, "reference"),
        symbol,
        plural(files_n, "file"),
        excluded
    ));
    if callers_n > 0 {
        let list: Vec<String> = callers
            .iter()
            .take(12)
            .map(|((c, f), n)| {
                if *n > 1 {
                    format!("{} ({} x{})", c, f, n)
                } else {
                    format!("{} ({})", c, f)
                }
            })
            .collect();
        head.push_str(&format!(
            "Callers ({}): {}{}\n",
            callers_n,
            list.join(", "),
            if callers_n > 12 { ", …" } else { "" }
        ));
    }
    if shown == 0 {
        head.push_str(&format!(
            "Showing nothing: offset {} is past the end ({} references).\n",
            p.offset, total
        ));
        return (head, false);
    }
    head.push_str(&format!(
        "Showing references {}-{} of {}",
        start + 1,
        start + shown,
        total
    ));
    if start + shown < total {
        head.push_str(&format!(
            "; more available: pass offset={} (next_offset={}).\n",
            start + shown,
            start + shown
        ));
    } else {
        head.push_str(" (end of results).\n");
    }
    head.push_str(&body);
    (head, false)
}

// ---------------------------------------------------------------------------
// repo_map
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MapParams {
    focus: Option<StrOrList>,
    focus_symbols: Option<StrOrList>,
    max_tokens: Option<usize>,
    file_type: Option<String>,
    path_pattern: Option<String>,
    glob: Option<StrOrList>,
    exclude: Option<StrOrList>,
}

fn apply_filters(paths: Vec<String>, f: &Filters) -> Result<Vec<String>, String> {
    let exts = f
        .file_type
        .as_deref()
        .and_then(filetype::extensions_for_type);
    let glob = if f.globs.is_empty() {
        None
    } else {
        Some(GlobFilter::new(&f.globs).map_err(|e| e.to_string())?)
    };
    Ok(paths
        .into_iter()
        .filter(|p| {
            exts.as_ref().is_none_or(|exts| {
                std::path::Path::new(p)
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| exts.contains(&e))
            }) && f
                .path_pattern
                .as_ref()
                .is_none_or(|pp| p.contains(pp.as_str()))
                && glob.as_ref().is_none_or(|g| g.matches(p))
        })
        .collect())
}

/// Handler for the `repo_map` tool.
pub fn handle_repo_map(xg: &Xgrep, params: &Value) -> (String, bool) {
    let p: MapParams = match parse(params) {
        Ok(v) => v,
        Err(e) => return (e, true),
    };
    if !cfg!(feature = "syntax") {
        return (
            format!("repo_map needs syntax support; {}", supported_languages()),
            true,
        );
    }
    let filters = match build_filters(p.file_type, p.path_pattern, p.glob, p.exclude) {
        Ok(f) => f,
        Err(e) => return (e, true),
    };
    let t0 = Instant::now();
    let all = match xg.find_files("") {
        Ok(v) => v,
        Err(e) => return (format!("Find error: {}", e), true),
    };
    // Prune cache entries of files that left the index (code files only;
    // vendored ones stay cached for find_definitions/find_references).
    {
        let indexed: HashSet<&str> = all
            .iter()
            .filter(|f| lang_for_path(f).is_some_and(|l| l.is_code()))
            .map(|s| s.as_str())
            .collect();
        store_for(xg.index_path()).retain_code(&indexed);
    }
    let code: Vec<String> = all
        .into_iter()
        .filter(|f| {
            lang_for_path(f).is_some_and(|l| l.is_code())
                && rank::path_class(f) != rank::PathClass::Vendor
        })
        .collect();
    let code = match apply_filters(code, &filters) {
        Ok(v) => v,
        Err(e) => return (e, true),
    };
    let refs: Vec<&str> = code.iter().map(|s| s.as_str()).collect();
    let store = store_for(xg.index_path());
    let (tags, stats) = store.tags(xg.root(), &refs);
    let _ = store.save();
    let parse_ms = t0.elapsed().as_millis();
    let files: Vec<(String, Arc<FileTags>)> = code
        .iter()
        .zip(tags)
        .filter_map(|(f, t)| t.map(|t| (f.clone(), t)))
        .collect();
    let focus = p.focus.map(|f| f.into_vec()).unwrap_or_default();
    let focus_symbols = p.focus_symbols.map(|f| f.into_vec()).unwrap_or_default();
    let mut notes: Vec<String> = filters.notes.clone();
    for f in &focus {
        let f = f.trim_start_matches("./").trim_end_matches('/');
        if !files
            .iter()
            .any(|(p, _)| p == f || (p.starts_with(f) && p[f.len()..].starts_with('/')))
        {
            notes.push(format!(
                "focus '{}' matches no supported source file; ignored",
                f
            ));
        }
    }
    let t1 = Instant::now();
    let map = build_map(
        &files,
        &MapOptions {
            focus: focus.clone(),
            focus_symbols,
            max_tokens: p.max_tokens.unwrap_or(MAP_DEFAULT_TOKENS),
        },
    );
    let rank_ms = t1.elapsed().as_millis();
    let mut out = String::new();
    for n in &notes {
        out.push_str(&format!("note: {}\n", n));
    }
    if files.is_empty() {
        out.push_str(&format!(
            "No source files with syntax support. Supported: {}.\n",
            supported_languages()
        ));
        return (out, false);
    }
    out.push_str(&format!(
        "Repo map: {} symbols in {} of {} files (~{} tokens), ranked by PageRank over {} definitions and {} reference edges{}. Lines are `L<line> <signature>`; nesting shows the enclosing impl/class. Use outline(path) for a whole file, find_references(symbol) for call sites.\n",
        map.shown_symbols,
        map.shown_files,
        map.files,
        map.tokens,
        map.defs,
        map.edges,
        if focus.is_empty() {
            String::new()
        } else {
            format!(", personalized to {}", focus.join(", "))
        }
    ));
    out.push_str(&format!(
        "(tags: {} parsed, {} cached, {} ms; ranking {} ms)\n\n",
        stats.parsed, stats.hits, parse_ms, rank_ms
    ));
    out.push_str(&map.text);
    (out, false)
}

// ---------------------------------------------------------------------------
// find_definitions backend
// ---------------------------------------------------------------------------

/// Which backend classified the definitions of one `find_definitions` call.
#[derive(Debug, Default, Clone, Copy)]
pub struct BackendUse {
    pub syntax_files: usize,
    pub regex_files: usize,
}

impl BackendUse {
    pub fn describe(&self) -> &'static str {
        match (self.syntax_files > 0, self.regex_files > 0) {
            (true, false) => "tree-sitter",
            (true, true) => "tree-sitter + regex fallback",
            _ => HeuristicBackend.name(),
        }
    }
}

/// Kind label + score for definitions of `symbol` among candidate lines.
/// Files with syntax support are classified from their tags (exact), others
/// with the regex heuristics.
pub fn classify_definitions(
    xg: &Xgrep,
    symbol: &str,
    candidates: Vec<SearchResult>,
) -> (Vec<(SearchResult, crate::defs::Definition)>, BackendUse) {
    let tags = tags_by_file(xg, &candidates);
    let compiled = HeuristicBackend.compile(symbol, false);
    let mut out = Vec::new();
    let mut syntax: HashSet<Arc<str>> = HashSet::new();
    let mut regex: HashSet<Arc<str>> = HashSet::new();
    for r in candidates {
        match tags.get(&r.file) {
            Some(t) if lang_for_path(&r.file).is_some_and(|l| l.is_code()) => {
                syntax.insert(r.file.clone());
                let best = t
                    .defs
                    .iter()
                    .filter(|s| s.name == symbol && s.line as usize == r.line_number)
                    .max_by_key(|s| s.kind.def_score());
                if let Some(s) = best {
                    out.push((
                        r,
                        crate::defs::Definition {
                            kind: s.kind.label(),
                            score: s.kind.def_score(),
                        },
                    ));
                }
            }
            _ => {
                if let Some(d) = compiled.classify(&r.file, &r.line) {
                    regex.insert(r.file.clone());
                    out.push((r, d));
                }
            }
        }
    }
    (
        out,
        BackendUse {
            syntax_files: syntax.len(),
            regex_files: regex.len(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_lines() {
        assert!(is_import_line("a.rs", "use crate::foo::Bar;"));
        assert!(is_import_line("a.rs", "pub(crate) use foo::Bar;"));
        assert!(is_import_line("a.py", "from x.y import Bar"));
        assert!(is_import_line("a.py", "import os"));
        assert!(is_import_line("a.ts", "import { Bar } from './bar';"));
        assert!(is_import_line("a.ts", "export { Bar } from './bar';"));
        assert!(is_import_line("a.js", "const { Bar } = require('./bar');"));
        assert!(is_import_line("a.c", "#include \"bar.h\""));
        assert!(is_import_line("a.rb", "require 'bar'"));
        assert!(is_import_line("A.java", "import com.x.Bar;"));
        assert!(!is_import_line("a.rs", "let used = Bar::new();"));
        assert!(!is_import_line("a.md", "import Bar"));
        assert!(!is_import_line("a.py", "x = imported(Bar)"));
    }
}
