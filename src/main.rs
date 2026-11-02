mod batch;
mod encode;
mod estimate;
mod fsguard;
mod probe;
mod report;
mod search;
mod tools;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use estimate::{SizeRange, Verdict};
use probe::MediaInfo;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

const VIDEO_EXTS: &[&str] = &[
    "mp4", "mkv", "m4v", "mov", "webm", "avi", "wmv", "flv", "ts", "m2ts", "mpg", "mpeg", "3gp",
];
const SKIP_DIRS: &[&str] = &["System Volume Information", "$RECYCLE.BIN", "lost+found"];

#[derive(Parser)]
#[command(version, about = "Find videos worth re-encoding to AV1 and estimate the savings")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Probe every video under the given folders and report what re-encoding would save.
    /// Read-only: never modifies, moves or creates files in the scanned folders.
    Scan {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// ffprobe binary to use.
        #[arg(long, env = "AV1OPT_FFPROBE", default_value = "ffprobe")]
        ffprobe: String,
        /// Parallel ffprobe processes. Keep low on slow or FUSE-mounted drives.
        #[arg(short, long, default_value_t = 6)]
        jobs: usize,
        /// Minimum expected savings (0-1) for a file to count as a candidate.
        #[arg(long, default_value_t = 0.2)]
        min_savings: f64,
        /// Write one row per file to this CSV.
        #[arg(long)]
        csv: Option<PathBuf>,
        /// Number of top candidates to list.
        #[arg(long, default_value_t = 15)]
        top: usize,
    },
    /// Re-encode videos to AV1 at a target quality. Inputs are never modified; results go to --out-dir.
    /// Folders are processed recursively and their layout is mirrored. Re-running resumes.
    Encode {
        /// Video files and/or folders.
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        /// Folder for finished files (plus a .av1opt.json report each). Must not be on ZFS.
        #[arg(short, long)]
        out_dir: PathBuf,
        #[arg(long, value_enum, default_value_t = encode::Container::Mp4)]
        container: encode::Container,
        /// Mean VMAF the samples must reach. 93 is hard to tell apart from an already-lossy source.
        #[arg(long, default_value_t = 93.0)]
        target_vmaf: f64,
        /// SVT-AV1 preset: lower is slower and smaller.
        #[arg(long, default_value_t = 6, value_parser = clap::value_parser!(u8).range(0..=13))]
        preset: u8,
        #[arg(long, default_value_t = 20)]
        min_crf: u8,
        #[arg(long, default_value_t = 55, value_parser = clap::value_parser!(u8).range(1..=63))]
        max_crf: u8,
        /// Number of sample clips used by the CRF search.
        #[arg(long, default_value_t = 4)]
        samples: usize,
        #[arg(long, default_value_t = 8.0)]
        sample_secs: f64,
        /// Skip the full encode if predicted video savings are below this (0-1).
        #[arg(long, default_value_t = 0.15)]
        min_savings: f64,
        /// Encode even if already AV1 or below --min-savings.
        #[arg(long)]
        force: bool,
        #[arg(long)]
        overwrite: bool,
        /// Retry files an earlier run skipped (low savings, unreachable target, ...).
        #[arg(long)]
        retry_skipped: bool,
        /// Keep sample and partial files for debugging.
        #[arg(long)]
        keep_temp: bool,
        /// Allow writing output to ZFS.
        #[arg(long)]
        allow_zfs: bool,
        #[arg(long, env = "AV1OPT_FFMPEG", default_value = "ffmpeg")]
        ffmpeg: String,
        #[arg(long, env = "AV1OPT_FFPROBE", default_value = "ffprobe")]
        ffprobe: String,
        /// Memory cap for each ffmpeg run (systemd MemoryMax syntax, e.g. 12G). "auto" = 40% of RAM, "none" = no cap.
        #[arg(long, default_value = "auto")]
        memory_limit: String,
        /// CPU priority for encodes (0-19, higher = more polite to the desktop).
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(i32).range(0..=19))]
        nice: i32,
    },
}

/// Probe results are cached by path, size and mtime so rescans of big libraries are fast.
#[derive(Serialize, Deserialize, Clone)]
struct CacheEntry {
    size: u64,
    mtime: u64,
    info: Option<MediaInfo>,
    error: Option<String>,
}

pub struct FileResult {
    pub path: PathBuf,
    pub root: PathBuf,
    pub size: u64,
    pub info: Option<MediaInfo>,
    pub error: Option<String>,
    pub verdict: Verdict,
    pub estimate: SizeRange,
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Scan { paths, ffprobe, jobs, min_savings, csv, top } => {
            scan(&paths, &ffprobe, jobs, min_savings, csv.as_deref(), top)
        }
        Cmd::Encode {
            inputs, out_dir, container, target_vmaf, preset, min_crf, max_crf, samples, sample_secs,
            min_savings, force, overwrite, retry_skipped, keep_temp, allow_zfs, ffmpeg, ffprobe, memory_limit, nice,
        } => {
            if min_crf > max_crf {
                anyhow::bail!("--min-crf {min_crf} is above --max-crf {max_crf}");
            }
            fsguard::ensure_writable_location(&out_dir, allow_zfs)?;
            std::fs::create_dir_all(&out_dir)?;
            // SAFETY: plain syscall on our own process; children inherit the priority.
            unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) };
            let tools = tools::Tools::new(ffmpeg, ffprobe, memory_cap(&memory_limit));
            let opts = encode::Options {
                container, target_vmaf, preset, min_crf, max_crf, samples, sample_secs,
                min_savings, force, overwrite, keep_temp,
            };
            let jobs = batch::expand(&inputs, &out_dir, is_video);
            if jobs.is_empty() {
                anyhow::bail!("no video files found in the given inputs");
            }
            let s = batch::run(&tools, &jobs, &out_dir, &opts, retry_skipped)?;
            let saved = s.bytes_in.saturating_sub(s.bytes_out) as f64 / 1e9;
            eprintln!(
                "\nencoded {}, skipped {}, failed {}, already done {} | {:.2} GB -> {:.2} GB ({saved:.2} GB saved)",
                s.ok, s.skipped, s.failed, s.already_done, s.bytes_in as f64 / 1e9, s.bytes_out as f64 / 1e9
            );
            eprintln!("log: {}", out_dir.join(batch::LOG_NAME).display());
            if s.failed > 0 {
                std::process::exit(2);
            }
            Ok(())
        }
    }
}

fn memory_cap(setting: &str) -> Option<String> {
    match setting {
        "none" => None,
        "auto" => std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|m| m.lines().find(|l| l.starts_with("MemTotal:"))?.split_whitespace().nth(1)?.parse::<u64>().ok())
            .map(|kb| format!("{}M", kb * 4 / 10 / 1024)),
        s => Some(s.to_string()),
    }
}

fn scan(paths: &[PathBuf], ffprobe: &str, jobs: usize, min_savings: f64, csv: Option<&Path>, top: usize) -> Result<()> {
    rayon::ThreadPoolBuilder::new().num_threads(jobs).build_global()?;

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
    eprintln!("found {} video files", files.len());

    let cache_path = cache_file()?;
    let cache: HashMap<PathBuf, CacheEntry> = std::fs::read(&cache_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let cache = Mutex::new(cache);
    let done = AtomicUsize::new(0);
    let fresh = AtomicUsize::new(0);
    let total = files.len();

    let results: Vec<FileResult> = files
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
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_multiple_of(250) || n == total {
                eprint!("\rprobed {n}/{total}");
            }
            let (verdict, estimate) = match &entry.info {
                Some(i) => estimate::judge(i, *size, min_savings),
                None => (Verdict::Unreadable, SizeRange { low: *size as f64, likely: *size as f64, high: *size as f64 }),
            };
            FileResult { path: path.clone(), root: root.clone(), size: *size, info: entry.info, error: entry.error, verdict, estimate }
        })
        .collect();
    eprintln!();
    save_cache(&cache_path, &cache.lock().unwrap())?;
    eprintln!("probed {} new files, {} from cache", fresh.load(Ordering::Relaxed), total - fresh.load(Ordering::Relaxed));

    report::print(&results, top);
    if let Some(csv) = csv {
        report::write_csv(&results, csv)?;
        eprintln!("wrote {}", csv.display());
    }
    Ok(())
}

fn is_video(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| VIDEO_EXTS.contains(&e.to_ascii_lowercase().as_str()))
}

fn cache_file() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .context("no HOME or XDG_CACHE_HOME")?;
    let dir = base.join("av1opt");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("probe-cache.json"))
}

fn save_cache(path: &Path, cache: &HashMap<PathBuf, CacheEntry>) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(cache)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
