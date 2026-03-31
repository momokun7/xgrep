# xgrep

[![CI](https://github.com/momokun7/xgrep/actions/workflows/ci.yml/badge.svg)](https://github.com/momokun7/xgrep/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/xgrep-search.svg)](https://crates.io/crates/xgrep-search)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Ultra-fast indexed code search engine with MCP server for AI coding tools.

Pre-builds a trigram inverted index, then searches in milliseconds. Designed for repeated searches on large codebases — by humans and AI agents alike.

## Features

- **Indexed search** — trigram inverted index makes repeated searches 2-59x faster than ripgrep
- **File discovery** — `--find` mode locates files 4-36x faster than fd/find
- **MCP server** — built-in [Model Context Protocol](https://modelcontextprotocol.io/) server for AI coding tools (Claude Code, Cursor, etc.)
- **LLM-optimized output** — `--format llm` produces Markdown with language tags, context lines, and token-aware truncation
- **Git-aware** — search only changed files (`--changed`), recent commits (`--since 1h`), respects `.gitignore`
- **Zero config** — `cargo install xgrep-search`, then `xg "pattern"`. Index builds automatically on first search
- **Hybrid search** — serves results from index instantly while rebuilding in the background

## Why xgrep?

| | ripgrep | zoekt | xgrep |
|---|---------|-------|-------|
| Setup | None | Server required | None (`cargo install`) |
| First search | Instant | After server start | Auto-builds index |
| Repeated search (Linux kernel) | 2,236ms | 170ms (server) | 38ms |
| File discovery (next.js, 26K files) | N/A | N/A | 13ms (fd: 290ms) |
| Index size | N/A | 155% of source | 8% of source |
| AI agent integration | None | None | MCP server built-in |
| Memory (search) | 11MB | 288MB | 208MB |

xgrep is not a ripgrep replacement. Use ripgrep for one-off searches. Use xgrep when you search the same codebase repeatedly — the index pays for itself after ~2 searches.

## Quick Start

```bash
cargo install xgrep-search    # Installs the `xg` command
xg "pattern"                  # Search (auto-builds index on first run)
```

Requires Rust 1.85+. Works on macOS, Linux, and Windows.

<details>
<summary>Build from source</summary>

```bash
git clone https://github.com/momokun7/xgrep.git
cd xgrep/rust
cargo build --release
cp target/release/xg ~/.local/bin/
```
</details>

## Usage

```bash
xg "pattern"                  # Fixed string search
xg "pattern" /path/to/repo    # Search a specific directory
xg -e "handle_\w+"            # Regex search
xg "pattern" -t rs            # Filter by file type
xg "pattern" -C 3             # Context lines
xg "pattern" --format llm     # Markdown output for LLMs
xg "pattern" --changed        # Only git changed files
xg --find "*.rs"              # Find files by glob pattern
xg init                       # Explicitly rebuild index
```

Run `xg --help` for all options.

## MCP Server

xgrep runs as an [MCP](https://modelcontextprotocol.io/) server, giving AI coding tools fast indexed search.

```bash
xg serve                        # Start MCP server
xg serve --root /path/to/repo   # Specific directory
```

### Claude Code

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

**Available tools:** `search`, `find_definitions`, `read_file`, `index_status`, `build_index`

## Performance

Benchmarked with [hyperfine](https://github.com/sharkdp/hyperfine) on Apple M4, 32GB RAM, macOS. All numbers are warm cache, after index build.

### Search: Linux kernel (92,947 files, 2.0GB)

| Query | xg | ripgrep | vs ripgrep |
|-------|-----|---------|------------|
| `struct file_operations` | 38ms | 2,236ms | **59x faster** |
| `printk` | 54ms | 1,795ms | **33x faster** |
| `EXPORT_SYMBOL` | 70ms | 1,900ms | **27x faster** |

### File discovery: next.js (26,424 files)

| Query | xg --find | fd | vs fd |
|-------|-----------|-----|-------|
| `*.ts` (4,639 files) | 12.9ms | 289.7ms | **22x faster** |
| `config` (substring) | 6.4ms | 228.9ms | **36x faster** |

### Index cost

| Metric | xgrep | zoekt |
|--------|-------|-------|
| Build time (Linux kernel) | 6s | 46s |
| Index size | 175MB (8% of source) | 3.0GB (155%) |
| Breakeven | ~2 searches | — |

> First run includes a one-time index build. See [docs/benchmarks.md](docs/benchmarks.md) for full results including medium/small repos.

## Limitations

- **Short queries (< 3 chars)** bypass the index — no speed advantage over ripgrep
- **Index staleness** — background rebuild runs every ~30s. Use `--fresh` for up-to-date results
- **find_definitions** uses regex heuristics, not AST analysis — false positives expected

When to use ripgrep instead: one-off searches, very small codebases (< 100 files), or queries shorter than 3 characters.

## How It Works

1. **Index Build**: Walks the codebase, extracts 3-byte trigrams from each file, builds an inverted index with delta+varint compression
2. **Search**: Extracts trigrams from query, intersects posting lists to find candidate files, verifies matches
3. **Hybrid Mode**: Combines index results with direct scanning of changed files when index is stale
4. **MCP Server**: Exposes search via JSON-RPC over stdio, with token-aware truncation

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup and guidelines.

## License

MIT
