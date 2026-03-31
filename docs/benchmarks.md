# Benchmarks

Benchmarked with [hyperfine](https://github.com/sharkdp/hyperfine) on Apple M4, 32GB RAM, macOS. All numbers are warm cache, after index build.

## Search benchmarks

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

## File discovery benchmarks

### tokio (825 files, Rust async runtime)

| Query | xg --find | fd | find | vs fd |
|-------|-----------|-----|------|-------|
| `*.rs` (769 files) | 2.4ms | 8.9ms | 7.9ms | **3.7x faster** |
| `config` (substring) | 1.9ms | 8.1ms | 8.3ms | **4.3x faster** |

### next.js (26,424 files, React framework)

| Query | xg --find | fd | find | vs fd |
|-------|-----------|-----|------|-------|
| `*.ts` (4,639 files) | 12.9ms | 289.7ms | 606.5ms | **22x faster** |
| `config` (substring) | 6.4ms | 228.9ms | 637.0ms | **36x faster** |

`xg --find` reads file paths from the in-memory index (mmap), while fd/find walk the filesystem. The gap widens with repository size.

## Index cost

| Metric | xgrep | zoekt | ripgrep |
|--------|-------|-------|---------|
| Build time | 6s | 46s | N/A |
| Index size | 175MB (8%) | 3.0GB (155%) | N/A |
| Breakeven | ~2 searches | - | - |

> zoekt numbers are CLI mode. In server mode, zoekt search latency is significantly lower.

## Reproduce

```bash
./bench/run.sh small    # xgrep source (~20 files, 30s)
./bench/run.sh medium   # ripgrep source (~250 files, auto-downloads)
./bench/run.sh large    # Linux kernel (~92K files, requires manual download)
./benchmarks/bench_find.sh  # --find vs fd vs find (auto-clones repos)
```

## Regex optimization

xgrep extracts trigram literals from regex patterns to narrow search candidates before full regex matching.

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

**Tip:** Include at least 3 literal characters in your regex for best performance.

## Technical details

### Why trigrams?

xgrep prioritizes simplicity and small index size over search precision.

| Approach | Index size | Precision | Trade-off |
|----------|-----------|-----------|-----------|
| **Trigram** (xgrep, zoekt) | ~8% of source | Moderate (false positives) | Simple, small, fast to build |
| **Suffix array** (Livegrep) | 2-5x source | High | Large index, slow to build |
| **AST/Symbol** (Searkt, LSP) | Varies | Exact | Language-specific, complex |

Trigrams are the right choice when you want a single binary that works on any codebase without language-specific setup.

### Output formats

**Default** (ripgrep-compatible):
```
src/main.rs:42:fn handle_auth() {}
```

**LLM** (`--format llm`): Markdown code blocks with language tags and context lines.

**JSON** (`--json`): Structured output for programmatic use.

### Environment variables

| Variable | Description | Default |
|----------|-------------|---------|
| `XGREP_LLM_CONTEXT` | Default context lines for `--format llm` | `3` |
| `XGREP_ABSOLUTE_PATHS` | Set to `1` to always use absolute paths | unset |
| `XGREP_NO_HINTS` | Set to `1` to suppress regex pattern hints | unset |

### Exit codes

| Code | Meaning |
|------|---------|
| `0` | Matches found |
| `1` | No matches found (not an error) |
| `2` | Error (invalid pattern, missing index, I/O error, usage error) |

Follows the same convention as ripgrep.
