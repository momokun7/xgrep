#!/usr/bin/env bash
# xgrep vs ripgrep side-by-side demo
# Output is shown as is, so the speed difference is visible

XGREP="$HOME/Developer/oss/xgrep/rust/target/release/xgrep"
DIR="$HOME/Developer/oss/xgrep/bench/linux-src"

cd "$DIR"

echo "=========================================="
echo "  xgrep vs ripgrep side by side"
echo "  Dataset: Linux kernel (92k files)"
echo "=========================================="
echo ""

for query in "struct file_operations" "EXPORT_SYMBOL" "mutex_lock"; do
    echo "--- query: '$query' ---"
    echo ""
    echo "[xgrep]"
    time $XGREP "$query" | head -5
    echo "  ... ($(${XGREP} "$query" | wc -l | tr -d ' ') hits)"
    echo ""
    echo "[ripgrep]"
    time rg "$query" . | head -5
    echo "  ... ($(rg "$query" . | wc -l | tr -d ' ') hits)"
    echo ""
    echo ""
done
