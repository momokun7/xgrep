# xgrep

Ultra-fast indexed code search engine for Node.js. Powered by [trigram inverted index](https://swtch.com/~rsc/regexp/regexp4.html) in Rust via [napi-rs](https://napi.rs/).

27-59x faster than ripgrep on repeated searches.

## Install

```bash
npm install xgrep
```

Pre-built binaries are available for:
- Linux (x86_64, ARM64)
- macOS (x86_64, ARM64)
- Windows (x86_64)

## Usage

```typescript
import { Xgrep } from 'xgrep';

// Open a directory and build the index
const xg = Xgrep.open('/path/to/your/repo');
xg.buildIndex();

// Search for a pattern
const results = xg.search('fn main');
for (const r of results) {
  console.log(`${r.file}:${r.lineNumber}: ${r.line}`);
}

// Search with options
const filtered = xg.search('TODO', {
  fileType: 'ts',          // Filter by file type
  caseInsensitive: true,   // Case-insensitive search
  maxCount: 10,            // Limit results
  pathPattern: 'src/',     // Filter by path
});

// Regex search
const regexResults = xg.search('fn\\s+\\w+', { regex: true });

// Index status
console.log(xg.indexStatus());
```

## API

### `Xgrep.open(root: string): Xgrep`

Open a directory for searching. Index location is auto-resolved to `~/.cache/xgrep/`.

### `Xgrep.openLocal(root: string): Xgrep`

Open with local index storage (`.xgrep/` in the project root).

### `xg.buildIndex(): void`

Build or rebuild the search index.

### `xg.search(pattern: string, opts?: SearchOptions): SearchResult[]`

Search for a pattern in the indexed codebase.

**SearchOptions:**

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `caseInsensitive` | `boolean` | `false` | Case-insensitive search (ASCII-only) |
| `regex` | `boolean` | `false` | Treat pattern as regex |
| `fileType` | `string` | - | Filter by file type (e.g., `"rs"`, `"py"`, `"js"`) |
| `maxCount` | `number` | - | Maximum number of results |
| `changedOnly` | `boolean` | `false` | Only search git-changed files |
| `since` | `string` | - | Files changed within duration (`"1h"`, `"2d"`, `"3.commits"`) |
| `pathPattern` | `string` | - | Filter by path substring |
| `fresh` | `boolean` | `false` | Check index freshness before searching |

**SearchResult:**

| Field | Type | Description |
|-------|------|-------------|
| `file` | `string` | File path relative to root |
| `lineNumber` | `number` | Line number (1-based) |
| `line` | `string` | The matching line content |

### `xg.indexStatus(): string`

Get the current index status.

### `xg.root: string` (getter)

Get the root directory path.

### `xg.indexPath: string` (getter)

Get the index file path.

## How It Works

xgrep builds a trigram inverted index of your codebase. On the first search, the index is built automatically. Subsequent searches use the index to narrow down candidate files before scanning, making repeated searches extremely fast.

| Benchmark | xgrep | ripgrep | Speedup |
|-----------|-------|---------|---------|
| Linux kernel (92K files) | 38ms | 2,236ms | **59x** |
| ripgrep source (248 files) | 2.5ms | 7.9ms | **3.1x** |

## CLI

xgrep also provides a CLI tool. Install via Rust:

```bash
cargo install xgrep-search
xg "pattern" --type rs
```

## License

MIT
