#!/usr/bin/env bash
# Pre-PR checks: the same commands CI runs for the Rust crate.
# Usage: ./scripts/check.sh [--quick]   (--quick skips the no-default-features pass)
set -euo pipefail

cd "$(dirname "$0")/../rust"

run() {
    echo "==> $*"
    "$@"
}

run cargo fmt -- --check
run cargo clippy --all-targets -- -D warnings
run cargo test

if [[ "${1:-}" != "--quick" ]]; then
    run cargo clippy --all-targets --no-default-features -- -D warnings
    run cargo test --no-default-features
fi

echo "All checks passed."
