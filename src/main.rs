mod batch;
mod bench;
mod dups;
mod encode;
mod estimate;
mod events;
mod fsguard;
mod history;
mod metadata;
mod probe;
mod report;
mod scan;
mod scenes;
mod search;
mod sections;
mod tools;
#[cfg(feature = "ui")]
mod trash;
mod tuning;
#[cfg(feature = "ui")]
mod ui;

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
    /// Find videos that show the same thing: the same video at another resolution, bitrate,
    /// codec or container, and copies cut at the start or the end. Read-only: decodes a few
    /// frames of every video (cached) and lists the groups with the copy worth keeping first.
    Dups {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Videos read at once. Keep low on slow or FUSE-mounted drives.
        #[arg(short, long, default_value_t = 6)]
        jobs: usize,
        /// Between equally good copies, keep the one under this folder. Repeatable; first wins.
        #[arg(long, value_name = "DIR")]
        prefer: Vec<PathBuf>,
        /// Never suggest keeping a copy whose path contains this text (e.g. trash) if another
        /// copy exists. Repeatable; upper and lower case count as the same.
        #[arg(long, value_name = "TEXT")]
        avoid: Vec<String>,
        /// How different two frames may be and still count as the same picture (0-20 of 63 bits).
        /// Lower finds fewer, surer matches.
        #[arg(long, default_value_t = dups::DEFAULT_MAX_DIST, value_parser = clap::value_parser!(u32).range(0..=20))]
        tolerance: u32,
        /// Fingerprint every video again instead of using the cache.
        #[arg(long)]
        refresh: bool,
        /// Number of groups to list.
        #[arg(long, default_value_t = 20)]
        top: usize,
        /// Write one row per file of every group to this CSV.
        #[arg(long)]
        csv: Option<PathBuf>,
        /// Write the full result to this JSON file.
        #[arg(long)]
        json: Option<PathBuf>,
        #[arg(long, env = "AV1OPT_FFMPEG", default_value = "ffmpeg")]
        ffmpeg: String,
        #[arg(long, env = "AV1OPT_FFPROBE", default_value = "ffprobe")]
        ffprobe: String,
        /// Memory cap shared by the ffmpeg runs, as for `encode`.
        #[arg(long, default_value = "auto")]
        memory_limit: String,
        /// CPU priority (0-19, higher = more polite to the desktop).
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(i32).range(0..=19))]
        nice: i32,
    },
    /// Compare encoder settings on your own videos: for every preset and SVT-AV1 parameter set,
    /// find the CRF that reaches the target VMAF on sample clips, then rank the settings by
    /// file size and encode time at that equal quality. Read-only for the videos; sample
    /// encodes are remembered, so a stopped run resumes.
    Bench {
        /// Videos and/or folders to take the test videos from.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Presets to try, e.g. 4,6,8.
        #[arg(long, value_delimiter = ',', default_values_t = bench::DEFAULT_PRESETS, value_parser = clap::value_parser!(u8).range(0..=13))]
        presets: Vec<u8>,
        /// An SVT-AV1 parameter set to try with every preset, as key=value:key=value.
        /// Repeatable. Default: tune=0, tune=1, tune=2, tune=0 with quantisation matrices,
        /// tune=0 with variance boost.
        #[arg(long = "params", value_name = "PARAMS", value_parser = search::parse_params)]
        param_sets: Vec<String>,
        #[arg(long, default_value_t = 93.0)]
        target_vmaf: f64,
        /// Use at most this many videos, spread over the folders and resolution classes.
        #[arg(long, default_value_t = 6)]
        max_videos: usize,
        /// Sample clips per video.
        #[arg(long, default_value_t = 2)]
        samples: usize,
        #[arg(long, default_value_t = 6.0)]
        sample_secs: f64,
        #[arg(long, default_value_t = 20)]
        min_crf: u8,
        #[arg(long, default_value_t = 55, value_parser = clap::value_parser!(u8).range(1..=63))]
        max_crf: u8,
        /// Measure everything again instead of using remembered sample encodes.
        #[arg(long)]
        refresh: bool,
        /// Folder for the lossless sample clips while a video is measured (a few GB for 4K).
        /// Default: ~/.cache/av1opt. Must not be on ZFS.
        #[arg(long)]
        work_dir: Option<PathBuf>,
        /// Make `encode --tuned` use the winners, per resolution class: balanced (smallest
        /// files at about the default's encode time), smallest or fastest.
        #[arg(long, value_name = "PICK", num_args = 0..=1, default_missing_value = "balanced")]
        save: Option<bench::Pick>,
        /// Write one row per video and setting to this CSV.
        #[arg(long)]
        csv: Option<PathBuf>,
        /// Write the full result to this JSON file.
        #[arg(long)]
        json: Option<PathBuf>,
        /// Parallel ffprobe processes while collecting the videos.
        #[arg(short, long, default_value_t = 6)]
        jobs: usize,
        #[arg(long, env = "AV1OPT_FFMPEG", default_value = "ffmpeg")]
        ffmpeg: String,
        #[arg(long, env = "AV1OPT_FFPROBE", default_value = "ffprobe")]
        ffprobe: String,
        /// Memory cap for each ffmpeg run, as for `encode`.
        #[arg(long, default_value = "auto")]
        memory_limit: String,
        /// CPU priority (0-19, higher = more polite to the desktop).
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(i32).range(0..=19))]
        nice: i32,
    },
    /// Show the settings `encode --tuned` uses (saved by `bench --save`), or forget them.
    Tuning {
        /// Forget the tuned settings.
        #[arg(long)]
        clear: bool,
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
        /// SVT-AV1 parameters besides the CRF, as key=value:key=value. Replaces the default
        /// (tune=0); note that SVT-AV1's own default is tune=1.
        #[arg(long, value_name = "PARAMS", default_value = search::DEFAULT_PARAMS, value_parser = search::parse_params)]
        svt_params: String,
        /// Use the preset and SVT-AV1 parameters `av1opt bench --save` picked for each
        /// resolution class; --preset and --svt-params apply to classes without a pick.
        #[arg(long)]
        tuned: bool,
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
        /// Drop the source's tags and chapters (stream languages and track names stay).
        #[arg(long)]
        strip_metadata: bool,
        /// Give each output the source's modification time.
        #[arg(long)]
        keep_dates: bool,
        /// Add tags from a preset: youtube (as yt-dlp --embed-metadata), movie, tv, music.
        /// Values come from a yt-dlp <name>.info.json next to the video, else the source's tags.
        #[arg(long, value_name = "PRESET")]
        meta_preset: Option<String>,
        /// Set a tag, e.g. --meta genre=Documentary or --meta 'title={title|file}'.
        /// `{field}` is filled from the .info.json or the source tags, `{file}` is the file name.
        /// Repeatable; overrides the preset; an empty value removes a preset tag.
        #[arg(long = "meta", value_name = "KEY=VALUE", value_parser = metadata::parse_tag)]
        meta: Vec<(String, String)>,
        /// Don't read yt-dlp .info.json files.
        #[arg(long)]
        no_info_json: bool,
        /// Leave a section out of the output, e.g. --remove 1:20-2:05 or --remove 58:30-end.
        /// Times are h:mm:ss, m:ss or seconds, with optional decimals; cuts are frame-accurate.
        /// Repeatable; needs a single input file. Audio is re-encoded to Opus.
        #[arg(long, value_name = "START-END", value_parser = sections::parse_range)]
        remove: Vec<(f64, f64)>,
    },
    /// Local web app: scan folders, queue encodes with live progress, compare source and AV1
    /// output side by side, browse past encodes. Listens on 127.0.0.1 only.
    #[cfg(feature = "ui")]
    Ui {
        /// Port to listen on (0 = pick a free one).
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Don't open a browser; just print the sign-in link.
        #[arg(long)]
        no_open: bool,
        #[arg(long, env = "AV1OPT_FFMPEG", default_value = "ffmpeg")]
        ffmpeg: String,
        #[arg(long, env = "AV1OPT_FFPROBE", default_value = "ffprobe")]
        ffprobe: String,
        /// Memory cap for each ffmpeg run, as for `encode`.
        #[arg(long, default_value = "auto")]
        memory_limit: String,
        /// CPU priority for encodes (0-19).
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(i32).range(0..=19))]
        nice: i32,
        /// Parallel ffprobe processes for scans.
        #[arg(short, long, default_value_t = 6)]
        jobs: usize,
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
        Cmd::Dups { paths, jobs, prefer, avoid, tolerance, refresh, top, csv, json, ffmpeg, ffprobe, memory_limit, nice } => {
            let abs = |p: &PathBuf| std::path::absolute(p).unwrap_or_else(|_| p.clone());
            let paths: Vec<PathBuf> = paths.iter().map(abs).collect();
            // SAFETY: plain syscall on our own process; children inherit the priority.
            unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) };
            let jobs = jobs.clamp(1, 64);
            let tools = tools::Tools::new(ffmpeg, ffprobe, memory_cap(&memory_limit, jobs as u8));
            let opts = dups::Options { jobs, max_dist: tolerance, prefs: dups::matching::Prefs { prefer: prefer.iter().map(abs).collect(), avoid }, refresh };
            let start = std::time::Instant::now();
            let progress = |p: dups::Progress| match p {
                dups::Progress::Probing { done: 0, total } => eprintln!("found {total} video files"),
                dups::Progress::Probing { done, total } if done.is_multiple_of(250) || done == total => eprint!("\rprobed {done}/{total}"),
                dups::Progress::Fingerprinting { done: 0, total } => eprintln!("\nreading frames of {total} videos (cached ones are instant)"),
                dups::Progress::Fingerprinting { done, total } if done.is_multiple_of(10) || done == total => {
                    let el = start.elapsed().as_secs_f64();
                    eprint!("\rfingerprinted {done}/{total}  ETA {}   ", tools::hms(el / done as f64 * (total - done) as f64));
                }
                dups::Progress::Comparing { videos } => eprintln!("\ncomparing {videos} videos"),
                _ => {}
            };
            let report = dups::find(&paths, &tools, &opts, &progress)?;
            dups::print(&report, top);
            if let Some(csv) = csv {
                dups::write_csv(&report, &csv)?;
                eprintln!("wrote {}", csv.display());
            }
            if let Some(json) = json {
                std::fs::write(&json, serde_json::to_vec_pretty(&report)?)?;
                eprintln!("wrote {}", json.display());
            }
            Ok(())
        }
        Cmd::Bench {
            paths, presets, param_sets, target_vmaf, max_videos, samples, sample_secs, min_crf, max_crf, refresh, work_dir, save, csv, json,
            jobs, ffmpeg, ffprobe, memory_limit, nice,
        } => {
            let paths: Vec<PathBuf> = paths.iter().map(|p| std::path::absolute(p).unwrap_or_else(|_| p.clone())).collect();
            let param_sets = if param_sets.is_empty() { bench::DEFAULT_PARAM_SETS.map(String::from).to_vec() } else { param_sets };
            let configs = bench::matrix(&presets, &param_sets);
            // SAFETY: plain syscall on our own process; children inherit the priority.
            unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) };
            let tools = tools::Tools::new(ffmpeg, ffprobe, memory_cap(&memory_limit, 1));
            let opts = bench::Options {
                configs: configs.clone(), target_vmaf, min_crf, max_crf, samples: samples.clamp(1, 8), sample_secs: sample_secs.clamp(2.0, 30.0),
                max_videos, jobs: jobs.clamp(1, 64), refresh, work_dir,
            };
            let start = std::time::Instant::now();
            let plan = std::sync::Mutex::new((0usize, 0usize, 0usize));
            let progress = |p: bench::Progress| match p {
                bench::Progress::Probing { done: 0, total } => eprintln!("found {total} video files"),
                bench::Progress::Probing { .. } => {}
                bench::Progress::Plan { videos, configs } => {
                    *plan.lock().unwrap() = (videos, configs, 0);
                    eprintln!("comparing {configs} settings on {videos} videos (run it on an otherwise idle machine: encode times are measured)");
                }
                bench::Progress::Sampling { video, path } => eprintln!("[{}/{}] {}: cutting sample clips", video + 1, plan.lock().unwrap().0, path.display()),
                bench::Progress::Trial { measure: m, cached, .. } => eprintln!(
                    "      crf {:2}: VMAF {:.2}, {:.0} kbps, {:.1} s{}",
                    m.crf, m.vmaf, m.bytes as f64 * 8.0 / m.clip_secs / 1000.0, m.encode_secs, if cached { " (remembered)" } else { "" }
                ),
                bench::Progress::Cell { video, config, cell } => {
                    let (videos, n, done) = {
                        let mut p = plan.lock().unwrap();
                        p.2 += 1;
                        *p
                    };
                    let left = start.elapsed().as_secs_f64() / done as f64 * (videos * n - done) as f64;
                    let what = match (&cell.point, &cell.error) {
                        (Some(pt), _) => format!("crf {:.2}, {:.0} kbps, {:.2}x speed{}", pt.crf, pt.bitrate / 1000.0, 1.0 / pt.cost.max(1e-9), if pt.capped { " (above target at the highest CRF)" } else { "" }),
                        (None, Some(e)) => format!("failed: {}", e.lines().last().unwrap_or("")),
                        (None, None) => "target out of reach".to_string(),
                    };
                    eprintln!("  [{}/{videos}] {}: {what}   (about {} left)", video + 1, configs[config].label(), tools::hms(left));
                }
                bench::Progress::VideoDone { .. } => {}
            };
            let report = bench::run(&paths, &tools, &opts, &progress)?;
            bench::print(&report);
            if let Some(csv) = csv {
                bench::write_csv(&report, &csv)?;
                eprintln!("wrote {}", csv.display());
            }
            if let Some(json) = json {
                std::fs::write(&json, serde_json::to_vec_pretty(&report)?)?;
                eprintln!("wrote {}", json.display());
            }
            if let Some(pick) = save {
                let picks = bench::tuning_picks(&report, pick);
                if picks.is_empty() {
                    anyhow::bail!("--save: nothing was measured under every setting, so there is nothing to save");
                }
                let mut t = tuning::Tuning::load()?;
                for (class, c) in picks {
                    println!("tuned, {}: preset {}, {} ({} video{})", class.label(), c.preset, c.params, c.videos, if c.videos == 1 { "" } else { "s" });
                    t.classes.insert(class, c);
                }
                println!("saved to {}; `av1opt encode --tuned` uses these", t.save()?.display());
            }
            Ok(())
        }
        Cmd::Tuning { clear } => {
            let mut t = tuning::Tuning::load()?;
            if clear {
                t.classes.clear();
                t.save()?;
                println!("tuned settings forgotten");
                return Ok(());
            }
            if t.classes.is_empty() {
                println!("no tuned settings; `av1opt bench <videos> --save` makes them");
            }
            for (class, c) in &t.classes {
                // Entries written by hand carry no measurements.
                let from = if c.videos == 0 {
                    String::new()
                } else {
                    format!(
                        "  ({:+.1}% size, {:.2}x time against the defaults at VMAF {}, from {} video{})",
                        (c.size - 1.0) * 100.0, c.time, c.target_vmaf, c.videos, if c.videos == 1 { "" } else { "s" }
                    )
                };
                println!("{}: preset {}, {}{from}", class.label(), c.preset, c.params);
            }
            Ok(())
        }
        Cmd::Encode {
            inputs, out_dir, container, target_vmaf, preset, svt_params, tuned, min_crf, max_crf, samples, sample_secs,
            min_savings, force, overwrite, retry_skipped, keep_temp, allow_zfs, ffmpeg, ffprobe, scenes, workers, memory_limit, nice,
            strip_metadata, keep_dates, meta_preset, meta, no_info_json, remove,
        } => {
            let meta = metadata::MetaOptions {
                keep: !strip_metadata,
                keep_dates,
                info_json: !no_info_json,
                tags: metadata::with_preset(meta_preset.as_deref(), &meta)?,
            };
            if min_crf > max_crf {
                anyhow::bail!("--min-crf {min_crf} is above --max-crf {max_crf}");
            }
            if let Some(p) = tools::Capabilities::probe(&ffmpeg, &ffprobe).problem(&ffmpeg, &ffprobe) {
                anyhow::bail!("{p} (--ffmpeg / AV1OPT_FFMPEG, --ffprobe / AV1OPT_FFPROBE)");
            }
            fsguard::ensure_writable_location(&out_dir, allow_zfs)?;
            std::fs::create_dir_all(&out_dir)?;
            // SAFETY: plain syscall on our own process; children inherit the priority.
            unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) };
            let tools = tools::Tools::new(ffmpeg, ffprobe, memory_cap(&memory_limit, if scenes { workers } else { 1 }));
            let tuning = if tuned {
                let t = tuning::Tuning::load()?;
                if t.classes.is_empty() {
                    anyhow::bail!("--tuned: no tuned settings yet (run `av1opt bench ... --save` first)");
                }
                for (class, c) in &t.classes {
                    eprintln!("tuned, {}: preset {}, {}", class.label(), c.preset, c.params);
                    search::check_params(&tools, c.preset, &c.params)?;
                }
                Some(t)
            } else {
                None
            };
            if svt_params != search::DEFAULT_PARAMS {
                search::check_params(&tools, preset, &svt_params)?;
            }
            let opts = encode::Options {
                container, target_vmaf, preset, svt_params, tuning, min_crf, max_crf, samples, sample_secs,
                min_savings, force, overwrite, keep_temp, scenes, workers: workers.into(), meta, remove,
            };
            let jobs = batch::expand(&inputs, &out_dir, is_video);
            if jobs.is_empty() {
                anyhow::bail!("no video files found in the given inputs");
            }
            if !opts.remove.is_empty() && jobs.len() > 1 {
                anyhow::bail!("--remove applies to one video; {} were given", jobs.len());
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
        #[cfg(feature = "ui")]
        Cmd::Ui { port, no_open, ffmpeg, ffprobe, memory_limit, nice, jobs } => {
            // SAFETY: as for `encode`; the server itself is idle most of the time.
            unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) };
            ui::run(ui::Config { port, open: !no_open, ffmpeg, ffprobe, memory_limit, jobs })
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
