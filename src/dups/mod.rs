//! Finds videos that show the same thing: the same video at another resolution, bitrate,
//! codec or container, with or without black bars, and copies cut at the start or the end.
//! Read-only like `scan`: a few frames of every video are decoded and reduced to perceptual
//! hashes (cached), then all videos are compared and grouped.

pub mod matching;
pub mod phash;

use crate::estimate::Model;
use crate::events::Cancelled;
use crate::probe::MediaInfo;
use crate::scan;
use crate::tools::{Tools, hms, path_arg};
use anyhow::{Context, Result, bail};
use matching::{Fingerprint, Frame, Group, Kind, MIDDLE, Media, Prefs, ladder};
use phash::THUMB;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

/// Most bits (of 63) two frame hashes may differ by and still count as the same picture.
pub const DEFAULT_MAX_DIST: u32 = 10;

pub struct Options {
    /// Videos read at once (ffprobe, then ffmpeg).
    pub jobs: usize,
    pub max_dist: u32,
    pub prefs: Prefs,
    /// Fingerprint every video again instead of using the cache.
    pub refresh: bool,
}

/// What the run is doing, for progress displays.
#[derive(Clone, Copy, Debug)]
pub enum Progress {
    Probing { done: usize, total: usize },
    Fingerprinting { done: usize, total: usize },
    Comparing { videos: usize },
}

#[derive(Serialize, Clone, Debug)]
pub struct File {
    pub path: PathBuf,
    pub root: PathBuf,
    pub size: u64,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub duration_s: f64,
    pub video_mbps: f64,
    /// Files of a group with the same number here have the same bytes (sampled, not read in full).
    pub identical: Option<u32>,
    /// Modification time (seconds since 1970) the search saw, for [`File::unchanged`].
    #[serde(skip)]
    #[cfg_attr(not(feature = "ui"), allow(dead_code))]
    pub modified: u64,
    /// Sampled digest of the contents at the end of the search, None if they could not be read.
    #[serde(skip)]
    #[cfg_attr(not(feature = "ui"), allow(dead_code))]
    pub digest: Option<u64>,
}

impl File {
    /// Whether this is still the file the search saw: its size, its date and the bytes sampled
    /// at its start, middle and end. The size alone does not tell: two copies of one size can
    /// have changed places, or one been overwritten.
    #[cfg(feature = "ui")]
    pub fn unchanged(&self) -> bool {
        let Ok(meta) = std::fs::symlink_metadata(&self.path) else { return false };
        let modified = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
        meta.is_file() && meta.len() == self.size && modified == self.modified && self.digest.is_some() && crate::files::sample_digest(&self.path, self.size, IDENTICAL_SAMPLE).ok() == self.digest
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct Unreadable {
    pub path: PathBuf,
    pub error: String,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Stats {
    /// Video files found.
    pub videos: usize,
    /// Videos compared (readable, with at least two usable frames).
    pub compared: usize,
    /// Fingerprints made in this run; the rest came from the cache.
    pub fresh: usize,
    pub same_groups: usize,
    pub partial_groups: usize,
    /// Files that are a spare copy in a whole-video group, and their total size.
    pub spare_files: usize,
    pub spare_bytes: u64,
    /// Videos left out because their paths are not text.
    pub not_text: usize,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Report {
    /// Only the files that are in a group; `Group::members` index into this.
    pub files: Vec<File>,
    pub groups: Vec<Group>,
    /// Videos that could not be compared.
    pub unreadable: Vec<Unreadable>,
    pub stats: Stats,
}

#[derive(Serialize, Deserialize, Clone)]
struct CacheEntry {
    size: u64,
    mtime: u64,
    /// The frame times depend on the length, so a different probe result needs new frames.
    duration: f64,
    fp: Option<Fingerprint>,
    error: Option<String>,
}

fn cache_file() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .context("no HOME or XDG_CACHE_HOME")?;
    let dir = base.join("av1opt");
    crate::files::private_folder(&dir)?;
    // Bump the version when the frame times or the hash change, so old entries are redone.
    // Version 1 has no frames from the middle of a video.
    let _ = std::fs::remove_file(dir.join("dups-cache-v1.json"));
    Ok(dir.join("dups-cache-v2.json"))
}

fn save_cache(path: &Path, cache: &HashMap<PathBuf, CacheEntry>) -> Result<()> {
    Ok(crate::files::replace_with(path, &serde_json::to_vec(cache)?)?)
}

/// One ffmpeg run that seeks to each of `times` and returns the frames as square grayscale
/// thumbnails, or None if any of them is missing.
fn grab_batch(tools: &Tools, path: &Path, video_index: usize, times: &[f64]) -> Option<Vec<Vec<u8>>> {
    let mut a: Vec<String> = vec!["-v".into(), "error".into()];
    let mut graph = String::new();
    for (i, t) in times.iter().enumerate() {
        // One decoder thread per seek keeps memory and CPU bounded; the loop filter and
        // non-reference frames don't matter for a thumbnail this small.
        a.extend(["-threads", "1", "-skip_loop_filter", "all", "-skip_frame", "noref", "-ss"].map(String::from));
        a.extend([format!("{t:.3}"), "-i".into(), path_arg(path)]);
        graph += &format!("[{i}:{video_index}]trim=end_frame=1,scale={THUMB}:{THUMB}:flags=area,format=gray,setpts=0[v{i}];");
    }
    if times.len() == 1 {
        graph = graph.trim_end_matches("[v0];").to_string();
    } else {
        graph += &(0..times.len()).map(|i| format!("[v{i}]")).collect::<String>();
        graph += &format!("vstack=inputs={}", times.len());
    }
    a.extend(["-filter_complex".into(), graph, "-frames:v".into(), "1".into(), "-f".into(), "rawvideo".into(), "-".into()]);
    let out = tools.run_capture(&a, Duration::from_secs(90)).ok()?;
    (out.len() == times.len() * THUMB * THUMB).then(|| out.chunks(THUMB * THUMB).map(<[u8]>::to_vec).collect())
}

/// Thumbnails of the frames at `times`; None where there is no frame (past the end of the
/// video stream, damaged data).
fn grab(tools: &Tools, path: &Path, info: &MediaInfo, times: &[f64]) -> Result<Vec<Option<Vec<u8>>>> {
    // Every seek holds a decoder open; big frames get smaller batches.
    let per_run = if info.width as u64 * info.height as u64 > 2_500_000 { 3 } else { 8 };
    let mut out = Vec::with_capacity(times.len());
    for batch in times.chunks(per_run) {
        tools.check_cancel()?;
        match grab_batch(tools, path, info.video_index, batch) {
            Some(frames) => out.extend(frames.into_iter().map(Some)),
            // One missing frame fails the whole run: ask for each on its own.
            None if batch.len() > 1 => {
                for &t in batch {
                    tools.check_cancel()?;
                    out.push(grab_batch(tools, path, info.video_index, &[t]).and_then(|mut f| f.pop()));
                }
            }
            None => out.push(None),
        }
    }
    tools.check_cancel()?;
    Ok(out)
}

/// Decodes the sampled frames of one video and hashes them. The times are those of the picture:
/// sound that starts earlier or runs on has no frames to show.
pub fn fingerprint(tools: &Tools, path: &Path, info: &MediaInfo) -> Result<Fingerprint> {
    let (start, length) = (info.picture_start(), info.picture_secs());
    let lad = ladder(length);
    let from_end = lad.iter().map(|t| (length - t).max(0.0));
    let at_shares = MIDDLE.iter().map(|share| length * share);
    let times: Vec<f64> = lad.iter().copied().chain(from_end).chain(at_shares).map(|t| start + t).collect();
    let frames = grab(tools, path, info, &times)?;
    let read: Vec<&[u8]> = frames.iter().flatten().map(Vec::as_slice).collect();
    if read.is_empty() {
        bail!("no frame could be read");
    }
    let bounds = phash::content_bounds(&read);
    let hashes: Vec<Frame> = frames.iter().map(|f| f.as_ref().map_or(Frame::Unread, |f| phash::hash(f, bounds).map_or(Frame::Flat, Frame::Picture))).collect();
    let (head, rest) = hashes.split_at(lad.len());
    let (tail, middle) = rest.split_at(lad.len());
    Ok(Fingerprint { head: head.to_vec(), tail: tail.to_vec(), middle: middle.to_vec() })
}

/// How much of a file's start, middle and end is compared.
const IDENTICAL_SAMPLE: u64 = 64 * 1024;

/// Numbers the sets of files that have the same size and the same sampled bytes.
fn mark_identical(media: &mut [Media], linked: &[usize]) {
    let mut by_size: HashMap<u64, Vec<usize>> = HashMap::new();
    for &i in linked {
        by_size.entry(media[i].size).or_default().push(i);
    }
    let mut next = 0;
    let mut sizes: Vec<&u64> = by_size.keys().collect();
    sizes.sort();
    for size in sizes {
        let set = &by_size[size];
        if set.len() < 2 {
            continue;
        }
        let mut by_digest: HashMap<u64, Vec<usize>> = HashMap::new();
        for &i in set {
            if let Ok(d) = crate::files::sample_digest(&media[i].path, *size, IDENTICAL_SAMPLE) {
                by_digest.entry(d).or_default().push(i);
            }
        }
        let mut sets: Vec<&Vec<usize>> = by_digest.values().filter(|v| v.len() > 1).collect();
        sets.sort();
        for same in sets {
            for &i in same {
                media[i].identical = Some(next);
            }
            next += 1;
        }
    }
}

/// Finds the groups of videos under `paths` (absolute) that show the same thing.
pub fn find(paths: &[PathBuf], tools: &Tools, o: &Options, progress: &(dyn Fn(Progress) + Sync)) -> Result<Report> {
    // The verdicts of `scan` aren't used here, so the built-in size model will do.
    let model = Model::fit(&[], 93.0);
    let (mut found, _) = scan::collect(paths, &tools.ffprobe, o.jobs, 0.2, &model, &|done, total| progress(Progress::Probing { done, total }))?;
    tools.check_cancel()?;
    let not_text = scan::drop_unnamed(&mut found);
    if not_text > 0 {
        eprintln!("note: {not_text} videos left out: {}", crate::tools::NOT_TEXT);
    }

    let mut unreadable: Vec<Unreadable> = Vec::new();
    let mut videos: Vec<(&scan::FileResult, &MediaInfo)> = Vec::new();
    for f in &found {
        match &f.info {
            Some(i) if i.duration_s > 0.0 => videos.push((f, i)),
            Some(_) => unreadable.push(Unreadable { path: f.path.clone(), error: "unknown length".into() }),
            None => unreadable.push(Unreadable { path: f.path.clone(), error: f.error.clone().unwrap_or_else(|| "unreadable".into()) }),
        }
    }

    let cache_path = cache_file()?;
    // Two runs at once (UI) must not interleave their read-modify-write of the cache file.
    static CACHE_LOCK: Mutex<()> = Mutex::new(());
    let _guard = CACHE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut cache: HashMap<PathBuf, CacheEntry> = std::fs::read(&cache_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    // Forget files that were under these folders last time but are gone now.
    {
        let here: std::collections::HashSet<&Path> = found.iter().map(|f| f.path.as_path()).collect();
        cache.retain(|p, _| here.contains(p.as_path()) || !paths.iter().any(|root| p.starts_with(root)));
    }
    let cache = Mutex::new(cache);
    let (done, fresh) = (AtomicUsize::new(0), AtomicUsize::new(0));
    let cancelled = AtomicBool::new(false);
    let total = videos.len();
    progress(Progress::Fingerprinting { done: 0, total });

    let pool = rayon::ThreadPoolBuilder::new().num_threads(o.jobs.max(1)).build()?;
    let prints: Vec<Option<CacheEntry>> = pool.install(|| {
        videos
            .par_iter()
            .map(|(f, info)| {
                if cancelled.load(Ordering::Relaxed) {
                    return None;
                }
                let cached = (!o.refresh)
                    .then(|| cache.lock().unwrap().get(&f.path).filter(|c| c.size == f.size && c.mtime == f.mtime && (c.duration - info.duration_s).abs() < 0.01).cloned())
                    .flatten();
                let entry = match cached {
                    Some(e) => e,
                    None => {
                        let (fp, error) = match fingerprint(tools, &f.path, info) {
                            Ok(fp) => (Some(fp), None),
                            Err(e) if crate::events::is_cancelled(&e) => {
                                cancelled.store(true, Ordering::Relaxed);
                                return None;
                            }
                            Err(e) => (None, Some(format!("{e:#}"))),
                        };
                        let e = CacheEntry { size: f.size, mtime: f.mtime, duration: info.duration_s, fp, error };
                        let mut c = cache.lock().unwrap();
                        c.insert(f.path.clone(), e.clone());
                        // Save now and then so a crash doesn't lose hours of decoding.
                        if fresh.fetch_add(1, Ordering::Relaxed) % 200 == 199 {
                            let _ = save_cache(&cache_path, &c);
                        }
                        e
                    }
                };
                progress(Progress::Fingerprinting { done: done.fetch_add(1, Ordering::Relaxed) + 1, total });
                Some(entry)
            })
            .collect()
    });
    save_cache(&cache_path, &cache.lock().unwrap())?;
    if cancelled.load(Ordering::Relaxed) {
        return Err(Cancelled.into());
    }

    // Two usable frames are the least that can match; fewer says nothing.
    let mut media: Vec<Media> = Vec::new();
    let mut kept: Vec<(&scan::FileResult, &MediaInfo, Fingerprint)> = Vec::new();
    for ((f, info), entry) in videos.iter().zip(prints) {
        match entry {
            Some(CacheEntry { fp: Some(fp), .. }) if fp.usable() >= 2 => {
                media.push(Media {
                    path: f.path.clone(),
                    size: f.size,
                    codec: info.codec.clone(),
                    width: info.width,
                    height: info.height,
                    duration_s: info.duration_s,
                    video_bitrate: info.video_bitrate,
                    identical: None,
                });
                kept.push((f, info, fp));
            }
            Some(CacheEntry { fp: Some(_), .. }) => unreadable.push(Unreadable { path: f.path.clone(), error: "too few frames with detail to compare".into() }),
            Some(e) => unreadable.push(Unreadable { path: f.path.clone(), error: e.error.unwrap_or_else(|| "no fingerprint".into()) }),
            None => {}
        }
    }
    progress(Progress::Comparing { videos: kept.len() });
    let items: Vec<(f64, &Fingerprint)> = kept.iter().map(|(_, info, fp)| (info.picture_secs(), fp)).collect();
    let links = matching::links(&items, o.max_dist);
    let mut linked: Vec<usize> = links.iter().filter(|l| l.kind == Kind::Same).flat_map(|l| [l.a, l.b]).collect();
    linked.sort_unstable();
    linked.dedup();
    mark_identical(&mut media, &linked);
    let mut groups = matching::groups(&links, &media, &o.prefs);

    // Report only the files that ended up in a group.
    let mut index: HashMap<usize, usize> = HashMap::new();
    let mut files: Vec<File> = Vec::new();
    for g in &mut groups {
        for m in &mut g.members {
            *m = *index.entry(*m).or_insert_with(|| {
                let (f, info, _) = &kept[*m];
                files.push(File {
                    path: f.path.clone(),
                    root: f.root.clone(),
                    size: f.size,
                    codec: info.codec.clone(),
                    width: info.width,
                    height: info.height,
                    fps: info.fps,
                    duration_s: info.duration_s,
                    video_mbps: info.video_bitrate / 1e6,
                    identical: media[*m].identical,
                    modified: f.mtime,
                    digest: None,
                });
                files.len() - 1
            });
        }
    }
    // What the page checks again before it moves anything; only files in a group are read.
    pool.install(|| files.par_iter_mut().for_each(|f| f.digest = crate::files::sample_digest(&f.path, f.size, IDENTICAL_SAMPLE).ok()));
    let same: Vec<&Group> = groups.iter().filter(|g| g.kind == Kind::Same).collect();
    let stats = Stats {
        videos: found.len(),
        compared: kept.len(),
        fresh: fresh.load(Ordering::Relaxed),
        same_groups: same.len(),
        partial_groups: groups.len() - same.len(),
        spare_files: same.iter().map(|g| g.members.len() - 1).sum(),
        spare_bytes: same.iter().flat_map(|g| &g.members[1..]).map(|&i| files[i].size).sum(),
        not_text,
    };
    unreadable.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Report { files, groups, unreadable, stats })
}

pub fn kind_label(k: Kind) -> &'static str {
    match k {
        Kind::Same => "same video",
        Kind::SameStart => "same start",
        Kind::SameEnd => "same ending",
    }
}

fn gb(bytes: u64) -> String {
    if bytes >= 1_000_000_000 { format!("{:.2} GB", bytes as f64 / 1e9) } else { format!("{:.1} MB", bytes as f64 / 1e6) }
}

/// Prints the groups, the suggested copy first.
pub fn print(r: &Report, top: usize) {
    let s = &r.stats;
    println!("{} videos, {} compared ({} fingerprinted now, the rest from the cache)", s.videos, s.compared, s.fresh);
    if !r.unreadable.is_empty() {
        println!("{} could not be compared (listed in --json / --csv output)", r.unreadable.len());
    }
    println!(
        "{} group{} of the same video: {} spare cop{}, {} | {} pair{} sharing only a start or an ending",
        s.same_groups,
        if s.same_groups == 1 { "" } else { "s" },
        s.spare_files,
        if s.spare_files == 1 { "y" } else { "ies" },
        gb(s.spare_bytes),
        s.partial_groups,
        if s.partial_groups == 1 { "" } else { "s" },
    );
    for (n, g) in r.groups.iter().take(top).enumerate() {
        println!("\n#{} {} ({:.0}% of the compared frames match)", n + 1, kind_label(g.kind), g.score * 100.0);
        for (k, &i) in g.members.iter().enumerate() {
            let f = &r.files[i];
            let role = role(r, g, k);
            println!(
                "  {role:<5} {:>9} {:<5} {:>5.1} Mbps {:>8} {:>10}  {}{}",
                format!("{}x{}", f.width, f.height),
                f.codec,
                f.video_mbps,
                hms(f.duration_s),
                gb(f.size),
                f.path.display(),
                if f.identical.is_some() { "  [identical bytes]" } else { "" },
            );
        }
        if g.kind == Kind::Same && !g.reason.is_empty() {
            println!("  keep because: {}", g.reason);
        }
    }
    if r.groups.len() > top {
        println!("\n... and {} more groups (--top N shows more; --csv / --json list all)", r.groups.len() - top);
    }
}

/// One row per file of every group.
/// What a member of a group is called in the listing and the CSV. Only copies of one whole
/// video have one to keep and others to spare; videos that share a beginning or an ending
/// are different videos, told apart by length, and none of them is called spare (a list
/// acted on by its "spare" rows would delete a video that exists nowhere else).
fn role(r: &Report, g: &Group, k: usize) -> &'static str {
    let secs = |i: usize| r.files[i].duration_s;
    match g.kind {
        Kind::Same if k == 0 => "keep",
        Kind::Same => "spare",
        _ if g.members.iter().all(|&i| secs(i) <= secs(g.members[k])) => "long",
        _ => "short",
    }
}

const CSV_COLUMNS: &[&str] = &["group", "kind", "match", "role", "reason", "path", "size_bytes", "codec", "width", "height", "duration_s", "video_mbps", "identical"];
/// The CSV of a duplicate search, as a report file.
pub const CSV: crate::report_file::Kind = crate::report_file::Kind::Csv(CSV_COLUMNS);

pub fn write_csv(r: &Report, path: &Path, given: &[PathBuf]) -> Result<()> {
    let mut w = csv::Writer::from_writer(Vec::new());
    w.write_record(CSV_COLUMNS)?;
    for (n, g) in r.groups.iter().enumerate() {
        for (k, &i) in g.members.iter().enumerate() {
            let f = &r.files[i];
            w.write_record([
                (n + 1).to_string(),
                kind_label(g.kind).to_string(),
                format!("{:.2}", g.score),
                role(r, g, k).to_string(),
                if k == 0 { g.reason.clone() } else { String::new() },
                f.path.display().to_string(),
                f.size.to_string(),
                f.codec.clone(),
                f.width.to_string(),
                f.height.to_string(),
                format!("{:.2}", f.duration_s),
                format!("{:.3}", f.video_mbps),
                f.identical.map(|x| x.to_string()).unwrap_or_default(),
            ])?;
        }
    }
    for u in &r.unreadable {
        let mut row = vec![String::new(); 13];
        row[1] = "not compared".into();
        row[4] = crate::probe::one_line(&u.error).to_string();
        row[5] = u.path.display().to_string();
        w.write_record(&row)?;
    }
    crate::report_file::write_csv(w, path, CSV, given)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampled_digest_tells_files_apart() {
        let dir = std::env::temp_dir().join(format!("av1opt-dups-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let big = |fill: u8, mid: u8| {
            let mut v = vec![fill; 300_000];
            v[150_000] = mid;
            v
        };
        let write = |name: &str, data: &[u8]| {
            let p = dir.join(name);
            std::fs::write(&p, data).unwrap();
            p
        };
        let (a, b, c, d) = (write("a", &big(1, 9)), write("b", &big(1, 9)), write("c", &big(1, 8)), write("d", b"tiny"));
        let size = 300_000;
        let sample_digest = |path: &Path, size| crate::files::sample_digest(path, size, IDENTICAL_SAMPLE).ok();
        assert_eq!(sample_digest(&a, size), sample_digest(&b, size));
        assert_ne!(sample_digest(&a, size), sample_digest(&c, size));
        assert!(sample_digest(&d, 4).is_some());

        let m = |p: &Path, size: u64| Media { path: p.into(), size, codec: "h264".into(), width: 1, height: 1, duration_s: 1.0, video_bitrate: 1.0, identical: None };
        let mut media = vec![m(&a, size), m(&b, size), m(&c, size), m(&d, 4)];
        mark_identical(&mut media, &[0, 1, 2, 3]);
        assert_eq!(media.iter().map(|x| x.identical).collect::<Vec<_>>(), vec![Some(0), Some(0), None, None]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn report(kind: Kind, secs: &[f64]) -> (Report, Group) {
        let file = |i: usize, d: f64| File {
            path: format!("/v/{i}, été.mp4").into(),
            root: "/v".into(),
            size: 1000 + i as u64,
            codec: "h264".into(),
            width: 640,
            height: 360,
            fps: 24.0,
            duration_s: d,
            video_mbps: 1.5,
            identical: (i == 0).then_some(4),
            modified: 0,
            digest: None,
        };
        let g = Group { kind, score: 0.875, members: (0..secs.len()).collect(), reason: "largest file".into() };
        let r = Report { files: secs.iter().enumerate().map(|(i, &d)| file(i, d)).collect(), groups: vec![g.clone()], ..Default::default() };
        (r, g)
    }

    #[test]
    fn only_copies_of_one_whole_video_are_called_spare() {
        let (r, g) = report(Kind::Same, &[10.0, 10.0, 9.9]);
        assert_eq!((0..3).map(|k| role(&r, &g, k)).collect::<Vec<_>>(), ["keep", "spare", "spare"]);
        for kind in [Kind::SameStart, Kind::SameEnd] {
            let (r, g) = report(kind, &[10.0, 90.0]);
            assert_eq!((0..2).map(|k| role(&r, &g, k)).collect::<Vec<_>>(), ["short", "long"]);
            // Of equal length, neither is the shorter.
            let (r, g) = report(kind, &[10.0, 10.0]);
            assert_eq!((0..2).map(|k| role(&r, &g, k)).collect::<Vec<_>>(), ["long", "long"]);
        }
    }

    #[test]
    fn sizes_are_written_in_megabytes_up_to_a_gigabyte() {
        assert_eq!(gb(0), "0.0 MB");
        assert_eq!(gb(999_999_999), "1000.0 MB");
        assert_eq!(gb(1_000_000_000), "1.00 GB");
        assert_eq!(gb(2_500_000_000), "2.50 GB");
    }

    #[test]
    fn the_kinds_have_the_names_the_listing_uses() {
        assert_eq!(kind_label(Kind::Same), "same video");
        assert_eq!(kind_label(Kind::SameStart), "same start");
        assert_eq!(kind_label(Kind::SameEnd), "same ending");
    }

    #[test]
    fn the_csv_has_one_row_per_file_and_the_reason_only_on_the_first() {
        let dir = std::env::temp_dir().join(format!("av1opt-dups-csv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dups.csv");
        let (mut r, _) = report(Kind::Same, &[10.0, 10.0]);
        r.unreadable.push(Unreadable { path: "/v/bad.mp4".into(), error: "first line\nmoov atom not found\n".into() });
        write_csv(&r, &path, &[]).unwrap();
        let rows: Vec<csv::StringRecord> = csv::Reader::from_path(&path).unwrap().records().map(Result::unwrap).collect();
        let head = csv::Reader::from_path(&path).unwrap().headers().unwrap().clone();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(head.len(), 13);
        let at = |row: &csv::StringRecord, name: &str| row[head.iter().position(|h| h == name).unwrap()].to_string();
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|row| row.len() == 13));
        assert_eq!((at(&rows[0], "group"), at(&rows[0], "kind"), at(&rows[0], "match")), ("1".into(), "same video".into(), "0.88".into()));
        assert_eq!((at(&rows[0], "role"), at(&rows[0], "reason")), ("keep".into(), "largest file".into()));
        assert_eq!((at(&rows[1], "role"), at(&rows[1], "reason")), ("spare".into(), String::new()));
        // A path with a comma and accents comes back whole.
        assert_eq!(at(&rows[1], "path"), "/v/1, été.mp4");
        assert_eq!((at(&rows[0], "identical"), at(&rows[1], "identical")), ("4".into(), String::new()));
        // A video that could not be compared says why, in its last line.
        assert_eq!((at(&rows[2], "kind"), at(&rows[2], "reason"), at(&rows[2], "path")), ("not compared".into(), "moov atom not found".into(), "/v/bad.mp4".into()));
    }

    #[test]
    fn only_files_of_one_size_and_the_same_samples_are_marked_identical() {
        let dir = std::env::temp_dir().join(format!("av1opt-dups-same-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, data: &[u8]| {
            let p = dir.join(name);
            std::fs::write(&p, data).unwrap();
            p
        };
        let (a, b, c, d) = (write("a", b"aaaa"), write("b", b"aaaa"), write("c", b"cccccc"), write("d", b"cccccc"));
        let m = |p: &Path, size: u64| Media { path: p.into(), size, codec: "h264".into(), width: 1, height: 1, duration_s: 1.0, video_bitrate: 1.0, identical: None };
        let mut media = vec![m(&c, 6), m(&a, 4), m(&d, 6), m(&b, 4), m(&dir.join("gone"), 4)];
        // Each set gets its own number, the smaller size first; a file that cannot be read has none.
        mark_identical(&mut media, &[0, 1, 2, 3, 4]);
        assert_eq!(media.iter().map(|x| x.identical).collect::<Vec<_>>(), vec![Some(1), Some(0), Some(1), Some(0), None]);
        // Files that are in no group are not looked at.
        for x in &mut media {
            x.identical = None;
        }
        mark_identical(&mut media, &[0, 1]);
        assert!(media.iter().all(|x| x.identical.is_none()));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
