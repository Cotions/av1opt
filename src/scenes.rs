//! Scene-based encoding: split the video into chunks at scene changes, choose a CRF per chunk
//! so each one meets the VMAF target, encode the chunks separately and join them.
//!
//! A single CRF for a whole film over-spends bits on easy scenes and under-spends on hard ones.
//! Per-chunk CRFs keep quality even across the film. Chunks are cut frame-exactly into
//! lossless references (constant frame rate), which serve as encoder input and VMAF reference.
//! Finished chunks survive interruption, so a re-run resumes where it stopped.
//!
//! A source whose frames don't keep a constant rate is cut by its own timestamps instead of a
//! grid: each chunk holds its frames once, in order, and the joined video gives every frame
//! the time it had (see [`Times`]).

use crate::events::Event;
use crate::probe::MediaInfo;
use crate::search::{self, EncodeSettings, Trial};
use crate::tools::{FAILED_INSIDE, Tools, hms, path_arg};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// One chunk as timestamps of the constant-rate stream, in units of one frame (the time base of
/// ffmpeg's `fps` filter), or of the source itself where frames keep their own times (see
/// [`Plan::timed`]). `None` means from the very start / to the very end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bounds {
    pub start: Option<i64>,
    pub end: Option<i64>,
}

/// Everything decided before chunk encoding starts, saved so an interrupted run can resume
/// without repeating the search and scene detection.
#[derive(Serialize, Deserialize)]
pub struct Plan {
    pub key: String,
    pub best: Trial,
    pub trials: Vec<Trial>,
    /// Real-preset VMAF minus probe-preset VMAF at the same CRF, measured on the samples.
    pub bias: f64,
    pub first_pts: i64,
    pub total_frames: u64,
    pub chunks: Vec<Bounds>,
    /// Set where the source's frames come at uneven times and each keeps its own: the time
    /// base (numerator, denominator) of the source timestamps that `first_pts` and the chunk
    /// bounds then are. Absent: the constant-rate grid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timed: Option<(i64, i64)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkResult {
    pub index: usize,
    pub start_secs: f64,
    pub frames: u64,
    pub crf: f64,
    /// Probe encodes (crf, bias-corrected VMAF) used to choose `crf`.
    pub probes: Vec<(u8, f64)>,
    /// Measured VMAF of the final encode of this chunk.
    pub vmaf: f64,
    pub bytes: u64,
    /// The first final encode missed the target and the chunk was redone at a lower CRF.
    pub corrected: bool,
    /// The probes said no CRF reaches the target here (see [`Estimate::out_of_reach`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub out_of_reach: bool,
}

/// Chunk length limits in seconds.
pub struct Limits {
    pub min_secs: f64,
    pub max_secs: f64,
}

impl Limits {
    pub fn for_video(info: &MediaInfo) -> Self {
        // Lossless 4K references are big (~100 MB/s); keep them shorter.
        let max_secs = if info.width.max(info.height) >= 3200 { 10.0 } else { 20.0 };
        Limits { min_secs: 4.0, max_secs }
    }
}

/// Splits `total` frames into chunks that start at scene cuts, so each chunk holds one kind of
/// content and gets its own CRF. Scenes shorter than `min` frames are merged into the next one;
/// scenes longer than `max` are split evenly. Returns chunk start frames; the first is 0.
pub fn plan(cuts: &[u64], total: u64, min: u64, max: u64) -> Vec<u64> {
    let max = max.max(min + 1);
    let mut cuts: Vec<u64> = cuts.iter().copied().filter(|&c| c > 0 && c < total).collect();
    cuts.sort_unstable();
    cuts.dedup();
    // (At a frame every few seconds the shortest chunk allowed rounds down to no frames, and
    // a cut that is no further on than the last one would be taken again without end.)
    let min = min.max(1);
    let mut starts = vec![0];
    let mut s = 0;
    loop {
        // First cut that leaves both this chunk and the rest at least `min` long.
        let cut = cuts.iter().copied().find(|&c| c >= s + min && total - c >= min);
        let next = match cut {
            Some(c) if c <= s + max => c,
            _ if total - s > max => {
                let end = cut.unwrap_or(total);
                let parts = (end - s).div_ceil(max);
                s + (end - s) / parts
            }
            _ => break,
        };
        starts.push(next);
        s = next;
    }
    if starts.len() > 1 && total - s < min {
        starts.pop();
    }
    starts
}

/// Picks a chunk CRF from at most `max_probes` probe scores at integer CRFs. Unlike
/// [`search::find_crf`] it doesn't confirm neighbouring CRFs: probes are only predictions, and
/// the final encode of the chunk is measured and corrected anyway. Steps from `start` by the
/// expected VMAF-per-CRF slope, then interpolates to the quarter step nearest the target, so
/// chunks land on the target on average instead of systematically above it.
///
/// Some pictures score below the target however many bits they get (noise the metric reads as
/// damage, mostly). When every probe misses and the gain they show per CRF step would still
/// miss at `floor`, the lowest CRF allowed at all, the pick is the highest CRF probed that
/// scores within [`LEVEL_OFF`] of the best: spending more there buys nothing to see.
pub fn estimate_crf(
    start: u8,
    min: u8,
    max: u8,
    floor: u8,
    target: f64,
    max_probes: usize,
    mut eval: impl FnMut(u8) -> Result<f64>,
) -> Result<Estimate> {
    let mut probes: Vec<(u8, f64)> = Vec::new();
    let mut crf = start.clamp(min, max);
    loop {
        let v = eval(crf)?;
        probes.push((crf, v));
        if (v - target).abs() < 0.25 || probes.len() >= max_probes {
            break;
        }
        // Slope from the two probes nearest the target, else the typical value.
        let mut near = probes.clone();
        near.sort_by(|a, b| (a.1 - target).abs().total_cmp(&(b.1 - target).abs()));
        let slope = match near.as_slice() {
            [(c1, v1), (c2, v2), ..] if c1 != c2 => ((v1 - v2) / (*c2 as f64 - *c1 as f64)).clamp(0.1, 2.0),
            _ => search::VMAF_PER_CRF,
        };
        let (c0, v0) = near[0];
        let next = (c0 as f64 + (v0 - target) / slope).clamp(min as f64, max as f64);
        // Toward quality: floor, not round, so misses tend to land above the target (a whole
        // number that arithmetic left a hair short of itself is not rounded down a full step).
        let next = (next + 1e-6).floor() as u8;
        if probes.iter().any(|p| p.0 == next) {
            break;
        }
        crf = next;
    }
    // Interpolate between the closest passing and failing probes; with only one side known,
    // trust the passing probe or step down from the failing one.
    let pass = probes.iter().filter(|p| p.1 >= target).map(|p| p.0).max();
    let fail = probes.iter().filter(|p| p.1 < target).map(|p| p.0).min();
    let pick = match (pass, fail) {
        (Some(p), Some(f)) if f > p => {
            let (vp, vf) = (score_at(&probes, p), score_at(&probes, f));
            let frac = if vp > vf { (vp - target) / (vp - vf) } else { 0.0 };
            p as f64 + frac * (f - p) as f64
        }
        (Some(p), _) => p as f64,
        (None, Some(f)) => f as f64 - (target - score_at(&probes, f)) / search::VMAF_PER_CRF,
        (None, None) => start as f64,
    };
    let pick = (pick * 4.0).round() / 4.0;
    let crf = pick.clamp(min as f64, max as f64);
    if pass.is_none()
        && let Some(&(low, at_low)) = probes.iter().min_by_key(|p| p.0)
        && let Some(&(high, at_high)) = probes.iter().max_by_key(|p| p.0)
    {
        // What a CRF step was seen to buy; one probe alone shows nothing, so the usual gain.
        let gain = if high > low { ((at_low - at_high) / (high - low) as f64).max(0.0) } else { search::VMAF_PER_CRF };
        if at_low + gain * f64::from(low.saturating_sub(floor)) < target {
            let top = probes.iter().map(|p| p.1).fold(f64::MIN, f64::max);
            let level = probes.iter().filter(|p| p.1 >= top - LEVEL_OFF).map(|p| p.0).max().unwrap_or(low);
            return Ok(Estimate { crf: level as f64, probes, out_of_reach: true });
        }
    }
    Ok(Estimate { crf, probes, out_of_reach: false })
}

/// A probe scoring this close to the best one is taken as no worse: where quality has
/// levelled off below the target, the highest such CRF is the one to encode at.
const LEVEL_OFF: f64 = 0.5;

/// What [`estimate_crf`] chose.
pub struct Estimate {
    pub crf: f64,
    /// Every (crf, score) probed.
    pub probes: Vec<(u8, f64)>,
    /// No CRF down to the floor is expected to reach the target: every probe missed, and
    /// the gain between them is too small to get there.
    pub out_of_reach: bool,
}

fn score_at(probes: &[(u8, f64)], crf: u8) -> f64 {
    probes.iter().find(|p| p.0 == crf).map_or(0.0, |p| p.1)
}

/// What the scene pass found.
pub struct Detected {
    /// Scene-cut timestamps (frame units, absolute).
    pub cuts: Vec<i64>,
    /// Frames of the constant-rate stream.
    pub total: u64,
    /// Frames the source has, and how many of them the constant rate dropped or showed twice.
    pub pace: Option<Pace>,
    /// When each of the source's frames is shown, if its timestamps can be gone by.
    pub times: Option<Times>,
}

/// File in the work folder that the scene pass writes every frame's timestamp to.
const TIMES_FILE: &str = "frames.txt";

/// When each frame of the source is shown, as the decoder hands them over. A video that
/// doesn't keep a constant rate is cut into chunks by these, and its encode is given them
/// back frame for frame, so nothing is dropped, doubled or moved.
#[derive(Debug, Clone, PartialEq)]
pub struct Times {
    /// Timestamp of every frame, in order, in units of `tb`.
    pub pts: Vec<i64>,
    /// Seconds per unit as (numerator, denominator).
    pub tb: (i64, i64),
}

impl Times {
    /// From the lines ffmpeg's `metadata=mode=print` writes for each frame,
    /// `frame:2676 pts:8103450 pts_time:90.038333`. The filter sees the stream's time base,
    /// or half of it behind a deinterlacer: the one that turns the last timestamp into the
    /// seconds printed beside it is taken. `None` when the timestamps can't be gone by: no
    /// frames, two frames at one time or out of order, or neither time base fits.
    pub fn parse(text: &str, stream_tb: (i64, i64)) -> Option<Times> {
        let mut pts = Vec::new();
        let mut last_secs = 0.0;
        for l in text.lines().filter(|l| l.starts_with("frame:")) {
            let field = |name: &str| l.split_whitespace().find_map(|w| w.strip_prefix(name));
            pts.push(field("pts:")?.parse::<i64>().ok()?);
            last_secs = field("pts_time:")?.parse::<f64>().ok()?;
        }
        let last = *pts.last()?;
        if pts.windows(2).any(|w| w[1] <= w[0]) {
            return None;
        }
        // Older ffmpeg prints the seconds with six significant digits.
        let close = |(n, d): (i64, i64)| n > 0 && d > 0 && (last as f64 * n as f64 / d as f64 - last_secs).abs() <= (last_secs.abs() * 1e-5).max(1e-4);
        let (n, d) = stream_tb;
        let tb = [(n, d), (n, d.saturating_mul(2))].into_iter().find(|&tb| close(tb))?;
        Some(Times { pts, tb })
    }

    pub(crate) fn secs(&self, units: i64) -> f64 {
        units as f64 * self.tb.0 as f64 / self.tb.1 as f64
    }

    /// The frames are shown about as long as each other: no more than one step in a hundred
    /// is under half the usual step or over one and a half times it. A chunk's score counts
    /// each of its frames once, which is how the chunk is seen only then. In a screen
    /// recording that holds some frames three times as long as others, the frames that
    /// matter most to the eye would weigh least: such a video keeps its one pass, whose
    /// samples are scored along time.
    pub fn steady(&self) -> bool {
        let mut steps: Vec<i64> = self.pts.windows(2).map(|w| w[1] - w[0]).collect();
        if steps.is_empty() {
            return true;
        }
        steps.sort_unstable();
        let usual = steps[steps.len() / 2];
        let odd = steps.iter().filter(|&&s| s * 2 < usual || s * 2 > usual * 3).count();
        odd * 100 <= steps.len()
    }

    /// Frames per second on average, 0 for a single frame.
    pub fn rate(&self) -> f64 {
        let span = self.secs(self.pts[self.pts.len() - 1] - self.pts[0]);
        if span > 0.0 { (self.pts.len() - 1) as f64 / span } else { 0.0 }
    }
}

/// How the source's frames fit the constant frame rate chunks are cut at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pace {
    pub source_frames: u64,
    pub dropped: u64,
    pub repeated: u64,
}

impl Pace {
    /// The source's frames do not come at the constant rate its header names: a screen
    /// recording, a phone video, a Matroska file whose header says 24 fps over frames that
    /// come at 12. Cutting it into constant-rate chunks would drop frames or double them.
    /// A few are let through: a broadcast with a damaged moment is still constant-rate.
    ///
    /// Such a video keeps its frames' own times in scene mode if they are shown about as
    /// long as each other (see [`Times`]), and is encoded in one pass if not.
    ///
    /// Few means few. A webcam recording at 30 fps whose frames come a quarter of a frame
    /// late now and then (8 of 27,895 frames doubled once on the grid, 75 of 28,008 in
    /// another) was let through at one frame in 200: its chunks each met the target, and
    /// the joined video was then thrown away by a spot check of 87, because a frame doubled
    /// inside the stretch checked sets every frame after it against its neighbour.
    pub fn uneven(&self) -> bool {
        self.dropped + self.repeated > (self.source_frames / 10_000).max(2)
    }

    /// From ffmpeg's log at `-v verbose`, where the `fps` filter sums up what it did:
    /// `[Parsed_fps_0 @ 0x...] 96 frames in, 190 frames out; 0 frames dropped, 94 frames duplicated.`
    /// The last such line counts (a filter graph set up twice logs an empty one first).
    fn from_log(log: &str) -> Option<Pace> {
        log.lines().rev().filter(|l| l.contains("Parsed_fps_")).find_map(|l| {
            let nums: Vec<u64> = l.split("] ").nth(1)?.split(|c: char| !c.is_ascii_digit()).filter(|w| !w.is_empty()).filter_map(|w| w.parse().ok()).collect();
            match nums[..] {
                [source_frames, _out, dropped, repeated] => Some(Pace { source_frames, dropped, repeated }),
                _ => None,
            }
        })
    }
}

/// Finds the scene changes of the constant-rate stream, and counts its frames. On the way
/// there every source frame's timestamp is written down, for videos that turn out not to
/// keep that rate.
pub fn detect(tools: &Tools, input: &Path, info: &MediaInfo, work: &Path) -> Result<Detected> {
    let mut vf = search::source_filters(info);
    vf.extend([
        // `metadata` prints frames that carry the key, so every frame is given one.
        "metadata=mode=add:key=av1opt:value=1".into(),
        format!("metadata=mode=print:key=av1opt:file={TIMES_FILE}"),
        format!("fps={}", search::cfr(info)),
        "scale=-2:360".into(),
        "scdet=threshold=10".into(),
        // Relative file name: ffmpeg runs inside `work`, so no filter-graph escaping is needed.
        "metadata=mode=print:key=lavfi.scd.time:file=scenes.txt".into(),
    ]);
    let input = std::path::absolute(input)?;
    // Verbose, for the fps filter's count of dropped and repeated frames (see `Pace`).
    let args: Vec<String> = [
        "-v", "verbose", "-y", "-copyts", "-i", &path_arg(&input), "-map", &format!("0:{}", info.video_index),
        "-an", "-sn", "-dn", "-vf", &vf.join(","), "-f", "null", "-",
    ]
    .map(String::from)
    .to_vec();
    let (total, log) = tools.run_with_progress_log(Some(work), &args, info.duration_s, "finding scenes")?;
    let text = std::fs::read_to_string(work.join("scenes.txt")).unwrap_or_default();
    let cuts = text
        .lines()
        .filter_map(|l| l.split_whitespace().find_map(|w| w.strip_prefix("pts:"))?.parse().ok())
        .collect();
    let pace = Pace::from_log(&log);
    // Only timestamps that account for every frame the rate filter was given.
    let times = std::fs::read_to_string(work.join(TIMES_FILE))
        .ok()
        .and_then(|t| Times::parse(&t, info.time_base))
        .filter(|t| pace.is_some_and(|p| p.source_frames == t.pts.len() as u64));
    Ok(Detected { cuts, total, pace, times })
}

/// Turns detected cuts into chunk bounds. Returns (first frame timestamp, bounds).
pub fn bounds(info: &MediaInfo, cuts: &[i64], total: u64) -> (i64, Vec<Bounds>) {
    let rate = search::cfr_value(info);
    let lim = Limits::for_video(info);
    // Timestamp of the first frame after the fps filter (only used to place cuts; the first
    // chunk has no start bound, so being a frame off here can't lose frames).
    let first = (info.video_start * rate).round() as i64;
    let rel: Vec<u64> = cuts.iter().filter_map(|&c| u64::try_from(c - first).ok()).collect();
    let starts = plan(&rel, total, (lim.min_secs * rate) as u64, (lim.max_secs * rate) as u64);
    let abs = |i: usize| starts.get(i).map(|&s| s as i64 + first);
    let b = (0..starts.len())
        .map(|i| Bounds { start: if i == 0 { None } else { abs(i) }, end: abs(i + 1) })
        .collect();
    (first, b)
}

/// Chunk bounds for a video whose frames keep their own times: the cuts found on the
/// constant-rate stream, moved to the source frames shown there, and the bounds given as
/// those frames' timestamps. Returns (first frame's timestamp, bounds).
pub fn bounds_timed(info: &MediaInfo, cuts: &[i64], times: &Times) -> (i64, Vec<Bounds>) {
    let grid = search::cfr_value(info);
    let lim = Limits::for_video(info);
    // The fps filter shows a source frame at the grid step nearest its time.
    let step = |pts: i64| (times.secs(pts) * grid).round() as i64;
    let rel: Vec<u64> = cuts.iter().map(|&c| times.pts.partition_point(|&p| step(p) < c) as u64).collect();
    let rate = if times.rate() > 0.0 { times.rate() } else { grid };
    let starts = plan(&rel, times.pts.len() as u64, (lim.min_secs * rate) as u64, (lim.max_secs * rate) as u64);
    let at = |i: usize| starts.get(i).map(|&s| times.pts[s as usize]);
    let b = (0..starts.len())
        .map(|i| Bounds { start: if i == 0 { None } else { at(i) }, end: at(i + 1) })
        .collect();
    (times.pts[0], b)
}

/// One frame of the constant rate `cfr` ("30000/1001", "25", "27.300") in seconds, as a
/// fraction ffmpeg's `settb` takes.
fn frame_time(cfr: &str) -> String {
    match (cfr.split_once('/'), cfr.split_once('.')) {
        (Some((n, d)), _) => format!("{d}/{n}"),
        (None, Some((whole, frac))) => format!("1{}/{whole}{frac}", "0".repeat(frac.len())),
        (None, None) => format!("1/{cfr}"),
    }
}

/// Lossless constant-rate cut of exactly the frames in `b`, deinterlaced/cropped like every
/// other encode of this source. Seeks with pre-roll and keeps original timestamps (-copyts) so
/// the `fps` filter puts every frame on the same grid no matter where decoding started.
///
/// With `timed` (the time base of bounds that are the source's own timestamps, see
/// [`Plan::timed`]) no grid is laid over the source: the frames between the bounds are taken
/// as they are and numbered along the constant rate, so the encoder and VMAF get the same
/// kind of clip either way, holding every source frame once.
pub(crate) fn extract_chunk(tools: &Tools, input: &Path, info: &MediaInfo, b: Bounds, timed: Option<(i64, i64)>, out: &Path) -> Result<()> {
    let mut vf = search::source_filters(info);
    let cfr = search::cfr(info);
    if timed.is_none() {
        vf.push(format!("fps={cfr}"));
    }
    let trim: Vec<String> = [b.start.map(|s| format!("start_pts={s}")), b.end.map(|e| format!("end_pts={e}"))]
        .into_iter()
        .flatten()
        .collect();
    if !trim.is_empty() {
        vf.push(format!("trim={}", trim.join(":")));
    }
    match timed {
        // Already one step apart, the frames pass the fps filter untouched; it is there to
        // give each the length of one step. A frame that stood before a gap keeps its long
        // duration otherwise, and the ffmpeg reading this clip would show it twice.
        Some(_) => vf.extend([format!("settb={}", frame_time(&cfr)), "setpts=N".into(), format!("fps={cfr}")]),
        None => vf.push("setpts=PTS-STARTPTS".into()),
    }
    let rate = search::cfr_value(info);
    let secs = |s: i64| timed.map_or(s as f64 / rate, |(n, d)| s as f64 * n as f64 / d as f64);
    let seek = b.start.map_or(0.0, |s| (secs(s) - info.start_time - search::PREROLL_SECS).max(0.0));
    let args: Vec<String> = [
        "-y", "-v", "error", "-copyts", "-ss", &format!("{seek:.3}"), "-i", &path_arg(input),
        "-map", &format!("0:{}", info.video_index), "-an", "-sn", "-dn", "-vf", &vf.join(","),
        "-c:v", search::REFERENCE_CODEC, "-pix_fmt", search::reference_pix_fmt(info), &path_arg(out),
    ]
    .map(String::from)
    .to_vec();
    tools.run(&args).map(|_| ())
}

fn encode_obu(tools: &Tools, reference: &Path, enc: &EncodeSettings, crf: f64, out: &Path) -> Result<()> {
    let mut args: Vec<String> = ["-y", "-v", "error", "-i", &path_arg(reference)].map(String::from).to_vec();
    args.extend(enc.video_args(crf));
    args.extend(["-f".into(), "obu".into(), path_arg(out)]);
    tools.run(&args).map(|_| ())
}

/// Shared settings for encoding the chunks of one file.
pub struct Ctx<'a> {
    pub tools: &'a Tools,
    pub input: &'a Path,
    pub info: &'a MediaInfo,
    pub work: &'a Path,
    pub enc: &'a EncodeSettings,
    pub probe: &'a EncodeSettings,
    pub target: f64,
    pub min_crf: u8,
    pub max_crf: u8,
    pub vmaf_threads: usize,
    pub workers: usize,
}

/// How far from the whole-file CRF a chunk may go.
const CRF_SPREAD: u8 = 12;
use search::VMAF_STRIDE;
/// A chunk whose final encode misses the target by more than this (either way) is redone once
/// at a corrected CRF: below wastes quality, above wastes bits.
const CORRECT_MARGIN: f64 = 1.0;

fn chunk_path(work: &Path, i: usize, ext: &str) -> PathBuf {
    work.join(format!("chunk{i:05}.{ext}"))
}

fn encode_chunk(c: &Ctx, plan: &Plan, i: usize) -> Result<ChunkResult> {
    let b = plan.chunks[i];
    let from_first = (b.start.unwrap_or(plan.first_pts) - plan.first_pts) as f64;
    let start_secs = plan.timed.map_or(from_first / search::cfr_value(c.info), |(n, d)| from_first * n as f64 / d as f64);
    let reference = chunk_path(c.work, i, "ref.mkv");
    let probe_out = chunk_path(c.work, i, "probe.obu");
    let part = chunk_path(c.work, i, "obu.part");
    extract_chunk(c.tools, c.input, c.info, b, plan.timed, &reference)?;
    let frames = c.tools.count_frames(&reference)?;
    if frames == 0 {
        bail!("cutting the chunk at {} from the source gave no frames; {FAILED_INSIDE}", hms(start_secs));
    }

    let model = search::vmaf_model(c.info);
    let pix = search::reference_pix_fmt(c.info);
    let score = |out: &Path, subsample: u32| c.tools.vmaf(out, &reference, model, pix, c.vmaf_threads, 0, subsample);

    let center = plan.best.crf.round() as u8;
    let lo = center.saturating_sub(CRF_SPREAD).max(c.min_crf);
    let hi = center.saturating_add(CRF_SPREAD).min(c.max_crf);
    let Estimate { mut crf, probes, out_of_reach } = estimate_crf(center, lo, hi, c.min_crf, c.target, 3, |crf| {
        encode_obu(c.tools, &reference, c.probe, crf as f64, &probe_out)?;
        Ok(score(&probe_out, VMAF_STRIDE)? + plan.bias)
    })?;
    let _ = std::fs::remove_file(&probe_out);

    encode_obu(c.tools, &reference, c.enc, crf, &part)?;
    let mut vmaf = score(&part, VMAF_STRIDE)?;
    let mut corrected = false;
    let (lo_f, hi_f) = (c.min_crf as f64, c.max_crf as f64);
    // A chunk the probes put out of reach is not chased down the CRFs: they showed that
    // nothing is gained there.
    if (vmaf < c.target - CORRECT_MARGIN && crf > lo_f && !out_of_reach) || (vmaf > c.target + CORRECT_MARGIN && crf < hi_f) {
        // Below target: round toward quality. Above: undo only part of the surplus, since
        // the probe already predicted the target here and the slope is uncertain.
        let delta = (vmaf - c.target) / search::VMAF_PER_CRF;
        let next = if delta < 0.0 { crf + delta.min(-1.0) } else { crf + (delta * 0.75).max(0.5) };
        let prev = (crf, vmaf);
        let kept = chunk_path(c.work, i, "obu.first");
        std::fs::rename(&part, &kept)?;
        crf = search::quarter(next).clamp(lo_f, hi_f);
        encode_obu(c.tools, &reference, c.enc, crf, &part)?;
        vmaf = score(&part, VMAF_STRIDE)?;
        corrected = true;
        // Raising CRF overshot into a miss: the first encode was the better one.
        if prev.1 >= c.target && vmaf < c.target - CORRECT_MARGIN {
            std::fs::rename(&kept, &part)?;
            (crf, vmaf) = prev;
            corrected = false;
        } else {
            let _ = std::fs::remove_file(&kept);
        }
    }
    let bytes = std::fs::metadata(&part)?.len();
    let r = ChunkResult { index: i, start_secs, frames, crf, probes, vmaf, bytes, corrected, out_of_reach };
    // The JSON is written first and the encode renamed last: a chunk counts as done only when
    // its .obu exists, which can't happen half-written.
    std::fs::write(chunk_path(c.work, i, "json"), serde_json::to_vec(&r)?)?;
    std::fs::rename(&part, chunk_path(c.work, i, "obu"))?;
    let _ = std::fs::remove_file(&reference);
    Ok(r)
}

fn load_done(work: &Path, i: usize) -> Option<ChunkResult> {
    chunk_path(work, i, "obu").exists().then_some(())?;
    serde_json::from_slice(&std::fs::read(chunk_path(work, i, "json")).ok()?).ok()
}

/// Encodes all chunks not already done, `workers` at a time. Returns results in chunk order.
pub fn encode_all(c: &Ctx, plan: &Plan) -> Result<Vec<ChunkResult>> {
    let n = plan.chunks.len();
    let results: Mutex<Vec<Option<ChunkResult>>> = Mutex::new((0..n).map(|i| load_done(c.work, i)).collect());
    let todo: Vec<usize> = results.lock().unwrap().iter().enumerate().filter(|(_, r)| r.is_none()).map(|(i, _)| i).collect();
    if todo.len() < n {
        eprintln!("resuming: {} of {n} chunks already encoded", n - todo.len());
    }
    c.tools.emit(Event::ChunkPlan { chunks: n, done: n - todo.len() });
    c.tools.emit(Event::Phase { phase: "chunks".into(), detail: format!("{} of {n} chunks to encode", todo.len()) });
    eprintln!(
        "encoding {} chunks (whole-file crf {}, probe preset {}, bias {:+.2})",
        todo.len(), plan.best.crf, c.probe.preset, plan.bias
    );
    let todo_frames: u64 = todo.len() as u64 * plan.total_frames / n.max(1) as u64;
    let (next, stop, frames_done) = (AtomicUsize::new(0), AtomicBool::new(false), AtomicU64::new(0));
    let failure: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    let t0 = c.tools.stopwatch();

    std::thread::scope(|s| {
        for _ in 0..c.workers.clamp(1, todo.len().max(1)) {
            s.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    let Some(&i) = todo.get(next.fetch_add(1, Ordering::Relaxed)) else { break };
                    match encode_chunk(c, plan, i) {
                        Ok(r) => {
                            let done = frames_done.fetch_add(r.frames, Ordering::Relaxed) + r.frames;
                            let frac = (done as f64 / todo_frames.max(1) as f64).min(1.0);
                            let el = t0.secs(c.tools);
                            let eta = if frac > 0.0 { el / frac - el } else { 0.0 };
                            c.tools.emit(Event::Chunk {
                                index: i, chunks: n, start_secs: r.start_secs, frames: r.frames, crf: r.crf, vmaf: r.vmaf,
                                bytes: r.bytes, corrected: r.corrected, frac, eta_secs: eta,
                            });
                            let probes: Vec<String> = r.probes.iter().map(|(c, v)| format!("{c}:{v:.1}")).collect();
                            eprintln!(
                                "  chunk {}/{n} at {} ({} frames): crf {}{} [probes {}] VMAF {:.2}, {:.2} MB | {:.0}% ETA {}",
                                i + 1, hms(r.start_secs), r.frames, r.crf, if r.corrected { " (corrected)" } else { "" },
                                probes.join(" "), r.vmaf, r.bytes as f64 / 1e6, frac * 100.0,
                                hms(if frac > 0.0 { el / frac - el } else { 0.0 })
                            );
                            results.lock().unwrap()[i] = Some(r);
                        }
                        Err(e) => {
                            stop.store(true, Ordering::Relaxed);
                            failure.lock().unwrap().get_or_insert(e.context(format!("chunk {}", i + 1)));
                        }
                    }
                }
            });
        }
    });
    if let Some(e) = failure.into_inner().unwrap() {
        return Err(e);
    }
    let results: Vec<ChunkResult> = results.into_inner().unwrap().into_iter().map(|r| r.expect("all chunks done")).collect();
    let frames: u64 = results.iter().map(|r| r.frames).sum();
    if frames != plan.total_frames {
        bail!("the encoded chunks hold {frames} frames but the video has {}; {FAILED_INSIDE}", plan.total_frames);
    }
    Ok(results)
}

/// Joins the chunk encodes. Each chunk is a self-contained AV1 low-overhead bitstream starting
/// with a sequence header and key frame, so plain concatenation is a valid stream.
pub fn concat(work: &Path, n: usize) -> Result<PathBuf> {
    let out = work.join("video.obu");
    let mut f = std::fs::File::create(&out)?;
    for i in 0..n {
        let mut chunk = std::fs::File::open(chunk_path(work, i, "obu")).with_context(|| format!("chunk {} missing", i + 1))?;
        std::io::copy(&mut chunk, &mut f)?;
    }
    Ok(out)
}

/// Gives every frame of the joined stream the time its source frame has. The stream is
/// rewrapped as IVF, a container that is nothing but a size and a timestamp in front of each
/// frame, and those timestamps are then rewritten: the source's own, counted from its first.
/// AV1 leaves the encoder in the order it is shown, one packet per frame, so the n-th packet
/// is the n-th source frame.
pub fn stamp(tools: &Tools, work: &Path, video: &Path, info: &MediaInfo) -> Result<PathBuf> {
    let text = std::fs::read_to_string(work.join(TIMES_FILE)).with_context(|| format!("the source's frame times saved for this per-scene encode are missing; {FAILED_INSIDE}"))?;
    let times = Times::parse(&text, info.time_base).with_context(|| format!("the source's frame times saved for this per-scene encode can't be read; {FAILED_INSIDE}"))?;
    let (plain, out) = (work.join("plain.ivf"), work.join("video.ivf"));
    let args: Vec<String> = ["-y", "-v", "error", "-framerate", &search::cfr(info), "-i", &path_arg(video), "-c", "copy", "-f", "ivf", &path_arg(&plain)]
        .map(String::from)
        .to_vec();
    tools.run(&args)?;
    restamp(&plain, &out, &times)?;
    let _ = std::fs::remove_file(&plain);
    Ok(out)
}

/// Copies an IVF stream, its time base and frame timestamps replaced by `times`.
fn restamp(from: &Path, to: &Path, times: &Times) -> Result<()> {
    use std::io::{BufRead, Read, Write};
    let mut r = std::io::BufReader::new(std::fs::File::open(from)?);
    let mut w = std::io::BufWriter::new(std::fs::File::create(to)?);
    // 32 bytes: "DKIF", version, header length, codec, width, height, time base denominator
    // and numerator, length of the stream, four unused. Little-endian throughout.
    let mut head = [0u8; 32];
    r.read_exact(&mut head).with_context(|| format!("the joined chunks could not be read back; {FAILED_INSIDE}"))?;
    if &head[..4] != b"DKIF" || u16::from_le_bytes([head[6], head[7]]) != 32 {
        bail!("the joined chunks could not be read back; {FAILED_INSIDE}");
    }
    let (num, den) = (u32::try_from(times.tb.0)?, u32::try_from(times.tb.1)?);
    let first = times.pts[0];
    head[16..20].copy_from_slice(&den.to_le_bytes());
    head[20..24].copy_from_slice(&num.to_le_bytes());
    head[24..28].copy_from_slice(&u32::try_from(times.pts[times.pts.len() - 1] - first).unwrap_or(u32::MAX).to_le_bytes());
    w.write_all(&head)?;
    let mut n = 0;
    // Each frame: its size in 4 bytes, its timestamp in 8, the frame.
    while !r.fill_buf()?.is_empty() {
        let mut frame = [0u8; 12];
        r.read_exact(&mut frame)?;
        let size = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]) as u64;
        let Some(&pts) = times.pts.get(n) else { bail!("the joined chunks have more frames than the source's {}; {FAILED_INSIDE}", times.pts.len()) };
        frame[4..].copy_from_slice(&((pts - first) as u64).to_le_bytes());
        w.write_all(&frame)?;
        if std::io::copy(&mut (&mut r).take(size), &mut w)? != size {
            bail!("the joined chunks stop partway through frame {}; {FAILED_INSIDE}", n + 1);
        }
        n += 1;
    }
    if n != times.pts.len() {
        bail!("the joined chunks have {n} frames and the source has {}; {FAILED_INSIDE}", times.pts.len());
    }
    w.flush()?;
    Ok(())
}

/// Frame-weighted mean VMAF over all chunks.
pub fn mean_vmaf(results: &[ChunkResult]) -> f64 {
    let frames: u64 = results.iter().map(|r| r.frames).sum();
    results.iter().map(|r| r.vmaf * r.frames as f64).sum::<f64>() / frames.max(1) as f64
}

/// Stable 64-bit FNV-1a hash, used to name resumable work folders.
pub fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fps_filter_tells_whether_frames_keep_a_constant_rate() {
        let log = "[Parsed_fps_0 @ 0x62ae] 0 frames in, 0 frames out; 0 frames dropped, 0 frames duplicated.\n\
                   [h264 @ 0x1] Reinit context to 320x192, pix_fmt: yuv420p\n\
                   [Parsed_fps_0 @ 0x7a52] 96 frames in, 190 frames out; 0 frames dropped, 94 frames duplicated.\n\
                   [out#0/null @ 0x2] video:82KiB audio:0KiB\n";
        let p = Pace::from_log(log).unwrap();
        assert_eq!(p, Pace { source_frames: 96, dropped: 0, repeated: 94 });
        assert!(p.uneven());
        // A filter chain that starts with something else numbers the fps filter differently.
        let p = Pace::from_log("[Parsed_fps_1 @ 0x5d] 172800 frames in, 172803 frames out; 2 frames dropped, 5 frames duplicated.").unwrap();
        assert_eq!(p, Pace { source_frames: 172800, dropped: 2, repeated: 5 });
        // A handful over two hours is a damaged broadcast, not a variable rate.
        assert!(!p.uneven());
        assert!(!Pace { source_frames: 100, dropped: 1, repeated: 1 }.uneven());
        assert!(Pace { source_frames: 100, dropped: 3, repeated: 0 }.uneven());
        assert!(Pace { source_frames: 172800, dropped: 900, repeated: 0 }.uneven());
        // Webcam recordings whose frames drift off the grid, as seen.
        assert!(Pace { source_frames: 27_895, dropped: 0, repeated: 8 }.uneven());
        assert!(Pace { source_frames: 28_008, dropped: 0, repeated: 75 }.uneven());
        // No such line (another ffmpeg, another wording): nothing is concluded.
        assert_eq!(Pace::from_log("frame=  190 fps=0.0 q=-0.0 Lsize=N/A"), None);
        assert_eq!(Pace::from_log("[Parsed_fps_0 @ 0x1] something else entirely"), None);
    }

    fn lens(starts: &[u64], total: u64) -> Vec<u64> {
        starts.windows(2).map(|w| w[1] - w[0]).chain([total - starts.last().unwrap()]).collect()
    }

    #[test]
    fn splits_long_scenes_evenly() {
        assert_eq!(plan(&[], 1125, 100, 500), [0, 375, 750]);
        assert_eq!(plan(&[], 400, 100, 500), [0]);
        assert_eq!(plan(&[], 0, 100, 500), [0]);
    }

    #[test]
    fn a_shortest_chunk_of_no_frames_still_ends() {
        // What a frame every few seconds makes of the shortest chunk allowed.
        assert_eq!(plan(&[30, 60], 100, 0, 50), [0, 30, 60]);
        assert_eq!(plan(&[1, 2, 3], 4, 0, 0), [0, 1, 2, 3]);
        assert_eq!(plan(&[], 3, 0, 0), [0, 1, 2]);
    }

    #[test]
    fn cuts_at_scene_changes() {
        // Cuts at 300 and 650: both fit the limits, so chunks follow the scenes exactly.
        assert_eq!(plan(&[300, 650], 1000, 100, 500), [0, 300, 650]);
        // Scenes shorter than 100 frames are merged into the next.
        assert_eq!(plan(&[50, 120, 260, 480, 530, 900], 1000, 100, 500), [0, 120, 260, 480, 900]);
    }

    #[test]
    fn short_tail_joins_previous_chunk() {
        assert_eq!(plan(&[450], 480, 100, 500), [0]);
        assert_eq!(plan(&[450], 520, 100, 500), [0, 260]);
        assert_eq!(plan(&[450], 1000, 100, 500), [0, 450, 725]);
        assert_eq!(plan(&[450], 1200, 100, 500), [0, 450, 825]);
    }

    #[test]
    fn estimate_lands_near_answer_in_few_probes() {
        for slope in [0.25, 0.4, 0.7] {
            for offset in [80.0, 95.0, 105.0, 115.0] {
                let f = |c: u8| offset - slope * c as f64;
                let target = 93.0;
                let exact = (offset - target) / slope;
                if !(20.0..=55.0).contains(&exact) {
                    continue;
                }
                let Estimate { crf, probes, out_of_reach } = estimate_crf(35, 20, 55, 20, target, 3, |c| Ok(f(c))).unwrap();
                assert!(probes.len() <= 3);
                assert!(!out_of_reach, "slope {slope} offset {offset}");
                // Never above the true answer (quality first), at most 1 below.
                assert!((crf - exact).abs() <= 0.5,
                    "slope {slope} offset {offset}: crf {crf}, exact {exact:.2}, probes {probes:?}");
            }
        }
    }

    /// What ffmpeg's metadata filter prints for frames at these timestamps.
    fn printed(pts: &[i64], tb: (i64, i64)) -> String {
        pts.iter().enumerate().map(|(i, p)| format!("frame:{i:<4} pts:{p:<7} pts_time:{:.6}\nav1opt=1\n", *p as f64 * tb.0 as f64 / tb.1 as f64)).collect()
    }

    /// A webcam's half minute at 30 fps: a frame a quarter late now and then, one left out.
    fn drifting() -> Vec<i64> {
        let mut pts = vec![9000];
        for i in 1..900 {
            let step = if i % 100 == 0 { 3750 } else if i == 450 { 6000 } else { 3000 };
            pts.push(pts[i - 1] + step);
        }
        pts
    }

    #[test]
    fn frame_times_are_read_with_the_time_base_they_were_written_in() {
        let pts = drifting();
        let t = Times::parse(&printed(&pts, (1, 90000)), (1, 90000)).unwrap();
        assert_eq!((t.pts.len(), t.pts[0], t.tb), (900, 9000, (1, 90000)));
        assert!((t.rate() - 899.0 / ((pts[899] - 9000) as f64 / 90000.0)).abs() < 1e-9 && t.rate() < 30.0);
        assert!(t.steady());
        // Behind a deinterlacer the filter counts in half units.
        let doubled: Vec<i64> = pts.iter().map(|p| p * 2).collect();
        assert_eq!(Times::parse(&printed(&doubled, (1, 180000)), (1, 90000)).unwrap().tb, (1, 180000));
        // Older ffmpeg prints six significant digits: 5400.04 for 5400.0367 s.
        let long = "frame:0    pts:0       pts_time:0\nav1opt=1\nframe:1    pts:486003300 pts_time:5400.04\nav1opt=1\n";
        assert_eq!(Times::parse(long, (1, 90000)).unwrap().pts, [0, 486003300]);
        // Not to be gone by: another time base altogether, two frames at one time, a step
        // back, nothing at all, a line cut short.
        assert_eq!(Times::parse(&printed(&pts, (1, 90000)), (1, 1000)), None);
        assert_eq!(Times::parse(&printed(&pts, (1, 90000)), (0, 0)), None);
        assert_eq!(Times::parse(&printed(&[0, 3000, 3000, 9000], (1, 90000)), (1, 90000)), None);
        assert_eq!(Times::parse(&printed(&[0, 6000, 3000, 9000], (1, 90000)), (1, 90000)), None);
        assert_eq!(Times::parse("", (1, 90000)), None);
        // Two seconds at 24 fps, then every third frame: not frames that count alike. Nor
        // when every sixth is missing; one missing in a hundred is.
        let screen: Vec<i64> = (0..48).map(|i| i * 42).chain((0..48).map(|i| 2000 + i * 125)).collect();
        assert!(!Times { pts: screen, tb: (1, 1000) }.steady());
        let gaps = |every: i64| Times { pts: (0..600).filter(|i| i % every != 0).map(|i| i * 3000).collect(), tb: (1, 90000) };
        assert!(!gaps(6).steady() && gaps(100).steady() && !gaps(99).steady());
        assert!(Times { pts: vec![7], tb: (1, 1000) }.steady());
        assert_eq!(Times::parse("frame:0    pts:0\n", (1, 90000)), None);
    }

    #[test]
    fn chunks_of_an_uneven_video_are_bounded_by_its_own_frame_times() {
        let info = MediaInfo { width: 1920, height: 1080, fps: 30.0, nominal_fps: 30.0, ..Default::default() };
        let times = Times { pts: drifting(), tb: (1, 90000) };
        // Frame 420 is shown 14.13 s in: the first frame at 0.1 s, and by then four frames
        // came a quarter late. That is step 424 of the 30 fps grid, where the cut was found.
        assert_eq!((times.secs(times.pts[420]) * 30.0).round() as i64, 424);
        let (first, b) = bounds_timed(&info, &[424], &times);
        assert_eq!(first, 9000);
        assert_eq!(b, [Bounds { start: None, end: Some(times.pts[420]) }, Bounds { start: Some(times.pts[420]), end: None }]);
        // No cuts: split evenly by frames, under 20 s each at the rate the frames really come.
        let (_, b) = bounds_timed(&info, &[], &times);
        assert_eq!(b, [Bounds { start: None, end: Some(times.pts[450]) }, Bounds { start: Some(times.pts[450]), end: None }]);
        // A cut the grid found before the first frame or after the last one changes nothing.
        assert_eq!(bounds_timed(&info, &[0, 99_999], &times).1, b);
    }

    #[test]
    fn a_frame_of_any_rate_is_a_fraction_of_a_second() {
        assert_eq!(frame_time("30000/1001"), "1001/30000");
        assert_eq!(frame_time("25"), "1/25");
        assert_eq!(frame_time("27.300"), "1000/27300");
        assert_eq!(frame_time("12.000"), "1000/12000");
    }

    #[test]
    fn every_frame_of_the_joined_video_gets_its_source_frames_time() {
        let dir = std::env::temp_dir().join(format!("av1opt-stamptest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (from, to) = (dir.join("plain.ivf"), dir.join("video.ivf"));
        // Three frames of 5, 0 and 300 bytes, as ffmpeg writes them at a constant 30 fps.
        let ivf = |frames: &[(u64, usize)], den: u32, num: u32, length: u32| {
            let mut v = b"DKIF\0\0\x20\0AV01".to_vec();
            v.extend([1920u16.to_le_bytes(), 1080u16.to_le_bytes()].concat());
            v.extend([den.to_le_bytes(), num.to_le_bytes(), length.to_le_bytes(), [0; 4]].concat());
            for (i, &(pts, size)) in frames.iter().enumerate() {
                v.extend((size as u32).to_le_bytes());
                v.extend(pts.to_le_bytes());
                v.extend(std::iter::repeat_n(i as u8 + 1, size));
            }
            v
        };
        let plain = ivf(&[(0, 5), (1, 0), (2, 300)], 30, 1, 3);
        std::fs::write(&from, &plain).unwrap();
        let times = Times { pts: vec![9000, 12750, 15750], tb: (1, 90000) };
        restamp(&from, &to, &times).unwrap();
        assert_eq!(std::fs::read(&to).unwrap(), ivf(&[(0, 5), (3750, 0), (6750, 300)], 90000, 1, 6750));
        // A frame too many or too few, or a stream that stops inside a frame: no video.
        for pts in [vec![0, 3000], vec![0, 3000, 6000, 9000]] {
            let e = restamp(&from, &to, &Times { pts, tb: (1, 90000) }).unwrap_err().to_string();
            assert!(e.contains("no output was written"), "{e}");
        }
        std::fs::write(&from, &plain[..plain.len() - 10]).unwrap();
        assert!(restamp(&from, &to, &times).unwrap_err().to_string().contains("partway through frame 3"));
        std::fs::write(&from, b"not a video at all, though long enough").unwrap();
        assert!(restamp(&from, &to, &times).unwrap_err().to_string().contains("could not be read back"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn estimate_handles_unreachable_targets() {
        // Always above: as high as the range goes.
        let e = estimate_crf(35, 20, 55, 20, 93.0, 3, |_| Ok(99.0)).unwrap();
        assert_eq!((e.crf, e.out_of_reach), (55.0, false));
        // A score that falls as usual but starts too low: the lowest CRF, as close as it gets.
        let e = estimate_crf(35, 20, 55, 20, 93.0, 3, |c| Ok(95.0 - 0.3 * c as f64)).unwrap();
        assert_eq!((e.crf, e.out_of_reach), (20.0, true));
    }

    #[test]
    fn a_score_that_has_levelled_off_below_the_target_is_not_paid_for() {
        // The same score whatever the CRF: no bits spent on going lower.
        let e = estimate_crf(35, 20, 55, 20, 93.0, 3, |_| Ok(88.0)).unwrap();
        assert_eq!((e.crf, e.out_of_reach), (35.0, true));
        assert!(e.probes.len() >= 2, "one probe can't show a level score: {:?}", e.probes);
        // A slight rise toward low CRFs that would still miss at the floor: the highest CRF
        // that scores as well as the best, within half a point.
        let e = estimate_crf(35, 23, 47, 20, 93.0, 3, |c| Ok(91.0 - 0.02 * c as f64)).unwrap();
        assert_eq!((e.crf, e.out_of_reach), (35.0, true));
        // The probes stop at the range's end, but the gain they show reaches the target
        // before the floor: the lowest CRF of the range, and the final encode is corrected
        // from there as before.
        let e = estimate_crf(35, 30, 47, 20, 93.0, 3, |c| Ok(100.0 - 0.3 * c as f64)).unwrap();
        assert_eq!((e.crf, e.out_of_reach), (30.0, false));
        // Just short of the target at the first probe and reachable: nothing changes.
        let e = estimate_crf(35, 20, 55, 20, 93.0, 3, |c| Ok(103.0 - 0.3 * c as f64)).unwrap();
        assert!(!e.out_of_reach && (e.crf - 33.25).abs() <= 0.5, "{} {:?}", e.crf, e.probes);
    }

    #[test]
    fn chunk_lengths_stay_within_limits() {
        let (min, max) = (96, 480);
        for total in [10, 500, 1234, 9999, 172_800] {
            for step in [7, 61, 150, 333, 1000] {
                let cuts: Vec<u64> = (1..).map(|k| k * step + (k * k) % 37).take_while(|&c| c < total).collect();
                let starts = plan(&cuts, total, min, max);
                assert_eq!(starts[0], 0);
                assert!(starts.windows(2).all(|w| w[0] < w[1]));
                let l = lens(&starts, total);
                assert!(l.iter().all(|&x| x <= max + min), "total {total} step {step}: {l:?}");
                if starts.len() > 1 {
                    assert!(l.iter().all(|&x| x >= min / 2), "total {total} step {step}: {l:?}");
                }
            }
        }
    }
}
