#!/bin/bash
set -euo pipefail

# xgrep Benchmark Suite
# Requirements: hyperfine, ripgrep, xgrep (xg)
# Usage: ./bench/run.sh [small|medium|large|all]

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
RESULTS_DIR="$SCRIPT_DIR/results"
mkdir -p "$RESULTS_DIR"

# Check dependencies
for cmd in hyperfine rg xg; do
    if ! command -v "$cmd" &> /dev/null; then
        echo "ERROR: $cmd is required. Install it first."
        exit 1
    fi
done

MODE="${1:-medium}"

echo "=== xgrep Benchmark Suite ==="
echo "Mode: $MODE"
echo "Date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "xg version: $(xg --version 2>/dev/null || echo 'unknown')"
echo "rg version: $(rg --version | head -1)"
echo "hyperfine version: $(hyperfine --version)"
echo ""

# --- Small benchmark: xgrep's own source ---
if [[ "$MODE" == "small" || "$MODE" == "all" ]]; then
    echo "=== Small: xgrep source ($(find "$SCRIPT_DIR/../rust/src" -name '*.rs' | wc -l | tr -d ' ') files) ==="
    BENCH_DIR="$SCRIPT_DIR/../rust"

    cd "$BENCH_DIR"
    xg init 2>/dev/null

    for pattern in "fn main" "SearchResult" "Matcher"; do
        echo "--- Pattern: $pattern ---"
        hyperfine --warmup 10 --runs 20 \
            "xg '$pattern'" \
            "rg '$pattern'" \
            --export-json "$RESULTS_DIR/small_$(echo "$pattern" | tr ' ' '_').json"
    done
    echo ""
fi

# --- Medium benchmark: ripgrep source ---
if [[ "$MODE" == "medium" || "$MODE" == "all" ]]; then
    RG_SRC="$SCRIPT_DIR/ripgrep-src"
    if [[ ! -d "$RG_SRC" ]]; then
        echo "Downloading ripgrep source..."
        git clone --depth 1 https://github.com/BurntSushi/ripgrep.git "$RG_SRC"
    fi

    FILE_COUNT=$(find "$RG_SRC" -type f | wc -l | tr -d ' ')
    echo "=== Medium: ripgrep source ($FILE_COUNT files) ==="

    cd "$RG_SRC"
    xg init 2>/dev/null

    for pattern in "fn main" "Options" "pub struct"; do
        echo "--- Pattern: $pattern ---"
        hyperfine --warmup 10 --runs 20 \
            "xg '$pattern'" \
            "rg '$pattern'" \
            --export-json "$RESULTS_DIR/medium_$(echo "$pattern" | tr ' ' '_').json"
    done
    echo ""
fi

# --- Large benchmark: Linux kernel (if available) ---
if [[ "$MODE" == "large" || "$MODE" == "all" ]]; then
    LINUX_SRC="$SCRIPT_DIR/linux-src"
    if [[ ! -d "$LINUX_SRC" ]]; then
        echo "Linux kernel source not found at $LINUX_SRC"
        echo "Download with: git clone --depth 1 https://github.com/torvalds/linux.git $LINUX_SRC"
        echo "Skipping large benchmark."
    else
        FILE_COUNT=$(find "$LINUX_SRC" -type f | wc -l | tr -d ' ')
        echo "=== Large: Linux kernel ($FILE_COUNT files) ==="

        cd "$LINUX_SRC"
        xg init 2>/dev/null

        for pattern in "struct file_operations" "printk" "EXPORT_SYMBOL"; do
            echo "--- Pattern: $pattern ---"
            hyperfine --warmup 10 --runs 30 \
                "xg '$pattern'" \
                "rg '$pattern'" \
                --export-json "$RESULTS_DIR/large_$(echo "$pattern" | tr ' ' '_').json" \
                --export-markdown "$RESULTS_DIR/large_$(echo "$pattern" | tr ' ' '_').md"
        done
        echo ""
    fi
fi

echo "=== Results saved to $RESULTS_DIR ==="
ls -la "$RESULTS_DIR"/*.json 2>/dev/null || echo "No JSON results generated"
