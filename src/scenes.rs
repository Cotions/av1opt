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
use std::os::unix::fs::OpenOptionsExt;
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
    /// The source's fingerprint when this was planned: see `source::Recorded`. A touched source
    /// with the same one is still the source of the chunks here.
    #[serde(default)]
    pub source_fingerprint: Option<String>,
    /// When the source was last changed then, in seconds since 1970.
    #[serde(default)]
    pub source_mtime: Option<u64>,
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
        Times::read(text, stream_tb, false)
    }

    /// As [`Times::parse`], but two frames next to each other may share a time. Matroska keeps
    /// milliseconds, so two frames less than one apart in the file they were copied from
    /// land on the same one: 10 pairs in the 53,841 frames of a 15-minute video at 60 frames
    /// a second. Frames set against each other by their number are not troubled by that; a
    /// cut at such a time is (see [`crate::repair::survey`]).
    pub fn parse_with_shared_times(text: &str, stream_tb: (i64, i64)) -> Option<Times> {
        Times::read(text, stream_tb, true)
    }

    fn read(text: &str, stream_tb: (i64, i64), shared: bool) -> Option<Times> {
        let mut pts = Vec::new();
        let mut last_secs = 0.0;
        for l in text.lines().filter(|l| l.starts_with("frame:")) {
            let field = |name: &str| l.split_whitespace().find_map(|w| w.strip_prefix(name));
            pts.push(field("pts:")?.parse::<i64>().ok()?);
            last_secs = field("pts_time:")?.parse::<f64>().ok()?;
        }
        let last = *pts.last()?;
        if pts.windows(2).any(|w| w[1] < w[0] || (w[1] == w[0] && !shared)) {
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
        self.uneven_steps_within(1)
    }

    /// No more than `in_a_hundred` steps in a hundred are under half the usual step or over
    /// one and a half times it.
    pub fn uneven_steps_within(&self, in_a_hundred: usize) -> bool {
        let mut steps: Vec<i64> = self.pts.windows(2).map(|w| w[1] - w[0]).collect();
        if steps.is_empty() {
            return true;
        }
        steps.sort_unstable();
        let usual = steps[steps.len() / 2];
        let odd = steps.iter().filter(|&&s| s * 2 < usual || s * 2 > usual * 3).count();
        odd * 100 <= steps.len() * in_a_hundred
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
pub(crate) fn extract_chunk(tools: &Tools, input: &Path, info: &MediaInfo, b: Bounds, timed: Option<(i64, i64)>, out: &Path) -> Result<u64> {
    let mut vf = search::source_filters(info);
    let cfr = search::cfr(info);
    if timed.is_none() {
        vf.push(format!("fps={cfr}"));
    }
    let before_trim = vf.len();
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
    let cut = |seek: f64| -> Result<u64> {
        let mut filters = vf.clone();
        // A seek can land past the chunk's first frame (MPEG-TS, no index, key frames far
        // apart), and the trim would keep what is left of it. Such a cut keeps nothing instead,
        // and is made again from further back.
        if let (true, Some(start)) = (seek > 0.0, b.start) {
            filters.insert(before_trim, format!("select='if(eq(n,0),st(0,lte(pts,{start})),ld(0))'"));
        }
        let args: Vec<String> = [
            "-y", "-v", "error", "-copyts", "-ss", &format!("{seek:.3}"), "-i", &path_arg(input),
            "-map", &format!("0:{}", info.video_index), "-an", "-sn", "-dn", "-vf", &filters.join(","),
            "-c:v", search::REFERENCE_CODEC, "-pix_fmt", search::reference_pix_fmt(info), &path_arg(out),
        ]
        .map(String::from)
        .to_vec();
        tools.run(&args)?;
        tools.count_frames(out)
    };
    let mut frames = 0;
    for seek in [seek, seek - search::SEEK_BACK_SECS].into_iter().filter(|s| *s > 0.0).chain([0.0]) {
        frames = cut(seek)?;
        if frames > 0 {
            break;
        }
    }
    Ok(frames)
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
    let frames = extract_chunk(c.tools, c.input, c.info, b, plan.timed, &reference)?;
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
    let sha256 = crate::ledger::sha256_of_file(&part).with_context(|| format!("cannot read back the encode of chunk {}", i + 1))?;
    // The JSON is written first and the encode renamed last: a chunk counts as done only when
    // its .obu exists, which can't happen half-written.
    let done = Done { result: r.clone(), sha256: Some(sha256), plan: Some(plan.key.clone()) };
    crate::files::write_new(&chunk_path(c.work, i, "json"), &serde_json::to_vec(&done)?)?;
    std::fs::rename(&part, chunk_path(c.work, i, "obu"))?;
    let _ = std::fs::remove_file(&reference);
    Ok(r)
}

/// What is kept of a finished chunk beside its encode: its result, the SHA-256 of the encode,
/// which settles on resuming that the file there is the one that was measured, and the key of
/// the plan it was encoded for, so that a chunk of another plan (of another video, in a work
/// folder put at this one's name) is not taken for one of this.
#[derive(Serialize, Deserialize)]
struct Done {
    #[serde(flatten)]
    result: ChunkResult,
    /// Absent from the records of versions before hashes were kept, as is `plan`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plan: Option<String>,
}

/// More than the record of any chunk takes: a few numbers and a dozen probes.
const DONE_LIMIT: u64 = 1 << 20;

/// What a work folder holds of chunk `i`, looked at to go on from it.
#[derive(Debug)]
enum Found {
    /// Nothing: it is still to be encoded.
    Nothing,
    /// A finished chunk whose encode is the one its record measured.
    Taken(ChunkResult),
    /// A finished chunk of a version that kept no hash, whose encode has the size recorded.
    TakenBySize(ChunkResult),
    /// Something that cannot be gone on from, and why: it is encoded again.
    Redo(String),
}

/// A chunk found in the work folder is taken over only when its record is a plain file of a
/// sensible size, and its encode a plain file of the size and SHA-256 that record holds. The
/// encode is what goes into the output, and its score into the report: a file put there by
/// anyone but this encode (another valid AV1 stream of other pictures, say) must not pass for
/// it, nor a chunk encoded for another plan than `plan_key`. Records of versions that kept no
/// hash are taken by the size alone: only this user can write in the work folder, which has
/// passed the check for going on from it, and the chunks of a long video may be days of work.
fn load_done(work: &Path, i: usize, plan_key: &str) -> Found {
    let encode = chunk_path(work, i, "obu");
    if std::fs::symlink_metadata(&encode).is_err() {
        return Found::Nothing;
    }
    let done: Done = match crate::files::read_regular(&chunk_path(work, i, "json"), DONE_LIMIT) {
        Err(e) => return Found::Redo(format!("its record cannot be read ({e})")),
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(done) => done,
            Err(_) => return Found::Redo("its record cannot be understood".into()),
        },
    };
    let opened = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC).open(&encode);
    let file = match opened {
        Ok(file) => file,
        Err(e) => return Found::Redo(format!("its encode cannot be opened ({e})")),
    };
    match file.metadata() {
        Ok(m) if !m.is_file() => return Found::Redo(format!("its encode is {}", crate::files::kind_of(&m))),
        Ok(m) if m.len() != done.result.bytes => return Found::Redo(format!("its encode has {} bytes, not the {} recorded", m.len(), done.result.bytes)),
        Ok(_) => {}
        Err(e) => return Found::Redo(format!("its encode cannot be looked at ({e})")),
    }
    if done.result.index != i {
        return Found::Redo("its record is of another chunk".into());
    }
    let Some(recorded) = done.sha256 else { return Found::TakenBySize(done.result) };
    if done.plan.as_deref() != Some(plan_key) {
        return Found::Redo("it was encoded for another plan".into());
    }
    match crate::ledger::sha256_of_open(&file) {
        Ok(hash) if hash == recorded => Found::Taken(done.result),
        Ok(_) => Found::Redo("its encode is not the one recorded".into()),
        Err(e) => Found::Redo(format!("its encode cannot be read ({e})")),
    }
}

/// Encodes all chunks not already done, `workers` at a time. Returns results in chunk order.
pub fn encode_all(c: &Ctx, plan: &Plan) -> Result<Vec<ChunkResult>> {
    let n = plan.chunks.len();
    let (mut redone, mut by_size) = (0, 0);
    let found = (0..n).map(|i| match load_done(c.work, i, &plan.key) {
        Found::Taken(r) => Some(r),
        Found::TakenBySize(r) => {
            by_size += 1;
            Some(r)
        }
        Found::Nothing => None,
        Found::Redo(why) => {
            eprintln!("note: chunk {} is encoded again: {why}", i + 1);
            let _ = std::fs::remove_file(chunk_path(c.work, i, "obu"));
            redone += 1;
            None
        }
    });
    let results: Mutex<Vec<Option<ChunkResult>>> = Mutex::new(found.collect());
    let todo: Vec<usize> = results.lock().unwrap().iter().enumerate().filter(|(_, r)| r.is_none()).map(|(i, _)| i).collect();
    if todo.len() < n || redone > 0 {
        let again = if redone > 0 { format!(", {redone} found there encoded again as they did not match their records") } else { String::new() };
        let unhashed = if by_size > 0 { format!(" ({by_size} made by an earlier version that kept no hash, checked by their size alone)") } else { String::new() };
        eprintln!("resuming: {} of {n} chunks taken over{unhashed}{again}", n - todo.len());
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
    // The memory cap is shared only by encodes that really run at the same time.
    let at_once = c.workers.clamp(1, todo.len().max(1));
    let side_by_side = c.tools.for_chunks_at_once(at_once);
    let chunks = Ctx { tools: side_by_side.as_ref().unwrap_or(c.tools), ..*c };

    std::thread::scope(|s| {
        for _ in 0..at_once {
            s.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    let Some(&i) = todo.get(next.fetch_add(1, Ordering::Relaxed)) else { break };
                    match encode_chunk(&chunks, plan, i) {
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
    let mut f = crate::files::create_new(&out)?;
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
    let mut w = std::io::BufWriter::new(crate::files::create_new(to)?);
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
    fn a_chunk_is_taken_over_only_when_it_is_the_encode_its_record_measured() {
        let work = std::env::temp_dir().join(format!("av1opt-scenes-done-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).unwrap();
        let (encode, record) = (chunk_path(&work, 0, "obu"), chunk_path(&work, 0, "json"));
        let made = vec![7u8; 1000];
        let hash = |bytes: &[u8]| {
            std::fs::write(work.join("hashed"), bytes).unwrap();
            crate::ledger::sha256_of_file(&work.join("hashed")).unwrap()
        };
        let keep = |bytes: &[u8], recorded: &[u8]| {
            std::fs::write(&encode, bytes).unwrap();
            let result = ChunkResult { bytes: recorded.len() as u64, ..chunk(48, 93.0) };
            std::fs::write(&record, serde_json::to_vec(&Done { result, sha256: Some(hash(recorded)), plan: Some("plan".into()) }).unwrap()).unwrap();
        };
        assert!(matches!(load_done(&work, 0, "plan"), Found::Nothing));
        keep(&made, &made);
        assert!(matches!(load_done(&work, 0, "plan"), Found::Taken(r) if r.frames == 48));
        // The same chunk and record in the work folder of another plan (another video's, put at
        // this one's name) are not this plan's.
        assert!(matches!(load_done(&work, 0, "another plan"), Found::Redo(why) if why.contains("another plan")));
        // One byte changed, or another stream of the same size: the record does not vouch for it.
        let mut changed = made.clone();
        changed[500] ^= 1;
        keep(&changed, &made);
        assert!(matches!(load_done(&work, 0, "plan"), Found::Redo(why) if why == "its encode is not the one recorded"));
        keep(&vec![9u8; 1000], &made);
        assert!(matches!(load_done(&work, 0, "plan"), Found::Redo(why) if why == "its encode is not the one recorded"));
        keep(&made[..999], &made);
        assert!(matches!(load_done(&work, 0, "plan"), Found::Redo(why) if why.contains("has 999 bytes, not the 1000 recorded")));
        // A record of a chunk of another place in the video does not count for this one.
        keep(&made, &made);
        assert!(matches!(load_done(&work, 1, "plan"), Found::Nothing));
        std::fs::copy(&encode, chunk_path(&work, 1, "obu")).unwrap();
        std::fs::copy(&record, chunk_path(&work, 1, "json")).unwrap();
        assert!(matches!(load_done(&work, 1, "plan"), Found::Redo(_)));
        // A record from before hashes were kept is taken by its size alone, for any plan (none
        // was kept either); one too large, or a pipe at its name, is not.
        std::fs::write(&record, serde_json::to_vec(&ChunkResult { bytes: 1000, ..chunk(48, 93.0) }).unwrap()).unwrap();
        assert!(matches!(load_done(&work, 0, "plan"), Found::TakenBySize(r) if r.frames == 48));
        std::fs::write(&encode, &made[..999]).unwrap();
        assert!(matches!(load_done(&work, 0, "plan"), Found::Redo(why) if why.contains("has 999 bytes")));
        std::fs::write(&encode, &made).unwrap();
        std::fs::write(&record, b"{\"index\": 0}").unwrap();
        assert!(matches!(load_done(&work, 0, "plan"), Found::Redo(why) if why.contains("cannot be understood")));
        std::fs::write(&record, vec![b' '; DONE_LIMIT as usize + 1]).unwrap();
        assert!(matches!(load_done(&work, 0, "plan"), Found::Redo(why) if why.contains("larger than")));
        std::fs::remove_file(&record).unwrap();
        crate::files::tests::make_pipe(&record);
        let (tell, told) = std::sync::mpsc::channel();
        let looked = work.clone();
        std::thread::spawn(move || drop(tell.send(format!("{:?}", load_done(&looked, 0, "plan")))));
        assert!(told.recv_timeout(std::time::Duration::from_secs(2)).expect("the record was waited on").contains("it is a pipe"));
        // An encode that is a link to a file that would pass is not followed.
        std::fs::remove_file(&record).unwrap();
        keep(&made, &made);
        std::fs::rename(&encode, work.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(work.join("elsewhere"), &encode).unwrap();
        assert!(matches!(load_done(&work, 0, "plan"), Found::Redo(why) if why.contains("cannot be opened")));
        std::fs::remove_dir_all(&work).unwrap();
    }

    #[test]
    fn a_chunk_holds_its_frames_when_the_seek_lands_past_them() {
        let Some(tools) = crate::tools::real_tools("a_chunk_holds_its_frames_when_the_seek_lands_past_them") else { return };
        let ffmpeg = std::env::var("AV1OPT_FFMPEG").unwrap();
        let dir = std::env::temp_dir().join(format!("av1opt-chunk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Key frames 8 s apart in a file without an index: a seek to 3 s lands at 8 s.
        let path = dir.join("far.ts");
        let made = std::process::Command::new(&ffmpeg)
            .args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i", "testsrc2=s=160x120:r=25:d=16", "-c:v", "libx264", "-preset", "ultrafast"])
            .args(["-pix_fmt", "yuv420p", "-g", "200", "-keyint_min", "200", "-sc_threshold", "0", "-f", "mpegts"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
        let info = crate::probe::probe(&tools.ffprobe, &path, std::fs::metadata(&path).unwrap().len()).unwrap();
        let hashes = |file: &Path| -> Vec<String> {
            let out = std::process::Command::new(&ffmpeg).args(["-v", "error", "-nostdin", "-i"]).arg(file).args(["-map", "0:v:0", "-fps_mode", "passthrough", "-pix_fmt", "yuv420p", "-f", "framemd5", "-"]).output().unwrap();
            String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.starts_with('#')).filter_map(|l| Some(l.rsplit(',').next()?.trim().to_string())).collect()
        };
        let all = hashes(&path);
        let first_pts = (info.start_time * 25.0).round() as i64;
        let clip = dir.join("chunk.mkv");
        // The first runs across the key frame at 8 s: its end alone would be found there.
        for (from, to) in [(150usize, 250usize), (125, 150), (60, 100), (210, 240)] {
            let b = Bounds { start: Some(first_pts + from as i64), end: Some(first_pts + to as i64) };
            let frames = extract_chunk(&tools, &path, &info, b, None, &clip).unwrap();
            assert_eq!(frames as usize, to - from, "frames {from} to {to}");
            assert_eq!(hashes(&clip), all[from..to], "frames {from} to {to}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

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
        // Two frames at one time are taken where frames are only counted off, never one before.
        assert_eq!(Times::parse_with_shared_times(&printed(&[0, 3000, 3000, 9000], (1, 90000)), (1, 90000)).unwrap().pts, [0, 3000, 3000, 9000]);
        assert_eq!(Times::parse_with_shared_times(&printed(&[0, 6000, 3000, 9000], (1, 90000)), (1, 90000)), None);
        assert_eq!(Times::parse(&printed(&[0, 6000, 3000, 9000], (1, 90000)), (1, 90000)), None);
        assert_eq!(Times::parse("", (1, 90000)), None);
        // Two seconds at 24 fps, then every third frame: not frames that count alike. Nor
        // when every sixth is missing; one missing in a hundred is.
        let screen: Vec<i64> = (0..48).map(|i| i * 42).chain((0..48).map(|i| 2000 + i * 125)).collect();
        assert!(!Times { pts: screen, tb: (1, 1000) }.steady());
        let gaps = |every: i64| Times { pts: (0..600).filter(|i| i % every != 0).map(|i| i * 3000).collect(), tb: (1, 90000) };
        assert!(!gaps(6).steady() && gaps(100).steady() && !gaps(99).steady());
        assert!(gaps(11).uneven_steps_within(10) && !gaps(10).uneven_steps_within(10) && gaps(99).uneven_steps_within(10));
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

    #[test]
    fn cuts_outside_the_video_or_given_twice_count_for_nothing() {
        assert_eq!(plan(&[650, 300, 300, 0, 1000, 5000], 1000, 100, 500), [0, 300, 650]);
    }

    #[test]
    fn a_cut_may_lie_exactly_at_the_shortest_and_the_longest_distance() {
        assert_eq!(plan(&[100], 1000, 100, 1000), [0, 100]);
        assert_eq!(plan(&[99], 1000, 100, 1000), [0]);
        assert_eq!(plan(&[900], 1000, 100, 1000), [0, 900]);
        assert_eq!(plan(&[901], 1000, 100, 1000), [0]);
        assert_eq!(plan(&[500], 600, 100, 500), [0, 500]);
    }

    #[test]
    fn a_scene_too_long_is_split_evenly_up_to_its_cut() {
        assert_eq!(plan(&[800], 1000, 100, 500), [0, 400, 800]);
        // A cut too near the end does not count, and the rest is split as if it were not there.
        assert_eq!(plan(&[950], 1000, 100, 500), [0, 500]);
    }

    #[test]
    fn a_video_shorter_than_a_chunk_is_one_chunk() {
        assert_eq!(plan(&[], 1, 100, 500), [0]);
        assert_eq!(plan(&[50], 99, 100, 500), [0]);
    }

    #[test]
    fn a_longest_chunk_below_the_shortest_is_raised_above_it() {
        let starts = plan(&[], 1000, 100, 50);
        assert_eq!(starts, (0..10).map(|i| i * 100).collect::<Vec<u64>>());
    }

    #[test]
    fn chunk_starts_rise_and_stay_inside_the_video_for_any_cuts() {
        for total in [1, 2, 99, 100, 101, 777, 5000] {
            for (min, max) in [(0, 0), (1, 1), (10, 30), (100, 500), (500, 100)] {
                for step in [1, 2, 13, 250] {
                    let cuts: Vec<u64> = (0..total + 5).step_by(step).collect();
                    let starts = plan(&cuts, total, min, max);
                    assert_eq!(starts[0], 0, "{total} {min} {max} {step}");
                    assert!(starts.windows(2).all(|w| w[0] < w[1]), "{total} {min} {max} {step}: {starts:?}");
                    assert!(*starts.last().unwrap() < total.max(1), "{total} {min} {max} {step}: {starts:?}");
                }
            }
        }
    }

    fn fps30() -> MediaInfo {
        MediaInfo { width: 1920, height: 1080, fps: 30.0, ..Default::default() }
    }

    #[test]
    fn chunks_join_without_a_gap_and_the_outer_ends_stay_open() {
        // Too long for one chunk at 20 s, no cuts: halves.
        let (first, b) = bounds(&fps30(), &[], 1000);
        assert_eq!(first, 0);
        assert_eq!(b, [Bounds { start: None, end: Some(500) }, Bounds { start: Some(500), end: None }]);
        let (_, one) = bounds(&fps30(), &[], 100);
        assert_eq!(one, [Bounds { start: None, end: None }]);
    }

    #[test]
    fn cuts_are_placed_from_where_the_video_starts() {
        // The picture starts at 2 s, frame 60 of the grid. A cut before it means nothing.
        let info = MediaInfo { video_start: 2.0, ..fps30() };
        let (first, b) = bounds(&info, &[30, 360], 1000);
        assert_eq!(first, 60);
        assert_eq!(b, [Bounds { start: None, end: Some(360) }, Bounds { start: Some(360), end: Some(710) }, Bounds { start: Some(710), end: None }]);
    }

    #[test]
    fn very_large_pictures_get_shorter_chunks_whichever_way_up() {
        let longest = |width, height| Limits::for_video(&MediaInfo { width, height, ..Default::default() }).max_secs;
        assert_eq!(longest(3840, 2160), 10.0);
        assert_eq!(longest(2160, 3840), 10.0);
        assert_eq!(longest(3200, 1800), 10.0);
        assert_eq!(longest(3199, 1800), 20.0);
        assert_eq!(longest(1920, 1080), 20.0);
    }

    /// Probes that are read from a straight line: `at_0 - slope * crf`.
    fn line(at_0: f64, slope: f64) -> impl FnMut(u8) -> Result<f64> {
        move |c| Ok(at_0 - slope * c as f64)
    }

    #[test]
    fn a_probe_close_to_the_target_ends_the_search() {
        let mut asked = Vec::new();
        let e = estimate_crf(35, 20, 55, 20, 93.0, 3, |c| {
            asked.push(c);
            Ok(93.2)
        })
        .unwrap();
        assert_eq!((asked, e.crf, e.out_of_reach), (vec![35], 35.0, false));
    }

    #[test]
    fn at_least_one_probe_is_made_and_it_starts_inside_the_range() {
        assert_eq!(estimate_crf(35, 20, 55, 20, 93.0, 0, |_| Ok(91.0)).unwrap().probes, [(35, 91.0)]);
        assert_eq!(estimate_crf(10, 20, 55, 20, 93.0, 1, |_| Ok(93.0)).unwrap().probes, [(20, 93.0)]);
        assert_eq!(estimate_crf(60, 20, 55, 20, 93.0, 1, |_| Ok(93.0)).unwrap().probes, [(55, 93.0)]);
    }

    #[test]
    fn one_probe_below_the_target_steps_down_by_the_usual_slope_to_a_quarter() {
        let at = |v: f64| estimate_crf(35, 20, 55, 20, 93.0, 1, move |_| Ok(v)).unwrap().crf;
        assert_eq!(at(91.0), 31.0);
        // 0.1 short is 0.2 of a CRF, the nearest quarter step being 34.75.
        assert_eq!(at(92.9), 34.75);
    }

    #[test]
    fn the_answer_lies_between_the_probes_either_side_of_the_target() {
        let e = estimate_crf(35, 20, 55, 20, 93.0, 2, line(126.0, 1.0)).unwrap();
        assert_eq!(e.probes, [(35, 91.0), (31, 95.0)]);
        assert_eq!(e.crf, 33.0);
    }

    #[test]
    fn the_answer_never_leaves_the_range() {
        assert_eq!(estimate_crf(35, 30, 40, 20, 93.0, 3, |_| Ok(99.0)).unwrap().crf, 40.0);
        let e = estimate_crf(35, 30, 40, 0, 93.0, 3, line(120.0, 1.0)).unwrap();
        assert_eq!((e.crf, e.out_of_reach), (30.0, false));
    }

    #[test]
    fn a_target_is_out_of_reach_only_when_the_lowest_crf_would_still_miss_it() {
        // One probe at 35, floor 20: fifteen steps at the usual half point a step are 7.5.
        let e = estimate_crf(35, 20, 55, 20, 93.0, 1, |_| Ok(85.4)).unwrap();
        assert_eq!((e.crf, e.out_of_reach), (35.0, true));
        let e = estimate_crf(35, 20, 55, 20, 93.0, 1, |_| Ok(85.5)).unwrap();
        assert_eq!((e.crf, e.out_of_reach), (20.0, false));
        // A probe that passes settles it.
        assert!(!estimate_crf(35, 20, 55, 20, 93.0, 3, line(128.0, 1.0)).unwrap().out_of_reach);
    }

    #[test]
    fn a_probe_that_fails_ends_the_estimate_with_its_error() {
        let e = estimate_crf(35, 20, 55, 20, 93.0, 3, |_| anyhow::bail!("no such encoder"));
        assert_eq!(e.err().unwrap().to_string(), "no such encoder");
    }

    #[test]
    fn a_crf_never_probed_scores_nothing() {
        assert_eq!(score_at(&[(30, 91.0)], 30), 91.0);
        assert_eq!(score_at(&[(30, 91.0)], 31), 0.0);
    }

    #[test]
    fn frame_lines_may_come_among_others_and_a_single_frame_will_do() {
        let text = "header\nframe:0 pts:0 pts_time:0\nav1opt=1\nnoise pts:7 pts_time:1\nframe:1 pts:3000 pts_time:0.033333\n";
        assert_eq!(Times::parse(text, (1, 90000)).unwrap().pts, [0, 3000]);
        let one = Times::parse("frame:0 pts:0 pts_time:0\n", (1, 90000)).unwrap();
        assert_eq!((one.pts.len(), one.rate()), (1, 0.0));
        assert_eq!(Times::parse("frame:0 pts:N/A pts_time:N/A\n", (1, 90000)), None);
        assert_eq!(Times::parse("frame:0 pts:5 pts_time:soon\n", (1, 90000)), None);
    }

    #[test]
    fn half_and_one_and_a_half_steps_are_still_steady_and_one_in_a_hundred_may_be_odd() {
        let steps = |list: &[(usize, i64)]| {
            let mut pts = vec![0];
            for &(n, step) in list {
                for _ in 0..n {
                    pts.push(pts[pts.len() - 1] + step);
                }
            }
            Times { pts, tb: (1, 1000) }
        };
        assert!(steps(&[(100, 4), (50, 2), (50, 6)]).steady());
        // 1 and 7 are beyond half and one and a half times 4.
        assert!(steps(&[(200, 4), (2, 1)]).steady());
        assert!(!steps(&[(200, 4), (3, 1)]).steady());
        assert!(steps(&[(200, 4), (2, 7)]).steady());
        assert!(!steps(&[(200, 4), (3, 7)]).steady());
    }

    #[test]
    fn the_rate_is_frames_per_second_between_the_first_and_the_last() {
        let t = Times { pts: (5..=35).map(|i| i * 3000).collect(), tb: (1, 90000) };
        assert!((t.rate() - 30.0).abs() < 1e-9, "{}", t.rate());
        assert_eq!(t.secs(90000), 1.0);
    }

    #[test]
    fn a_handful_of_frames_off_the_grid_is_allowed_by_the_length_of_the_video() {
        let uneven = |source_frames, dropped, repeated| Pace { source_frames, dropped, repeated }.uneven();
        assert!(!uneven(0, 1, 1) && uneven(0, 2, 1));
        assert!(!uneven(29_999, 1, 1) && uneven(29_999, 2, 1));
        assert!(!uneven(30_000, 2, 1) && uneven(30_000, 2, 2));
    }

    #[test]
    fn of_the_frame_rate_filters_lines_the_last_readable_one_counts() {
        let log = "[Parsed_fps_0 @ 0x1] 9 frames in, 9 frames out; 1 frames dropped, 2 frames duplicated.\n[Parsed_fps_0 @ 0x2] something else\n";
        assert_eq!(Pace::from_log(log), Some(Pace { source_frames: 9, dropped: 1, repeated: 2 }));
        assert_eq!(Pace::from_log(""), None);
    }

    fn chunk(frames: u64, vmaf: f64) -> ChunkResult {
        ChunkResult { index: 0, start_secs: 0.0, frames, crf: 30.0, probes: Vec::new(), vmaf, bytes: 0, corrected: false, out_of_reach: false }
    }

    #[test]
    fn the_mean_over_chunks_weighs_them_by_their_frames() {
        assert_eq!(mean_vmaf(&[]), 0.0);
        assert_eq!(mean_vmaf(&[chunk(0, 90.0)]), 0.0);
        assert_eq!(mean_vmaf(&[chunk(300, 90.0), chunk(100, 94.0)]), 91.0);
    }

    #[test]
    fn the_name_hash_is_the_standard_one_so_work_folders_survive_an_update() {
        assert_eq!(fnv1a(""), 0xcbf29ce484222325);
        assert_eq!(fnv1a("a"), 0xaf63dc4c8601ec8c);
        assert_eq!(fnv1a("foobar"), 0x85944171f73967e8);
    }
}
