// First: its macros are for every module below.
#[macro_use]
mod output;

mod batch;
mod bench;
mod dups;
mod encode;
mod estimate;
mod events;
mod files;
mod fsguard;
mod history;
mod ledger;
mod metadata;
mod probe;
mod repair;
mod replace;
mod report;
mod report_file;
mod scan;
mod scenes;
mod search;
mod sections;
mod signals;
mod source;
mod tools;
mod trash;
mod tuning;
#[cfg(feature = "ui")]
mod ui;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use report_file::Kind;
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
        #[arg(long, env = "AV1OPT_FFPROBE", default_value = "ffprobe", value_parser = tool_path)]
        ffprobe: String,
        /// Parallel ffprobe processes (1-64). Keep low on slow or FUSE-mounted drives.
        #[arg(short, long, default_value_t = 6, value_parser = whole::<1, 64>)]
        jobs: usize,
        /// Minimum expected savings (0-0.95) for a file to count as a candidate.
        #[arg(long, default_value_t = 0.2, value_parser = parse_min_savings)]
        min_savings: f64,
        /// Write one row per file to this CSV.
        #[arg(long)]
        csv: Option<PathBuf>,
        /// Number of top candidates to list.
        #[arg(long, default_value_t = 15)]
        top: usize,
        /// VMAF target the estimates assume (past encodes for nearby targets calibrate them).
        #[arg(long, default_value_t = 93.0, value_parser = parse_target)]
        target_vmaf: f64,
        /// SVT-AV1 preset the encode time estimates assume.
        #[arg(long, default_value_t = search::DEFAULT_PRESET, value_parser = clap::value_parser!(u8).range(0..=13))]
        preset: u8,
        /// Order of the list of top candidates: most saved, or most saved per hour of encoding.
        #[arg(long, value_enum, default_value_t)]
        sort: report::TopBy,
        /// Also list the candidates that save the most in this many hours of encoding
        /// (for a night's worth: --hours 8).
        #[arg(long, value_name = "HOURS", value_parser = positive_hours)]
        hours: Option<f64>,
        /// Write the paths picked by --hours to this file, one per line, best value first
        /// (then: xargs -d '\n' -a FILE av1opt encode -o OUT).
        #[arg(long, value_name = "FILE", requires = "hours")]
        list: Option<PathBuf>,
    },
    /// Find videos that show the same thing: the same video at another resolution, bitrate,
    /// codec or container, and copies cut at the start or the end. Read-only: decodes a few
    /// frames of every video (cached) and lists the groups with the copy worth keeping first.
    Dups {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Videos read at once (1-64). Keep low on slow or FUSE-mounted drives.
        #[arg(short, long, default_value_t = 6, value_parser = whole::<1, 64>)]
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
        #[arg(long, env = "AV1OPT_FFMPEG", default_value = "ffmpeg", value_parser = tool_path)]
        ffmpeg: String,
        #[arg(long, env = "AV1OPT_FFPROBE", default_value = "ffprobe", value_parser = tool_path)]
        ffprobe: String,
        /// Memory cap shared by the ffmpeg runs, as for `encode`.
        #[arg(long, default_value = "auto", value_parser = parse_memory)]
        memory_limit: String,
        /// CPU priority (0-19, higher = more polite to the desktop).
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(i32).range(0..=19))]
        nice: i32,
    },
    /// Put finished AV1 outputs where their originals were: the original goes to the trash,
    /// the output takes its name in its folder. Lists what would happen and changes nothing
    /// until --apply is given. Only outputs whose report (`.av1opt.json`) still matches both
    /// files are touched, and no existing file is ever written over. A report is followed only
    /// to originals inside the folders named here (name the originals' folder as well as the
    /// output folder, or give --anywhere), unless av1opt's own record says that this output was
    /// made from that original.
    Replace {
        /// Output folders, the folders of the originals, reports (`.av1opt.json`) or output files.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Do it. Without this the command only lists.
        #[arg(long)]
        apply: bool,
        /// The other way round: take replaced originals back out of the trash and return the
        /// AV1 files to where they were (also needs --apply to act).
        #[arg(long)]
        undo: bool,
        /// Also replace originals stored on ZFS.
        #[arg(long)]
        allow_zfs: bool,
        /// Take an original whose date changed as the file that was encoded, where its report
        /// cannot tell (reports written before the whole file was hashed). The risk is yours: if
        /// the file was in fact completed or rewritten since, its newer contents go to the trash
        /// in favour of an encode of the older ones. Originals seen to differ are still refused.
        #[arg(long, conflicts_with = "undo")]
        accept_changed_date: bool,
        /// Follow reports to originals wherever they are, not only in the folders named. A
        /// report can name any file: use this only for reports you trust.
        #[arg(long)]
        anywhere: bool,
    },
    /// Take outputs that are not on av1opt's private record, such as those made before it began,
    /// onto it, so that the page can replace them and put their originals back. Reads the reports
    /// under the folders named and lists what each would add; only --apply writes, and only to
    /// the record. Naming a folder here says that you trust the reports in it, as naming it to
    /// `replace` does. A report is refused unless its output and original (or, once replaced,
    /// the original in the trash) pass the checks `replace` makes.
    Adopt {
        /// Output folders, reports (`.av1opt.json`) or output files.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Write to the record. Without this the command only lists.
        #[arg(long)]
        apply: bool,
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
        /// Repeatable. Default: tune=0, tune=1, tune=2, tune=0 with qm-min=0,
        /// tune=0 with variance boost.
        #[arg(long = "params", value_name = "PARAMS", value_parser = search::parse_params)]
        param_sets: Vec<String>,
        #[arg(long, default_value_t = 93.0, value_parser = parse_target)]
        target_vmaf: f64,
        /// Use at most this many videos (1 or more), spread over the folders and resolution classes.
        #[arg(long, default_value_t = 6, value_parser = at_least_one)]
        max_videos: usize,
        /// Sample clips per video (1-8).
        #[arg(long, default_value_t = 2, value_parser = whole::<1, 8>)]
        samples: usize,
        /// Length of a sample clip in seconds (2-30).
        #[arg(long, default_value_t = 6.0, value_parser = parse_bench_sample_secs)]
        sample_secs: f64,
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u8).range(1..=63))]
        min_crf: u8,
        #[arg(long, default_value_t = 55, value_parser = clap::value_parser!(u8).range(1..=63))]
        max_crf: u8,
        /// Measure everything again instead of using remembered sample encodes.
        #[arg(long)]
        refresh: bool,
        /// Folder for the lossless sample clips while a video is measured (several GB for 4K).
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
        /// Parallel ffprobe processes while collecting the videos (1-64).
        #[arg(short, long, default_value_t = 6, value_parser = whole::<1, 64>)]
        jobs: usize,
        #[arg(long, env = "AV1OPT_FFMPEG", default_value = "ffmpeg", value_parser = tool_path)]
        ffmpeg: String,
        #[arg(long, env = "AV1OPT_FFPROBE", default_value = "ffprobe", value_parser = tool_path)]
        ffprobe: String,
        /// Memory cap for each ffmpeg run, as for `encode`.
        #[arg(long, default_value = "auto", value_parser = parse_memory)]
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
    #[command(after_help = "Exit status: 0 when no file failed, 1 on an error that stopped the run, 2 on wrong usage, 3 when some files failed and the others were dealt with, 130 or 143 when stopped by Ctrl-C or SIGTERM.")]
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
        #[arg(long, default_value_t = 93.0, value_parser = parse_target)]
        target_vmaf: f64,
        /// SVT-AV1 preset: lower is slower and smaller (6 is about 6% larger than 5, for a fifth less processor time).
        #[arg(long, default_value_t = search::DEFAULT_PRESET, value_parser = clap::value_parser!(u8).range(0..=13))]
        preset: u8,
        /// SVT-AV1 parameters besides the CRF, as key=value:key=value. Replaces the default
        /// (tune=0); note that SVT-AV1's own default is tune=1.
        #[arg(long, value_name = "PARAMS", default_value = search::DEFAULT_PARAMS, value_parser = search::parse_params)]
        svt_params: String,
        /// Use the preset and SVT-AV1 parameters `av1opt bench --save` picked for each
        /// resolution class; --preset and --svt-params apply to classes without a pick.
        #[arg(long)]
        tuned: bool,
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u8).range(1..=63))]
        min_crf: u8,
        #[arg(long, default_value_t = 55, value_parser = clap::value_parser!(u8).range(1..=63))]
        max_crf: u8,
        /// Number of sample clips the CRF search goes by. Without it and --sample-secs: 1 s
        /// clips, one for about every 8 seconds of video (8 at least, 64 at most), where the
        /// whole video is checked after its encode; one 4 s clip for about every two minutes
        /// (8 to 32) where it is not (--scenes, removed sections, HDR).
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=200))]
        samples: Option<u64>,
        /// Length of a sample clip in seconds, 1 to 60 (4 with --samples alone).
        #[arg(long, value_parser = parse_sample_secs)]
        sample_secs: Option<f64>,
        /// Skip the full encode if predicted video savings are below this (0-0.95).
        #[arg(long, default_value_t = 0.15, value_parser = parse_min_savings)]
        min_savings: f64,
        /// Encode even if already AV1 or below --min-savings.
        #[arg(long)]
        force: bool,
        /// Encode again over an output already there, if its report says it was made from this
        /// same file. Any other file of that name is refused and left as it is.
        #[arg(long)]
        overwrite: bool,
        /// Retry files an earlier run skipped (low savings, unreachable target, ...).
        #[arg(long)]
        retry_skipped: bool,
        /// Which file goes first: as given, the largest estimated saving, or the largest
        /// estimated saving per hour of encoding (so a run stopped early has saved the most).
        #[arg(long, value_enum, default_value_t)]
        order: batch::Order,
        /// Keep the temporary folder (with the partial encode if the encode fails) and the
        /// work folder of a per-scene encode, for debugging.
        #[arg(long)]
        keep_temp: bool,
        /// Allow writing output to ZFS.
        #[arg(long)]
        allow_zfs: bool,
        #[arg(long, env = "AV1OPT_FFMPEG", default_value = "ffmpeg", value_parser = tool_path)]
        ffmpeg: String,
        #[arg(long, env = "AV1OPT_FFPROBE", default_value = "ffprobe", value_parser = tool_path)]
        ffprobe: String,
        /// Choose the CRF per scene: split at scene changes, target VMAF for each chunk, then join.
        /// Evens out quality across the video; slower than one CRF for the whole file.
        /// `--scenes=auto` does it only for videos whose samples differ in quality at one CRF.
        #[arg(long, value_enum, value_name = "WHEN", num_args = 0..=1, require_equals = true, default_value_t, default_missing_value = "always")]
        scenes: encode::Scenes,
        /// Chunks encoded in parallel with --scenes. Chunks running at once share --memory-limit.
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=8))]
        workers: u8,
        /// Memory cap for each ffmpeg run (systemd MemoryMax syntax, e.g. 12G). "auto" = 40% of RAM, "none" = no cap.
        #[arg(long, default_value = "auto", value_parser = parse_memory)]
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
        /// Measure every CRF trial again. Normally a trial measured in an earlier run for the
        /// same file, sample clips, settings and ffmpeg build is reused.
        #[arg(long)]
        fresh_search: bool,
        /// Sound tracks: keep copies AAC, Opus and MP3 untouched (other formats become Opus);
        /// shrink also re-encodes tracks far above what Opus needs (stereo above 144 kbps
        /// becomes Opus at 96 kbps).
        #[arg(long, value_enum, default_value_t = encode::Audio::Keep)]
        audio: encode::Audio,
        /// Leave a section out of the output, e.g. --remove 1:20-2:05 or --remove 58:30-end.
        /// Times are h:mm:ss, m:ss or seconds, with optional decimals; cuts are frame-accurate.
        /// Repeatable; needs a single input file. Audio is re-encoded to Opus.
        #[arg(long, value_name = "START-END", value_parser = sections::parse_range)]
        remove: Vec<(f64, f64)>,
    },
    /// Development aid: measures the VMAF of a video's sample clips at each CRF in a list, one
    /// JSON line per CRF. The CRF search is tested against curves measured this way.
    #[command(hide = true)]
    Curve {
        input: PathBuf,
        /// CRFs to measure: ranges and single values, e.g. 24-40 or 28,28.25,28.5.
        #[arg(long, default_value = "24-40")]
        crfs: String,
        /// Folder for the sample clips (removed afterwards).
        #[arg(long)]
        tmp: PathBuf,
        #[arg(long, default_value_t = search::DEFAULT_PRESET, value_parser = clap::value_parser!(u8).range(0..=13))]
        preset: u8,
        #[arg(long, default_value = search::DEFAULT_PARAMS, value_parser = search::parse_params)]
        svt_params: String,
        /// Number of sample clips (1-200). Without it: as many as `encode` would take.
        #[arg(long, value_parser = whole::<1, 200>)]
        samples: Option<usize>,
        #[arg(long, default_value_t = search::SAMPLE_SECS, value_parser = parse_sample_secs)]
        sample_secs: f64,
        #[arg(long, env = "AV1OPT_FFMPEG", default_value = "ffmpeg", value_parser = tool_path)]
        ffmpeg: String,
        #[arg(long, env = "AV1OPT_FFPROBE", default_value = "ffprobe", value_parser = tool_path)]
        ffprobe: String,
        #[arg(long, default_value = "auto", value_parser = parse_memory)]
        memory_limit: String,
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
        #[arg(long, env = "AV1OPT_FFMPEG", default_value = "ffmpeg", value_parser = tool_path)]
        ffmpeg: String,
        #[arg(long, env = "AV1OPT_FFPROBE", default_value = "ffprobe", value_parser = tool_path)]
        ffprobe: String,
        /// Memory cap for each ffmpeg run, as for `encode`.
        #[arg(long, default_value = "auto", value_parser = parse_memory)]
        memory_limit: String,
        /// CPU priority for encodes (0-19).
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(i32).range(0..=19))]
        nice: i32,
        /// Parallel ffprobe processes for scans (1-64).
        #[arg(short, long, default_value_t = 6, value_parser = whole::<1, 64>)]
        jobs: usize,
    },
}

/// A VMAF target is a score: a number above 0 and up to 100 (not `nan`, which is neither
/// reached nor missed by any encode).
fn parse_target(s: &str) -> Result<f64, String> {
    match s.parse::<f64>() {
        Ok(v) if v > 0.0 && v <= 100.0 => Ok(v),
        _ => Err("a VMAF target is a number above 0 and up to 100".into()),
    }
}

/// A number within limits. `nan` and the infinities are numbers to Rust and fall outside any.
fn number_in(s: &str, low: f64, high: f64, what: &str) -> Result<f64, String> {
    match s.parse::<f64>() {
        Ok(v) if v >= low && v <= high => Ok(v),
        _ => Err(format!("{what} is a number from {low} to {high}")),
    }
}

/// A clip of no length measures nothing, and a very long one is a lossless copy of the video.
fn parse_sample_secs(s: &str) -> Result<f64, String> {
    number_in(s, 1.0, 60.0, "the length of a sample clip in seconds")
}

fn parse_bench_sample_secs(s: &str) -> Result<f64, String> {
    number_in(s, 2.0, 30.0, "the length of a sample clip in seconds")
}

/// Below nothing would keep outputs larger than their source, and near 1 no encode could pass.
fn parse_min_savings(s: &str) -> Result<f64, String> {
    number_in(s, 0.0, 0.95, "the share of the size to save")
}

fn whole<const LOW: usize, const HIGH: usize>(s: &str) -> Result<usize, String> {
    match s.parse::<usize>() {
        Ok(n) if (LOW..=HIGH).contains(&n) => Ok(n),
        _ => Err(format!("a whole number from {LOW} to {HIGH}")),
    }
}

fn at_least_one(s: &str) -> Result<usize, String> {
    s.parse::<usize>().ok().filter(|n| *n >= 1).ok_or_else(|| "a whole number, 1 or more".to_string())
}

/// A tool named by a path is made absolute here. It is started from other folders later (VMAF
/// runs in the temporary folder, for the sake of its log's name), where a relative path leads
/// nowhere. A bare name is left to be looked up in the PATH.
fn tool_path(s: &str) -> Result<String, String> {
    if !s.contains('/') {
        return Ok(s.to_string());
    }
    let absolute = std::path::absolute(s).map_err(|e| format!("cannot tell where this is: {e}"))?;
    absolute.into_os_string().into_string().map_err(|_| "the working folder's path is not valid UTF-8, so a tool cannot be named relative to it".to_string())
}

/// A memory amount cut in two: its number and what follows it.
fn split_amount(s: &str) -> (&str, &str) {
    s.split_at(s.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(s.len()))
}

/// A memory cap is "auto", "none", or what systemd takes for MemoryMax: a number of bytes, or
/// a number that may have decimals followed by K, M, G or T for larger units or % for a share
/// of the memory. Anything else would have been no cap at all, with nothing said.
fn parse_memory(s: &str) -> Result<String, String> {
    let (number, unit) = split_amount(s);
    let digits = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
    let (whole, decimals) = number.split_once('.').map_or((number, None), |(w, d)| (w, Some(d)));
    let written_well = digits(whole) && decimals.is_none_or(digits);
    let amount = written_well
        && match unit.to_ascii_uppercase().as_str() {
            // Bytes come whole.
            "" => decimals.is_none() && number.parse::<u64>().is_ok_and(|n| n > 0),
            "K" | "M" | "G" | "T" | "%" => number.parse::<f64>().is_ok_and(|n| n > 0.0 && n.is_finite()),
            _ => false,
        };
    if matches!(s, "auto" | "none" | "infinity") || amount {
        Ok(s.to_string())
    } else {
        Err("a memory cap is auto, none, or an amount such as 12G, 1.5G, 800M or 40%".into())
    }
}

/// The exit status of a batch in which files failed and the rest was dealt with. An error that
/// stops the program is 1 and wrong usage is 2, so that a script can tell the three apart.
const SOME_FILES_FAILED: i32 = 3;

fn main() -> Result<()> {
    let result = run_command(Cli::parse().cmd);
    // Stopped by a signal, whatever the work then ended with.
    if let Some(code) = signals::exit_code() {
        std::process::exit(code);
    }
    result
}

fn run_command(command: Cmd) -> Result<()> {
    match command {
        Cmd::Scan { paths, ffprobe, jobs, min_savings, csv, top, target_vmaf, preset, sort, hours, list } => {
            let past = history::load();
            let model = estimate::Model::fit(&past, target_vmaf);
            let time = estimate::TimeModel::fit(&past, preset);
            // Absolute roots keep cache keys stable however the folder was typed.
            let paths: Vec<PathBuf> = paths.iter().map(|p| std::path::absolute(p).unwrap_or_else(|_| p.clone())).collect();
            scan(&paths, &ffprobe, jobs, min_savings, csv.as_deref(), top, &model, &time, sort, hours.map(|h| (h, list)))
        }
        Cmd::Dups { paths, jobs, prefer, avoid, tolerance, refresh, top, csv, json, ffmpeg, ffprobe, memory_limit, nice } => {
            let abs = |p: &PathBuf| std::path::absolute(p).unwrap_or_else(|_| p.clone());
            let paths: Vec<PathBuf> = paths.iter().map(abs).collect();
            check_reports(&[(csv.as_deref(), dups::CSV), (json.as_deref(), Kind::Json)], &paths)?;
            // SAFETY: plain syscall on our own process; children inherit the priority.
            unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) };
            let tools = signals::stopping_on_signals(tools::Tools::new(ffmpeg, ffprobe, memory_cap(&memory_limit, jobs as u8)));
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
            // The files first: printing ends the program once nobody reads standard output.
            let written = (|| -> Result<()> {
                if let Some(csv) = csv {
                    dups::write_csv(&report, &csv, &paths)?;
                    eprintln!("wrote {}", csv.display());
                }
                if let Some(json) = json {
                    report_file::write(&json, Kind::Json, &paths, &serde_json::to_vec_pretty(&report)?)?;
                    eprintln!("wrote {}", json.display());
                }
                Ok(())
            })();
            dups::print(&report, top);
            written
        }
        Cmd::Replace { paths, apply, undo, allow_zfs, accept_changed_date, anywhere } => {
            let paths: Vec<PathBuf> = paths.iter().map(|p| std::path::absolute(p).unwrap_or_else(|_| p.clone())).collect();
            replace::run(&paths, apply, undo, allow_zfs, accept_changed_date, anywhere)
        }
        Cmd::Adopt { paths, apply } => {
            let paths: Vec<PathBuf> = paths.iter().map(|p| std::path::absolute(p).unwrap_or_else(|_| p.clone())).collect();
            replace::adopt(&paths, apply)
        }
        Cmd::Bench {
            paths, presets, param_sets, target_vmaf, max_videos, samples, sample_secs, min_crf, max_crf, refresh, work_dir, save, csv, json,
            jobs, ffmpeg, ffprobe, memory_limit, nice,
        } => {
            let paths: Vec<PathBuf> = paths.iter().map(|p| std::path::absolute(p).unwrap_or_else(|_| p.clone())).collect();
            check_reports(&[(csv.as_deref(), bench::CSV), (json.as_deref(), Kind::Json)], &paths)?;
            let param_sets = if param_sets.is_empty() { bench::DEFAULT_PARAM_SETS.map(String::from).to_vec() } else { param_sets };
            let configs = bench::matrix(&presets, &param_sets);
            // SAFETY: plain syscall on our own process; children inherit the priority.
            unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) };
            let tools = signals::stopping_on_signals(tools::Tools::new(ffmpeg, ffprobe, memory_cap(&memory_limit, 1)));
            let opts = bench::Options {
                configs: configs.clone(), target_vmaf, min_crf, max_crf, samples, sample_secs,
                max_videos, jobs, refresh, work_dir,
            };
            let start = std::time::Instant::now();
            let plan = std::sync::Mutex::new((0usize, 0usize, 0usize));
            let progress = |p: bench::Progress| match p {
                bench::Progress::Probing { done: 0, total } => eprintln!("found {total} video files"),
                bench::Progress::Probing { .. } => {}
                bench::Progress::LeftOut { videos } => eprintln!("note: {videos} videos left out: {}", tools::NOT_TEXT),
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
            // The files and the tuning first: printing ends the program once nobody reads
            // standard output, and hours of measuring would be lost with them.
            let mut said = Vec::new();
            let written = (|| -> Result<()> {
                if let Some(csv) = csv {
                    bench::write_csv(&report, &csv, &paths)?;
                    eprintln!("wrote {}", csv.display());
                }
                if let Some(json) = json {
                    report_file::write(&json, Kind::Json, &paths, &serde_json::to_vec_pretty(&report)?)?;
                    eprintln!("wrote {}", json.display());
                }
                if let Some(pick) = save {
                    let picks = bench::tuning_picks(&report, pick);
                    if picks.is_empty() {
                        anyhow::bail!("--save: nothing was measured under every setting, so there is nothing to save");
                    }
                    let mut t = tuning::Tuning::load()?;
                    for (class, c) in picks {
                        said.push(format!("tuned, {}: preset {}, {} ({} video{})", class.label(), c.preset, c.params, c.videos, if c.videos == 1 { "" } else { "s" }));
                        t.classes.insert(class, c);
                    }
                    said.push(format!("saved to {}; `av1opt encode --tuned` uses these", t.save()?.display()));
                }
                Ok(())
            })();
            bench::print(&report);
            for line in said {
                println!("{line}");
            }
            written
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
            strip_metadata, keep_dates, meta_preset, meta, no_info_json, remove, audio, fresh_search, order,
        } => {
            if let Some(why) = scenes_value_taken_for_input(&inputs) {
                Cli::command().error(clap::error::ErrorKind::ValueValidation, why).exit();
            }
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
            // SAFETY: plain syscall on our own process; children inherit the priority.
            unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) };
            let tools = signals::stopping_on_signals(tools::Tools::new(ffmpeg, ffprobe, memory_cap(&memory_limit, 1)).sharing_between_chunks(memory_cap(&memory_limit, workers)));
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
                out_dir_must_exist: false,
                container, target_vmaf, preset, svt_params, tuning, min_crf, max_crf, samples: samples.map(|n| n as usize), sample_secs,
                min_savings, force, overwrite, keep_temp, scenes, workers: workers.into(), meta, remove, audio, fresh_search,
            };
            let mut jobs = batch::expand(&inputs, &out_dir, is_video);
            if jobs.is_empty() {
                anyhow::bail!("no video files found in the given inputs");
            }
            if !opts.remove.is_empty() && jobs.len() > 1 {
                anyhow::bail!("--remove applies to one video; {} were given", jobs.len());
            }
            // Only now: a run refused above leaves no empty folder behind.
            std::fs::create_dir_all(&out_dir).with_context(|| format!("cannot make the output folder {}", out_dir.display()))?;
            if order != batch::Order::Given && jobs.len() > 1 {
                order_jobs(&mut jobs, &inputs, &tools.ffprobe, &opts, order)?;
            }
            let s = batch::run(&tools, &jobs, &out_dir, &opts, retry_skipped)?;
            let saved = s.bytes_in.saturating_sub(s.bytes_out) as f64 / 1e9;
            eprintln!(
                "\nencoded {}, skipped {}, failed {}, already done {} | {:.2} GB -> {:.2} GB ({saved:.2} GB saved)",
                s.ok, s.skipped, s.failed, s.already_done, s.bytes_in as f64 / 1e9, s.bytes_out as f64 / 1e9
            );
            eprintln!("log: {}", out_dir.join(batch::LOG_NAME).display());
            for folder in &s.kept {
                eprintln!("kept for the next run: {}", folder.display());
            }
            if s.failed > 0 && !s.cancelled {
                std::process::exit(SOME_FILES_FAILED);
            }
            Ok(())
        }
        Cmd::Curve { input, crfs, tmp, preset, svt_params, samples, sample_secs, ffmpeg, ffprobe, memory_limit } => {
            let mut list: Vec<f64> = Vec::new();
            for part in crfs.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                match part.split_once('-') {
                    Some((a, b)) => list.extend((a.parse::<u8>()?..=b.parse::<u8>()?).map(f64::from)),
                    None => list.push(part.parse()?),
                }
            }
            fsguard::ensure_writable_location(&tmp, false)?;
            std::fs::create_dir_all(&tmp)?;
            let dir = files::make_run_folder(&tmp, ".av1opt-curve-")?;
            // SAFETY: plain syscall on our own process; children inherit the priority.
            unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 10) };
            let tools = signals::stopping_on_signals(tools::Tools::new(ffmpeg, ffprobe, memory_cap(&memory_limit, 1)));
            let run = || -> Result<()> {
                let size = std::fs::metadata(&input)?.len();
                let info = probe::probe(&tools.ffprobe, &input, size)?;
                let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
                let vmaf_threads = if info.width.max(info.height) >= 3200 { threads.min(4) } else { threads };
                let enc = search::EncodeSettings { preset, pix_fmt_out: "yuv420p10le", params: svt_params.clone() };
                let pos = search::in_file_time(&info, search::sample_positions(info.picture_secs(), samples.unwrap_or_else(|| search::sample_count(info.picture_secs())), search::clip_secs(&info, sample_secs)));
                let clips = search::extract_samples(&tools, &input, &info, &dir, pos)?;
                for crf in list {
                    let started = std::time::Instant::now();
                    let t = search::trial(&tools, &info, &clips, &dir, &enc, crf, vmaf_threads, info.video_bitrate / 8.0, None)?;
                    println!(
                        "{}",
                        serde_json::json!({
                            "video": input.file_name().map(|n| n.to_string_lossy()), "width": info.width, "height": info.height,
                            "preset": preset, "params": svt_params, "crf": crf, "vmaf": t.mean_vmaf, "min_vmaf": t.min_vmaf,
                            "size_ratio": t.size_ratio, "secs": started.elapsed().as_secs_f64(),
                        })
                    );
                }
                Ok(())
            };
            let result = run();
            let _ = std::fs::remove_dir_all(&dir);
            result
        }
        #[cfg(feature = "ui")]
        Cmd::Ui { port, no_open, ffmpeg, ffprobe, memory_limit, nice, jobs } => {
            // SAFETY: as for `encode`; the server itself is idle most of the time.
            unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) };
            ui::run(ui::Config { port, open: !no_open, ffmpeg, ffprobe, memory_limit, jobs })
        }
    }
}

/// `--scenes` takes its value only after `=`, so in `--scenes auto` the word becomes an input and
/// scenes `always`, about three times the encode time. An input of that name that is no file is
/// that mistake.
fn scenes_value_taken_for_input(inputs: &[PathBuf]) -> Option<String> {
    let word = inputs.iter().filter_map(|p| p.to_str()).find(|p| matches!(*p, "auto" | "off" | "always") && !Path::new(p).exists())?;
    Some(format!("there is no file or folder called `{word}`; to choose when scenes are used, write --scenes={word}, with the equals sign"))
}

/// Per-ffmpeg memory cap: the configured total, split between the `workers` that run at the
/// same time. Units are in capitals, the only way systemd reads them.
fn memory_cap(setting: &str, workers: u8) -> Option<String> {
    let workers = workers.max(1) as f64;
    let as_given = || Some(setting.to_ascii_uppercase());
    let mb = match setting {
        "none" | "infinity" => return None,
        "auto" => std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|m| m.lines().find(|l| l.starts_with("MemTotal:"))?.split_whitespace().nth(1)?.parse::<u64>().ok())
            .map(|kb| (kb * 4 / 10 / 1024) as f64)?,
        s => {
            let (number, unit) = split_amount(s);
            let Ok(n) = number.parse::<f64>() else { return as_given() };
            match unit.to_ascii_uppercase().as_str() {
                "%" => return Some(share_of_memory(n, workers)),
                _ if workers <= 1.0 => return as_given(),
                "" => n / 1024.0 / 1024.0,
                "K" => n / 1024.0,
                "M" => n,
                "G" => n * 1024.0,
                "T" => n * 1024.0 * 1024.0,
                _ => return as_given(),
            }
        }
    };
    Some(format!("{}M", ((mb / workers) as u64).max(1)))
}

/// A worker's part of `percent`, in the two decimals systemd takes at most.
fn share_of_memory(percent: f64, workers: f64) -> String {
    let hundredths = (((percent * 100.0).round() / workers) as u64).max(1);
    let (whole, part) = (hundredths / 100, hundredths % 100);
    match part {
        0 => format!("{whole}%"),
        p if p % 10 == 0 => format!("{whole}.{}%", p / 10),
        p => format!("{whole}.{p:02}%"),
    }
}

/// Sorts a batch by what `scan` expects of each file (probe results come from its cache).
fn order_jobs(jobs: &mut [batch::Job], inputs: &[PathBuf], ffprobe: &str, o: &encode::Options, order: batch::Order) -> Result<()> {
    let abs = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let past = history::load();
    let model = estimate::Model::fit(&past, o.target_vmaf);
    let time = estimate::TimeModel::fit(&past, o.preset);
    let roots: Vec<PathBuf> = inputs.iter().map(|p| abs(p)).collect();
    eprintln!("probing {} files to order them", jobs.len());
    let (found, _) = scan::collect(&roots, ffprobe, 6, o.min_savings, &model, &|_, _| {})?;
    // Files that would be skipped anyway are worth nothing and go last.
    let worth: std::collections::HashMap<PathBuf, f64> = found
        .iter()
        .filter(|r| r.verdict == estimate::Verdict::Candidate)
        .map(|r| {
            let saved = r.size as f64 - r.estimate.likely;
            let secs = r.info.as_ref().map_or(0.0, |i| time.secs(i));
            let w = match order {
                batch::Order::PerHour if secs > 0.0 => saved / secs,
                batch::Order::PerHour => 0.0,
                _ => saved,
            };
            (r.path.clone(), w)
        })
        .collect();
    batch::order_by(jobs, |p| worth.get(&abs(p)).copied().unwrap_or(0.0));
    let what = if order == batch::Order::PerHour { "largest estimated saving per hour of encoding" } else { "largest estimated saving" };
    eprintln!("order: {what} first");
    Ok(())
}

/// Refuses, before the work, the report files named that could not be written after it.
fn check_reports(named: &[(Option<&Path>, Kind)], given: &[PathBuf]) -> Result<()> {
    let reports: Vec<(&Path, Kind)> = named.iter().filter_map(|&(path, kind)| Some((path?, kind))).collect();
    report_file::check_all(&reports, given)
}

fn positive_hours(s: &str) -> Result<f64, String> {
    match s.parse::<f64>() {
        Ok(h) if h > 0.0 && h <= 10_000.0 => Ok(h),
        _ => Err("a number of hours above 0 and up to 10000, like 8 or 2.5".into()),
    }
}

#[allow(clippy::too_many_arguments)]
fn scan(
    paths: &[PathBuf],
    ffprobe: &str,
    jobs: usize,
    min_savings: f64,
    csv: Option<&Path>,
    top: usize,
    model: &estimate::Model,
    time: &estimate::TimeModel,
    sort: report::TopBy,
    pick: Option<(f64, Option<PathBuf>)>,
) -> Result<()> {
    let list = pick.as_ref().and_then(|(_, list)| list.as_deref());
    check_reports(&[(list, Kind::List), (csv, report::CSV)], paths)?;
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
    if let Some(note) = stats.unreadable_note() {
        eprintln!("warning: {note}");
    }

    if model.samples == 0 {
        eprintln!("estimates: built-in pilot model (no encodes in the history yet; `encode` improves them)");
    } else {
        eprintln!("estimates: learned from {} past encodes (spread x{:.2})", model.samples, (1.28 * model.sigma).exp());
    }
    // The files first: printing ends the program once nobody reads standard output.
    let picked = pick.as_ref().map(|(hours, _)| scan::pick_for_time(&results, time, hours * 3600.0));
    let written = (|| -> Result<()> {
        if let (Some(picked), Some((_, Some(list)))) = (&picked, &pick) {
            let mut text = String::new();
            for r in picked {
                // A line break in a name would make two paths of one.
                match r.path.to_str().filter(|p| !p.contains('\n')) {
                    Some(p) => text += &format!("{p}\n"),
                    None => eprintln!("warning: left out of {}: {}", list.display(), r.path.display()),
                }
            }
            report_file::write(list, Kind::List, paths, text.as_bytes())?;
            eprintln!("wrote {}", list.display());
        }
        if let Some(csv) = csv {
            report::write_csv(&results, csv, time, paths)?;
            eprintln!("wrote {}", csv.display());
        }
        Ok(())
    })();
    report::print(&results, top, time, sort);
    if let (Some(picked), Some((hours, _))) = (&picked, &pick) {
        report::print_pick(picked, *hours, &results, time);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the command line says to `words`, as text: empty when it is accepted.
    fn refusal(words: &str) -> String {
        Cli::try_parse_from(words.split_whitespace()).err().map_or(String::new(), |e| e.to_string())
    }

    #[test]
    fn numbers_are_refused_by_their_real_range() {
        let encode = "av1opt encode v.mkv -o o";
        let scan = "av1opt scan v";
        let dups = "av1opt dups v";
        let bench = "av1opt bench v";
        let curve = "av1opt curve v.mkv --tmp t";
        /// Command, option, range as said, accepted values, refused values.
        type Case<'a> = (&'a str, &'a str, &'a str, &'a [&'a str], &'a [&'a str]);
        let cases: &[Case] = &[
            (encode, "--sample-secs", "from 1 to 60", &["1", "4.5", "60"], &["nan", "inf", "-inf", "0", "0.99", "-1", "60.5", "1e9", "x"]),
            (curve, "--sample-secs", "from 1 to 60", &["1", "60"], &["nan", "0", "61"]),
            (bench, "--sample-secs", "from 2 to 30", &["2", "30"], &["nan", "inf", "1.9", "31"]),
            (encode, "--min-savings", "from 0 to 0.95", &["0", "0.5", "0.95"], &["nan", "inf", "-0.1", "0.96", "5"]),
            (scan, "--min-savings", "from 0 to 0.95", &["0", "0.95"], &["nan", "-1", "5"]),
            (scan, "--jobs", "from 1 to 64", &["1", "64"], &["0", "65", "-1", "x"]),
            (dups, "--jobs", "from 1 to 64", &["1", "64"], &["0", "65"]),
            (bench, "--jobs", "from 1 to 64", &["1", "64"], &["0", "65"]),
            (bench, "--samples", "from 1 to 8", &["1", "8"], &["0", "9"]),
            (bench, "--max-videos", "1 or more", &["1", "500"], &["0", "-1"]),
            (curve, "--samples", "from 1 to 200", &["1", "200"], &["0", "201"]),
            (scan, "--hours", "up to 10000", &["0.5", "10000"], &["0", "nan", "inf", "20000"]),
        ];
        for (command, option, range, good, bad) in cases {
            for value in *good {
                assert_eq!(refusal(&format!("{command} {option}={value}")), "", "{option} {value}");
            }
            for value in *bad {
                let said = refusal(&format!("{command} {option}={value}"));
                assert!(said.contains(option) && said.contains(range), "{option} {value}: {said}");
            }
        }
        for command in [encode, bench] {
            assert_eq!(refusal(&format!("{command} --min-crf=1")), "");
            let said = refusal(&format!("{command} --min-crf=0"));
            assert!(said.contains("--min-crf") && said.contains("1..=63"), "{said}");
        }
        assert!(refusal(&format!("{curve} --preset=14")).contains("--preset"));
    }

    #[test]
    fn the_defaults_pass_their_own_limits() {
        for words in ["av1opt encode v.mkv -o o", "av1opt scan v", "av1opt dups v", "av1opt bench v", "av1opt curve v.mkv --tmp t"] {
            assert_eq!(refusal(words), "", "{words}");
        }
    }

    #[test]
    fn a_tool_named_by_a_path_stays_itself_from_any_folder() {
        let here = std::env::current_dir().unwrap();
        assert_eq!(tool_path("./bin/ffmpeg").unwrap(), here.join("bin/ffmpeg").to_str().unwrap());
        assert_eq!(tool_path("bin/ffprobe").unwrap(), here.join("bin/ffprobe").to_str().unwrap());
        assert_eq!(tool_path("/opt/ffmpeg").unwrap(), "/opt/ffmpeg");
        // A bare name is the system's to find.
        assert_eq!(tool_path("ffmpeg").unwrap(), "ffmpeg");
        let Cmd::Encode { ffmpeg, .. } = Cli::try_parse_from(["av1opt", "encode", "v.mkv", "-o", "o", "--ffmpeg", "./bin/ffmpeg"]).unwrap().cmd else { panic!() };
        assert_eq!(ffmpeg, here.join("bin/ffmpeg").to_str().unwrap());
    }

    #[test]
    fn a_memory_cap_is_written_as_systemd_takes_it() {
        for good in ["auto", "none", "infinity", "12G", "12g", "1.5G", "0.5t", "800M", "512k", "40%", "50.5%", "1048576"] {
            assert_eq!(parse_memory(good).as_deref(), Ok(good));
        }
        for bad in ["", "0", "0G", "0.0G", "0%", "1.", "1.G", ".5G", "1.5", "1.5.2G", "G", "%", "-1G", "1e3G", "12GB", "1 G", "NaNG", "infG", "Auto", "99999999999999999999999"] {
            assert!(parse_memory(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_memory_cap_is_shared_between_the_workers() {
        let cap = |s, workers| memory_cap(s, workers);
        assert_eq!((cap("none", 2), cap("infinity", 1)), (None, None));
        // One at a time: the amount as given, in capitals.
        assert_eq!(cap("12g", 1).as_deref(), Some("12G"));
        assert_eq!(cap("1.5g", 1).as_deref(), Some("1.5G"));
        assert_eq!(cap("1048576", 1).as_deref(), Some("1048576"));
        assert_eq!(cap("40%", 1).as_deref(), Some("40%"));
        assert_eq!(cap("50.5%", 1).as_deref(), Some("50.5%"));
        // Several: each its part, percentages too.
        assert_eq!(cap("12G", 2).as_deref(), Some("6144M"));
        assert_eq!(cap("1.5g", 2).as_deref(), Some("768M"));
        assert_eq!(cap("1T", 4).as_deref(), Some("262144M"));
        assert_eq!(cap("1073741824", 2).as_deref(), Some("512M"));
        assert_eq!(cap("40%", 2).as_deref(), Some("20%"));
        assert_eq!(cap("50%", 4).as_deref(), Some("12.5%"));
        assert_eq!(cap("50.5%", 2).as_deref(), Some("25.25%"));
        assert_eq!(cap("100%", 3).as_deref(), Some("33.33%"));
        // Never nothing: a cap of 0 would stop the program at once.
        assert_eq!(cap("500K", 2).as_deref(), Some("1M"));
        assert_eq!(cap("0.01%", 8).as_deref(), Some("0.01%"));
        // More decimals than systemd reads.
        assert_eq!(cap("12.346%", 1).as_deref(), Some("12.35%"));
        assert!(cap("auto", 2).is_some_and(|c| c.ends_with('M')));
    }
}
