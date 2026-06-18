# xgrep

[![CI](https://github.com/momokun7/xgrep/actions/workflows/ci.yml/badge.svg)](https://github.com/momokun7/xgrep/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/xgrep-search.svg)](https://crates.io/crates/xgrep-search)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Ultra-fast indexed code search engine with MCP server for AI coding tools.

Pre-builds a trigram inverted index, then searches in milliseconds. Designed for repeated searches on large codebases — by humans and AI agents alike.

## Features

- **Indexed search** — trigram inverted index makes repeated searches 2-46x faster than ripgrep
- **File discovery** — `--find` mode locates files 2-15x faster than fd
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
| Repeated search (Linux kernel) | 1,687ms | 170ms (server) | 37ms |
| File discovery (next.js, 26K files) | N/A | N/A | 9ms (fd: 191ms) |
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
xg "pattern"                  # Smart-case search (all-lowercase = case-insensitive)
xg "Pattern"                  # Mixed/upper case in pattern = case-sensitive
xg "pattern" -i               # Force case-insensitive
xg "pattern" -s               # Force case-sensitive (disable smart-case)
xg "pattern" /path/to/repo    # Search a specific directory
xg -e "handle_\w+"            # Regex search
xg "pattern" -w               # Match whole words only
xg "pattern" -t rs            # Filter by file type
xg "pattern" -C 3             # Context lines (symmetric)
xg "pattern" -A 2 -B 1        # 2 lines after, 1 line before
xg "pattern" -g "*.rs"        # Include only paths matching glob (repeatable)
xg "pattern" -g "!*_test.rs"  # Exclude paths matching glob (! prefix)
xg "pattern" --format llm     # Markdown output for LLMs
xg "pattern" --changed        # Only git changed files
xg "pattern" --exclude vendor  # Exclude paths containing "vendor"
xg "pattern" --absolute-paths # Show absolute paths
xg "pattern" --no-hints       # Suppress regex pattern hints
xg --find "*.rs"              # Find files by glob pattern
xg --list-types               # Show supported file types
xg status                     # Show index status
xg init                       # Explicitly rebuild index
```

Search is **smart-case** by default: an all-lowercase pattern matches case-insensitively, while any uppercase letter makes the search case-sensitive. Use `-i` or `-s` to override (priority: `-i` > `-s` > smart-case).

### Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `XGREP_LLM_CONTEXT` | Default context lines for `--format llm` | `3` |
| `XGREP_ABSOLUTE_PATHS` | Set to `1` to always use absolute paths | unset |
| `XGREP_NO_HINTS` | Set to `1` to suppress regex pattern hints | unset |

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

See [docs/agents.md](docs/agents.md) for agent-oriented usage patterns.

## Performance

> **Measurement environment (tables below):** Apple M4, 32GB RAM, NVMe SSD, macOS 15.
> Warm filesystem cache, `hyperfine --warmup 5 --runs 20`. Results on other hardware — especially
> shared Linux machines or HDD — will differ; see [Reproducibility](#reproducibility) below.

### Search: Linux kernel (92,947 files, 2.0GB)

| Query | xg | ripgrep | vs ripgrep | Pattern type |
|-------|-----|---------|------------|--------------|
| `struct file_operations` | 37ms | 1,687ms | **46x faster** | focused |
| `printk` | 52ms | 1,756ms | **34x faster** | focused |
| `EXPORT_SYMBOL` | 66ms | 1,773ms | **27x faster** | focused |

The queries above were selected for xgrep's sweet spot (trigram index filters well).
For distributed patterns that appear across most files, xgrep approaches ripgrep speed —
see the [Reproducibility](#reproducibility) section for honest numbers.

### File discovery: next.js (27,332 files)

| Query | xg --find | fd | vs fd |
|-------|-----------|-----|-------|
| `*.ts` (4,838 files) | 20.8ms | 187.3ms | **9x faster** |
| `config` (substring) | 12.7ms | 188.1ms | **15x faster** |

### Index cost

| Metric | xgrep | zoekt |
|--------|-------|-------|
| Build time (Linux kernel) | 6s | 46s |
| Index size | 175MB (8% of source) | 3.0GB (155%) |
| Breakeven | ~2 searches | — |

> First run includes a one-time index build. See [docs/benchmarks.md](docs/benchmarks.md) for full results including medium/small repos.

### Reproducibility

The numbers above were measured on the author's Apple M4 with a warm filesystem cache.
Third-party benchmarks on shared Linux machines (different CPU, spinning disk, cold cache)
show a different picture depending on pattern type:

| Query | hits | M4 result | Third-party Linux (example) |
|-------|------|-----------|------------------------------|
| `CONFIG_PREEMPT_RT` | ~300 | xg wins big | xg wins big (cache fits in L3) |
| `EXPORT_SYMBOL_GPL` | ~21k | xg wins | xg faster, but gap narrows |
| `raw_spin_lock_irqsave` | ~1.2k | xg wins | xg ≈ ripgrep (互角) |
| `devm_kzalloc` | ~7.4k | xg wins | **xg slower than ripgrep** |

The pattern: when trigram intersection leaves many candidate files (widely-used symbols),
xgrep must scan nearly the whole codebase — the index overhead then outweighs the savings.
This is a known limitation being investigated. Use `bench/fair-bench.sh` to measure on
your own hardware; the script runs both focused and distributed queries for a complete picture.

## Limitations

- **Short queries (< 3 chars)** bypass the index — no speed advantage over ripgrep
- **Tiny files (< 3 bytes)** hold no trigrams and are invisible to indexed content search — a deliberate trade-off of the trigram index
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
