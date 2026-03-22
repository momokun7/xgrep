use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

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

/// `git status --porcelain` の1行からファイルパスを抽出する
///
/// フォーマット: "XY filename" or "XY \"filename with spaces\"" or "XY old -> new"
fn parse_status_path(line: &str) -> Option<String> {
    if line.len() < 4 {
        return None;
    }
    let path_part = &line[3..];

    // リネームの場合: "old -> new" → 新しい名前を使う
    let path = if let Some(arrow_pos) = path_part.find(" -> ") {
        &path_part[arrow_pos + 4..]
    } else {
        path_part
    };

    // クォートされたパスを処理（特殊文字を含むファイル名はgitがクォートする）
    let path = if path.starts_with('"') && path.ends_with('"') {
        &path[1..path.len() - 1]
    } else {
        path
    };

    if path.is_empty() {
        None
    } else {
        Some(path.to_string())
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

    // 未コミットの変更（staged + unstaged）
    let output = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(root)
        .output()?;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(path) = parse_status_path(line) {
            files.insert(path);
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

/// インデックスが最新かチェックし、必要に応じて再構築する
pub fn ensure_fresh_index(root: &Path, index_path: &Path) -> Result<()> {
    if !index_path.exists() {
        // インデックスが存在しない場合はフルビルド
        eprintln!("[indexing...]");
        crate::index::builder::build_index(root, index_path)?;
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
            let output = std::process::Command::new("git")
                .args(["status", "--porcelain"])
                .current_dir(root)
                .output()?;
            let status = String::from_utf8_lossy(&output.stdout);
            if status.trim().is_empty() {
                // 変更なし、インデックスは最新
                return Ok(());
            }
            // 未コミットの変更あり、再構築
            eprintln!("[updating index...]");
            crate::index::builder::build_index(root, index_path)?;
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

            // 変更があるのでフルリビルド
            eprintln!("[updating index ({} files changed)...]", changed.len());
            crate::index::builder::build_index(root, index_path)?;
            IndexMeta::save(index_path, Some(curr))?;
            eprintln!("[done]");
        }
        _ => {
            // 非Gitリポジトリ or メタデータなし、再構築
            eprintln!("[updating index...]");
            crate::index::builder::build_index(root, index_path)?;
            save_meta(root, index_path)?;
            eprintln!("[done]");
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
        fs::write(root.join(".gitignore"), "*.xgrep\n*.meta\n").unwrap();
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
        Command::new("git").args(["add", "."]).current_dir(root).output().unwrap();
        Command::new("git").args(["commit", "-m", "init"]).current_dir(root).output().unwrap();

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
        Command::new("git").args(["add", "."]).current_dir(root).output().unwrap();
        Command::new("git").args(["commit", "-m", "init"]).current_dir(root).output().unwrap();

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
    fn test_parse_status_path_simple() {
        assert_eq!(parse_status_path(" M hello.txt"), Some("hello.txt".to_string()));
    }

    #[test]
    fn test_parse_status_path_rename() {
        assert_eq!(parse_status_path("R  old.txt -> new.txt"), Some("new.txt".to_string()));
    }

    #[test]
    fn test_parse_status_path_quoted() {
        assert_eq!(
            parse_status_path(" M \"file with spaces.txt\""),
            Some("file with spaces.txt".to_string())
        );
    }

    #[test]
    fn test_parse_status_path_short() {
        assert_eq!(parse_status_path("M"), None);
    }
}
