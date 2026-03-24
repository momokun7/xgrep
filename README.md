# xgrep

Ultra-fast indexed code search engine with MCP server for AI coding tools.

Pre-builds a trigram inverted index, then searches in milliseconds. Designed for repeated searches on large codebases — by humans and AI agents alike.

## Why xgrep?

| | ripgrep | zoekt | xgrep |
|---|---------|-------|-------|
| Setup | None | Server required | None (`cargo install`) |
| First search | Instant | After server start | Auto-builds index |
| Repeated search (Linux kernel) | 2,236ms | 170ms (server) | 38ms |
| Index size | N/A | 155% of source | 8% of source |
| AI agent integration | None | None | MCP server built-in |
| Memory (search) | 11MB | 288MB | 208MB |

xgrep is not a ripgrep replacement. Use ripgrep for one-off searches. Use xgrep when you search the same codebase repeatedly — the index pays for itself after ~12 searches.

## Install

```bash
# From source
git clone https://github.com/momokun7/xgrep.git
cd xgrep/rust
cargo build --release
cp target/release/xg ~/.local/bin/
```

### Requirements

- Rust 1.85+
- macOS or Linux

## Quick Start

```bash
xg "pattern"              # Search (auto-builds index on first run)
xg -e "handle_\w+"        # Regex search
xg "pattern" -i            # Case-insensitive
xg "pattern" --type rs     # Filter by file type
xg "pattern" -C 3          # Context lines
xg "pattern" --format llm  # Markdown output for LLMs
xg "pattern" --changed     # Only git changed files
xg "pattern" --since 1h    # Recently changed files
xg init                    # Explicitly build index
```

## MCP Server for AI Agents

xgrep runs as an [MCP](https://modelcontextprotocol.io/) server, giving AI coding tools fast indexed search.

```bash
xg serve                        # Start MCP server
xg serve --root /path/to/repo   # Specific directory
```

### Claude Code

Add to settings:

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

### Available Tools

| Tool | Description |
|------|-------------|
| `search` | Text/regex search with context. Auto-builds index. Max 4000 tokens by default. |
| `find_definitions` | Find likely definitions by regex heuristics (may include false positives) |
| `read_file` | Read file contents with optional line range |
| `index_status` | Check index freshness and stats |
| `build_index` | Explicitly rebuild index |

## Performance

Benchmarked with [hyperfine](https://github.com/sharkdp/hyperfine) on Apple M4, 32GB RAM, macOS. **All numbers are warm cache, after index build.** First run includes a one-time index build (~6s for Linux kernel). See [Index Cost](#index-cost) for details.

### Large: Linux kernel (92,947 files, 2.0GB)

| Query | xg | ripgrep | vs ripgrep |
|-------|-----|---------|------------|
| `struct file_operations` | 38ms | 2,236ms | **59x faster** |
| `printk` | 54ms | 1,795ms | **33x faster** |
| `EXPORT_SYMBOL` | 70ms | 1,900ms | **27x faster** |

### Medium: ripgrep source (248 files, 4.3MB)

| Query | xg | ripgrep | vs ripgrep |
|-------|-----|---------|------------|
| `fn main` | 2.5ms | 7.9ms | **3.1x faster** |
| `Options` | 2.3ms | 7.7ms | **3.3x faster** |
| `pub struct` | 2.6ms | 7.8ms | **3.1x faster** |

### Small: xgrep source (17 files)

| Query | xg | ripgrep | vs ripgrep |
|-------|-----|---------|------------|
| `fn main` | 2.1ms | 5.2ms | **2.5x faster** |
| `SearchResult` | 1.6ms | 4.7ms | **2.9x faster** |
| `Matcher` | 2.2ms | 5.0ms | **2.3x faster** |

### Index Cost

| Metric | xgrep | zoekt | ripgrep |
|--------|-------|-------|---------|
| Build time | 6s | 46s | N/A |
| Index size | 175MB (8%) | 3.0GB (155%) | N/A |
| Breakeven | ~2 searches | - | - |

> zoekt numbers are CLI mode. In server mode, zoekt search latency is significantly lower.

Reproduce these benchmarks: [`bench/run.sh`](bench/run.sh)

```bash
./bench/run.sh small    # xgrep source (~20 files, 30s)
./bench/run.sh medium   # ripgrep source (~250 files, auto-downloads)
./bench/run.sh large    # Linux kernel (~92K files, requires manual download)
```

## Output Formats

**Default** (ripgrep-compatible):
```
src/main.rs:42:fn handle_auth() {}
```

**LLM** (`--format llm`): Markdown code blocks with language tags and context lines.

**JSON** (`--json`): Structured output for programmatic use.

### Regex Performance Notes

xgrep extracts trigram literals from regex patterns to narrow search candidates before full regex matching. This works well for patterns with literal substrings but falls back to full scan for purely abstract patterns.

**Fast (trigram-optimized):**

| Pattern | Why | Trigrams extracted |
|---------|-----|--------------------|
| `handle_\w+` | Literal prefix "handle_" | `han`, `and`, `ndl`, `dle`, `le_` |
| `fn\s+main` | Literal parts "fn" and "main" | `mai`, `ain` |
| `error.*timeout` | Literals "error" and "timeout" | Both sets |

**Slow (full scan fallback):**

| Pattern | Why |
|---------|-----|
| `.*` | No literals |
| `[a-z]+` | Only character classes |
| `\d{3}-\d{4}` | No literal strings |
| `.+error` | Leading `.+` prevents extraction |

For patterns that fall back to full scan, xgrep will show a warning: `warning: regex cannot be optimized with trigram index (full scan)`.

**Tip:** Include at least 3 literal characters in your regex for best performance. `handle_\w+` is much faster than `\w+_auth`.

## Limitations

xgrep uses a [trigram inverted index](https://swtch.com/~rsc/regexp/regexp4.html), the same technique as Google Code Search (2006) and zoekt. This approach has inherent trade-offs:

- **Short queries (< 3 chars) bypass the index**: Patterns like `if`, `fn`, `go` fall back to full file scan with no speed advantage over ripgrep.
- **Common trigrams reduce filtering**: Queries containing frequent trigrams (`the`, `int`, `return`) produce many candidate files, narrowing the speed gap with ripgrep.
- **Not designed for monorepo scale**: Tested up to ~100K files (Linux kernel). Beyond that, posting list intersection costs dominate and the index becomes less effective.
- **Index staleness**: Background rebuild runs every ~30 seconds. Recently saved files may not appear until the next rebuild completes.
- **find_definitions is regex-based**: Uses heuristic patterns (`fn`/`struct`/`class`/`def`), not AST analysis. False positives are expected.
- **ASCII-only case folding**: Case-insensitive search (`-i`) handles ASCII letters only. Unicode case folding is not supported.

### When to use ripgrep instead

- One-off searches on a codebase you won't search again
- Very small codebases (< 100 files, where index overhead outweighs benefit)
- Queries shorter than 3 characters
- When you need results from files saved within the last 30 seconds

### Why trigrams?

xgrep prioritizes **simplicity and small index size** over search precision. Alternative approaches:

| Approach | Index size | Precision | Trade-off |
|----------|-----------|-----------|-----------|
| **Trigram** (xgrep, zoekt) | ~8% of source | Moderate (false positives) | Simple, small, fast to build |
| **Suffix array** (Livegrep) | 2-5x source | High | Large index, slow to build |
| **AST/Symbol** (Searkt, LSP) | Varies | Exact | Language-specific, complex |

Trigrams are the right choice when you want a single binary that works on any codebase without language-specific setup.

## How It Works

1. **Index Build**: Walks the codebase, extracts 3-byte trigrams from each file, builds an inverted index (trigram -> file IDs) with delta+varint compression
2. **Search**: Extracts trigrams from query, intersects posting lists to find candidate files, verifies matches
3. **Hybrid Mode**: When the index is stale, combines index results with direct scanning of changed files — no rebuild needed
4. **MCP Server**: Exposes search via JSON-RPC over stdio, with LLM-optimized output and token-aware truncation

## License

MIT
