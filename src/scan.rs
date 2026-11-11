//! Library survey: finds videos, probes them in parallel (cached), and judges each one.
//! Shared by the `scan` command and the web UI.

use crate::estimate::{self, SizeRange, Verdict};
use crate::probe::{self, MediaInfo};
use anyhow::{Context, Result};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

pub const VIDEO_EXTS: &[&str] = &[
    "mp4", "mkv", "m4v", "mov", "webm", "avi", "wmv", "flv", "ts", "m2ts", "mpg", "mpeg", "3gp",
];
const SKIP_DIRS: &[&str] = &["System Volume Information", "$RECYCLE.BIN", "lost+found"];

/// Probe results are cached by path, size and mtime so rescans of big libraries are fast.
#[derive(Serialize, Deserialize, Clone)]
struct CacheEntry {
    size: u64,
    mtime: u64,
    info: Option<MediaInfo>,
    error: Option<String>,
}

#[derive(Serialize)]
pub struct FileResult {
    pub path: PathBuf,
    pub root: PathBuf,
    pub size: u64,
    /// Modification time (seconds since 1970), with `size` the key of the caches.
    #[serde(skip)]
    pub mtime: u64,
    pub info: Option<MediaInfo>,
    pub error: Option<String>,
    pub verdict: Verdict,
    pub estimate: SizeRange,
}

/// Counts of a finished scan, for the log lines.
pub struct Stats {
    pub fresh: usize,
    pub cached: usize,
    pub dropped: usize,
}

/// Probes every video under `paths` (absolute) with `jobs` ffprobe processes at once.
/// `progress(done, total)` is called as files finish.
pub fn collect(
    paths: &[PathBuf],
    ffprobe: &str,
    jobs: usize,
    min_savings: f64,
    model: &estimate::Model,
    progress: &(dyn Fn(usize, usize) + Sync),
) -> Result<(Vec<FileResult>, Stats)> {
    let pool = rayon::ThreadPoolBuilder::new().num_threads(jobs.max(1)).build()?;

    let mut files: Vec<(PathBuf, PathBuf, u64, u64)> = Vec::new();
    for root in paths {
        for entry in WalkDir::new(root).into_iter().filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !(e.file_type().is_dir() && (SKIP_DIRS.contains(&name.as_ref()) || name.starts_with(".Trash")))
        }) {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_file() || !is_video(entry.path()) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
            files.push((entry.path().to_path_buf(), root.clone(), meta.len(), mtime));
        }
    }
    let total = files.len();
    progress(0, total);

    let cache_path = cache_file()?;
    // Two scans at once (UI) must not interleave their read-modify-write of the cache file.
    static CACHE_LOCK: Mutex<()> = Mutex::new(());
    let _guard = CACHE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cache: HashMap<PathBuf, CacheEntry> = std::fs::read(&cache_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let cache = Mutex::new(cache);
    let done = AtomicUsize::new(0);
    let fresh = AtomicUsize::new(0);

    // Forget files that were under the scanned folders last time but are gone now.
    let dropped = {
        let found: std::collections::HashSet<&Path> = files.iter().map(|f| f.0.as_path()).collect();
        let mut c = cache.lock().unwrap();
        let before = c.len();
        c.retain(|p, _| found.contains(p.as_path()) || !paths.iter().any(|root| p.starts_with(root)));
        before - c.len()
    };

    let results: Vec<FileResult> = pool.install(|| {
        files
            .par_iter()
            .map(|(path, root, size, mtime)| {
                let cached = cache.lock().unwrap().get(path).filter(|c| c.size == *size && c.mtime == *mtime).cloned();
                let entry = cached.unwrap_or_else(|| {
                    let (info, error) = match probe::probe(ffprobe, path, *size) {
                        Ok(i) => (Some(i), None),
                        Err(e) => (None, Some(format!("{e:#}"))),
                    };
                    let e = CacheEntry { size: *size, mtime: *mtime, info, error };
                    let mut c = cache.lock().unwrap();
                    c.insert(path.clone(), e.clone());
                    // Save progress periodically so a crash doesn't lose a long scan.
                    if fresh.fetch_add(1, Ordering::Relaxed) % 500 == 499 {
                        let _ = save_cache(&cache_path, &c);
                    }
                    e
                });
                progress(done.fetch_add(1, Ordering::Relaxed) + 1, total);
                let (verdict, estimate) = match &entry.info {
                    Some(i) => estimate::judge(i, *size, min_savings, model),
                    None => (Verdict::Unreadable, SizeRange { low: *size as f64, likely: *size as f64, high: *size as f64 }),
                };
                FileResult { path: path.clone(), root: root.clone(), size: *size, mtime: *mtime, info: entry.info, error: entry.error, verdict, estimate }
            })
            .collect()
    });
    save_cache(&cache_path, &cache.lock().unwrap())?;
    let fresh = fresh.load(Ordering::Relaxed);
    Ok((results, Stats { fresh, cached: total - fresh, dropped }))
}

pub fn is_video(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| VIDEO_EXTS.contains(&e.to_ascii_lowercase().as_str()))
}

fn cache_file() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .context("no HOME or XDG_CACHE_HOME")?;
    let dir = base.join("av1opt");
    std::fs::create_dir_all(&dir)?;
    // Bump the version when MediaInfo gains fields that change verdicts, so old entries are re-probed.
    Ok(dir.join("probe-cache-v2.json"))
}

fn save_cache(path: &Path, cache: &HashMap<PathBuf, CacheEntry>) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(cache)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
