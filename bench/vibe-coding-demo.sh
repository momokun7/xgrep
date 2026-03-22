#!/bin/bash
# バイブコーディング中のxgrep活用シナリオ + grep/ripgrep比較
# 実際の開発フローを再現

XGREP="$HOME/Developer/oss/xgrep/rust/target/release/xgrep"
PROJECT="$HOME/Developer/work/one-admin"

cd "$PROJECT"

echo "================================================"
echo "  バイブコーディング x xgrep 実践デモ"
echo "  プロジェクト: $(basename $PROJECT)"
echo "================================================"
echo ""

# シナリオ1: 関数の使用箇所を探す
echo "--- シナリオ1: 関数の使用箇所を探す ---"
echo ""
echo "[xgrep]"
time $XGREP "useState" --type ts 2>/dev/null | head -5
echo "  ..."
echo ""
echo "[ripgrep]"
time rg "useState" --type ts . 2>/dev/null | head -5
echo "  ..."
echo ""
echo "[grep]"
time grep -rn "useState" --include="*.ts" --include="*.tsx" . 2>/dev/null | head -5
echo "  ..."
echo ""

# シナリオ2: AIにコンテキストを渡す
echo "--- シナリオ2: AIに渡すコードを取得 ---"
echo ""
echo "[xgrep --format llm]"
time $XGREP "useState" --format llm --type ts -C 3 --max-count 2 2>/dev/null
echo ""
echo "(grep/ripgrepにはこの機能がない)"
echo ""

# シナリオ3: 正規表現検索
echo "--- シナリオ3: 正規表現でHookを検索 ---"
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

# シナリオ4: ファイル一覧
echo "--- シナリオ4: マッチファイル一覧 ---"
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

# シナリオ5: case-insensitive
echo "--- シナリオ5: 大文字小文字無視 ---"
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

# シナリオ6: JSON出力
echo "--- シナリオ6: JSON出力 ---"
echo ""
echo "[xgrep]"
time $XGREP "export" --json --max-count 3 2>/dev/null
echo ""
echo "[ripgrep]"
time rg "export" --json . --max-count 3 2>/dev/null | head -5
echo ""
echo "(grepにはJSON出力がない)"
echo ""

# シナリオ7: Git変更ファイルのみ
echo "--- シナリオ7: 変更ファイルからTODO ---"
echo ""
echo "[xgrep]"
time $XGREP "TODO" --changed 2>/dev/null || echo "(変更なし)"
echo ""
echo "[ripgrep + git]"
time bash -c 'git diff --name-only | xargs rg "TODO" 2>/dev/null' || echo "(変更なし)"
echo ""
echo "(grepでやるには手動でファイルリスト作成が必要)"
echo ""

echo "================================================"
echo "  全シナリオ完了"
echo "================================================"
