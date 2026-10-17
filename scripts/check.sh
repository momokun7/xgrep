#!/usr/bin/env bash
# Pre-PR checks: the same commands CI runs for the Rust crate and the
# Python bindings' Rust code.
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

# Python bindings (PyO3): lint only, like the CI python-bindings job.
cd ../python
run cargo fmt -- --check
run cargo clippy -- -D warnings

# npm bindings (napi-rs): lint only, like the CI npm-bindings job.
cd ../npm
run cargo fmt -- --check
run cargo clippy -- -D warnings

echo "All checks passed."
