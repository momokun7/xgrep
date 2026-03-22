#!/usr/bin/env bash
# bench/fair-bench.sh — xgrep vs ripgrep vs zoekt 公正ベンチマーク
#
# レビュー指摘を全て反映:
#   0. 環境情報の記録
#   1. 正確性の検証 (match count diff)
#   2. インデックス構築コスト
#   3. コールドスタート (初回利用コスト)
#   4. ウォーム検索レイテンシ (hyperfine --shell=none)
#   5. メモリ使用量 (/usr/bin/time -l)
#   6. 損益分岐分析 (何回検索でインデックスの元が取れるか)
#   7. サマリーテーブル
set -euo pipefail

###############################################################################
# Config
###############################################################################
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BENCH_DIR="$SCRIPT_DIR"
RESULTS_DIR="$BENCH_DIR/results"
mkdir -p "$RESULTS_DIR"

XGREP="$SCRIPT_DIR/../rust/target/release/xgrep"
ZOEKT_INDEX="$HOME/.asdf/installs/golang/1.24.4/bin/zoekt-index"
ZOEKT="$HOME/.asdf/installs/golang/1.24.4/bin/zoekt"
RG="rg"

# Datasets
DS_LARGE="$BENCH_DIR/linux-src"
DS_MEDIUM="$BENCH_DIR/ripgrep-src"
DS_SMALL="$SCRIPT_DIR/../rust"

# Zoekt index dirs (isolated per dataset)
ZOEKT_IDX_DIR_LARGE="$BENCH_DIR/.zoekt-idx-large"
ZOEKT_IDX_DIR_MEDIUM="$BENCH_DIR/.zoekt-idx-medium"

# xgrep cache
XGREP_CACHE="$HOME/Library/Caches/xgrep"

# Queries
QUERIES_LARGE=("struct file_operations" "printk" "EXPORT_SYMBOL")
QUERIES_MEDIUM=("fn main" "Options" "pub struct")
QUERIES_SMALL=("fn search" "trigram")

# Continue on correctness failure?
FORCE_CONTINUE="${FORCE_CONTINUE:-0}"

# Timestamp
TIMESTAMP="$(date +%Y%m%d_%H%M%S)"
REPORT="$RESULTS_DIR/report_${TIMESTAMP}.md"

###############################################################################
# Helpers
###############################################################################
log() { echo ">>> $*"; }
hr()  { echo "---"; }

tee_report() {
  tee -a "$REPORT"
}

append() {
  echo "$*" >> "$REPORT"
}

append_block() {
  cat >> "$REPORT"
}

# Get xgrep match count (xgrep outputs matching lines to stdout)
xgrep_count() {
  local dir="$1" query="$2"
  (cd "$dir" && "$XGREP" "$query" 2>/dev/null | wc -l | tr -d ' ')
}

# Get ripgrep match count
rg_count() {
  local dir="$1" query="$2"
  "$RG" --no-filename "$query" "$dir" 2>/dev/null | wc -l | tr -d ' '
}

# Get zoekt match count
zoekt_count() {
  local idx_dir="$1" query="$2"
  "$ZOEKT" -index_dir "$idx_dir" "$query" 2>/dev/null | wc -l | tr -d ' '
}

# Format bytes to human readable
human_bytes() {
  local bytes="$1"
  if (( bytes >= 1073741824 )); then
    echo "$(echo "scale=2; $bytes / 1073741824" | bc)GB"
  elif (( bytes >= 1048576 )); then
    echo "$(echo "scale=2; $bytes / 1048576" | bc)MB"
  elif (( bytes >= 1024 )); then
    echo "$(echo "scale=2; $bytes / 1024" | bc)KB"
  else
    echo "${bytes}B"
  fi
}

# Get directory size in bytes
dir_size_bytes() {
  du -sk "$1" 2>/dev/null | awk '{print $1 * 1024}'
}

# Count files (excluding hidden dirs and build artifacts)
file_count() {
  find "$1" -type f -not -path '*/\.*' -not -path '*/target/*' -not -path '*/node_modules/*' 2>/dev/null | wc -l | tr -d ' '
}

# Extract max RSS from /usr/bin/time -l output (macOS format)
extract_max_rss() {
  grep "maximum resident set size" | awk '{print $1}'
}

# Extract wall clock from /usr/bin/time -l output (macOS: real line)
extract_wall_time() {
  grep "real" | head -1 | awk '{print $1}'
}

###############################################################################
# Prep: Clone medium dataset if needed
###############################################################################
if [ ! -d "$DS_MEDIUM" ]; then
  log "Medium dataset not found. Cloning ripgrep repo..."
  git clone --depth 1 https://github.com/BurntSushi/ripgrep "$DS_MEDIUM"
fi

###############################################################################
# Start report
###############################################################################
cat > "$REPORT" <<'HEADER'
# xgrep Benchmark Report

HEADER

###############################################################################
# Section 0: Environment
###############################################################################
log "Section 0: Environment"

{
  echo "## Section 0: Environment"
  echo ""
  echo "| Item | Value |"
  echo "|------|-------|"
  echo "| Date | $(date '+%Y-%m-%d %H:%M:%S %Z') |"
  echo "| OS | $(sw_vers -productName 2>/dev/null || uname -s) $(sw_vers -productVersion 2>/dev/null || uname -r) |"
  echo "| CPU | $(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -m) |"
  echo "| RAM | $(echo "scale=0; $(sysctl -n hw.memsize 2>/dev/null || echo 0) / 1073741824" | bc)GB |"
  echo "| Disk | $(diskutil info / 2>/dev/null | grep 'Solid State' | awk '{print $NF}' || echo 'unknown') |"
  echo ""
  echo "### Tool Versions"
  echo ""
  echo '```'
  echo "xgrep: $("$XGREP" --help 2>&1 | head -1 || echo 'unknown')"
  echo "ripgrep: $("$RG" --version | head -1)"
  echo "zoekt: $("$ZOEKT" 2>&1 | head -1 || echo 'installed')"
  echo "hyperfine: $(hyperfine --version)"
  echo '```'
  echo ""
  echo "### Datasets"
  echo ""
  echo "| Dataset | Path | Files | Size |"
  echo "|---------|------|-------|------|"
  echo "| Large (linux) | $DS_LARGE | $(file_count "$DS_LARGE") | $(du -sh "$DS_LARGE" | awk '{print $1}') |"
  echo "| Medium (ripgrep) | $DS_MEDIUM | $(file_count "$DS_MEDIUM") | $(du -sh "$DS_MEDIUM" | awk '{print $1}') |"
  echo "| Small (xgrep/rust) | $DS_SMALL | $(file_count "$DS_SMALL") | $(du -sh "$DS_SMALL" | awk '{print $1}') |"
  echo ""
} >> "$REPORT"

###############################################################################
# Section 1: Correctness Verification
###############################################################################
log "Section 1: Correctness verification"

{
  echo "## Section 1: Correctness Verification"
  echo ""
} >> "$REPORT"

# Build xgrep index for large dataset first
log "Building xgrep index for large dataset..."
rm -rf "$XGREP_CACHE"
(cd "$DS_LARGE" && "$XGREP" init 2>&1)

CORRECTNESS_FAIL=0

for query in "${QUERIES_LARGE[@]}"; do
  log "  Checking: '$query'"

  # Get match lines (not just counts) for comparison
  xgrep_out="$RESULTS_DIR/correctness_xgrep_${query// /_}.txt"
  rg_out="$RESULTS_DIR/correctness_rg_${query// /_}.txt"

  # パス形式を正規化して比較（xgrep: path:line:content, rg: ./path:line:content）
  (cd "$DS_LARGE" && "$XGREP" "$query" 2>/dev/null) | sort > "$xgrep_out"
  "$RG" --no-heading "$query" "$DS_LARGE" 2>/dev/null | sed "s|^$DS_LARGE/||" | sort > "$rg_out"

  xc=$(wc -l < "$xgrep_out" | tr -d ' ')
  rc=$(wc -l < "$rg_out" | tr -d ' ')

  if [ "$xc" != "$rc" ]; then
    status="MISMATCH"
    CORRECTNESS_FAIL=1
    diff_file="$RESULTS_DIR/diff_${query// /_}.txt"
    diff "$xgrep_out" "$rg_out" > "$diff_file" 2>&1 || true
    {
      echo "- **$query**: xgrep=$xc, ripgrep=$rc -- **MISMATCH** (diff: $diff_file)"
    } >> "$REPORT"
  else
    {
      echo "- **$query**: xgrep=$xc, ripgrep=$rc -- OK"
    } >> "$REPORT"
  fi
done

echo "" >> "$REPORT"

if [ "$CORRECTNESS_FAIL" -eq 1 ]; then
  echo "WARNING: Correctness mismatch detected. See $RESULTS_DIR for diffs." >> "$REPORT"
  echo "" >> "$REPORT"
  if [ "$FORCE_CONTINUE" -ne 1 ]; then
    echo "FATAL: Correctness issues found. Set FORCE_CONTINUE=1 to proceed anyway."
    echo "See: $REPORT"
    exit 1
  fi
  log "WARNING: Correctness issues found, continuing due to FORCE_CONTINUE=1"
fi

###############################################################################
# Section 2: Index Build Cost
###############################################################################
log "Section 2: Index build cost"

{
  echo "## Section 2: Index Build Cost"
  echo ""
} >> "$REPORT"

# --- xgrep index build ---
log "  xgrep index build (large)..."
rm -rf "$XGREP_CACHE"
xgrep_build_out="$RESULTS_DIR/xgrep_build_time.txt"
/usr/bin/time -l bash -c "cd '$DS_LARGE' && '$XGREP' init" > "$xgrep_build_out" 2>&1
xgrep_build_wall=$(grep "real" "$xgrep_build_out" | awk '{print $1}')
xgrep_build_rss=$(grep "maximum resident" "$xgrep_build_out" | awk '{print $1}')
xgrep_idx_size=$(du -sh "$XGREP_CACHE" 2>/dev/null | awk '{print $1}')
xgrep_idx_bytes=$(dir_size_bytes "$XGREP_CACHE")
source_bytes=$(dir_size_bytes "$DS_LARGE")
xgrep_overhead=$(echo "scale=2; $xgrep_idx_bytes * 100 / $source_bytes" | bc)

# --- zoekt index build ---
log "  zoekt index build (large)..."
rm -rf "$ZOEKT_IDX_DIR_LARGE"
mkdir -p "$ZOEKT_IDX_DIR_LARGE"
zoekt_build_out="$RESULTS_DIR/zoekt_build_time.txt"
/usr/bin/time -l "$ZOEKT_INDEX" -index "$ZOEKT_IDX_DIR_LARGE" "$DS_LARGE" > "$zoekt_build_out" 2>&1
zoekt_build_wall=$(grep "real" "$zoekt_build_out" | awk '{print $1}')
zoekt_build_rss=$(grep "maximum resident" "$zoekt_build_out" | awk '{print $1}')
zoekt_idx_size=$(du -sh "$ZOEKT_IDX_DIR_LARGE" 2>/dev/null | awk '{print $1}')
zoekt_idx_bytes=$(dir_size_bytes "$ZOEKT_IDX_DIR_LARGE")
zoekt_overhead=$(echo "scale=2; $zoekt_idx_bytes * 100 / $source_bytes" | bc)

{
  echo "| Tool | Wall Time | Peak RSS | Index Size | Overhead (idx/src) |"
  echo "|------|-----------|----------|------------|--------------------|"
  echo "| xgrep | ${xgrep_build_wall}s | $(human_bytes "$xgrep_build_rss") | $xgrep_idx_size | ${xgrep_overhead}% |"
  echo "| zoekt | ${zoekt_build_wall}s | $(human_bytes "$zoekt_build_rss") | $zoekt_idx_size | ${zoekt_overhead}% |"
  echo ""
  echo "Source size: $(du -sh "$DS_LARGE" | awk '{print $1}')"
  echo ""
} >> "$REPORT"

###############################################################################
# Section 3: Cold Start (First-time Use Cost)
###############################################################################
log "Section 3: Cold start"

{
  echo "## Section 3: Cold Start (First-Use Cost)"
  echo ""
  echo "Measures total wall time for a first-time user: index build + first search."
  echo ""
} >> "$REPORT"

COLD_QUERY="struct file_operations"

# xgrep cold start
log "  xgrep cold start..."
rm -rf "$XGREP_CACHE"
xgrep_cold_out="$RESULTS_DIR/xgrep_cold.txt"
/usr/bin/time -l bash -c "
  cd '$DS_LARGE'
  '$XGREP' init >/dev/null 2>&1
  '$XGREP' '$COLD_QUERY' >/dev/null 2>&1
" > "$xgrep_cold_out" 2>&1
xgrep_cold_wall=$(grep "real" "$xgrep_cold_out" | awk '{print $1}')

# ripgrep cold start (no setup needed)
log "  ripgrep cold start..."
rg_cold_out="$RESULTS_DIR/rg_cold.txt"
# Drop filesystem cache as much as possible (purge won't work without sudo, but sync helps)
sync
/usr/bin/time -l "$RG" "$COLD_QUERY" "$DS_LARGE" > /dev/null 2> "$rg_cold_out"
rg_cold_wall=$(grep "real" "$rg_cold_out" | awk '{print $1}')

# zoekt cold start
log "  zoekt cold start..."
rm -rf "$ZOEKT_IDX_DIR_LARGE"
mkdir -p "$ZOEKT_IDX_DIR_LARGE"
zoekt_cold_out="$RESULTS_DIR/zoekt_cold.txt"
/usr/bin/time -l bash -c "
  '$ZOEKT_INDEX' -index '$ZOEKT_IDX_DIR_LARGE' '$DS_LARGE' >/dev/null 2>&1
  '$ZOEKT' -index_dir '$ZOEKT_IDX_DIR_LARGE' '$COLD_QUERY' >/dev/null 2>&1
" > "$zoekt_cold_out" 2>&1
zoekt_cold_wall=$(grep "real" "$zoekt_cold_out" | awk '{print $1}')

{
  echo "Query: \`$COLD_QUERY\`"
  echo ""
  echo "| Tool | Total Wall Time | Notes |"
  echo "|------|-----------------|-------|"
  echo "| xgrep | ${xgrep_cold_wall}s | index build + search |"
  echo "| ripgrep | ${rg_cold_wall}s | no setup needed |"
  echo "| zoekt | ${zoekt_cold_wall}s | index build + search |"
  echo ""
} >> "$REPORT"

###############################################################################
# Section 4: Warm Search Latency (hyperfine)
###############################################################################
log "Section 4: Warm search latency"

{
  echo "## Section 4: Warm Search Latency"
  echo ""
  echo "Large: hyperfine --warmup 5 --runs 20 (default shell). Medium/Small: hyperfine --warmup 10 --runs 100 -N (no shell, high precision)"
  echo ""
} >> "$REPORT"

# --- Large dataset ---
log "  Large dataset benchmarks..."

# Ensure xgrep index exists for large
rm -rf "$XGREP_CACHE"
(cd "$DS_LARGE" && "$XGREP" init 2>&1 >/dev/null)

# Ensure zoekt index exists for large
if [ ! -d "$ZOEKT_IDX_DIR_LARGE" ] || [ -z "$(ls -A "$ZOEKT_IDX_DIR_LARGE" 2>/dev/null)" ]; then
  rm -rf "$ZOEKT_IDX_DIR_LARGE"
  mkdir -p "$ZOEKT_IDX_DIR_LARGE"
  "$ZOEKT_INDEX" -index "$ZOEKT_IDX_DIR_LARGE" "$DS_LARGE" >/dev/null 2>&1
fi

{
  echo "### Large Dataset (Linux kernel)"
  echo ""
} >> "$REPORT"

pushd "$DS_LARGE" > /dev/null

for query in "${QUERIES_LARGE[@]}"; do
  log "    query: '$query'"
  json_file="$RESULTS_DIR/hyperfine_large_${query// /_}.json"

  hyperfine \
    --warmup 5 \
    --runs 20 \
    --export-json "$json_file" \
    --command-name "xgrep" \
    "'$XGREP' '$query'" \
    --ignore-failure \
    --command-name "ripgrep" \
    "'$RG' '$query' ." \
    --command-name "zoekt" \
    "'$ZOEKT' -index_dir '$ZOEKT_IDX_DIR_LARGE' '$query'" \
    2>&1 | tail -10

  # Extract results from JSON
  {
    echo "#### Query: \`$query\`"
    echo ""
    echo "| Tool | Mean | Min | Max | Stddev |"
    echo "|------|------|-----|-----|--------|"
    # Parse JSON with python (more reliable than jq which may not be installed)
    python3 -c "
import json, sys
with open('$json_file') as f:
    data = json.load(f)
for r in data['results']:
    name = r['command']
    mean = r['mean']
    min_t = r['min']
    max_t = r['max']
    stddev = r['stddev']
    print(f'| {name} | {mean:.4f}s | {min_t:.4f}s | {max_t:.4f}s | {stddev:.4f}s |')
"
    echo ""
  } >> "$REPORT"
done

popd > /dev/null

# --- Medium dataset ---
log "  Medium dataset benchmarks..."

# Build xgrep index for medium
rm -rf "$XGREP_CACHE"
(cd "$DS_MEDIUM" && "$XGREP" init 2>&1 >/dev/null)

{
  echo "### Medium Dataset (ripgrep source)"
  echo ""
} >> "$REPORT"

pushd "$DS_MEDIUM" > /dev/null

for query in "${QUERIES_MEDIUM[@]}"; do
  log "    query: '$query'"
  json_file="$RESULTS_DIR/hyperfine_medium_${query// /_}.json"

  hyperfine \
    --warmup 10 \
    --runs 100 \
    -N \
    --export-json "$json_file" \
    --command-name "xgrep" \
    "'$XGREP' '$query'" \
    --ignore-failure \
    --command-name "ripgrep" \
    "'$RG' '$query' ." \
    2>&1 | tail -6

  {
    echo "#### Query: \`$query\`"
    echo ""
    echo "| Tool | Mean | Min | Max | Stddev |"
    echo "|------|------|-----|-----|--------|"
    python3 -c "
import json
with open('$json_file') as f:
    data = json.load(f)
for r in data['results']:
    name = r['command']
    mean = r['mean']
    min_t = r['min']
    max_t = r['max']
    stddev = r['stddev']
    print(f'| {name} | {mean:.4f}s | {min_t:.4f}s | {max_t:.4f}s | {stddev:.4f}s |')
"
    echo ""
  } >> "$REPORT"
done

popd > /dev/null

# --- Small dataset ---
log "  Small dataset benchmarks..."

# Build xgrep index for small
rm -rf "$XGREP_CACHE"
(cd "$DS_SMALL" && "$XGREP" init 2>&1 >/dev/null)

{
  echo "### Small Dataset (xgrep/rust source)"
  echo ""
} >> "$REPORT"

pushd "$DS_SMALL" > /dev/null

for query in "${QUERIES_SMALL[@]}"; do
  log "    query: '$query'"
  json_file="$RESULTS_DIR/hyperfine_small_${query// /_}.json"

  hyperfine \
    --warmup 10 \
    --runs 100 \
    -N \
    --export-json "$json_file" \
    --command-name "xgrep" \
    "'$XGREP' '$query'" \
    --ignore-failure \
    --command-name "ripgrep" \
    "'$RG' '$query' ." \
    2>&1 | tail -6

  {
    echo "#### Query: \`$query\`"
    echo ""
    echo "| Tool | Mean | Min | Max | Stddev |"
    echo "|------|------|-----|-----|--------|"
    python3 -c "
import json
with open('$json_file') as f:
    data = json.load(f)
for r in data['results']:
    name = r['command']
    mean = r['mean']
    min_t = r['min']
    max_t = r['max']
    stddev = r['stddev']
    print(f'| {name} | {mean:.4f}s | {min_t:.4f}s | {max_t:.4f}s | {stddev:.4f}s |')
"
    echo ""
  } >> "$REPORT"
done

popd > /dev/null

###############################################################################
# Section 5: Memory Usage
###############################################################################
log "Section 5: Memory usage"

{
  echo "## Section 5: Memory Usage"
  echo ""
} >> "$REPORT"

MEM_QUERY="struct file_operations"

# Rebuild xgrep index for large (for search memory measurement)
rm -rf "$XGREP_CACHE"
(cd "$DS_LARGE" && "$XGREP" init 2>&1 >/dev/null)

# xgrep search memory (cd into target dir, then run directly)
xgrep_search_mem_out="$RESULTS_DIR/xgrep_search_mem.txt"
(cd "$DS_LARGE" && /usr/bin/time -l "$XGREP" "$MEM_QUERY" > /dev/null) 2> "$xgrep_search_mem_out"
xgrep_search_rss=$(grep "maximum resident" "$xgrep_search_mem_out" | awk '{print $1}')

# ripgrep search memory
rg_search_mem_out="$RESULTS_DIR/rg_search_mem.txt"
/usr/bin/time -l "$RG" "$MEM_QUERY" "$DS_LARGE" > /dev/null 2> "$rg_search_mem_out"
rg_search_rss=$(grep "maximum resident" "$rg_search_mem_out" | awk '{print $1}')

# zoekt search memory
zoekt_search_mem_out="$RESULTS_DIR/zoekt_search_mem.txt"
/usr/bin/time -l "$ZOEKT" -index_dir "$ZOEKT_IDX_DIR_LARGE" "$MEM_QUERY" > /dev/null 2> "$zoekt_search_mem_out"
zoekt_search_rss=$(grep "maximum resident" "$zoekt_search_mem_out" | awk '{print $1}')

{
  echo "### Index Build (Large dataset)"
  echo ""
  echo "| Tool | Peak RSS |"
  echo "|------|----------|"
  echo "| xgrep | $(human_bytes "$xgrep_build_rss") |"
  echo "| zoekt | $(human_bytes "$zoekt_build_rss") |"
  echo ""
  echo "### Search (Large dataset, query: \`$MEM_QUERY\`)"
  echo ""
  echo "| Tool | Peak RSS |"
  echo "|------|----------|"
  echo "| xgrep | $(human_bytes "$xgrep_search_rss") |"
  echo "| ripgrep | $(human_bytes "$rg_search_rss") |"
  echo "| zoekt | $(human_bytes "$zoekt_search_rss") |"
  echo ""
} >> "$REPORT"

###############################################################################
# Section 6: Breakeven Analysis
###############################################################################
log "Section 6: Breakeven analysis"

{
  echo "## Section 6: Breakeven Analysis"
  echo ""
  echo "At how many searches does xgrep's index cost pay off vs ripgrep?"
  echo ""
  echo "Formula: index_build_time + N * xgrep_search_time < N * rg_search_time"
  echo "Solve: N > index_build_time / (rg_search_time - xgrep_search_time)"
  echo ""
} >> "$REPORT"

# Use the hyperfine JSON from large dataset, first query
breakeven_json="$RESULTS_DIR/hyperfine_large_struct_file_operations.json"

if [ -f "$breakeven_json" ]; then
  python3 -c "
import json

with open('$breakeven_json') as f:
    data = json.load(f)

times = {}
for r in data['results']:
    times[r['command']] = r['mean']

xgrep_search = times.get('xgrep', 0)
rg_search = times.get('ripgrep', 0)

# Get index build time from /usr/bin/time output
import re
with open('$RESULTS_DIR/xgrep_build_time.txt') as f:
    content = f.read()
m = re.search(r'(\d+\.\d+)\s+real', content)
if m:
    index_time = float(m.group(1))
else:
    # Try format: real X.XXs
    m = re.search(r'real\s+(\d+)m([\d.]+)s', content)
    if m:
        index_time = int(m.group(1)) * 60 + float(m.group(2))
    else:
        index_time = 0

diff = rg_search - xgrep_search
if diff > 0:
    n = index_time / diff
    print(f'| Metric | Value |')
    print(f'|--------|-------|')
    print(f'| xgrep index build | {index_time:.2f}s |')
    print(f'| xgrep search (mean) | {xgrep_search:.4f}s |')
    print(f'| ripgrep search (mean) | {rg_search:.4f}s |')
    print(f'| Difference per search | {diff:.4f}s |')
    print(f'| **Breakeven at** | **{int(n)+1} searches** |')
elif diff == 0:
    print('Search times are identical. Index never pays off for speed.')
else:
    print(f'ripgrep is faster per search ({rg_search:.4f}s vs {xgrep_search:.4f}s).')
    print('Index build cost never recovers.')
" >> "$REPORT"
  echo "" >> "$REPORT"
else
  echo "Breakeven JSON not found, skipping." >> "$REPORT"
fi

###############################################################################
# Section 7: Summary
###############################################################################
log "Section 7: Summary"

{
  echo "## Section 7: Summary"
  echo ""
  echo "### Search Latency (Large dataset, mean)"
  echo ""
} >> "$REPORT"

# Build summary from all large dataset hyperfine results
python3 -c "
import json, glob, os

results_dir = '$RESULTS_DIR'
files = sorted(glob.glob(os.path.join(results_dir, 'hyperfine_large_*.json')))

print('| Query | xgrep | ripgrep | zoekt | xgrep vs rg |')
print('|-------|-------|---------|-------|-------------|')

for f in files:
    query = os.path.basename(f).replace('hyperfine_large_', '').replace('.json', '').replace('_', ' ')
    with open(f) as fh:
        data = json.load(fh)
    times = {}
    for r in data['results']:
        times[r['command']] = r['mean']

    xg = times.get('xgrep', 0)
    rg_t = times.get('ripgrep', 0)
    zo = times.get('zoekt', 0)

    if rg_t > 0 and xg > 0:
        speedup = rg_t / xg
        ratio = f'{speedup:.1f}x faster'
    else:
        ratio = 'N/A'

    zo_str = f'{zo:.4f}s' if zo > 0 else 'N/A'
    print(f'| {query} | {xg:.4f}s | {rg_t:.4f}s | {zo_str} | {ratio} |')
" >> "$REPORT"

{
  echo ""
  echo "### Key Findings"
  echo ""
  echo "- Index build times and sizes are in Section 2"
  echo "- Cold start comparison (first-time user experience) is in Section 3"
  echo "- Memory usage is in Section 5"
  echo "- Breakeven analysis is in Section 6"
  echo ""
  echo "### Caveats"
  echo ""
  echo "- Section 4 benchmarks use hyperfine with the default shell; each invocation has equal shell startup overhead (~3ms)"
  echo "- xgrep and ripgrep run with cwd set to the target directory via pushd/popd; no bash -c wrapper is used"
  echo "- zoekt is designed as a server (zoekt-webserver); CLI mode adds process startup overhead. Server mode would be significantly faster"
  echo "- xgrep requires \`cd\` into the target directory; index is tied to cwd"
  echo "- ripgrep requires no setup; index-based tools have amortized cost (see Section 6)"
  echo "- File system cache state affects cold start measurements; \`sync\` is called but page cache is not purged (requires sudo)"
  echo "- All measurements on a single machine; results may vary by hardware"
  echo "- zoekt only benchmarked on large dataset (it targets large-scale code search)"
  echo ""
} >> "$REPORT"

###############################################################################
# Cleanup & Done
###############################################################################
log "Done! Report saved to: $REPORT"
log "JSON results in: $RESULTS_DIR/"
echo ""
cat "$REPORT"
