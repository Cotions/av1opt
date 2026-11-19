//! Benchmark of encoder settings on the user's own videos: for every setting (preset and
//! SVT-AV1 parameters) and every video, find the CRF that gives the target VMAF on a few sample
//! clips, and compare the settings by file size and encode time at that equal quality.
//!
//! Every sample encode is remembered in `$XDG_CACHE_HOME/av1opt/bench-v2.jsonl`, so a stopped
//! run resumes, and adding a setting or changing the target only measures what is new.

pub mod stats;

use crate::probe::MediaInfo;
use crate::scan;
use crate::search::{self, EncodeSettings, Sample};
use crate::tools::{Capabilities, Tools, path_arg};
use crate::tuning::{Choice, Class};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use stats::{Measure, Point, Summary};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Version of the report layout (`--json`, and what the page keeps). Bump it when a field
/// changes meaning or is removed.
pub const REPORT_FORMAT: u32 = 1;

pub const DEFAULT_PRESETS: [u8; 3] = [4, 6, 8];

/// Parameter sets tried by default: the three tunes that suit video, and two additions to the
/// default tune that cost no encode time.
pub const DEFAULT_PARAM_SETS: [&str; 5] = ["tune=0", "tune=1", "tune=2", "tune=0:enable-qm=1:qm-min=0", "tune=0:enable-variance-boost=1"];

/// One encoder setting under test.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Config {
    pub preset: u8,
    /// SVT-AV1 parameters besides the CRF.
    pub params: String,
}

impl Config {
    /// What `encode` uses when nothing is said; everything is compared against it.
    pub fn baseline() -> Config {
        Config { preset: search::DEFAULT_PRESET, params: search::DEFAULT_PARAMS.to_string() }
    }

    pub fn label(&self) -> String {
        format!("preset {} · {}", self.preset, if self.params.is_empty() { "SVT-AV1 defaults" } else { &self.params })
    }
}

/// The settings to test: the baseline first, then every preset with every parameter set.
pub fn matrix(presets: &[u8], param_sets: &[String]) -> Vec<Config> {
    let mut out = vec![Config::baseline()];
    for &preset in presets {
        for params in param_sets {
            let c = Config { preset, params: params.clone() };
            if !out.contains(&c) {
                out.push(c);
            }
        }
    }
    out
}

pub struct Options {
    /// Settings to compare; the first one is the baseline.
    pub configs: Vec<Config>,
    pub target_vmaf: f64,
    pub min_crf: u8,
    pub max_crf: u8,
    /// Sample clips per video and their length.
    pub samples: usize,
    pub sample_secs: f64,
    /// At most this many videos, spread over the folders and resolution classes.
    pub max_videos: usize,
    /// Parallel ffprobe runs while collecting the videos.
    pub jobs: usize,
    /// Measure again instead of using remembered sample encodes.
    pub refresh: bool,
    /// Where the lossless sample clips go while a video is measured (default: the cache folder).
    pub work_dir: Option<PathBuf>,
}

/// One setting on one video.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Cell {
    /// None when the target is out of reach or the encode failed.
    pub point: Option<Point>,
    pub trials: Vec<Measure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Video {
    pub path: PathBuf,
    pub class: Class,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub duration_s: f64,
    pub source_bitrate: f64,
    pub hdr: bool,
    /// One per setting, in the report's order; fewer while the video is being measured.
    pub cells: Vec<Cell>,
    /// Why nothing could be measured (sample clips could not be cut).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Skipped {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    pub format: u32,
    /// Unix time the run ended.
    pub made: u64,
    /// `ffmpeg -version`'s first line: results belong to this encoder build.
    pub encoder: String,
    pub target_vmaf: f64,
    pub min_crf: u8,
    pub max_crf: u8,
    pub samples: usize,
    pub sample_secs: f64,
    pub configs: Vec<Config>,
    /// Index of the setting the others are compared with.
    pub baseline: usize,
    pub videos: Vec<Video>,
    /// All videos together first, then each resolution class that has videos.
    pub summaries: Vec<Summary>,
    /// Videos found but not used, with the reason (unreadable, too short, over the limit).
    pub skipped: Vec<Skipped>,
    pub elapsed_secs: f64,
    /// Sample encodes run now, and taken from earlier runs.
    pub fresh: usize,
    pub cached: usize,
}

#[cfg_attr(not(feature = "ui"), allow(dead_code))]
pub enum Progress<'a> {
    Probing { done: usize, total: usize },
    /// This many videos cannot be used because their names are not valid UTF-8.
    LeftOut { videos: usize },
    /// The videos and settings are known; `cells` = videos x settings is the work to do.
    Plan { videos: usize, configs: usize },
    /// Cutting the sample clips of a video.
    Sampling { video: usize, path: &'a Path },
    /// One sample encode (or its remembered result).
    Trial { video: usize, config: usize, measure: &'a Measure, cached: bool },
    /// A setting is finished on a video.
    Cell { video: usize, config: usize, cell: &'a Cell },
    /// A video is finished under every setting.
    VideoDone { video: &'a Video },
}

/// Compares the settings over all videos and per resolution class.
pub fn summaries(videos: &[Video], configs: usize, baseline: usize) -> Vec<Summary> {
    let points: Vec<(Class, Vec<Option<Point>>)> = videos.iter().map(|v| (v.class, v.cells.iter().map(|c| c.point).collect())).collect();
    let of = |class: Option<Class>| {
        let set: Vec<&[Option<Point>]> = points.iter().filter(|(c, _)| class.is_none_or(|k| k == *c)).map(|(_, p)| p.as_slice()).collect();
        stats::summarize(class, &set, configs, baseline)
    };
    std::iter::once(None).chain(Class::ALL.into_iter().map(Some)).filter_map(of).collect()
}

/// A remembered sample encode. Everything that could change its result is part of the line.
#[derive(Serialize, Deserialize)]
struct Line {
    path: PathBuf,
    size: u64,
    mtime: u64,
    samples: usize,
    sample_secs: f64,
    encoder: String,
    preset: u8,
    params: String,
    #[serde(flatten)]
    m: Measure,
}

/// The remembered sample encodes. Read as bytes and line by line: one damaged line costs that
/// measurement, not all the others.
fn read_lines(path: &Path) -> Vec<Line> {
    let Ok(bytes) = std::fs::read(path) else { return Vec::new() };
    bytes.split(|b| *b == b'\n').filter_map(|l| serde_json::from_slice(l).ok()).collect()
}

pub fn cache_dir() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .context("no HOME or XDG_CACHE_HOME")?;
    let dir = base.join("av1opt");
    crate::files::private_folder(&dir)?;
    Ok(dir)
}

const WORK_PREFIX: &str = "bench-work-";

/// Lossless sample clips are big; the folder goes away with the run, and ones left by runs
/// that were killed (their process is gone) are removed by the next run.
struct WorkDir(PathBuf);

impl WorkDir {
    fn new(parent: &Path) -> Result<WorkDir> {
        crate::fsguard::ensure_writable_location(parent, false)?;
        std::fs::create_dir_all(parent)?;
        crate::files::remove_abandoned_folders(parent, WORK_PREFIX);
        Ok(WorkDir(crate::files::make_run_folder(parent, WORK_PREFIX)?))
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Encodes every sample at `crf`, timed, and scores it against its reference.
fn measure(tools: &Tools, info: &MediaInfo, samples: &[Sample], tmp: &Path, enc: &EncodeSettings, crf: u8, threads: usize) -> Result<Measure> {
    let (mut scores, mut bytes, mut clip_secs, mut encode_secs) = (Vec::new(), 0u64, 0.0, 0.0);
    for (i, s) in samples.iter().enumerate() {
        let out = tmp.join(format!("bench{i}.mkv"));
        let mut args: Vec<String> = ["-y", "-v", "error", "-i", &path_arg(&s.reference)].map(String::from).to_vec();
        args.extend(enc.video_args(crf as f64));
        args.push(path_arg(&out));
        let t = Instant::now();
        tools.run(&args)?;
        encode_secs += t.elapsed().as_secs_f64();
        scores.push(tools.vmaf(&out, &s.reference, search::vmaf_model(info), search::reference_pix_fmt(info), threads, 0, search::VMAF_STRIDE)?);
        bytes += std::fs::metadata(&out)?.len();
        clip_secs += s.secs;
        std::fs::remove_file(&out)?;
    }
    if scores.is_empty() {
        bail!("the video reports no length, so no sample clips could be cut from it and it was not measured; check that it plays");
    }
    Ok(Measure {
        crf,
        vmaf: scores.iter().sum::<f64>() / scores.len() as f64,
        vmaf_min: scores.iter().copied().fold(f64::INFINITY, f64::min),
        bytes,
        clip_secs,
        encode_secs,
    })
}

/// Runs the benchmark. `progress` is called from this thread, in order.
pub fn run(paths: &[PathBuf], tools: &Tools, o: &Options, progress: &(dyn Fn(Progress) + Sync)) -> Result<Report> {
    let started = Instant::now();
    if o.configs.is_empty() {
        bail!("no settings to compare");
    }
    if o.min_crf > o.max_crf {
        bail!("--min-crf {} is above --max-crf {}", o.min_crf, o.max_crf);
    }
    let caps = Capabilities::probe(&tools.ffmpeg, &tools.ffprobe);
    if let Some(p) = caps.problem(&tools.ffmpeg, &tools.ffprobe) {
        bail!("{p}");
    }
    let encoder = caps.ffmpeg_version.unwrap_or_default();
    // What remembered measurements are told apart by.
    let measured_with = format!("{encoder}; {} clips", search::REFERENCE_CODEC);
    for c in &o.configs {
        search::check_params(tools, c.preset, &c.params)?;
    }

    // The verdicts of `scan` aren't used here, so the built-in size model will do.
    let model = crate::estimate::Model::fit(&[], 93.0);
    let (mut found, _) = scan::collect(paths, &tools.ffprobe, o.jobs.max(1), 0.2, &model, &|done, total| progress(Progress::Probing { done, total }))?;
    tools.check_cancel()?;
    let not_text = scan::drop_unnamed(&mut found);
    if not_text > 0 {
        progress(Progress::LeftOut { videos: not_text });
    }
    let mut skipped: Vec<Skipped> = Vec::new();
    let mut usable: Vec<(Class, &scan::FileResult)> = Vec::new();
    let mut found: Vec<&scan::FileResult> = found.iter().collect();
    found.sort_by(|a, b| a.path.cmp(&b.path));
    for f in found {
        let skip = |reason: String| Skipped { path: f.path.clone(), reason };
        match &f.info {
            None => skipped.push(skip(f.error.clone().unwrap_or_else(|| "unreadable".into()))),
            Some(i) if i.dolby_vision => skipped.push(skip("Dolby Vision (not encoded by av1opt)".into())),
            Some(i) if i.duration_s < (2.0 * o.sample_secs).max(4.0) => skipped.push(skip("too short for sample clips".into())),
            Some(i) => usable.push((Class::of(i.width, i.height), f)),
        }
    }
    let chosen: Vec<&scan::FileResult> = stats::spread(&usable, o.max_videos.max(1));
    for (_, f) in &usable {
        if !chosen.iter().any(|c| c.path == f.path) {
            skipped.push(Skipped { path: f.path.clone(), reason: format!("over the limit of {} videos", o.max_videos.max(1)) });
        }
    }
    if chosen.is_empty() {
        bail!("no usable videos in the given folders");
    }
    progress(Progress::Plan { videos: chosen.len(), configs: o.configs.len() });

    let cache_path = cache_dir()?.join("bench-v2.jsonl");
    let lines = if o.refresh { Vec::new() } else { read_lines(&cache_path) };
    let work = WorkDir::new(&match &o.work_dir {
        Some(d) => d.clone(),
        None => cache_dir()?,
    })?;
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let (mut fresh, mut cached) = (0usize, 0usize);

    let mut videos: Vec<Video> = Vec::new();
    for (vi, f) in chosen.iter().enumerate() {
        let info = f.info.as_ref().expect("usable videos have a probe result");
        let mut video = Video {
            path: f.path.clone(), class: Class::of(info.width, info.height), codec: info.codec.clone(), width: info.width, height: info.height,
            fps: info.fps, duration_s: info.duration_s, source_bitrate: info.video_bitrate, hdr: info.hdr, cells: Vec::new(), error: None,
        };
        // Earlier measurements of this very file with these clips and this encoder build,
        // read from clips of the same kind (the time to read a clip is part of the timing).
        let mut known: HashMap<(u8, &str, u8), Measure> = lines
            .iter()
            .filter(|l| l.path == f.path && l.size == f.size && l.mtime == f.mtime && l.samples == o.samples && (l.sample_secs - o.sample_secs).abs() < 1e-6 && l.encoder == measured_with)
            .map(|l| ((l.preset, l.params.as_str(), l.m.crf), l.m))
            .collect();
        // 4K VMAF with many threads was part of the memory blow-up in the first pilot.
        let vmaf_threads = if info.width.max(info.height) >= 3200 { threads.min(4) } else { threads };
        // Cut only when something actually has to be encoded.
        let mut samples: Option<Vec<Sample>> = None;
        let mut start_crf = search::START_CRF;
        for (ci, c) in o.configs.iter().enumerate() {
            let enc = EncodeSettings { preset: c.preset, pix_fmt_out: "yuv420p10le", params: c.params.clone() };
            let mut trials: Vec<Measure> = Vec::new();
            let searched = search::find_crf(start_crf, o.min_crf, o.max_crf, o.target_vmaf, |crf| {
                let key = (c.preset, c.params.as_str(), crf);
                let (m, was_cached) = match known.get(&key) {
                    Some(m) => (*m, true),
                    None => {
                        if samples.is_none() {
                            progress(Progress::Sampling { video: vi, path: &f.path });
                            let pos = search::sample_positions(info.duration_s, o.samples.max(1), search::clip_secs(info, o.sample_secs));
                            samples = Some(search::extract_samples(tools, &f.path, info, &work.0, pos)?);
                        }
                        let m = measure(tools, info, samples.as_deref().unwrap_or_default(), &work.0, &enc, crf, vmaf_threads)?;
                        let line = Line {
                            path: f.path.clone(), size: f.size, mtime: f.mtime, samples: o.samples, sample_secs: o.sample_secs,
                            encoder: measured_with.clone(), preset: c.preset, params: c.params.clone(), m,
                        };
                        // Losing the memory of one encode is not worth failing the run for.
                        let _ = crate::files::append_line(&cache_path, &serde_json::to_string(&line)?, true);
                        known.insert(key, m);
                        (m, false)
                    }
                };
                if was_cached {
                    cached += 1;
                } else {
                    fresh += 1;
                }
                progress(Progress::Trial { video: vi, config: ci, measure: &m, cached: was_cached });
                trials.push(m);
                Ok(m.vmaf)
            });
            let cell = match searched {
                Ok(_) => {
                    trials.sort_by_key(|m| m.crf);
                    let point = stats::at_target(&trials, o.target_vmaf);
                    Cell { point, trials, error: None }
                }
                Err(e) if crate::events::is_cancelled(&e) => return Err(e),
                Err(e) => {
                    // Cutting the clips failed: no setting can be measured on this video.
                    if samples.is_none() && trials.is_empty() && video.cells.iter().all(|c| c.trials.is_empty()) {
                        video.error = Some(format!("{e:#}"));
                    }
                    Cell { point: None, trials, error: Some(format!("{e:#}")) }
                }
            };
            // The next setting's answer is usually close to this one's.
            if let Some(p) = &cell.point {
                start_crf = (p.crf.round() as u8).clamp(o.min_crf, o.max_crf);
            }
            progress(Progress::Cell { video: vi, config: ci, cell: &cell });
            video.cells.push(cell);
        }
        // Make room before the next video's clips.
        if let Some(s) = samples {
            for x in s {
                let _ = std::fs::remove_file(x.reference);
            }
        }
        progress(Progress::VideoDone { video: &video });
        videos.push(video);
    }

    let summaries = summaries(&videos, o.configs.len(), 0);
    Ok(Report {
        format: REPORT_FORMAT,
        made: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()),
        encoder,
        target_vmaf: o.target_vmaf,
        min_crf: o.min_crf,
        max_crf: o.max_crf,
        samples: o.samples,
        sample_secs: o.sample_secs,
        configs: o.configs.clone(),
        baseline: 0,
        videos,
        summaries,
        skipped,
        elapsed_secs: started.elapsed().as_secs_f64(),
        fresh,
        cached,
    })
}

/// Which of a summary's picks to act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pick {
    /// Smallest files without taking noticeably longer than the default settings.
    Balanced,
    /// Smallest files, whatever the encode time.
    Smallest,
    /// Fastest encodes without noticeably bigger files.
    Fastest,
}

impl Pick {
    pub fn of(self, s: &Summary) -> usize {
        match self {
            Pick::Balanced => s.picks.balanced,
            Pick::Smallest => s.picks.smallest,
            Pick::Fastest => s.picks.fastest,
        }
    }
}

/// The tuning entry for setting `config` as measured in summary `s`.
pub fn choice(report: &Report, s: &Summary, config: usize) -> Option<Choice> {
    let (c, row) = (report.configs.get(config)?, s.rows.iter().find(|r| r.config == config)?);
    Some(Choice { preset: c.preset, params: c.params.clone(), size: row.size, time: row.time, videos: s.videos, target_vmaf: report.target_vmaf, made: report.made })
}

/// The chosen pick of every resolution class the report has videos for.
pub fn tuning_picks(report: &Report, pick: Pick) -> Vec<(Class, Choice)> {
    report.summaries.iter().filter_map(|s| Some((s.class?, choice(report, s, pick.of(s))?))).collect()
}

fn percent(x: f64) -> String {
    format!("{:+.1}%", (x - 1.0) * 100.0)
}

pub fn print(r: &Report) {
    println!(
        "\n{} settings on {} videos at VMAF {} ({} sample encodes, {} from earlier runs, {})",
        r.configs.len(), r.videos.len(), r.target_vmaf, r.fresh + r.cached, r.cached, crate::tools::hms(r.elapsed_secs)
    );
    for v in &r.videos {
        if let Some(e) = &v.error {
            println!("not measured: {}: {}", v.path.display(), e.lines().next().unwrap_or("failed"));
            continue;
        }
        for (c, k) in v.cells.iter().zip(&r.configs).filter(|(c, _)| c.point.is_none()) {
            let why = c.error.as_ref().map_or("target out of reach".to_string(), |e| format!("failed: {}", e.lines().last().unwrap_or("")));
            println!("{}: {}: {why}", v.path.display(), k.label());
        }
    }
    if r.summaries.is_empty() {
        println!("the default settings could not be measured on any video; nothing to compare with");
        return;
    }
    let width = r.configs.iter().map(|c| c.label().chars().count()).max().unwrap_or(10).max(10);
    for s in &r.summaries {
        let title = s.class.map_or("All videos", Class::label);
        println!("\n{title}: {} video{}{}", s.videos, if s.videos == 1 { "" } else { "s" }, if s.class.is_some() && s.videos < 3 { " (few: treat with care)" } else { "" });
        println!("  {:<width$}  {:>8}  {:>17}  {:>7}  {:>9}  {:>5}", "setting", "size", "per video", "time", "speed", "CRF");
        let mut rows: Vec<&stats::Row> = s.rows.iter().collect();
        // Smallest first; settings without a result on every video after the others.
        rows.sort_by(|a, b| (a.missing > 0).cmp(&(b.missing > 0)).then(a.size.total_cmp(&b.size)));
        for row in rows {
            let c = &r.configs[row.config];
            let mut marks: Vec<&str> = Vec::new();
            if row.config == r.baseline {
                marks.push("default");
            }
            if row.pareto {
                marks.push("front");
            }
            if row.missing > 0 {
                marks.push("no result on some videos");
            }
            if row.size.is_nan() {
                println!("  {:<width$}  {:>8}  {:>17}  {:>7}  {:>9}  {:>5}  no result", c.label(), "", "", "", "", "");
                continue;
            }
            let range = if s.videos - row.missing > 1 && row.config != r.baseline { format!("{} to {}", percent(row.size_best), percent(row.size_worst)) } else { String::new() };
            println!(
                "  {:<width$}  {:>8}  {:>17}  {:>6.2}x  {:>8.2}x  {:>5.1}  {}",
                c.label(), percent(row.size), range, row.time, row.speed, row.crf, marks.join(", ")
            );
        }
        let say = |what: &str, i: usize| {
            let row = s.rows.iter().find(|x| x.config == i);
            match row {
                Some(row) if i != r.baseline => println!("  {what}: {} ({} size, {:.2}x time)", r.configs[i].label(), percent(row.size), row.time),
                _ => println!("  {what}: the default settings"),
            }
        };
        say("smallest files", s.picks.smallest);
        say("smallest at about the default's time (balanced)", s.picks.balanced);
        say("fastest at about the default's size", s.picks.fastest);
    }
    println!("\nsize and time are against the default settings at equal VMAF; speed is video seconds per encode second; \"front\": nothing else is both smaller and faster");
}

/// One row per video and setting.
const CSV_COLUMNS: &[&str] = &["path", "class", "width", "height", "preset", "params", "crf", "kbps", "encode_secs_per_video_sec", "vmaf", "capped", "trials", "error"];
/// The CSV of a benchmark, as a report file.
pub const CSV: crate::report_file::Kind = crate::report_file::Kind::Csv(CSV_COLUMNS);

pub fn write_csv(r: &Report, path: &Path, given: &[PathBuf]) -> Result<()> {
    let mut w = csv::Writer::from_writer(Vec::new());
    w.write_record(CSV_COLUMNS)?;
    for v in &r.videos {
        for (cell, c) in v.cells.iter().zip(&r.configs) {
            let p = cell.point;
            let num = |x: Option<f64>, digits: usize| x.map_or(String::new(), |x| format!("{x:.digits$}"));
            w.write_record([
                v.path.display().to_string(),
                v.class.id().to_string(),
                v.width.to_string(),
                v.height.to_string(),
                c.preset.to_string(),
                c.params.clone(),
                num(p.map(|p| p.crf), 2),
                num(p.map(|p| p.bitrate / 1000.0), 1),
                num(p.map(|p| p.cost), 3),
                num(p.map(|p| p.vmaf), 2),
                p.map_or(String::new(), |p| p.capped.to_string()),
                cell.trials.len().to_string(),
                cell.error.clone().or_else(|| v.error.clone()).unwrap_or_default().lines().next().unwrap_or("").to_string(),
            ])?;
        }
    }
    crate::report_file::write_csv(w, path, CSV, given)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_damaged_line_costs_only_that_measurement() {
        let line = |crf: u8| {
            let m = serde_json::json!({ "path": "/v/a.mkv", "size": 1, "mtime": 2, "samples": 2, "sample_secs": 6.0, "encoder": "e", "preset": 6, "params": "tune=0",
                "crf": crf, "vmaf": 93.0, "vmaf_min": 92.0, "bytes": 1000, "clip_secs": 12.0, "encode_secs": 3.0 });
            serde_json::to_vec(&m).unwrap()
        };
        let mut file = line(30);
        file.extend_from_slice(b"\n{\"path\":\"/v/\xff.mkv\"}\nnot json\n");
        file.extend(line(31));
        file.push(b'\n');
        let path = std::env::temp_dir().join(format!("av1opt-bench-lines-{}.jsonl", std::process::id()));
        std::fs::write(&path, file).unwrap();
        let crfs: Vec<u8> = read_lines(&path).iter().map(|l| l.m.crf).collect();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(crfs, [30, 31]);
    }

    #[test]
    fn matrix_starts_with_the_baseline_and_has_no_repeats() {
        let sets: Vec<String> = ["tune=0", "tune=1"].map(String::from).to_vec();
        let m = matrix(&[4, 6], &sets);
        assert_eq!(m[0], Config::baseline());
        let labels: Vec<String> = m.iter().map(Config::label).collect();
        assert_eq!(labels, ["preset 5 · tune=0", "preset 4 · tune=0", "preset 4 · tune=1", "preset 6 · tune=0", "preset 6 · tune=1"]);
        assert_eq!(matrix(&[5], &sets).len(), 2, "the baseline is not measured twice");
        assert_eq!(matrix(&[], &sets), [Config::baseline()]);
        assert_eq!(Config { preset: 8, params: String::new() }.label(), "preset 8 · SVT-AV1 defaults");
    }

    fn video(class: Class, kbps: &[Option<f64>]) -> Video {
        let cells = kbps
            .iter()
            .map(|k| Cell { point: k.map(|k| Point { crf: 35.0, bitrate: k * 1000.0, cost: 1.0, vmaf: 93.0, capped: false }), trials: Vec::new(), error: None })
            .collect();
        Video { path: "v.mkv".into(), class, codec: "h264".into(), width: 1920, height: 1080, fps: 30.0, duration_s: 60.0, source_bitrate: 5e6, hdr: false, cells, error: None }
    }

    #[test]
    fn summaries_cover_all_videos_then_each_class_present() {
        let videos = [
            video(Class::Hd, &[Some(1000.0), Some(900.0)]),
            video(Class::Hd, &[Some(2000.0), Some(1800.0)]),
            video(Class::Uhd, &[Some(8000.0), Some(8800.0)]),
            video(Class::Sd, &[Some(500.0), None]),
        ];
        let s = summaries(&videos, 2, 0);
        // The second setting has no result at 720p and below: shown there, with nothing to pick.
        assert_eq!(s.iter().map(|x| x.class).collect::<Vec<_>>(), [None, Some(Class::Sd), Some(Class::Hd), Some(Class::Uhd)]);
        assert_eq!((s[0].videos, s[0].left_out, s[0].rows[1].missing), (4, 0, 1));
        assert_eq!(s[1].rows[1].missing, 1);
        assert!((s[2].rows[1].size - 0.9).abs() < 1e-12);
        assert!((s[3].rows[1].size - 1.1).abs() < 1e-12);

        let report = Report {
            format: REPORT_FORMAT, made: 5, encoder: "ffmpeg".into(), target_vmaf: 93.0, min_crf: 20, max_crf: 55, samples: 2, sample_secs: 6.0,
            configs: vec![Config::baseline(), Config { preset: 6, params: "tune=1".into() }], baseline: 0,
            videos: videos.to_vec(), summaries: s, skipped: Vec::new(), elapsed_secs: 1.0, fresh: 0, cached: 0,
        };
        // 1080p gains 10% at the same time; the others keep the default.
        let picks = tuning_picks(&report, Pick::Balanced);
        assert_eq!(picks.iter().map(|(c, x)| (*c, x.params.as_str())).collect::<Vec<_>>(), [(Class::Sd, "tune=0"), (Class::Hd, "tune=1"), (Class::Uhd, "tune=0")]);
        assert_eq!(picks[1].1.videos, 2);
        assert!((picks[1].1.size - 0.9).abs() < 1e-12);
        assert!(choice(&report, &report.summaries[0], 7).is_none());
    }

    fn timed_video(class: Class, points: &[(f64, f64)]) -> Video {
        let mut v = video(class, &points.iter().map(|p| Some(p.0)).collect::<Vec<_>>());
        for (cell, p) in v.cells.iter_mut().zip(points) {
            cell.point.as_mut().unwrap().cost = p.1;
        }
        v
    }

    fn settings(n: u8) -> Vec<Config> {
        (0..n).map(|i| Config { preset: 4 + i, params: format!("tune={i}") }).collect()
    }

    fn report(videos: Vec<Video>, configs: Vec<Config>) -> Report {
        let summaries = summaries(&videos, configs.len(), 0);
        Report {
            format: REPORT_FORMAT, made: 5, encoder: "ffmpeg".into(), target_vmaf: 93.0, min_crf: 20, max_crf: 55, samples: 2, sample_secs: 6.0,
            configs, baseline: 0, videos, summaries, skipped: Vec::new(), elapsed_secs: 1.0, fresh: 0, cached: 0,
        }
    }

    #[test]
    fn the_settings_keep_the_order_given_and_lose_what_is_repeated() {
        let sets: Vec<String> = ["b", "a", "b"].map(String::from).to_vec();
        let got: Vec<(u8, &str)> = matrix(&[9, 7, 9], &sets).iter().skip(1).map(|c| (c.preset, if c.params == "a" { "a" } else { "b" })).collect();
        assert_eq!(got, [(9, "b"), (9, "a"), (7, "b"), (7, "a")]);
        assert_eq!(matrix(&[4, 6], &[]), [Config::baseline()]);
    }

    #[test]
    fn the_default_settings_are_the_ones_encode_uses() {
        let b = Config::baseline();
        assert_eq!((b.preset, b.params.as_str()), (search::DEFAULT_PRESET, search::DEFAULT_PARAMS));
        // Everything tried by default is different from each other.
        let sets: Vec<String> = DEFAULT_PARAM_SETS.map(String::from).to_vec();
        assert_eq!(matrix(&DEFAULT_PRESETS, &sets).len(), 1 + DEFAULT_PRESETS.len() * DEFAULT_PARAM_SETS.len());
    }

    #[test]
    fn with_no_video_or_no_default_result_there_is_nothing_to_compare() {
        assert!(summaries(&[], 2, 0).is_empty());
        assert!(summaries(&[video(Class::Hd, &[None, Some(900.0)])], 2, 0).is_empty());
    }

    #[test]
    fn a_class_where_the_default_failed_is_absent_and_its_video_left_out_of_the_whole() {
        let videos = [video(Class::Hd, &[Some(1000.0), Some(900.0)]), video(Class::Sd, &[None, Some(300.0)])];
        let s = summaries(&videos, 2, 0);
        assert_eq!(s.iter().map(|x| x.class).collect::<Vec<_>>(), [None, Some(Class::Hd)]);
        assert_eq!((s[0].videos, s[0].left_out), (1, 1));
    }

    #[test]
    fn a_video_still_being_measured_does_not_count_yet() {
        let videos = [video(Class::Hd, &[Some(1000.0), Some(900.0)]), video(Class::Hd, &[Some(1000.0)])];
        let s = summaries(&videos, 2, 0);
        assert_eq!((s[0].videos, s[0].left_out, s[1].videos, s[1].left_out), (1, 1, 1, 1));
    }

    #[test]
    fn each_pick_means_its_own_winner() {
        let s = Summary { class: None, videos: 1, left_out: 0, rows: Vec::new(), picks: stats::Picks { smallest: 1, balanced: 2, fastest: 3 } };
        assert_eq!((Pick::Smallest.of(&s), Pick::Balanced.of(&s), Pick::Fastest.of(&s)), (1, 2, 3));
        assert_eq!(serde_json::to_string(&Pick::Balanced).unwrap(), "\"balanced\"");
    }

    #[test]
    fn saved_settings_are_the_chosen_winner_of_each_class_and_not_of_the_whole() {
        // Setting 1 is smaller but slow, setting 2 fast at about the same size.
        let r = report(vec![timed_video(Class::Hd, &[(1000.0, 1.0), (800.0, 3.0), (1010.0, 0.5)])], settings(3));
        let params = |pick| tuning_picks(&r, pick).into_iter().map(|(c, x)| (c, x.params)).collect::<Vec<_>>();
        assert_eq!(params(Pick::Smallest), [(Class::Hd, "tune=1".to_string())]);
        assert_eq!(params(Pick::Balanced), [(Class::Hd, "tune=0".to_string())]);
        assert_eq!(params(Pick::Fastest), [(Class::Hd, "tune=2".to_string())]);
        assert!(tuning_picks(&report(Vec::new(), settings(3)), Pick::Balanced).is_empty());
    }

    #[test]
    fn a_choice_carries_what_the_benchmark_measured_for_it() {
        let r = report(vec![timed_video(Class::Uhd, &[(1000.0, 1.0), (800.0, 3.0)])], settings(2));
        let c = choice(&r, &r.summaries[1], 1).unwrap();
        assert_eq!((c.preset, c.params.as_str(), c.videos, c.target_vmaf, c.made), (5, "tune=1", 1, 93.0, 5));
        assert!((c.size - 0.8).abs() < 1e-12 && (c.time - 3.0).abs() < 1e-12);
        // A setting the report has but the summary does not.
        let empty = Summary { rows: Vec::new(), ..r.summaries[1].clone() };
        assert!(choice(&r, &empty, 1).is_none());
    }

    #[test]
    fn differences_from_the_default_are_written_with_their_sign() {
        assert_eq!(percent(1.0), "+0.0%");
        assert_eq!(percent(0.9), "-10.0%");
        assert_eq!(percent(1.25), "+25.0%");
    }

    #[test]
    fn the_csv_has_one_row_per_video_and_setting_with_blanks_for_what_is_missing() {
        let dir = std::env::temp_dir().join(format!("av1opt-bench-csv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut a = video(Class::Hd, &[Some(2000.0), None]);
        a.path = "/v/one, two.mkv".into();
        a.cells[1].error = Some("first line\nsecond line".into());
        let mut b = video(Class::Sd, &[None]);
        b.error = Some("no clips\nmore".into());
        let file = dir.join("r.csv");
        write_csv(&report(vec![a, b], settings(2)), &file, &[]).unwrap();
        let rows: Vec<Vec<String>> = csv::ReaderBuilder::new().has_headers(false).from_path(&file).unwrap().records().map(|r| r.unwrap().iter().map(String::from).collect()).collect();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(rows.len(), 4, "{rows:?}");
        assert_eq!(rows[0][0], "path");
        assert!(rows.iter().all(|r| r.len() == rows[0].len()), "{rows:?}");
        assert_eq!(rows[1], ["/v/one, two.mkv", "hd", "1920", "1080", "4", "tune=0", "35.00", "2000.0", "1.000", "93.00", "false", "0", ""]);
        assert_eq!(rows[2], ["/v/one, two.mkv", "hd", "1920", "1080", "5", "tune=1", "", "", "", "", "", "0", "first line"]);
        assert_eq!((rows[3][1].as_str(), rows[3][6].as_str(), rows[3][12].as_str()), ("sd", "", "no clips"));
    }

    #[test]
    fn no_file_of_remembered_encodes_is_nothing_remembered() {
        let path = std::env::temp_dir().join(format!("av1opt-bench-none-{}/lines.jsonl", std::process::id()));
        assert!(read_lines(&path).is_empty());
    }
}
