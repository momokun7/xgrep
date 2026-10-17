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
| What are the important parts of this repo? | `repo_map` |
| What is in this file (without reading it)? | `outline` |
| Where is `X` declared? | `find_definitions` |
| Who calls / uses `X`? | `find_references` |
| Where does this text occur? | `search` (add `word: true` for identifiers, `expand: "block"` for whole functions) |
| Which files are named like `X`? | `find_files` |
| Show me lines 120-180 of a file | `read_file` with `start_line`/`end_line` |

| Tool | Purpose | Key parameters |
|------|---------|----------------|
| `search` | Trigram-indexed search. Results are ranked and paginated (see below). | `pattern` (literal unless `regex: true`), `queries` (array of searches run in parallel), `case_insensitive` (default: smart-case), `word`, `file_type`, `path_pattern` (substring; globs are applied as globs), `glob` / `exclude` (ripgrep `-g` semantics, `!` excludes), `output_mode` / `mode` (`content`, `files`, `count`, `locations`), `expand` (`block`: whole enclosing function/class, see *Syntax-aware tools*), `context_lines` (default 0), `max_results` (default 20), `offset`, `max_tokens` (default 4000), `changed_only`, `since`, `fresh`, `include_seen` |
| `find_definitions` | Where a symbol is defined. Candidate lines come from the trigram index; files in Rust, Python, JS/TS, Go, Java, C/C++ and Ruby are then parsed with tree-sitter and only real definitions (fn/method/class/struct/enum/trait/interface/type/impl/const/macro/module) count, so strings, comments and locals never match. Other languages use per-language regex heuristics (Go `func (r *T) X`, `#define X`, PHP, Kotlin, ...). The header names the backend (`tree-sitter`, `tree-sitter + regex fallback`, `regex-heuristic`). | `symbol` (required; `Type::method` uses the last segment), filters as in `search`, `context_lines` (lines after the definition, default 2), `max_results`, `offset`, `max_tokens`, `output_mode`, `include_seen` |
| `find_references` | Usages of an identifier: whole-word, case-sensitive matches minus definition lines and import/use/include/require lines, each annotated with its enclosing function (`in Server.handle_login`) and, in Markdown, its section. The header lists the distinct callers. Ranked source, tests, then docs. Text-based: same-named symbols are not told apart. | `symbol` (required), filters as in `search`, `include_definitions`, `include_imports`, `max_results` (default 50), `offset`, `max_tokens` (default 4000), `fresh` |
| `outline` | Symbols of a file without bodies: line range, signature and nesting (methods under their class/impl). A directory outlines each supported file (top-level only unless `max_depth`); Markdown lists headings. | `path` (file, directory, or a list), `max_depth`, `max_tokens` (default 4000) |
| `repo_map` | Aider-style overview: PageRank over the cross-file definition/reference graph, then the top symbols' signatures grouped by file within a token budget. | `focus` (files/directories to personalize the ranking), `focus_symbols` (identifiers to weight up), `max_tokens` (default 1500), filters as in `search` |
| `find_files` | File names/paths from the index, like `xg --find`. Substring (case-insensitive) or glob (`*.rs` matches at any depth). Exact file-name matches rank first. | `pattern` (required), `file_type`, `glob`, `exclude`, `max_results` (default 50), `offset` |
| `read_file` | Read a file with line numbers. Capped at 2000 lines or ~10k tokens; the header shows the total line count and a cut response ends with `next start_line=N`. Lines over 1000 chars are shortened. Files with NUL bytes, paths inside `.git/` and paths outside the project are refused; `start_line` past the end or `end_line < start_line` is an error (`end_line` past the end is clamped). | `path` (required), `start_line`, `end_line`, `max_tokens` |
| `index_status` | Report index health. Returns JSON: `state` (`"fresh"`, `"stale"`, or `"missing"`), `changed_files` (count, present only when `state` is `"stale"`), `indexed_files`, `index_size_bytes`, `index_path`, and `oversize_files` (`count`, `limit_bytes`, `examples`; present only when files over the max file size, `XGREP_MAX_FILESIZE` (default 10MB), were left out of the index). | none |
| `build_index` | Build or rebuild the search index for the codebase. | none |
| `session_reset` | Forget which ranges were already returned (see *Session dedup*). | none |

Unknown argument names are ignored, and the response starts with a one-line
`note: ignored unknown argument(s): paht (did you mean path_pattern?)` (a
`"note"` field for `index_status`). Unknown `file_type`s and unknown
`output_mode`s are errors that name the valid choices.

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

### Syntax-aware tools

`outline`, `find_references`, `repo_map`, `search` with `expand: "block"`
and the tree-sitter backend of `find_definitions` share one per-file symbol
table built with tree-sitter (each grammar's `tags.scm` plus a few extra
patterns). Supported: Rust, Python, JavaScript/JSX, TypeScript/TSX, Go, Java,
C, C++ (`.h` is parsed as C++) and Ruby; Markdown files contribute their
headings. Files over 1 MB are skipped.

The tables are built lazily, only for the files a call needs, in parallel,
and cached in a sidecar file next to the index (`<index>.tags`), keyed by
path, mtime and size, so an edited file is re-parsed on the next call and
the main index format is unaffected. `repo_map` needs every source file, so
its first call on a large repo pays the parse (about 3 s for 10k files, see
below); later calls reuse the cache.

**`outline`** (`xg --outline FILE` on the CLI):

```
## rust/src/session.rs (rust, 73 lines, 8 symbols)
L13-15 pub struct Session
L17-55 impl Session
  L18-20 pub fn new() -> Self
  L39-54 pub fn mark(&mut self, file: &str, start: usize, end: usize)
L58-73 mod tests
  L62-72 fn mark_merges_and_finds()
```

**`search` with `expand: "block"`** (`--expand block` on the CLI) returns the
smallest enclosing definition of each match instead of a line. Matches in the
same block are merged (marked with `>`); blocks longer than 80 lines keep the
signature, ±2 lines around each match and the closing line, with `⋮ (N lines)`
for the gaps. Matches outside any definition (or in unsupported languages) get
±3 lines. Pagination counts blocks; `max_tokens` and session dedup apply as
usual:

```
## app/auth.py:2-5 method AuthService.login (2 matches)
2      def login(self, user, pw):
3>         if not check_password(pw):
4              return False
5>         return check_password(pw + user)
```

**`find_references`**:

```
Found 4 references to `validate_token` in 3 files (excluded 1 definition line and 1 import line; ...).
Callers (3): Server.handle_login (src/server.rs x2), boot (src/lib.rs), token_ok (tests/server_test.rs)
src/server.rs
  L7 in Server.handle_login: let ok = validate_token(token);
```

**`repo_map`** follows Aider's repo map: a file that uses identifier `x` links
to every file defining `x` (weight `sqrt(uses)`, x10 for long specific names
and `focus_symbols`, x0.1 for `_private`/very short names and names defined
in more than 5 files); PageRank, restarted at `focus` files when given, ranks
files, and each file's rank is spread over the definitions it uses. Test,
example and doc files are down-weighted. The best definitions are printed as
`L<line> <signature>` under their file, with their enclosing impl/class for
context, until `max_tokens` is reached.

Measured over MCP on a 28-thread WSL2 machine (release build, warm OS
cache). *Cold* = no `.tags` yet; *warm* = a new server process with the
sidecar on disk (the first syntax call of a process loads it: ~90 ms for the
93 MB registry sidecar, included in the first warm `outline`).

| Repo | Call | Cold | Warm |
|------|------|------|------|
| xgrep (60 source files) | plain `search` (word regex), for comparison | 4 ms | 3 ms |
| xgrep | `outline` (2,100-line file) | 20 ms | 0.4 ms |
| xgrep | `find_definitions` | 29-44 ms | 5-6 ms |
| xgrep | `find_references` / `search` `expand:"block"` | 8 / 6 ms | 5 / 7 ms |
| xgrep | `repo_map` | 17 ms | 2.4 ms |
| cargo registry (10,050 source files, 370 MB) | plain `search` (word regex) | 15 ms | 12 ms |
| cargo registry | `outline` (5,300-line file) | 27 ms | 89 ms first call (sidecar load), then <1 ms |
| cargo registry | `find_definitions` (`Deserializer`, ~300 candidate files) | 87 ms | 16 ms |
| cargo registry | `find_references` / `search` `expand:"block"` | 46 / 42 ms | 14 / 13 ms |
| cargo registry | `repo_map` (1.1M reference edges) | 3.0 s | 340-370 ms |

On the registry the sidecar is 93 MB and a server that has loaded it uses
~390 MB RSS (125 MB for search alone). Newly parsed files are appended to
`<index>.tags.log`, which is folded into the main file once it reaches a
quarter of its size, so small calls never rewrite a large cache.

Builds with `--no-default-features` leave tree-sitter out: `outline` then
only handles Markdown, `repo_map` reports that syntax support is missing,
`find_definitions`/`find_references` use the regex heuristics and
`expand: "block"` falls back to ±3 line windows.

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

# 6. Whole enclosing functions instead of lines, and a file's shape without bodies
xg "validate_token" /path --expand block --format llm
xg --outline src/server.rs
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
