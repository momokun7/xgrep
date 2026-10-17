# Using xgrep from AI Agents

xgrep is an indexed code search engine fast enough to call inside an agent loop
(typically tens of milliseconds on large repos), with token-aware Markdown
output and a built-in MCP server so AI coding tools can search a codebase
natively instead of re-scanning it on every query.

## Quick reference (CLI)

```bash
xg "pattern" /path --format llm   # Markdown output sized for context windows
xg "pattern" /path --format llm --max-tokens 4000   # stop at ~4000 tokens
```

- **`--format llm` layout:** one `## path:start-end` block per match group;
  each line is prefixed by its number, with `>` on matched lines
  (`181> ...`) and a space on context lines (`178  ...`). Lines over 300
  characters are cut with `…[+N chars]` (change with `--max-columns`, `0` = off).
  `--max-tokens` is unlimited by default for the CLI.
- **`--json`** prints JSON Lines: `{"type":"match","file",...,"line_number",...,"line",...}`
  per matching line, then `{"type":"summary","total_matches",...,"total_files",...,"truncated",...}`.

- **Exit codes:** `0` = matches found, `1` = no matches (not an error), `2` = error.
- **Path argument is optional.** Omit it to search the current directory, or pass
  an absolute path to search a specific repo without `cd`.

### Environment variables

| Variable | Effect | Default |
|----------|--------|---------|
| `XGREP_LLM_CONTEXT` | Default context lines for `--format llm` | `3` |
| `XGREP_ABSOLUTE_PATHS` | Set to `1` to always emit absolute paths | unset |
| `XGREP_NO_HINTS` | Set to `1` to suppress regex pattern hints on stderr | unset |

### Useful one-liners

```bash
# Smart-case is the default: an all-lowercase pattern is case-insensitive.
# Force case-sensitive matching with -s:
xg "Parser" /path -s --format llm

# Word-boundary match (-w): "cat" matches "the cat" but not "concatenate"
xg "cat" /path -w --format llm

# Include / exclude by glob (-g). Prefix with ! to exclude. Repeatable:
xg "TODO" /path -g "*.rs" -g "!*_test.rs" --format llm

# Asymmetric context: 0 lines before, 10 lines after each match
xg "fn handle_request" /path -B 0 -A 10 --format llm

# Filter by file type and limit results
xg "import" /path -t py --max-count 20 --format llm
```

## MCP server

Start the server over stdio:

```bash
xg serve                        # current directory
xg serve --root /path/to/repo   # specific directory
```

Register it with Claude Code (`.mcp.json` or settings):

```json
{
  "mcpServers": {
    "xgrep": {
      "command": "xg",
      "args": ["serve"]
    }
  }
}
```

### Tools

Pick the tool by intent:

| Intent | Tool |
|--------|------|
| Where is `X` declared? | `find_definitions` |
| Where is `X` used / where does this text occur? | `search` (add `word: true` for identifiers) |
| Which files are named like `X`? | `find_files` |
| Show me lines 120-180 of a file | `read_file` with `start_line`/`end_line` |

| Tool | Purpose | Key parameters |
|------|---------|----------------|
| `search` | Trigram-indexed search. Results are ranked and paginated (see below). | `pattern` (literal unless `regex: true`), `queries` (array of searches run in parallel), `case_insensitive` (default: smart-case), `word`, `file_type`, `path_pattern` (substring; globs are applied as globs), `glob` / `exclude` (ripgrep `-g` semantics, `!` excludes), `output_mode` / `mode` (`content`, `files`, `count`, `locations`), `context_lines` (default 0), `max_results` (default 20), `offset`, `max_tokens` (default 4000), `changed_only`, `since`, `fresh`, `include_seen` |
| `find_definitions` | Where a symbol is defined, via per-language heuristics: Rust `fn`/`struct`/`impl … for X`/`macro_rules!`, Go `func (r *T) X`, C/C++/Java `type X(`, `#define X`, TS/JS functions, classes, class methods and top-level consts, Python `def`/`class`, and more. Comment lines, Markdown and local variables (`let`, indented `const`) are excluded. Not a parser: verify with `read_file` when it matters. | `symbol` (required; `Type::method` uses the last segment), filters as in `search`, `context_lines` (lines after the definition, default 2), `max_results`, `offset`, `max_tokens`, `output_mode`, `include_seen` |
| `find_files` | File names/paths from the index, like `xg --find`. Substring (case-insensitive) or glob (`*.rs` matches at any depth). Exact file-name matches rank first. | `pattern` (required), `file_type`, `glob`, `exclude`, `max_results` (default 50), `offset` |
| `read_file` | Read a file with line numbers. Capped at 2000 lines or ~10k tokens; the header shows the total line count and a cut response ends with `next start_line=N`. Lines over 1000 chars are shortened. Files with NUL bytes, paths inside `.git/` and paths outside the project are refused; `start_line` past the end or `end_line < start_line` is an error (`end_line` past the end is clamped). | `path` (required), `start_line`, `end_line`, `max_tokens` |
| `index_status` | Report index health. Returns JSON: `state` (`"fresh"`, `"stale"`, or `"missing"`), `changed_files` (count, present only when `state` is `"stale"`), `indexed_files`, `index_size_bytes`, `index_path`. | none |
| `build_index` | Build or rebuild the search index for the codebase. | none |
| `session_reset` | Forget which ranges were already returned (see *Session dedup*). | none |

Unknown argument names, unknown `file_type`s and unknown `output_mode`s are
errors that name the valid choices, instead of being silently ignored.

The `index_status` JSON lets an agent branch on freshness without parsing prose:
call `build_index` when `state` is `"missing"`, or proceed directly when `state`
is `"fresh"`.

### Reading `search` / `find_definitions` responses

Every response starts with a header like:

```
Found 75 matches in 16 files for `SearchOptions` (smart-case: case-sensitive). Ranked: definitions and source files first.
Collapsed 12 matches in 3 files with content identical to a higher-ranked file.
Showing results 1-20 of 63; more available: pass offset=20 (next_offset=20).
```

- **Exact totals and pagination.** The total is exact. Pass `offset` to get
  the next page; `next_offset` is present whenever more results remain. When
  `max_tokens` cannot fit `max_results` matches the page is shortened and
  `next_offset` points at the first match that was not shown, so nothing is
  silently skipped.
- **Ranking.** Definition lines first, then source files, then tests,
  examples/benches, docs, vendored code and CHANGELOGs. File names containing
  the query get a boost, comment lines are ranked last, and at most 5 matches
  per file come before other files' matches. Files with byte-identical content
  (xxh64) are collapsed into the highest-ranked copy.
- **Smart-case.** An all-lowercase ASCII pattern is case-insensitive; any
  uppercase letter makes it case-sensitive. `case_insensitive: true/false`
  overrides this.
- **Zero results come with hints**: a literal pattern with backslash escapes
  (`handleLogin\(`) reports how many matches the unescaped text has, an
  uppercase pattern reports the case-insensitive count, filtered searches
  report how many matches exist outside the filters, and `fresh: true` is
  suggested for code written moments ago.
- **Long lines** (minified JS, data) are shown as a ~300 character window
  around the match.
- **Why `context_lines` defaults to 0:** on this repository 20 results cost
  1.9k characters with 0 lines of context, 3.2k with 1 and 5.3k with 3; with
  the default 4000-token budget, 3 lines of context fits only 49 of 75
  `SearchOptions` matches, 0 lines fits all of them. Ask for context
  (or call `read_file` on a hit) when you need it.

### Output modes and multi-query

`output_mode` (alias `mode`) controls the shape of each result; in all modes
except `content` pagination counts files:

| Mode | Output |
|------|--------|
| `content` | Markdown code blocks with the matching lines (default) |
| `files` | `path (N matches, first L12)` per file |
| `count` | `path:N` per file |
| `locations` | `path: L12,L40-42` per file, no line text: the cheapest way to survey usages |

Several searches can be issued in one round trip; they run in parallel and the
top-level fields act as defaults for every item (`max_tokens` is split between
them unless an item sets its own):

```json
{"queries": [{"pattern": "handleLogin", "word": true},
             {"pattern": "LoginError", "output_mode": "files"},
             "session_token"],
 "mode": "locations", "glob": "!**/tests/**"}
```

### Session dedup

The server remembers which `(file, line range)` it has returned during the
session (search blocks, definitions and `read_file` windows). Later results
inside those ranges are collapsed to one line such as
`already shown: src/auth.rs:L10-40 (2 matches)` instead of repeating the code.
Pass `include_seen: true` to repeat them for one call, or call
`session_reset` to forget everything.

## Recipes for agents

```bash
# 1. Find where a symbol is defined (definitions only, not call sites)
xg -e "(?:pub\s+)?(?:fn|struct|enum|trait)\s+TokenStream\b" /path -t rs --format llm
# (the find_definitions MCP tool does this per language, ranked, without comments/locals)

# 2. Search only files changed since the last commit (fast, scoped review)
xg "unwrap\(\)" /path --changed -e --format llm

# 3. Repeated search on a large repo — the first call builds the index,
#    subsequent calls reuse it and return in milliseconds
xg "deprecated" /path --format llm
xg "TODO"       /path --format llm   # reuses the warm index

# 4. Filter by type AND glob to narrow a noisy match
xg "Config" /path -t rs -g "!**/tests/**" --format llm

# 5. Asymmetric context to read more code *after* each match (e.g. function bodies)
xg "fn build_index" /path -B 0 -A 20 --format llm
```

## Library (Rust)

Add the crate and drive it directly. Options use a fluent builder, so you never
need an exhaustive struct literal:

```rust
use xgrep_search::{Xgrep, SearchOptions};

let xg = Xgrep::open(".")?;
xg.build_index()?;

let opts = SearchOptions::new()
    .with_case_insensitive(true)
    .with_file_type("rs")
    .with_max_count(20)
    .with_glob("!*_test.rs");

for r in xg.search("fn main", &opts)? {
    println!("{}:{}: {}", r.file, r.line_number, r.line);
}
# Ok::<(), xgrep_search::XgrepError>(())
```

`index_status()` returns a structured `IndexStatusInfo` you can branch on
without string parsing (its `Display` impl reproduces the `xg status` text):

```rust
use xgrep_search::{Xgrep, IndexState};

let xg = Xgrep::open(".")?;
let info = xg.index_status()?;
match info.state {
    IndexState::Fresh => println!("up to date ({} files)", info.indexed_files),
    IndexState::Stale { changed_files } => println!("{} files changed", changed_files),
    IndexState::Missing => xg.build_index()?,
}
println!("index: {} bytes at {}", info.index_size_bytes, info.index_path.display());
# Ok::<(), xgrep_search::XgrepError>(())
```
