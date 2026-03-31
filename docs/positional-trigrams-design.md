# Positional Trigrams Design Document

**Status:** Draft
**Issue:** #29
**Target:** Index Format v3

## Background

### Current approach: existence-only trigrams

xgrep's current index (format v2) stores **existence-only** trigrams. For each trigram (3-byte sequence), the posting list records which files contain that trigram, but not *where* within the file it appears.

Search pipeline:

```
Query "hello world"
  -> extract trigrams: ["hel", "ell", "llo", "lo ", "o w", " wo", "wor", "orl", "rld"]
  -> look up posting lists for each trigram
  -> intersect all posting lists -> candidate file IDs
  -> read each candidate file and verify with full string match
```

This means a 11-character literal query requires **9 posting list lookups** and a full N-way intersection. The candidate set may include files that contain all 9 trigrams but not in the correct order or adjacency, leading to **false positives** that must be eliminated by reading the file content.

### Positional trigrams (zoekt approach)

zoekt encodes position information into its posting lists. Instead of just recording "file X contains trigram T", it records "file X contains trigram T at offset P". This enables **distance verification** during the index lookup phase, before reading file content.

For a query "hello world", zoekt only needs the **first and last** trigrams:

```
Query "hello world"
  -> first trigram: "hel" at offset 0
  -> last trigram:  "rld" at offset 8
  -> look up posting lists for "hel" and "rld"
  -> for each file in both lists, check if any (pos_rld - pos_hel) == 8
  -> only files passing the distance check become candidates
```

Key advantages:

- **Fewer posting list lookups:** 2 instead of N-2 for an N-character query
- **Dramatically smaller candidate sets:** distance verification eliminates most false positives at the index level
- **Especially effective for long literals:** the longer the query, the more discriminating the distance check

## Index Format v3

### Current v2 byte layout

```
Header (24 bytes)
  magic: [u8; 4]         = "XGRP"
  version: u32            = 2
  trigram_count: u32
  file_count: u32
  posting_total_bytes: u64

Trigram Table (16 bytes x trigram_count)
  trigram: [u8; 3]
  _padding: u8
  posting_offset: u64
  posting_len: u32

Posting Lists (variable)
  Per trigram:
    count: varint
    file_ids: varint[] (delta-encoded)

File Table (28 bytes x file_count)
String Pool (variable)
```

### Proposed v3 byte layout

```
Header (24 bytes)
  magic: [u8; 4]         = "XGRP"
  version: u32            = 3
  trigram_count: u32
  file_count: u32
  posting_total_bytes: u64    // now includes position data

Trigram Table (16 bytes x trigram_count)   // UNCHANGED
  trigram: [u8; 3]
  _padding: u8
  posting_offset: u64
  posting_len: u32

Posting Lists (variable)                  // CHANGED
  Per trigram:
    count: varint
    For each file entry:
      file_id_delta: varint               // delta-encoded as before
      position_count: varint              // number of positions in this file
      positions: varint[]                 // delta-encoded offsets within file

File Table (28 bytes x file_count)        // UNCHANGED
String Pool (variable)                    // UNCHANGED
```

### Design decisions

**Per-file position lists:** Positions are grouped by file ID rather than stored as a flat list. This enables efficient per-file distance verification without needing to track file boundaries in a separate structure.

**Delta-encoded positions:** Within each file's position list, offsets are stored as deltas from the previous offset. Since trigrams are extracted in order, positions are naturally sorted, making delta encoding effective.

**Position truncation for large files:** Files with an excessive number of occurrences of a single trigram (e.g., >65535 positions) should have their position list truncated. The search pipeline already falls back to full content verification, so truncated files would simply skip the distance check and proceed to verification as they do today.

**TrigramEntry remains 16 bytes:** The trigram table entry is unchanged. `posting_offset` and `posting_len` already point into the posting data; the new position data is interleaved within the posting list format itself.

### Posting list encoding detail

Current v2:

```
[count: varint] [delta_id_0: varint] [delta_id_1: varint] ...
```

Proposed v3:

```
[count: varint]
[delta_id_0: varint] [pos_count_0: varint] [pos_delta_0_0: varint] [pos_delta_0_1: varint] ...
[delta_id_1: varint] [pos_count_1: varint] [pos_delta_1_0: varint] [pos_delta_1_1: varint] ...
...
```

Example for trigram "hel" appearing in file 3 at offsets [10, 200, 5000] and file 7 at offsets [42]:

```
count=2
  file_id_delta=3, pos_count=3, pos_deltas=[10, 190, 4800]
  file_id_delta=4, pos_count=1, pos_deltas=[42]
```

## Backward Compatibility

### Migration strategy: v2 -> v3

**Option A: Automatic rebuild (recommended)**

When `IndexReader::open()` encounters a v2 index, it returns an error or a special status indicating "needs rebuild". The caller (`xg init` or background updater) triggers a full rebuild with position extraction. This is the simplest approach and consistent with the existing behavior when an index is missing.

- Pro: No complex migration code. v3 builder is the single source of truth.
- Pro: Rebuilds are already fast (6s for Linux kernel).
- Con: First search after upgrade triggers a rebuild.

**Option B: Dual-version reader**

`IndexReader` detects the version and supports both v2 (existence-only) and v3 (positional) reading. v2 indices fall back to the current full-intersection behavior. v3 indices use distance verification.

- Pro: No rebuild latency on upgrade.
- Con: Two code paths to maintain. Complexity for a transitional feature.

**Recommendation:** Option A. The rebuild cost is small relative to the benefit, and maintaining a single code path is worth the one-time rebuild cost.

### Version detection

The `version` field in the header already enables clean version detection. `IndexReader::open()` should:

1. Read the version field
2. If version < 3, return `XgrepError::IndexError("index version 2 detected, please rebuild with `xg init`")`
3. If version == 3, proceed with positional reading
4. If version > 3, return an error (future-proofing)

## Memory/Disk Overhead Analysis

### Current v2 index size

For the Linux kernel (~2.1GB source):
- Index size: 175MB (8% of source)
- Breakdown (estimated from format):
  - Header: 24 bytes (negligible)
  - Trigram table: ~16 bytes x ~500K unique trigrams = ~8MB
  - Posting lists: ~140MB (dominant component)
  - File table: ~28 bytes x ~78K files = ~2.2MB
  - String pool: ~25MB

### Position data overhead estimate

The posting lists grow because each `(file_id)` entry gains `(position_count, positions...)`.

**Model assumptions:**
- Average file size: 27KB (2.1GB / 78K files)
- Average unique trigrams per file: ~2,000 (typical for source code)
- Average occurrences per trigram per file: ~5-10
- Position values: 0-27000 range, varint-encoded (2-3 bytes each)
- Delta-encoded positions: average delta ~500-5000, varint 2-3 bytes

**Per-trigram overhead per file:**
- `position_count`: 1 varint (1 byte)
- `positions`: ~5-10 varints x 2 bytes avg = 10-20 bytes
- Total per file entry: +11-21 bytes (vs current 1-3 bytes for file_id_delta only)

**Aggregate estimate:**

| Component | v2 size | v3 size (est.) | Growth |
|-----------|---------|----------------|--------|
| Posting lists | 140MB | 500-700MB | 3.5-5x |
| Other sections | 35MB | 35MB | 1x |
| **Total index** | **175MB** | **535-735MB** | **3-4x** |
| **% of source** | **8%** | **25-35%** | - |

This brings xgrep's index size to roughly 25-35% of source, still well below zoekt's 155% but a significant increase from the current 8%.

### Mitigation strategies

1. **Position truncation:** Cap positions per (trigram, file) pair at 256. Files with more occurrences skip distance verification (fall back to content check). This bounds the worst case without affecting typical files.

2. **Bit-packing positions:** Instead of exact byte offsets, store positions as `offset / 64` (block-level granularity). Reduces position values by 6 bits, making varints shorter. Distance checks become approximate but still effective for eliminating false positives.

3. **Separate position section:** Store position data in a separate section after the file-id-only posting lists. v3 reader uses both; in memory-constrained environments, positions can be skipped. This also enables lazy loading of position data.

4. **Top-K trigrams only:** Only store positions for the most common trigrams (e.g., top 50% by document frequency). Rare trigrams already have small posting lists, so distance verification provides less benefit for them.

**Recommended initial approach:** Strategy 1 (truncation at 256) + Strategy 2 (block-level positions at 64-byte granularity). Estimated index size: **15-20% of source** -- a reasonable middle ground.

## Expected False Positive Improvement

### Theoretical model

For a query of length L characters, the number of trigrams is L-2.

**v2 (existence-only):** Candidate files must contain all L-2 trigrams. The probability of a random file being a false positive depends on how common each trigram is.

**v3 (positional):** Candidate files must contain the first and last trigrams at exactly the right distance apart (L-3 bytes). Even if both trigrams appear in a file, the probability of the correct distance is approximately:

```
P(distance match) = 1 - (1 - 1/avg_file_size)^(count_first * count_last)
```

For a 27KB file with ~5 occurrences of each trigram:
- P(distance match by chance) ~ 5 * 5 / 27000 ~ 0.1%

This means positional trigrams eliminate approximately **99.9%** of false positives that pass the existence check.

### Practical impact

| Scenario | v2 candidates | v3 candidates (est.) | Reduction |
|----------|---------------|----------------------|-----------|
| Short literal (5 chars) | 100-1000 files | 1-10 files | 90-99% |
| Long literal (20 chars) | 10-100 files | 1-2 files | 90-98% |
| Common pattern ("return") | 50K+ files | 50K+ files | minimal (query < 6 chars) |
| Rare long string | 1-5 files | 1 file | 0-80% |

The largest benefit comes from **medium-frequency, medium-length queries** where v2 produces hundreds of candidates that all need content verification. For very short queries (< 6 chars), only 2 trigrams exist anyway, so positional verification cannot help. For very rare strings, the candidate set is already small.

### Search latency impact

The bottleneck in xgrep's search is reading candidate files from disk for content verification. Reducing the candidate set from 500 to 5 files means:

- **Disk I/O:** 100x reduction in bytes read for verification
- **Expected latency improvement:** 2-10x for medium-length literal queries
- **Negligible impact:** regex queries (which already extract limited trigrams) and very short queries

## Implementation Phases

### Phase 1: Position extraction during build (index builder changes)

Modify `trigram::extract_trigrams()` to optionally return `Vec<([u8; 3], u32)>` (trigram + offset) instead of `Vec<[u8; 3]>`. Update `IndexBuilder` to collect and encode position data into the v3 posting list format.

- Changes: `trigram.rs`, `index/builder.rs`, `index/format.rs`
- Risk: Low. Additive change to the builder.
- Estimated effort: 1-2 days

### Phase 2: Position-aware reader

Update `IndexReader` to decode v3 posting lists, extracting both file IDs and position lists. Add `lookup_trigram_with_positions()` method returning `Vec<(u32, Vec<u32>)>` (file_id, positions).

- Changes: `index/reader.rs`, `index/format.rs`
- Risk: Low. New method alongside existing `lookup_trigram()`.
- Estimated effort: 1 day

### Phase 3: Distance verification in search pipeline

Modify `resolve_literal_candidates()` to use distance verification when the query is >= 6 characters. For the first and last trigrams, verify that at least one pair of positions has the correct distance.

- Changes: `candidates.rs`, `search.rs`
- Risk: Medium. Core search path change. Requires thorough testing.
- Estimated effort: 2-3 days

### Phase 4: Benchmarking and tuning

Run benchmarks comparing v2 and v3 on small/medium/large repositories. Tune position truncation thresholds and block granularity. Update README with new numbers.

- Changes: `bench/`, `README.md`, `docs/benchmarks.md`
- Risk: Low.
- Estimated effort: 1 day

### Phase 5: MCP and API integration

Ensure MCP tools and `lib.rs` public API work correctly with v3 indices. No API changes expected (the improvement is transparent to callers).

- Changes: Minimal. Possibly `mcp_tools.rs` error messages.
- Risk: Low.
- Estimated effort: Half day

### Total estimated effort: 5-7 days

## Open Questions

1. **Block granularity:** Should positions be stored at byte granularity or block-level (e.g., 64-byte blocks)? Byte-level is more precise but uses more space. Block-level is smaller but introduces approximate matching.

2. **Minimum query length for distance verification:** 6 characters (4 trigrams, allowing first/last distance check)? Or lower with a different strategy?

3. **Position cap per file:** 256 positions? 1024? The cap trades index size for candidate precision.

4. **Separate position section vs interleaved:** Interleaved is simpler to implement but makes it impossible to skip position data during existence-only searches. A separate section adds complexity but enables graceful degradation.

## References

- [Google Code Search (Russ Cox)](https://swtch.com/~rsc/regexp/regexp4.html) -- original trigram index design
- [zoekt (Sourcegraph)](https://github.com/sourcegraph/zoekt) -- positional trigrams implementation
- [xgrep index format v2](../rust/src/index/format.rs) -- current implementation
