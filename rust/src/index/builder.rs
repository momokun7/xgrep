use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::{Result, XgrepError};
use ignore::WalkBuilder;
use rayon::prelude::*;
use xxhash_rust::xxh64::Xxh64;

use crate::index::cache::{CachedFile, TrigramCache};
use crate::index::format::*;
use crate::trigram;

type NewFileInfo = (String, u64, u64, u64, Vec<[u8; 3]>);

// ============================================================
// Corpus fingerprint: fast "nothing changed" detection
// ============================================================

fn fingerprint_path(index_path: &Path) -> PathBuf {
    let mut s = index_path.as_os_str().to_os_string();
    s.push(".fp");
    PathBuf::from(s)
}

fn read_fingerprint(fp_path: &Path) -> Option<u64> {
    let data = fs::read(fp_path).ok()?;
    Some(u64::from_le_bytes(data.get(..8)?.try_into().ok()?))
}

fn write_fingerprint(fp_path: &Path, fingerprint: u64) -> Result<()> {
    let mut tmp_s = fp_path.as_os_str().to_os_string();
    tmp_s.push(".tmp");
    let tmp = PathBuf::from(tmp_s);
    fs::write(&tmp, fingerprint.to_le_bytes())?;
    fs::rename(&tmp, fp_path)?;
    Ok(())
}

fn binary_cache_path(index_path: &Path) -> PathBuf {
    let mut s = index_path.as_os_str().to_os_string();
    s.push(".bincache");
    PathBuf::from(s)
}

/// 既知バイナリファイルのキャッシュをロード。
/// 形式: 各行 `{path}\t{mtime}\t{size}\n`
fn load_binary_cache(cache_path: &Path) -> HashMap<String, (u64, u64)> {
    let Ok(data) = fs::read_to_string(cache_path) else {
        return HashMap::new();
    };
    let mut map = HashMap::new();
    for line in data.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(path), Some(mtime_s), Some(size_s)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let (Ok(mtime), Ok(size)) = (mtime_s.parse::<u64>(), size_s.parse::<u64>()) else {
            continue;
        };
        map.insert(path.to_owned(), (mtime, size));
    }
    map
}

fn save_binary_cache(cache_path: &Path, cache: &HashMap<String, (u64, u64)>) {
    let mut data = String::with_capacity(cache.len() * 80);
    let mut entries: Vec<(&str, u64, u64)> = cache
        .iter()
        .map(|(p, &(m, s))| (p.as_str(), m, s))
        .collect();
    entries.sort_unstable_by_key(|e| e.0);
    for (path, mtime, size) in entries {
        data.push_str(path);
        data.push('\t');
        data.push_str(&mtime.to_string());
        data.push('\t');
        data.push_str(&size.to_string());
        data.push('\n');
    }
    let _ = fs::write(cache_path, data.as_bytes());
}

/// Walk `root` and compute a fingerprint of all file paths + mtimes + sizes.
///
/// The fingerprint captures the state of the entire file tree, including
/// binary files excluded from the index. Any change to the corpus (additions,
/// deletions, or modifications) produces a different fingerprint and triggers
/// a rebuild.
///
/// Limitation: mtime + size cannot detect content changes that deliberately
/// preserve both values (e.g., `touch -r oldfile newfile`, certain VCS
/// operations that manually restore timestamps). Such edits are vanishingly
/// rare in normal development and CI workflows. Use `xg init` after an
/// explicit `touch -r` if you need to force a rebuild.
/// Returns `true` for files and directories that are xgrep internal artefacts
/// and should never be included in the corpus walk.
fn is_xgrep_internal(name: &str) -> bool {
    name == ".xgrep"              // index directory
        || name.ends_with(".xgrep")   // index file (e.g. index.xgrep)
        || name.ends_with(".xgrep.fp")    // fingerprint file
        || name.ends_with(".xgrep.lock")  // build lock file
        || name.starts_with(".xgrep_tmp_") // atomic write tmp files
}

/// Walk corpus and collect (relative_path, mtime, size) for every non-binary-filtered file.
/// Returns (fingerprint, walk_data). The caller can reuse walk_data in Phase 1 to skip the
/// duplicate walk.
#[allow(clippy::type_complexity)]
fn collect_corpus_walk(root: &Path, lock_path: &Path) -> Option<(u64, Vec<(String, u64, u64)>)> {
    let mut entries: Vec<(String, u64, u64)> = Vec::new();
    for entry in WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(|e| !is_xgrep_internal(&e.file_name().to_string_lossy()))
        .build()
    {
        let entry = entry.ok()?;
        if entry.file_type().is_none_or(|ft| !ft.is_file()) {
            continue;
        }
        let path = entry.path();
        if path == lock_path {
            continue;
        }
        let relative = path.strip_prefix(root).ok()?.to_string_lossy().into_owned();
        let meta = entry.metadata().ok()?;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        entries.push((relative, mtime, meta.len()));
    }
    entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    let mut h = Xxh64::new(0);
    for (path, mtime, size) in &entries {
        h.update(path.as_bytes());
        h.update(&[0u8]); // NUL separator prevents "ab"+"c" == "a"+"bc"
        h.update(&mtime.to_le_bytes());
        h.update(&size.to_le_bytes());
    }
    Some((h.digest(), entries))
}

fn compute_corpus_fingerprint(root: &Path, lock_path: &Path) -> Option<u64> {
    collect_corpus_walk(root, lock_path).map(|(fp, _)| fp)
}

// ============================================================
// Lock Guard: advisory file lock to prevent concurrent builds
// ============================================================

struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn acquire_lock(index_path: &Path) -> Result<LockGuard> {
    acquire_lock_with_retry(index_path, 3)
}

fn acquire_lock_with_retry(index_path: &Path, retries: u32) -> Result<LockGuard> {
    if retries == 0 {
        return Err(XgrepError::LockError(format!(
            "failed to acquire lock after retries (lock: {})",
            index_path.with_extension("lock").display()
        )));
    }
    let lock_path = index_path.with_extension("lock");
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
    {
        Ok(mut f) => {
            let _ = write!(f, "{}", std::process::id());
            Ok(LockGuard { path: lock_path })
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let mut stale = false;

            // Unix: check if locking process is still alive
            #[cfg(unix)]
            if let Ok(pid_str) = fs::read_to_string(&lock_path) {
                if let Ok(pid) = pid_str.trim().parse::<u32>() {
                    // SAFETY: kill(pid, 0) sends no signal; it only checks process
                    // existence. An invalid PID returns -1 (ESRCH), handled by != 0.
                    if unsafe { libc::kill(pid as i32, 0) } != 0 {
                        stale = true;
                    }
                }
            }

            // All platforms: lock older than 5 minutes is considered stale
            if !stale {
                if let Ok(meta) = fs::metadata(&lock_path) {
                    if let Ok(modified) = meta.modified() {
                        if modified.elapsed().unwrap_or_default().as_secs() > 300 {
                            stale = true;
                        }
                    }
                }
            }

            if stale {
                let _ = fs::remove_file(&lock_path);
                return acquire_lock_with_retry(index_path, retries - 1);
            }

            Err(XgrepError::LockError(format!(
                "index build already in progress (lock: {})",
                lock_path.display()
            )))
        }
        Err(e) => Err(XgrepError::LockError(format!(
            "failed to create lock file: {}",
            e
        ))),
    }
}

#[allow(dead_code)]
pub fn build_index(root: &Path, index_path: &Path) -> Result<bool> {
    build_index_with_cache(root, index_path, None)
}

/// Build or update the search index.
///
/// Returns `true` if the index was (re)built, `false` if the corpus
/// fingerprint matched the stored value and no rebuild was needed.
pub fn build_index_with_cache(
    root: &Path,
    index_path: &Path,
    cache_path: Option<&Path>,
) -> Result<bool> {
    let fp_path = fingerprint_path(index_path);
    let lock_path = index_path.with_extension("lock");

    // 高速パス: フィンガープリントが一致すれば即返却 (ロック前 = 競合なし)
    // compute_corpus_fingerprint はウォーク 1 回 (~1s) で済み、
    // diff ウォーク (~6s: mmap 初期化 + per-file section スキャン) より速い。
    // pre-lock fp walk: walkデータを収集して diff path に流用 (Phase1 walkを省略)
    let mut pre_walk_data: Option<Vec<(String, u64, u64)>> = None;
    if index_path.exists() && fp_path.exists() {
        if let Some(stored) = read_fingerprint(&fp_path) {
            if let Some((current_fp, walk_data)) = collect_corpus_walk(root, &lock_path) {
                if current_fp == stored {
                    return Ok(false);
                }
                // fp不一致: walkデータをdiff pathに渡してPhase1 walkを省略
                pre_walk_data = Some(walk_data);
            }
        }
    }

    let _lock_guard = acquire_lock(index_path)?;

    // diff update を試みる (pre_walk_dataがあればPhase1 walkを省略)
    if index_path.exists() {
        if let Some(v) = try_build_index_diff(
            root,
            index_path,
            cache_path,
            &fp_path,
            &lock_path,
            pre_walk_data,
        )? {
            return Ok(v); // フォールバック不要: diff 更新成功
        }
    }

    let mut cache = cache_path
        .map(TrigramCache::load)
        .unwrap_or_else(TrigramCache::new);
    let mut cache_hits = 0usize;
    let mut cache_misses = 0usize;

    // ============================================================
    // Pass 1: collect file paths via a single-threaded directory walk.
    // Metadata (mtime/size) is fetched in parallel in Pass 2.
    // ============================================================
    let mut file_paths: Vec<PathBuf> = Vec::new();
    for entry in WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(|e| !is_xgrep_internal(&e.file_name().to_string_lossy()))
        .build()
    {
        let entry = entry.map_err(|e| XgrepError::IndexError(e.to_string()))?;
        if entry.file_type().is_none_or(|ft| !ft.is_file()) {
            continue;
        }
        let path = entry.path().to_path_buf();
        if path == lock_path {
            continue;
        }
        file_paths.push(path);
    }

    let mut files: Vec<FileInfo> = Vec::new();
    let mut file_trigrams: Vec<Vec<[u8; 3]>> = Vec::new();
    let mut trigram_count: HashMap<[u8; 3], u32> = HashMap::new();
    let mut total_pairs: usize = 0;

    const CHUNK_SIZE: usize = 1000;

    for chunk in file_paths.chunks(CHUNK_SIZE) {
        struct ChunkResult {
            relative_path: String,
            mtime: u64,
            size: u64,
            content_hash: u64,
            trigrams: Vec<[u8; 3]>,
            from_cache: bool,
        }

        let chunk_data: Vec<ChunkResult> = chunk
            .par_iter()
            .filter_map(|path| {
                let relative = path.strip_prefix(root).ok()?.to_string_lossy().to_string();
                let meta = fs::metadata(path).ok()?;
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let size = meta.len();

                // Cache hit: same relative path + same mtime means content unchanged.
                if let Some(cached) = cache.entries.get(&relative) {
                    if cached.mtime == mtime {
                        return Some(ChunkResult {
                            relative_path: relative,
                            mtime,
                            size,
                            content_hash: cached.content_hash,
                            trigrams: cached.trigrams.clone(),
                            from_cache: true,
                        });
                    }
                }

                // Cache miss: read file and extract trigrams
                let content = fs::read(path).ok()?;
                if memchr::memchr(0, &content).is_some() {
                    return None;
                }
                let hash = xxhash_rust::xxh64::xxh64(&content, 0);
                let trigrams = trigram::extract_trigrams(&content);
                Some(ChunkResult {
                    relative_path: relative,
                    mtime,
                    size,
                    content_hash: hash,
                    trigrams,
                    from_cache: false,
                })
            })
            .collect();

        for cr in chunk_data {
            if cr.from_cache {
                cache_hits += 1;
            } else {
                cache_misses += 1;
            }
            files.push(FileInfo {
                relative_path: cr.relative_path,
                mtime: cr.mtime,
                size: cr.size,
                content_hash: cr.content_hash,
            });
            for &t in &cr.trigrams {
                *trigram_count.entry(t).or_insert(0) += 1;
                total_pairs += 1;
            }
            file_trigrams.push(cr.trigrams);
        }
    }

    // ============================================================
    // Offset table computation (prefix sum)
    // ============================================================
    let mut sorted_trigrams: Vec<[u8; 3]> = trigram_count.keys().copied().collect();
    sorted_trigrams.sort();

    if files.len() > u32::MAX as usize {
        return Err(XgrepError::IndexError(format!(
            "too many files: {} (maximum {})",
            files.len(),
            u32::MAX
        )));
    }
    if sorted_trigrams.len() > u32::MAX as usize {
        return Err(XgrepError::IndexError(format!(
            "too many unique trigrams: {} (maximum {})",
            sorted_trigrams.len(),
            u32::MAX
        )));
    }

    if total_pairs == 0 {
        // No trigrams at all: write directly without mmap
        write_index_no_postings(index_path, &sorted_trigrams, &files, &file_trigrams)?;
        save_cache(&mut cache, &files, &file_trigrams, cache_path)?;
        if let Some(fp) = compute_corpus_fingerprint(root, &lock_path) {
            let _ = write_fingerprint(&fp_path, fp);
        }
        return Ok(true);
    }

    write_full_index_v3(index_path, &files, &file_trigrams)?;

    // Update and save cache
    save_cache(&mut cache, &files, &file_trigrams, cache_path)?;

    if cache_hits > 0 {
        eprintln!("[cache: {} hits, {} misses]", cache_hits, cache_misses);
    }

    // Persist corpus fingerprint so the next `xg init` can skip the rebuild
    // if nothing has changed. Computed here (after the build) to reflect the
    // actual file state that the index was built from, including binary files.
    if let Some(fp) = compute_corpus_fingerprint(root, &lock_path) {
        let _ = write_fingerprint(&fp_path, fp);
    }

    Ok(true)
}

/// Write v3 index from pre-built postings map (differential update path).
/// Skips the 2-pass temp-mmap sorting phase used by write_full_index_v3.
#[allow(dead_code)]
fn write_index_from_postings_v3(
    index_path: &Path,
    files: &[FileInfo],
    per_file_trigrams: &[Vec<[u8; 3]>],
    postings: &HashMap<[u8; 3], Vec<u32>>,
) -> Result<()> {
    let mut sorted_trigrams: Vec<[u8; 3]> = postings.keys().copied().collect();
    sorted_trigrams.sort();

    let parent = index_path.parent().unwrap_or(std::path::Path::new("."));
    fs::create_dir_all(parent)?;
    let temp_index_path = parent.join(format!(".xgrep_tmp_{}", std::process::id()));
    let out_file = fs::File::create(&temp_index_path)?;
    let mut writer = BufWriter::with_capacity(256 * 1024, out_file);

    let mut header = Header {
        magic: MAGIC,
        version: VERSION,
        trigram_count: sorted_trigrams.len() as u32,
        file_count: files.len() as u32,
        posting_total_bytes: 0,
        per_file_section_offset: 0,
    };
    writer.write_all(&header.to_bytes())?;
    writer.write_all(&vec![0u8; sorted_trigrams.len() * TrigramEntry::SIZE])?;

    let mut trigram_entries: Vec<TrigramEntry> = Vec::with_capacity(sorted_trigrams.len());
    let mut posting_buf: Vec<u8> = Vec::with_capacity(4096);
    let mut current_posting_offset: u64 = 0;

    for &t in &sorted_trigrams {
        let file_ids = &postings[&t];
        posting_buf.clear();
        encode_varint(&mut posting_buf, file_ids.len() as u32);
        let mut prev: u32 = 0;
        for &fid in file_ids {
            encode_varint(&mut posting_buf, fid - prev);
            prev = fid;
        }
        let len = posting_buf.len() as u32;
        writer.write_all(&posting_buf)?;
        trigram_entries.push(TrigramEntry {
            trigram: t,
            _padding: 0,
            posting_offset: current_posting_offset,
            posting_len: len,
        });
        current_posting_offset += len as u64;
    }

    let mut string_pool = Vec::new();
    for fi in files {
        let path_offset = string_pool.len() as u32;
        string_pool.extend_from_slice(fi.relative_path.as_bytes());
        string_pool.push(0);
        writer.write_all(
            &FileEntry {
                path_offset,
                mtime: fi.mtime,
                size: fi.size,
                content_hash: fi.content_hash,
            }
            .to_bytes(),
        )?;
    }
    writer.write_all(&string_pool)?;

    header.posting_total_bytes = current_posting_offset;
    writer.flush()?;
    let mut file = writer
        .into_inner()
        .map_err(|e| XgrepError::Io(e.into_error()))?;

    let per_file_section_offset = file.seek(SeekFrom::End(0))?;
    {
        let mut pf_writer = BufWriter::with_capacity(256 * 1024, &mut file);
        pf_writer.write_all(&(files.len() as u32).to_le_bytes())?;
        for (i, fi) in files.iter().enumerate() {
            pf_writer.write_all(&fi.mtime.to_le_bytes())?;
            pf_writer.write_all(&fi.content_hash.to_le_bytes())?;
            let sorted_trig = if i < per_file_trigrams.len() {
                trigrams_to_sorted_u32(&per_file_trigrams[i])
            } else {
                vec![]
            };
            pf_writer.write_all(&(sorted_trig.len() as u32).to_le_bytes())?;
            for &t_u32 in &sorted_trig {
                pf_writer.write_all(&t_u32.to_le_bytes())?;
            }
        }
        pf_writer.flush()?;
    }

    header.per_file_section_offset = per_file_section_offset;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&header.to_bytes())?;
    file.seek(SeekFrom::Start(Header::SIZE as u64))?;
    let mut trig_writer = BufWriter::with_capacity(64 * 1024, file);
    for entry in &trigram_entries {
        trig_writer.write_all(&entry.to_bytes())?;
    }
    trig_writer.flush()?;
    drop(trig_writer);

    fs::rename(&temp_index_path, index_path)?;
    Ok(())
}

/// Core index write function (v3 format with per-file section).
fn write_full_index_v3(
    index_path: &Path,
    files: &[FileInfo],
    file_trigrams: &[Vec<[u8; 3]>],
) -> Result<()> {
    let mut trigram_count: HashMap<[u8; 3], u32> = HashMap::new();
    let mut total_pairs: usize = 0;
    for trigrams in file_trigrams {
        for &t in trigrams {
            *trigram_count.entry(t).or_insert(0) += 1;
            total_pairs += 1;
        }
    }

    let mut sorted_trigrams: Vec<[u8; 3]> = trigram_count.keys().copied().collect();
    sorted_trigrams.sort();

    let mut offset_table: Vec<u32> = Vec::with_capacity(sorted_trigrams.len());
    let mut cumulative: u32 = 0;
    for t in &sorted_trigrams {
        offset_table.push(cumulative);
        cumulative += trigram_count[t];
    }

    let mut trigram_to_index: HashMap<[u8; 3], usize> = HashMap::new();
    for (i, t) in sorted_trigrams.iter().enumerate() {
        trigram_to_index.insert(*t, i);
    }

    let mut write_positions: Vec<u32> = offset_table.clone();

    let temp_dir = tempfile::tempdir()?;
    let temp_path = temp_dir.path().join("postings.tmp");
    {
        let f = fs::File::create(&temp_path)?;
        let temp_size = total_pairs.checked_mul(4).ok_or_else(|| {
            XgrepError::IndexError("index too large: total_pairs overflow".to_string())
        })?;
        f.set_len(temp_size as u64)?;
    }

    let temp_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&temp_path)?;
    // SAFETY: temp_file was just created with a unique name and opened
    // with read+write. No other process shares this file.
    let mut temp_mmap = unsafe { memmap2::MmapMut::map_mut(&temp_file)? };

    for (file_id, trigrams) in file_trigrams.iter().enumerate() {
        let file_id = file_id as u32;
        for t in trigrams {
            if let Some(&idx) = trigram_to_index.get(t) {
                let pos = write_positions[idx] as usize;
                write_positions[idx] += 1;
                let byte_offset = pos * 4;
                if byte_offset + 4 <= temp_mmap.len() {
                    temp_mmap[byte_offset..byte_offset + 4].copy_from_slice(&file_id.to_le_bytes());
                }
            }
        }
    }

    temp_mmap.flush()?;

    let parent = index_path.parent().unwrap_or(std::path::Path::new("."));
    fs::create_dir_all(parent)?;
    let temp_index_path = parent.join(format!(".xgrep_tmp_{}", std::process::id()));
    let out_file = fs::File::create(&temp_index_path)?;
    let mut writer = BufWriter::with_capacity(256 * 1024, out_file);

    // Write placeholder header
    let mut header = Header {
        magic: MAGIC,
        version: VERSION,
        trigram_count: sorted_trigrams.len() as u32,
        file_count: files.len() as u32,
        posting_total_bytes: 0,
        per_file_section_offset: 0,
    };
    writer.write_all(&header.to_bytes())?;

    let trigram_table_size = sorted_trigrams.len() * TrigramEntry::SIZE;
    writer.write_all(&vec![0u8; trigram_table_size])?;

    let mut trigram_entries: Vec<TrigramEntry> = Vec::with_capacity(sorted_trigrams.len());
    let mut posting_buf: Vec<u8> = Vec::with_capacity(4096);
    let mut current_posting_offset: u64 = 0;

    for (i, t) in sorted_trigrams.iter().enumerate() {
        let start = offset_table[i] as usize;
        let count = trigram_count[t] as usize;

        let mut file_ids: Vec<u32> = Vec::with_capacity(count);
        for j in 0..count {
            let pos = (start + j) * 4;
            let fid = u32::from_le_bytes([
                temp_mmap[pos],
                temp_mmap[pos + 1],
                temp_mmap[pos + 2],
                temp_mmap[pos + 3],
            ]);
            file_ids.push(fid);
        }
        file_ids.sort_unstable();

        posting_buf.clear();
        encode_varint(&mut posting_buf, file_ids.len() as u32);
        let mut prev: u32 = 0;
        for &fid in &file_ids {
            encode_varint(&mut posting_buf, fid - prev);
            prev = fid;
        }

        let offset = current_posting_offset;
        if posting_buf.len() > u32::MAX as usize {
            return Err(XgrepError::IndexError(
                "posting list too large for index format (> 4GB)".to_string(),
            ));
        }
        let len = posting_buf.len() as u32;
        writer.write_all(&posting_buf)?;
        current_posting_offset += len as u64;

        trigram_entries.push(TrigramEntry {
            trigram: *t,
            _padding: 0,
            posting_offset: offset,
            posting_len: len,
        });
    }

    // Release mmap and temporary file
    drop(temp_mmap);
    drop(temp_file);
    drop(temp_dir);

    // Write File Table
    let mut string_pool = Vec::new();
    for fi in files {
        let path_offset = string_pool.len() as u32;
        string_pool.extend_from_slice(fi.relative_path.as_bytes());
        string_pool.push(0);
        let entry = FileEntry {
            path_offset,
            mtime: fi.mtime,
            size: fi.size,
            content_hash: fi.content_hash,
        };
        writer.write_all(&entry.to_bytes())?;
    }

    // Write String Pool
    writer.write_all(&string_pool)?;

    // Flush and get file handle to write per-file section
    writer.flush()?;
    let mut file = writer
        .into_inner()
        .map_err(|e| XgrepError::Io(e.into_error()))?;

    // Get current position as per_file_section_offset
    let per_file_section_offset = file.seek(SeekFrom::End(0))?;

    // Write Per-File Section. Wrap in BufWriter to avoid per-4-byte syscalls
    // (the inner loop writes one u32 at a time; without buffering this causes
    // ~173M write() syscalls on a 137K-file corpus).
    {
        let mut pf_writer = BufWriter::with_capacity(256 * 1024, &mut file);
        pf_writer.write_all(&(files.len() as u32).to_le_bytes())?;
        for (i, fi) in files.iter().enumerate() {
            pf_writer.write_all(&fi.mtime.to_le_bytes())?;
            pf_writer.write_all(&fi.content_hash.to_le_bytes())?;
            let sorted_trigrams_u32 = if i < file_trigrams.len() {
                trigrams_to_sorted_u32(&file_trigrams[i])
            } else {
                vec![]
            };
            pf_writer.write_all(&(sorted_trigrams_u32.len() as u32).to_le_bytes())?;
            for &t_u32 in &sorted_trigrams_u32 {
                pf_writer.write_all(&t_u32.to_le_bytes())?;
            }
        }
        pf_writer.flush()?;
    }

    // Update header with final values
    header.posting_total_bytes = current_posting_offset;
    header.per_file_section_offset = per_file_section_offset;

    // Seek back to beginning and write header
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&header.to_bytes())?;

    // Seek and overwrite Trigram Table
    file.seek(SeekFrom::Start(Header::SIZE as u64))?;
    let mut trig_writer = BufWriter::with_capacity(64 * 1024, file);
    for entry in &trigram_entries {
        trig_writer.write_all(&entry.to_bytes())?;
    }
    trig_writer.flush()?;
    drop(trig_writer);

    // Atomic replacement: rename temp file to final path
    fs::rename(&temp_index_path, index_path)?;

    Ok(())
}

/// Update and save the cache.
fn save_cache(
    cache: &mut TrigramCache,
    files: &[FileInfo],
    file_trigrams: &[Vec<[u8; 3]>],
    cache_path: Option<&Path>,
) -> Result<()> {
    if let Some(cp) = cache_path {
        // Update cache with current file list (excluding deleted files)
        let mut new_entries = HashMap::with_capacity(files.len());
        for (i, fi) in files.iter().enumerate() {
            new_entries.insert(
                fi.relative_path.clone(),
                CachedFile {
                    mtime: fi.mtime,
                    content_hash: fi.content_hash,
                    trigrams: file_trigrams[i].clone(),
                },
            );
        }
        cache.entries = new_entries;
        cache.save(cp)?;
    }
    Ok(())
}

/// Write index when there are no trigrams (only empty files or files shorter than 3 bytes).
fn write_index_no_postings(
    index_path: &Path,
    sorted_trigrams: &[[u8; 3]],
    files: &[FileInfo],
    file_trigrams: &[Vec<[u8; 3]>],
) -> Result<()> {
    let parent = index_path.parent().unwrap_or(std::path::Path::new("."));
    fs::create_dir_all(parent)?;
    let temp_path = parent.join(format!(".xgrep_tmp_{}", std::process::id()));
    let out_file = fs::File::create(&temp_path)?;
    let mut writer = BufWriter::with_capacity(256 * 1024, out_file);

    let mut header = Header {
        magic: MAGIC,
        version: VERSION,
        trigram_count: sorted_trigrams.len() as u32,
        file_count: files.len() as u32,
        posting_total_bytes: 0,
        per_file_section_offset: 0,
    };
    writer.write_all(&header.to_bytes())?;

    let trigram_table_size = sorted_trigrams.len() * TrigramEntry::SIZE;
    writer.write_all(&vec![0u8; trigram_table_size])?;

    // Write File Table
    let mut string_pool = Vec::new();
    for fi in files {
        let path_offset = string_pool.len() as u32;
        string_pool.extend_from_slice(fi.relative_path.as_bytes());
        string_pool.push(0);
        let entry = FileEntry {
            path_offset,
            mtime: fi.mtime,
            size: fi.size,
            content_hash: fi.content_hash,
        };
        writer.write_all(&entry.to_bytes())?;
    }

    writer.write_all(&string_pool)?;
    writer.flush()?;
    let mut file = writer
        .into_inner()
        .map_err(|e| XgrepError::Io(e.into_error()))?;

    // Get current position as per_file_section_offset
    let per_file_section_offset = file.seek(SeekFrom::End(0))?;

    // Write Per-File Section with BufWriter (same buffering reason as main build path)
    {
        let mut pf_writer = BufWriter::with_capacity(256 * 1024, &mut file);
        pf_writer.write_all(&(files.len() as u32).to_le_bytes())?;
        for (i, fi) in files.iter().enumerate() {
            pf_writer.write_all(&fi.mtime.to_le_bytes())?;
            pf_writer.write_all(&fi.content_hash.to_le_bytes())?;
            let sorted_trigrams_u32 = if i < file_trigrams.len() {
                trigrams_to_sorted_u32(&file_trigrams[i])
            } else {
                vec![]
            };
            pf_writer.write_all(&(sorted_trigrams_u32.len() as u32).to_le_bytes())?;
            for &t_u32 in &sorted_trigrams_u32 {
                pf_writer.write_all(&t_u32.to_le_bytes())?;
            }
        }
        pf_writer.flush()?;
    }

    header.per_file_section_offset = per_file_section_offset;

    // Seek back to beginning and write header
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&header.to_bytes())?;
    file.flush()?;
    drop(file);

    // Atomic replacement: rename temp file to final path
    fs::rename(&temp_path, index_path)?;

    Ok(())
}

/// Compute byte offsets for each per-file entry in the raw per-file section.
/// Returns `offsets` of length `file_count + 1`; entry `i` occupies
/// `pf_raw[offsets[i]..offsets[i+1]]`.
fn compute_pf_offsets(pf_raw: &[u8]) -> Option<Vec<usize>> {
    if pf_raw.len() < 4 {
        return None;
    }
    let file_count = u32::from_le_bytes(pf_raw[..4].try_into().ok()?) as usize;
    let mut offsets = Vec::with_capacity(file_count + 1);
    let mut pos = 4usize;
    for _ in 0..file_count {
        offsets.push(pos);
        if pos + 20 > pf_raw.len() {
            return None;
        }
        // entry: [mtime:8][content_hash:8][trigram_count:4][trigrams:4*tc]
        let tc = u32::from_le_bytes(pf_raw[pos + 16..pos + 20].try_into().ok()?) as usize;
        pos += 20 + tc * 4;
        if pos > pf_raw.len() {
            return None;
        }
    }
    offsets.push(pos); // sentinel
    Some(offsets)
}

/// Read sorted u32 trigrams from one per-file entry starting at `start`.
fn read_pf_trigrams(pf_raw: &[u8], start: usize) -> Vec<u32> {
    if start + 20 > pf_raw.len() {
        return vec![];
    }
    let tc =
        u32::from_le_bytes(pf_raw[start + 16..start + 20].try_into().unwrap_or([0; 4])) as usize;
    let trigs_start = start + 20;
    let trigs_end = trigs_start + tc * 4;
    if trigs_end > pf_raw.len() {
        return vec![];
    }
    (0..tc)
        .map(|i| {
            let b = trigs_start + i * 4;
            u32::from_le_bytes([pf_raw[b], pf_raw[b + 1], pf_raw[b + 2], pf_raw[b + 3]])
        })
        .collect()
}

/// Per-file section entry for the new index.
enum PerFileSection<'a> {
    /// Unchanged file: copy raw bytes (mtime + content_hash + trigrams) from old mmap.
    Raw(&'a [u8]),
    /// Changed or new file: write mtime/content_hash from FileInfo + these sorted trigrams.
    New(Vec<u32>),
}

/// Try to perform a diff-based index update.
/// Returns Some(true) on success, None if fallback to full rebuild is needed.
fn try_build_index_diff(
    root: &Path,
    index_path: &Path,
    cache_path: Option<&Path>,
    fp_path: &Path,
    lock_path: &Path,
    pre_walk_data: Option<Vec<(String, u64, u64)>>, // (rel_path, mtime, size) from pre-lock walk
) -> Result<Option<bool>> {
    use crate::index::format::u32_to_trigram;
    use crate::index::reader::IndexReader;

    let reader = match IndexReader::open(index_path) {
        Ok(r) => r,
        Err(_) => return Ok(None),
    };

    // per_file section raw bytes (no allocation, no parse)
    let pf_raw = reader.per_file_section_raw();
    if pf_raw.is_empty() {
        return Ok(None);
    }
    let pf_offsets = match compute_pf_offsets(pf_raw) {
        Some(o) => o,
        None => return Ok(None),
    };

    let old_file_count = reader.file_count() as usize;

    // 既知バイナリファイルのキャッシュをロード (phase2 で不要な peek を回避)
    let bc_path = binary_cache_path(index_path);
    let mut binary_cache = load_binary_cache(&bc_path);
    let binary_cache_original_len = binary_cache.len();

    // path → file_id / mtime のマップを構築 (regular file table = fixed-size, fast)
    let mut path_to_file_id: HashMap<String, u32> = HashMap::new();
    let mut path_to_mtime: HashMap<String, u64> = HashMap::new();
    for fid in 0..old_file_count as u32 {
        let path = reader.file_path(fid).to_string();
        path_to_file_id.insert(path.clone(), fid);
        if let Some(fe) = reader.file_entry(fid) {
            path_to_mtime.insert(path, fe.mtime);
        }
    }

    // Phase 1: mtime のみ確認 (ファイル内容は読まない)
    // pre_walk_data がある場合 (pre-lock fp walk の結果を再利用) → Phase1 walk を省略
    let mut changed_candidates: Vec<(String, PathBuf, u64)> = Vec::new(); // (rel, abs, mtime)
    let mut new_candidates: Vec<(String, PathBuf, u64, u64)> = Vec::new(); // (rel, abs, mtime, size)
    let mut seen_paths: HashSet<String> = HashSet::new();
    let mut fp_entries: Vec<(String, u64, u64)>;

    if let Some(walk_data) = pre_walk_data {
        // pre-lock walkデータを流用: Phase1 walkを省略 (~1s節約)
        fp_entries = walk_data;
        for (relative, mtime, size) in &fp_entries {
            seen_paths.insert(relative.clone());
            if path_to_file_id.contains_key(relative) {
                let old_mtime = path_to_mtime.get(relative).copied().unwrap_or(0);
                if *mtime != old_mtime {
                    let abs_path = root.join(relative);
                    changed_candidates.push((relative.clone(), abs_path, *mtime));
                }
            } else {
                // 既知バイナリかつ mtime+size 一致 → phase2 peek をスキップ
                if binary_cache
                    .get(relative)
                    .is_some_and(|&(m, s)| m == *mtime && s == *size)
                {
                    // skip
                } else {
                    let abs_path = root.join(relative);
                    new_candidates.push((relative.clone(), abs_path, *mtime, *size));
                }
            }
        }
    } else {
        fp_entries = Vec::with_capacity(old_file_count + 256);
        for entry in WalkBuilder::new(root)
            .hidden(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .filter_entry(|e| !is_xgrep_internal(&e.file_name().to_string_lossy()))
            .build()
        {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if entry.file_type().is_none_or(|ft| !ft.is_file()) {
                continue;
            }
            let path = entry.path();
            if path == lock_path {
                continue;
            }
            let relative = match path.strip_prefix(root) {
                Ok(r) => r.to_string_lossy().to_string(),
                Err(_) => continue,
            };
            seen_paths.insert(relative.clone());

            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let size = meta.len();
            fp_entries.push((relative.clone(), mtime, size));

            if path_to_file_id.contains_key(&relative) {
                let old_mtime = path_to_mtime.get(&relative).copied().unwrap_or(0);
                if mtime != old_mtime {
                    changed_candidates.push((relative, path.to_path_buf(), mtime));
                }
            } else {
                // 既知バイナリかつ mtime+size 一致 → phase2 peek をスキップ
                if binary_cache
                    .get(&relative)
                    .is_some_and(|&(m, s)| m == mtime && s == size)
                {
                    // skip
                } else {
                    new_candidates.push((relative, path.to_path_buf(), mtime, size));
                }
            }
        }
    }

    let deleted_paths: Vec<String> = path_to_file_id
        .keys()
        .filter(|p| !seen_paths.contains(*p))
        .cloned()
        .collect();

    // フォールバック判定: コンテンツ読み込み前に閾値チェック
    // (大量変更の場合ファイル内容を一切読まずに即座に返却)
    let total_candidates = changed_candidates.len() + new_candidates.len() + deleted_paths.len();
    if total_candidates * 2 > old_file_count.max(1) {
        return Ok(None);
    }

    // Phase 2: 変更ファイルのコンテンツ読み込みとトライグラム抽出
    let mut changed_files: Vec<(String, Vec<[u8; 3]>)> = Vec::new();
    let mut new_files: Vec<NewFileInfo> = Vec::new();

    for (relative, abs_path, _mtime) in changed_candidates {
        let content = match fs::read(&abs_path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if memchr::memchr(0, &content).is_some() {
            // テキスト→バイナリ: 旧 trigrams を削除するため空リストで登録
            changed_files.push((relative, vec![]));
            continue;
        }
        changed_files.push((relative, trigram::extract_trigrams(&content)));
    }

    for (relative, abs_path, mtime, size) in new_candidates {
        // 8KB peek でバイナリ早期検出: バイナリファイル(数MB)の全件読み込みを排除
        use std::io::Read;
        let mut file = match fs::File::open(&abs_path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let mut peek = [0u8; 8192];
        let n = match file.read(&mut peek) {
            Ok(n) => n,
            Err(_) => continue,
        };
        if memchr::memchr(0, &peek[..n]).is_some() {
            binary_cache.insert(relative, (mtime, size));
            continue; // バイナリ: cache に記録して捨てる
        }
        let mut content = peek[..n].to_vec();
        if file.read_to_end(&mut content).is_err() {
            continue;
        }
        let hash = xxhash_rust::xxh64::xxh64(&content, 0);
        new_files.push((
            relative,
            mtime,
            size,
            hash,
            trigram::extract_trigrams(&content),
        ));
    }

    // phase2 で新たにバイナリと判定されたファイルを保存 (次回以降の phase2 をスキップ)
    if binary_cache.len() != binary_cache_original_len {
        save_binary_cache(&bc_path, &binary_cache);
    }

    // インデックス関連の変更がなければフィンガープリントのみ更新して終了
    // (バイナリファイルの変更などでフィンガープリントは変わるがインデックスは変わらない場合)
    if changed_files.is_empty() && new_files.is_empty() && deleted_paths.is_empty() {
        fp_entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let mut h = Xxh64::new(0);
        for (path, mtime, size) in &fp_entries {
            h.update(path.as_bytes());
            h.update(&[0u8]);
            h.update(&mtime.to_le_bytes());
            h.update(&size.to_le_bytes());
        }
        let _ = write_fingerprint(fp_path, h.digest());
        return Ok(Some(false));
    }

    // 50% を超えたらフルリビルドにフォールバック。
    // これは性能上の最適点ではなく安全側の保守値: diff 更新の追加コスト
    // (旧 posting list の decode) と全件再構築コストの交差点は CPU のみなら
    // ~91% (実測で検証予定)。50% はその手前 41pt のマージン。書込コストは
    // 両パスとも v3 index フル書き直し (~650MB) で同じ。
    let total_changes = changed_files.len() + new_files.len() + deleted_paths.len();
    if total_changes * 2 > old_file_count.max(1) {
        return Ok(None);
    }

    // ID 削除・remap の構築
    let deleted_ids: HashSet<u32> = deleted_paths
        .iter()
        .filter_map(|p| path_to_file_id.get(p).copied())
        .collect();
    let mut old_to_new: HashMap<u32, u32> = HashMap::new();
    let mut new_id = 0u32;
    for old_id in 0..old_file_count as u32 {
        if !deleted_ids.contains(&old_id) {
            old_to_new.insert(old_id, new_id);
            new_id += 1;
        }
    }
    let base_new_id = new_id;

    // affected_set: 変更が必要な trigrams のみ
    // unaffected (affected_set 外) の posting list は old mmap から raw byte コピーする
    let mut affected_set: HashSet<[u8; 3]> = HashSet::new();

    // 変更ファイルの旧 + 新 trigrams
    for (path, new_trigs) in &changed_files {
        if let Some(&fid) = path_to_file_id.get(path) {
            for &t_u32 in &read_pf_trigrams(pf_raw, pf_offsets[fid as usize]) {
                affected_set.insert(u32_to_trigram(t_u32));
            }
        }
        for &t in new_trigs {
            affected_set.insert(t);
        }
    }
    // 削除ファイルの trigrams
    for path in &deleted_paths {
        if let Some(&fid) = path_to_file_id.get(path) {
            for &t_u32 in &read_pf_trigrams(pf_raw, pf_offsets[fid as usize]) {
                affected_set.insert(u32_to_trigram(t_u32));
            }
        }
    }
    // 新規ファイルの trigrams
    for (_, _, _, _, trigs) in &new_files {
        for &t in trigs {
            affected_set.insert(t);
        }
    }
    // 削除がある場合: min_deleted_id より大きい file_id を持つファイルの trigrams も
    // ID remap が必要なため affected_set に追加
    if !deleted_ids.is_empty() {
        let min_deleted = deleted_ids.iter().min().copied().unwrap_or(u32::MAX);
        for fid in (min_deleted + 1)..old_file_count as u32 {
            for &t_u32 in &read_pf_trigrams(pf_raw, pf_offsets[fid as usize]) {
                affected_set.insert(u32_to_trigram(t_u32));
            }
        }
    }

    // affected_set の posting list のみデコード（最重要最適化）
    let mut affected_postings: HashMap<[u8; 3], Vec<u32>> =
        HashMap::with_capacity(affected_set.len());
    for &t in &affected_set {
        let ids = reader.lookup_trigram(t);
        affected_postings.insert(t, ids);
    }

    // 削除 + remap: 削除がない場合は skip (デコード済みリストはソート済みのまま)
    // 削除がある場合のみ retain + remap + sort が必要
    if !deleted_ids.is_empty() {
        for ids in affected_postings.values_mut() {
            ids.retain(|id| !deleted_ids.contains(id));
            for id in ids.iter_mut() {
                if let Some(&mapped) = old_to_new.get(id) {
                    *id = mapped;
                }
            }
            ids.sort_unstable();
            ids.dedup();
        }
    }

    // 変更ファイルの旧 trigrams 除去:
    // retain の代わりに binary_search + remove でソート済み状態を維持
    let mut changed_file_new_trigrams: HashMap<String, Vec<[u8; 3]>> = HashMap::new();
    for (path, new_trigrams) in &changed_files {
        if let Some(&old_fid) = path_to_file_id.get(path) {
            if let Some(&new_fid) = old_to_new.get(&old_fid) {
                for &t_u32 in &read_pf_trigrams(pf_raw, pf_offsets[old_fid as usize]) {
                    let t = u32_to_trigram(t_u32);
                    if let Some(ids) = affected_postings.get_mut(&t) {
                        if let Ok(pos) = ids.binary_search(&new_fid) {
                            ids.remove(pos);
                        }
                    }
                }
                changed_file_new_trigrams.insert(path.clone(), new_trigrams.clone());
            }
        }
    }

    // 変更ファイルの新 trigrams 追加:
    // push の代わりに partition_point + insert でソート済み状態を維持
    for (path, new_trigrams) in &changed_file_new_trigrams {
        if let Some(&old_fid) = path_to_file_id.get(path) {
            if let Some(&new_fid) = old_to_new.get(&old_fid) {
                for &t in new_trigrams {
                    let ids = affected_postings.entry(t).or_default();
                    let pos = ids.partition_point(|&x| x < new_fid);
                    if pos >= ids.len() || ids[pos] != new_fid {
                        ids.insert(pos, new_fid);
                    }
                }
            }
        }
    }

    // 新規ファイルの trigrams 追加 (sorted insert)
    let mut new_file_infos: Vec<NewFileInfo> = Vec::new();
    for (i, (path, mtime, size, content_hash, trigs)) in new_files.into_iter().enumerate() {
        let assigned_id = base_new_id + i as u32;
        for &t in &trigs {
            let ids = affected_postings.entry(t).or_default();
            let pos = ids.partition_point(|&x| x < assigned_id);
            if pos >= ids.len() || ids[pos] != assigned_id {
                ids.insert(pos, assigned_id);
            }
        }
        new_file_infos.push((path, mtime, size, content_hash, trigs));
    }

    // sorted insert を維持しているため sort_unstable は不要。空リストのみ除去
    affected_postings.retain(|_, ids| !ids.is_empty());

    // 新 file リストと per-file section data を構築
    let mut remapped: Vec<(u32, u32)> = old_to_new.iter().map(|(&o, &n)| (n, o)).collect();
    remapped.sort_by_key(|&(n, _)| n);

    let mut new_files_list: Vec<FileInfo> = Vec::new();
    // PerFileSection::Raw は pf_raw への参照 (no allocation, no sort)
    // PerFileSection::New は変更/新規ファイルのみ Vec<u32> を持つ
    let mut pf_section: Vec<PerFileSection<'_>> = Vec::new();

    for (_, old_id) in &remapped {
        let path = reader.file_path(*old_id).to_string();
        let fe = reader.file_entry(*old_id);
        let (mtime, size, content_hash) = match fe {
            Some(fe) => (fe.mtime, fe.size, fe.content_hash),
            None => (0, 0, 0),
        };

        if let Some(new_trigs) = changed_file_new_trigrams.get(&path) {
            let abs_path = root.join(&path);
            let new_mtime = fs::metadata(&abs_path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(mtime);
            let new_hash = fs::read(&abs_path)
                .map(|c| xxhash_rust::xxh64::xxh64(&c, 0))
                .unwrap_or(content_hash);
            new_files_list.push(FileInfo {
                relative_path: path,
                mtime: new_mtime,
                size,
                content_hash: new_hash,
            });
            pf_section.push(PerFileSection::New(trigrams_to_sorted_u32(new_trigs)));
        } else {
            // 不変ファイル: raw bytes を per_file section からそのままコピー
            // Vec<u32> アロケーションも sort も不要
            let raw_start = pf_offsets[*old_id as usize];
            let raw_end = pf_offsets[*old_id as usize + 1];
            new_files_list.push(FileInfo {
                relative_path: path,
                mtime,
                size,
                content_hash,
            });
            pf_section.push(PerFileSection::Raw(&pf_raw[raw_start..raw_end]));
        }
    }

    // 新規ファイルを末尾に追加
    for (path, mtime, size, content_hash, trigs) in new_file_infos {
        new_files_list.push(FileInfo {
            relative_path: path,
            mtime,
            size,
            content_hash,
        });
        pf_section.push(PerFileSection::New(trigrams_to_sorted_u32(&trigs)));
    }

    // smart diff write: affected は再エンコード、unaffected は raw bytes コピー
    // reader はここで まだ alive (pf_section の Raw エントリが reader.mmap を参照)
    write_smart_diff_index_v3(
        index_path,
        &reader,
        &new_files_list,
        &pf_section,
        &affected_postings,
        &affected_set,
    )?;

    drop(reader);
    // diff mode ではキャッシュを更新しない:
    // - diff path はキャッシュを読まないため更新不要
    // - フルビルド fallback 時はフルビルド側でキャッシュを再構築する
    // - stale になった変更ファイルのエントリは次回フルビルドで自動再計算される
    let _ = cache_path; // suppress unused warning

    // Phase1 walkで収集済みの (path, mtime, size) から fingerprint を計算
    // compute_corpus_fingerprint (3回目のwalk ~1.2s) を排除
    fp_entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let mut h = Xxh64::new(0);
    for (path, mtime, size) in &fp_entries {
        h.update(path.as_bytes());
        h.update(&[0u8]);
        h.update(&mtime.to_le_bytes());
        h.update(&size.to_le_bytes());
    }
    let _ = write_fingerprint(fp_path, h.digest());

    Ok(Some(true))
}

/// Smart differential index write.
///
/// 最適化の3層:
/// 1. affected_postings のみ再エンコード (decode/encode ~25K vs 100K)
/// 2. unaffected posting list は old mmap から raw bytes コピー
/// 3. per-file section は unchanged ファイルを raw bytes コピー
///    (137K × Vec<u32> alloc を排除 = 274K mmap syscall 削減)
///
/// 前提: ID remap が必要な trigrams はすべて affected_set に含まれていること。
fn write_smart_diff_index_v3<'a>(
    index_path: &Path,
    old_reader: &'a crate::index::reader::IndexReader,
    files: &[FileInfo],
    pf_section: &[PerFileSection<'a>],
    affected_postings: &HashMap<[u8; 3], Vec<u32>>,
    affected_set: &HashSet<[u8; 3]>,
) -> Result<()> {
    // old mmap の全 trigram entry (sorted) を raw bytes 付きで取得
    let old_raw_entries = old_reader.all_trigram_entries_raw();
    let old_raw_map: HashMap<[u8; 3], &[u8]> =
        old_raw_entries.iter().map(|&(t, b)| (t, b)).collect();

    // 最終的な trigram リスト: (old - affected) ∪ non-empty affected
    let final_trigrams: Vec<[u8; 3]> = {
        let mut v: Vec<[u8; 3]> = old_raw_map
            .keys()
            .filter(|t| !affected_set.contains(*t))
            .copied()
            .collect();
        for &t in affected_postings.keys() {
            v.push(t);
        }
        v.sort_unstable();
        v.dedup();
        v
    };

    let parent = index_path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let temp_path = parent.join(format!(".xgrep_tmp_{}", std::process::id()));
    let out_file = fs::File::create(&temp_path)?;
    let mut writer = BufWriter::with_capacity(256 * 1024, out_file);

    let mut header = Header {
        magic: MAGIC,
        version: VERSION,
        trigram_count: final_trigrams.len() as u32,
        file_count: files.len() as u32,
        posting_total_bytes: 0,
        per_file_section_offset: 0,
    };
    writer.write_all(&header.to_bytes())?;
    writer.write_all(&vec![0u8; final_trigrams.len() * TrigramEntry::SIZE])?;

    let mut trigram_entries: Vec<TrigramEntry> = Vec::with_capacity(final_trigrams.len());
    let mut posting_buf: Vec<u8> = Vec::with_capacity(4096);
    let mut current_offset: u64 = 0;

    for &t in &final_trigrams {
        let raw: &[u8] = if let Some(ids) = affected_postings.get(&t) {
            // 再エンコード
            posting_buf.clear();
            encode_varint(&mut posting_buf, ids.len() as u32);
            let mut prev = 0u32;
            for &fid in ids {
                encode_varint(&mut posting_buf, fid - prev);
                prev = fid;
            }
            &posting_buf
        } else if let Some(&r) = old_raw_map.get(&t) {
            // raw bytes コピー (decode/encode なし)
            r
        } else {
            continue;
        };

        let len = raw.len() as u32;
        writer.write_all(raw)?;
        trigram_entries.push(TrigramEntry {
            trigram: t,
            _padding: 0,
            posting_offset: current_offset,
            posting_len: len,
        });
        current_offset += len as u64;
    }

    // File Table + String Pool
    let mut string_pool = Vec::new();
    for fi in files {
        let path_offset = string_pool.len() as u32;
        string_pool.extend_from_slice(fi.relative_path.as_bytes());
        string_pool.push(0);
        writer.write_all(
            &FileEntry {
                path_offset,
                mtime: fi.mtime,
                size: fi.size,
                content_hash: fi.content_hash,
            }
            .to_bytes(),
        )?;
    }
    writer.write_all(&string_pool)?;

    header.posting_total_bytes = current_offset;
    writer.flush()?;
    let mut file = writer
        .into_inner()
        .map_err(|e| XgrepError::Io(e.into_error()))?;

    let per_file_section_offset = file.seek(SeekFrom::End(0))?;

    // Per-File Section:
    // - Unchanged files: bulk memcpy from old mmap (no alloc, no parse)
    // - Changed/new files: write new entry from FileInfo + Vec<u32> trigrams
    {
        let mut pf_writer = BufWriter::with_capacity(256 * 1024, &mut file);
        pf_writer.write_all(&(files.len() as u32).to_le_bytes())?;
        for (i, fi) in files.iter().enumerate() {
            match &pf_section[i] {
                PerFileSection::Raw(raw) => {
                    // Copy old entry (mtime + content_hash + trigrams) as-is
                    pf_writer.write_all(raw)?;
                }
                PerFileSection::New(trigrams_u32) => {
                    pf_writer.write_all(&fi.mtime.to_le_bytes())?;
                    pf_writer.write_all(&fi.content_hash.to_le_bytes())?;
                    pf_writer.write_all(&(trigrams_u32.len() as u32).to_le_bytes())?;
                    for &t_u32 in trigrams_u32.iter() {
                        pf_writer.write_all(&t_u32.to_le_bytes())?;
                    }
                }
            }
        }
        pf_writer.flush()?;
    }

    header.per_file_section_offset = per_file_section_offset;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&header.to_bytes())?;
    file.seek(SeekFrom::Start(Header::SIZE as u64))?;
    let mut trig_writer = BufWriter::with_capacity(64 * 1024, file);
    for entry in &trigram_entries {
        trig_writer.write_all(&entry.to_bytes())?;
    }
    trig_writer.flush()?;
    drop(trig_writer);

    fs::rename(&temp_path, index_path)?;
    Ok(())
}

struct FileInfo {
    relative_path: String,
    mtime: u64,
    size: u64,
    content_hash: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::cache::cache_path_for;
    use std::fs;
    use tempfile::tempdir;

    fn collect_trigram_to_paths(
        reader: &crate::index::reader::IndexReader,
    ) -> std::collections::HashMap<[u8; 3], Vec<String>> {
        let mut result = std::collections::HashMap::new();
        for t in reader.all_trigrams() {
            let file_ids = reader.lookup_trigram(t);
            let paths: Vec<String> = file_ids
                .iter()
                .map(|&fid| reader.file_path(fid).to_string())
                .collect();
            result.insert(t, paths);
        }
        result
    }

    fn assert_diff_eq_full(dir: &Path, index_path: &Path) {
        use crate::index::reader::IndexReader;

        // 差分更新結果を読む
        let diff_reader = IndexReader::open(index_path).unwrap();
        let mut diff_map = collect_trigram_to_paths(&diff_reader);
        // paths をソートして比較可能にする
        for v in diff_map.values_mut() {
            v.sort();
        }
        drop(diff_reader);

        // フルリビルドを行う
        let full_index_path = dir.join("full_rebuild.xgrep");
        build_index(dir, &full_index_path).unwrap();

        let full_reader = IndexReader::open(&full_index_path).unwrap();
        let mut full_map = collect_trigram_to_paths(&full_reader);
        for v in full_map.values_mut() {
            v.sort();
        }

        assert_eq!(diff_map, full_map, "差分更新結果がフルリビルドと一致しない");
    }

    #[test]
    fn test_build_index_creates_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("hello.txt"), "hello world").unwrap();
        fs::write(root.join("foo.rs"), "fn main() {}").unwrap();

        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        assert!(index_path.exists());
        let data = fs::read(&index_path).unwrap();
        assert!(data.len() > Header::SIZE);
        assert_eq!(&data[0..4], b"XGRP");
    }

    #[test]
    fn test_build_index_header() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "abcdef").unwrap();

        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        let data = fs::read(&index_path).unwrap();
        let header = crate::index::reader::read_header(&data[..Header::SIZE]);
        assert_eq!(&header.magic, b"XGRP");
        assert_eq!(header.version, VERSION);
        assert_eq!(header.file_count, 1);
        assert!(header.trigram_count > 0);
    }

    #[test]
    fn test_build_respects_gitignore() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        // Create .gitignore
        fs::write(root.join(".gitignore"), "ignored_dir/\n*.log\n").unwrap();

        // Create files
        fs::write(root.join("real.txt"), "hello world").unwrap();
        fs::create_dir(root.join("ignored_dir")).unwrap();
        fs::write(root.join("ignored_dir/secret.txt"), "should be ignored").unwrap();
        fs::write(root.join("debug.log"), "should be ignored").unwrap();

        // Need to init git repo for .gitignore to work
        crate::git::git_cmd()
            .args(["init"])
            .current_dir(root)
            .output()
            .unwrap();

        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        let data = fs::read(&index_path).unwrap();
        let header = crate::index::reader::read_header(&data[..Header::SIZE]);
        // Only real.txt should be indexed (not .gitignore, not ignored files)
        assert_eq!(header.file_count, 1);
    }

    #[test]
    fn test_build_empty_directory() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // No files at all
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();
        let data = fs::read(&index_path).unwrap();
        let header = crate::index::reader::read_header(&data[..Header::SIZE]);
        assert_eq!(header.file_count, 0);
        assert_eq!(header.trigram_count, 0);
    }

    #[test]
    fn test_build_skips_binary_files() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // Binary file (contains NUL byte)
        fs::write(root.join("binary.bin"), b"hello\x00world").unwrap();
        fs::write(root.join("text.txt"), "hello world").unwrap();
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();
        let data = fs::read(&index_path).unwrap();
        let header = crate::index::reader::read_header(&data[..Header::SIZE]);
        assert_eq!(header.file_count, 1); // only text.txt
    }

    #[test]
    fn test_build_empty_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("empty.txt"), "").unwrap();
        fs::write(root.join("real.txt"), "hello world").unwrap();
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();
        let data = fs::read(&index_path).unwrap();
        let header = crate::index::reader::read_header(&data[..Header::SIZE]);
        // Empty file has no trigrams but is still indexed
        assert_eq!(header.file_count, 2);
    }

    #[test]
    fn test_build_file_shorter_than_3_bytes() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("tiny.txt"), "ab").unwrap();
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();
        let data = fs::read(&index_path).unwrap();
        let header = crate::index::reader::read_header(&data[..Header::SIZE]);
        assert_eq!(header.file_count, 1);
        // File has no trigrams (< 3 bytes)
        assert_eq!(header.trigram_count, 0);
    }

    #[test]
    fn test_build_nested_directories() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("a/b/c")).unwrap();
        fs::write(root.join("a/b/c/deep.txt"), "deep file content").unwrap();
        fs::write(root.join("top.txt"), "top level").unwrap();
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();
        let data = fs::read(&index_path).unwrap();
        let header = crate::index::reader::read_header(&data[..Header::SIZE]);
        assert_eq!(header.file_count, 2);
    }

    #[test]
    fn test_build_utf8_content() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("japanese.txt"), "これは日本語のテストです").unwrap();
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();
        let data = fs::read(&index_path).unwrap();
        let header = crate::index::reader::read_header(&data[..Header::SIZE]);
        assert_eq!(header.file_count, 1);
        assert!(header.trigram_count > 0);
    }

    #[test]
    fn test_build_skips_dotgit() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join(".git/config"), "git config content here").unwrap();
        fs::write(root.join("real.txt"), "hello world").unwrap();

        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        let data = fs::read(&index_path).unwrap();
        let header = crate::index::reader::read_header(&data[..Header::SIZE]);
        assert_eq!(header.file_count, 1);
    }

    #[test]
    fn test_trigram_cache_save_load_roundtrip() {
        let dir = tempdir().unwrap();
        let cache_path = dir.path().join("test.cache");

        let mut cache = TrigramCache {
            entries: HashMap::new(),
        };
        cache.entries.insert(
            "hello.txt".to_string(),
            CachedFile {
                mtime: 12345,
                content_hash: 99999,
                trigrams: vec![*b"hel", *b"ell", *b"llo"],
            },
        );
        cache.entries.insert(
            "foo.rs".to_string(),
            CachedFile {
                mtime: 67890,
                content_hash: 11111,
                trigrams: vec![*b"fn ", *b"n m", *b" ma"],
            },
        );
        cache.save(&cache_path).unwrap();

        let loaded = TrigramCache::load(&cache_path);
        assert_eq!(loaded.entries.len(), 2);

        let hello = loaded.entries.get("hello.txt").unwrap();
        assert_eq!(hello.mtime, 12345);
        assert_eq!(hello.content_hash, 99999);
        assert_eq!(hello.trigrams, vec![*b"hel", *b"ell", *b"llo"]);

        let foo = loaded.entries.get("foo.rs").unwrap();
        assert_eq!(foo.mtime, 67890);
        assert_eq!(foo.content_hash, 11111);
        assert_eq!(foo.trigrams, vec![*b"fn ", *b"n m", *b" ma"]);
    }

    #[test]
    fn test_trigram_cache_load_missing_file() {
        let cache = TrigramCache::load(Path::new("/nonexistent/path/test.cache"));
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn test_trigram_cache_load_corrupt_data() {
        let dir = tempdir().unwrap();
        let cache_path = dir.path().join("bad.cache");
        fs::write(&cache_path, b"xx").unwrap(); // Less than 4 bytes
        let cache = TrigramCache::load(&cache_path);
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn test_build_with_cache_creates_cache_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("hello.txt"), "hello world").unwrap();

        let index_path = root.join("index.xgrep");
        let cache_path = cache_path_for(&index_path);
        build_index_with_cache(root, &index_path, Some(&cache_path)).unwrap();

        assert!(index_path.exists());
        assert!(cache_path.exists());

        // Verify cache contains entries
        let cache = TrigramCache::load(&cache_path);
        assert_eq!(cache.entries.len(), 1);
        assert!(cache.entries.contains_key("hello.txt"));
    }

    #[test]
    fn test_build_with_cache_incremental_produces_same_index() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world foo bar").unwrap();
        fs::write(root.join("b.txt"), "another file content here").unwrap();

        let index_path = root.join("index.xgrep");
        let cache_path = cache_path_for(&index_path);

        // First build (no cache)
        build_index_with_cache(root, &index_path, Some(&cache_path)).unwrap();
        let index_data_1 = fs::read(&index_path).unwrap();

        // Second build (with cache, no file changes)
        build_index_with_cache(root, &index_path, Some(&cache_path)).unwrap();
        let index_data_2 = fs::read(&index_path).unwrap();

        // Verify index contents are identical
        assert_eq!(index_data_1, index_data_2);
    }

    #[test]
    fn test_build_with_cache_after_file_change() {
        use crate::index::reader::IndexReader;

        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world").unwrap();
        fs::write(root.join("b.txt"), "unchanged content here").unwrap();

        let index_path = root.join("index.xgrep");
        let cache_path = cache_path_for(&index_path);

        // First build
        build_index_with_cache(root, &index_path, Some(&cache_path)).unwrap();

        let reader1 = IndexReader::open(&index_path).unwrap();
        assert_eq!(reader1.file_count(), 2);

        // Modify a.txt
        // Wait briefly to ensure mtime changes
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(root.join("a.txt"), "modified content xyz").unwrap();

        // Incremental build (b.txt cache hit, a.txt re-read)
        build_index_with_cache(root, &index_path, Some(&cache_path)).unwrap();

        let reader2 = IndexReader::open(&index_path).unwrap();
        assert_eq!(reader2.file_count(), 2);

        // Verify "xyz" trigram is found
        let posting = reader2.lookup_trigram(*b"xyz");
        assert!(
            !posting.is_empty(),
            "changed file content should be indexed"
        );
    }

    #[test]
    fn test_build_with_cache_file_added() {
        use crate::index::reader::IndexReader;

        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world").unwrap();

        let index_path = root.join("index.xgrep");
        let cache_path = cache_path_for(&index_path);

        build_index_with_cache(root, &index_path, Some(&cache_path)).unwrap();
        let reader1 = IndexReader::open(&index_path).unwrap();
        assert_eq!(reader1.file_count(), 1);

        // Add a new file
        fs::write(root.join("b.txt"), "new file zqx").unwrap();

        build_index_with_cache(root, &index_path, Some(&cache_path)).unwrap();
        let reader2 = IndexReader::open(&index_path).unwrap();
        assert_eq!(reader2.file_count(), 2);
    }

    #[test]
    fn test_concurrent_build_lock() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello").unwrap();
        let index_path = root.join("index.xgrep");

        // Manually create a lock file with our PID (simulating a concurrent build)
        let lock_path = index_path.with_extension("lock");
        fs::write(&lock_path, format!("{}", std::process::id())).unwrap();

        // Build should fail because lock exists and our process is alive
        let result = build_index(root, &index_path);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("already in progress"));

        // Clean up lock
        fs::remove_file(&lock_path).unwrap();

        // Now build should succeed
        let result = build_index(root, &index_path);
        assert!(result.is_ok());
    }

    #[test]
    #[cfg(unix)]
    fn test_stale_lock_recovery() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello").unwrap();
        let index_path = root.join("index.xgrep");

        // Create a lock file with a non-existent PID
        let lock_path = index_path.with_extension("lock");
        fs::write(&lock_path, "999999999").unwrap();

        // Build should succeed (stale lock recovered via PID check)
        let result = build_index(root, &index_path);
        assert!(result.is_ok());

        // Lock file should be cleaned up
        assert!(!lock_path.exists());
    }

    #[test]
    fn test_build_with_cache_file_deleted() {
        use crate::index::reader::IndexReader;

        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world").unwrap();
        fs::write(root.join("b.txt"), "goodbye world").unwrap();

        let index_path = root.join("index.xgrep");
        let cache_path = cache_path_for(&index_path);

        build_index_with_cache(root, &index_path, Some(&cache_path)).unwrap();
        let reader1 = IndexReader::open(&index_path).unwrap();
        assert_eq!(reader1.file_count(), 2);

        // Delete b.txt
        fs::remove_file(root.join("b.txt")).unwrap();

        build_index_with_cache(root, &index_path, Some(&cache_path)).unwrap();
        let reader2 = IndexReader::open(&index_path).unwrap();
        assert_eq!(reader2.file_count(), 1);

        // Verify it was also removed from cache
        let cache = TrigramCache::load(&cache_path);
        assert_eq!(cache.entries.len(), 1);
        assert!(!cache.entries.contains_key("b.txt"));
    }

    // ----------------------------------------------------------------
    // Fingerprint / early-return tests
    // ----------------------------------------------------------------

    #[test]
    fn test_second_init_unchanged_returns_false() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world").unwrap();
        fs::write(root.join("b.txt"), "another file").unwrap();

        let index_path = root.join("index.xgrep");

        let first = build_index(root, &index_path).unwrap();
        assert!(first, "first build should return true (rebuilt)");

        let second = build_index(root, &index_path).unwrap();
        assert!(
            !second,
            "second build with no changes should return false (up to date)"
        );
    }

    #[test]
    fn test_init_after_file_modification_returns_true() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world").unwrap();

        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        // Wait for mtime to advance, then modify the file.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(root.join("a.txt"), "modified content xyz").unwrap();

        let rebuilt = build_index(root, &index_path).unwrap();
        assert!(
            rebuilt,
            "build after file change should return true (rebuilt)"
        );
    }

    #[test]
    fn test_init_after_new_file_returns_true() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world").unwrap();

        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        fs::write(root.join("b.txt"), "new file content").unwrap();

        let rebuilt = build_index(root, &index_path).unwrap();
        assert!(rebuilt, "build after new file should return true (rebuilt)");
    }

    #[test]
    fn test_init_after_file_deletion_returns_true() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world").unwrap();
        fs::write(root.join("b.txt"), "will be deleted").unwrap();

        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        fs::remove_file(root.join("b.txt")).unwrap();

        let rebuilt = build_index(root, &index_path).unwrap();
        assert!(
            rebuilt,
            "build after file deletion should return true (rebuilt)"
        );
    }

    #[test]
    fn test_fingerprint_file_created_after_build() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello").unwrap();

        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        let fp_path = fingerprint_path(&index_path);
        assert!(
            fp_path.exists(),
            "fingerprint file should be created after build"
        );
        assert_eq!(
            fs::read(&fp_path).unwrap().len(),
            8,
            "fingerprint file should be exactly 8 bytes (u64)"
        );
    }

    // ----------------------------------------------------------------
    // Diff update tests
    // ----------------------------------------------------------------

    #[test]
    fn test_diff_update_modify_file_eq_full_rebuild() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world foo").unwrap();
        fs::write(root.join("b.txt"), "another content here").unwrap();
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(root.join("b.txt"), "completely different xyz").unwrap();
        build_index_with_cache(root, &index_path, None).unwrap();

        assert_diff_eq_full(root, &index_path);
    }

    #[test]
    fn test_diff_update_add_file_eq_full_rebuild() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world foo").unwrap();
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        fs::write(root.join("b.txt"), "new file content xyz").unwrap();
        build_index_with_cache(root, &index_path, None).unwrap();

        assert_diff_eq_full(root, &index_path);
    }

    #[test]
    fn test_diff_update_delete_file_eq_full_rebuild() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world foo").unwrap();
        fs::write(root.join("b.txt"), "another content here").unwrap();
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        fs::remove_file(root.join("b.txt")).unwrap();
        build_index_with_cache(root, &index_path, None).unwrap();

        assert_diff_eq_full(root, &index_path);
    }

    #[test]
    fn test_diff_update_double_modify_eq_full_rebuild() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world foo").unwrap();
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(root.join("a.txt"), "first modification abc").unwrap();
        build_index_with_cache(root, &index_path, None).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(root.join("a.txt"), "second modification def").unwrap();
        build_index_with_cache(root, &index_path, None).unwrap();

        assert_diff_eq_full(root, &index_path);
    }

    #[test]
    fn test_diff_update_all_files_changed_uses_fallback() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.txt"), "hello world foo").unwrap();
        fs::write(root.join("b.txt"), "another content here").unwrap();
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(root.join("a.txt"), "changed a content xyz").unwrap();
        fs::write(root.join("b.txt"), "changed b content qrs").unwrap();

        // フォールバック（フルビルド）が走ってもOk(true)を返す
        let result = build_index_with_cache(root, &index_path, None).unwrap();
        assert!(result);

        // 正しくインデックスが更新されている
        use crate::index::reader::IndexReader;
        let reader = IndexReader::open(&index_path).unwrap();
        assert!(!reader.lookup_trigram(*b"xyz").is_empty());
    }

    #[test]
    fn test_diff_update_noop_trigrams_eq_full_rebuild() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // trigramが増減しないファイル変更（末尾に改行追加、内容は短い）
        fs::write(root.join("a.txt"), "abcdef").unwrap();
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1100));
        // 全く同じtrigramになるように同じ内容を書く（mtime変更のみ）
        // → diff updateはmtimeが変わったと判断するが、trigramは同じ
        fs::write(root.join("a.txt"), "abcdef").unwrap();
        build_index_with_cache(root, &index_path, None).unwrap();

        assert_diff_eq_full(root, &index_path);
    }

    #[test]
    fn test_diff_update_last_file_for_trigram_deleted() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // "zzz" というtrigramを含むのはb.txtだけ
        fs::write(root.join("a.txt"), "hello world foo").unwrap();
        fs::write(root.join("b.txt"), "zzz unique trigram").unwrap();
        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        use crate::index::reader::IndexReader;
        {
            let reader = IndexReader::open(&index_path).unwrap();
            assert!(!reader.lookup_trigram(*b"zzz").is_empty());
        }

        fs::remove_file(root.join("b.txt")).unwrap();
        build_index_with_cache(root, &index_path, None).unwrap();

        let reader = IndexReader::open(&index_path).unwrap();
        // パニックしないこと + 空リストを返すこと
        let result = reader.lookup_trigram(*b"zzz");
        assert!(result.is_empty());
    }

    // --- bincache correctness tests ---
    //
    // bincache の不変条件: diff 更新後のインデックスはフルリビルドと同一でなければならない。
    // bincache が関与する経路（new_candidates のバイナリ判定）も同じ原則に従う。
    //
    // 既知の制限: mtime と size が同一のままバイナリ→テキストに変化したファイルは
    // bincache によって誤ってスキップされる（OS の mtime 精度と同じ前提）。
    // これは git が `--assume-unchanged` で採用するのと同じトレードオフ。

    // bincache が存在しない / 破損している場合でも diff 更新がクラッシュせず
    // 結果がフルリビルドと一致することを確認する。
    #[test]
    fn test_bincache_missing_does_not_crash() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // text + binary
        fs::write(root.join("text.txt"), "hello world").unwrap();
        fs::write(root.join("binary.bin"), b"data\x00null").unwrap();

        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        // bincache が存在しないまま diff 更新
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(root.join("text.txt"), "hello world updated").unwrap();
        build_index_with_cache(root, &index_path, None).unwrap();

        assert_diff_eq_full(root, &index_path);
    }

    // bincache に記録されたバイナリファイルの mtime が変化した場合、
    // bincache ミスとなり再 peek される。バイナリのまま → インデックス対象外のまま。
    #[test]
    fn test_bincache_binary_mtime_changed_still_excluded() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("text.txt"), "hello world unique_abc").unwrap();
        fs::write(root.join("binary.bin"), b"data\x00null").unwrap();

        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        // 1回目 diff: binary.bin が new_candidate → bincache に記録される
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(root.join("text.txt"), "hello world unique_abc v2").unwrap();
        build_index_with_cache(root, &index_path, None).unwrap();

        // binary.bin の mtime を変更 (bincache ミスを誘発)
        std::thread::sleep(std::time::Duration::from_millis(1100));
        // 内容はバイナリのまま、mtime だけ更新
        fs::write(root.join("binary.bin"), b"data\x00null").unwrap();
        build_index_with_cache(root, &index_path, None).unwrap();

        // バイナリは依然インデックス対象外 → フルリビルドと一致
        assert_diff_eq_full(root, &index_path);
    }

    // バイナリ→テキストに変化し mtime が更新された場合、
    // bincache ミス → 再 peek → テキストと判定 → 正しくインデックスに含まれる。
    #[test]
    fn test_bincache_binary_to_text_reindexed() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("text.txt"), "hello world").unwrap();
        fs::write(root.join("became_text.bin"), b"was\x00binary").unwrap();

        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        // 1回目 diff: became_text.bin が new_candidate → bincache に記録
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(root.join("text.txt"), "hello world v2").unwrap();
        build_index_with_cache(root, &index_path, None).unwrap();

        // became_text.bin をテキストに変更 (mtime も更新)
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(root.join("became_text.bin"), "now text content qzx").unwrap();
        build_index_with_cache(root, &index_path, None).unwrap();

        // テキストになったので "qzx" が検索可能になる → フルリビルドと一致
        use crate::index::reader::IndexReader;
        let reader = IndexReader::open(&index_path).unwrap();
        let qzx = reader.lookup_trigram(*b"qzx");
        assert!(
            !qzx.is_empty(),
            "binary→text 変換後のコンテンツが検索可能でなければならない"
        );
        drop(reader);

        assert_diff_eq_full(root, &index_path);
    }

    // テキスト→バイナリに変化し mtime が更新された場合、
    // 旧 trigrams が posting list から除去され、その trigram を持つ最後のファイルが
    // 除外されると posting list が空になることを確認する。
    #[test]
    fn test_bincache_text_to_binary_posting_list_emptied() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // "qqq" trigram は became_binary.txt のみが持つ
        fs::write(root.join("became_binary.txt"), "qqq unique trigram").unwrap();
        fs::write(root.join("other.txt"), "hello world").unwrap();

        let index_path = root.join("index.xgrep");
        build_index(root, &index_path).unwrap();

        use crate::index::reader::IndexReader;
        {
            let reader = IndexReader::open(&index_path).unwrap();
            assert!(
                !reader.lookup_trigram(*b"qqq").is_empty(),
                "初期 build で qqq が存在"
            );
        }

        // became_binary.txt をバイナリに変更
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(root.join("became_binary.txt"), b"binary\x00content").unwrap();
        build_index_with_cache(root, &index_path, None).unwrap();

        let reader = IndexReader::open(&index_path).unwrap();
        let result = reader.lookup_trigram(*b"qqq");
        assert!(
            result.is_empty(),
            "テキスト→バイナリ後、qqq の posting list が空でなければならない"
        );
        drop(reader);

        assert_diff_eq_full(root, &index_path);
    }
}
