//! Benchmark of encoder settings on the user's own videos: for every setting (preset and
//! SVT-AV1 parameters) and every video, find the CRF that gives the target VMAF on a few sample
//! clips, and compare the settings by file size and encode time at that equal quality.
//!
//! Every sample encode is remembered in `$XDG_CACHE_HOME/av1opt/bench-v1.jsonl`, so a stopped
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
use std::io::Write;
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
        Config { preset: 6, params: search::DEFAULT_PARAMS.to_string() }
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

pub fn cache_dir() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .context("no HOME or XDG_CACHE_HOME")?;
    let dir = base.join("av1opt");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

const WORK_PREFIX: &str = "bench-work-";

/// Lossless sample clips are big; the folder goes away with the run, and ones left by runs
/// that were killed (their PID is gone) are removed by the next run.
struct WorkDir(PathBuf);

impl WorkDir {
    fn new(parent: &Path) -> Result<WorkDir> {
        crate::fsguard::ensure_writable_location(parent, false)?;
        std::fs::create_dir_all(parent)?;
        for e in std::fs::read_dir(parent)?.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(pid) = name.strip_prefix(WORK_PREFIX).and_then(|p| p.parse::<u32>().ok())
                && pid != std::process::id()
                && !Path::new(&format!("/proc/{pid}")).exists()
            {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
        let path = parent.join(format!("{WORK_PREFIX}{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path)?;
        Ok(WorkDir(path))
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
        bail!("no samples");
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
    for c in &o.configs {
        search::check_params(tools, c.preset, &c.params)?;
    }

    // The verdicts of `scan` aren't used here, so the built-in size model will do.
    let model = crate::estimate::Model::fit(&[], 93.0);
    let (found, _) = scan::collect(paths, &tools.ffprobe, o.jobs.max(1), 0.2, &model, &|done, total| progress(Progress::Probing { done, total }))?;
    tools.check_cancel()?;
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

    let cache_path = cache_dir()?.join("bench-v1.jsonl");
    let lines: Vec<Line> = if o.refresh {
        Vec::new()
    } else {
        std::fs::read_to_string(&cache_path).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
    };
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
        // Earlier measurements of this very file with these clips and this encoder build.
        let mut known: HashMap<(u8, &str, u8), Measure> = lines
            .iter()
            .filter(|l| l.path == f.path && l.size == f.size && l.mtime == f.mtime && l.samples == o.samples && (l.sample_secs - o.sample_secs).abs() < 1e-6 && l.encoder == encoder)
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
                            let pos = search::sample_positions(info.duration_s, o.samples.max(1), o.sample_secs);
                            samples = Some(search::extract_samples(tools, &f.path, info, &work.0, pos)?);
                        }
                        let m = measure(tools, info, samples.as_deref().unwrap_or_default(), &work.0, &enc, crf, vmaf_threads)?;
                        let line = Line {
                            path: f.path.clone(), size: f.size, mtime: f.mtime, samples: o.samples, sample_secs: o.sample_secs,
                            encoder: encoder.clone(), preset: c.preset, params: c.params.clone(), m,
                        };
                        // Losing the memory of one encode is not worth failing the run for.
                        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&cache_path) {
                            let _ = writeln!(file, "{}", serde_json::to_string(&line)?);
                        }
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
pub fn write_csv(r: &Report, path: &Path) -> Result<()> {
    let mut w = csv::Writer::from_path(path)?;
    w.write_record(["path", "class", "width", "height", "preset", "params", "crf", "kbps", "encode_secs_per_video_sec", "vmaf", "capped", "trials", "error"])?;
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
    w.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_starts_with_the_baseline_and_has_no_repeats() {
        let sets: Vec<String> = ["tune=0", "tune=1"].map(String::from).to_vec();
        let m = matrix(&[4, 6], &sets);
        assert_eq!(m[0], Config::baseline());
        let labels: Vec<String> = m.iter().map(Config::label).collect();
        assert_eq!(labels, ["preset 6 · tune=0", "preset 4 · tune=0", "preset 4 · tune=1", "preset 6 · tune=1"]);
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
}
