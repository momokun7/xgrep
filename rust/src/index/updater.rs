use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use ignore::WalkBuilder;

/// インデックスの鮮度チェック結果
#[derive(Debug)]
pub enum IndexStatus {
    /// インデックスは最新、変更なし
    Fresh,
    /// インデックスは存在するが一部ファイルが変更済み。インデックス検索+変更ファイル直接スキャンで対応
    Stale { changed_files: Vec<PathBuf> },
    /// インデックスが存在しない、フルビルドが必要
    NeedsFullBuild,
}

/// インデックスと一緒に保存するメタデータ
#[derive(Debug)]
struct IndexMeta {
    commit_hash: Option<String>,
}

impl IndexMeta {
    fn path_for(index_path: &Path) -> PathBuf {
        index_path.with_extension("meta")
    }

    fn load(index_path: &Path) -> Option<Self> {
        let meta_path = Self::path_for(index_path);
        let content = fs::read_to_string(&meta_path).ok()?;
        let commit_hash = content.lines().next().map(|s| s.trim().to_string());
        Some(IndexMeta { commit_hash })
    }

    fn save(index_path: &Path, commit_hash: Option<&str>) -> Result<()> {
        let meta_path = Self::path_for(index_path);
        let content = commit_hash.unwrap_or("");
        fs::write(&meta_path, content)?;
        Ok(())
    }
}

/// 現在のgit HEADコミットハッシュを取得
fn current_commit_hash(root: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

/// ディレクトリ内の最新ファイルのmtimeを取得（UNIX epoch秒）
fn newest_file_mtime(root: &Path) -> Option<u64> {
    let mut newest = 0u64;
    for entry in WalkBuilder::new(root).build().flatten() {
        if entry.file_type().is_some_and(|ft| ft.is_file()) {
            if let Ok(meta) = entry.metadata() {
                if let Ok(mtime) = meta.modified() {
                    let secs = mtime
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    if secs > newest {
                        newest = secs;
                    }
                }
            }
        }
    }
    if newest > 0 {
        Some(newest)
    } else {
        None
    }
}

/// `git status --porcelain` の1行からファイルパスを抽出する
///
/// フォーマット: "XY filename" or "XY \"filename with spaces\"" or "XY old -> new"
/// リネームの場合は旧パスと新パスの両方を返す（旧パスのstaleエントリも除外するため）
fn parse_status_paths(line: &str) -> Vec<String> {
    if line.len() < 4 {
        return vec![];
    }
    let path_part = &line[3..];

    let clean = |p: &str| -> String {
        let p = if p.starts_with('"') && p.ends_with('"') {
            &p[1..p.len() - 1]
        } else {
            p
        };
        p.to_string()
    };

    if let Some(arrow_pos) = path_part.find(" -> ") {
        // リネームの場合: 旧パスと新パスの両方を返す
        let old = &path_part[..arrow_pos];
        let new = &path_part[arrow_pos + 4..];
        let old = clean(old);
        let new = clean(new);
        let mut result = vec![];
        if !old.is_empty() {
            result.push(old);
        }
        if !new.is_empty() {
            result.push(new);
        }
        result
    } else {
        let path = clean(path_part);
        if path.is_empty() {
            vec![]
        } else {
            vec![path]
        }
    }
}

/// 2つのコミット間の変更ファイル + 未コミットの変更ファイルを取得
fn changed_files_since(root: &Path, old_hash: &str) -> Result<Vec<String>> {
    let mut files = std::collections::HashSet::new();

    // コミット済みの変更: old_hash..HEAD
    let output = std::process::Command::new("git")
        .args([
            "diff-tree",
            "-r",
            "--name-only",
            "--no-commit-id",
            old_hash,
            "HEAD",
        ])
        .current_dir(root)
        .output()?;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim();
        if !line.is_empty() {
            files.insert(line.to_string());
        }
    }

    // 未コミットの変更（staged + unstaged、追跡済みファイルのみ）
    // -uno でuntracked filesを除外し、大規模リポジトリでのハングを防止
    let output = std::process::Command::new("git")
        .args(["status", "--porcelain", "-uno"])
        .current_dir(root)
        .output()?;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        for path in parse_status_paths(line) {
            files.insert(path);
        }
    }

    // 未追跡ファイル（.gitignoreを尊重した高速な列挙）
    // git status --porcelain の代わりに ls-files --others を使うことで
    // node_modules等の大量のuntracked filesがある場合でも高速に動作する
    let output = std::process::Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard"])
        .current_dir(root)
        .output()?;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim();
        if !line.is_empty() {
            files.insert(line.to_string());
        }
    }

    let mut result: Vec<String> = files.into_iter().collect();
    result.sort();
    Ok(result)
}

/// インデックスメタデータを保存する（build_index後に呼ぶ）
pub fn save_meta(root: &Path, index_path: &Path) -> Result<()> {
    let hash = current_commit_hash(root);
    IndexMeta::save(index_path, hash.as_deref())
}

/// インデックスの鮮度をチェックし、変更ファイルリストを返す（リビルドしない）
pub fn check_index_status(root: &Path, index_path: &Path) -> Result<IndexStatus> {
    if !index_path.exists() {
        return Ok(IndexStatus::NeedsFullBuild);
    }

    let meta = IndexMeta::load(index_path);
    let current_hash = current_commit_hash(root);

    let mut changed = std::collections::HashSet::new();

    match (&meta, &current_hash) {
        (Some(m), Some(curr)) => {
            // コミット済み変更をチェック
            if m.commit_hash.as_deref() != Some(curr.as_str()) {
                let old_hash = m.commit_hash.as_deref().unwrap_or("");
                if let Ok(files) = changed_files_since(root, old_hash) {
                    for f in files {
                        changed.insert(PathBuf::from(f));
                    }
                }
            }

            // 未コミット変更（staged + unstaged、untracked除外で高速化）
            let output = std::process::Command::new("git")
                .args(["status", "--porcelain", "-uno"])
                .current_dir(root)
                .output()?;
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                for path in parse_status_paths(line) {
                    changed.insert(PathBuf::from(path));
                }
            }

            // 未追跡ファイル（インデックスに含まれていない新規ファイル）
            let output = std::process::Command::new("git")
                .args(["ls-files", "--others", "--exclude-standard"])
                .current_dir(root)
                .output()?;
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let line = line.trim();
                if !line.is_empty() {
                    changed.insert(PathBuf::from(line));
                }
            }
        }
        _ => {
            // 非Gitリポジトリ or メタデータなし: mtimeベースで鮮度判定
            let index_mtime = fs::metadata(index_path)?
                .modified()?
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            if let Some(newest) = newest_file_mtime(root) {
                if index_mtime >= newest {
                    return Ok(IndexStatus::Fresh);
                }
            }
            return Ok(IndexStatus::NeedsFullBuild);
        }
    }

    if changed.is_empty() {
        Ok(IndexStatus::Fresh)
    } else if changed.len() > 500 {
        // 変更が多すぎる場合はリビルドの方が効率的
        Ok(IndexStatus::NeedsFullBuild)
    } else {
        let mut files: Vec<PathBuf> = changed.into_iter().collect();
        files.sort();
        Ok(IndexStatus::Stale {
            changed_files: files,
        })
    }
}

/// キャッシュ付きでインデックスをビルドする（増分更新）
fn build_with_cache(root: &Path, index_path: &Path) -> Result<()> {
    let cache_path = crate::index::builder::cache_path_for(index_path);
    crate::index::builder::build_index_with_cache(root, index_path, Some(&cache_path))
}

/// インデックスが最新かチェックし、必要に応じて再構築する
pub fn ensure_fresh_index(root: &Path, index_path: &Path) -> Result<()> {
    if !index_path.exists() {
        // インデックスが存在しない場合はフルビルド（キャッシュ作成付き）
        eprintln!("[indexing...]");
        build_with_cache(root, index_path)?;
        save_meta(root, index_path)?;
        eprintln!("[done]");
        return Ok(());
    }

    // インデックスが存在する場合、更新が必要かチェック
    let meta = IndexMeta::load(index_path);
    let current_hash = current_commit_hash(root);

    match (&meta, &current_hash) {
        (Some(m), Some(curr)) if m.commit_hash.as_deref() == Some(curr.as_str()) => {
            // 同じコミット。未コミットの変更があるかチェック
            // -uno で追跡済みファイルの変更を確認（untracked除外で高速化）
            let output = std::process::Command::new("git")
                .args(["status", "--porcelain", "-uno"])
                .current_dir(root)
                .output()?;
            let tracked_changes = String::from_utf8_lossy(&output.stdout);
            // 未追跡ファイルも確認（.gitignoreを尊重）
            let output = std::process::Command::new("git")
                .args(["ls-files", "--others", "--exclude-standard"])
                .current_dir(root)
                .output()?;
            let untracked = String::from_utf8_lossy(&output.stdout);
            if tracked_changes.trim().is_empty() && untracked.trim().is_empty() {
                // 変更なし、インデックスは最新
                return Ok(());
            }
            // 未コミットの変更あり、キャッシュ付き増分再構築
            eprintln!("[updating index...]");
            build_with_cache(root, index_path)?;
            IndexMeta::save(index_path, Some(curr))?;
            eprintln!("[done]");
        }
        (Some(m), Some(curr)) => {
            // コミットが異なる
            let old_hash = m.commit_hash.as_deref().unwrap_or("");
            let changed = changed_files_since(root, old_hash)?;

            if changed.is_empty() {
                // ファイル変更なし（merge commitなど）
                IndexMeta::save(index_path, Some(curr))?;
                return Ok(());
            }

            // 変更があるのでキャッシュ付き増分リビルド
            eprintln!("[updating index ({} files changed)...]", changed.len());
            build_with_cache(root, index_path)?;
            IndexMeta::save(index_path, Some(curr))?;
            eprintln!("[done]");
        }
        _ => {
            // 非Gitリポジトリ or メタデータなし: mtimeベースで鮮度判定
            let index_mtime = fs::metadata(index_path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);

            let needs_rebuild = match newest_file_mtime(root) {
                Some(newest) => index_mtime < newest,
                None => true,
            };

            if needs_rebuild {
                eprintln!("[updating index...]");
                build_with_cache(root, index_path)?;
                save_meta(root, index_path)?;
                eprintln!("[done]");
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::tempdir;

    fn init_git_repo(dir: &Path) {
        Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "test"])
            .current_dir(dir)
            .output()
            .unwrap();
    }

    #[test]
    fn test_ensure_fresh_index_creates_new() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("hello.txt"), "hello world").unwrap();
        let index_path = root.join("test.xgrep");

        ensure_fresh_index(root, &index_path).unwrap();

        assert!(index_path.exists());
    }

    #[test]
    fn test_ensure_fresh_index_no_rebuild_if_unchanged() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        init_git_repo(root);
        // インデックスファイルをgitignoreに追加（git statusで検出されないようにする）
        fs::write(root.join(".gitignore"), "*.xgrep\n*.meta\n*.cache\n").unwrap();
        fs::write(root.join("hello.txt"), "hello world").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();

        let index_path = root.join("test.xgrep");
        ensure_fresh_index(root, &index_path).unwrap();

        let mtime1 = fs::metadata(&index_path).unwrap().modified().unwrap();

        // 変更なしで再実行
        std::thread::sleep(std::time::Duration::from_millis(100));
        ensure_fresh_index(root, &index_path).unwrap();

        let mtime2 = fs::metadata(&index_path).unwrap().modified().unwrap();
        assert_eq!(mtime1, mtime2); // インデックスは再構築されていない
    }

    #[test]
    fn test_ensure_fresh_index_rebuilds_after_commit() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        init_git_repo(root);
        fs::write(root.join("hello.txt"), "hello world").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();

        let index_path = root.join("test.xgrep");
        ensure_fresh_index(root, &index_path).unwrap();

        let mtime1 = fs::metadata(&index_path).unwrap().modified().unwrap();

        // 新しいコミットを作成
        std::thread::sleep(std::time::Duration::from_millis(100));
        fs::write(root.join("new_file.txt"), "new content").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "add file"])
            .current_dir(root)
            .output()
            .unwrap();

        ensure_fresh_index(root, &index_path).unwrap();

        let mtime2 = fs::metadata(&index_path).unwrap().modified().unwrap();
        assert_ne!(mtime1, mtime2); // インデックスが再構築された
    }

    #[test]
    fn test_meta_save_load() {
        let dir = tempdir().unwrap();
        let index_path = dir.path().join("test.xgrep");
        fs::write(&index_path, "dummy").unwrap();

        IndexMeta::save(&index_path, Some("abc123")).unwrap();
        let meta = IndexMeta::load(&index_path).unwrap();
        assert_eq!(meta.commit_hash, Some("abc123".to_string()));
    }

    #[test]
    fn test_meta_load_missing() {
        let dir = tempdir().unwrap();
        let index_path = dir.path().join("nonexistent.xgrep");
        assert!(IndexMeta::load(&index_path).is_none());
    }

    #[test]
    fn test_ensure_fresh_index_rebuilds_on_uncommitted_changes() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        init_git_repo(root);
        fs::write(root.join("hello.txt"), "hello world").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();

        let index_path = root.join("test.xgrep");
        ensure_fresh_index(root, &index_path).unwrap();

        let mtime1 = fs::metadata(&index_path).unwrap().modified().unwrap();

        // 未コミットの変更を加える（同じコミット、ダーティな作業ツリー）
        std::thread::sleep(std::time::Duration::from_millis(100));
        fs::write(root.join("hello.txt"), "changed content").unwrap();

        ensure_fresh_index(root, &index_path).unwrap();

        let mtime2 = fs::metadata(&index_path).unwrap().modified().unwrap();
        assert_ne!(mtime1, mtime2); // 未コミットの変更によりインデックスが再構築された
    }

    #[test]
    fn test_ensure_fresh_index_rebuilds_on_new_untracked_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        init_git_repo(root);
        fs::write(root.join("hello.txt"), "hello world").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();

        let index_path = root.join("test.xgrep");
        ensure_fresh_index(root, &index_path).unwrap();

        let mtime1 = fs::metadata(&index_path).unwrap().modified().unwrap();

        // 新しい未追跡ファイルを追加
        std::thread::sleep(std::time::Duration::from_millis(100));
        fs::write(root.join("new_file.txt"), "new content").unwrap();

        ensure_fresh_index(root, &index_path).unwrap();

        let mtime2 = fs::metadata(&index_path).unwrap().modified().unwrap();
        assert_ne!(mtime1, mtime2); // 新しい未追跡ファイルによりインデックスが再構築された
    }

    #[test]
    fn test_parse_status_paths_simple() {
        assert_eq!(
            parse_status_paths(" M hello.txt"),
            vec!["hello.txt".to_string()]
        );
    }

    #[test]
    fn test_parse_status_paths_rename() {
        let paths = parse_status_paths("R  old.txt -> new.txt");
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&"old.txt".to_string()));
        assert!(paths.contains(&"new.txt".to_string()));
    }

    #[test]
    fn test_parse_status_paths_quoted() {
        assert_eq!(
            parse_status_paths(" M \"file with spaces.txt\""),
            vec!["file with spaces.txt".to_string()]
        );
    }

    #[test]
    fn test_parse_status_paths_short() {
        assert!(parse_status_paths("M").is_empty());
    }

    #[test]
    fn test_parse_status_paths_empty_line() {
        assert!(parse_status_paths("?? ").is_empty());
    }

    #[test]
    fn test_check_index_status_fresh() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        init_git_repo(root);
        fs::write(root.join(".gitignore"), "*.xgrep\n*.meta\n*.cache\n").unwrap();
        fs::write(root.join("hello.txt"), "hello world").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();

        let index_path = root.join("test.xgrep");
        ensure_fresh_index(root, &index_path).unwrap();

        let status = check_index_status(root, &index_path).unwrap();
        assert!(matches!(status, IndexStatus::Fresh));
    }

    #[test]
    fn test_check_index_status_stale() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        init_git_repo(root);
        fs::write(root.join(".gitignore"), "*.xgrep\n*.meta\n*.cache\n").unwrap();
        fs::write(root.join("hello.txt"), "hello world").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();

        let index_path = root.join("test.xgrep");
        ensure_fresh_index(root, &index_path).unwrap();

        // ファイルを変更
        fs::write(root.join("hello.txt"), "changed").unwrap();

        let status = check_index_status(root, &index_path).unwrap();
        match status {
            IndexStatus::Stale { changed_files } => {
                assert!(!changed_files.is_empty());
            }
            other => panic!("expected Stale, got {:?}", other),
        }
    }

    #[test]
    fn test_check_index_status_no_index() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let index_path = root.join("nonexistent.xgrep");

        let status = check_index_status(root, &index_path).unwrap();
        assert!(matches!(status, IndexStatus::NeedsFullBuild));
    }

    #[test]
    fn test_check_index_status_stale_new_untracked_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        init_git_repo(root);
        fs::write(root.join(".gitignore"), "*.xgrep\n*.meta\n*.cache\n").unwrap();
        fs::write(root.join("hello.txt"), "hello world").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();

        let index_path = root.join("test.xgrep");
        ensure_fresh_index(root, &index_path).unwrap();

        // 新しい未追跡ファイルを追加
        fs::write(root.join("new_file.txt"), "new content").unwrap();

        let status = check_index_status(root, &index_path).unwrap();
        match status {
            IndexStatus::Stale { changed_files } => {
                assert!(changed_files
                    .iter()
                    .any(|p| p.to_string_lossy().contains("new_file.txt")));
            }
            other => panic!("expected Stale, got {:?}", other),
        }
    }

    #[test]
    fn test_non_git_mtime_freshness() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello").unwrap();

        let index_path = root.join("index.xgrep");
        crate::index::builder::build_index(root, &index_path).unwrap();
        save_meta(root, &index_path).unwrap();

        // インデックスがビルド直後なのでFreshのはず
        let status = check_index_status(root, &index_path).unwrap();
        assert!(
            matches!(status, IndexStatus::Fresh),
            "expected Fresh, got {:?}",
            status
        );

        // 少し待ってからファイルを変更
        std::thread::sleep(std::time::Duration::from_secs(1));
        fs::write(root.join("b.txt"), "world").unwrap();

        // インデックスがNeedsFullBuildになるはず
        let status = check_index_status(root, &index_path).unwrap();
        assert!(
            matches!(status, IndexStatus::NeedsFullBuild),
            "expected NeedsFullBuild, got {:?}",
            status
        );
    }

    #[test]
    fn test_non_git_ensure_fresh_skips_rebuild_when_fresh() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello").unwrap();

        let index_path = root.join("test.xgrep");
        ensure_fresh_index(root, &index_path).unwrap();

        let mtime1 = fs::metadata(&index_path).unwrap().modified().unwrap();

        // 変更なしで再実行
        std::thread::sleep(std::time::Duration::from_millis(100));
        ensure_fresh_index(root, &index_path).unwrap();

        let mtime2 = fs::metadata(&index_path).unwrap().modified().unwrap();
        assert_eq!(mtime1, mtime2); // リビルドされていない
    }

    #[test]
    fn test_check_index_status_stale_after_commit() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        init_git_repo(root);
        fs::write(root.join(".gitignore"), "*.xgrep\n*.meta\n*.cache\n").unwrap();
        fs::write(root.join("hello.txt"), "hello world").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(root)
            .output()
            .unwrap();

        let index_path = root.join("test.xgrep");
        ensure_fresh_index(root, &index_path).unwrap();

        // 新しいコミット
        fs::write(root.join("new_file.txt"), "new content").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "add file"])
            .current_dir(root)
            .output()
            .unwrap();

        let status = check_index_status(root, &index_path).unwrap();
        match status {
            IndexStatus::Stale { changed_files } => {
                assert!(!changed_files.is_empty());
            }
            other => panic!("expected Stale, got {:?}", other),
        }
    }
}
