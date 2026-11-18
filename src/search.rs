//! Target-quality CRF search: encode short sample clips at several CRFs and pick the
//! highest CRF (smallest file) whose mean VMAF still meets the target.

use crate::probe::MediaInfo;
use crate::tools::{Tools, path_arg};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct Sample {
    pub reference: PathBuf,
    pub secs: f64,
    /// Frames in the reference.
    pub frames: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Trial {
    /// Integer for real trials; the chosen trial may be interpolated to a quarter step.
    pub crf: f64,
    pub mean_vmaf: f64,
    pub min_vmaf: f64,
    /// Predicted video stream size relative to the source video stream.
    pub size_ratio: f64,
}

/// SVT-AV1 parameters used when none are given. SVT-AV1's own default is tune=1 (PSNR).
pub const DEFAULT_PARAMS: &str = "tune=0";

#[derive(Clone)]
pub struct EncodeSettings {
    pub preset: u8,
    pub pix_fmt_out: &'static str,
    /// SVT-AV1 parameters as `key=value:key=value`, without the CRF.
    pub params: String,
}

/// Key frames at most this far apart, unless the parameters set `keyint` themselves.
/// SVT-AV1's own default is about five seconds. Every key frame is paid for in bits, and at
/// ten seconds the same VMAF took 3.3% less on eight 10-second clips of real videos (2.6%
/// less on four of 40 seconds), with VMAF NEG, XPSNR and SSIMULACRA2 agreeing and the encode
/// no slower. Seeking lands on key frames: a player shows the frame asked for after decoding
/// from the one before it, up to ten seconds of video instead of five.
pub const KEYINT: &str = "10s";

/// Parameters added to whatever set is in use, unless it names the key itself: the key
/// frame gap above, and quantization matrices (coarser steps for the fine detail an eye
/// misses first; SVT-AV1 leaves them off). With them the same VMAF took 1.1% less on eight
/// clips, less on every one of them, with VMAF NEG and SSIMULACRA2 saying the same and XPSNR
/// seeing no difference, for 0.5% more encoding time.
const IMPLIED: &[(&str, &str)] = &[("keyint", KEYINT), ("enable-qm", "1")];

impl EncodeSettings {
    /// The parameters the encoder is given besides the CRF: the set in use and what is
    /// added to every set (see [`IMPLIED`]).
    pub fn full_params(&self) -> String {
        let sets = |key: &str| self.params.split(':').any(|p| p.split_once('=').is_some_and(|(k, _)| k.trim() == key));
        let own = (!self.params.is_empty()).then(|| self.params.clone());
        let implied = IMPLIED.iter().filter(|(k, _)| !sets(k)).map(|(k, v)| format!("{k}={v}"));
        own.into_iter().chain(implied).collect::<Vec<_>>().join(":")
    }

    /// SVT-AV1 takes CRF in quarter steps through its own parameters (ffmpeg's -crf is integer).
    pub fn video_args(&self, crf: f64) -> Vec<String> {
        let params = format!("{}:crf={}", self.full_params(), quarter(crf));
        // Stream specifiers keep these off copied cover-art streams.
        ["-c:v:0", "libsvtav1", "-preset:v:0", &self.preset.to_string(), "-pix_fmt:v:0", self.pix_fmt_out, "-svtav1-params:v:0", &params]
            .map(String::from)
            .to_vec()
    }
}

/// Checks and tidies a `key=value:key=value` list of SVT-AV1 parameters. Parameters the CRF
/// search sets itself are refused.
pub fn parse_params(s: &str) -> Result<String> {
    const OWNED: [&str; 9] = ["crf", "qp", "rc", "tbr", "mbr", "preset", "passes", "pass", "aq-mode-crf"];
    let mut out: Vec<String> = Vec::new();
    for part in s.split(':').map(str::trim).filter(|p| !p.is_empty()) {
        let Some((k, v)) = part.split_once('=').map(|(k, v)| (k.trim(), v.trim())) else { bail!("SVT-AV1 parameter '{part}' is not key=value") };
        let plain = |x: &str| !x.is_empty() && x.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
        if !plain(k) || !plain(v) {
            bail!("SVT-AV1 parameter '{part}': names and values are letters, digits, '-', '_' and '.'");
        }
        if OWNED.contains(&k) {
            bail!("SVT-AV1 parameter '{k}' is set by av1opt (CRF search and --preset)");
        }
        if out.iter().any(|o| o.split('=').next() == Some(k)) {
            bail!("SVT-AV1 parameter '{k}' is given twice");
        }
        out.push(format!("{k}={v}"));
    }
    Ok(out.join(":"))
}

/// Tries `params` on a few generated frames: ffmpeg only warns about names SVT-AV1 doesn't
/// know and carries on without them, which would quietly encode with other settings.
pub fn check_params(tools: &Tools, preset: u8, params: &str) -> Result<()> {
    let enc = EncodeSettings { preset: preset.max(8), pix_fmt_out: "yuv420p10le", params: params.to_string() };
    let mut args: Vec<String> = ["-v", "warning", "-f", "lavfi", "-i", "testsrc2=s=256x144:r=24:d=0.5"].map(String::from).to_vec();
    args.extend(enc.video_args(35.0));
    args.extend(["-f", "null", "-"].map(String::from));
    let stderr = match tools.run(&args) {
        Ok(s) => s,
        Err(e) if crate::events::is_cancelled(&e) => return Err(e),
        Err(e) => {
            let text = format!("{e:#}");
            let why = text.lines().find(|l| l.contains("Svt[error]")).map(|l| l.split("Svt[error]:").last().unwrap_or(l).trim().to_string());
            bail!("SVT-AV1 refuses '{params}'{}", why.map_or(String::new(), |w| format!(": {w}")));
        }
    };
    if let Some(bad) = stderr.lines().find_map(|l| l.split("Error parsing option ").nth(1)) {
        bail!("SVT-AV1 doesn't know the parameter '{}' (in '{params}')", bad.split(':').next().unwrap_or(bad).trim());
    }
    Ok(())
}

/// Rounds down to SVT-AV1's quarter-step CRF grid.
pub fn quarter(crf: f64) -> f64 {
    (crf * 4.0).floor() / 4.0
}

/// Lossless reference pixel format matching the source bit depth.
/// Codec of the lossless references (sample clips, scene chunks). Every trial reads each clip
/// twice, as encoder input and as VMAF reference, so what counts is how cheap it is to
/// decode: FFV1 took 40 ms of processor time per 1080p frame, a fifth of a whole trial;
/// this one takes 8 ms, for files twice the size (250 MB for 8 s of 1080p, 700 MB of 4K).
pub const REFERENCE_CODEC: &str = "ffvhuff";

pub fn reference_pix_fmt(info: &MediaInfo) -> &'static str {
    if info.pix_fmt.contains("10") || info.pix_fmt.contains("12") { "yuv420p10le" } else { "yuv420p" }
}

pub fn vmaf_model(info: &MediaInfo) -> &'static str {
    if info.width.max(info.height) >= 3200 { "vmaf_4k_v0.6.1" } else { "vmaf_v0.6.1" }
}

/// Constant frame rate used for references, so frames line up for VMAF even on VFR sources.
pub fn cfr(info: &MediaInfo) -> String {
    const STD: [(f64, &str); 8] = [
        (23.976, "24000/1001"), (24.0, "24"), (25.0, "25"), (29.97, "30000/1001"),
        (30.0, "30"), (50.0, "50"), (59.94, "60000/1001"), (60.0, "60"),
    ];
    // A steady stream is on the grid it declares. Its average can sit elsewhere: a webcam's
    // 30 fps with drifting frame times averages 29.96, and a 30000/1001 grid would drop a
    // frame in every thousand of it.
    let fps = if !info.vfr && info.nominal_fps > 0.0 { info.nominal_fps } else { info.fps };
    // Closest standard rate, not the first within tolerance: 24 must not become 23.976,
    // which drops a frame every ~42 and misaligns VMAF comparisons.
    STD.iter()
        .min_by(|a, b| (a.0 - fps).abs().total_cmp(&(b.0 - fps).abs()))
        .filter(|(f, _)| (f - fps).abs() / f < 0.01)
        .map_or_else(|| format!("{fps:.3}"), |(_, s)| s.to_string())
}

/// Numeric value of [`cfr`], i.e. frames per second of the constant-rate stream.
/// That rate as people write it: "23.98", "30", "27.3".
pub fn cfr_label(info: &MediaInfo) -> String {
    let s = format!("{:.2}", cfr_value(info));
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

pub fn cfr_value(info: &MediaInfo) -> f64 {
    let r = cfr(info);
    match r.split_once('/') {
        Some((n, d)) => n.parse::<f64>().unwrap_or(0.0) / d.parse::<f64>().unwrap_or(1.0),
        None => r.parse().unwrap_or(info.fps),
    }
}

/// Filters every encode of this source needs: deinterlace, and crop to even dimensions
/// (4:2:0 AV1 can't have odd sizes). The same filters go into the references, so VMAF
/// compares like with like.
pub fn source_filters(info: &MediaInfo) -> Vec<String> {
    let mut f = Vec::new();
    if info.interlaced {
        f.push("bwdif=mode=send_frame".to_string());
    }
    if info.width % 2 == 1 || info.height % 2 == 1 {
        f.push(format!("crop={}:{}:0:0", info.width & !1, info.height & !1));
    }
    f
}

/// Score every 3rd frame: ~45% less VMAF time. The mean of a trial's samples stays within
/// ~0.1 of scoring every frame (one 8 s sample by itself: within ~0.25). Must be odd:
/// SVT-AV1's hierarchical GOP alternates better and worse frames, so an even stride sees only
/// one kind (measured +0.2 VMAF with a stride of 2).
pub const VMAF_STRIDE: u32 = 3;

pub const PREROLL_SECS: f64 = 2.0;

/// Moves a seek point to a quarter of a frame before the nearest frame. A seek that lands on
/// a frame (or half-way between two) is decided by the last digit of a timestamp, and
/// containers round those differently: Matroska keeps milliseconds, so the same frame is at
/// 2.2 s in an MP4 source and at 2.200 or 2.199 s in its MKV output. A quarter frame is
/// further from both edges than any such rounding.
/// `first_frame` is the time of the video's first frame after the start of the file.
pub fn snap_seek(t: f64, fps: f64, first_frame: f64) -> f64 {
    if !(fps > 0.0 && t > 0.0) {
        return t.max(0.0);
    }
    let frame = ((t - first_frame) * fps + 0.25).round();
    (first_frame + (frame - 0.25) / fps).max(0.0)
}

/// Lossless, constant-frame-rate cut of `input` starting at `start` seconds (moved by less
/// than a frame, see [`snap_seek`]). `is_source` applies [`source_filters`]; pass false when
/// cutting from an already-encoded output. `first_frame` is the time of this file's first
/// video frame after the start of the file.
#[allow(clippy::too_many_arguments)]
pub fn extract_reference(
    tools: &Tools,
    input: &Path,
    info: &MediaInfo,
    is_source: bool,
    first_frame: f64,
    start: f64,
    secs: f64,
    out: &Path,
) -> Result<()> {
    let mut vf = if is_source { source_filters(info) } else { Vec::new() };
    // Time counts from the first frame kept before frames are put on the constant-rate grid:
    // every frame is then at a whole number of frame times, give or take the container's
    // rounding, and lands in its own slot. Counted from the seek point instead, a cut half-way
    // between two frames puts every frame on the edge between two slots, where millisecond
    // rounding decides: an MKV output then came out with every third frame doubled and the
    // next one dropped, while its MP4 source did not (seen as a spot check of VMAF 70).
    vf.push("setpts=PTS-STARTPTS".into());
    vf.push(format!("fps={}", cfr(info)));
    // Seeking straight to `start` can begin decoding mid-GOP (MPEG-2/TS, open-GOP H.264) and
    // return frames from the wrong place. Seek a little earlier and discard the warm-up after
    // decoding (output-side -ss), so the decoder has a clean run-in.
    let preroll = start.min(PREROLL_SECS);
    let seek = snap_seek(start - preroll, cfr_value(info), first_frame);
    let args: Vec<String> = [
        "-y", "-v", "error", "-ss", &format!("{seek:.4}"), "-i", &path_arg(input),
        "-ss", &format!("{preroll:.3}"), "-t", &format!("{secs:.3}"),
        "-map", &format!("0:{}", if is_source { info.video_index } else { 0 }), "-an", "-sn",
        "-vf", &vf.join(","), "-c:v", REFERENCE_CODEC, "-pix_fmt", reference_pix_fmt(info), &path_arg(out),
    ]
    .map(String::from)
    .to_vec();
    tools.run(&args).map(|_| ())
}

/// When a file's first video frame is shown, counted from the start of the file (other
/// streams can start earlier).
pub fn first_frame(info: &MediaInfo) -> f64 {
    (info.video_start - info.start_time).max(0.0)
}

/// How long a sample clip is unless `--sample-secs` says otherwise: about what a spot check
/// looks at, so that a sample can score as low as a spot check of the same stretch will.
pub const SAMPLE_SECS: f64 = 4.0;

/// How many sample clips a video of this length gets unless `--samples` says otherwise:
/// one for about every two minutes, so that two minutes that are harder than the rest
/// can't lie between samples, with 8 at least and 32 at most.
pub fn sample_count(duration: f64) -> usize {
    ((duration / 120.0).round() as usize).clamp(8, 32)
}

/// Sample start times: up to `count` clips of `secs` seconds, spread over the video and
/// skipping the first and last 5% (intros, credits). Returns (start, length) pairs.
pub fn sample_positions(duration: f64, count: usize, secs: f64) -> Vec<(f64, f64)> {
    if duration <= 0.0 {
        return Vec::new();
    }
    let secs = secs.min(duration);
    let n = ((duration / (secs * 2.0)) as usize).clamp(1, count.max(1));
    let span = (duration * 0.9 - secs).max(0.0);
    (0..n)
        .map(|i| {
            let start = if span == 0.0 { 0.0 } else { duration * 0.05 + span * (i as f64 + 0.5) / n as f64 };
            (start, secs)
        })
        .collect()
}

/// How long a sample clip is when the search only has to find the video's mean, which is
/// when the whole video is checked after its encode (see [`crate::repair`]): the weak
/// stretches are found there, so a sample need not be long enough to score like one.
/// Measured on three 15-minute videos (2026-11-18): a sample encoded among others scores
/// within 0.2 VMAF of the same frames in the encode of the whole video, at 1 s as at 4 s.
/// What the samples' mean misses of the video's is where they lie, not how they are encoded:
/// 8 clips of 4 s were off by 2.4 VMAF on one video (1.3 on average over where the grid can
/// lie), 64 clips of 1 s by 0.3. So the same seconds go to many short clips.
pub const MEAN_SAMPLE_SECS: f64 = 1.0;

/// How many sample clips a search for the mean alone takes: one for about every 8 seconds
/// of video, 8 at least and 64 at most.
pub fn mean_sample_count(duration: f64) -> usize {
    ((duration / 8.0).round() as usize).clamp(8, 64)
}

/// Sample start times for a search that only needs the video's mean: `count` clips of `secs`
/// seconds, each in the middle of its share of the whole video. The start and the end are
/// part of the mean like the rest (leaving 5% out at each end read 0.24 VMAF high on a
/// video whose first minute was its hardest).
pub fn mean_sample_positions(duration: f64, count: usize, secs: f64) -> Vec<(f64, f64)> {
    if duration <= 0.0 {
        return Vec::new();
    }
    let secs = secs.min(duration);
    let n = ((duration / (secs * 2.0)) as usize).clamp(1, count.max(1));
    let span = duration - secs;
    (0..n).map(|i| (span * (i as f64 + 0.5) / n as f64, secs)).collect()
}

/// Cuts the sample clips at `pos` ((start, length) pairs in source seconds), a few at a time:
/// a cut is a seek, a second or so of decoding and a small lossless encode, and a search for
/// the mean wants up to 64 of them.
pub fn extract_samples(tools: &Tools, input: &Path, info: &MediaInfo, tmp: &Path, pos: Vec<(f64, f64)>) -> Result<Vec<Sample>> {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let n = pos.len();
    let workers = (std::thread::available_parallelism().map_or(4, |c| c.get()) / 3).clamp(1, 4);
    let (next, failed) = (AtomicUsize::new(0), AtomicBool::new(false));
    let done: Vec<std::sync::Mutex<Option<Result<Sample>>>> = (0..n).map(|_| std::sync::Mutex::new(None)).collect();
    let first = first_frame(info);
    std::thread::scope(|s| {
        for _ in 0..workers.min(n) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    if i >= n || failed.load(Ordering::SeqCst) {
                        break;
                    }
                    let (start, secs) = pos[i];
                    let reference = tmp.join(format!("sample{i}.ref.mkv"));
                    eprintln!("  sample {}/{n} at {} ({secs:.0}s)", i + 1, crate::tools::hms(start));
                    tools.emit(crate::events::Event::Phase { phase: "sampling".into(), detail: format!("sample {}/{n} at {}", i + 1, crate::tools::hms(start)) });
                    let cut = extract_reference(tools, input, info, true, first, start, secs, &reference)
                        .and_then(|_| tools.count_frames(&reference))
                        .map(|frames| Sample { reference, secs, frames });
                    if cut.is_err() {
                        failed.store(true, Ordering::SeqCst);
                    }
                    *done[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(cut);
                }
            });
        }
    });
    // In order, and the first failure as it is; clips never started after one are not missed.
    let mut samples = Vec::with_capacity(n);
    for slot in done {
        match slot.into_inner().unwrap_or_else(|e| e.into_inner()) {
            Some(Ok(sample)) => samples.push(sample),
            Some(Err(e)) => return Err(e),
            None => {}
        }
    }
    if samples.len() != n {
        bail!("the sample clips could not all be cut");
    }
    Ok(samples)
}

/// CRF trials measured in earlier runs, kept in `$XDG_CACHE_HOME/av1opt/trials-v1.jsonl`: an
/// encode that was stopped, failed after its search, or is run again with another target or
/// container doesn't measure the same CRFs twice. A trial counts only for the same file
/// (path, size, date), the same sample clips and the same ffmpeg build.
pub struct TrialCache {
    path: Option<PathBuf>,
    /// What the trials were measured on, as one string.
    file: String,
    /// By encoder settings (see [`settings_key`]) and CRF in quarter steps.
    known: std::collections::HashMap<(String, u32), Trial>,
}

/// Everything about an encode besides the CRF, as the cache names it.
fn settings_key(preset: u8, pix_fmt: &str, params: &str) -> String {
    format!("{preset}|{pix_fmt}|{params}")
}

#[derive(Serialize, Deserialize)]
struct CacheLine {
    file: String,
    preset: u8,
    pix_fmt: String,
    params: String,
    time: u64,
    #[serde(flatten)]
    trial: Trial,
}

/// The cache is tidied once it holds more lines than this: the newest half stays.
const CACHE_MAX_LINES: usize = 40_000;

impl TrialCache {
    /// A cache that remembers nothing and writes nothing.
    pub fn off() -> Self {
        TrialCache { path: None, file: String::new(), known: Default::default() }
    }

    /// Opens the cache for one video and its sample positions. `reuse` false starts empty
    /// (trials are measured again) but still records what is measured.
    pub fn open(input: &Path, size: u64, positions: &[(f64, f64)], encoder: &str, reuse: bool) -> Self {
        let Ok(dir) = crate::bench::cache_dir() else { return Self::off() };
        Self::open_at(dir.join("trials-v1.jsonl"), input, size, positions, encoder, reuse)
    }

    fn open_at(path: PathBuf, input: &Path, size: u64, positions: &[(f64, f64)], encoder: &str, reuse: bool) -> Self {
        let mtime = std::fs::metadata(input).ok().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
        let abs = std::fs::canonicalize(input).unwrap_or_else(|_| input.to_path_buf());
        let clips: Vec<String> = positions.iter().map(|(start, secs)| format!("{start:.3}+{secs:.3}")).collect();
        let file = format!("{}|{size}|{mtime}|{}|{encoder}", abs.display(), clips.join(","));
        let mut known = std::collections::HashMap::new();
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        if reuse {
            // The file name is in every line; skip the JSON of the others.
            let needle = serde_json::to_string(&file).unwrap_or_default();
            for l in text.lines().filter(|l| l.contains(&needle)) {
                if let Ok(c) = serde_json::from_str::<CacheLine>(l)
                    && c.file == file
                {
                    known.insert((settings_key(c.preset, &c.pix_fmt, &c.params), quarters(c.trial.crf)), c.trial);
                }
            }
        }
        if text.lines().count() > CACHE_MAX_LINES {
            let keep: Vec<&str> = text.lines().skip(text.lines().count() - CACHE_MAX_LINES / 2).collect();
            let tmp = path.with_extension("jsonl.tmp");
            if std::fs::write(&tmp, keep.join("\n") + "\n").is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
        TrialCache { path: Some(path), file, known }
    }

    fn get(&self, enc: &EncodeSettings, crf: f64) -> Option<Trial> {
        self.known.get(&(settings_key(enc.preset, enc.pix_fmt_out, &enc.full_params()), quarters(crf))).copied()
    }

    /// Every trial known for these settings at a whole CRF, lowest CRF first.
    fn whole(&self, enc: &EncodeSettings) -> Vec<Trial> {
        let key = settings_key(enc.preset, enc.pix_fmt_out, &enc.full_params());
        let mut all: Vec<Trial> = self.known.iter().filter(|((k, q), _)| *k == key && q % 4 == 0).map(|(_, t)| *t).collect();
        all.sort_by(|a, b| a.crf.total_cmp(&b.crf));
        all
    }

    fn put(&mut self, enc: &EncodeSettings, t: Trial) {
        self.known.insert((settings_key(enc.preset, enc.pix_fmt_out, &enc.full_params()), quarters(t.crf)), t);
        let Some(path) = &self.path else { return };
        let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let line = CacheLine { file: self.file.clone(), preset: enc.preset, pix_fmt: enc.pix_fmt_out.to_string(), params: enc.full_params(), time, trial: t };
        // Losing the memory of one trial is not worth failing the encode for.
        if let (Ok(json), Ok(mut f)) = (serde_json::to_string(&line), std::fs::OpenOptions::new().create(true).append(true).open(path)) {
            use std::io::Write;
            let _ = writeln!(f, "{json}");
        }
    }
}

/// A CRF as a whole number of quarter steps.
fn quarters(crf: f64) -> u32 {
    (crf * 4.0).round().max(0.0) as u32
}

/// Where trials are run for one video: cuts the sample clips when the first trial that
/// isn't known from an earlier run needs them, and remembers every trial.
pub struct Lab<'a> {
    pub tools: &'a Tools,
    pub info: &'a MediaInfo,
    input: &'a Path,
    tmp: &'a Path,
    positions: Vec<(f64, f64)>,
    samples: Vec<Sample>,
    vmaf_threads: usize,
    cache: TrialCache,
    /// CRFs (in quarter steps) of the trials a search has listed.
    shown: std::collections::HashSet<u32>,
}

impl<'a> Lab<'a> {
    pub fn new(tools: &'a Tools, input: &'a Path, info: &'a MediaInfo, tmp: &'a Path, positions: Vec<(f64, f64)>, vmaf_threads: usize, cache: TrialCache) -> Self {
        Lab { tools, info, input, tmp, positions, samples: Vec::new(), vmaf_threads, cache, shown: Default::default() }
    }

    /// The trial at `crf`, and whether it was known from an earlier run.
    pub fn trial(&mut self, enc: &EncodeSettings, crf: f64) -> Result<(Trial, bool)> {
        if let Some(t) = self.cache.get(enc, crf) {
            return Ok((t, true));
        }
        if self.samples.is_empty() {
            self.samples = extract_samples(self.tools, self.input, self.info, self.tmp, self.positions.clone())?;
        }
        let t = trial(self.tools, self.info, &self.samples, self.tmp, enc, crf, self.vmaf_threads, self.info.video_bitrate / 8.0)?;
        self.cache.put(enc, t);
        Ok((t, false))
    }

    /// Deletes the sample clips (they are large); a later trial would cut them again.
    pub fn discard_samples(&mut self) {
        for s in self.samples.drain(..) {
            let _ = std::fs::remove_file(s.reference);
        }
    }
}

#[derive(Clone, Copy)]
pub struct SearchParams {
    pub target_vmaf: f64,
    pub min_crf: u8,
    pub max_crf: u8,
    /// The first CRF tried; the closer to the answer, the fewer trials.
    pub start_crf: u8,
    /// Give up once the answer is known to come out larger than this share of the source
    /// video (None: search to the end whatever the size).
    pub max_size_ratio: Option<f64>,
    /// VMAF the worst sample has to reach as well (None: only the mean counts).
    pub worst_sample: Option<f64>,
}

impl SearchParams {
    /// What a trial is worth to the search: its mean, lowered by what its worst sample
    /// lacks. It reaches the target when the mean does and the worst sample its own mark.
    pub fn score(&self, t: &Trial) -> f64 {
        match self.worst_sample {
            Some(floor) => t.mean_vmaf.min(t.min_vmaf + (self.target_vmaf - floor)),
            None => t.mean_vmaf,
        }
    }
}

/// What the CRF search concluded.
pub enum Outcome {
    /// Encode at this trial's CRF; its scores and size are what the line through the two
    /// nearest measurements predicts.
    Found(Trial),
    /// Even the lowest CRF allowed misses the target.
    OutOfReach,
    /// This trial misses the target and is already larger than wanted; a lower CRF, which
    /// the target needs, is larger still.
    TooLarge(Trial),
}

/// Ends the search from inside a trial.
#[derive(Debug)]
struct TooLarge;

impl std::fmt::Display for TooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the encode would be too large")
    }
}

impl std::error::Error for TooLarge {}

/// First guess when nothing is known about the video: measured curves of eight clips of
/// different kinds cross VMAF 93 at CRF 35 on average (28 to 39).
pub const START_CRF: u8 = 35;

/// Winning CRF (None if even the minimum fails) and every (crf, score) evaluated.
pub type CrfSearch = (Option<u8>, Vec<(u8, f64)>);
/// Usual VMAF drop per CRF step for SVT-AV1 around [`START_CRF`] (measured on the same clips:
/// 0.3 to 0.6 there, 0.47 on average, and about 0.5 where the score passes 93).
pub const VMAF_PER_CRF: f64 = 0.5;

/// How much that drop grows with each CRF step: on the measured curves it went from about
/// 0.15 VMAF per step at CRF 22 to 0.65 at CRF 44.
const SLOPE_RISE: f64 = 0.02;

/// CRF where a curve of the usual shape through one measured point reaches `target`. A
/// straight line with the usual slope steps too far toward low CRFs, where the curve flattens,
/// and not far enough toward high ones.
fn usual_crossing(crf: f64, score: f64, target: f64) -> f64 {
    let slope = (VMAF_PER_CRF + SLOPE_RISE * (crf - START_CRF as f64)).max(0.1);
    // score - target = slope·x + SLOPE_RISE/2·x², x being the way to go. Past the flat top
    // of that parabola no CRF reaches the target: the top is as far as the shape can tell.
    let reach = (slope * slope + 2.0 * SLOPE_RISE * (score - target)).max(0.0);
    crf + (reach.sqrt() - slope) / SLOPE_RISE
}

/// Finds the highest CRF in `min..=max` whose score is >= `target`, assuming the score falls
/// as CRF rises. Instead of halving the range, it interpolates between the nearest passing
/// and failing CRFs, which usually needs 3-4 evaluations instead of 5-6.
/// Returns the winning CRF (None if even `min` fails) and every (crf, score) evaluated.
pub fn find_crf(start: u8, min: u8, max: u8, target: f64, mut eval: impl FnMut(u8) -> Result<f64>) -> Result<CrfSearch> {
    if min > max {
        bail!("--min-crf {min} is above --max-crf {max}");
    }
    let mut tried: BTreeMap<u8, f64> = BTreeMap::new();
    let mut crf = start.clamp(min, max);
    loop {
        let score = eval(crf)?;
        tried.insert(crf, score);
        // Highest passing and lowest failing CRF seen so far.
        let pass = tried.iter().filter(|(_, v)| **v >= target).map(|(c, v)| (*c, *v)).max_by_key(|(c, _)| *c);
        let fail = tried.iter().filter(|(_, v)| **v < target).map(|(c, v)| (*c, *v)).min_by_key(|(c, _)| *c);
        // With scores on one side of the target only: along the line through the two nearest
        // of them, or along the usual shape of the curve when there is just one.
        let step = |c: u8, v: f64, other: Option<(&u8, &f64)>| {
            let to = match other {
                Some((&b, &vb)) => (v - target).abs() / ((vb - v).abs() / c.abs_diff(b) as f64).clamp(0.1, 2.0),
                None => (usual_crossing(c as f64, v, target) - c as f64).abs(),
            };
            to.round().max(1.0) as u8
        };
        let next = match (pass, fail) {
            (Some((p, _)), Some((f, _))) if f <= p + 1 => None,
            (Some((p, vp)), Some((f, vf))) => {
                let frac = if vp > vf { (vp - target) / (vp - vf) } else { 0.5 };
                let c = p as f64 + frac * (f - p) as f64;
                Some((c.round() as u8).clamp(p + 1, f - 1))
            }
            (Some((p, _)), None) if p >= max => None,
            (Some((p, vp)), None) => Some(p.saturating_add(step(p, vp, tried.range(..p).next_back())).min(max)),
            (None, Some((f, _))) if f <= min => None,
            (None, Some((f, vf))) => Some(f.saturating_sub(step(f, vf, tried.range(f + 1..).next())).max(min)),
            (None, None) => unreachable!("just inserted a trial"),
        };
        match next {
            // Non-monotonic scores could revisit a CRF; stop rather than loop.
            Some(c) if !tried.contains_key(&c) => crf = c,
            _ => {
                let best = tried.iter().filter(|(_, v)| **v >= target).map(|(c, _)| *c).max();
                return Ok((best, tried.into_iter().collect()));
            }
        }
    }
}

/// Where the score crosses the target, and the measurements the answer rests on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Crossing {
    /// Quarter-step CRF expected to just meet the target.
    pub crf: f64,
    /// The two measured CRFs it was read from (the same one twice: `crf` itself was measured).
    pub from: (u8, u8),
}

/// How far an estimate may sit from the two measurements it is read from: the product of its
/// distances to them, in CRF steps. A straight line through two points of a curve is off by
/// about half the curvature times this product, so a small product means a small error
/// whether the estimate lies between the points or just outside them.
const MAX_REACH: f64 = 6.0;

/// Curvature assumed when an estimate lies outside its two measurements (VMAF per CRF step
/// squared). Scores fall faster as the CRF rises, so outside the points the straight line
/// promises more than the encoder gives; the estimate is moved toward quality by that much.
const CURVATURE: f64 = 0.03;

enum Step {
    Try(u8),
    Done(Option<Crossing>),
}

fn next_step(tried: &BTreeMap<u8, f64>, min: u8, max: u8, target: f64) -> Step {
    // The highest passing CRF, and the lowest failing one above it (a failing score below a
    // passing one is noise).
    let pass = tried.iter().rev().find(|(_, v)| **v >= target).map(|(c, v)| (*c, *v));
    let fail = tried.iter().find(|(c, v)| **v < target && pass.is_none_or(|p| **c > p.0)).map(|(c, v)| (*c, *v));
    match (pass, fail) {
        (Some((p, vp)), Some((f, vf))) => {
            let (lo, hi) = (p as f64, f as f64);
            let x = lo + (vp - target) / (vp - vf) * (hi - lo);
            if f - p <= 1 || (x - lo) * (hi - x) <= MAX_REACH {
                return Step::Done(Some(Crossing { crf: quarter(x).clamp(lo, hi - 0.25), from: (p, f) }));
            }
            Step::Try((x.round() as u8).clamp(p + 1, f - 1))
        }
        (Some((p, _)), None) if p >= max => Step::Done(Some(Crossing { crf: p as f64, from: (p, p) })),
        (Some((p, vp)), None) => {
            let Some((&b, &vb)) = tried.range(..p).next_back() else {
                return Step::Try((usual_crossing(p as f64, vp, target).round() as u8).clamp(p + 1, max));
            };
            let slope = ((vb - vp) / (p - b) as f64).clamp(0.1, 2.0);
            let x = p as f64 + (vp - target) / slope;
            let reach = (x - p as f64) * (x - b as f64);
            if reach <= MAX_REACH && x <= max as f64 {
                let safe = x - CURVATURE * reach / (2.0 * slope);
                return Step::Done(Some(Crossing { crf: quarter(safe).max(p as f64), from: (b, p) }));
            }
            Step::Try((x.round() as u8).clamp(p + 1, max))
        }
        (None, Some((f, _))) if f <= min => Step::Done(None),
        (None, Some((f, vf))) => {
            let Some((&b, &vb)) = tried.range(f + 1..).next() else {
                return Step::Try((usual_crossing(f as f64, vf, target).round().max(0.0) as u8).clamp(min, f - 1));
            };
            let slope = ((vf - vb) / (b - f) as f64).clamp(0.1, 2.0);
            let x = f as f64 - (target - vf) / slope;
            let reach = (f as f64 - x) * (b as f64 - x);
            let safe = quarter(x - CURVATURE * reach / (2.0 * slope));
            if reach <= MAX_REACH && safe >= min as f64 {
                return Step::Done(Some(Crossing { crf: safe, from: (f, b) }));
            }
            Step::Try((x.round().max(0.0) as u8).clamp(min, f - 1))
        }
        (None, None) => unreachable!("called after a trial"),
    }
}

/// Mean VMAF by the CRF it was measured at.
pub type Scores = Vec<(u8, f64)>;

/// Finds the quarter-step CRF in `min..=max` where the score crosses `target`, assuming the
/// score falls as CRF rises, with as few evaluations as it can: each new CRF is where a line
/// through the two nearest measurements crosses the target, and the search stops as soon as
/// that crossing is close enough to its measurements to be trusted ([`MAX_REACH`]), instead
/// of narrowing down to two neighbouring CRFs as [`find_crf`] does. Usually 2-3 evaluations.
/// Returns the crossing (None if even `min` fails) and every (crf, score) evaluated.
pub fn find_crossing(
    known: &[(u8, f64)],
    start: u8,
    min: u8,
    max: u8,
    target: f64,
    mut eval: impl FnMut(u8) -> Result<f64>,
) -> Result<(Option<Crossing>, Scores)> {
    if min > max {
        bail!("--min-crf {min} is above --max-crf {max}");
    }
    let mut tried: BTreeMap<u8, f64> = known.iter().filter(|(c, _)| (min..=max).contains(c)).copied().collect();
    let start = start.clamp(min, max);
    // Scores known beforehand that have the target between them say more than a guess. Known
    // scores on one side only may be far off: the guess goes first while it can still tell
    // something they don't.
    let pass = tried.iter().rev().find(|(_, v)| **v >= target).map(|(c, _)| *c);
    let fail = tried.iter().find(|(c, v)| **v < target && pass.is_none_or(|p| **c > p)).map(|(c, _)| *c);
    let guess_first = match (pass, fail) {
        (Some(_), Some(_)) => false,
        (Some(p), None) => start > p,
        (None, Some(f)) => start < f,
        (None, None) => true,
    };
    let mut next = if guess_first { Step::Try(start) } else { next_step(&tried, min, max, target) };
    loop {
        let found = match next {
            Step::Try(c) if !tried.contains_key(&c) => {
                tried.insert(c, eval(c)?);
                next = next_step(&tried, min, max, target);
                continue;
            }
            // Scores that don't fall with the CRF could send the search back to a CRF it
            // has measured; settle for the highest one that passed.
            Step::Try(_) => tried.iter().rev().find(|(_, v)| **v >= target).map(|(c, _)| Crossing { crf: *c as f64, from: (*c, *c) }),
            Step::Done(found) => found,
        };
        return Ok((found, tried.into_iter().collect()));
    }
}

/// Runs [`find_crossing`] with real sample encodes. Returns the outcome and all trials.
pub fn search(lab: &mut Lab, enc: &EncodeSettings, p: &SearchParams) -> Result<(Outcome, Vec<Trial>)> {
    let tools = lab.tools;
    // A second search of the same video (see `SearchParams::worst_sample`) does not list
    // the first one's trials again.
    let show = |t: &Trial, known: bool, shown: &mut std::collections::HashSet<u32>| {
        if !shown.insert(quarters(t.crf)) {
            return;
        }
        eprintln!(
            "  crf {:2}: VMAF {:.2} (worst sample {:.2}), size ~{:.0}% of source{}",
            t.crf, t.mean_vmaf, t.min_vmaf, t.size_ratio * 100.0, if known { " (measured earlier)" } else { "" }
        );
        tools.emit(crate::events::Event::Trial { crf: t.crf, mean_vmaf: t.mean_vmaf, min_vmaf: t.min_vmaf, size_ratio: t.size_ratio });
    };
    // A trial that misses the target while already too large settles the matter, unless a
    // higher CRF is known to pass: then its miss was noise.
    let too_large = |t: &Trial, all: &BTreeMap<u8, Trial>| {
        p.score(t) < p.target_vmaf
            && p.max_size_ratio.is_some_and(|max| t.size_ratio > max)
            && !all.values().any(|u| u.crf > t.crf && p.score(u) >= p.target_vmaf)
    };
    // Trials from earlier runs count as if they had just been measured.
    let mut trials: BTreeMap<u8, Trial> = BTreeMap::new();
    for t in lab.cache.whole(enc) {
        let crf = t.crf as u8;
        if (p.min_crf..=p.max_crf).contains(&crf) {
            show(&t, true, &mut lab.shown);
            trials.insert(crf, t);
        }
    }
    let known: Vec<(u8, f64)> = trials.iter().map(|(c, t)| (*c, p.score(t))).collect();
    let found = match trials.values().find(|t| too_large(t, &trials)) {
        Some(_) => Err(TooLarge.into()),
        None => find_crossing(&known, p.start_crf, p.min_crf, p.max_crf, p.target_vmaf, |crf| {
            let (t, known) = lab.trial(enc, crf as f64)?;
            show(&t, known, &mut lab.shown);
            trials.insert(crf, t);
            if too_large(&t, &trials) {
                return Err(TooLarge.into());
            }
            Ok(p.score(&t))
        }),
    };
    let outcome = match found {
        Ok((Some(c), _)) => Outcome::Found(expected(&trials[&c.from.0], &trials[&c.from.1], c.crf)),
        Ok((None, _)) => Outcome::OutOfReach,
        // The trial that ended the search is the lowest failing one so far or below it.
        Err(e) if e.is::<TooLarge>() => match trials.values().filter(|t| p.score(t) < p.target_vmaf).min_by(|a, b| a.crf.total_cmp(&b.crf)) {
            Some(t) => Outcome::TooLarge(*t),
            None => return Err(e),
        },
        Err(e) => return Err(e),
    };
    Ok((outcome, trials.into_values().collect()))
}

/// What an encode at `crf` should give, read off the line through two measured trials:
/// scores change linearly with the CRF over a few steps, sizes by a constant factor per step.
fn expected(a: &Trial, b: &Trial, crf: f64) -> Trial {
    if a.crf == b.crf || crf == a.crf {
        return Trial { crf, ..*a };
    }
    let w = (crf - a.crf) / (b.crf - a.crf);
    let mix = |x: f64, y: f64| x + (y - x) * w;
    let size_ratio = if a.size_ratio > 0.0 && b.size_ratio > 0.0 { mix(a.size_ratio.ln(), b.size_ratio.ln()).exp() } else { mix(a.size_ratio, b.size_ratio) };
    Trial { crf, mean_vmaf: mix(a.mean_vmaf, b.mean_vmaf), min_vmaf: mix(a.min_vmaf, b.min_vmaf), size_ratio }
}

/// Encodes the samples at `crf` and scores them against their references.
///
/// The samples are encoded end to end as one clip and scored in one run: the encoder and
/// libvmaf start once instead of once per sample (a tenth to a sixth of a trial's time), and
/// no sample begins on a keyframe it would not get in the real encode. Checked frame for
/// frame against the same stretches of a whole encode (four clips, two CRFs each): the mean
/// over the samples came out 0.03 VMAF higher on average and at most 0.16 off, where samples
/// encoded one by one came out 0.04 lower and at most 0.13 off.
#[allow(clippy::too_many_arguments)]
pub fn trial(
    tools: &Tools,
    info: &MediaInfo,
    samples: &[Sample],
    tmp: &Path,
    enc: &EncodeSettings,
    crf: f64,
    threads: usize,
    source_bps: f64,
) -> Result<Trial> {
    if samples.is_empty() {
        bail!("no samples");
    }
    let joined = if samples.len() > 1 { trial_joined(tools, info, samples, tmp, enc, crf, threads)? } else { None };
    let (scores, bytes) = match joined {
        Some(r) => r,
        None => trial_each(tools, info, samples, tmp, enc, crf, threads)?,
    };
    let secs: f64 = samples.iter().map(|s| s.secs).sum();
    let mean = scores.iter().sum::<f64>() / scores.len() as f64;
    let min = scores.iter().copied().fold(f64::INFINITY, f64::min);
    let ratio = if source_bps > 0.0 { bytes as f64 / secs / source_bps } else { 1.0 };
    Ok(Trial { crf, mean_vmaf: mean, min_vmaf: min, size_ratio: ratio })
}

/// Each sample encoded and scored by itself: the score of each, and the bytes of all.
fn trial_each(tools: &Tools, info: &MediaInfo, samples: &[Sample], tmp: &Path, enc: &EncodeSettings, crf: f64, threads: usize) -> Result<(Vec<f64>, u64)> {
    let (mut scores, mut bytes) = (Vec::new(), 0u64);
    for (i, s) in samples.iter().enumerate() {
        let out = tmp.join(format!("sample{i}.crf{crf}.mkv"));
        let mut args: Vec<String> = ["-y", "-v", "error", "-i", &path_arg(&s.reference)].map(String::from).to_vec();
        args.extend(enc.video_args(crf));
        args.push(path_arg(&out));
        tools.run(&args)?;
        scores.push(tools.vmaf(&out, &s.reference, vmaf_model(info), reference_pix_fmt(info), threads, 0, VMAF_STRIDE)?);
        bytes += std::fs::metadata(&out)?.len();
        std::fs::remove_file(&out)?;
    }
    Ok((scores, bytes))
}

/// A line of a concat demuxer list.
fn concat_line(path: &Path) -> String {
    format!("file '{}'\n", path.to_string_lossy().replace('\'', "'\\''"))
}

/// The samples encoded end to end as one clip: the score of each, and the bytes of all.
/// None when the frames of the encode do not line up with the samples (never seen; the
/// caller then measures each sample by itself).
fn trial_joined(tools: &Tools, info: &MediaInfo, samples: &[Sample], tmp: &Path, enc: &EncodeSettings, crf: f64, threads: usize) -> Result<Option<(Vec<f64>, u64)>> {
    // Absolute: a relative name in a concat list counts from the list's folder.
    let list = tmp.join("samples.txt");
    // A list is read line by line: a folder with a line break in its name cannot be listed.
    if samples.iter().any(|s| s.reference.to_string_lossy().contains(['\n', '\r'])) {
        return Ok(None);
    }
    let lines = samples.iter().map(|s| Ok(concat_line(&std::path::absolute(&s.reference)?))).collect::<Result<String>>()?;
    std::fs::write(&list, lines)?;
    let out = tmp.join(format!("samples.crf{crf}.mkv"));
    let mut args: Vec<String> = ["-y", "-v", "error", "-f", "concat", "-safe", "0", "-i", &path_arg(&list)].map(String::from).to_vec();
    args.extend(enc.video_args(crf));
    args.push(path_arg(&out));
    tools.run(&args)?;
    let frames = tools.vmaf_frames(&out, &list, tmp, vmaf_model(info), reference_pix_fmt(info), threads, VMAF_STRIDE)?;
    let bytes = std::fs::metadata(&out)?.len();
    std::fs::remove_file(&out)?;
    Ok(split_scores(&frames, &samples.iter().map(|s| s.frames).collect::<Vec<_>>(), VMAF_STRIDE as u64).map(|scores| (scores, bytes)))
}

/// Mean score of each clip from the scored frames (number, score) of clips of `lengths`
/// frames laid end to end. None unless the scored frames reach the end of the last clip and
/// stop there, to within the `stride` between scored frames.
fn split_scores(frames: &[(u64, f64)], lengths: &[u64], stride: u64) -> Option<Vec<f64>> {
    let total: u64 = lengths.iter().sum();
    let last = frames.iter().map(|f| f.0).max()?;
    if last >= total || last + stride < total {
        return None;
    }
    let mut start = 0;
    lengths
        .iter()
        .map(|&n| {
            let own: Vec<f64> = frames.iter().filter(|f| (start..start + n).contains(&f.0)).map(|f| f.1).collect();
            start += n;
            (!own.is_empty()).then(|| own.iter().sum::<f64>() / own.len() as f64)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joined_samples_are_scored_one_by_one() {
        // Three clips of 5, 4 and 6 frames; every third frame is scored.
        let frames: Vec<(u64, f64)> = [(0, 90.0), (3, 92.0), (6, 80.0), (9, 70.0), (12, 72.0)].to_vec();
        assert_eq!(split_scores(&frames, &[5, 4, 6], 3), Some(vec![91.0, 80.0, 71.0]));
        // In any order, as a log may list them.
        let mut mixed = frames.clone();
        mixed.reverse();
        assert_eq!(split_scores(&mixed, &[5, 4, 6], 3), Some(vec![91.0, 80.0, 71.0]));
        // The encode came out shorter or longer than the samples: not to be trusted.
        assert_eq!(split_scores(&frames[..4], &[5, 4, 6], 3), None);
        assert_eq!(split_scores(&frames, &[5, 4, 3], 3), None);
        assert_eq!(split_scores(&frames, &[5, 4, 9], 3), None);
        // A clip too short to hold a scored frame, and no frames at all.
        assert_eq!(split_scores(&[(0, 90.0), (3, 91.0)], &[2, 1, 2], 3), None);
        assert_eq!(split_scores(&[], &[5], 3), None);
        assert_eq!(concat_line(Path::new("/a b/it's.mkv")), "file '/a b/it'\\''s.mkv'\n");
    }

    #[test]
    fn paths_given_to_ffmpeg_cannot_pass_for_a_protocol() {
        let rel = path_arg(Path::new("a:b/x.mkv"));
        assert!(rel.starts_with('/') && rel.ends_with("/a:b/x.mkv"), "{rel}");
        assert_eq!(path_arg(Path::new("/lib/a:b/x.mkv")), "/lib/a:b/x.mkv");
    }

    /// Highest CRF passing by brute force, for checking `find_crf`.
    fn brute(min: u8, max: u8, target: f64, f: impl Fn(u8) -> f64) -> Option<u8> {
        (min..=max).filter(|&c| f(c) >= target).max()
    }

    fn check(min: u8, max: u8, target: f64, f: impl Fn(u8) -> f64 + Copy) -> usize {
        let (got, tried) = find_crf(START_CRF, min, max, target, |c| Ok(f(c))).unwrap();
        assert_eq!(got, brute(min, max, target, f), "target {target}, tried {tried:?}");
        tried.len()
    }

    #[test]
    fn finds_exact_answer_on_linear_curves() {
        for slope in [0.2, 0.36, 0.5, 0.9] {
            for target in [85.0, 90.0, 93.0, 95.0, 97.0] {
                let n = check(20, 55, target, |c| 105.0 - slope * c as f64);
                assert!(n <= 6, "slope {slope} target {target}: {n} trials");
            }
        }
    }

    #[test]
    fn finds_exact_answer_on_curved_scores() {
        // VMAF flattens near 100 and drops faster at high CRF.
        let f = |c: u8| 100.0 - 0.002 * (c as f64).powi(2) - 0.05 * c as f64;
        for target in [88.0, 91.0, 93.0, 95.0] {
            check(18, 60, target, f);
        }
    }

    #[test]
    fn handles_unreachable_and_always_passing_targets() {
        let (got, _) = find_crf(START_CRF, 20, 55, 99.0, |c| Ok(90.0 - c as f64 * 0.1)).unwrap();
        assert_eq!(got, None);
        let (got, _) = find_crf(START_CRF, 20, 55, 50.0, |_| Ok(99.0)).unwrap();
        assert_eq!(got, Some(55));
    }

    #[test]
    fn needs_fewer_trials_than_binary_search() {
        // Realistic curve from the pilot: ~0.36 VMAF per CRF, plus a little curvature.
        let f = |c: u8| 103.5 - 0.30 * c as f64 - 0.0015 * (c as f64).powi(2);
        let targets: Vec<f64> = (0..=40).map(|i| 86.0 + i as f64 * 0.25).collect();
        let total: usize = targets.iter().map(|&t| check(20, 55, t, f)).sum();
        let avg = total as f64 / targets.len() as f64;
        eprintln!("average trials: {avg:.2}");
        // Binary search over 20..=55 needs ~5.2 on average.
        assert!(avg <= 4.0, "average {avg:.2} trials");
    }

    /// Exact crossing of a curve given at integer CRFs: between the highest passing CRF and
    /// the next one.
    fn crossing(min: u8, max: u8, target: f64, f: impl Fn(u8) -> f64) -> Option<f64> {
        let p = brute(min, max, target, &f)?;
        if p == max {
            return Some(max as f64);
        }
        Some(p as f64 + (f(p) - target) / (f(p) - f(p + 1)))
    }

    /// Runs `find_crossing` on a curve; returns (trials, chosen - exact crossing, score at the
    /// chosen CRF - target).
    fn cross(start: u8, min: u8, max: u8, target: f64, f: impl Fn(u8) -> f64 + Copy) -> (usize, f64, f64) {
        let (got, tried) = find_crossing(&[], start, min, max, target, |c| Ok(f(c))).unwrap();
        let (got, want) = (got.expect("a crossing"), crossing(min, max, target, f).expect("reachable"));
        let lo = got.crf.floor() as u8;
        let score = if lo >= max { f(max) } else { f(lo) + (f(lo + 1) - f(lo)) * (got.crf - lo as f64) };
        assert!(got.crf >= min as f64 && got.crf <= max as f64, "{got:?}");
        assert!(tried.iter().any(|t| t.0 == got.from.0) && tried.iter().any(|t| t.0 == got.from.1), "{got:?} {tried:?}");
        (tried.len(), got.crf - want, score - target)
    }

    #[test]
    fn crossing_is_found_in_few_trials_on_smooth_curves() {
        // Shapes like measured ones: flat near the top, falling faster as the CRF rises.
        let curves: [&dyn Fn(u8) -> f64; 4] = [
            &|c| (100.4 - 0.10 * c as f64 - 0.0052 * (c as f64).powi(2)).min(99.5),
            &|c| (101.0 + 0.25 * c as f64 - 0.0142 * (c as f64).powi(2)).min(99.5),
            &|c| (103.5 - 0.02 * c as f64 - 0.0065 * (c as f64).powi(2)).min(99.5),
            &|c| 105.0 - 0.36 * c as f64,
        ];
        let (mut trials, mut cases) = (0, 0);
        for f in curves {
            for target in (0..=16).map(|i| 88.0 + i as f64 * 0.5) {
                let Some(want) = crossing(20, 55, target, f) else { continue };
                if want >= 55.0 {
                    continue;
                }
                let (n, off, miss) = cross(START_CRF, 20, 55, target, f);
                assert!(n <= 4, "target {target}: {n} trials");
                // Never more than a hair below the target, never far above it.
                assert!(miss > -0.05, "target {target}: {miss:+.3} VMAF");
                assert!(off > -0.75 && off < 0.15, "target {target}: {off:+.2} CRF from the crossing");
                trials += n;
                cases += 1;
            }
        }
        let avg = trials as f64 / cases as f64;
        eprintln!("average trials: {avg:.2} over {cases} cases");
        assert!(avg <= 3.0, "average {avg:.2} trials");
    }

    #[test]
    fn a_good_first_guess_needs_two_trials() {
        let f = |c: u8| 100.4 - 0.10 * c as f64 - 0.0052 * (c as f64).powi(2);
        for target in [90.0, 92.0, 93.0, 94.5, 96.0] {
            let want = crossing(20, 55, target, f).unwrap();
            for d in [-2i32, -1, 0, 1, 2] {
                let start = (want.round() as i32 + d) as u8;
                let (n, off, miss) = cross(start, 20, 55, target, f);
                assert_eq!(n, 2, "target {target}, start {start}");
                assert!(miss > -0.05 && off > -0.75, "target {target}, start {start}: {off:+.2} CRF, {miss:+.3} VMAF");
            }
        }
    }

    #[test]
    fn crossing_handles_the_ends_of_the_range() {
        // Out of reach: the lowest CRF is tried before giving up.
        let (got, tried) = find_crossing(&[], START_CRF, 20, 55, 99.0, |c| Ok(90.0 - c as f64 * 0.1)).unwrap();
        assert_eq!(got, None);
        assert!(tried.iter().any(|t| t.0 == 20), "{tried:?}");
        // Always passing: the highest CRF, measured.
        let (got, tried) = find_crossing(&[], START_CRF, 20, 55, 50.0, |_| Ok(99.0)).unwrap();
        assert_eq!(got, Some(Crossing { crf: 55.0, from: (55, 55) }));
        assert!(tried.len() <= 3, "{tried:?}");
        // One CRF allowed.
        let (got, tried) = find_crossing(&[], START_CRF, 30, 30, 93.0, |_| Ok(94.0)).unwrap();
        assert_eq!((got.map(|c| c.crf), tried.len()), (Some(30.0), 1));
        let (got, _) = find_crossing(&[], START_CRF, 30, 30, 93.0, |_| Ok(92.0)).unwrap();
        assert_eq!(got, None);
        assert!(find_crossing(&[], START_CRF, 40, 30, 93.0, |_| Ok(95.0)).is_err());
        // The crossing sits just above the lowest CRF: nothing below `min` is ever chosen.
        let (got, _) = find_crossing(&[], START_CRF, 28, 55, 93.0, |c| Ok(93.1 - (c as f64 - 28.0) * 0.4)).unwrap();
        let got = got.unwrap();
        assert!(got.crf >= 28.0 && got.crf <= 28.25, "{got:?}");
    }

    #[test]
    fn crossing_survives_scores_that_do_not_fall_steadily() {
        // Bumps of up to 0.3 VMAF on a 0.36/CRF slope, so neighbouring CRFs can swap order.
        for seed in 0..40u32 {
            let f = move |c: u8| 105.0 - 0.36 * c as f64 + 0.3 * (((c as u32 * 7919 + seed * 104729) % 1000) as f64 / 500.0 - 1.0);
            for target in [90.0, 93.0, 95.0] {
                let (got, tried) = find_crossing(&[], START_CRF, 20, 55, target, |c| Ok(f(c))).unwrap();
                let got = got.expect("reachable");
                assert!(tried.len() <= 6, "seed {seed}: {} trials", tried.len());
                // Within the bumps of where the smooth curve crosses.
                let smooth = (105.0 - target) / 0.36;
                assert!((got.crf - smooth).abs() < 2.5, "seed {seed} target {target}: {} vs {smooth:.2}", got.crf);
            }
        }
        // Flat, then a cliff: still ends.
        let (got, tried) = find_crossing(&[], START_CRF, 20, 55, 93.0, |c| Ok(if c < 41 { 96.0 } else { 80.0 })).unwrap();
        assert!(tried.len() <= 7, "{tried:?}");
        assert!(got.unwrap().crf < 41.0);
    }

    /// Mean VMAF at CRF 22..=46 of real clips of different kinds (SVT-AV1 preset 6, tune 0,
    /// four 8-second samples), measured with `av1opt curve`. The searches are held to these as
    /// well as to made-up shapes.
    const MEASURED: [[f64; 25]; 8] = [
        // 1080x1920 archive_W_h264_highbpp.mp4
        [97.53, 97.45, 97.34, 97.16, 96.97, 96.78, 96.59, 96.36, 96.12, 95.79, 95.46, 95.09, 94.72,
         94.27, 93.79, 93.31, 92.79, 92.23, 91.74, 91.12, 90.49, 89.81, 89.15, 88.44, 87.77],
        // 576x1024 archive_feedvault_h264.mp4
        [97.93, 97.79, 97.60, 97.31, 97.04, 96.78, 96.50, 96.22, 95.91, 95.51, 95.07, 94.65, 94.25,
         93.74, 93.17, 92.66, 92.12, 91.54, 91.01, 90.40, 89.77, 89.09, 88.44, 87.76, 87.13],
        // 1920x1080 h264_1080p_landscape.mp4
        [96.79, 96.68, 96.54, 96.31, 96.11, 95.90, 95.70, 95.50, 95.29, 95.00, 94.71, 94.42, 94.15,
         93.81, 93.45, 93.12, 92.77, 92.38, 92.05, 91.61, 91.21, 90.74, 90.30, 89.84, 89.43],
        // 1284x720 h264_720p.mp4
        [94.95, 94.76, 94.51, 94.11, 93.75, 93.39, 93.07, 92.75, 92.40, 91.93, 91.49, 91.06, 90.63,
         90.10, 89.60, 89.13, 88.63, 88.03, 87.50, 86.87, 86.23, 85.57, 84.91, 84.19, 83.57],
        // 1282x720 h264_high_bitrate.mp4
        [95.06, 94.96, 94.82, 94.59, 94.42, 94.23, 94.03, 93.84, 93.65, 93.39, 93.17, 92.89, 92.65,
         92.35, 92.02, 91.75, 91.41, 91.08, 90.74, 90.35, 89.97, 89.53, 89.13, 88.72, 88.30],
        // 1080x1920 h264_vertical_1920.mp4
        [99.06, 98.97, 98.87, 98.65, 98.45, 98.24, 98.04, 97.79, 97.55, 97.16, 96.70, 96.26, 95.86,
         95.29, 94.74, 94.18, 93.55, 92.87, 92.26, 91.50, 90.75, 89.82, 88.96, 88.04, 87.35],
        // 1920x1080 hevc.mov
        [98.84, 98.71, 98.52, 98.22, 97.92, 97.61, 97.30, 96.97, 96.64, 96.16, 95.68, 95.21, 94.73,
         94.13, 93.56, 92.99, 92.40, 91.71, 91.18, 90.46, 89.83, 89.04, 88.35, 87.59, 86.89],
        // 1280x720 mixed.mp4
        [96.23, 96.08, 95.89, 95.61, 95.30, 95.04, 94.77, 94.49, 94.19, 93.77, 93.42, 93.03, 92.65,
         92.18, 91.71, 91.25, 90.78, 90.23, 89.81, 89.22, 88.66, 87.99, 87.33, 86.75, 86.18],
    ];

    fn measured(curve: &[f64; 25]) -> impl Fn(u8) -> f64 + Copy + '_ {
        move |crf| curve[(crf - 22) as usize]
    }

    /// Targets a curve crosses well inside the measured range, with the exact crossing.
    fn measured_targets(curve: &[f64; 25]) -> Vec<(f64, f64)> {
        (0..=18)
            .map(|i| 88.0 + i as f64 * 0.5)
            .filter_map(|target| Some((target, crossing(22, 46, target, measured(curve))?)))
            .filter(|(_, want)| (23.0..=45.0).contains(want))
            .collect()
    }

    #[test]
    fn measured_curves_take_two_or_three_trials() {
        // From the fixed first guess, then from guesses near the answer, as history gives them.
        let (mut far, mut near) = (Vec::new(), Vec::new());
        for (i, curve) in MEASURED.iter().enumerate() {
            for (target, want) in measured_targets(curve) {
                let run = |start: u8| {
                    let (n, off, miss) = cross(start, 22, 46, target, measured(curve));
                    assert!(n <= if start == START_CRF { 4 } else { 3 }, "clip {i}, target {target}, start {start}: {n} trials");
                    // Never more than a hair below the target, never far above it.
                    assert!(miss > -0.05, "clip {i}, target {target}, start {start}: {miss:+.3} VMAF");
                    assert!(off > -0.8 && off < 0.15, "clip {i}, target {target}, start {start}: {off:+.2} CRF from the crossing");
                    n
                };
                far.push(run(START_CRF));
                near.extend((-3..=3).map(|d| run((want.round() as i32 + d).clamp(22, 46) as u8)));
            }
        }
        let mean = |n: &[usize]| n.iter().sum::<usize>() as f64 / n.len() as f64;
        eprintln!("trials: {:.2} from CRF {START_CRF} ({} cases), {:.2} from near the answer ({} cases)", mean(&far), far.len(), mean(&near), near.len());
        assert!(mean(&far) <= 2.3 && mean(&near) <= 2.05);
    }

    #[test]
    fn measured_curves_take_three_trials_down_to_neighbouring_crfs() {
        let mut counts = Vec::new();
        for curve in &MEASURED {
            for (target, _) in measured_targets(curve) {
                counts.push(check(22, 46, target, measured(curve)));
            }
        }
        let mean = counts.iter().sum::<usize>() as f64 / counts.len() as f64;
        eprintln!("trials: {mean:.2} ({} cases), at most {}", counts.len(), counts.iter().max().unwrap());
        assert!(mean <= 3.2 && counts.iter().all(|n| *n <= 5));
    }

    #[test]
    fn the_worst_sample_can_hold_the_crf_down() {
        // Mean 93 at CRF 40; the worst sample is 7 below the mean there and gains 0.9 a step.
        let trial = |crf: u8| Trial { crf: crf as f64, mean_vmaf: 93.0 + (40.0 - crf as f64) * 0.45, min_vmaf: 86.0 + (40.0 - crf as f64) * 0.9, size_ratio: 0.1 };
        let p = |worst_sample| SearchParams { target_vmaf: 93.0, min_crf: 20, max_crf: 55, start_crf: 41, max_size_ratio: None, worst_sample };
        let find = |p: &SearchParams| find_crossing(&[], p.start_crf, p.min_crf, p.max_crf, p.target_vmaf, |c| Ok(p.score(&trial(c)))).unwrap().0.unwrap().crf;
        assert_eq!(find(&p(None)), 40.0);
        // 89 for the worst sample is reached at CRF 36.67: a quarter step on the safe side.
        assert_eq!(find(&p(Some(89.0))), 36.5);
        // A mark the worst sample already meets changes nothing.
        assert_eq!(find(&p(Some(86.0))), 40.0);
        assert_eq!(p(Some(89.0)).score(&trial(40)), 90.0);
        assert_eq!(p(Some(89.0)).score(&trial(30)), 97.5);
    }

    #[test]
    fn known_scores_spare_trials() {
        let f = |c: u8| 100.0 - 0.012 * (c as f64 - 10.0).powi(2);
        let run = |known: &[(u8, f64)], start: u8, target: f64| {
            let mut asked = Vec::new();
            let (got, _) = find_crossing(known, start, 20, 55, target, |c| {
                asked.push(c);
                Ok(f(c))
            })
            .unwrap();
            (got.map(|c| c.crf), asked)
        };
        let (fresh, asked) = run(&[], 35, 93.0);
        let fresh = fresh.unwrap();
        assert!(asked.len() >= 2, "{asked:?}");
        // The same search again, knowing its own trials, measures nothing.
        let known: Vec<(u8, f64)> = asked.iter().map(|c| (*c, f(*c))).collect();
        let (again, asked_again) = run(&known, 35, 93.0);
        assert_eq!(again, Some(fresh));
        assert!(asked_again.is_empty(), "{asked_again:?}");
        // Another target between known scores starts from them, not from the guess.
        let (got, asked) = run(&[(30, f(30)), (40, f(40))], 22, 92.0);
        assert!(!asked.contains(&22), "{asked:?}");
        assert!(asked.len() <= 1, "{asked:?}");
        let x = got.unwrap();
        assert!(f(x.floor() as u8) >= 92.0 && (30.0..40.0).contains(&x), "{x}");
        // A known pass far below: the guess still goes first, and beyond the pass.
        let (_, asked) = run(&[(20, f(20))], 36, 91.0);
        assert_eq!(asked[0], 36, "{asked:?}");
        // A guess the known scores already rule out is not measured.
        let (_, asked) = run(&[(40, f(40))], 36, 85.0);
        assert!(asked.iter().all(|c| *c > 40), "{asked:?}");
        // Known scores outside the allowed range are left out.
        let (got, asked) = run(&[(10, 99.9), (60, 50.0)], 35, 93.0);
        assert_eq!((got, asked.len()), (Some(fresh), 2));
    }

    #[test]
    fn trials_are_remembered_per_file_clips_and_settings() {
        let dir = std::env::temp_dir().join(format!("av1opt-trials-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("a \"video\".mkv");
        std::fs::write(&video, b"12345").unwrap();
        let store = dir.join("trials-v1.jsonl");
        let clips = [(10.0, 8.0), (50.0, 8.0)];
        let enc = EncodeSettings { preset: 6, pix_fmt_out: "yuv420p10le", params: "tune=0".into() };
        let open = |clips: &[(f64, f64)], encoder: &str, reuse: bool| TrialCache::open_at(store.clone(), &video, 5, clips, encoder, reuse);

        let mut c = open(&clips, "ffmpeg 1", true);
        assert!(c.get(&enc, 35.0).is_none());
        c.put(&enc, Trial { crf: 35.0, mean_vmaf: 94.5, min_vmaf: 92.0, size_ratio: 0.4 });
        c.put(&enc, Trial { crf: 38.0, mean_vmaf: 93.5, min_vmaf: 91.0, size_ratio: 0.3 });
        c.put(&enc, Trial { crf: 38.5, mean_vmaf: 93.2, min_vmaf: 90.5, size_ratio: 0.28 });
        assert_eq!(c.get(&enc, 35.0).unwrap().mean_vmaf, 94.5);

        // The next run knows them; the whole CRFs come back in order.
        let c = open(&clips, "ffmpeg 1", true);
        assert_eq!(c.get(&enc, 38.5).unwrap().size_ratio, 0.28);
        assert_eq!(c.whole(&enc).iter().map(|t| t.crf).collect::<Vec<_>>(), [35.0, 38.0]);
        // Other settings, other clips, another ffmpeg: measured again.
        assert!(c.get(&EncodeSettings { preset: 4, ..enc.clone() }, 35.0).is_none());
        assert!(c.get(&EncodeSettings { params: "tune=0:enable-qm=1".into(), ..enc.clone() }, 35.0).is_none());
        assert!(c.get(&EncodeSettings { pix_fmt_out: "yuv420p", ..enc.clone() }, 35.0).is_none());
        assert!(open(&[(10.0, 8.0)], "ffmpeg 1", true).get(&enc, 35.0).is_none());
        assert!(open(&clips, "ffmpeg 2", true).get(&enc, 35.0).is_none());
        // Asked to start fresh: nothing is known, but what is measured is kept.
        let mut fresh = open(&clips, "ffmpeg 1", false);
        assert!(fresh.get(&enc, 35.0).is_none());
        fresh.put(&enc, Trial { crf: 35.0, mean_vmaf: 94.6, min_vmaf: 92.0, size_ratio: 0.4 });
        assert_eq!(open(&clips, "ffmpeg 1", true).get(&enc, 35.0).unwrap().mean_vmaf, 94.6);
        // A file that changed is another file.
        std::fs::write(&video, b"123456").unwrap();
        assert!(TrialCache::open_at(store.clone(), &video, 6, &clips, "ffmpeg 1", true).get(&enc, 35.0).is_none());
        // A cache that can't be written still works for the run.
        let mut off = TrialCache::off();
        off.put(&enc, Trial { crf: 30.0, mean_vmaf: 96.0, min_vmaf: 95.0, size_ratio: 0.6 });
        assert!(off.get(&enc, 30.0).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expectations_follow_the_two_trials() {
        let a = Trial { crf: 30.0, mean_vmaf: 94.0, min_vmaf: 92.0, size_ratio: 0.40 };
        let b = Trial { crf: 34.0, mean_vmaf: 92.0, min_vmaf: 89.0, size_ratio: 0.25 };
        let mid = expected(&a, &b, 32.0);
        assert!((mid.mean_vmaf - 93.0).abs() < 1e-9 && (mid.min_vmaf - 90.5).abs() < 1e-9);
        // Sizes shrink by a constant factor per step: the geometric mean halfway.
        assert!((mid.size_ratio - (0.40f64 * 0.25).sqrt()).abs() < 1e-9);
        // Outside the two trials the line carries on.
        let out = expected(&a, &b, 29.0);
        assert!((out.mean_vmaf - 94.5).abs() < 1e-9 && out.size_ratio > 0.40);
        // A measured CRF is reported as measured.
        let same = expected(&a, &a, 30.0);
        assert_eq!((same.crf, same.mean_vmaf, same.size_ratio), (30.0, 94.0, 0.40));
    }

    #[test]
    fn params_are_tidied_and_checked() {
        assert_eq!(parse_params(" tune=0 : enable-qm=1:qm-min=0: ").unwrap(), "tune=0:enable-qm=1:qm-min=0");
        assert_eq!(parse_params("").unwrap(), "");
        assert_eq!(parse_params("keyint=10s:ac-bias=1.5").unwrap(), "keyint=10s:ac-bias=1.5");
        for bad in ["tune", "crf=30", "tune=0:tune=1", "a=b c", "x='1'", "=1", "preset=4", "tune=0,enable-qm=1"] {
            assert!(parse_params(bad).is_err(), "{bad}");
        }
        let enc = |params: &str| EncodeSettings { preset: 6, pix_fmt_out: "yuv420p10le", params: params.into() };
        assert_eq!(enc("tune=0").video_args(35.3).last().unwrap(), "tune=0:keyint=10s:enable-qm=1:crf=35.25");
        // What the user's own parameters name is left alone.
        assert_eq!(enc("tune=0:keyint=5s").video_args(35.0).last().unwrap(), "tune=0:keyint=5s:enable-qm=1:crf=35");
        assert_eq!(enc("enable-qm=0:tune=0").video_args(35.0).last().unwrap(), "enable-qm=0:tune=0:keyint=10s:crf=35");
        assert_eq!(enc("").video_args(35.0).last().unwrap(), "keyint=10s:enable-qm=1:crf=35");
    }

    #[test]
    fn rejects_inverted_range() {
        assert!(find_crf(START_CRF, 40, 30, 93.0, |_| Ok(95.0)).is_err());
    }

    #[test]
    fn single_value_range() {
        let (got, tried) = find_crf(START_CRF, 30, 30, 93.0, |_| Ok(94.0)).unwrap();
        assert_eq!((got, tried.len()), (Some(30), 1));
    }

    fn info_with_fps(fps: f64) -> MediaInfo {
        MediaInfo {
            video_index: 0, codec: "h264".into(), width: 1920, height: 1080, fps, duration_s: 60.0,
            video_bitrate: 5e6, pix_fmt: "yuv420p".into(), hdr: false, dolby_vision: false, color_transfer: String::new(), interlaced: false, vfr: false,
            start_time: 0.0, video_start: 0.0, covers: vec![], attachments: 0, tags: Default::default(), video_s: 0.0, nominal_fps: 0.0, time_base: (0, 0),
            audio_streams: 0, subtitle_streams: 0, audio: vec![], subtitle_codecs: vec![],
        }
    }

    #[test]
    fn seeks_stay_clear_of_frame_edges() {
        for fps in [23.976, 25.0, 30.0, 59.94] {
            for first in [0.0, 0.021, 0.5] {
                for i in 0..400 {
                    let t = i as f64 * 0.0137;
                    let s = snap_seek(t, fps, first);
                    assert!(s >= 0.0 && (s - t).abs() <= 0.75 / fps + 1e-9, "{t} -> {s} at {fps}");
                    if s > 0.0 {
                        // A quarter of a frame before a frame: nowhere near a frame or the
                        // middle between two, where a millisecond decides.
                        let phase = ((s - first) * fps).rem_euclid(1.0);
                        assert!((phase - 0.75).abs() < 1e-6, "{t} -> {s} at {fps}: phase {phase}");
                    }
                }
            }
        }
        // The very start stays the very start, and nonsense rates change nothing.
        assert_eq!(snap_seek(0.0, 30.0, 0.0), 0.0);
        assert_eq!(snap_seek(0.004, 30.0, 0.0), 0.0);
        assert_eq!(snap_seek(3.21, 0.0, 0.0), 3.21);
        // 2.2167 s is frame 66.5 at 30 fps: moved to a quarter before frame 67.
        assert!((snap_seek(2.2167 - 2.0, 30.0, 0.0) - 6.75 / 30.0).abs() < 1e-9);
    }

    #[test]
    fn cfr_picks_closest_standard_rate() {
        for (fps, want) in [
            (24.0, "24"), (23.976, "24000/1001"), (23.98, "24000/1001"), (25.0, "25"),
            (29.97, "30000/1001"), (30.0, "30"), (29.999, "30"), (59.94, "60000/1001"),
            (60.0, "60"), (59.997, "60"), (12.0, "12.000"), (120.0, "120.000"),
        ] {
            assert_eq!(cfr(&info_with_fps(fps)), want, "fps {fps}");
        }
    }

    #[test]
    fn cfr_goes_by_the_declared_rate_of_a_steady_stream() {
        let info = |fps, nominal_fps, vfr| MediaInfo { nominal_fps, vfr, ..info_with_fps(fps) };
        // 30 fps declared, 29.96 on average: the average alone is nearest to 30000/1001.
        assert_eq!(cfr(&info_with_fps(29.96)), "30000/1001");
        assert_eq!(cfr(&info(29.96, 30.0, false)), "30");
        assert_eq!(cfr(&info(29.89, 30.0, false)), "30");
        assert_eq!(cfr(&info(23.976, 24000.0 / 1001.0, false)), "24000/1001");
        // A variable-rate stream declares whatever fits all its frame times: not a grid.
        assert_eq!(cfr(&info(27.3, 120.0, true)), "27.300");
        assert_eq!(cfr(&info(29.97, 90000.0, true)), "30000/1001");
        // As a message names them.
        assert_eq!([23.976, 30.0, 27.3, 59.94].map(|f| cfr_label(&info_with_fps(f))), ["23.98", "30", "27.3", "59.94"]);
    }

    #[test]
    fn source_filters_fix_odd_sizes_and_interlacing() {
        let mut i = info_with_fps(25.0);
        assert!(source_filters(&i).is_empty());
        (i.width, i.height, i.interlaced) = (641, 361, true);
        assert_eq!(source_filters(&i), ["bwdif=mode=send_frame", "crop=640:360:0:0"]);
    }

    #[test]
    fn sample_positions_stay_inside_video() {
        for dur in [0.5, 3.0, 10.0, 45.0, 600.0, 7200.0] {
            let pos = sample_positions(dur, 4, 8.0);
            assert!(!pos.is_empty());
            for (start, len) in pos {
                assert!(start >= 0.0 && start + len <= dur + 1e-9, "dur {dur}: {start}+{len}");
            }
        }
        assert!(sample_positions(0.0, 4, 8.0).is_empty());
    }

    #[test]
    fn a_longer_video_gets_more_samples() {
        // Eight up to a quarter of an hour, then one for every two minutes more.
        assert_eq!([10.0, 600.0, 900.0, 1019.0].map(sample_count), [8; 4]);
        assert_eq!([1021.0, 1800.0, 3600.0, 3840.0, 7200.0, 36000.0].map(sample_count), [9, 15, 30, 32, 32, 32]);
        // No more than two minutes and a bit from one sample to the next, up to an hour.
        for dur in [900.0, 1500.0, 2400.0, 3600.0] {
            let pos = sample_positions(dur, sample_count(dur), SAMPLE_SECS);
            assert_eq!(pos.len(), sample_count(dur));
            assert!(pos.windows(2).all(|w| w[1].0 - w[0].0 < 125.0), "{dur}: {pos:?}");
        }
        // A short one gets what fits, with as much between two samples as they are long.
        assert_eq!(sample_positions(20.0, sample_count(20.0), SAMPLE_SECS).len(), 2);
        // For the mean alone: many short clips over the whole length, apart from each other.
        assert_eq!([10.0, 60.0, 240.0, 512.0, 900.0, 7200.0].map(mean_sample_count), [8, 8, 30, 64, 64, 64]);
        for dur in [3.0, 10.0, 61.0, 900.0, 7200.0] {
            let pos = mean_sample_positions(dur, mean_sample_count(dur), MEAN_SAMPLE_SECS);
            assert_eq!(pos.len(), mean_sample_count(dur).min((dur / 2.0) as usize).max(1), "{dur}");
            assert!(pos[0].0 >= 0.0 && pos.last().unwrap().0 + pos.last().unwrap().1 <= dur + 1e-9, "{dur}");
            assert!(pos.windows(2).all(|w| w[0].0 + w[0].1 <= w[1].0), "{dur}: clips overlap");
            // Each clip sits in the middle of its share.
            let share = (dur - pos[0].1) / pos.len() as f64;
            assert!(pos.iter().enumerate().all(|(i, p)| (p.0 - share * (i as f64 + 0.5)).abs() < 1e-9), "{dur}");
        }
    }
}
