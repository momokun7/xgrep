#!/usr/bin/env bash
#
# xg --find vs fd vs find: Reproducible Benchmark
#
# Prerequisites:
#   - cargo (Rust toolchain)
#   - fd (https://github.com/sharkdp/fd)
#   - hyperfine (https://github.com/sharkdp/hyperfine)
#   - git
#
# Usage:
#   ./benchmarks/bench_find.sh
#
# This script clones public repositories to a temporary directory,
# builds xg in release mode, creates indexes, and runs benchmarks.
# All temporary data is cleaned up automatically.

set -euo pipefail

# --- Configuration -----------------------------------------------------------

REPOS=(
    "https://github.com/tokio-rs/tokio.git"           # ~1,200 files (Rust async runtime)
    "https://github.com/vercel/next.js.git"            # ~20,000+ files (React framework)
)

REPO_NAMES=(
    "tokio"
    "next.js"
)

# Shallow clone depth (1 = only latest commit, minimizes download)
CLONE_DEPTH=1

# --- Dependency check --------------------------------------------------------

for cmd in cargo fd hyperfine git; do
    if ! command -v "$cmd" &>/dev/null; then
        echo "error: '$cmd' is required but not found" >&2
        exit 1
    fi
done

# --- Setup -------------------------------------------------------------------

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TMPDIR="$(mktemp -d)"
XG_BIN="$PROJECT_ROOT/rust/target/release/xg"

cleanup() {
    echo ""
    echo "Cleaning up $TMPDIR ..."
    rm -rf "$TMPDIR"
}
trap cleanup EXIT

echo "=== xg --find Benchmark ==="
echo ""
echo "Temporary directory: $TMPDIR"
echo ""

# --- Build xg ----------------------------------------------------------------

echo "Building xg (release) ..."
(cd "$PROJECT_ROOT/rust" && cargo build --release --quiet)
echo "  -> $XG_BIN"
echo ""

# --- Clone & Benchmark -------------------------------------------------------

for i in "${!REPOS[@]}"; do
    REPO_URL="${REPOS[$i]}"
    REPO_NAME="${REPO_NAMES[$i]}"
    REPO_DIR="$TMPDIR/$REPO_NAME"

    echo "================================================================"
    echo "Repository: $REPO_NAME ($REPO_URL)"
    echo "================================================================"
    echo ""

    # Clone
    echo "Cloning (depth=$CLONE_DEPTH) ..."
    git clone --depth "$CLONE_DEPTH" --quiet "$REPO_URL" "$REPO_DIR" 2>/dev/null
    FILE_COUNT=$(fd --type f . "$REPO_DIR" | wc -l | tr -d ' ')
    echo "  -> $FILE_COUNT files"
    echo ""

    # Build xg index
    echo "Building xg index ..."
    INDEX_OUTPUT=$("$XG_BIN" init "$REPO_DIR" 2>&1)
    echo "  -> $INDEX_OUTPUT"
    echo ""

    # --- Benchmark 1: Glob pattern (extension match) -------------------------

    # Determine file extension based on repo
    case "$REPO_NAME" in
        tokio)     EXT="rs";  GLOB="*.rs"  ;;
        next.js)   EXT="ts";  GLOB="*.ts"  ;;
        *)         EXT="py";  GLOB="*.py"  ;;
    esac

    XG_COUNT=$("$XG_BIN" --find "$GLOB" "$REPO_DIR" 2>/dev/null | wc -l | tr -d ' ')
    FD_COUNT=$(fd -e "$EXT" . "$REPO_DIR" | wc -l | tr -d ' ')

    echo "--- Glob: $GLOB ($XG_COUNT files, fd: $FD_COUNT) ---"
    if [ "$XG_COUNT" != "$FD_COUNT" ]; then
        echo "  WARNING: result count mismatch (xg=$XG_COUNT, fd=$FD_COUNT)"
        echo "  This may be due to .gitignore handling differences."
    fi
    echo ""

    hyperfine -N --warmup 5 --min-runs 50 \
        --command-name "xg --find '$GLOB'" \
            "$XG_BIN --find '$GLOB' $REPO_DIR" \
        --command-name "fd -e $EXT" \
            "fd -e $EXT . $REPO_DIR" \
        --command-name "find -name '$GLOB'" \
            "find $REPO_DIR -name '$GLOB' -type f"

    echo ""

    # --- Benchmark 2: Substring match ----------------------------------------

    SUBSTR="config"
    XG_SUB=$("$XG_BIN" --find "$SUBSTR" "$REPO_DIR" 2>/dev/null | wc -l | tr -d ' ')
    FD_SUB=$(fd "$SUBSTR" "$REPO_DIR" | wc -l | tr -d ' ')

    echo "--- Substring: '$SUBSTR' ($XG_SUB files, fd: $FD_SUB) ---"
    echo ""

    hyperfine -N --warmup 5 --min-runs 50 \
        --command-name "xg --find '$SUBSTR'" \
            "$XG_BIN --find $SUBSTR $REPO_DIR" \
        --command-name "fd '$SUBSTR'" \
            "fd $SUBSTR $REPO_DIR" \
        --command-name "find -name '*${SUBSTR}*'" \
            "find $REPO_DIR -name '*${SUBSTR}*' -type f"

    echo ""
done

echo "================================================================"
echo "Done."
echo "================================================================"
