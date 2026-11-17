//! Repair of a one-pass encode: the whole output is scored against its source, and the
//! stretches that fall short are encoded again and put in place of the ones they replace.
//!
//! One CRF gives a video one quantizer, not one quality. Along a real 15-minute video
//! encoded at one CRF the 5-second stretches ran from VMAF 83 to 99.6 around a mean of 95,
//! a tenth of them below 89; two spot checks see ten seconds of that. Lowering the CRF of the
//! whole video until its worst stretch holds spends bits on the nine tenths that needed none.
//! Here the video keeps the CRF its mean asks for, and only the stretches below the floor
//! get a lower one.
//!
//! The encoder starts every group of pictures (GOP) with a key frame, which refers to nothing
//! before it and after which nothing refers to what came before. A run of GOPs can therefore
//! be swapped for another encode of the same frames without touching its neighbours: the
//! stream is rewrapped as IVF (a size and a timestamp in front of each frame), the frames of
//! the weak GOPs are replaced, and every frame keeps the time it had.

use crate::events::Event;
use crate::probe::MediaInfo;
use crate::scenes::{self, Bounds, Times};
use crate::search::{self, EncodeSettings, VMAF_STRIDE};
use crate::tools::{Tools, hms, path_arg};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Length of the stretches that are judged, in seconds: what a spot check looks at.
pub const WINDOW_SECS: f64 = 5.0;
/// How far apart the judged stretches start.
const STEP_SECS: f64 = 1.0;
/// How far above the floor a stretch that is encoded again is aimed: its score moves a
/// little with the frames around it, and one on the line would fail half the time.
const AIM_MARGIN: f64 = 1.0;
/// A new encode counts as holding the floor from this far above it.
const ACCEPT: f64 = 0.25;
/// A new encode this far above its aim is tried once more at a higher CRF: it spent bits
/// the floor did not ask for.
const OVERSHOOT: f64 = 2.0;
/// VMAF gained per CRF step in a weak stretch, to start from: the five stretches of a real
/// video's first minutes gained 0.9 to 1.15 a step, twice what the mean of a video gains
/// (`search::VMAF_PER_CRF`). Later tries go by what the stretch itself showed.
const SLOPE: f64 = 1.0;
/// How far below the video's CRF a stretch may go.
const CRF_SPREAD: f64 = 16.0;
/// A stretch that gains less than this over three or more CRF steps has nothing to gain
/// from more bits (see `scenes::Estimate::out_of_reach`).
const LEVEL_OFF: f64 = 0.5;
/// Encodes per stretch and round.
const TRIES: usize = 3;
/// Times the video is gone over: a stretch encoded again can leave a window that lies
/// across its edge short, which the next round sees.
const ROUNDS: usize = 3;
/// GOPs longer than this are not cut out: their lossless reference would be too large.
const MAX_GOP_SECS: f64 = 30.0;

const TIMES_FILE: &str = "survey-frames.txt";
const LOG_FILE: &str = "survey-vmaf.json";

/// The finished encode measured against its source, frame for frame.
pub struct Survey {
    /// VMAF of every scored frame (each `VMAF_STRIDE`-th): frame number and score, in order.
    pub scores: Vec<(usize, f64)>,
    /// When each frame of the source is shown.
    pub times: Times,
    /// The frame each GOP of the output starts with; the first is 0.
    pub gops: Vec<usize>,
}

/// Scores the whole of `output` against `input`. `None` where that can't be gone by: the
/// two don't hold the same number of frames, the source's timestamps can't be read, or its
/// frames are not shown about as long as each other (a window of frames would then not be a
/// window of time; see [`Times::steady`]).
pub fn survey(tools: &Tools, input: &Path, info: &MediaInfo, output: &Path, threads: usize, dir: &Path) -> Result<Option<Survey>> {
    let keys = tools.key_packets(output)?;
    let gops: Vec<usize> = keys.iter().enumerate().filter(|(_, k)| **k).map(|(i, _)| i).collect();
    if gops.first() != Some(&0) {
        return Ok(None);
    }
    let pix = search::reference_pix_fmt(info);
    let mut vf = search::source_filters(info);
    vf.extend([
        // `metadata` prints frames that carry the key, so every frame is given one.
        "metadata=mode=add:key=av1opt:value=1".into(),
        format!("metadata=mode=print:key=av1opt:file={TIMES_FILE}"),
        format!("format={pix}"),
        "settb=1".into(),
        "setpts=N".into(),
    ]);
    // Frames are paired by their number, as in `Tools::vmaf`. File names are relative:
    // ffmpeg runs inside `dir`, so nothing in the filter graph needs escaping.
    let filter = format!(
        "[0:v:0]format={pix},settb=1,setpts=N[d];[1:{}]{}[r];\
         [d][r]libvmaf=model=version={}:n_threads={threads}:n_subsample={VMAF_STRIDE}:shortest=1:log_fmt=json:log_path={LOG_FILE}",
        info.video_index,
        vf.join(","),
        search::vmaf_model(info)
    );
    let args: Vec<String> = [
        "-y", "-copyts", "-i", &path_arg(output), "-i", &path_arg(&std::path::absolute(input)?),
        "-lavfi", &filter, "-an", "-sn", "-dn", "-f", "null", "-",
    ]
    .map(String::from)
    .to_vec();
    // The scored frames are numbered in seconds (`settb=1`), so the output's clock reads
    // frames where it says seconds.
    tools.run_with_progress_in(Some(dir), &args, keys.len() as f64, "checking")?;
    let text = std::fs::read_to_string(dir.join(TIMES_FILE)).unwrap_or_default();
    let log = std::fs::read(dir.join(LOG_FILE)).context("libvmaf wrote no log")?;
    let _ = std::fs::remove_file(dir.join(TIMES_FILE));
    let _ = std::fs::remove_file(dir.join(LOG_FILE));
    let Some(times) = Times::parse(&text, info.time_base) else { return Ok(None) };
    if times.pts.len() != keys.len() || !times.steady() {
        return Ok(None);
    }
    let scores: Vec<(usize, f64)> = crate::tools::parse_vmaf_frames(&log)?.into_iter().map(|(n, v)| (n as usize, v)).collect();
    if scores.last().is_none_or(|&(n, _)| n >= keys.len() || n + (VMAF_STRIDE as usize) < keys.len()) {
        return Ok(None);
    }
    Ok(Some(Survey { scores, times, gops }))
}

/// Seconds from the first frame at which each frame is shown, then the end of the last one.
fn timeline(times: &Times) -> Vec<f64> {
    let first = times.pts[0];
    let mut secs: Vec<f64> = times.pts.iter().map(|&p| times.secs(p - first)).collect();
    let n = secs.len();
    let step = if n > 1 { (secs[n - 1] - secs[0]) / (n - 1) as f64 } else { 0.04 };
    secs.push(secs[n - 1] + step);
    secs
}

/// Running sums of the scored frames, for the mean of any range of frames.
struct Scores {
    at: Vec<usize>,
    cum: Vec<f64>,
}

impl Scores {
    /// `scores` in frame order.
    fn new(scores: &[(usize, f64)]) -> Scores {
        let mut cum = Vec::with_capacity(scores.len() + 1);
        cum.push(0.0);
        for (i, &(_, v)) in scores.iter().enumerate() {
            cum.push(cum[i] + v);
        }
        Scores { at: scores.iter().map(|s| s.0).collect(), cum }
    }

    /// Mean of the scored frames in [a, b), if there are any.
    fn mean(&self, a: usize, b: usize) -> Option<f64> {
        let (i, j) = (self.at.partition_point(|&f| f < a), self.at.partition_point(|&f| f < b));
        (j > i).then(|| (self.cum[j] - self.cum[i]) / (j - i) as f64)
    }
}

/// A stretch of the video as frame numbers [start, end), and its mean VMAF.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Window {
    pub start: usize,
    pub end: usize,
    pub vmaf: f64,
}

/// Every stretch of [`WINDOW_SECS`] within frames [from, to), a second apart; the last one
/// ends with the range, and a range shorter than a window is one window.
fn windows(secs: &[f64], scores: &Scores, from: usize, to: usize) -> Vec<Window> {
    let (t0, t1) = (secs[from], secs[to]);
    let frame_at = |t: f64| from + secs[from..to].partition_point(|&s| s < t - 1e-9);
    let mut out = Vec::new();
    let mut t = t0;
    loop {
        let last = t + WINDOW_SECS >= t1 - 1e-9;
        let (start, end) = if last { (frame_at((t1 - WINDOW_SECS).max(t0)), to) } else { (frame_at(t), frame_at(t + WINDOW_SECS)) };
        if let Some(vmaf) = scores.mean(start, end) {
            out.push(Window { start, end, vmaf });
        }
        if last {
            return out;
        }
        t += STEP_SECS;
    }
}

/// Frames [start, end) of the output that are encoded again as one piece.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Run {
    pub start: usize,
    pub end: usize,
}

impl Run {
    fn overlaps(&self, start: usize, end: usize) -> bool {
        self.start < end && start < self.end
    }
}

/// Which units (GOPs, or runs of them already encoded again; `units` holds the frame each
/// starts with) the windows below the floor are put down to: of the units a low window lies
/// over, those whose own part of it is below the floor. A window can be low without any
/// part being so only where a part holds no scored frame; the unit holding most of it is
/// taken then. A window that lies over a stretch in `settled` is let be.
fn blame(low: &[Window], units: &[usize], frames: usize, scores: &Scores, floor: f64, settled: &[Run]) -> Vec<bool> {
    let mut blamed = vec![false; units.len()];
    let end_of = |u: usize| units.get(u + 1).copied().unwrap_or(frames);
    for w in low {
        let first = units.partition_point(|&s| s <= w.start).saturating_sub(1);
        let over: Vec<usize> = (first..units.len()).take_while(|&u| units[u] < w.end).collect();
        // A settled stretch is what holds this window down, and it stays as it is.
        if over.iter().any(|&u| settled.iter().any(|s| s.overlaps(units[u], end_of(u)))) {
            continue;
        }
        let part = |u: usize| (units[u].max(w.start), end_of(u).min(w.end));
        let short: Vec<usize> = over.iter().copied().filter(|&u| scores.mean(part(u).0, part(u).1).is_some_and(|v| v < floor)).collect();
        if short.is_empty()
            && let Some(&u) = over.iter().max_by_key(|&&u| part(u).1 - part(u).0)
        {
            blamed[u] = true;
        }
        for u in short {
            blamed[u] = true;
        }
    }
    blamed
}

/// Neighbouring blamed units joined into runs of at most `max_secs` (a unit longer than
/// that is a run by itself).
fn runs(blamed: &[bool], units: &[usize], frames: usize, secs: &[f64], max_secs: f64) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::new();
    for (u, _) in blamed.iter().enumerate().filter(|(_, b)| **b) {
        let (start, end) = (units[u], units.get(u + 1).copied().unwrap_or(frames));
        match out.last_mut() {
            Some(r) if r.end == start && secs[end] - secs[r.start] <= max_secs => r.end = end,
            _ => out.push(Run { start, end }),
        }
    }
    out
}

/// The CRF to try after the encodes so far, or `None` where another try has nothing to
/// offer. `known` holds (CRF, lowest window) of what was measured, the last being the
/// latest; `lo` is the lowest CRF allowed. The second value says the floor is out of reach.
fn next_crf(known: &[(f64, f64)], floor: f64, lo: f64) -> (Option<f64>, bool) {
    let aim = floor + AIM_MARGIN;
    let &(crf, vmaf) = known.last().expect("a first point is always known");
    let slope_to = |other: (f64, f64)| ((vmaf - other.1) / (other.0 - crf)).clamp(0.25, 2.0);
    if vmaf < floor + ACCEPT {
        // Short: lower, by the slope from the nearest higher CRF measured.
        if crf <= lo {
            return (None, true);
        }
        let above = known.iter().copied().filter(|k| k.0 > crf).min_by(|a, b| a.0.total_cmp(&b.0));
        if let Some(a) = above
            && a.0 - crf >= 3.0
            && vmaf - a.1 < LEVEL_OFF
        {
            return (None, true);
        }
        let slope = above.map_or(SLOPE, slope_to);
        let next = search::quarter(crf - ((aim - vmaf) / slope).max(1.0)).max(lo);
        ((next < crf).then_some(next), false)
    } else if vmaf > aim + OVERSHOOT {
        // More than asked for: higher, toward the nearest higher CRF that fell short.
        let Some(a) = known.iter().copied().filter(|k| k.0 > crf && k.1 < aim).min_by(|a, b| a.0.total_cmp(&b.0)) else { return (None, false) };
        if a.0 - crf < 1.0 {
            return (None, false);
        }
        let next = search::quarter(crf + (vmaf - aim) / slope_to(a)).clamp(crf + 0.5, a.0 - 0.5);
        (Some(next), false)
    } else {
        (None, false)
    }
}

/// What a stretch was encoded again as.
struct Fixed {
    run: Run,
    crf: f64,
    file: PathBuf,
    /// Scores of its frames, numbered as the video's.
    scores: Vec<(usize, f64)>,
    /// Its lowest window before any repair, and now.
    before: f64,
    lowest: f64,
    bytes: u64,
    out_of_reach: bool,
}

/// One stretch that was encoded again, for the report.
#[derive(Debug, Clone, Serialize)]
pub struct Stretch {
    pub start_secs: f64,
    pub secs: f64,
    pub frames: usize,
    pub crf: f64,
    /// VMAF of its lowest five seconds before and after.
    pub vmaf_before: f64,
    pub vmaf: f64,
    pub bytes_before: u64,
    pub bytes: u64,
    /// No CRF brought it up to the floor (see [`LEVEL_OFF`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub out_of_reach: bool,
}

/// The whole-video check and what it led to, for the report.
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    /// Mean VMAF of the whole video and of its lowest five seconds, as it is kept.
    pub mean_vmaf: f64,
    pub lowest_vmaf: f64,
    /// The lowest five seconds before anything was encoded again.
    pub lowest_before: f64,
    /// The floor: what five seconds may score at least.
    pub floor: f64,
    pub stretches: Vec<Stretch>,
}

pub struct Outcome {
    pub summary: Summary,
    /// The video with the stretches replaced, as IVF, where any were.
    pub video: Option<PathBuf>,
    /// Frame scores of the video as it is kept, and when each frame is shown (seconds from
    /// the first; one more entry than frames).
    scores: Vec<(usize, f64)>,
    secs: Vec<f64>,
}

impl Outcome {
    /// Mean VMAF of the `secs` seconds starting `start` seconds after the first frame.
    pub fn at(&self, start: f64, secs: f64) -> Option<f64> {
        let n = self.secs.len() - 1;
        let frame = |t: f64| self.secs[..n].partition_point(|&s| s < t - 1e-9);
        Scores::new(&self.scores).mean(frame(start), frame(start + secs))
    }
}

/// Windows that stay below the floor although more bits would have helped: the tries ran out.
#[derive(Debug)]
pub struct StillLow {
    /// (seconds from the first frame, length in seconds, VMAF)
    pub windows: Vec<(f64, f64, f64)>,
}

impl std::fmt::Display for StillLow {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{} stretches stay below the floor after being encoded again", self.windows.len())
    }
}

impl std::error::Error for StillLow {}

pub struct Ctx<'a> {
    pub tools: &'a Tools,
    pub input: &'a Path,
    pub info: &'a MediaInfo,
    pub enc: &'a EncodeSettings,
    /// The CRF of the one-pass encode.
    pub crf: f64,
    /// What five seconds may score at least.
    pub floor: f64,
    pub min_crf: u8,
    pub threads: usize,
    pub dir: &'a Path,
}

/// Looks for stretches of `output` below the floor and encodes them again. Fails with
/// [`StillLow`] where stretches stay short that more tries could have helped; a stretch no
/// CRF brings up is kept at the best it was given.
pub fn repair(c: &Ctx, output: &Path, survey: &Survey) -> Result<Outcome> {
    let frames = survey.times.pts.len();
    let secs = timeline(&survey.times);
    let mut scores = survey.scores.clone();
    let lowest_of = |ws: &[Window]| ws.iter().map(|w| w.vmaf).fold(f64::INFINITY, f64::min);
    let all = windows(&secs, &Scores::new(&scores), 0, frames);
    let lowest_before = lowest_of(&all);
    let mean = |scores: &[(usize, f64)]| scores.iter().map(|s| s.1).sum::<f64>() / scores.len().max(1) as f64;
    let low_count = all.iter().filter(|w| w.vmaf < c.floor).count();
    eprintln!(
        "whole video: mean VMAF {:.2}, lowest {WINDOW_SECS:.0} s {lowest_before:.2}{}",
        mean(&scores),
        if low_count > 0 { format!(", {low_count} of {} stretches below {:.0}", all.len(), c.floor) } else { String::new() }
    );
    if low_count == 0 {
        let summary = Summary { mean_vmaf: mean(&scores), lowest_vmaf: lowest_before, lowest_before, floor: c.floor, stretches: Vec::new() };
        return Ok(Outcome { summary, video: None, scores, secs });
    }

    let plain = c.dir.join("repair.source.ivf");
    let args: Vec<String> = ["-y", "-v", "error", "-i", &path_arg(output), "-map", "0:v:0", "-c", "copy", "-f", "ivf", &path_arg(&plain)].map(String::from).to_vec();
    c.tools.run(&args)?;
    let (head, index) = ivf_index(&plain)?;
    if index.len() != frames {
        bail!("the encode holds {} frames as IVF, {frames} as it is", index.len());
    }
    if index.windows(2).any(|w| w[1].pts <= w[0].pts) {
        bail!("the encode's frames are not in the order they are shown");
    }

    let max_secs = scenes::Limits::for_video(c.info).max_secs;
    let mut units = survey.gops.clone();
    let mut fixed: Vec<Fixed> = Vec::new();
    let mut settled: Vec<Run> = Vec::new();
    let mut serial = 0;
    for round in 0..ROUNDS {
        let sc = Scores::new(&scores);
        let low: Vec<Window> = windows(&secs, &sc, 0, frames).into_iter().filter(|w| w.vmaf < c.floor).collect();
        let todo = runs(&blame(&low, &units, frames, &sc, c.floor, &settled), &units, frames, &secs, max_secs);
        if todo.is_empty() {
            break;
        }
        if let Some(r) = todo.iter().find(|r| secs[r.end] - secs[r.start] > MAX_GOP_SECS.max(max_secs)) {
            bail!("a stretch of {:.0} s between two key frames is too long to encode again", secs[r.end] - secs[r.start]);
        }
        let total: f64 = todo.iter().map(|r| secs[r.end] - secs[r.start]).sum();
        let detail = format!("{} stretch{} ({}) below VMAF {:.0}{}", todo.len(), if todo.len() == 1 { "" } else { "es" }, hms(total), c.floor, if round > 0 { ", again" } else { "" });
        eprintln!("encoding again: {detail}");
        c.tools.emit(Event::Phase { phase: "repairing".into(), detail });
        for (k, run) in todo.iter().enumerate() {
            c.tools.emit(Event::Progress { label: "repairing".into(), frac: k as f64 / todo.len() as f64, fps: 0.0, eta_secs: 0.0 });
            // What stands there now: the one-pass encode, or an earlier round's.
            let earlier: Vec<&Fixed> = fixed.iter().filter(|f| f.run.overlaps(run.start, run.end)).collect();
            let from_crf = earlier.iter().map(|f| f.crf).fold(c.crf, f64::min);
            let now = low.iter().filter(|w| run.overlaps(w.start, w.end)).map(|w| w.vmaf).fold(f64::INFINITY, f64::min);
            let before = earlier.iter().map(|f| f.before).fold(now, f64::min);
            let f = fix(c, &survey.times, &secs, *run, serial, from_crf, now, before)?;
            serial += 1;
            // It stands at the lowest CRF allowed: what is there stays.
            let Some(f) = f else {
                settled.push(*run);
                continue;
            };
            for old in fixed.iter().filter(|f| f.run.overlaps(run.start, run.end)) {
                let _ = std::fs::remove_file(&old.file);
            }
            fixed.retain(|f| !f.run.overlaps(run.start, run.end));
            scores.retain(|s| s.0 < run.start || s.0 >= run.end);
            scores.extend(f.scores.iter().copied());
            scores.sort_by_key(|s| s.0);
            units.retain(|&u| u <= run.start || u >= run.end);
            if f.out_of_reach {
                settled.push(*run);
            }
            fixed.push(f);
        }
    }
    fixed.sort_by_key(|f| f.run.start);

    let after = windows(&secs, &Scores::new(&scores), 0, frames);
    let still: Vec<(f64, f64, f64)> = after
        .iter()
        .filter(|w| w.vmaf < c.floor && !settled.iter().any(|s| s.overlaps(w.start, w.end)))
        .map(|w| (secs[w.start], secs[w.end] - secs[w.start], w.vmaf))
        .collect();
    if !still.is_empty() {
        return Err(StillLow { windows: still }.into());
    }

    let video = c.dir.join("repaired.ivf");
    splice(&plain, &head, &index, &fixed, &video)?;
    let _ = std::fs::remove_file(&plain);
    let stretches: Vec<Stretch> = fixed
        .iter()
        .map(|f| Stretch {
            start_secs: secs[f.run.start],
            secs: secs[f.run.end] - secs[f.run.start],
            frames: f.run.end - f.run.start,
            crf: f.crf,
            vmaf_before: f.before,
            vmaf: f.lowest,
            bytes_before: index[f.run.start..f.run.end].iter().map(|x| x.size as u64).sum(),
            bytes: f.bytes,
            out_of_reach: f.out_of_reach,
        })
        .collect();
    for f in &fixed {
        let _ = std::fs::remove_file(&f.file);
    }
    let summary = Summary { mean_vmaf: mean(&scores), lowest_vmaf: lowest_of(&after), lowest_before, floor: c.floor, stretches };
    Ok(Outcome { summary, video: Some(video), scores, secs })
}

/// Encodes the frames of `run` again, at the highest CRF found that keeps its lowest window
/// above the floor. `from_crf` and `now` are the CRF it stands at and its lowest window
/// there; `before`, its lowest window in the one-pass encode. `None` where it stands at the
/// lowest CRF allowed already.
#[allow(clippy::too_many_arguments)]
fn fix(c: &Ctx, times: &Times, secs: &[f64], run: Run, serial: usize, from_crf: f64, now: f64, before: f64) -> Result<Option<Fixed>> {
    struct Try {
        crf: f64,
        lowest: f64,
        file: PathBuf,
        scores: Vec<(usize, f64)>,
    }
    let at = hms(secs[run.start]);
    let reference = c.dir.join(format!("repair{serial:04}.ref.mkv"));
    // The frames between two of the source's own timestamps, as scene mode cuts a source
    // whose frames keep their times: the encode holds every source frame once.
    let b = Bounds { start: (run.start > 0).then(|| times.pts[run.start]), end: times.pts.get(run.end).copied() };
    scenes::extract_chunk(c.tools, c.input, c.info, b, Some(times.tb), &reference)?;
    let got = c.tools.count_frames(&reference)? as usize;
    if got != run.end - run.start {
        bail!("the stretch at {at} holds {got} frames cut from the source, {} in the encode", run.end - run.start);
    }
    let (model, pix) = (search::vmaf_model(c.info), search::reference_pix_fmt(c.info));
    let lo = (c.crf - CRF_SPREAD).max(c.min_crf as f64);
    let mut tries: Vec<Try> = Vec::new();
    let mut known = vec![(from_crf, now)];
    let out_of_reach = loop {
        let (next, out_of_reach) = next_crf(&known, c.floor, lo);
        let Some(crf) = next.filter(|_| tries.len() < TRIES) else { break out_of_reach };
        let file = c.dir.join(format!("repair{serial:04}.{}.ivf", tries.len()));
        let mut args: Vec<String> = ["-y", "-v", "error", "-i", &path_arg(&reference)].map(String::from).to_vec();
        args.extend(c.enc.video_args(crf));
        args.extend(["-f".into(), "ivf".into(), path_arg(&file)]);
        c.tools.run(&args)?;
        let scores: Vec<(usize, f64)> = c
            .tools
            .vmaf_each(&file, &reference, c.dir, model, pix, c.threads, VMAF_STRIDE)?
            .into_iter()
            .map(|(i, v)| (run.start + i as usize, v))
            .collect();
        let lowest = windows(secs, &Scores::new(&scores), run.start, run.end).iter().map(|w| w.vmaf).fold(f64::INFINITY, f64::min);
        if !lowest.is_finite() {
            bail!("the stretch at {at} could not be scored");
        }
        eprintln!("  {at} ({:.1} s): CRF {crf} gives VMAF {lowest:.2} in its lowest {WINDOW_SECS:.0} s (CRF {from_crf}: {now:.2})", secs[run.end] - secs[run.start]);
        known.push((crf, lowest));
        tries.push(Try { crf, lowest, file, scores });
    };
    let _ = std::fs::remove_file(&reference);
    // The smallest encode that holds the floor; failing that, the best one.
    let holds = |t: &Try| t.lowest >= c.floor + ACCEPT;
    let pick = (0..tries.len())
        .filter(|&i| holds(&tries[i]))
        .max_by(|&a, &b| tries[a].crf.total_cmp(&tries[b].crf))
        .or_else(|| (0..tries.len()).max_by(|&a, &b| tries[a].lowest.total_cmp(&tries[b].lowest)));
    let Some(pick) = pick else { return Ok(None) };
    let kept = tries.swap_remove(pick);
    for t in &tries {
        let _ = std::fs::remove_file(&t.file);
    }
    let bytes = ivf_index(&kept.file)?.1.iter().map(|f| f.size as u64).sum();
    let out_of_reach = out_of_reach && !holds(&kept);
    Ok(Some(Fixed { run, crf: kept.crf, file: kept.file, scores: kept.scores, before, lowest: kept.lowest, bytes, out_of_reach }))
}

/// Where a frame's bytes are in an IVF file, and its timestamp.
struct IvfFrame {
    offset: u64,
    size: u32,
    pts: u64,
}

/// The header of an IVF file and the place of each of its frames.
fn ivf_index(path: &Path) -> Result<([u8; 32], Vec<IvfFrame>)> {
    use std::io::{BufRead, Read};
    let mut r = std::io::BufReader::new(std::fs::File::open(path)?);
    // 32 bytes: "DKIF", version, header length, codec, width, height, time base denominator
    // and numerator, number of frames, four unused. Little-endian throughout.
    let mut head = [0u8; 32];
    r.read_exact(&mut head).context("not an IVF stream")?;
    if &head[..4] != b"DKIF" || u16::from_le_bytes([head[6], head[7]]) != 32 {
        bail!("not an IVF stream");
    }
    let mut frames = Vec::new();
    let mut offset = 32u64;
    // Each frame: its size in 4 bytes, its timestamp in 8, the frame.
    while !r.fill_buf()?.is_empty() {
        let mut f = [0u8; 12];
        r.read_exact(&mut f)?;
        let size = u32::from_le_bytes([f[0], f[1], f[2], f[3]]);
        let pts = u64::from_le_bytes(f[4..12].try_into().expect("eight bytes"));
        frames.push(IvfFrame { offset: offset + 12, size, pts });
        r.seek_relative(size as i64)?;
        offset += 12 + size as u64;
    }
    if offset != std::fs::metadata(path)?.len() {
        bail!("the IVF stream ends inside frame {}", frames.len());
    }
    Ok((head, frames))
}

/// Writes the video with the frames of each fixed run taken from its new encode. Every frame
/// keeps the timestamp the one-pass encode gave it, counted from the first.
fn splice(source: &Path, head: &[u8; 32], index: &[IvfFrame], fixed: &[Fixed], out: &Path) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::FileExt;
    let src = std::fs::File::open(source)?;
    let mut w = std::io::BufWriter::new(std::fs::File::create(out)?);
    w.write_all(head)?;
    let first = index[0].pts;
    let mut buf = Vec::new();
    let mut copy = |w: &mut std::io::BufWriter<std::fs::File>, from: &std::fs::File, f: &IvfFrame, pts: u64| -> Result<()> {
        buf.resize(f.size as usize, 0);
        from.read_exact_at(&mut buf, f.offset)?;
        w.write_all(&f.size.to_le_bytes())?;
        w.write_all(&(pts - first).to_le_bytes())?;
        w.write_all(&buf)?;
        Ok(())
    };
    let mut i = 0;
    let mut next = fixed.iter().peekable();
    while i < index.len() {
        match next.peek() {
            Some(f) if f.run.start == i => {
                let (_, new) = ivf_index(&f.file)?;
                if new.len() != f.run.end - f.run.start {
                    bail!("a stretch encoded again holds {} frames, {} were to be replaced; refusing to join", new.len(), f.run.end - f.run.start);
                }
                let file = std::fs::File::open(&f.file)?;
                for (k, frame) in new.iter().enumerate() {
                    copy(&mut w, &file, frame, index[i + k].pts)?;
                }
                i = f.run.end;
                next.next();
            }
            _ => {
                copy(&mut w, &src, &index[i], index[i].pts)?;
                i += 1;
            }
        }
    }
    if next.peek().is_some() {
        bail!("a stretch encoded again does not start at a frame of the video; refusing to join");
    }
    w.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 30 fps: frame `i` at `i / 30` s.
    fn secs(frames: usize) -> Vec<f64> {
        (0..=frames).map(|i| i as f64 / 30.0).collect()
    }

    /// Every third frame scored, `f(frame)` each.
    fn scored(frames: usize, f: impl Fn(usize) -> f64) -> Vec<(usize, f64)> {
        (0..frames).step_by(3).map(|i| (i, f(i))).collect()
    }

    #[test]
    fn windows_cover_the_range_a_second_apart() {
        let s = secs(900);
        let sc = Scores::new(&scored(900, |_| 95.0));
        let w = windows(&s, &sc, 0, 900);
        // 30 s: windows start at 0, 1, ... 25 s.
        assert_eq!(w.len(), 26);
        assert_eq!((w[0].start, w[0].end), (0, 150));
        assert_eq!((w[1].start, w[1].end), (30, 180));
        assert_eq!((w[25].start, w[25].end), (750, 900));
        assert!(w.iter().all(|x| (x.vmaf - 95.0).abs() < 1e-9));
    }

    #[test]
    fn a_range_shorter_than_a_window_is_one_window() {
        let s = secs(900);
        let sc = Scores::new(&scored(900, |i| if i < 450 { 90.0 } else { 80.0 }));
        let w = windows(&s, &sc, 420, 510);
        assert_eq!(w.len(), 1);
        assert_eq!((w[0].start, w[0].end), (420, 510));
        // A third of it at 90, two thirds at 80.
        assert!((w[0].vmaf - 83.33).abs() < 0.2, "{}", w[0].vmaf);
    }

    #[test]
    fn the_last_window_ends_with_the_range() {
        // 7.5 s: windows at 0, 1, 2 and one over the last five seconds.
        let s = secs(225);
        let sc = Scores::new(&scored(225, |_| 95.0));
        let w = windows(&s, &sc, 0, 225);
        assert_eq!(w.iter().map(|x| (x.start, x.end)).collect::<Vec<_>>(), vec![(0, 150), (30, 180), (60, 210), (75, 225)]);
    }

    #[test]
    fn only_the_gop_that_is_low_is_blamed() {
        // GOPs of 5 s; the third (frames 300-450) sits at 84, the rest at 95.
        let (frames, gops) = (900, vec![0, 150, 300, 450, 600, 750]);
        let s = secs(frames);
        let sc = Scores::new(&scored(frames, |i| if (300..450).contains(&i) { 84.0 } else { 95.0 }));
        let low: Vec<Window> = windows(&s, &sc, 0, frames).into_iter().filter(|w| w.vmaf < 89.0).collect();
        assert!(!low.is_empty());
        let blamed = blame(&low, &gops, frames, &sc, 89.0, &[]);
        assert_eq!(blamed, vec![false, false, true, false, false, false]);
        assert_eq!(runs(&blamed, &gops, frames, &s, 20.0), vec![Run { start: 300, end: 450 }]);
    }

    #[test]
    fn a_dip_across_a_key_frame_blames_both_sides() {
        // Low from 4 s to 7 s: the end of the first GOP and the start of the second.
        let (frames, gops) = (900, vec![0, 150, 300, 450, 600, 750]);
        let s = secs(frames);
        let sc = Scores::new(&scored(frames, |i| if (120..210).contains(&i) { 70.0 } else { 96.0 }));
        let low: Vec<Window> = windows(&s, &sc, 0, frames).into_iter().filter(|w| w.vmaf < 89.0).collect();
        let blamed = blame(&low, &gops, frames, &sc, 89.0, &[]);
        assert_eq!(blamed, vec![true, true, false, false, false, false]);
        assert_eq!(runs(&blamed, &gops, frames, &s, 20.0), vec![Run { start: 0, end: 300 }]);
    }

    #[test]
    fn settled_stretches_are_left_alone() {
        let (frames, gops) = (900, vec![0, 150, 300, 450, 600, 750]);
        let s = secs(frames);
        let sc = Scores::new(&scored(frames, |i| if (300..450).contains(&i) { 84.0 } else { 95.0 }));
        let low: Vec<Window> = windows(&s, &sc, 0, frames).into_iter().filter(|w| w.vmaf < 89.0).collect();
        let blamed = blame(&low, &gops, frames, &sc, 89.0, &[Run { start: 300, end: 450 }]);
        assert!(blamed.iter().all(|b| !b), "{blamed:?}");
    }

    #[test]
    fn runs_stop_at_the_longest_chunk() {
        let (frames, gops) = (900, vec![0, 150, 300, 450, 600, 750]);
        let s = secs(frames);
        // All six blamed, 12 s at most per run: two GOPs each (three would be 15 s).
        let r = runs(&[true; 6], &gops, frames, &s, 12.0);
        assert_eq!(r, vec![Run { start: 0, end: 300 }, Run { start: 300, end: 600 }, Run { start: 600, end: 900 }]);
        // A gap keeps runs apart.
        let r = runs(&[true, false, true, true, false, false], &gops, frames, &s, 20.0);
        assert_eq!(r, vec![Run { start: 0, end: 150 }, Run { start: 300, end: 600 }]);
    }

    #[test]
    fn the_first_try_goes_by_the_usual_slope() {
        // 85 at CRF 38, floor 89: aim 90, five points at one a step.
        let (next, out) = next_crf(&[(38.0, 85.0)], 89.0, 20.0);
        assert_eq!((next, out), (Some(33.0), false));
        // Never less than a whole step.
        assert_eq!(next_crf(&[(38.0, 88.9)], 89.0, 20.0).0, Some(36.75));
        assert_eq!(next_crf(&[(38.0, 89.1)], 89.0, 20.0).0, Some(37.0));
    }

    #[test]
    fn later_tries_go_by_what_was_measured() {
        // 38 -> 85, 32 -> 88: half a point a step, so two more points take four.
        let (next, out) = next_crf(&[(38.0, 85.0), (32.0, 88.0)], 89.0, 20.0);
        assert_eq!((next, out), (Some(28.0), false));
        // A try within the band is kept.
        assert_eq!(next_crf(&[(38.0, 85.0), (32.0, 90.5)], 89.0, 20.0), (None, false));
        // Too good by far: back up, part of the way to the CRF that fell short.
        let (next, _) = next_crf(&[(38.0, 85.0), (30.0, 95.0)], 89.0, 20.0);
        assert_eq!(next, Some(34.0));
    }

    #[test]
    fn a_stretch_no_crf_helps_is_out_of_reach() {
        // Six steps for 0.2: more bits add nothing.
        assert_eq!(next_crf(&[(38.0, 85.0), (32.0, 85.2)], 89.0, 20.0), (None, true));
        // At the lowest CRF allowed and still short.
        assert_eq!(next_crf(&[(38.0, 85.0), (22.0, 88.0)], 89.0, 22.0), (None, true));
        // Standing at the lowest CRF before any try.
        assert_eq!(next_crf(&[(22.0, 85.0)], 89.0, 22.0), (None, true));
        // The lowest CRF allowed is tried before giving up.
        assert_eq!(next_crf(&[(24.0, 80.0)], 89.0, 22.0), (Some(22.0), false));
    }

    fn write_ivf(path: &Path, frames: &[(u64, Vec<u8>)]) {
        let mut b = Vec::new();
        b.extend(b"DKIF");
        b.extend(0u16.to_le_bytes());
        b.extend(32u16.to_le_bytes());
        b.extend(b"AV01");
        b.extend(64u16.to_le_bytes());
        b.extend(64u16.to_le_bytes());
        b.extend(30u32.to_le_bytes());
        b.extend(1u32.to_le_bytes());
        b.extend((frames.len() as u32).to_le_bytes());
        b.extend(0u32.to_le_bytes());
        for (pts, data) in frames {
            b.extend((data.len() as u32).to_le_bytes());
            b.extend(pts.to_le_bytes());
            b.extend(data);
        }
        std::fs::write(path, b).unwrap();
    }

    fn fixed(run: Run, file: PathBuf) -> Fixed {
        Fixed { run, crf: 30.0, file, scores: Vec::new(), before: 80.0, lowest: 90.0, bytes: 0, out_of_reach: false }
    }

    #[test]
    fn splice_swaps_frames_and_keeps_their_times() {
        let dir = std::env::temp_dir().join(format!("av1opt-splice-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Six frames from timestamp 100 on, uneven steps; frames 2-3 and 5 are replaced.
        let src: Vec<(u64, Vec<u8>)> = [100u64, 110, 125, 130, 150, 151].iter().enumerate().map(|(i, &p)| (p, vec![i as u8; i + 1])).collect();
        write_ivf(&dir.join("src.ivf"), &src);
        write_ivf(&dir.join("a.ivf"), &[(0, vec![0xaa; 7]), (1, vec![0xab; 2])]);
        write_ivf(&dir.join("b.ivf"), &[(0, vec![0xbb; 3])]);
        let (head, index) = ivf_index(&dir.join("src.ivf")).unwrap();
        assert_eq!(index.len(), 6);
        let fx = vec![fixed(Run { start: 2, end: 4 }, dir.join("a.ivf")), fixed(Run { start: 5, end: 6 }, dir.join("b.ivf"))];
        splice(&dir.join("src.ivf"), &head, &index, &fx, &dir.join("out.ivf")).unwrap();
        let (head2, out) = ivf_index(&dir.join("out.ivf")).unwrap();
        assert_eq!(head, head2);
        assert_eq!(out.iter().map(|f| f.pts).collect::<Vec<_>>(), vec![0, 10, 25, 30, 50, 51]);
        assert_eq!(out.iter().map(|f| f.size).collect::<Vec<_>>(), vec![1, 2, 7, 2, 5, 3]);
        let bytes = std::fs::read(dir.join("out.ivf")).unwrap();
        assert_eq!(&bytes[out[2].offset as usize..][..7], &[0xaa; 7]);
        assert_eq!(&bytes[out[5].offset as usize..][..3], &[0xbb; 3]);
        assert_eq!(&bytes[out[4].offset as usize..][..5], &[4; 5]);

        // A new encode with the wrong number of frames is refused.
        let fx = vec![fixed(Run { start: 2, end: 5 }, dir.join("a.ivf"))];
        let err = splice(&dir.join("src.ivf"), &head, &index, &fx, &dir.join("bad.ivf")).unwrap_err();
        assert!(err.to_string().contains("refusing to join"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_cut_off_ivf_is_refused() {
        let dir = std::env::temp_dir().join(format!("av1opt-ivf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        write_ivf(&dir.join("x.ivf"), &[(0, vec![1; 10]), (1, vec![2; 10])]);
        let mut b = std::fs::read(dir.join("x.ivf")).unwrap();
        b.truncate(b.len() - 4);
        std::fs::write(dir.join("x.ivf"), b).unwrap();
        assert!(ivf_index(&dir.join("x.ivf")).is_err());
        std::fs::write(dir.join("y.ivf"), b"not an ivf file at all, just text that is long enough").unwrap();
        assert!(ivf_index(&dir.join("y.ivf")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn outcome_reads_any_stretch() {
        let o = Outcome {
            summary: Summary { mean_vmaf: 0.0, lowest_vmaf: 0.0, lowest_before: 0.0, floor: 89.0, stretches: Vec::new() },
            video: None,
            scores: scored(900, |i| if i < 300 { 90.0 } else { 96.0 }),
            secs: secs(900),
        };
        assert_eq!(o.at(0.0, 5.0), Some(90.0));
        assert_eq!(o.at(10.0, 5.0), Some(96.0));
        assert!((o.at(7.5, 5.0).unwrap() - 93.0).abs() < 0.1);
        assert_eq!(o.at(40.0, 5.0), None);
    }
}
