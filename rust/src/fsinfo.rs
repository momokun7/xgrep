//! Filesystem properties that affect freshness checking.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// True if `path` is on a filesystem whose change notifications are known
/// to be incomplete and whose `stat` is a round trip: WSL drives
/// (`/mnt/<drive>`, 9p/drvfs), NFS, SMB/CIFS, FUSE.
///
/// The answer is cached per path for the life of the process.
pub(crate) fn is_remote_fs(path: &Path) -> bool {
    static CACHE: std::sync::OnceLock<Mutex<HashMap<PathBuf, bool>>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(v) = cache.lock().ok().and_then(|c| c.get(path).copied()) {
        return v;
    }
    let v = detect_remote_fs(path);
    if let Ok(mut c) = cache.lock() {
        if c.len() > 64 {
            c.clear();
        }
        c.insert(path.to_path_buf(), v);
    }
    v
}

#[cfg(target_os = "linux")]
fn detect_remote_fs(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    const V9FS: u32 = 0x0102_1997;
    const NFS: u32 = 0x6969;
    const CIFS: u32 = 0xFF53_4D42;
    const SMB2: u32 = 0xFE53_4D42;
    const SMB: u32 = 0x517B;
    const FUSE: u32 = 0x6573_5546;
    if let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) {
        // SAFETY: `c` is NUL-terminated and `st` is a writable statfs buffer.
        let mut st: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(c.as_ptr(), &mut st) } == 0 {
            #[allow(clippy::unnecessary_cast)]
            let magic = st.f_type as u64 as u32;
            if [V9FS, NFS, CIFS, SMB2, SMB, FUSE].contains(&magic) {
                return true;
            }
        }
    }
    // WSL1 drvfs reports its own type; fall back to the mount layout.
    is_wsl_drive_path(path)
}

#[cfg(not(target_os = "linux"))]
fn detect_remote_fs(_path: &Path) -> bool {
    false
}

/// `/mnt/<letter>` or below, on a WSL kernel.
#[cfg(target_os = "linux")]
fn is_wsl_drive_path(path: &Path) -> bool {
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut it = canon.components().skip(1);
    let on_drive = it.next().is_some_and(|c| c.as_os_str() == "mnt")
        && it.next().is_some_and(|c| {
            let s = c.as_os_str().as_encoded_bytes();
            s.len() == 1 && s[0].is_ascii_alphabetic()
        });
    on_drive
        && std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .is_ok_and(|r| r.to_ascii_lowercase().contains("microsoft"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_answer_is_cached_and_stable() {
        let dir = tempfile::tempdir().unwrap();
        let first = is_remote_fs(dir.path());
        assert_eq!(is_remote_fs(dir.path()), first);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_wsl_drive_is_remote() {
        let on_wsl = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .is_ok_and(|r| r.to_ascii_lowercase().contains("microsoft"));
        if on_wsl && Path::new("/mnt/c/Windows").is_dir() {
            assert!(is_remote_fs(Path::new("/mnt/c")));
            assert!(is_wsl_drive_path(Path::new("/mnt/c/Windows")));
        }
        assert!(!is_wsl_drive_path(Path::new("/tmp")));
        assert!(!is_wsl_drive_path(Path::new("/mnt")));
    }
}
