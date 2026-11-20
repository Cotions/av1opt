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
    "mp4", "mkv", "m4v", "mov", "webm", "avi", "wmv", "flv", "ts", "m2ts", "mts", "mpg", "mpeg", "vob", "3gp",
];
const SKIP_DIRS: &[&str] = &["System Volume Information", "$RECYCLE.BIN", "lost+found"];

/// Whether a walk stays out of this: the trash folders and the system's folders hold no videos
/// of the library. What `replace` sent to a trash must not come back as work, and a copy in a
/// trash must not pass for the one to keep while the last real one is thrown away. That goes
/// for a walk that starts inside a trash too, and for the user's own trash, which a search of
/// the home folder comes across and which has no telling name.
pub fn not_for_videos(entry: &walkdir::DirEntry) -> bool {
    if !entry.file_type().is_dir() {
        return false;
    }
    let name = entry.file_name();
    SKIP_DIRS.iter().any(|skipped| name == *skipped)
        || crate::trash::is_trash_name(name)
        || (name == "Trash" && crate::trash::is_home_trash(entry.path()))
        || (entry.depth() == 0 && crate::trash::in_a_trash(entry.path()))
}

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
    /// The probe could not run or was cut short: "unreadable" here says nothing lasting about
    /// the file, and it is worth trying again.
    #[serde(skip)]
    #[cfg_attr(not(feature = "ui"), allow(dead_code))] // Only the watched folders act on it.
    pub unjudged: bool,
}

/// How many of the folders that could not be read are named to the person.
pub const UNREADABLE_SHOWN: usize = 5;

/// Counts of a finished scan, for the log lines.
pub struct Stats {
    pub fresh: usize,
    pub cached: usize,
    pub dropped: usize,
    /// Folders that could not be read, so that what is in them is not in the results.
    pub unreadable: Vec<PathBuf>,
}

impl Stats {
    /// What to tell about the folders that could not be read, if there are any.
    pub fn unreadable_note(&self) -> Option<String> {
        unreadable_note(&self.unreadable)
    }
}

/// What to tell about folders a walk could not read, if there are any.
pub fn unreadable_note(folders: &[PathBuf]) -> Option<String> {
    let (count, named) = (folders.len(), folders.iter().take(UNREADABLE_SHOWN));
    let names = named.map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ");
    let more = if count > UNREADABLE_SHOWN { format!(" and {} more", count - UNREADABLE_SHOWN) } else { String::new() };
    (count > 0).then(|| format!("{count} folder{} could not be read, so the videos in {} are left out: {names}{more}", if count == 1 { "" } else { "s" }, if count == 1 { "it" } else { "them" }))
}

/// The place a walk under `root` could not read. What went away during the walk was simply not
/// there to read.
pub fn unread_in_walk(e: &walkdir::Error, root: &Path) -> Option<PathBuf> {
    let gone = e.io_error().is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound);
    e.path().filter(|p| !gone || *p == root).map(Path::to_path_buf)
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
    collect_with_cache(&cache_file()?, paths, ffprobe, jobs, min_savings, model, progress)
}

fn collect_with_cache(
    cache_path: &Path,
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
    let mut unreadable: Vec<PathBuf> = Vec::new();
    for root in paths {
        for entry in WalkDir::new(root).into_iter().filter_entry(|e| !not_for_videos(e)) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    unreadable.extend(unread_in_walk(&e, root));
                    continue;
                }
            };
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

    // Two scans at once (UI) must not interleave their read-modify-write of the cache file.
    static CACHE_LOCK: Mutex<()> = Mutex::new(());
    let _guard = CACHE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cache: HashMap<PathBuf, CacheEntry> = std::fs::read(cache_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let cache = Mutex::new(cache);
    let done = AtomicUsize::new(0);
    let fresh = AtomicUsize::new(0);

    // Forget files that were under the scanned folders last time but are gone now. A folder
    // with nothing in it at all is more likely a drive that is not mounted than a library
    // that was emptied: what is known of its files is kept, as it is for a folder that could
    // not be read this time.
    let dropped = {
        let found: std::collections::HashSet<&Path> = files.iter().map(|f| f.0.as_path()).collect();
        let live: std::collections::HashSet<&Path> = files.iter().map(|f| f.1.as_path()).collect();
        let mut c = cache.lock().unwrap();
        let before = c.len();
        c.retain(|p, _| {
            found.contains(p.as_path()) || unreadable.iter().any(|folder| p.starts_with(folder)) || !paths.iter().any(|root| live.contains(root.as_path()) && p.starts_with(root))
        });
        before - c.len()
    };
    // A file ffprobe rejected is remembered as such, so that a broken file is not probed again
    // on every scan. A probe that could not run or was cut short says nothing about the file
    // and is not remembered; an ffprobe that cannot be started at all stops the scan here.
    let unknown = {
        let c = cache.lock().unwrap();
        files.iter().any(|(path, _, size, mtime)| !c.get(path).is_some_and(|e| e.size == *size && e.mtime == *mtime))
    };
    let mut version = std::process::Command::new(ffprobe);
    version.arg("-version");
    if unknown && crate::probe::run_ffprobe(version, crate::probe::Reads::Little).is_err() {
        anyhow::bail!("cannot run ffprobe ({ffprobe}); nothing was scanned");
    }

    let results: Vec<FileResult> = pool.install(|| {
        files
            .par_iter()
            .map(|(path, root, size, mtime)| {
                // The silent failure was kept by earlier versions and says nothing of the file.
                let cached = cache.lock().unwrap().get(path).filter(|c| c.size == *size && c.mtime == *mtime && c.error.as_deref() != Some(SILENT_FAILURE)).cloned();
                let (entry, unjudged) = cached.map_or_else(
                    || {
                        let (info, error, unjudged) = match probe::probe(ffprobe, path, *size) {
                            Ok(i) => (Some(i), None, false),
                            Err(e) => (None, Some(format!("{e:#}")), probe::left_unjudged(&e)),
                        };
                        let e = CacheEntry { size: *size, mtime: *mtime, info, error };
                        let mut c = cache.lock().unwrap();
                        if !unjudged {
                            c.insert(path.clone(), e.clone());
                        }
                        // Save progress periodically so a crash doesn't lose a long scan.
                        if fresh.fetch_add(1, Ordering::Relaxed) % 500 == 499 {
                            let _ = save_cache(cache_path, &c);
                        }
                        (e, unjudged)
                    },
                    |e| (e, false),
                );
                progress(done.fetch_add(1, Ordering::Relaxed) + 1, total);
                let (verdict, estimate) = match &entry.info {
                    Some(i) => estimate::judge(i, *size, min_savings, model),
                    None => (Verdict::Unreadable, SizeRange { low: *size as f64, likely: *size as f64, high: *size as f64, chance: 0.0 }),
                };
                FileResult { path: path.clone(), root: root.clone(), size: *size, mtime: *mtime, info: entry.info, error: entry.error, verdict, estimate, unjudged }
            })
            .collect()
    });
    save_cache(cache_path, &cache.lock().unwrap())?;
    let fresh = fresh.load(Ordering::Relaxed);
    Ok((results, Stats { fresh, cached: total - fresh, dropped, unreadable }))
}

/// The error earlier versions noted for an ffprobe that failed without saying why.
const SILENT_FAILURE: &str = "ffprobe: could not read the file";

/// What earlier scans learned, read once for looking up many files without running ffprobe.
#[cfg(feature = "ui")]
pub struct ProbeCache(HashMap<PathBuf, CacheEntry>);

#[cfg(feature = "ui")]
impl ProbeCache {
    pub fn load() -> ProbeCache {
        ProbeCache(cache_file().ok().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default())
    }

    /// What is known of these files: only those whose size and date still match their entry.
    pub fn known(&self, paths: &[PathBuf]) -> HashMap<PathBuf, MediaInfo> {
        paths
            .iter()
            .filter_map(|p| {
                let e = self.0.get(p)?;
                let meta = std::fs::metadata(p).ok()?;
                let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
                (e.size == meta.len() && e.mtime == mtime).then(|| Some((p.clone(), e.info.clone()?)))?
            })
            .collect()
    }
}

/// How finely [`best_within`] cuts the time for `videos` videos. Its work is the two multiplied.
/// Finer slots waste less of the time on rounding; fewer keep a library of tens of thousands of
/// videos to a moment's work.
fn slots_for(videos: usize) -> usize {
    (40_000_000 / videos.max(1)).clamp(1000, 10_000)
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
    let slots = slots_for(usable.len());
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
    let in_order = by_rate(usable);
    let picked: std::collections::HashSet<usize> = chosen.iter().copied().collect();
    for &i in &in_order {
        if items[i].1 <= left && !picked.contains(&i) {
            chosen.push(i);
            left -= items[i].1;
        }
    }
    // With more videos than slots, each short one counts as a whole slot and the slots go
    // to those that save the most apiece, not per second: there, taking the best value per
    // second until the time is full does better.
    let mut plain = Vec::new();
    let mut left = budget_secs;
    for &i in &in_order {
        if items[i].1 <= left {
            plain.push(i);
            left -= items[i].1;
        }
    }
    let saved = |v: &[usize]| v.iter().map(|&i| items[i].0).sum::<f64>();
    if saved(&plain) > saved(&chosen) { plain } else { by_rate(chosen) }
}

/// The files of a scan to encode in `budget_secs`, each counted for what it is likely to save
/// given its chance of being kept: see [`best_within`].
pub fn pick_for_time<'a>(results: &'a [FileResult], time: &estimate::TimeModel, budget_secs: f64) -> Vec<&'a FileResult> {
    let cands: Vec<&FileResult> = results.iter().filter(|r| r.verdict.will_be_tried()).collect();
    let items: Vec<(f64, f64)> = cands.iter().map(|r| (r.estimate.weighted_saving(r.size), r.info.as_ref().map_or(0.0, |i| time.secs(i)))).collect();
    best_within(&items, budget_secs).into_iter().map(|i| cands[i]).collect()
}

pub fn is_video(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| VIDEO_EXTS.contains(&e.to_ascii_lowercase().as_str()))
}

fn cache_file() -> Result<PathBuf> {
    let base = crate::files::xdg_folder("XDG_CACHE_HOME", ".cache").context("no HOME or XDG_CACHE_HOME that is an absolute path")?;
    cache_file_in(&base.join("av1opt"))
}

fn cache_file_in(dir: &Path) -> Result<PathBuf> {
    crate::files::private_folder(dir)?;
    // Bump the version when MediaInfo gains fields that change verdicts or what is read into
    // them, so old entries are re-probed. Version 3 holds Matroska lengths taken from what the
    // file calls its duration, wrong for files that do not start at zero.
    for old in ["probe-cache-v2.json", "probe-cache-v3.json"] {
        let _ = std::fs::remove_file(dir.join(old));
    }
    Ok(dir.join("probe-cache-v4.json"))
}

/// Leaves out the files whose paths are not text and says how many there were, for what has
/// to name its files in JSON or have ffmpeg read them.
pub fn drop_unnamed(found: &mut Vec<FileResult>) -> usize {
    let before = found.len();
    found.retain(|f| f.path.to_str().is_some());
    before - found.len()
}

/// A path that is not text has no place in a JSON file: such a file is probed on every scan,
/// and the scan does not stop for it.
fn save_cache(path: &Path, cache: &HashMap<PathBuf, CacheEntry>) -> Result<()> {
    let text: HashMap<&PathBuf, &CacheEntry> = cache.iter().filter(|(p, _)| p.to_str().is_some()).collect();
    Ok(crate::files::replace_with(path, &serde_json::to_vec(&text)?)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ffprobe_that_hangs_when_asked_its_version_stops_the_scan() {
        let dir = std::env::temp_dir().join(format!("av1opt-scan-hang-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("videos")).unwrap();
        std::fs::write(dir.join("videos/a.mkv"), b"x").unwrap();
        let ffprobe = dir.join("ffprobe");
        crate::files::write_script(&ffprobe, "exec sleep 60\n");
        crate::probe::LIMIT_IN_TESTS.set(Some(std::time::Duration::from_millis(100)));
        let started = std::time::Instant::now();
        let scan = collect_with_cache(&dir.join("cache.json"), &[dir.join("videos")], ffprobe.to_str().unwrap(), 1, 0.15, &estimate::Model::fit(&[], 93.0), &|_, _| {});
        crate::probe::LIMIT_IN_TESTS.set(None);
        assert!(started.elapsed() < std::time::Duration::from_secs(10), "{:?}", started.elapsed());
        assert!(scan.err().is_some_and(|e| e.to_string().contains("cannot run ffprobe")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_walk_finds_what_lies_in_a_trash() {
        let dir = std::fs::canonicalize(std::env::temp_dir()).unwrap().join(format!("av1opt-scan-trash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("share/Trash");
        crate::trash::TEST_HOME_TRASH.with(|h| *h.borrow_mut() = Some(home.clone()));
        for file in ["share/Trash/files/home.mkv", "lib/.Trash-1000/files/own.mkv", "lib/.Trash/1000/files/shared.mkv", "lib/kept.mkv", "lib/Trash/named-so.mkv", "share/kept-too.mkv"] {
            std::fs::create_dir_all(dir.join(file).parent().unwrap()).unwrap();
            std::fs::write(dir.join(file), b"x").unwrap();
        }
        let ffprobe = dir.join("ffprobe");
        crate::files::write_script(&ffprobe, "for last; do :; done\necho \"$last: Invalid data found when processing input\" >&2\nexit 1\n");
        let found = |roots: &[PathBuf]| {
            let (results, _) = collect_with_cache(&dir.join("cache.json"), roots, ffprobe.to_str().unwrap(), 1, 0.15, &estimate::Model::fit(&[], 93.0), &|_, _| {}).unwrap();
            let mut names: Vec<String> = results.iter().map(|r| r.path.file_name().unwrap().to_string_lossy().into_owned()).collect();
            names.sort();
            names
        };
        // A folder that is merely called Trash is the person's own business.
        assert_eq!(found(std::slice::from_ref(&dir)), ["kept-too.mkv", "kept.mkv", "named-so.mkv"]);
        // Given as the place to look, a trash stays as empty as when it is come across.
        assert!(found(&[home.join("files")]).is_empty());
        assert!(found(&[dir.join("lib/.Trash-1000/files")]).is_empty());
        assert!(found(&[dir.join("lib/.Trash/1000")]).is_empty());
        crate::trash::TEST_HOME_TRASH.with(|h| *h.borrow_mut() = None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_a_probe_that_really_ran_is_remembered() {
        let dir = std::env::temp_dir().join(format!("av1opt-scan-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("videos")).unwrap();
        for name in ["broken.mkv", "killed.mkv", "stale.mkv"] {
            std::fs::write(dir.join("videos").join(name), b"x").unwrap();
        }
        // Rejects one file, is killed on another, and meets a read error on the third.
        let ffprobe = dir.join("ffprobe");
        let script = "for last; do :; done\ncase \"$last\" in\n*broken*) echo \"$last: Invalid data found when processing input\" >&2; exit 1;;\n*killed*) kill -9 $$;;\n*) echo \"$last: Input/output error\" >&2; exit 1;;\nesac\n";
        crate::files::write_script(&ffprobe, script);
        let cache = dir.join("cache.json");
        let scan = || collect_with_cache(&cache, &[dir.join("videos")], ffprobe.to_str().unwrap(), 2, 0.15, &estimate::Model::fit(&[], 93.0), &|_, _| {}).unwrap().0;
        let results = scan();
        let unjudged = |results: &[FileResult], name: &str| results.iter().find(|r| r.path.ends_with(name)).unwrap().unjudged;
        assert!(results.iter().all(|r| r.verdict == Verdict::Unreadable));
        assert!(!unjudged(&results, "broken.mkv") && unjudged(&results, "killed.mkv") && unjudged(&results, "stale.mkv"));
        let kept: HashMap<PathBuf, CacheEntry> = serde_json::from_slice(&std::fs::read(&cache).unwrap()).unwrap();
        assert_eq!(kept.keys().map(|p| p.file_name().unwrap().to_str().unwrap()).collect::<Vec<_>>(), ["broken.mkv"]);

        // An entry an earlier version kept for an ffprobe that failed without a word is read again.
        let mut kept = kept;
        kept.values_mut().for_each(|e| e.error = Some(SILENT_FAILURE.into()));
        std::fs::write(&cache, serde_json::to_vec(&kept).unwrap()).unwrap();
        let again = scan();
        assert!(again.iter().find(|r| r.path.ends_with("broken.mkv")).unwrap().error.as_deref().is_some_and(|e| e.contains("Invalid data")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folder_that_cannot_be_read_is_reported_and_what_is_known_of_it_stays() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: a plain query of this process's user.
        if unsafe { libc::geteuid() } == 0 {
            return crate::tools::skipped("a_folder_that_cannot_be_read_is_reported_and_what_is_known_of_it_stays", "root can read every folder");
        }
        let dir = std::env::temp_dir().join(format!("av1opt-scan-unreadable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let closed = dir.join("videos/closed");
        std::fs::create_dir_all(&closed).unwrap();
        std::fs::write(closed.join("kept.mkv"), b"x").unwrap();
        std::fs::write(dir.join("videos/open.mkv"), b"x").unwrap();
        // Links are not followed, so a link back up is no endless walk and hides nothing.
        std::os::unix::fs::symlink(dir.join("videos"), dir.join("videos/back")).unwrap();
        let ffprobe = dir.join("ffprobe");
        std::fs::write(&ffprobe, "#!/bin/sh\nfor last; do :; done\necho \"$last: Invalid data found when processing input\" >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(&ffprobe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cache = dir.join("cache.json");
        let scan = |paths: &[PathBuf]| collect_with_cache(&cache, paths, ffprobe.to_str().unwrap(), 2, 0.15, &estimate::Model::fit(&[], 93.0), &|_, _| {}).unwrap();
        let videos = [dir.join("videos")];
        let (results, stats) = scan(&videos);
        assert_eq!((results.len(), stats.unreadable.len()), (2, 0));
        assert_eq!(stats.unreadable_note(), None);

        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000)).unwrap();
        let (results, stats) = scan(&videos);
        let (again, missing) = (scan(&videos).1, scan(&[dir.join("nowhere")]).1);
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(stats.unreadable, std::slice::from_ref(&closed));
        assert_eq!(stats.dropped + again.dropped, 0, "what is known of the folder is kept");
        let note = stats.unreadable_note().unwrap();
        assert!(note.starts_with("1 folder could not be read") && note.ends_with(closed.to_str().unwrap()), "{note}");
        assert_eq!(missing.unreadable, [dir.join("nowhere")]);

        let many = Stats { fresh: 0, cached: 0, dropped: 0, unreadable: (0..8).map(|i| PathBuf::from(format!("/f{i}"))).collect() };
        let note = many.unreadable_note().unwrap();
        assert!(note.starts_with("8 folders") && note.contains("/f4") && !note.contains("/f5") && note.ends_with(" and 3 more"), "{note}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dvd_and_camcorder_files_are_looked_at() {
        for name in ["a.vob", "b.VOB", "c.mts", "d.MTS", "e.m2ts"] {
            assert!(is_video(Path::new(name)), "{name}");
        }
        assert!(!is_video(Path::new("a.srt")) && !is_video(Path::new("vob")));
    }

    #[test]
    fn probes_of_an_older_version_are_left_behind() {
        let dir = std::env::temp_dir().join(format!("av1opt-scan-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("probe-cache-v3.json"), b"{}").unwrap();
        assert_eq!(cache_file_in(&dir).unwrap(), dir.join("probe-cache-v4.json"));
        assert!(!dir.join("probe-cache-v3.json").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

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
    fn a_name_that_is_not_text_does_not_stop_the_cache_being_saved() {
        use std::os::unix::ffi::OsStrExt;
        let dir = std::env::temp_dir().join(format!("av1opt-scancache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let entry = CacheEntry { size: 1, mtime: 2, info: None, error: Some("x".into()) };
        let odd = PathBuf::from(std::ffi::OsStr::from_bytes(b"/v/a\xff.mkv"));
        let cache = HashMap::from([(odd, entry.clone()), (PathBuf::from("/v/b.mkv"), entry)]);
        let at = dir.join("cache.json");
        save_cache(&at, &cache).unwrap();
        let back: HashMap<PathBuf, CacheEntry> = serde_json::from_slice(&std::fs::read(&at).unwrap()).unwrap();
        assert_eq!(back.keys().collect::<Vec<_>>(), [Path::new("/v/b.mkv")]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_time_goes_to_the_file_likelier_to_be_kept() {
        let info = crate::probe::MediaInfo { codec: "h264".into(), width: 1920, height: 1080, fps: 30.0, duration_s: 600.0, ..Default::default() };
        let file = |name: &str, verdict, likely: f64, chance| FileResult {
            path: name.into(), root: "/".into(), size: 1000, mtime: 0, info: Some(info.clone()), error: None, verdict, unjudged: false,
            estimate: SizeRange { low: likely, likely, high: likely, chance },
        };
        let time = estimate::TimeModel::fit(&[], 5);
        let one_video = time.secs(&info) * 1.5;
        // The doubtful one promises more, 600 bytes against 500, and may well not be kept.
        let results = [file("doubtful", Verdict::Maybe, 400.0, 0.15), file("sure", Verdict::Candidate, 500.0, 0.99), file("no", Verdict::LowGain, 100.0, 0.01)];
        let names = |budget| pick_for_time(&results, &time, budget).iter().map(|r| r.path.to_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(names(one_video), ["sure"]);
        // Given the time it is tried too, after the sure one.
        assert_eq!(names(one_video * 2.0), ["sure", "doubtful"]);
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
    fn many_short_videos_are_not_rounded_up_to_a_slot_each() {
        // Twenty-second clips that save 1 GB and three-minute ones that save 2: per second
        // the clips are worth four times as much, per slot of the time half.
        let items: Vec<(f64, f64)> = (0..30_000).map(|i| if i % 3 == 0 { (1e9, 20.0) } else { (2e9, 170.0) }).collect();
        let budget = 100.0 * 3600.0;
        let picked = best_within(&items, budget);
        let (value, secs) = totals(&items, &picked);
        assert!(secs <= budget);
        // All ten thousand clips, and the rest of the time in the longer ones.
        assert_eq!(picked.iter().filter(|&&i| i % 3 == 0).count(), 10_000);
        assert!(value >= 10_000.0 * 1e9 + 940.0 * 2e9, "{value}");
    }

    #[test]
    fn a_large_library_is_picked_from_in_a_moment() {
        let items: Vec<(f64, f64)> = (0..30_000_u64).map(|i| (1e8 + (i * 7919 % 1000) as f64 * 5e6, 300.0 + (i * 104_729 % 5000) as f64)).collect();
        let picked = best_within(&items, 8.0 * 3600.0);
        let (_, secs) = totals(&items, &picked);
        assert!(secs <= 8.0 * 3600.0 && secs > 7.9 * 3600.0, "{secs}");
        // A moment is counted in steps, which a busy machine does not change: a step per video
        // and slot, tens of millions at most for any library of that kind of size.
        for videos in [1, 100, 4_000, 30_000, 40_000] {
            assert!(videos * slots_for(videos) <= 40_000_000, "{videos} videos");
        }
        assert_eq!(slots_for(1_000_000), 1000, "never so coarse that the rounding wastes the time");
    }
}
