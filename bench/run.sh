#!/usr/bin/env bash
# bench/run.sh — xgrep (Rust) vs xgrep (Zig) vs ripgrep vs grep ベンチマーク
# スペックのベンチマーク4項目を全てカバー:
#   1. インデックス構築速度
#   2. 検索レイテンシ (cold/warm)
#   3. 検索スループット
#   4. メモリ使用量
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
XGREP_RUST="$SCRIPT_DIR/../rust/target/release/xgrep"
XGREP_ZIG="$SCRIPT_DIR/../zig/zig-out/bin/xgrep"
TARGET="$SCRIPT_DIR/linux-src"
QUERIES=("struct file_operations" "printk" "mutex_lock" "EXPORT_SYMBOL" "void __init")
THROUGHPUT_N=${1:-100}

echo "=== Build ==="
echo "Rust:"
cd "$SCRIPT_DIR/../rust" && cargo build --release
echo "Zig:"
cd "$SCRIPT_DIR/../zig" && zig build -Doptimize=ReleaseFast
cd "$TARGET"

echo ""
echo "=== 1. Index Build Time ==="
echo "--- Rust ---"
rm -rf ~/Library/Caches/xgrep
time "$XGREP_RUST" init 2>&1

echo "--- Zig ---"
rm -rf ~/Library/Caches/xgrep
time "$XGREP_ZIG" init 2>&1

echo ""
echo "=== 2. Search Latency (warm) ==="
# ウォームアップ
"$XGREP_RUST" "warmup" > /dev/null 2>&1 || true
"$XGREP_ZIG" "warmup" > /dev/null 2>&1 || true

for q in "${QUERIES[@]}"; do
    echo "--- query: '$q' ---"
    echo "xgrep (Rust):"
    time "$XGREP_RUST" "$q" | wc -l
    echo "xgrep (Zig):"
    time "$XGREP_ZIG" "$q" | wc -l
    echo "ripgrep:"
    time rg "$q" . | wc -l
    echo "grep:"
    time grep -r "$q" . | wc -l
    echo ""
done

echo ""
echo "=== 3. Throughput ($THROUGHPUT_N queries) ==="
echo "xgrep (Rust):"
time for i in $(seq 1 "$THROUGHPUT_N"); do "$XGREP_RUST" "struct file_operations" > /dev/null; done

echo "xgrep (Zig):"
time for i in $(seq 1 "$THROUGHPUT_N"); do "$XGREP_ZIG" "struct file_operations" > /dev/null; done

echo "ripgrep:"
time for i in $(seq 1 "$THROUGHPUT_N"); do rg "struct file_operations" . > /dev/null; done

echo "grep:"
time for i in $(seq 1 "$THROUGHPUT_N"); do grep -r "struct file_operations" . > /dev/null; done

echo ""
echo "=== 4. Memory Usage ==="
echo "xgrep Rust (index build):"
/usr/bin/time -l "$XGREP_RUST" init 2>&1 | grep -i "maximum resident" || echo "(measurement failed)"
echo "xgrep Zig (index build):"
/usr/bin/time -l "$XGREP_ZIG" init 2>&1 | grep -i "maximum resident" || echo "(measurement failed)"
echo "xgrep Rust (search):"
/usr/bin/time -l "$XGREP_RUST" "struct file_operations" > /dev/null 2>&1 | grep -i "maximum resident" || echo "(measurement failed)"
echo "xgrep Zig (search):"
/usr/bin/time -l "$XGREP_ZIG" "struct file_operations" > /dev/null 2>&1 | grep -i "maximum resident" || echo "(measurement failed)"
echo "ripgrep (search):"
/usr/bin/time -l rg "struct file_operations" . > /dev/null 2>&1 | grep -i "maximum resident" || echo "(measurement failed)"

echo ""
echo "=== Index Size ==="
du -h ~/Library/Caches/xgrep/*/index 2>/dev/null || echo "(no index found)"
