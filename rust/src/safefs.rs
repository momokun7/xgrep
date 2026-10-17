//! Filesystem helpers that keep reads inside the search root and writes
//! inside the index directory.
//!
//! The searched tree is not trusted: it may contain symlinks, and paths may
//! come from an index or sidecar file on disk. Reads of corpus files go
//! through [`is_safe_rel`] and [`is_regular_file`]; writes next to the index
//! go through [`write_atomic`] / [`temp_file_in`], which never open an
//! existing path.

use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path};

/// Prefix of temp files created next to the index. Matched by
/// `builder::is_xgrep_internal`, so a leftover temp file is never indexed.
pub(crate) const TEMP_PREFIX: &str = ".xgrep_tmp_";

/// True if `rel` is a non-empty relative path made only of normal components:
/// no root, drive or UNC prefix, `.` or `..`. Joining such a path to a root
/// always stays under that root (symlinks aside).
pub(crate) fn is_safe_rel(rel: &Path) -> bool {
    let mut any = false;
    for c in rel.components() {
        match c {
            Component::Normal(s) => {
                // On Windows a `:` inside a component names a drive or an
                // alternate data stream; no plain file name contains one.
                if cfg!(windows) && s.to_string_lossy().contains(':') {
                    return false;
                }
                any = true;
            }
            _ => return false,
        }
    }
    any
}

/// True if `rel` cannot leave the directory it is joined to, judged from the
/// string alone: no root, drive or UNC prefix and no `..`. Empty and `.`
/// paths (the directory itself) are allowed. Checking before touching the
/// filesystem matters on Windows, where merely resolving a UNC path
/// contacts that server.
pub(crate) fn is_contained_rel(rel: &Path) -> bool {
    rel.components().all(|c| match c {
        Component::CurDir => true,
        Component::Normal(s) => !(cfg!(windows) && s.to_string_lossy().contains(':')),
        _ => false,
    })
}

/// True if `path` is a regular file. Symlinks are not followed, so a link
/// pointing outside the root (or at a device or FIFO) is not a file here.
pub(crate) fn is_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
}

/// Create `dir` (and its parents) and check that `dir` itself is a real
/// directory, not a symlink or junction to somewhere else.
pub(crate) fn create_real_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let meta = fs::symlink_metadata(dir)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a directory",
            dir.display()
        )));
    }
    Ok(())
}

/// Create a new, uniquely named temp file in `dir`. The file is created
/// exclusively, so an existing file or symlink at that name is never opened.
pub(crate) fn temp_file_in(dir: &Path) -> io::Result<tempfile::NamedTempFile> {
    tempfile::Builder::new()
        .prefix(TEMP_PREFIX)
        .tempfile_in(dir)
}

/// Write `data` to `path` through a new temp file in the same directory and
/// rename it into place. A symlink at `path` is replaced, not followed.
pub(crate) fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let mut tmp = temp_file_in(dir)?;
    tmp.write_all(data)?;
    rename_into_place(tmp.into_temp_path(), path)
}

/// Rename a temp file over `dst` with `std::fs::rename`, which on Windows
/// also replaces a file that is open or memory-mapped (an index being read).
/// The temp file is removed if the rename fails.
pub(crate) fn rename_into_place(tmp: tempfile::TempPath, dst: &Path) -> io::Result<()> {
    let src = tmp.keep().map_err(|e| e.error)?;
    fs::rename(&src, dst).inspect_err(|_| {
        let _ = fs::remove_file(&src);
    })
}

/// Make `opts` open the final path component itself instead of following a
/// symlink there (`O_NOFOLLOW` on Unix, the reparse point on Windows).
pub(crate) fn no_follow(opts: &mut fs::OpenOptions) -> &mut fs::OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        opts.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    opts
}

/// Open `path` for reading if it is a regular file. Returns `Ok(None)` for a
/// symlink (not followed), directory, device or FIFO; opening never blocks.
pub(crate) fn open_regular_file(path: &Path) -> io::Result<Option<fs::File>> {
    let mut opts = fs::OpenOptions::new();
    opts.read(true);
    no_follow(&mut opts);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A FIFO would block the open; O_NONBLOCK has no effect on files.
        opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = match opts.open(path) {
        Ok(f) => f,
        // O_NOFOLLOW reports a symlink as ELOOP.
        #[cfg(unix)]
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Ok(None),
        Err(e) => return Err(e),
    };
    if file.metadata()?.file_type().is_file() {
        Ok(Some(file))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_rel_accepts_plain_relative_paths() {
        assert!(is_safe_rel(Path::new("a.txt")));
        assert!(is_safe_rel(Path::new("src/deep/b.rs")));
        assert!(is_safe_rel(Path::new("日本/語.txt")));
    }

    #[test]
    fn safe_rel_rejects_escapes() {
        for p in ["", ".", "..", "../x", "a/../../x", "./a", "/etc/passwd"] {
            assert!(!is_safe_rel(Path::new(p)), "{p:?} should be rejected");
        }
        if cfg!(windows) {
            for p in [r"C:\Windows", r"C:x", r"\\server\share\x", r"\x", "a:b"] {
                assert!(!is_safe_rel(Path::new(p)), "{p:?} should be rejected");
            }
        }
    }

    #[test]
    fn write_atomic_writes_and_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        write_atomic(&p, b"one").unwrap();
        write_atomic(&p, b"two").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(TEMP_PREFIX))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_replaces_a_symlink_instead_of_following_it() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, "keep").unwrap();
        let p = dir.path().join("f");
        std::os::unix::fs::symlink(&victim, &p).unwrap();
        write_atomic(&p, b"new").unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
        assert!(!fs::symlink_metadata(&p).unwrap().file_type().is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn regular_file_and_real_dir_reject_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("t");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("x"), "x").unwrap();
        let link = dir.path().join("l");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let file_link = dir.path().join("fl");
        std::os::unix::fs::symlink(target.join("x"), &file_link).unwrap();
        assert!(is_regular_file(&target.join("x")));
        assert!(!is_regular_file(&file_link));
        assert!(!is_regular_file(Path::new("/dev/null")));
        assert!(create_real_dir(&target).is_ok());
        assert!(create_real_dir(&link).is_err());
    }
}
