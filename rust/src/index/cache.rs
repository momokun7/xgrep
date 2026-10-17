//! Index sidecar: per-file trigram lists used only by diff updates.
//!
//! Since index format v4 this file replaces both the old trigram cache
//! (`index.cache` v2) and the per-file section that v3 kept at the end of the
//! memory-mapped main index (the same information was stored twice, and the
//! per-file section was 75 % of the mapped index). Searches never read it.
//!
//! A diff update needs, for a changed or deleted file, the trigrams whose
//! posting lists mention it. Entries are stored in the main index's file ID
//! order, and the header carries the `sidecar_id` of the main index it was
//! written with; a diff update only trusts a sidecar whose id and entry count
//! match the main index, and otherwise falls back to a full rebuild.
//!
//! Masks are deliberately *not* stored here: they live only in the posting
//! lists, and a diff update never needs the old masks of a file it edits.
//! (Consequently a full rebuild re-reads every file; it no longer reuses
//! cached trigrams, which would require duplicating the masks.)
//!
//! ```text
//! Header (20 bytes)
//!   magic: "XGCH", version: u32 = 3, sidecar_id: u64, count: u32
//! Entry (repeated `count` times, file ID order)
//!   gram_count: varint, body_len: varint
//!   body: the file's folded trigrams, sorted, as delta-encoded varints
//! ```

use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use memmap2::Mmap;

use crate::error::{Result, XgrepError};
use crate::index::format::{decode_varint, encode_varint};
use crate::trigram::Gram;

/// Magic bytes identifying the xgrep sidecar/cache format.
const CACHE_MAGIC: &[u8; 4] = b"XGCH";

/// Current sidecar format version.
///
/// v3: folded trigram lists in file-ID order with a sidecar id (index v4).
/// v2: path-keyed trigram cache with nanosecond stat (index v3); v1: second
/// resolution mtime. Older versions are ignored (a full rebuild follows).
const CACHE_VERSION: u32 = 3;

/// Header: magic(4) + version(4) + sidecar_id(8) + count(4).
const HEADER_LEN: usize = 20;

/// File metadata used to decide whether a file may have changed.
///
/// `ctime_ns` and `ino` are 0 on platforms where they are unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct FileStat {
    /// Modification time in nanoseconds since the UNIX epoch.
    pub mtime_ns: u64,
    pub size: u64,
    /// Inode change time in nanoseconds since the UNIX epoch (unix only).
    pub ctime_ns: u64,
    /// Inode number (unix only).
    pub ino: u64,
}

impl FileStat {
    pub fn from_metadata(meta: &fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let ns = |s: i64, n: i64| -> u64 {
                if s < 0 {
                    0
                } else {
                    (s as u64)
                        .saturating_mul(1_000_000_000)
                        .saturating_add(n.max(0) as u64)
                }
            };
            Self {
                mtime_ns: ns(meta.mtime(), meta.mtime_nsec()),
                size: meta.len(),
                ctime_ns: ns(meta.ctime(), meta.ctime_nsec()),
                ino: meta.ino(),
            }
        }
        #[cfg(not(unix))]
        {
            let mtime_ns = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos().min(u64::MAX as u128) as u64)
                .unwrap_or(0);
            Self {
                mtime_ns,
                size: meta.len(),
                ctime_ns: 0,
                ino: 0,
            }
        }
    }

    /// "Racily clean" check (same idea as git's racy-git handling).
    ///
    /// A file whose mtime falls in the same second as (or after) the moment a
    /// build started may have been modified again after it was read without its
    /// mtime/size changing (coarse filesystem timestamps). Such files must not be
    /// trusted on stat data alone and are re-verified by content hash.
    pub fn is_racy(&self, build_start_ns: u64) -> bool {
        self.mtime_ns / 1_000_000_000 >= build_start_ns / 1_000_000_000
    }
}

/// Parse the entry starting at `pos`: (gram_count, body range, end offset).
fn parse_entry(data: &[u8], pos: usize) -> Option<(usize, std::ops::Range<usize>)> {
    let (count, n1) = decode_varint(data.get(pos..)?);
    let (body_len, n2) = decode_varint(data.get(pos + n1..)?);
    if n1 == 0 || n2 == 0 {
        return None;
    }
    let start = pos + n1 + n2;
    let end = start.checked_add(body_len as usize)?;
    if end > data.len() {
        return None;
    }
    Some((count as usize, start..end))
}

/// Decode a body of `count` delta-encoded, strictly increasing trigrams.
fn decode_body(body: &[u8], count: usize) -> Option<Vec<u32>> {
    if count > body.len() {
        return None;
    }
    let mut out = Vec::with_capacity(count);
    let mut pos = 0usize;
    let mut prev = 0u32;
    for i in 0..count {
        if pos >= body.len() {
            return None;
        }
        let (d, n) = decode_varint(&body[pos..]);
        pos += n;
        if i > 0 && d == 0 {
            return None;
        }
        prev = prev.checked_add(d)?;
        if prev >= 1 << 24 {
            return None;
        }
        out.push(prev);
    }
    (pos == body.len()).then_some(out)
}

/// Streaming sidecar writer (temp file + fsync + atomic rename).
pub(crate) struct SidecarWriter {
    w: Option<BufWriter<fs::File>>,
    temp_path: PathBuf,
    path: PathBuf,
    remaining: usize,
}

impl SidecarWriter {
    pub fn create(path: &Path, sidecar_id: u64, count: usize) -> Result<Self> {
        let count_u32 = u32::try_from(count)
            .map_err(|_| XgrepError::IndexError("too many files for the sidecar".into()))?;
        let parent = path.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let temp_path = parent.join(format!(".xgrep_cache_tmp_{}", std::process::id()));
        let mut out = Self {
            w: Some(BufWriter::with_capacity(
                256 * 1024,
                fs::File::create(&temp_path)?,
            )),
            temp_path,
            path: path.to_path_buf(),
            remaining: count,
        };
        let w = out.w.as_mut().expect("just created");
        w.write_all(CACHE_MAGIC)?;
        w.write_all(&CACHE_VERSION.to_le_bytes())?;
        w.write_all(&sidecar_id.to_le_bytes())?;
        w.write_all(&count_u32.to_le_bytes())?;
        Ok(out)
    }

    fn writer(&mut self) -> Result<&mut BufWriter<fs::File>> {
        if self.remaining == 0 {
            return Err(XgrepError::IndexError(
                "internal error: too many sidecar entries".into(),
            ));
        }
        self.remaining -= 1;
        Ok(self.w.as_mut().expect("writer present until finish"))
    }

    /// Encode the entry of one file (`grams` sorted by trigram). Lets callers
    /// encode entries in parallel and append them with [`Self::write_raw`].
    pub fn encode_entry(grams: &[Gram]) -> Result<Vec<u8>> {
        let mut body = Vec::with_capacity(grams.len() * 2);
        let mut prev = 0u32;
        for g in grams {
            let k = g.key();
            encode_varint(&mut body, k - prev);
            prev = k;
        }
        let count = u32::try_from(grams.len())
            .map_err(|_| XgrepError::IndexError("sidecar entry too large".into()))?;
        let body_len = u32::try_from(body.len())
            .map_err(|_| XgrepError::IndexError("sidecar entry too large".into()))?;
        let mut out = Vec::with_capacity(body.len() + 10);
        encode_varint(&mut out, count);
        encode_varint(&mut out, body_len);
        out.extend_from_slice(&body);
        Ok(out)
    }

    /// Append the entry of one file. `grams` must be sorted by trigram.
    pub fn write_entry(&mut self, grams: &[Gram]) -> Result<()> {
        let entry = Self::encode_entry(grams)?;
        self.write_raw(&entry)
    }

    /// Append an entry copied verbatim from another sidecar.
    pub fn write_raw(&mut self, raw: &[u8]) -> Result<()> {
        self.writer()?.write_all(raw)?;
        Ok(())
    }

    /// Flush, fsync and move the file into place.
    pub fn finish(mut self) -> Result<()> {
        if self.remaining != 0 {
            return Err(XgrepError::IndexError(
                "internal error: sidecar entry count mismatch".into(),
            ));
        }
        let w = self.w.take().expect("writer present until finish");
        let file = w.into_inner().map_err(|e| XgrepError::Io(e.into_error()))?;
        file.sync_all()?;
        drop(file);
        if let Err(e) = fs::rename(&self.temp_path, &self.path) {
            let _ = fs::remove_file(&self.temp_path);
            return Err(e.into());
        }
        Ok(())
    }
}

impl Drop for SidecarWriter {
    fn drop(&mut self) {
        if self.w.take().is_some() {
            // Not finished (error path): remove the temp file.
            let _ = fs::remove_file(&self.temp_path);
        }
    }
}

/// Zero-copy view of a sidecar for diff updates (entries by file ID).
pub(crate) struct SidecarView {
    mmap: Mmap,
    /// Start offset of each entry, plus a final end offset.
    offsets: Vec<usize>,
    pub sidecar_id: u64,
}

impl SidecarView {
    /// Map and index a sidecar; `None` if missing, of another version or corrupt.
    pub fn open(path: &Path) -> Option<Self> {
        let file = fs::File::open(path).ok()?;
        // SAFETY: the sidecar is only ever replaced via rename (never
        // modified in place), and builds are serialized by the index lock.
        let mmap = unsafe { Mmap::map(&file).ok()? };
        if mmap.len() < HEADER_LEN || &mmap[0..4] != CACHE_MAGIC {
            return None;
        }
        if u32::from_le_bytes(mmap[4..8].try_into().ok()?) != CACHE_VERSION {
            return None;
        }
        let sidecar_id = u64::from_le_bytes(mmap[8..16].try_into().ok()?);
        let count = u32::from_le_bytes(mmap[16..20].try_into().ok()?) as usize;
        let mut offsets = Vec::with_capacity(count.min(1 << 24) + 1);
        let mut pos = HEADER_LEN;
        for _ in 0..count {
            offsets.push(pos);
            pos = parse_entry(&mmap, pos)?.1.end;
        }
        offsets.push(pos);
        Some(Self {
            mmap,
            offsets,
            sidecar_id,
        })
    }

    pub fn len(&self) -> usize {
        self.offsets.len() - 1
    }

    /// Raw bytes of entry `i` (for verbatim copying).
    pub fn raw(&self, i: usize) -> &[u8] {
        &self.mmap[self.offsets[i]..self.offsets[i + 1]]
    }

    /// Sorted folded trigrams (as u32) of entry `i`; `None` if corrupt.
    pub fn trigrams(&self, i: usize) -> Option<Vec<u32>> {
        let (count, body) = parse_entry(&self.mmap, self.offsets[i])?;
        decode_body(&self.mmap[body], count)
    }
}

/// Return the sidecar (`index.cache`) path for a given index path.
pub fn cache_path_for(index_path: &Path) -> PathBuf {
    index_path.with_extension("cache")
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn keys(grams: &[Gram]) -> Vec<u32> {
        grams.iter().map(|g| g.key()).collect()
    }

    #[test]
    fn test_old_format_is_rejected() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(&0u32.to_le_bytes()).unwrap();
        f.flush().unwrap();
        assert!(SidecarView::open(f.path()).is_none());
    }

    #[test]
    fn test_wrong_version_is_rejected() {
        for version in [1u32, 2, 99] {
            // v2 layout (index v3 era): magic, version, build_start, count, entries.
            let mut f = NamedTempFile::new().unwrap();
            f.write_all(b"XGCH").unwrap();
            f.write_all(&version.to_le_bytes()).unwrap();
            f.write_all(&0u64.to_le_bytes()).unwrap();
            f.write_all(&0u32.to_le_bytes()).unwrap();
            f.write_all(&[0u8; 16]).unwrap();
            f.flush().unwrap();
            assert!(SidecarView::open(f.path()).is_none(), "v{version}");
        }
    }

    #[test]
    fn test_missing_file() {
        assert!(SidecarView::open(Path::new("/nonexistent/path/test.cache")).is_none());
    }

    #[test]
    fn test_roundtrip_and_raw_copy() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.cache");
        let b = dir.path().join("b.cache");
        let g1 = crate::trigram::extract_grams(b"hello world");
        let g2 = crate::trigram::extract_grams(b"fn main() {}");
        let mut w = SidecarWriter::create(&a, 42, 3).unwrap();
        w.write_entry(&g1).unwrap();
        w.write_entry(&[]).unwrap();
        w.write_entry(&g2).unwrap();
        w.finish().unwrap();

        let data = fs::read(&a).unwrap();
        assert_eq!(&data[0..4], b"XGCH");
        assert_eq!(
            u32::from_le_bytes(data[4..8].try_into().unwrap()),
            CACHE_VERSION
        );

        let v = SidecarView::open(&a).unwrap();
        assert_eq!(v.sidecar_id, 42);
        assert_eq!(v.len(), 3);
        assert_eq!(v.trigrams(0).unwrap(), keys(&g1));
        assert_eq!(v.trigrams(1).unwrap(), Vec::<u32>::new());
        assert_eq!(v.trigrams(2).unwrap(), keys(&g2));

        // Raw copy round-trips.
        let mut w = SidecarWriter::create(&b, 43, 2).unwrap();
        w.write_raw(v.raw(2)).unwrap();
        w.write_raw(v.raw(0)).unwrap();
        w.finish().unwrap();
        let v2 = SidecarView::open(&b).unwrap();
        assert_eq!(v2.sidecar_id, 43);
        assert_eq!(v2.trigrams(0).unwrap(), keys(&g2));
        assert_eq!(v2.trigrams(1).unwrap(), keys(&g1));
    }

    #[test]
    fn test_truncated_sidecar_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.cache");
        let mut w = SidecarWriter::create(&a, 1, 1).unwrap();
        w.write_entry(&crate::trigram::extract_grams(b"some text here"))
            .unwrap();
        w.finish().unwrap();
        let data = fs::read(&a).unwrap();
        fs::write(&a, &data[..data.len() - 1]).unwrap();
        assert!(SidecarView::open(&a).is_none());
    }

    #[test]
    fn test_writer_count_mismatch_is_error_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.cache");
        let w = SidecarWriter::create(&p, 0, 1).unwrap();
        assert!(w.finish().is_err());
        assert!(!p.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);

        let mut w = SidecarWriter::create(&p, 0, 0).unwrap();
        assert!(w.write_entry(&[]).is_err());
        drop(w);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn test_is_racy() {
        let s = |mtime_ns| FileStat {
            mtime_ns,
            ..Default::default()
        };
        let start = 10_500_000_000; // 10.5 s
        assert!(!s(9_999_999_999).is_racy(start));
        assert!(s(10_000_000_000).is_racy(start)); // same second, before start
        assert!(s(10_900_000_000).is_racy(start));
        assert!(s(11_000_000_000).is_racy(start));
    }
}
