#!/bin/bash
# xgrep in a vibe-coding session, compared with grep/ripgrep
# Replays a typical development flow

XGREP="$HOME/Developer/oss/xgrep/rust/target/release/xgrep"
PROJECT="$HOME/Developer/work/one-admin"

cd "$PROJECT"

echo "================================================"
echo "  Vibe coding with xgrep: demo"
echo "  Project: $(basename $PROJECT)"
echo "================================================"
echo ""

# Scenario 1: find where a function is used
echo "--- Scenario 1: find where a function is used ---"
echo ""
echo "[xgrep]"
time $XGREP "useState" --type ts --max-count 5 2>/dev/null
echo ""
echo "[ripgrep]"
time rg "useState" --type ts . --max-count 5 2>/dev/null
echo ""
echo "[grep]"
time grep -rn "useState" --include="*.ts" --include="*.tsx" . -m 5 2>/dev/null
echo ""

# Scenario 2: give context to an AI
echo "--- Scenario 2: get code to hand to an AI ---"
echo ""
echo "[xgrep --format llm]"
time $XGREP "useState" --format llm --type ts -C 3 --max-count 2 2>/dev/null
echo ""
echo "(grep/ripgrep have no equivalent)"
echo ""

# Scenario 3: regex search
echo "--- Scenario 3: find hooks with a regex ---"
echo ""
echo "[xgrep]"
time $XGREP -e "use[A-Z]\w+" --type ts --max-count 5 2>/dev/null
echo ""
echo "[ripgrep]"
time rg "use[A-Z]\w+" --type ts . --max-count 5 2>/dev/null
echo ""
echo "[grep]"
time grep -rn -E "use[A-Z]\w+" --include="*.ts" --include="*.tsx" . 2>/dev/null | head -5
echo ""

# Scenario 4: list files
echo "--- Scenario 4: list matching files ---"
echo ""
echo "[xgrep]"
time $XGREP "component" -l --type ts 2>/dev/null | head -5
echo "  ..."
echo ""
echo "[ripgrep]"
time rg "component" --type ts -l . 2>/dev/null | head -5
echo "  ..."
echo ""
echo "[grep]"
time grep -rl "component" --include="*.ts" --include="*.tsx" . 2>/dev/null | head -5
echo "  ..."
echo ""

# Scenario 5: case-insensitive
echo "--- Scenario 5: ignore case ---"
echo ""
echo "[xgrep]"
time $XGREP "error" -i --max-count 5 2>/dev/null
echo ""
echo "[ripgrep]"
time rg "error" -i . --max-count 5 2>/dev/null
echo ""
echo "[grep]"
time grep -rni "error" . 2>/dev/null | head -5
echo ""

# Scenario 6: JSON output
echo "--- Scenario 6: JSON output ---"
echo ""
echo "[xgrep]"
time $XGREP "export" --json --max-count 3 2>/dev/null
echo ""
echo "[ripgrep]"
time rg "export" --json . --max-count 3 2>/dev/null | head -5
echo ""
echo "(grep has no JSON output)"
echo ""

# Scenario 7: git-changed files only
echo "--- Scenario 7: TODOs in changed files ---"
echo ""
echo "[xgrep]"
time $XGREP "TODO" --changed 2>/dev/null || echo "(no changes)"
echo ""
echo "[ripgrep + git]"
time bash -c 'git diff --name-only | xargs rg "TODO" 2>/dev/null' || echo "(no changes)"
echo ""
echo "(with grep you have to build the file list by hand)"
echo ""

echo "================================================"
echo "  All scenarios done"
echo "================================================"
