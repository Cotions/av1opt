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
    // A file is listed once, however many of the folders given lead to it (one folder
    // inside another, the same one twice, a link to it): `dups` would otherwise find it
    // identical to itself and offer one of the two for the trash.
    let mut seen = std::collections::HashSet::new();
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
            if !seen.insert({
                use std::os::unix::fs::MetadataExt;
                (meta.dev(), meta.ino())
            }) {
                continue;
            }
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

    // Forget files that were under the scanned folders last time but are gone now. A folder
    // with nothing in it at all is more likely a drive that is not mounted than a library
    // that was emptied: what is known of its files is kept.
    let dropped = {
        let found: std::collections::HashSet<&Path> = files.iter().map(|f| f.0.as_path()).collect();
        let live: std::collections::HashSet<&Path> = files.iter().map(|f| f.1.as_path()).collect();
        let mut c = cache.lock().unwrap();
        let before = c.len();
        c.retain(|p, _| found.contains(p.as_path()) || !paths.iter().any(|root| live.contains(root.as_path()) && p.starts_with(root)));
        before - c.len()
    };
    // A probe that fails is remembered with the file, so that a broken file is not probed
    // again on every scan. An ffprobe that cannot be started at all (no such program) says
    // nothing about any file.
    let unknown = {
        let c = cache.lock().unwrap();
        files.iter().any(|(path, _, size, mtime)| !c.get(path).is_some_and(|e| e.size == *size && e.mtime == *mtime))
    };
    if unknown {
        if std::process::Command::new(ffprobe).arg("-version").stdin(std::process::Stdio::null()).output().is_err() {
            anyhow::bail!("cannot run ffprobe ({ffprobe}); nothing was scanned");
        }
    }

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

/// What earlier scans learned about these files, without running ffprobe: only files whose
/// size and date still match their cache entry.
#[cfg(feature = "ui")]
pub fn cached_info(paths: &[PathBuf]) -> HashMap<PathBuf, MediaInfo> {
    let Some(cache) = cache_file().ok().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice::<HashMap<PathBuf, CacheEntry>>(&b).ok()) else {
        return HashMap::new();
    };
    paths
        .iter()
        .filter_map(|p| {
            let e = cache.get(p)?;
            let meta = std::fs::metadata(p).ok()?;
            let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
            (e.size == meta.len() && e.mtime == mtime).then(|| Some((p.clone(), e.info.clone()?)))?
        })
        .collect()
}

/// The videos to encode when there are `budget_secs` to encode in: those of `items` (bytes
/// saved, seconds to encode) that save the most together without taking longer. Returned as
/// indexes into `items`, the most saved per second first.
///
/// Taking the best value per hour first is not it: one long video can leave a gap that two
/// shorter ones would have filled. This tries every way of filling the time, with the time
/// cut in slots (lengths are rounded up to whole slots, so the choice never runs over).
pub fn best_within(items: &[(f64, f64)], budget_secs: f64) -> Vec<usize> {
    let rate = |i: usize| items[i].0 / items[i].1;
    let by_rate = |mut v: Vec<usize>| {
        v.sort_by(|&a, &b| rate(b).total_cmp(&rate(a)).then(a.cmp(&b)));
        v
    };
    // A video whose time is not known cannot be fitted in, and one that saves nothing is not worth it.
    let usable: Vec<usize> = (0..items.len()).filter(|&i| items[i].0 > 0.0 && items[i].1 > 0.0 && items[i].1 <= budget_secs).collect();
    if usable.iter().map(|&i| items[i].1).sum::<f64>() <= budget_secs {
        return by_rate(usable);
    }
    // Finer slots waste less of the time on rounding; fewer keep a library of tens of
    // thousands of videos to a moment's work.
    let slots = (40_000_000 / usable.len()).clamp(1000, 10_000);
    let slot_secs = budget_secs / slots as f64;
    let width: Vec<usize> = usable.iter().map(|&i| ((items[i].1 / slot_secs).ceil() as usize).clamp(1, slots)).collect();
    let mut best = vec![0.0_f64; slots + 1];
    let words = (slots + 1).div_ceil(64);
    // took[k] has a bit per number of slots: whether the best choice for that many slots
    // among the first k+1 videos has video k in it.
    let mut took = vec![0_u64; usable.len() * words];
    for (k, &i) in usable.iter().enumerate() {
        for c in (width[k]..=slots).rev() {
            let with = best[c - width[k]] + items[i].0;
            if with > best[c] {
                best[c] = with;
                took[k * words + c / 64] |= 1 << (c % 64);
            }
        }
    }
    let (mut c, mut chosen, mut left) = (slots, Vec::new(), budget_secs);
    for k in (0..usable.len()).rev() {
        if took[k * words + c / 64] >> (c % 64) & 1 == 1 {
            chosen.push(usable[k]);
            c -= width[k];
            left -= items[usable[k]].1;
        }
    }
    // What rounding up left unused may still hold a short video or two.
    for i in by_rate(usable) {
        if items[i].1 <= left && !chosen.contains(&i) {
            chosen.push(i);
            left -= items[i].1;
        }
    }
    by_rate(chosen)
}

/// The candidates of a scan to encode in `budget_secs`: see [`best_within`].
pub fn pick_for_time<'a>(results: &'a [FileResult], time: &estimate::TimeModel, budget_secs: f64) -> Vec<&'a FileResult> {
    let cands: Vec<&FileResult> = results.iter().filter(|r| r.verdict == Verdict::Candidate).collect();
    let items: Vec<(f64, f64)> = cands.iter().map(|r| (r.size as f64 - r.estimate.likely, r.info.as_ref().map_or(0.0, |i| time.secs(i)))).collect();
    best_within(&items, budget_secs).into_iter().map(|i| cands[i]).collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn totals(items: &[(f64, f64)], picked: &[usize]) -> (f64, f64) {
        picked.iter().fold((0.0, 0.0), |(v, t), &i| (v + items[i].0, t + items[i].1))
    }

    /// The most that fits, by trying every subset.
    fn brute(items: &[(f64, f64)], budget: f64) -> f64 {
        (0..1_u32 << items.len())
            .map(|mask| (0..items.len()).filter(|i| mask >> i & 1 == 1).collect::<Vec<_>>())
            .map(|set| totals(items, &set))
            .filter(|(_, t)| *t <= budget)
            .map(|(v, _)| v)
            .fold(0.0, f64::max)
    }

    #[test]
    fn two_short_videos_beat_the_long_one_with_the_best_rate() {
        // Six hours at the best rate leave two that nothing fills; the two four-hour ones save more.
        let h = 3600.0;
        let items = [(10e9, 6.0 * h), (6e9, 4.0 * h), (6e9, 4.0 * h), (0.5e9, 3.0 * h)];
        assert_eq!(best_within(&items, 8.0 * h), [1, 2]);
        // With ten hours the long one and a four-hour one fit.
        assert_eq!(best_within(&items, 10.0 * h), [0, 1]);
        // Everything fits: all of it, best value first.
        assert_eq!(best_within(&items, 17.0 * h), [0, 1, 2, 3]);
        assert!(best_within(&items, 2.0 * h).is_empty());
        assert!(best_within(&items, 0.0).is_empty());
    }

    #[test]
    fn videos_without_a_time_or_a_saving_are_left_out() {
        let items = [(5e9, 0.0), (0.0, 600.0), (-1e9, 600.0), (2e9, 900.0), (1e9, 7200.0)];
        assert_eq!(best_within(&items, 3600.0), [3]);
    }

    #[test]
    fn the_choice_never_runs_over_and_is_as_good_as_trying_everything() {
        let mut seed = 12345_u64;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as f64 / (1_u64 << 31) as f64
        };
        for round in 0..200 {
            let n = 4 + round % 11;
            // From a few minutes to a few hours each, saving from nothing much to a few GB.
            let items: Vec<(f64, f64)> = (0..n).map(|_| (next() * 4e9, 200.0 + next() * 12_000.0)).collect();
            let budget = 1800.0 + next() * 40_000.0;
            let picked = best_within(&items, budget);
            let (value, secs) = totals(&items, &picked);
            assert!(secs <= budget, "round {round}: {secs} s in {budget} s");
            let mut unique = picked.clone();
            unique.sort_unstable();
            unique.dedup();
            assert_eq!(unique.len(), picked.len(), "round {round}: a video picked twice");
            let most = brute(&items, budget);
            // Lengths are rounded up to slots, which may cost a sliver of the best choice.
            assert!(value >= most * 0.99, "round {round}: saves {value}, {most} was possible");
        }
    }

    #[test]
    fn a_large_library_is_picked_from_in_a_moment() {
        let items: Vec<(f64, f64)> = (0..30_000_u64).map(|i| (1e8 + (i * 7919 % 1000) as f64 * 5e6, 300.0 + (i * 104_729 % 5000) as f64)).collect();
        let started = std::time::Instant::now();
        let picked = best_within(&items, 8.0 * 3600.0);
        let (_, secs) = totals(&items, &picked);
        assert!(secs <= 8.0 * 3600.0 && secs > 7.9 * 3600.0, "{secs}");
        assert!(started.elapsed().as_secs_f64() < 5.0, "{:?}", started.elapsed());
    }
}
