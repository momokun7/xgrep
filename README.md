# xgrep

Trigram inverted index-based code search tool. Pre-builds an index of your codebase, then searches it in milliseconds.

## Performance

Benchmarked on Linux kernel source (92,471 files, 2.0GB) with [hyperfine](https://github.com/sharkdp/hyperfine), Apple M4, 32GB RAM, macOS 26.2.

### Search Latency (warm)

| Query | xgrep | ripgrep | zoekt (CLI) | xgrep vs ripgrep |
|-------|-------|---------|-------------|------------------|
| `struct file_operations` | 38ms | 1,865ms | 170ms | **50x faster** |
| `printk` | 64ms | 1,905ms | 94ms | **30x faster** |
| `EXPORT_SYMBOL` | 70ms | 1,897ms | 93ms | **27x faster** |

### Medium Project (ripgrep source, 209 files, 4.3MB)

| Query | xgrep | ripgrep | xgrep vs ripgrep |
|-------|-------|---------|------------------|
| `fn main` | 2.6ms | 7.3ms | **2.8x faster** |
| `Options` | 2.8ms | 8.2ms | **2.9x faster** |

### Index Cost

| Metric | xgrep | zoekt | ripgrep |
|--------|-------|-------|---------|
| Index build time | 22s | 46s | N/A |
| Index size | 167MB (8% of source) | 3.0GB (155%) | N/A |
| Build memory (peak footprint) | 160MB | 2.24GB | N/A |
| Search memory | 208MB | 288MB | 11MB |
| Cold start (build + search) | 23s | 41s | 1.6s |
| **Breakeven** | **12 searches** | - | - |

xgrep requires an upfront index build. After 12 searches, the total time is less than using ripgrep from the start. ripgrep requires no setup and is the better choice for one-off searches.

zoekt is designed as a persistent server (zoekt-webserver). The numbers above use its CLI mode, which adds process startup overhead. In server mode, zoekt search latency would be significantly lower.

## Install

```bash
git clone https://github.com/momokun7/xgrep.git
cd xgrep/rust
cargo build --release
# Binary at: target/release/xgrep
```

Optionally, copy to PATH:

```bash
cp target/release/xgrep ~/.local/bin/
```

### Requirements

- Rust 1.70+
- macOS or Linux

## Usage

```bash
# Build index (first time, or after major changes)
xgrep init

# Search
xgrep "pattern"

# If no index exists, xgrep builds one automatically
```

### Options

```
xgrep "pattern"                     # Fixed string search
xgrep -e "handle_\w+"               # Regex search
xgrep "pattern" -i                  # Case-insensitive
xgrep "pattern" --type rs           # Filter by file type
xgrep "pattern" -C 3                # Show context lines
xgrep "pattern" --format llm        # Markdown output for LLMs
xgrep "pattern" --changed           # Only search git changed files
xgrep "pattern" --since 1h          # Only search recently changed files
xgrep init --local                  # Store index in .xgrep/ instead of cache
```

### Output Formats

**Default** (ripgrep-compatible):

```
src/main.rs:42:fn handle_auth() {}
```

**LLM** (`--format llm`):

````
## src/main.rs:40-44

```rust
fn handle_auth(req: Request) -> Response {
    let token = req.header("Authorization");
    validate_token(token)?;
}
```
````

### Git Integration

```bash
xgrep "TODO" --changed           # Unstaged + staged changes
xgrep "TODO" --since 1h          # Last hour
xgrep "TODO" --since 2d          # Last 2 days
xgrep "TODO" --since 3.commits   # Last 3 commits
```

### File Types

`--type` accepts: `rs`, `py`, `js`, `ts`, `go`, `rb`, `java`, `c`, `cpp`, `sh`, `json`, `yaml`, `md`, `html`, `css`, `sql`, `toml`, `xml`

## How It Works

1. **Index build** (`xgrep init`): Walks the directory tree (respecting `.gitignore`), extracts [trigrams](https://swtch.com/~rsc/regexp/regexp4.html) (3-byte substrings) from each file, builds an inverted index mapping trigrams to file IDs. Uses delta + varint compression for posting lists.

2. **Search** (`xgrep "pattern"`): Extracts trigrams from the search pattern, looks up each trigram's posting list in the index, intersects them to find candidate files, then verifies matches by reading only those files. This avoids scanning the entire codebase.

3. **Optimizations**: mmap for zero-cost index loading, madvise(WILLNEED) for prefetch, rayon for parallel file I/O, memchr/memmem for SIMD string matching, LTO + mimalloc allocator, counting sort with mmap temp file for low-memory index builds.

## Index Storage

By default, indexes are stored in `~/Library/Caches/xgrep/<hash>/` (macOS) or `~/.cache/xgrep/<hash>/` (Linux). The hash is derived from the project directory path.

Use `xgrep init --local` to store the index in `.xgrep/` within the project (add `.xgrep/` to `.gitignore`).

## Reproducing Benchmarks

### Prerequisites

```bash
# Required
cargo install hyperfine
brew install ripgrep          # or: cargo install ripgrep

# Optional (for zoekt comparison)
go install github.com/sourcegraph/zoekt/cmd/zoekt-index@latest
go install github.com/sourcegraph/zoekt/cmd/zoekt@latest
```

### Quick Comparison

```bash
cd xgrep/rust && cargo build --release
cd /path/to/large/project
xgrep init
hyperfine --warmup 5 --runs 20 \
  'xgrep "your_pattern"' \
  'rg "your_pattern" .'
```

### Full Benchmark Suite

```bash
# Clone Linux kernel as test dataset
git clone --depth 1 https://github.com/torvalds/linux.git bench/linux-src

# Clone ripgrep as medium dataset
git clone --depth 1 https://github.com/BurntSushi/ripgrep bench/ripgrep-src

# Run the full benchmark (takes ~20 minutes)
FORCE_CONTINUE=1 bash bench/fair-bench.sh

# Results saved to bench/results/report_<timestamp>.md
```

The benchmark script (`bench/fair-bench.sh`) includes:

1. **Correctness verification** — Compares xgrep match counts against ripgrep
2. **Index build cost** — Time, memory, and index size for xgrep and zoekt
3. **Cold start** — First-time user experience (build + search vs ripgrep's instant search)
4. **Warm search latency** — hyperfine with 5 warmups, 20 runs, on 3 dataset sizes (large/medium/small)
5. **Memory usage** — Peak RSS for build and search
6. **Breakeven analysis** — Number of searches needed to recoup index build cost
7. **Summary** — Markdown table with all results and caveats

### Environment

Benchmark results vary by hardware. Key factors: CPU (single-thread speed for search, core count for build), SSD speed (for index build I/O), and RAM (for OS page cache warmth).

## Limitations

- Index must be rebuilt when files change (no automatic incremental updates yet)
- Index build uses ~160MB peak memory (footprint) for large codebases
- No color output
- Regex literal extraction is basic (simple heuristic, not full regex analysis)
- Not yet published on crates.io

## License

MIT
