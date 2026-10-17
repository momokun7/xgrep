# xgrep

[![crates.io](https://img.shields.io/crates/v/xgrep-search?style=flat-square&label=crates.io)](https://crates.io/crates/xgrep-search)
[![npm](https://img.shields.io/npm/v/xgrep?style=flat-square&label=npm)](https://www.npmjs.com/package/xgrep)
[![PyPI](https://img.shields.io/pypi/v/xgrep-search?style=flat-square&label=PyPI)](https://pypi.org/project/xgrep-search/)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue?style=flat-square)](#license)

Ultra-fast indexed code search engine with MCP server for AI coding tools.

Pre-builds a trigram inverted index, then searches in milliseconds. Designed for repeated searches on large codebases — by humans and AI agents alike.

## Features

- **Indexed search** — trigram inverted index makes repeated searches 2-46x faster than ripgrep
- **File discovery** — `--find` mode locates files 2-15x faster than fd
- **MCP server** — built-in [Model Context Protocol](https://modelcontextprotocol.io/) server for AI coding tools (Claude Code, Cursor, etc.)
- **LLM-optimized output** — `--format llm` produces Markdown with language tags, context lines, and token-aware truncation
- **Git-aware** — search only changed files (`--changed`), recent commits (`--since 1h`), respects `.gitignore`
- **Zero config** — install via cargo/npm/pip, then `xg "pattern"`. Index builds automatically on first search
- **Always fresh** — files edited since the last index build are rescanned on every search; on Linux, `xg serve` watches the tree so this check costs ~0.1ms
- **Syntax-aware** — tree-sitter outlines (`--outline`), enclosing-block results (`--expand block`), definitions, references and a repo map for Rust, Python, JS/TS, Go, Java, C/C++ and Ruby
- **Boolean and history search** — file-level `--and` / `--not`, search inside `git diff` (`--in-diff`) or commit messages (`--log`)

## Why xgrep?

| | ripgrep | zoekt | xgrep |
|---|---------|-------|-------|
| Setup | None | Server required | None (`cargo`/`npm`/`pip`) |
| First search | Instant | After server start | Auto-builds index |
| Repeated search (Linux kernel) | 1,687ms | 170ms (server) | 37ms |
| File discovery (next.js, 26K files) | N/A | N/A | 9ms (fd: 191ms) |
| Index size | N/A | 155% of source | ~13% of source (+6% build-only sidecar) |
| AI agent integration | None | None | MCP server built-in |

xgrep is not a ripgrep replacement. Use ripgrep for one-off searches. Use xgrep when you search the same codebase repeatedly — the index pays for itself after ~2 searches.

## Installation

Choose your package manager:

```bash
# Rust (recommended — native binary, no runtime)
cargo install xgrep-search    # Requires Rust 1.85+

# npm (Node.js 18+)
npm install -g xgrep

# pip (Python 3.8+)
pip install xgrep-search
```

Prebuilt binaries are also available on the [Releases](https://github.com/momokun7/xgrep/releases) page.

The `cargo` build and the release binaries include tree-sitter grammars for the syntax-aware features (`--outline`, `--expand block`, `outline`/`find_references`/`repo_map` over MCP), which need a C compiler to build and add ~12MB to the binary. To leave them out, use `cargo install xgrep-search --no-default-features`; the npm and pip packages are built this way.

## Quick Start

```bash
xg "pattern"                  # Search (auto-builds index on first run)
```

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
xg "pattern" --format llm --max-tokens 4000  # Cap LLM output (approx. tokens)
xg "pattern" -M 200           # Cut lines longer than 200 chars (--max-columns)
xg "pattern" --json           # JSON Lines: one object per match + summary
xg "pattern" --changed        # Only git changed files
xg "pattern" --fresh          # Rebuild a very stale index before searching
xg "foo" --and bar --not test # Files containing foo and bar but not test
xg "pattern" --in-diff main...HEAD  # Only lines added/removed in a git diff
xg "pattern" --log            # Search commit messages
xg "pattern" --expand block   # Show the enclosing function/class per match
xg --outline src/lib.rs       # List symbols without bodies
xg "pattern" dir1 dir2        # Search several paths
cat app.log | xg "error"      # Search standard input (also: xg "error" -)
xg "pattern" --exclude vendor  # Exclude paths containing "vendor"
xg "pattern" --absolute-paths # Show absolute paths
xg "pattern" --no-hints       # Suppress regex pattern hints
xg --find "*.rs"              # Find files by glob pattern
xg --list-types               # Show supported file types
xg status                     # Show index status
xg init                       # Explicitly rebuild index
```

Without a PATH, xg searches standard input when it is a pipe or a file with data; otherwise (a terminal, `/dev/null`, `NUL`, or empty input as CI runners often attach) it searches the current directory. `-` always reads standard input. Matches are shown as `<stdin>:LINE:TEXT`.

Search is **smart-case** by default: an all-lowercase pattern matches case-insensitively, while any uppercase letter makes the search case-sensitive. Use `-i` or `-s` to override (priority: `-i` > `-s` > smart-case).

### Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `XGREP_LLM_CONTEXT` | Default context lines for `--format llm` | `3` |
| `XGREP_ABSOLUTE_PATHS` | Set to `1` to always use absolute paths | unset |
| `XGREP_NO_HINTS` | Set to `1` to suppress regex pattern hints | unset |
| `XGREP_MAX_FILESIZE` | Skip larger files when indexing (`512K`, `20M`, `0` = no limit) | `10M` |
| `XGREP_NO_OVERLAY` | Set to `1` to skip rescanning files changed since the index build | unset |
| `XGREP_NO_WATCH` | Set to `1` to disable file watching in `xg serve` | unset |

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

**Available tools:** `search` (ranked, paginated, multi-query, `expand:"block"`), `find_definitions`, `find_references`, `find_files`, `outline`, `repo_map`, `read_file`, `index_status`, `build_index`, `session_reset`

Results already returned in the session are collapsed to one line (`already shown: path:L10-40`), and every tool honours `max_tokens`.

See [docs/agents.md](docs/agents.md) for agent-oriented usage patterns.

## Performance

> **Measurement environment (tables below):** Apple M4, 32GB RAM, NVMe SSD, macOS 15.
> Warm filesystem cache, `hyperfine --warmup 5 --runs 20`. Results on other hardware — especially
> shared Linux machines or HDD — will differ; see [Reproducibility](#reproducibility) below.

> The Linux kernel, next.js and index-cost tables below were measured with v0.7 (index format v3)
> and have not been re-run for the current index format (v4).

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

### v0.7 → current, same machine

crates.io registry sources (13,818 files, 361MB), WSL2 on a 28-core machine, warm cache, median of 5 runs:

| | v0.7 | current |
|---|---|---|
| Index build | 1.8s | 0.2s |
| Index on disk (incl. sidecar) | 138MB | 70MB |
| `serialize` | 43ms | 20ms |
| `the` (151k matching lines) | 107ms | 40ms |
| `the -l` | 107ms | 22ms |
| `the -g '*.md'` | 160ms | 11ms |

Lowercased trigrams with per-posting position/next-byte masks cut candidate files that do not match by ~77%.

### Reproducibility

The numbers above were measured on the author's Apple M4 with a warm filesystem cache.
Third-party benchmarks on shared Linux machines (different CPU, spinning disk, cold cache)
show a different picture depending on pattern type:

| Query | hits | M4 result | Third-party Linux (example) |
|-------|------|-----------|------------------------------|
| `CONFIG_PREEMPT_RT` | ~300 | xg wins big | xg wins big (cache fits in L3) |
| `EXPORT_SYMBOL_GPL` | ~21k | xg wins | xg faster, but gap narrows |
| `raw_spin_lock_irqsave` | ~1.2k | xg wins | xg ≈ ripgrep (even) |
| `devm_kzalloc` | ~7.4k | xg wins | **xg slower than ripgrep** |

The pattern: when trigram intersection leaves many candidate files (widely-used symbols),
xgrep must scan nearly the whole codebase — the index overhead then outweighs the savings.
This is a known limitation being investigated. Use `bench/fair-bench.sh` to measure on
your own hardware; the script runs both focused and distributed queries for a complete picture.

## Limitations

- **Single-character queries** bypass the index (full scan) — no speed advantage over ripgrep; 2-character literals use a coarser trigram-prefix lookup
- **Large files (> 10MB by default)** are left out of the index; `xg status`, MCP `index_status` and zero-result hints report how many. Set `XGREP_MAX_FILESIZE` (e.g. `50M`, or `0` for no limit) and rebuild to index them
- **Binary files** (any NUL byte, including UTF-16 text) are not indexed
- **Freshness** — edited files are rescanned on each search; with more than 500 changed files the search warns and relies on the background rebuild (`--fresh` rebuilds first)
- **Syntax features** cover Rust, Python, JS/TS, Go, Java, C/C++ and Ruby; other languages fall back to regex heuristics. The npm and Python packages are built without them
- **find_references** is text-based: different symbols with the same name are not told apart

When to use ripgrep instead: one-off searches, very small codebases (< 100 files), or single-character queries.

## How It Works

1. **Index Build**: Walks the codebase, extracts ASCII-lowercased 3-byte trigrams from each file, and builds an inverted index with delta+varint compression plus a 2-byte mask per posting (position mod 8, next-byte bloom)
2. **Search**: Intersects the rarest posting lists first, prunes candidates with the masks, then verifies matches in the files, streaming results in path order
3. **Freshness**: Files changed since the build (found by a parallel stat probe, or by the file watcher in `xg serve`) are excluded from the index scan and searched directly
4. **MCP Server**: Exposes search, syntax tools and file reading via JSON-RPC over stdio, with ranking, pagination, token budgets and session dedup

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup and guidelines.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
