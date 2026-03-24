# xgrep

Ultra-fast indexed code search engine with MCP server for AI coding tools.

Pre-builds a trigram inverted index, then searches in milliseconds. Designed for repeated searches on large codebases — by humans and AI agents alike.

## Why xgrep?

| | ripgrep | zoekt | xgrep |
|---|---------|-------|-------|
| Setup | None | Server required | None (`cargo install`) |
| First search | Instant | After server start | Auto-builds index |
| Repeated search (Linux kernel) | 2,070ms | 170ms (server) | 38ms |
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
| `find_definitions` | Find function/struct/class definitions across languages |
| `read_file` | Read file contents with optional line range |
| `index_status` | Check index freshness and stats |
| `build_index` | Explicitly rebuild index |

## Performance

Benchmarked on Linux kernel source (92,471 files, 2.0GB) with [hyperfine](https://github.com/sharkdop/hyperfine), Apple M4, 32GB RAM.

### Search Latency (warm cache)

| Query | xgrep | ripgrep | vs ripgrep |
|-------|-------|---------|------------|
| `struct file_operations` | 38ms | 2,070ms | **55x faster** |
| `printk` | 53ms | 2,026ms | **39x faster** |
| `EXPORT_SYMBOL` | 68ms | 2,177ms | **32x faster** |

### Medium Project (ripgrep source, 248 files)

| Query | xgrep | ripgrep | vs ripgrep |
|-------|-------|---------|------------|
| `fn main` | 18ms | 8ms | ripgrep 2.2x faster |
| `Options` | 19ms | 8ms | ripgrep 2.5x faster |

> On small/medium codebases, ripgrep is faster due to xgrep's index loading overhead. xgrep's advantage grows with codebase size.

### Index Cost

| Metric | xgrep | zoekt | ripgrep |
|--------|-------|-------|---------|
| Build time | 6s | 46s | N/A |
| Index size | 175MB (8%) | 3.0GB (155%) | N/A |
| Breakeven | ~10 searches | - | - |

> zoekt numbers are CLI mode. In server mode, zoekt search latency is significantly lower.

Reproduce these benchmarks on your machine: [`bench/run.sh`](bench/run.sh)

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

## How It Works

1. **Index Build**: Walks the codebase, extracts 3-byte trigrams from each file, builds an inverted index (trigram -> file IDs) with delta+varint compression
2. **Search**: Extracts trigrams from query, intersects posting lists to find candidate files, verifies matches
3. **Hybrid Mode**: When the index is stale, combines index results with direct scanning of changed files — no rebuild needed
4. **MCP Server**: Exposes search via JSON-RPC over stdio, with LLM-optimized output and token-aware truncation

## License

MIT
