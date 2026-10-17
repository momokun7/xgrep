#!/bin/bash
set -euo pipefail

echo "=== Environment ==="
echo "CPU: $(nproc) cores"
echo "RAM: $(free -h | awk '/^Mem:/{print $2}')"
echo "xg: $(xg --help 2>&1 | head -1)"
echo "ripgrep: $(rg --version | head -1)"
echo ""

echo "=== Correctness ==="
cd /test/ripgrep-src
xg init 2>&1
XC=$(xg "fn main" | wc -l)
RC=$(rg "fn main" . | wc -l)
echo "xgrep: $XC, ripgrep: $RC"
if [ "$XC" = "$RC" ]; then echo "OK"; else echo "MISMATCH"; fi
echo ""

echo "=== Index Build ==="
rm -rf ~/.cache/xgrep
/usr/bin/time -v xg init 2>&1 | grep -E "(Index built|Maximum resident|wall clock)"
echo ""

echo "=== Search Latency (medium dataset) ==="
hyperfine --warmup 5 --runs 20 \
  'xg "fn main"' \
  'rg "fn main" .' \
  2>&1
echo ""

echo "=== Memory Usage ==="
/usr/bin/time -v xg "pub struct" 2>&1 | grep -E "Maximum resident"
/usr/bin/time -v rg "pub struct" . 2>&1 | grep -E "Maximum resident"
