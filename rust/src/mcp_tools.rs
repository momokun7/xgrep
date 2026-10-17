//! MCP tool handlers for xgrep search operations.
//!
//! Every handler returns `(text, is_error)`. Output is designed for agents:
//! results are ranked (definitions and source files first), paginated with an
//! exact total and `next_offset`, bounded by `max_tokens`, and ranges already
//! returned in this session are collapsed to one-line pointers.

use rayon::prelude::*;
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

use crate::defs::{DefinitionBackend, HeuristicBackend};
use crate::globfilter::GlobFilter;
use crate::mcp_render::{self, OutputMode, Page, RenderCfg};
use crate::rank::{self, QueryTerm, Scored};
use crate::safefs;
use crate::session::Session;
use crate::{filetype, output, SearchOptions, SearchResult, Xgrep};

const DEFAULT_MAX_RESULTS: usize = 20;
/// Default context for `search`. 0 keeps one match ≈ 4 lines of Markdown; with
/// 3 lines of context (the old default) a match costs ~10 lines and pages hold
/// 2-3x fewer results for the same token budget. Agents that need surrounding
/// code ask for `context_lines` or call `read_file` on the hit.
const DEFAULT_CONTEXT: usize = 0;
/// Default lines shown after a definition (signature continuation / first body lines).
const DEFAULT_DEF_CONTEXT: usize = 2;
const DEFAULT_MAX_TOKENS: usize = 4000;
const MAX_RESULTS_CAP: usize = 1000;
const MAX_CONTEXT: usize = 50;
const DEFAULT_FIND_FILES: usize = 50;
/// read_file caps.
const READ_MAX_LINES: usize = 2000;
const READ_DEFAULT_TOKENS: usize = 10_000;
const READ_LINE_CHARS: usize = 1000;
/// Largest file `read_file` loads (it reads the whole file to slice lines).
const READ_MAX_BYTES: u64 = 16 * 1024 * 1024;
/// Upper bound for any `max_tokens`, and for the number of `queries` items.
pub(crate) const MAX_TOKENS_CAP: usize = 100_000;
const MAX_QUERIES: usize = 20;
const OUTSIDE_ROOT: &str = "Error: path is outside the project root";
/// Minimum per-query budget when `queries` splits `max_tokens`.
const MIN_QUERY_TOKENS: usize = 400;

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

#[derive(Deserialize, Clone)]
#[serde(untagged)]
pub(crate) enum StrOrList {
    One(String),
    Many(Vec<String>),
}

impl StrOrList {
    pub(crate) fn into_vec(self) -> Vec<String> {
        match self {
            StrOrList::One(s) => vec![s],
            StrOrList::Many(v) => v,
        }
    }
}

#[derive(Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
struct SearchParams {
    pattern: Option<String>,
    queries: Option<Vec<Value>>,
    #[serde(default)]
    regex: bool,
    case_insensitive: Option<bool>,
    #[serde(default)]
    word: bool,
    file_type: Option<String>,
    path_pattern: Option<String>,
    glob: Option<StrOrList>,
    exclude: Option<StrOrList>,
    max_results: Option<usize>,
    #[serde(default)]
    offset: usize,
    context_lines: Option<usize>,
    max_tokens: Option<usize>,
    output_mode: Option<String>,
    mode: Option<String>,
    #[serde(default)]
    changed_only: bool,
    since: Option<String>,
    #[serde(default)]
    fresh: bool,
    #[serde(default)]
    include_seen: bool,
    expand: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FindDefinitionsParams {
    symbol: String,
    file_type: Option<String>,
    path_pattern: Option<String>,
    glob: Option<StrOrList>,
    exclude: Option<StrOrList>,
    max_results: Option<usize>,
    #[serde(default)]
    offset: usize,
    context_lines: Option<usize>,
    max_tokens: Option<usize>,
    output_mode: Option<String>,
    #[serde(default)]
    fresh: bool,
    #[serde(default)]
    include_seen: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FindFilesParams {
    pattern: String,
    file_type: Option<String>,
    glob: Option<StrOrList>,
    exclude: Option<StrOrList>,
    max_results: Option<usize>,
    #[serde(default)]
    offset: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadFileParams {
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
    max_tokens: Option<usize>,
}

pub(crate) fn parse<T: for<'de> Deserialize<'de>>(params: &Value) -> Result<T, String> {
    serde_json::from_value(params.clone()).map_err(|e| format!("Invalid parameters: {}", e))
}

// ---------------------------------------------------------------------------
// Tool definitions
// ---------------------------------------------------------------------------

fn filter_props() -> serde_json::Map<String, Value> {
    let v = serde_json::json!({
        "file_type": {
            "type": "string",
            "description": "Restrict to a language/file type name, e.g. rs, py, ts, go, md (a leading '.' is ignored). Unknown types are an error that lists valid ones."
        },
        "path_pattern": {
            "type": "string",
            "description": "Keep paths containing this plain substring (e.g. 'src/auth', 'tests/'). If it contains * ? or [ it is applied as a glob instead."
        },
        "glob": {
            "type": ["string", "array"],
            "items": {"type": "string"},
            "description": "Include glob(s), ripgrep -g semantics: '*.rs' matches at any depth, 'src/**/*.ts' is anchored at the root, prefix '!' to exclude ('!**/tests/**')."
        },
        "exclude": {
            "type": ["string", "array"],
            "items": {"type": "string"},
            "description": "Exclude glob(s); same as glob entries prefixed with '!'."
        }
    });
    match v {
        Value::Object(m) => m,
        _ => unreachable!(),
    }
}

fn paging_props() -> serde_json::Map<String, Value> {
    let v = serde_json::json!({
        "max_results": {
            "type": "integer",
            "description": "Results per page (default 20, max 1000). In files/count/locations mode this counts files."
        },
        "offset": {
            "type": "integer",
            "description": "Skip this many ranked results. The response header gives the exact total and next_offset when more remain."
        },
        "max_tokens": {
            "type": "integer",
            "description": "Approximate response budget in tokens (default 4000). The page is shrunk to fit; next_offset continues where it stopped."
        },
        "include_seen": {
            "type": "boolean",
            "description": "Repeat ranges already returned in this session instead of collapsing them to 'already shown: path:L10-40' (default false)."
        },
        "fresh": {
            "type": "boolean",
            "description": "Check index freshness and scan files changed since the last build directly (slower; use right after editing files)."
        }
    });
    match v {
        Value::Object(m) => m,
        _ => unreachable!(),
    }
}

fn merged(
    mut base: serde_json::Map<String, Value>,
    extra: Vec<serde_json::Map<String, Value>>,
) -> Value {
    for m in extra {
        base.extend(m);
    }
    Value::Object(base)
}

/// Return MCP tool definitions.
pub fn tools_list() -> Vec<Value> {
    let search_own = serde_json::json!({
        "pattern": {
            "type": "string",
            "description": "Literal text by default (backslashes match themselves). Set regex:true for Rust regex syntax."
        },
        "queries": {
            "type": "array",
            "items": {"type": "object"},
            "description": "Run several searches in one call (executed in parallel). Each item is an object with the same fields as this tool (pattern required); top-level fields are defaults for every item. max_tokens is split across items."
        },
        "regex": {"type": "boolean", "description": "Treat pattern as a regex (default false)."},
        "case_insensitive": {
            "type": "boolean",
            "description": "Force case (in)sensitivity. Default is smart-case: an all-lowercase ASCII pattern is case-insensitive, any uppercase makes it case-sensitive."
        },
        "word": {"type": "boolean", "description": "Match whole words only (\\b...\\b), e.g. 'id' does not match 'width'."},
        "output_mode": {
            "type": "string",
            "enum": ["content", "files", "count", "locations"],
            "description": "content (default): matching lines in Markdown blocks. files: ranked file list with match counts. count: path:count. locations: path: L10,L20-22 without line text (cheapest way to survey usages)."
        },
        "mode": {"type": "string", "enum": ["content", "files", "count", "locations"], "description": "Alias of output_mode."},
        "context_lines": {
            "type": "integer",
            "description": "Lines of context before and after each match (default 0, max 50). Prefer read_file on a hit when you need more."
        },
        "expand": {
            "type": "string",
            "enum": ["none", "block"],
            "description": "block: return the smallest enclosing function/method/class/impl of each match (tree-sitter) instead of single lines; matches in the same block are merged and marked with '>'. Long blocks keep the signature and windows around the matches. Pagination then counts blocks. Saves a read_file round-trip."
        },
        "changed_only": {"type": "boolean", "description": "Only search files with uncommitted git changes."},
        "since": {"type": "string", "description": "Only search files changed within a duration or commit count, e.g. '2h', '3d', '5.commits'."}
    });
    let Value::Object(search_own) = search_own else {
        unreachable!()
    };
    let search_props = merged(search_own, vec![filter_props(), paging_props()]);

    let def_own = serde_json::json!({
        "symbol": {
            "type": "string",
            "description": "Exact identifier (case-sensitive). 'Type::method' / 'obj.method' use the last segment."
        },
        "context_lines": {"type": "integer", "description": "Lines shown after each definition line (default 2)."},
        "output_mode": {"type": "string", "enum": ["content", "files", "count", "locations"], "description": "Same as search.output_mode."}
    });
    let Value::Object(def_own) = def_own else {
        unreachable!()
    };
    let def_props = merged(def_own, vec![filter_props(), paging_props()]);

    let refs_own = serde_json::json!({
        "symbol": {
            "type": "string",
            "description": "Exact identifier (case-sensitive). 'Type::method' / 'obj.method' use the last segment."
        },
        "include_definitions": {"type": "boolean", "description": "Also list the definition lines (default false)."},
        "include_imports": {"type": "boolean", "description": "Also list import/use/include lines (default false)."},
        "max_results": {"type": "integer", "description": "References per page (default 50)."},
        "offset": {"type": "integer", "description": "Skip this many ranked references."},
        "max_tokens": {"type": "integer", "description": "Approximate response budget (default 4000)."},
        "fresh": {"type": "boolean", "description": "Scan files changed since the last index build directly."}
    });
    let Value::Object(refs_own) = refs_own else {
        unreachable!()
    };
    let refs_props = merged(refs_own, vec![filter_props()]);

    let outline_props = serde_json::json!({
        "path": {
            "type": ["string", "array"],
            "items": {"type": "string"},
            "description": "File (or directory, or a list of them) relative to the project root."
        },
        "max_depth": {"type": "integer", "description": "Hide symbols nested deeper than this (0 = top level only). Default: all for files, 0 for directories."},
        "max_tokens": {"type": "integer", "description": "Approximate response budget (default 4000)."}
    });

    let map_own = serde_json::json!({
        "focus": {
            "type": ["string", "array"],
            "items": {"type": "string"},
            "description": "Files or directories to personalize the ranking to (e.g. the files you are editing)."
        },
        "focus_symbols": {
            "type": ["string", "array"],
            "items": {"type": "string"},
            "description": "Identifiers to weight up (x10), e.g. names mentioned in the task."
        },
        "max_tokens": {"type": "integer", "description": "Token budget for the map (default 1500)."}
    });
    let Value::Object(map_own) = map_own else {
        unreachable!()
    };
    let map_props = merged(map_own, vec![filter_props()]);

    let mut ff_filters = filter_props();
    ff_filters.remove("path_pattern");
    let ff_own = serde_json::json!({
        "pattern": {
            "type": "string",
            "description": "Case-insensitive substring of the path (e.g. 'mcp_tools', 'auth/'), or a glob if it contains * ? [ ('*.proto', 'src/**/test_*.py'; no '/' = any depth)."
        },
        "max_results": {"type": "integer", "description": "Paths per page (default 50)."},
        "offset": {"type": "integer", "description": "Skip this many ranked paths."}
    });
    let Value::Object(ff_own) = ff_own else {
        unreachable!()
    };
    let ff_props = merged(ff_own, vec![ff_filters]);

    vec![
        serde_json::json!({
            "name": "search",
            "description": "Find where text or a regex occurs (usages, call sites, strings, config keys). Trigram-indexed, so it is fast on large repos. Results are ranked (definition lines and source files before tests, docs, vendored and CHANGELOG files; comment lines last; identical files collapsed) and paginated: the header states the exact total, the shown range and next_offset. Smart-case by default. Use find_definitions instead when you want where a symbol is declared, find_files to locate files by name, and output_mode:'locations' or 'files' to survey many hits cheaply. Pass queries:[...] to run several searches in one call.",
            "inputSchema": {"type": "object", "properties": search_props}
        }),
        serde_json::json!({
            "name": "find_definitions",
            "description": "Find where a symbol is defined (fn/struct/class/def/func incl. Go receiver methods, C/Java 'type name(', TS/JS class methods and top-level consts, Rust impl blocks and macro_rules!, C #define, Python def/class). Local variables, comment lines and Markdown are excluded; results are ranked source-first. Heuristic (regex per language, not a parser): verify with read_file when it matters. For usages/call sites use search with word:true.",
            "inputSchema": {"type": "object", "properties": def_props, "required": ["symbol"]}
        }),
        serde_json::json!({
            "name": "find_references",
            "description": "Find usages of an identifier: whole-word, case-sensitive matches with definition lines and import/use/include lines removed, each annotated with the enclosing function ('in Server.handle') so you see the callers. Ranked source, then tests, then docs; the header lists the distinct callers. Uses tree-sitter for Rust, Python, JS/TS, Go, Java, C/C++, Ruby (regex fallback elsewhere). Text-based, not type-aware: same-named symbols are not told apart.",
            "inputSchema": {"type": "object", "properties": refs_props, "required": ["symbol"]}
        }),
        serde_json::json!({
            "name": "outline",
            "description": "List the symbols of a file without bodies: kind, signature, line range and nesting (methods under their class/impl). Much cheaper than read_file for getting the shape of a file; then read_file a line range. A directory path outlines every supported file under it (top-level symbols only unless max_depth is set). Markdown files list their headings.",
            "inputSchema": {"type": "object", "properties": outline_props, "required": ["path"]}
        }),
        serde_json::json!({
            "name": "repo_map",
            "description": "Aider-style map of the repository: the most important symbols (by PageRank over the cross-file definition/reference graph) with their signatures, grouped by file, within a token budget (default ~1500). Use it first in an unfamiliar codebase. Pass focus (files/directories you are working on) to rank what they use and what uses them; focus_symbols to boost identifiers from the task.",
            "inputSchema": {"type": "object", "properties": map_props}
        }),
        serde_json::json!({
            "name": "find_files",
            "description": "Locate files by name or path from the index (like 'xg --find' / fd): substring or glob. Ranked: exact file-name matches first, then source before tests/docs. Use this instead of searching file contents when you know (part of) a file name.",
            "inputSchema": {"type": "object", "properties": ff_props, "required": ["pattern"]}
        }),
        serde_json::json!({
            "name": "read_file",
            "description": "Read a file with line numbers. Default cap: 2000 lines or ~10k tokens; the header gives the total line count and, when cut, the next start_line. Lines longer than 1000 chars are shortened. Binary files (NUL bytes) and paths inside .git/ or outside the project are refused. Prefer a start_line/end_line window around a search hit over reading whole files.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path relative to the project root."},
                    "start_line": {"type": "integer", "description": "First line, 1-based (default 1). Past the end of the file is an error."},
                    "end_line": {"type": "integer", "description": "Last line, inclusive (default: start_line + 1999). Clamped to the file length."},
                    "max_tokens": {"type": "integer", "description": "Approximate budget (default 10000)."}
                },
                "required": ["path"]
            }
        }),
        serde_json::json!({
            "name": "index_status",
            "description": "Check the status of the search index. Returns a JSON object: state (\"fresh\", \"stale\", or \"missing\"), changed_files (count, present only when state is \"stale\"), indexed_files (count), index_size_bytes, index_path, and oversize_files ({count, limit_bytes, examples}, present only when files over the max file size were left out of the index).",
            "inputSchema": {"type": "object", "properties": {}}
        }),
        serde_json::json!({
            "name": "build_index",
            "description": "Build or rebuild the search index for the codebase. Searches auto-build a missing index; call this after large changes (branch switch, mass edits).",
            "inputSchema": {"type": "object", "properties": {}}
        }),
        serde_json::json!({
            "name": "session_reset",
            "description": "Forget which file ranges were already returned in this session, so later results are shown in full again instead of 'already shown: path:L10-40'.",
            "inputSchema": {"type": "object", "properties": {}}
        }),
    ]
}

// ---------------------------------------------------------------------------
// Unknown arguments
// ---------------------------------------------------------------------------

/// Drop arguments that `tool`'s input schema does not declare (also inside
/// `search` `queries` items) so a typo does not fail the whole call.
/// Returns the cleaned arguments and, if anything was dropped, a one-line
/// `note: ignored unknown argument(s): ...` with "did you mean" hints.
/// Unknown tools and non-object arguments are returned unchanged.
pub(crate) fn strip_unknown_args(tool: &str, args: &Value) -> (Value, Option<String>) {
    let Value::Object(map) = args else {
        return (args.clone(), None);
    };
    if map.is_empty() {
        return (args.clone(), None);
    }
    let tools = tools_list();
    let Some(props) = tools
        .iter()
        .find(|t| t["name"] == tool)
        .and_then(|t| t["inputSchema"]["properties"].as_object())
    else {
        return (args.clone(), None);
    };
    let known: Vec<&str> = props.keys().map(String::as_str).collect();
    let item_known: Vec<&str> = known.iter().copied().filter(|k| *k != "queries").collect();
    let mut ignored: Vec<String> = Vec::new();
    let mut out = serde_json::Map::new();
    for (k, v) in map {
        if !known.contains(&k.as_str()) {
            ignored.push(describe_unknown(k, k, &known));
            continue;
        }
        let v = match v {
            Value::Array(items) if k == "queries" => Value::Array(
                items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| match item {
                        Value::Object(m) => Value::Object(
                            m.iter()
                                .filter(|(ik, _)| {
                                    let ok = item_known.contains(&ik.as_str());
                                    if !ok {
                                        let label = format!("queries[{}].{}", i, ik);
                                        ignored.push(describe_unknown(&label, ik, &item_known));
                                    }
                                    ok
                                })
                                .map(|(a, b)| (a.clone(), b.clone()))
                                .collect(),
                        ),
                        other => other.clone(),
                    })
                    .collect(),
            ),
            other => other.clone(),
        };
        out.insert(k.clone(), v);
    }
    if ignored.is_empty() {
        return (args.clone(), None);
    }
    let note = format!("note: ignored unknown argument(s): {}", ignored.join(", "));
    (Value::Object(out), Some(note))
}

/// `label`, plus ` (did you mean x?)` when a known name is close to `name`.
fn describe_unknown(label: &str, name: &str, known: &[&str]) -> String {
    match suggest_arg(name, known) {
        Some(s) => format!("{} (did you mean {}?)", label, s),
        None => label.to_string(),
    }
}

/// Closest known argument name: common aliases, same name ignoring case and
/// `_`/`-`, a prefix (`context` -> `context_lines`), or a small edit distance.
fn suggest_arg<'a>(name: &str, known: &[&'a str]) -> Option<&'a str> {
    let norm = |s: &str| -> String {
        s.chars()
            .filter(|c| *c != '_' && *c != '-')
            .flat_map(char::to_lowercase)
            .collect()
    };
    let n = norm(name);
    let pick = |want: &str| known.iter().copied().find(|k| *k == want);
    let alias = match n.as_str() {
        "limit" | "max" | "maxcount" | "headlimit" => Some("max_results"),
        "type" | "lang" | "language" | "filetypes" | "ext" | "extension" => Some("file_type"),
        "query" | "q" | "regexp" | "text" | "needle" => Some("pattern"),
        "ignorecase" | "i" | "nocase" => Some("case_insensitive"),
        "context" | "c" | "contextlines" | "lines" => Some("context_lines"),
        "path" | "paths" | "dir" | "directory" | "file" | "files" | "filepath" => {
            pick("path").map(|_| "path").or(Some("path_pattern"))
        }
        "globs" | "include" => Some("glob"),
        "excludes" | "ignore" => Some("exclude"),
        "start" | "from" | "line" => Some("start_line"),
        "end" | "to" => Some("end_line"),
        "name" | "identifier" => Some("symbol"),
        "page" | "skip" => Some("offset"),
        _ => None,
    };
    if let Some(a) = alias.and_then(pick) {
        return Some(a);
    }
    if let Some(k) = known.iter().copied().find(|k| norm(k) == n) {
        return Some(k);
    }
    if n.len() >= 4 {
        if let Some(k) = known.iter().copied().find(|k| norm(k).starts_with(&n)) {
            return Some(k);
        }
    }
    let limit = if n.len() >= 6 { 2 } else { 1 };
    let close = known
        .iter()
        .copied()
        .map(|k| (osa_distance(&n, &norm(k)), k))
        .filter(|(d, _)| *d <= limit)
        .min_by_key(|(d, _)| *d)
        .map(|(_, k)| k);
    if close.is_some() || n.len() < 4 {
        return close;
    }
    // A typo in a prefix: `paht` -> `path_pattern`.
    known.iter().copied().find(|k| {
        let kn = norm(k);
        let head: String = kn.chars().take(n.chars().count()).collect();
        osa_distance(&n, &head) <= 1
    })
}

/// Optimal string alignment distance (Levenshtein + adjacent transposition).
fn osa_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = v;
        }
    }
    d[a.len()][b.len()]
}

// ---------------------------------------------------------------------------
// Shared validation
// ---------------------------------------------------------------------------

#[derive(Default)]
pub(crate) struct Filters {
    pub(crate) file_type: Option<String>,
    pub(crate) path_pattern: Option<String>,
    pub(crate) globs: Vec<String>,
    pub(crate) notes: Vec<String>,
}

impl Filters {
    pub(crate) fn is_active(&self) -> bool {
        self.file_type.is_some() || self.path_pattern.is_some() || !self.globs.is_empty()
    }
}

fn valid_type_names() -> String {
    filetype::list_all_types()
        .iter()
        .map(|(n, _)| *n)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Normalise and validate a file_type value.
fn normalize_file_type(ft: &str, notes: &mut Vec<String>) -> Result<String, String> {
    let raw = ft.trim();
    let norm = raw
        .trim_start_matches("*.")
        .trim_start_matches('.')
        .to_string();
    if norm.is_empty() || filetype::extensions_for_type(&norm).is_none() {
        return Err(format!(
            "Unknown file_type '{}'. Valid types: {}. (To filter by an arbitrary extension use glob:'*.ext'.)",
            ft,
            valid_type_names()
        ));
    }
    if norm != raw {
        notes.push(format!("file_type '{}' read as '{}'", raw, norm));
    }
    Ok(norm)
}

fn looks_like_glob(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[')
}

pub(crate) fn build_filters(
    file_type: Option<String>,
    path_pattern: Option<String>,
    glob: Option<StrOrList>,
    exclude: Option<StrOrList>,
) -> Result<Filters, String> {
    let mut f = Filters::default();
    if let Some(ft) = file_type {
        f.file_type = Some(normalize_file_type(&ft, &mut f.notes)?);
    }
    if let Some(g) = glob {
        f.globs.extend(g.into_vec());
    }
    if let Some(e) = exclude {
        f.globs.extend(e.into_vec().into_iter().map(|g| {
            if g.starts_with('!') {
                g
            } else {
                format!("!{}", g)
            }
        }));
    }
    if let Some(pp) = path_pattern {
        let pp = pp.trim_start_matches("./").to_string();
        if looks_like_glob(&pp) {
            f.notes.push(format!(
                "path_pattern '{}' contains wildcards, so it was applied as a glob (path_pattern itself is a plain substring)",
                pp
            ));
            f.globs.push(pp);
        } else if !pp.is_empty() {
            f.path_pattern = Some(pp);
        }
    }
    if !f.globs.is_empty() {
        GlobFilter::new(&f.globs).map_err(|e| e.to_string())?;
    }
    Ok(f)
}

fn parse_mode(output_mode: Option<&str>, mode: Option<&str>) -> Result<OutputMode, String> {
    let chosen = match (output_mode, mode) {
        (Some(a), Some(b)) if a != b => {
            return Err(format!(
                "output_mode '{}' conflicts with mode '{}'; pass only one",
                a, b
            ))
        }
        (Some(a), _) => Some(a),
        (None, b) => b,
    };
    match chosen {
        None => Ok(OutputMode::Content),
        Some(s) => OutputMode::parse(s).ok_or_else(|| {
            format!(
                "Unknown output_mode '{}'. Valid: content, files, count, locations",
                s
            )
        }),
    }
}

pub(crate) fn check_max_results(v: Option<usize>, default: usize) -> Result<usize, String> {
    match v {
        None => Ok(default),
        Some(0) => Err("max_results must be at least 1".to_string()),
        Some(n) => Ok(n.min(MAX_RESULTS_CAP)),
    }
}

fn is_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

/// `Type::method` / `obj.method` / `Class#method` -> `method`; must be an identifier.
pub(crate) fn normalize_symbol(raw: &str, notes: &mut Vec<String>) -> Result<String, String> {
    let raw = raw.trim();
    let symbol = raw
        .rsplit("::")
        .next()
        .unwrap_or(raw)
        .rsplit(['.', '#'])
        .next()
        .unwrap_or(raw)
        .to_string();
    if symbol.is_empty() {
        return Err("symbol must not be empty".to_string());
    }
    if symbol != raw {
        notes.push(format!("symbol '{}' read as '{}'", raw, symbol));
    }
    if !is_identifier(&symbol) {
        return Err(format!(
            "symbol '{}' is not an identifier; use search (optionally regex:true) for arbitrary text",
            symbol
        ));
    }
    Ok(symbol)
}

/// Resolve a project-relative path safely: it must exist, stay inside the
/// root and not be inside `.git/`. Returns (canonical path, display path
/// relative to the root with `/` separators; empty for the root itself).
pub(crate) fn resolve_project_path(
    xg: &Xgrep,
    path: &str,
) -> Result<(std::path::PathBuf, String), String> {
    // Reject absolute, drive, UNC and `..` paths before touching the
    // filesystem: on Windows, resolving a UNC path already contacts the server.
    if !safefs::is_contained_rel(std::path::Path::new(path)) {
        return Err(OUTSIDE_ROOT.to_string());
    }
    let full_path = xg.root().join(path);
    let canonical = full_path.canonicalize().map_err(|e| {
        format!(
            "Cannot read file '{}': {}. Use find_files to locate files by name.",
            path, e
        )
    })?;
    let root_canonical = xg
        .root()
        .canonicalize()
        .map_err(|e| format!("Cannot resolve root: {}", e))?;
    let Ok(rel) = canonical.strip_prefix(&root_canonical) else {
        return Err(OUTSIDE_ROOT.to_string());
    };
    if rel.components().any(|c| c.as_os_str() == ".git") {
        return Err(format!(
            "Refusing to read '{}': files inside .git/ are not project sources.",
            path
        ));
    }
    let display = rel.to_string_lossy().replace('\\', "/");
    Ok((canonical, display))
}

fn count_files(results: &[SearchResult]) -> usize {
    let mut files: Vec<&str> = results.iter().map(|r| &*r.file).collect();
    files.sort_unstable();
    files.dedup();
    files.len()
}

// ---------------------------------------------------------------------------
// search
// ---------------------------------------------------------------------------

struct Prepared {
    pattern: String,
    regex: bool,
    case_insensitive: bool,
    case_desc: &'static str,
    word: bool,
    filters: Filters,
    changed_only: bool,
    since: Option<String>,
    fresh: bool,
    mode: OutputMode,
    max_results: usize,
    offset: usize,
    context: usize,
    max_tokens: usize,
    include_seen: bool,
    /// `expand: "block"`: return enclosing definitions instead of lines.
    expand_block: bool,
}

impl Prepared {
    fn options(&self) -> SearchOptions {
        SearchOptions {
            case_insensitive: self.case_insensitive,
            regex: self.regex,
            word: self.word,
            file_type: self.filters.file_type.clone(),
            path_pattern: self.filters.path_pattern.clone(),
            globs: self.filters.globs.clone(),
            changed_only: self.changed_only,
            since: self.since.clone(),
            fresh: self.fresh,
            max_count: None,
            ..Default::default()
        }
    }

    /// Regex locating the match inside a line (used to window long lines).
    fn highlight(&self) -> Option<Regex> {
        let mut src = if self.regex {
            self.pattern.clone()
        } else {
            regex::escape(&self.pattern)
        };
        if self.word {
            src = format!(r"\b(?:{})\b", src);
        }
        if self.case_insensitive {
            src = format!("(?i){}", src);
        }
        Regex::new(&src).ok()
    }
}

fn prepare(p: SearchParams) -> Result<Prepared, String> {
    let pattern = p
        .pattern
        .ok_or("Invalid parameters: missing field `pattern` (or pass queries:[{pattern:...}])")?;
    if pattern.is_empty() {
        return Err("pattern must not be empty".to_string());
    }
    if p.regex {
        if let Err(e) = Regex::new(&pattern) {
            return Err(format!("Invalid regex: {}", e));
        }
    }
    let (case_insensitive, case_desc) = match p.case_insensitive {
        Some(true) => (true, "case-insensitive"),
        Some(false) => (false, "case-sensitive"),
        None => {
            if !pattern.is_ascii() {
                (false, "case-sensitive: non-ASCII pattern")
            } else if crate::pattern_has_uppercase(&pattern, p.regex) {
                (false, "smart-case: case-sensitive")
            } else {
                (true, "smart-case: case-insensitive")
            }
        }
    };
    let filters = build_filters(p.file_type, p.path_pattern, p.glob, p.exclude)?;
    let mode = parse_mode(p.output_mode.as_deref(), p.mode.as_deref())?;
    let expand_block = match p.expand.as_deref() {
        None | Some("none") | Some("line") => false,
        Some("block") => true,
        Some(other) => {
            return Err(format!(
                "Unknown expand '{}'. Valid: block (enclosing function/class/impl), none",
                other
            ))
        }
    };
    if expand_block && mode != OutputMode::Content {
        return Err("expand:'block' only applies to output_mode 'content'".to_string());
    }
    Ok(Prepared {
        pattern,
        regex: p.regex,
        case_insensitive,
        case_desc,
        word: p.word,
        filters,
        changed_only: p.changed_only,
        since: p.since,
        fresh: p.fresh,
        mode,
        max_results: check_max_results(p.max_results, DEFAULT_MAX_RESULTS)?,
        offset: p.offset,
        context: p.context_lines.unwrap_or(DEFAULT_CONTEXT).min(MAX_CONTEXT),
        max_tokens: p
            .max_tokens
            .unwrap_or(DEFAULT_MAX_TOKENS)
            .clamp(50, MAX_TOKENS_CAP),
        include_seen: p.include_seen,
        expand_block,
    })
}

fn rank_search(results: Vec<SearchResult>, prep: &Prepared) -> Vec<SearchResult> {
    let q = QueryTerm::new(&prep.pattern, prep.regex);
    let defs = (!prep.regex && is_identifier(&prep.pattern))
        .then(|| HeuristicBackend.compile(&prep.pattern, prep.case_insensitive));
    let scored: Vec<Scored> = results
        .into_iter()
        .map(|r| {
            let mut score = rank::path_score(&r.file) + q.filename_bonus(&r.file);
            if rank::is_comment_line(&r.file, &r.line) {
                score -= 30;
            } else if let Some(d) = defs.as_ref().and_then(|d| d.classify(&r.file, &r.line)) {
                score += d.score;
            }
            Scored { result: r, score }
        })
        .collect();
    rank::order(scored)
}

pub(crate) fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("{} {}", n, word)
    } else if word.ends_with('h') {
        format!("{} {}es", n, word)
    } else {
        format!("{} {}s", n, word)
    }
}

/// "Showing ..." line shared by search and find_definitions.
fn page_line(page: &Page, unit: &str) -> String {
    let mut s = if page.shown == 0 {
        format!(
            "Showing nothing: offset {} is past the end ({} {}s).",
            page.start, page.total, unit
        )
    } else {
        format!(
            "Showing {}s {}-{} of {}",
            unit,
            page.start + 1,
            page.start + page.shown,
            page.total
        )
    };
    if page.shown > 0 {
        match page.next_offset {
            Some(n) => s.push_str(&format!(
                "; more available: pass offset={} (next_offset={})",
                n, n
            )),
            None => s.push_str(" (end of results)"),
        }
        s.push('.');
    }
    if page.collapsed_seen > 0 {
        s.push_str(&format!(
            " {} already shown earlier in this session (collapsed; include_seen:true repeats them).",
            plural(page.collapsed_seen, "match")
        ));
    }
    s
}

fn render_page(
    xg: &Xgrep,
    session: &mut Session,
    ranked: &[SearchResult],
    dedup: &rank::Dedup,
    mode: OutputMode,
    cfg: RenderParts,
) -> Page {
    let rc = RenderCfg {
        root: xg.root(),
        before: cfg.before,
        after: cfg.after,
        max_results: cfg.max_results,
        offset: cfg.offset,
        max_tokens: cfg.max_tokens,
        include_seen: cfg.include_seen,
        highlight: cfg.highlight.as_ref(),
        duplicates: &dedup.duplicates,
    };
    match mode {
        OutputMode::Content => mcp_render::render_content(ranked, &rc, session),
        other => mcp_render::render_list(ranked, other, &rc),
    }
}

struct RenderParts {
    before: usize,
    after: usize,
    max_results: usize,
    offset: usize,
    max_tokens: usize,
    include_seen: bool,
    highlight: Option<Regex>,
}

fn dedup_line(d: &rank::Dedup) -> Option<String> {
    (d.dropped_files > 0).then(|| {
        format!(
            "Collapsed {} in {} with content identical to a higher-ranked file.",
            plural(d.dropped_matches, "match"),
            plural(d.dropped_files, "file")
        )
    })
}

/// Search again with a modified option set and return the match count.
fn count_variant(xg: &Xgrep, pattern: &str, opts: &SearchOptions) -> usize {
    xg.search(pattern, opts).map(|r| r.len()).unwrap_or(0)
}

/// Remove backslashes that escape punctuation: `handleLogin\(` -> `handleLogin(`.
fn unescape_punct(p: &str) -> String {
    let mut out = String::new();
    let mut chars = p.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&n) = chars.peek() {
                if n.is_ascii_punctuation() {
                    out.push(n);
                    chars.next();
                    continue;
                }
            }
        }
        out.push(c);
    }
    out
}

fn zero_result_hints(xg: &Xgrep, prep: &Prepared) -> Vec<String> {
    let mut hints = Vec::new();
    let base = prep.options();
    if !prep.regex && prep.pattern.contains('\\') {
        let un = unescape_punct(&prep.pattern);
        if un != prep.pattern {
            let n = count_variant(xg, &un, &base);
            hints.push(format!(
                "The pattern is literal, so backslashes match themselves. Unescaped `{}` has {}; or set regex:true to use `{}` as a regex.",
                un,
                plural(n, "match"),
                prep.pattern
            ));
        }
    }
    if !prep.regex && hints.is_empty() {
        if let Some(h) = crate::hints::detect_regex_hint(&prep.pattern) {
            hints.push(h.replace(
                "Use -e to enable regex mode.",
                "Set regex:true to search it as a regex.",
            ));
        }
    }
    if !prep.case_insensitive && prep.pattern.chars().any(|c| c.is_alphabetic()) {
        let mut o = base.clone();
        o.case_insensitive = true;
        let n = count_variant(xg, &prep.pattern, &o);
        if n > 0 {
            hints.push(format!(
                "Case-insensitive search finds {}: set case_insensitive:true.",
                plural(n, "match")
            ));
        }
    }
    if prep.word {
        let mut o = base.clone();
        o.word = false;
        let n = count_variant(xg, &prep.pattern, &o);
        if n > 0 {
            hints.push(format!(
                "Without word:true there are {}.",
                plural(n, "match")
            ));
        }
    }
    if prep.filters.is_active() {
        let mut o = base.clone();
        o.file_type = None;
        o.path_pattern = None;
        o.globs.clear();
        let n = count_variant(xg, &prep.pattern, &o);
        if n > 0 {
            hints.push(format!(
                "{} exist outside the file_type/path_pattern/glob filters.",
                plural(n, "match")
            ));
        }
    }
    if !prep.fresh && !prep.changed_only && prep.since.is_none() {
        hints.push(
            "If the code was written moments ago, retry with fresh:true (the index may lag behind the working tree)."
                .to_string(),
        );
    }
    if let Some(o) = xg.oversize_files() {
        hints.push(format!(
            "{}.",
            o.hint("search them outside the index (e.g. `xg PATTERN FILE` or rg)")
        ));
    }
    hints
}

/// Run one prepared search and render its section (header + body).
fn render_search(
    xg: &Xgrep,
    session: &mut Session,
    prep: &Prepared,
    results: Result<Vec<SearchResult>, String>,
) -> (String, bool) {
    let results = match results {
        Ok(r) => r,
        Err(e) => return (format!("Search error: {}", e), true),
    };
    let mut head = String::new();
    for n in &prep.filters.notes {
        head.push_str(&format!("note: {}\n", n));
    }
    // Only mention freshness when it changed the results; "fresh" is noise.
    if let Some(f) = xg.last_search_freshness().filter(|f| f.is_stale()) {
        head.push_str(&format!("note: {}\n", f));
    }
    if results.is_empty() {
        head.push_str(&format!(
            "No matches for `{}` ({}{}).\n",
            prep.pattern,
            prep.case_desc,
            if prep.regex { ", regex" } else { ", literal" }
        ));
        for h in zero_result_hints(xg, prep) {
            head.push_str(&format!("hint: {}\n", h));
        }
        return (head, false);
    }
    let total = results.len();
    let files = count_files(&results);
    let ranked = rank_search(results, prep);
    let (ranked, dedup) = rank::dedupe_identical(xg.root(), ranked);
    if prep.expand_block {
        return render_search_blocks(xg, session, prep, head, &ranked, &dedup, total, files);
    }
    let page = render_page(
        xg,
        session,
        &ranked,
        &dedup,
        prep.mode,
        RenderParts {
            before: prep.context,
            after: prep.context,
            max_results: prep.max_results,
            offset: prep.offset,
            max_tokens: prep.max_tokens,
            include_seen: prep.include_seen,
            highlight: prep.highlight(),
        },
    );
    head.push_str(&format!(
        "Found {} in {} for `{}` ({}). Ranked: definitions and source files first.\n",
        plural(total, "match"),
        plural(files, "file"),
        prep.pattern,
        prep.case_desc
    ));
    if let Some(d) = dedup_line(&dedup) {
        head.push_str(&d);
        head.push('\n');
    }
    let unit = if prep.mode == OutputMode::Content {
        "result"
    } else {
        "file"
    };
    head.push_str(&page_line(&page, unit));
    head.push_str("\n\n");
    head.push_str(&page.body);
    (head, false)
}

/// `search` with `expand:"block"`: one unit per enclosing definition.
#[allow(clippy::too_many_arguments)]
fn render_search_blocks(
    xg: &Xgrep,
    session: &mut Session,
    prep: &Prepared,
    mut head: String,
    ranked: &[SearchResult],
    dedup: &rank::Dedup,
    total: usize,
    files: usize,
) -> (String, bool) {
    let (blocks, syntax_files) = crate::syntax::tools::expand_blocks(xg, ranked);
    let rc = RenderCfg {
        root: xg.root(),
        before: 0,
        after: 0,
        max_results: prep.max_results,
        offset: prep.offset,
        max_tokens: prep.max_tokens,
        include_seen: prep.include_seen,
        highlight: None,
        duplicates: &dedup.duplicates,
    };
    let page = mcp_render::render_blocks(&blocks, &rc, session);
    head.push_str(&format!(
        "Found {} in {} for `{}` ({}), grouped into {}: the smallest enclosing function/class/impl{}; ±{} lines where no definition encloses the match. Ranked: definitions and source files first.\n",
        plural(total, "match"),
        plural(files, "file"),
        prep.pattern,
        prep.case_desc,
        plural(blocks.len(), "block"),
        if syntax_files == 0 && !cfg!(feature = "syntax") {
            " (syntax support not built in: Markdown sections only)"
        } else {
            ""
        },
        3
    ));
    if let Some(d) = dedup_line(dedup) {
        head.push_str(&d);
        head.push('\n');
    }
    head.push_str(&page_line(&page, "block"));
    head.push_str("\n\n");
    head.push_str(&page.body);
    (head, false)
}

/// Merge top-level defaults with one `queries` item.
fn merge_query(base: &serde_json::Map<String, Value>, item: &Value) -> Result<Value, String> {
    let mut m = base.clone();
    m.remove("queries");
    match item {
        Value::String(s) => {
            m.insert("pattern".into(), Value::String(s.clone()));
        }
        Value::Object(o) => {
            if o.contains_key("queries") {
                return Err("queries items cannot contain queries".to_string());
            }
            for (k, v) in o {
                m.insert(k.clone(), v.clone());
            }
        }
        _ => return Err("each queries item must be an object or a string".to_string()),
    }
    Ok(Value::Object(m))
}

/// Handler for the `search` tool.
pub fn handle_search(xg: &Xgrep, session: &mut Session, params: &Value) -> (String, bool) {
    let p: SearchParams = match parse(params) {
        Ok(v) => v,
        Err(e) => return (e, true),
    };
    let Some(queries) = p.queries.clone() else {
        let prep = match prepare(p) {
            Ok(v) => v,
            Err(e) => return (e, true),
        };
        let results = xg
            .search(&prep.pattern, &prep.options())
            .map_err(|e| e.to_string());
        return render_search(xg, session, &prep, results);
    };

    // Multi-query.
    if queries.is_empty() {
        return ("queries must not be empty".to_string(), true);
    }
    if queries.len() > MAX_QUERIES {
        return (
            format!("queries accepts at most {} items", MAX_QUERIES),
            true,
        );
    }
    if p.pattern.is_some() {
        return (
            "Pass either pattern or queries, not both (put the pattern inside queries)".to_string(),
            true,
        );
    }
    let Value::Object(base) = params else {
        return ("Invalid parameters: expected an object".to_string(), true);
    };
    let total_budget = p
        .max_tokens
        .unwrap_or(DEFAULT_MAX_TOKENS)
        .min(MAX_TOKENS_CAP);
    let share = (total_budget / queries.len()).max(MIN_QUERY_TOKENS);
    let mut preps = Vec::with_capacity(queries.len());
    for (i, q) in queries.iter().enumerate() {
        let item = match merge_query(base, q) {
            Ok(v) => v,
            Err(e) => return (format!("queries[{}]: {}", i, e), true),
        };
        let has_own_budget = q.get("max_tokens").is_some();
        let mut qp: SearchParams = match parse(&item) {
            Ok(v) => v,
            Err(e) => return (format!("queries[{}]: {}", i, e), true),
        };
        if !has_own_budget {
            qp.max_tokens = Some(share);
        }
        match prepare(qp) {
            Ok(v) => preps.push(v),
            Err(e) => return (format!("queries[{}]: {}", i, e), true),
        }
    }
    // Build a missing index once up front; parallel searches would otherwise
    // race to auto-build it and trip over the build lock.
    if !xg.index_path().exists() {
        if let Err(e) = xg.build_index() {
            return (format!("Build error: {}", e), true);
        }
    }
    let results: Vec<Result<Vec<SearchResult>, String>> = preps
        .par_iter()
        .map(|prep| {
            xg.search(&prep.pattern, &prep.options())
                .map_err(|e| e.to_string())
        })
        .collect();
    let mut out = format!(
        "{} queries (run in parallel; ~{} tokens each).\n",
        preps.len(),
        share
    );
    let mut any_error = false;
    for (i, (prep, res)) in preps.iter().zip(results).enumerate() {
        let (text, err) = render_search(xg, session, prep, res);
        any_error |= err;
        out.push_str(&format!("\n### Query {}: `{}`\n\n", i + 1, prep.pattern));
        out.push_str(&text);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    (out, any_error && preps.len() == 1)
}

// ---------------------------------------------------------------------------
// find_definitions
// ---------------------------------------------------------------------------

/// Handler for the `find_definitions` tool.
pub fn handle_find_definitions(
    xg: &Xgrep,
    session: &mut Session,
    params: &Value,
) -> (String, bool) {
    let p: FindDefinitionsParams = match parse(params) {
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
    let mode = match parse_mode(p.output_mode.as_deref(), None) {
        Ok(m) => m,
        Err(e) => return (e, true),
    };
    let max_results = match check_max_results(p.max_results, DEFAULT_MAX_RESULTS) {
        Ok(v) => v,
        Err(e) => return (e, true),
    };

    let backend = HeuristicBackend;
    let opts = SearchOptions {
        regex: true,
        file_type: filters.file_type.clone(),
        path_pattern: filters.path_pattern.clone(),
        globs: filters.globs.clone(),
        fresh: p.fresh,
        ..Default::default()
    };
    let candidates = match xg.search(&backend.prefilter(&symbol), &opts) {
        Ok(r) => r,
        Err(e) => return (format!("Search error: {}", e), true),
    };
    let (classified, used) = crate::syntax::tools::classify_definitions(xg, &symbol, candidates);
    let backend_name = used.describe();
    let q = QueryTerm::new(&symbol, false);
    let mut scored = Vec::new();
    for (r, d) in classified {
        let score = d.score + rank::path_score(&r.file) + q.filename_bonus(&r.file);
        scored.push(Scored { result: r, score });
    }

    let mut head = String::new();
    for n in &notes {
        head.push_str(&format!("note: {}\n", n));
    }
    if scored.is_empty() {
        head.push_str(&format!(
            "No definitions of `{}` found ({} backend; comments, docs and local variables are skipped).\n",
            symbol, backend_name
        ));
        // Did-you-mean: same symbol with different case.
        let mut ci_opts = opts.clone();
        ci_opts.case_insensitive = true;
        if let Ok(cands) = xg.search(&format!("(?i){}", backend.prefilter(&symbol)), &ci_opts) {
            let ci = backend.compile(&symbol, true);
            let mut seen = Vec::new();
            for r in cands {
                if ci.classify(&r.file, &r.line).is_some() {
                    seen.push(format!("{}:{}", r.file, r.line_number));
                    if seen.len() >= 5 {
                        break;
                    }
                }
            }
            if !seen.is_empty() {
                head.push_str(&format!(
                    "hint: definitions with different case exist: {}\n",
                    seen.join(", ")
                ));
            }
        }
        head.push_str(
            "hint: for local variables, fields or call sites use search with word:true; if the code is new, retry with fresh:true.\n",
        );
        return (head, false);
    }
    let total = scored.len();
    let files = count_files(&scored.iter().map(|s| s.result.clone()).collect::<Vec<_>>());
    let ranked = rank::order(scored);
    let (ranked, dedup) = rank::dedupe_identical(xg.root(), ranked);
    let highlight = Regex::new(&backend.prefilter(&symbol)).ok();
    let page = render_page(
        xg,
        session,
        &ranked,
        &dedup,
        mode,
        RenderParts {
            before: 0,
            after: p
                .context_lines
                .unwrap_or(DEFAULT_DEF_CONTEXT)
                .min(MAX_CONTEXT),
            max_results,
            offset: p.offset,
            max_tokens: p
                .max_tokens
                .unwrap_or(DEFAULT_MAX_TOKENS)
                .clamp(50, MAX_TOKENS_CAP),
            include_seen: p.include_seen,
            highlight,
        },
    );
    head.push_str(&format!(
        "Found {} of `{}` in {} ({} backend; ranked source-first).\n",
        plural(total, "definition"),
        symbol,
        plural(files, "file"),
        backend_name
    ));
    if let Some(d) = dedup_line(&dedup) {
        head.push_str(&d);
        head.push('\n');
    }
    let unit = if mode == OutputMode::Content {
        "definition"
    } else {
        "file"
    };
    head.push_str(&page_line(&page, unit));
    head.push_str("\n\n");
    head.push_str(&page.body);
    (head, false)
}

// ---------------------------------------------------------------------------
// find_files
// ---------------------------------------------------------------------------

/// Handler for the `find_files` tool.
pub fn handle_find_files(xg: &Xgrep, params: &Value) -> (String, bool) {
    let p: FindFilesParams = match parse(params) {
        Ok(v) => v,
        Err(e) => return (e, true),
    };
    let pattern = p.pattern.trim().trim_start_matches("./").to_string();
    if pattern.is_empty() {
        return ("pattern must not be empty".to_string(), true);
    }
    let filters = match build_filters(p.file_type, None, p.glob, p.exclude) {
        Ok(f) => f,
        Err(e) => return (e, true),
    };
    let max_results = match check_max_results(p.max_results, DEFAULT_FIND_FILES) {
        Ok(v) => v,
        Err(e) => return (e, true),
    };
    let is_glob = looks_like_glob(&pattern);
    let mut paths = if is_glob {
        let g = match GlobFilter::new(std::slice::from_ref(&pattern)) {
            Ok(g) => g,
            Err(e) => return (e.to_string(), true),
        };
        match xg.find_files("") {
            Ok(all) => all.into_iter().filter(|f| g.matches(f)).collect(),
            Err(e) => return (format!("Find error: {}", e), true),
        }
    } else {
        match xg.find_files(&pattern) {
            Ok(v) => v,
            Err(e) => return (format!("Find error: {}", e), true),
        }
    };
    if let Some(ft) = &filters.file_type {
        let exts = filetype::extensions_for_type(ft).unwrap_or_default();
        paths.retain(|f| {
            std::path::Path::new(f)
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| exts.contains(&e))
        });
    }
    if !filters.globs.is_empty() {
        if let Ok(g) = GlobFilter::new(&filters.globs) {
            paths.retain(|f| g.matches(f));
        }
    }
    // Rank: exact file name > file stem > file name contains > path contains.
    let pl = pattern.to_lowercase();
    let name_rank = |f: &str| -> u8 {
        if is_glob {
            return 0;
        }
        let name = f.rsplit('/').next().unwrap_or(f).to_lowercase();
        let stem = name.split('.').next().unwrap_or("").to_string();
        if name == pl {
            0
        } else if stem == pl {
            1
        } else if name.contains(&pl) {
            2
        } else {
            3
        }
    };
    paths.sort_by(|a, b| {
        name_rank(a)
            .cmp(&name_rank(b))
            .then(rank::path_score(b).cmp(&rank::path_score(a)))
            .then(a.len().cmp(&b.len()))
            .then(a.cmp(b))
    });
    let mut out = String::new();
    for n in &filters.notes {
        out.push_str(&format!("note: {}\n", n));
    }
    let total = paths.len();
    if total == 0 {
        out.push_str(&format!("No indexed files match `{}`.\n", pattern));
        out.push_str("hint: without * ? [ the pattern is a case-insensitive substring of the path; with them it is a glob ('*.rs' matches at any depth, 'src/*.rs' only directly under src/). Files created after the last build_index are not listed until the index is rebuilt.\n");
        return (out, false);
    }
    let start = p.offset.min(total);
    let end = (start + max_results).min(total);
    out.push_str(&format!(
        "Found {} matching `{}` ({}). ",
        plural(total, "file"),
        pattern,
        if is_glob { "glob" } else { "substring" }
    ));
    if start >= end {
        out.push_str(&format!("offset {} is past the end.\n", p.offset));
        return (out, false);
    }
    out.push_str(&format!("Showing {}-{} of {}", start + 1, end, total));
    if end < total {
        out.push_str(&format!("; more available: pass offset={}", end));
    }
    out.push_str(".\n\n");
    for f in &paths[start..end] {
        out.push_str(f);
        out.push('\n');
    }
    (out, false)
}

// ---------------------------------------------------------------------------
// index tools
// ---------------------------------------------------------------------------

/// Dispatch the tools added after the original set (`outline`,
/// `find_references`, `repo_map`). Returns None for unknown names.
pub fn handle_extra(
    name: &str,
    xg: &Xgrep,
    _session: &mut Session,
    params: &Value,
) -> Option<(String, bool)> {
    Some(match name {
        "outline" => crate::syntax::tools::handle_outline(xg, params),
        "find_references" => crate::syntax::tools::handle_find_references(xg, params),
        "repo_map" => crate::syntax::tools::handle_repo_map(xg, params),
        _ => return None,
    })
}

/// Handler for the `build_index` tool.
pub fn handle_build_index(xg: &Xgrep) -> (String, bool) {
    let start = std::time::Instant::now();
    match xg.build_index() {
        Ok(rebuilt) => {
            let elapsed = start.elapsed().as_secs_f64();
            let size = std::fs::metadata(xg.index_path())
                .map(|m| m.len())
                .unwrap_or(0);
            let msg = if rebuilt {
                format!(
                    "Index built successfully in {:.2}s ({} bytes)",
                    elapsed, size
                )
            } else {
                format!("Index is up to date ({:.2}s, {} bytes)", elapsed, size)
            };
            (msg, false)
        }
        Err(e) => (format!("Build error: {}", e), true),
    }
}

/// Handler for the `index_status` tool. Returns a structured JSON object so
/// agents can branch on the index state without parsing prose.
pub fn handle_index_status(xg: &Xgrep) -> (String, bool) {
    use crate::IndexState;
    match xg.index_status() {
        Ok(info) => {
            let (state, changed_files) = match info.state {
                IndexState::Fresh => ("fresh", None),
                IndexState::Stale { changed_files } => ("stale", Some(changed_files)),
                IndexState::Missing => ("missing", None),
            };
            let mut json = serde_json::json!({
                "state": state,
                "indexed_files": info.indexed_files,
                "index_size_bytes": info.index_size_bytes,
                "index_path": info.index_path.to_string_lossy(),
            });
            if let Some(c) = changed_files {
                json["changed_files"] = c.into();
            }
            if let Some(o) = info.oversize {
                json["oversize_files"] = serde_json::json!({
                    "count": o.count,
                    "limit_bytes": o.limit,
                    "examples": o.examples,
                });
            }
            (json.to_string(), false)
        }
        Err(e) => (format!("Status check error: {}", e), true),
    }
}

/// Handler for the `session_reset` tool.
pub fn handle_session_reset(session: &mut Session) -> (String, bool) {
    let n = session.reset();
    (
        format!(
            "Session reset: forgot shown ranges for {}. Later results are shown in full.",
            plural(n, "file")
        ),
        false,
    )
}

// ---------------------------------------------------------------------------
// read_file
// ---------------------------------------------------------------------------

/// Handler for the `read_file` tool.
pub fn handle_read_file(xg: &Xgrep, session: &mut Session, params: &Value) -> (String, bool) {
    let p: ReadFileParams = match parse(params) {
        Ok(v) => v,
        Err(e) => return (e, true),
    };

    let (canonical, display) = match resolve_project_path(xg, &p.path) {
        Ok(v) => v,
        Err(e) => return (e, true),
    };
    let meta = match std::fs::metadata(&canonical) {
        Ok(m) => m,
        Err(e) => return (format!("Cannot read file '{}': {}", p.path, e), true),
    };
    if meta.is_dir() {
        return (
            format!("'{}' is a directory. Use find_files to list files.", p.path),
            true,
        );
    }
    if !meta.is_file() {
        return (format!("'{}' is not a regular file.", p.path), true);
    }
    if meta.len() > READ_MAX_BYTES {
        return (
            format!(
                "'{}' is too large to read ({} bytes, limit {}). Use search to find text inside it.",
                p.path,
                meta.len(),
                READ_MAX_BYTES
            ),
            true,
        );
    }
    let bytes = match std::fs::read(&canonical) {
        Ok(b) => b,
        Err(e) => return (format!("Cannot read file '{}': {}", p.path, e), true),
    };
    if memchr::memchr(0, &bytes).is_some() {
        return (
            format!(
                "'{}' looks binary (contains NUL bytes, {} bytes); not printing it. Use search to find text inside it.",
                p.path,
                bytes.len()
            ),
            true,
        );
    }
    let lossy = std::str::from_utf8(&bytes).is_err();
    let content = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();

    if total == 0 {
        return (format!("## {}\n\nFile is empty.\n", display), false);
    }
    let start = p.start_line.unwrap_or(1);
    if start == 0 {
        return (
            "start_line is 1-based; use 1 for the first line".to_string(),
            true,
        );
    }
    if start > total {
        return (
            format!(
                "start_line {} is past the end of '{}' ({}).",
                start,
                display,
                plural(total, "line")
            ),
            true,
        );
    }
    if let Some(e) = p.end_line {
        if e < start {
            return (
                format!("end_line {} is before start_line {}.", e, start),
                true,
            );
        }
    }
    let mut notes = Vec::new();
    let requested_end = p.end_line.unwrap_or(usize::MAX);
    if p.end_line.is_some_and(|e| e > total) {
        notes.push(format!(
            "end_line {} clamped to the last line ({})",
            requested_end, total
        ));
    }
    let hard_end = requested_end.min(total);
    let budget = p
        .max_tokens
        .unwrap_or(READ_DEFAULT_TOKENS)
        .clamp(100, MAX_TOKENS_CAP);

    let mut body = String::new();
    let mut used = 0usize;
    let mut end = start - 1;
    let mut cut_lines = 0usize;
    let mut stop_reason: Option<String> = None;
    for (i, line) in lines.iter().enumerate().take(hard_end).skip(start - 1) {
        let n = i + 1;
        if n - start >= READ_MAX_LINES {
            stop_reason = Some(format!("{} lines", READ_MAX_LINES));
            break;
        }
        let text = if line.chars().count() > READ_LINE_CHARS {
            cut_lines += 1;
            mcp_render::truncate_line(line, READ_LINE_CHARS)
        } else {
            (*line).to_string()
        };
        let row = format!("{:4} | {}\n", n, text);
        let t = mcp_render::estimate_tokens(&row);
        if n > start && used + t > budget {
            stop_reason = Some(format!("~{} tokens", budget));
            break;
        }
        used += t;
        body.push_str(&row);
        end = n;
    }
    session.mark(&display, start, end);

    let lang = std::path::Path::new(&p.path)
        .extension()
        .and_then(|e| e.to_str())
        .map(output::lang_from_ext)
        .unwrap_or("");
    let fence = mcp_render::fence_for(&body);
    let mut out = format!(
        "## {}:{}-{} ({} total)\n",
        display,
        start,
        end,
        plural(total, "line")
    );
    if lossy {
        notes.push("file is not valid UTF-8; invalid bytes shown as U+FFFD".to_string());
    }
    if cut_lines > 0 {
        notes.push(format!(
            "{} longer than {} chars shortened",
            plural(cut_lines, "line"),
            READ_LINE_CHARS
        ));
    }
    for n in &notes {
        out.push_str(&format!("note: {}\n", n));
    }
    out.push_str(&format!("\n{}{}\n{}{}\n", fence, lang, body, fence));
    if let Some(reason) = stop_reason {
        out.push_str(&format!(
            "[truncated at {}; showing lines {}-{} of {}; next start_line={}]\n",
            reason,
            start,
            end,
            total,
            end + 1
        ));
    } else if end < total && p.end_line.is_none() {
        out.push_str(&format!("[next start_line={}]\n", end + 1));
    }
    (out, false)
}

/// Legacy single-regex definition pattern (kept for reference/tests of the
/// generic fallback; `find_definitions` uses [`crate::defs`]).
#[cfg(test)]
fn definition_regex(symbol: &str) -> String {
    let escaped = regex::escape(symbol);
    format!(
        r"(?:pub\s+)?(?:fn|struct|enum|trait|type|impl|class|def|function|func|fun|const|let|var|val|interface)\s+{}\b",
        escaped
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn git(root: &std::path::Path, args: &[&str]) {
        crate::git::git_cmd()
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
    }

    fn init_repo(root: &std::path::Path) {
        git(root, &["init"]);
        git(root, &["config", "user.email", "test@test.com"]);
        git(root, &["config", "user.name", "test"]);
        fs::write(root.join(".gitignore"), ".xgrep/\n").unwrap();
    }

    fn commit_and_index(root: &std::path::Path) -> Xgrep {
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "init"]);
        let xg = Xgrep::open_local(root)
            .unwrap()
            .with_config(crate::Config { quiet: true });
        xg.build_index().unwrap();
        xg
    }

    fn setup_test_repo() -> (tempfile::TempDir, Xgrep) {
        let dir = tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        fs::write(
            root.join("hello.rs"),
            "fn hello() {\n    println!(\"hello\");\n}\n\nstruct Foo {\n    x: i32,\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("world.py"),
            "def world():\n    print(\"world\")\n\nclass Bar:\n    pass\n",
        )
        .unwrap();
        let xg = commit_and_index(root);
        (dir, xg)
    }

    fn search(xg: &Xgrep, v: Value) -> (String, bool) {
        handle_search(xg, &mut Session::new(), &v)
    }

    fn defs(xg: &Xgrep, v: Value) -> (String, bool) {
        handle_find_definitions(xg, &mut Session::new(), &v)
    }

    fn read(xg: &Xgrep, v: Value) -> (String, bool) {
        handle_read_file(xg, &mut Session::new(), &v)
    }

    #[test]
    fn test_tools_list() {
        let tools = tools_list();
        assert_eq!(tools.len(), 10);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for n in [
            "search",
            "find_definitions",
            "find_files",
            "index_status",
            "build_index",
            "read_file",
            "session_reset",
            "outline",
            "find_references",
            "repo_map",
        ] {
            assert!(names.contains(&n), "missing {}", n);
        }
    }

    #[test]
    fn test_syntax_tools_with_and_without_feature() {
        let (_dir, xg) = setup_test_repo();
        let mut s = Session::new();
        let (o, err) = handle_extra(
            "outline",
            &xg,
            &mut s,
            &serde_json::json!({"path": "hello.rs"}),
        )
        .unwrap();
        if cfg!(feature = "syntax") {
            assert!(!err, "{}", o);
            assert!(o.contains("L1-3 fn hello()"), "{}", o);
            assert!(o.contains("L5-7 struct Foo"), "{}", o);
        } else {
            assert!(err, "{}", o);
            assert!(o.contains("without the `syntax` feature"), "{}", o);
        }
        // find_references works either way (regex fallback for definitions).
        let (r, err) = handle_extra(
            "find_references",
            &xg,
            &mut s,
            &serde_json::json!({"symbol": "Foo"}),
        )
        .unwrap();
        assert!(!err, "{}", r);
        assert!(r.contains("No references"), "{}", r);
        let (m, err) = handle_extra("repo_map", &xg, &mut s, &serde_json::json!({})).unwrap();
        assert_eq!(err, !cfg!(feature = "syntax"), "{}", m);
        assert!(handle_extra("nope", &xg, &mut s, &serde_json::json!({})).is_none());
        // expand:block falls back to context windows without syntax support.
        let (b, err) = search(
            &xg,
            serde_json::json!({"pattern": "println", "expand": "block"}),
        );
        assert!(!err, "{}", b);
        assert!(b.contains("grouped into 1 block"), "{}", b);
        if cfg!(feature = "syntax") {
            assert!(b.contains("## hello.rs:1-3 fn hello (1 match)"), "{}", b);
        }
    }

    #[test]
    fn test_handle_search() {
        let (_dir, xg) = setup_test_repo();
        let (output, is_error) = search(&xg, serde_json::json!({"pattern": "hello"}));
        assert!(!is_error, "output was: {}", output);
        assert!(output.contains("hello"));
        assert!(output.contains("Found"));
    }

    #[test]
    fn test_handle_find_definitions() {
        let (_dir, xg) = setup_test_repo();
        let (output, is_error) = defs(&xg, serde_json::json!({"symbol": "hello"}));
        assert!(!is_error);
        assert!(output.contains("hello"));
        assert!(output.contains("definition"));
    }

    #[test]
    fn test_handle_find_definitions_missing_symbol() {
        let (_dir, xg) = setup_test_repo();
        let (output, is_error) = defs(&xg, serde_json::json!({}));
        assert!(is_error);
        assert!(output.contains("Invalid parameters"));
        assert!(output.contains("symbol"));
    }

    #[test]
    fn test_handle_find_definitions_no_truncation() {
        let (_dir, xg) = setup_test_repo();
        let (output, is_error) = defs(
            &xg,
            serde_json::json!({"symbol": "hello", "max_results": 100}),
        );
        assert!(!is_error, "unexpected error: {}", output);
        assert!(output.contains("Found 1 definition"));
        assert!(output.contains("end of results"));
        assert!(!output.contains("next_offset"));
    }

    #[test]
    fn test_handle_find_definitions_truncation_signal() {
        let (dir, xg) = setup_test_repo();
        fs::write(
            dir.path().join("extra.rs"),
            "fn hello() {}\nfn hello_world() {}\n",
        )
        .unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-m", "add extra"]);
        xg.build_index().unwrap();

        let (output, is_error) = defs(
            &xg,
            serde_json::json!({"symbol": "hello", "max_results": 1}),
        );
        assert!(!is_error, "unexpected error: {}", output);
        assert!(output.contains("Found 2 definitions"), "{}", output);
        assert!(output.contains("next_offset=1"), "{}", output);
        let (second, _) = defs(
            &xg,
            serde_json::json!({"symbol": "hello", "max_results": 1, "offset": 1}),
        );
        assert!(
            second.contains("Showing definitions 2-2 of 2"),
            "{}",
            second
        );
    }

    #[test]
    fn test_find_definitions_skips_locals_and_comments() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        fs::write(
            root.join("a.rs"),
            "// fn compute() in a comment\nfn main() {\n    let compute = 3;\n}\npub(crate) fn compute() {}\n",
        )
        .unwrap();
        fs::write(root.join("README.md"), "```rust\nfn compute() {}\n```\n").unwrap();
        let xg = commit_and_index(root);
        let (out, err) = defs(&xg, serde_json::json!({"symbol": "compute"}));
        assert!(!err);
        assert!(out.contains("Found 1 definition"), "{}", out);
        assert!(out.contains("a.rs:5"), "{}", out);
    }

    #[test]
    fn test_find_definitions_case_hint() {
        let (_dir, xg) = setup_test_repo();
        let (out, err) = defs(&xg, serde_json::json!({"symbol": "HELLO"}));
        assert!(!err);
        assert!(out.contains("different case"), "{}", out);
        assert!(out.contains("hello.rs:1"), "{}", out);
    }

    #[test]
    fn test_handle_index_status_structured_json() {
        let (_dir, xg) = setup_test_repo();
        let (output, is_error) = handle_index_status(&xg);
        assert!(!is_error, "output was: {}", output);
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["state"], "fresh");
        assert!(json["indexed_files"].as_u64().unwrap() >= 1);
        assert!(json["index_size_bytes"].as_u64().unwrap() > 0);
        assert!(json["index_path"].as_str().unwrap().contains("index"));
        assert!(json.get("changed_files").is_none());
    }

    #[test]
    fn test_handle_index_status_stale_reports_changed_files() {
        let (dir, xg) = setup_test_repo();
        fs::write(
            dir.path().join("hello.rs"),
            "fn hello() {\n    println!(\"changed\");\n}\n",
        )
        .unwrap();
        let (output, is_error) = handle_index_status(&xg);
        assert!(!is_error, "output was: {}", output);
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["state"], "stale");
        assert!(json["changed_files"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn test_handle_build_index() {
        let (_dir, xg) = setup_test_repo();
        let (output, is_error) = handle_build_index(&xg);
        assert!(!is_error);
        assert!(
            output.contains("Index built successfully") || output.contains("Index is up to date"),
            "unexpected output: {output}"
        );
        assert!(output.contains("bytes"));
    }

    #[test]
    fn test_handle_search_missing_pattern() {
        let (_dir, xg) = setup_test_repo();
        let (output, is_error) = search(&xg, serde_json::json!({}));
        assert!(is_error);
        assert!(output.contains("Invalid parameters"));
        assert!(output.contains("pattern"));
    }

    #[test]
    fn test_handle_search_with_max_tokens() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        let content: String = (1..=30)
            .map(|i| format!("fn handler_{i}() {{}}\n"))
            .collect();
        fs::write(root.join("a.rs"), &content).unwrap();
        let xg = commit_and_index(root);

        let (text, is_error) = search(
            &xg,
            serde_json::json!({"pattern": "handler", "max_tokens": 100}),
        );
        assert!(!is_error);
        assert!(text.contains("handler"));
        assert!(text.contains("next_offset"), "{}", text);
        // body respects the budget (~4 chars/token, header excluded)
        let body = text.split("\n\n").skip(1).collect::<Vec<_>>().join("\n\n");
        assert!(body.len() <= 100 * 4 + 50, "body too long: {}", body.len());
    }

    #[test]
    fn test_definition_regex() {
        let re = regex::Regex::new(&definition_regex("Foo")).unwrap();
        assert!(re.is_match("fn Foo("));
        assert!(!re.is_match("fn FooBar("));
    }

    #[test]
    fn test_handle_read_file() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("hello.rs"),
            "line1\nline2\nline3\nline4\nline5",
        )
        .unwrap();
        let xg = crate::Xgrep::open(dir.path()).unwrap();
        let (text, is_error) = read(&xg, serde_json::json!({"path": "hello.rs"}));
        assert!(!is_error);
        assert!(text.contains("line1"));
        assert!(text.contains("line5"));
        assert!(text.contains("(5 lines total)"));
    }

    #[test]
    fn test_handle_read_file_line_range() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("hello.rs"),
            "line1\nline2\nline3\nline4\nline5",
        )
        .unwrap();
        let xg = crate::Xgrep::open(dir.path()).unwrap();
        let (text, is_error) = read(
            &xg,
            serde_json::json!({"path": "hello.rs", "start_line": 2, "end_line": 4}),
        );
        assert!(!is_error);
        assert!(text.contains("line2"));
        assert!(text.contains("line4"));
        assert!(!text.contains("line1"));
        assert!(!text.contains("line5"));
    }

    #[test]
    fn test_handle_read_file_out_of_range() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.py"), "1\n2\n3\n").unwrap();
        let xg = crate::Xgrep::open(dir.path()).unwrap();
        let (t, e) = read(&xg, serde_json::json!({"path": "a.py", "start_line": 50}));
        assert!(e);
        assert!(t.contains("past the end"), "{}", t);
        let (t, e) = read(
            &xg,
            serde_json::json!({"path": "a.py", "start_line": 3, "end_line": 2}),
        );
        assert!(e);
        assert!(t.contains("before start_line"), "{}", t);
        let (t, e) = read(
            &xg,
            serde_json::json!({"path": "a.py", "start_line": 2, "end_line": 10}),
        );
        assert!(!e);
        assert!(t.contains("clamped"), "{}", t);
    }

    #[test]
    fn test_handle_read_file_caps_and_binary() {
        let dir = tempdir().unwrap();
        let big: String = (1..=5000).map(|i| format!("line {i}\n")).collect();
        fs::write(dir.path().join("big.txt"), big).unwrap();
        fs::write(dir.path().join("bin.dat"), b"abc\0def").unwrap();
        fs::write(dir.path().join("long.js"), "x".repeat(5000)).unwrap();
        let xg = crate::Xgrep::open(dir.path()).unwrap();

        let (t, e) = read(&xg, serde_json::json!({"path": "big.txt"}));
        assert!(!e);
        assert!(t.contains("5000 lines total"));
        assert!(
            t.contains("next start_line=2001"),
            "{}",
            &t[t.len() - 200..]
        );

        let (t, e) = read(&xg, serde_json::json!({"path": "bin.dat"}));
        assert!(e);
        assert!(t.contains("binary"));

        let (t, e) = read(&xg, serde_json::json!({"path": "long.js"}));
        assert!(!e);
        assert!(t.contains("…[+4000 chars]"));
        assert!(t.len() < 1500);
    }

    #[test]
    fn test_handle_read_file_denies_git_dir() {
        let (dir, xg) = setup_test_repo();
        assert!(dir.path().join(".git/config").exists());
        let (t, e) = read(&xg, serde_json::json!({"path": ".git/config"}));
        assert!(e);
        assert!(t.contains(".git"));
    }

    #[test]
    fn test_handle_read_file_path_traversal() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("hello.rs"), "safe").unwrap();
        let xg = crate::Xgrep::open(dir.path()).unwrap();
        let (_, is_error) = read(&xg, serde_json::json!({"path": "../../etc/passwd"}));
        assert!(is_error);
    }

    #[test]
    fn test_handle_read_file_rejects_absolute_paths_before_resolving() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("hello.rs"), "safe").unwrap();
        let xg = crate::Xgrep::open(dir.path()).unwrap();
        let abs = dir.path().join("hello.rs");
        let mut paths = vec![abs.to_string_lossy().into_owned(), "/etc/passwd".into()];
        if cfg!(windows) {
            paths.extend([r"\\192.0.2.1\share\x".into(), r"C:\Windows\win.ini".into()]);
        }
        for p in paths {
            let (text, is_error) = read(&xg, serde_json::json!({ "path": p }));
            assert!(is_error, "{p}");
            assert_eq!(text, OUTSIDE_ROOT, "{p}");
        }
        // Paths that stay inside the root still work.
        let (_, is_error) = read(&xg, serde_json::json!({"path": "./hello.rs"}));
        assert!(!is_error);
    }

    #[cfg(unix)]
    #[test]
    fn test_handle_read_file_rejects_symlink_out_of_root() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        fs::write(outside.path().join("secret"), "s3cret").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), dir.path().join("link")).unwrap();
        let xg = crate::Xgrep::open(dir.path()).unwrap();
        let (text, is_error) = read(&xg, serde_json::json!({"path": "link"}));
        assert!(is_error);
        assert!(!text.contains("s3cret"));
    }

    #[test]
    fn test_handle_search_rejects_too_many_queries() {
        let (_dir, xg) = setup_test_repo();
        let queries: Vec<Value> = (0..=MAX_QUERIES).map(|_| "hello".into()).collect();
        let (text, is_error) = search(&xg, serde_json::json!({ "queries": queries }));
        assert!(is_error);
        assert!(text.contains("at most"));
    }

    #[test]
    fn test_handle_read_file_empty() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("empty.txt"), "").unwrap();
        let xg = crate::Xgrep::open(dir.path()).unwrap();
        let (text, is_error) = read(&xg, serde_json::json!({"path": "empty.txt"}));
        assert!(!is_error);
        assert!(text.contains("empty"));
    }

    #[test]
    fn test_handle_search_invalid_max_results() {
        let (_dir, xg) = setup_test_repo();
        let (output, is_error) = search(
            &xg,
            serde_json::json!({"pattern": "hello", "max_results": "not_a_number"}),
        );
        assert!(is_error);
        assert!(output.contains("Invalid parameters"));
    }

    #[test]
    fn test_handle_search_invalid_bool() {
        let (_dir, xg) = setup_test_repo();
        let (output, is_error) =
            search(&xg, serde_json::json!({"pattern": "hello", "regex": "yes"}));
        assert!(is_error);
        assert!(output.contains("Invalid parameters"));
    }

    #[test]
    fn test_handle_search_negative_max_results() {
        let (_dir, xg) = setup_test_repo();
        let (output, is_error) = search(
            &xg,
            serde_json::json!({"pattern": "hello", "max_results": -5}),
        );
        assert!(is_error);
        assert!(output.contains("Invalid parameters"));
    }

    #[test]
    fn test_oversize_files_in_index_status_and_zero_result_hint() {
        let (_dir, xg) = setup_test_repo();
        // Without skipped files: no field, no hint.
        let (s, _) = handle_index_status(&xg);
        assert!(!s.contains("oversize_files"), "{s}");
        let (t, _) = search(&xg, serde_json::json!({"pattern": "zzz_nothing"}));
        assert!(!t.contains("not indexed"), "{t}");

        let info = crate::OversizeInfo {
            limit: 10 << 20,
            count: 2,
            examples: vec!["data/dump.sql".into(), "logs/a.log".into()],
        };
        fs::write(
            crate::index::builder::oversize_path(xg.index_path()),
            serde_json::to_vec(&info).unwrap(),
        )
        .unwrap();
        let (s, err) = handle_index_status(&xg);
        assert!(!err, "{s}");
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["oversize_files"]["count"], 2);
        assert_eq!(v["oversize_files"]["limit_bytes"], 10 << 20);
        assert_eq!(v["oversize_files"]["examples"][0], "data/dump.sql");

        let (t, _) = search(&xg, serde_json::json!({"pattern": "zzz_nothing"}));
        assert!(
            t.contains("hint: 2 files over 10MB are not indexed (e.g. data/dump.sql)")
                && t.contains("XGREP_MAX_FILESIZE=0"),
            "{t}"
        );
        // Not added to non-empty results.
        let (t, _) = search(&xg, serde_json::json!({"pattern": "hello"}));
        assert!(!t.contains("not indexed"), "{t}");
    }

    #[test]
    fn test_strip_unknown_args() {
        let (args, note) =
            strip_unknown_args("read_file", &serde_json::json!({"paht": "a", "path": "b"}));
        assert_eq!(args, serde_json::json!({"path": "b"}));
        assert_eq!(
            note.as_deref(),
            Some("note: ignored unknown argument(s): paht (did you mean path?)")
        );
        let (args, note) = strip_unknown_args(
            "search",
            &serde_json::json!({"pattern": "x", "pathpattern": "src", "limit": 3, "context": 2}),
        );
        assert_eq!(args, serde_json::json!({"pattern": "x"}));
        let note = note.unwrap();
        assert!(
            note.contains("pathpattern (did you mean path_pattern?)"),
            "{note}"
        );
        assert!(note.contains("limit (did you mean max_results?)"), "{note}");
        assert!(
            note.contains("context (did you mean context_lines?)"),
            "{note}"
        );
        // Known arguments only, unknown tools and non-objects: unchanged.
        let ok = serde_json::json!({"pattern": "x", "word": true});
        assert_eq!(strip_unknown_args("search", &ok), (ok.clone(), None));
        assert_eq!(strip_unknown_args("nope", &ok), (ok.clone(), None));
    }

    #[test]
    fn test_unknown_argument_reaches_handler_only_via_dispatch() {
        // Handlers still reject unknown fields (the MCP dispatcher strips
        // them first), which keeps the schema and the param structs in sync.
        let (_dir, xg) = setup_test_repo();
        let (output, is_error) = search(&xg, serde_json::json!({"pattern": "hello", "paht": "x"}));
        assert!(is_error);
        assert!(output.contains("unknown field `paht`"), "{}", output);
    }

    #[test]
    fn test_unescape_punct() {
        assert_eq!(unescape_punct(r"handleLogin\("), "handleLogin(");
        assert_eq!(unescape_punct(r"a\.b\n"), r"a.b\n");
    }
}
