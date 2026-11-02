mod batch;
mod encode;
mod estimate;
mod events;
mod fsguard;
mod history;
mod probe;
mod report;
mod scan;
mod scenes;
mod search;
mod tools;

use anyhow::Result;
use clap::{Parser, Subcommand};
use scan::is_video;
use std::path::{Path, PathBuf};


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
        /// VMAF target the estimates assume (past encodes for nearby targets calibrate them).
        #[arg(long, default_value_t = 93.0)]
        target_vmaf: f64,
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
        /// Choose the CRF per scene: split at scene changes, target VMAF for each chunk, then join.
        /// Evens out quality across the video; slower than one CRF for the whole file.
        #[arg(long)]
        scenes: bool,
        /// Chunks encoded in parallel with --scenes. Each ffmpeg gets its share of --memory-limit.
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=8))]
        workers: u8,
        /// Memory cap for each ffmpeg run (systemd MemoryMax syntax, e.g. 12G). "auto" = 40% of RAM, "none" = no cap.
        #[arg(long, default_value = "auto")]
        memory_limit: String,
        /// CPU priority for encodes (0-19, higher = more polite to the desktop).
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(i32).range(0..=19))]
        nice: i32,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Scan { paths, ffprobe, jobs, min_savings, csv, top, target_vmaf } => {
            let model = estimate::Model::fit(&history::load(), target_vmaf);
            // Absolute roots keep cache keys stable however the folder was typed.
            let paths: Vec<PathBuf> = paths.iter().map(|p| std::path::absolute(p).unwrap_or_else(|_| p.clone())).collect();
            scan(&paths, &ffprobe, jobs, min_savings, csv.as_deref(), top, &model)
        }
        Cmd::Encode {
            inputs, out_dir, container, target_vmaf, preset, min_crf, max_crf, samples, sample_secs,
            min_savings, force, overwrite, retry_skipped, keep_temp, allow_zfs, ffmpeg, ffprobe, scenes, workers, memory_limit, nice,
        } => {
            if min_crf > max_crf {
                anyhow::bail!("--min-crf {min_crf} is above --max-crf {max_crf}");
            }
            fsguard::ensure_writable_location(&out_dir, allow_zfs)?;
            std::fs::create_dir_all(&out_dir)?;
            // SAFETY: plain syscall on our own process; children inherit the priority.
            unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) };
            let tools = tools::Tools::new(ffmpeg, ffprobe, memory_cap(&memory_limit, if scenes { workers } else { 1 }));
            let opts = encode::Options {
                container, target_vmaf, preset, min_crf, max_crf, samples, sample_secs,
                min_savings, force, overwrite, keep_temp, scenes, workers: workers.into(),
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

/// Per-ffmpeg memory cap: the configured total, split between parallel workers.
fn memory_cap(setting: &str, workers: u8) -> Option<String> {
    let mb = match setting {
        "none" => return None,
        "auto" => std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|m| m.lines().find(|l| l.starts_with("MemTotal:"))?.split_whitespace().nth(1)?.parse::<u64>().ok())
            .map(|kb| kb * 4 / 10 / 1024)?,
        s if workers <= 1 => return Some(s.to_string()),
        s => {
            let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
            let n: u64 = num.parse().ok()?;
            match unit.to_ascii_uppercase().as_str() {
                "" => n / 1024 / 1024,
                "K" => n / 1024,
                "M" => n,
                "G" => n * 1024,
                "T" => n * 1024 * 1024,
                _ => return Some(s.to_string()),
            }
        }
    };
    Some(format!("{}M", mb / workers.max(1) as u64))
}

fn scan(paths: &[PathBuf], ffprobe: &str, jobs: usize, min_savings: f64, csv: Option<&Path>, top: usize, model: &estimate::Model) -> Result<()> {
    let progress = |n: usize, total: usize| {
        if n == 0 {
            eprintln!("found {total} video files");
        } else if n.is_multiple_of(250) || n == total {
            eprint!("\rprobed {n}/{total}");
        }
    };
    let (results, stats) = scan::collect(paths, ffprobe, jobs, min_savings, model, &progress)?;
    eprintln!();
    if stats.dropped > 0 {
        eprintln!("dropped {} cache entries for files that no longer exist", stats.dropped);
    }
    eprintln!("probed {} new files, {} from cache", stats.fresh, stats.cached);

    if model.samples == 0 {
        eprintln!("estimates: built-in pilot model (no encodes in the history yet; `encode` improves them)");
    } else {
        eprintln!("estimates: learned from {} past encodes (spread x{:.2})", model.samples, (1.28 * model.sigma).exp());
    }
    report::print(&results, top);
    if let Some(csv) = csv {
        report::write_csv(&results, csv)?;
        eprintln!("wrote {}", csv.display());
    }
    Ok(())
}
