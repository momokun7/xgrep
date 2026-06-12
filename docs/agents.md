# Using xgrep from AI Agents

xgrep is an indexed code search engine fast enough to call inside an agent loop
(typically tens of milliseconds on large repos), with token-aware Markdown
output and a built-in MCP server so AI coding tools can search a codebase
natively instead of re-scanning it on every query.

## Quick reference (CLI)

```bash
xg "pattern" /path --format llm   # Markdown output sized for context windows
```

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

| Tool | Purpose | Key parameters |
|------|---------|----------------|
| `search` | Trigram-indexed pattern search returning matching lines with context. | `pattern` (required), `regex`, `case_insensitive`, `file_type`, `path_pattern`, `max_results` (default 20), `context_lines` (default 3), `max_tokens` (default 4000) |
| `find_definitions` | Locate likely symbol definitions via regex heuristics (`fn`/`struct`/`class`/`def`…). Not AST-based; may include false positives. | `symbol` (required), `file_type`, `path_pattern` |
| `read_file` | Read a file (optionally a line range) with line numbers. Use after `search` for full context. | `path` (required, relative to project root), `start_line`, `end_line` |
| `index_status` | Report index health. Returns JSON: `state` (`"fresh"`, `"stale (N changed files)"`, or `"missing"`), `indexed_files`, `index_size_bytes`, `index_path`. | none |
| `build_index` | Build or rebuild the search index for the codebase. | none |

The `index_status` JSON lets an agent branch on freshness without parsing prose:
call `build_index` when `state` is `"missing"`, or proceed directly when `state`
is `"fresh"`.

## Recipes for agents

```bash
# 1. Find where a symbol is defined (definitions only, not call sites)
xg -e "(?:pub\s+)?(?:fn|struct|enum|trait)\s+TokenStream\b" /path -t rs --format llm
# (the find_definitions MCP tool wraps this heuristic for you)

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
use xgrep_search::{Xgrep, SearchOptions, IndexState};

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
