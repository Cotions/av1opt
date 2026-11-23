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
use crate::sections::Sections;
use crate::search::{self, EncodeSettings, VMAF_STRIDE};
use crate::sequence_header;
use crate::tools::{Tools, WENT_WRONG_INSIDE, hms, path_arg};
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
const OVERSHOOT: f64 = 1.5;
/// VMAF gained per CRF step in a weak stretch, to start from: the weak stretches of real
/// videos gained 0.72 to 0.93 a step, more than the mean of a video gains
/// (`search::VMAF_PER_CRF`). Later tries go by what the stretch itself showed.
const SLOPE: f64 = 0.8;
/// The least gain per CRF step a later try goes by (see [`next_try`]).
const LEAST_SLOPE: f64 = 0.5;
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
/// Rounds more for the stretches still further below the floor than [`NEAR`] after those,
/// while they are few (see [`MOST_LEFT_LOW`]): their tries ran out on the way up, and a few
/// more are cheaper than the second search of the whole video they would be sent to.
const MORE_ROUNDS: usize = 3;
/// After the last round, windows this far below the floor at most are let be: the video is
/// kept with them and the report says so.
const NEAR: f64 = 1.0;
/// Stretches lower than that get [`MORE_ROUNDS`], and what stays low after them is kept too,
/// each at the best encode its tries gave it, and named in the report, as long as it comes to
/// no more than this share of the video. A
/// second search lowers the CRF of all of it to lift them, or gives the video up as not worth
/// encoding: that is for a video weak throughout, not for a few stretches the tries ran out on.
/// A twentieth is three minutes of an hour: more than that kept a point or more under the
/// floor is no longer a few stretches but a video that looks worse than was asked for.
const MOST_LEFT_LOW: f64 = 0.05;
/// GOPs longer than this are not cut out: their lossless reference would be too large.
const MAX_GOP_SECS: f64 = 30.0;
/// Where the imitation of a repair puts a stretch: this far above the floor it was encoded
/// again for. The 57 stretches of three videos ended 1.3 to 1.5 above it on average in their
/// lowest five seconds, and their means rose more than an even rise toward 100 gives for that;
/// counted as 1.75 the mean foretold was within 0.03 of the mean each video ended with, where
/// 0.75 foretold 0.15 to 0.24 too little.
const IMITATED_LANDING: f64 = 1.75;
/// The share of a video that may be encoded again to bring its mean up, on top of the repairs
/// below the floor. Past that a lower CRF for the whole video is the cheaper way there.
const MOST_TOPPED_UP: f64 = 0.25;
/// A mean this close under the target gets no further round: it is less than two scores of the
/// same frames differ by.
pub const MEAN_SLACK: f64 = 0.05;
/// How much higher GOPs may be lifted in the one further round a mean still short gets.
const LEVEL_RAISE: f64 = 0.5;
/// How far above the target the mean foretold after the repairs may lie before the video is
/// encoded again at a higher CRF. Such a file is about a tenth larger than it needs to be, which
/// is worth one more encode. Less is not: at 0.7 the second encode took 4 to 6% off for twice the
/// time on that video, where the same time spent on the next video saves many times that.
const MOST_OVER: f64 = 1.5;
/// The least a GOP is lifted for the mean: about what a quarter step of CRF gives, so that an
/// encode is not spent on less than it can deliver.
const LEAST_LIFT: f64 = 0.25;
/// A GOP encoded again for the mean counts as landed from this far below its level; what it
/// falls short by is left to the GOPs after it.
const LANDING_SHORT: f64 = 0.15;
/// A GOP lifted for the mean is tried once more at a higher CRF from this far above its level.
const LANDING_OVER: f64 = 0.5;
/// What the levels are worked out to reach beyond the target, so that a last GOP landing a
/// little low does not send one more GOP after a hundredth of a point.
const MEAN_EXTRA: f64 = 0.01;

/// A scored frame this far under the median of the scored frames around it was not made so
/// by a hard scene, where the frames around it are low too: something went wrong with it, in
/// the encode, or in the decoding of either video for the check. Its GOP is encoded again as
/// one with a window below the floor is. In one run of four of the same five-minute video,
/// made while the machine was very busy, eight scored frames in a row read VMAF 9 to 54 among
/// frames at 91 to 94: one GOP of the source, which the check had decoded wrongly. In the
/// first encodes of 21 runs on 9 sources (95,000 scored frames) nothing else lay more than
/// 19.2 under, and the eight lay 38 to 83 under. One such scored frame takes 1.6 off the mean
/// of a five-second window, which a window at 91 does not show.
const FAR_BELOW: f64 = 25.0;
/// The frames around a frame: the scored ones shown within this long of it, either side (at
/// the ends of the video, the two seconds it begins or ends with). A burst is seen while it is
/// shorter than half of that; a longer one of wrecked frames takes its window under the floor.
const AROUND_SECS: f64 = 1.0;
/// Fewer scored frames around a frame than this say nothing of it.
const LEAST_AROUND: usize = 6;
/// A stretch holding a frame far below those around it counts as this far under the floor,
/// however far under the frame is: how badly a frame is wrecked says nothing of the CRF that
/// would mend it. The next try is then five CRF steps down, and a frame that scores no better
/// there is how the picture is (see [`LEVEL_OFF`]).
const FAR_BELOW_COUNTS: f64 = 3.0;

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
/// two don't hold the same number of frames, the source's timestamps can't be read, too many
/// of its frames are shown for unequal times (see [`UNEVEN_STEPS_IN_A_HUNDRED`]), or a GOP
/// starts where no cut can be made.
///
/// With `cut` (sections removed) the source is decoded through the same picture filters as the
/// encode was (see [`Sections::picture_filters`]): both sides then hold the frames the output
/// shows, in its order, and the times are those of the output's picture. Should the two not
/// hold as many frames, this is `None` like the rest: frames would be scored against others.
pub fn survey(tools: &Tools, input: &Path, info: &MediaInfo, output: &Path, cut: Option<&Sections>, threads: usize, dir: &Path) -> Result<Option<Survey>> {
    let keys = tools.key_packets(output)?;
    let gops: Vec<usize> = keys.iter().enumerate().filter(|(_, k)| **k).map(|(i, _)| i).collect();
    if gops.first() != Some(&0) {
        return Ok(None);
    }
    let pix = search::reference_pix_fmt(info);
    let mut vf: Vec<String> = cut.map(|c| c.picture_filters(info.duration_s).to_vec()).unwrap_or_default();
    vf.extend(search::source_filters(info));
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
    let mut args: Vec<String> = ["-y", "-copyts"].map(String::from).to_vec();
    // The picture of a cut is picked by times counted from the start of the file, as the
    // encode counted them.
    if cut.is_some() {
        args.push("-start_at_zero".into());
    }
    args.extend(["-i", &path_arg(output), "-i", &path_arg(&std::path::absolute(input)?), "-lavfi", &filter].map(String::from));
    // And the start of the file is where the encode had it (see `Sections::sound_read_too`).
    match cut.filter(|_| !info.audio.is_empty()) {
        Some(_) => args.extend(Sections::sound_read_too(1, None)),
        None => args.push("-an".into()),
    }
    args.extend(["-sn", "-dn", "-f", "null", "-"].map(String::from));
    // The scored frames are numbered in seconds (`settb=1`), so the output's clock reads
    // frames where it says seconds.
    tools.run_with_progress_in(Some(dir), &args, keys.len() as f64, "checking")?;
    let text = std::fs::read_to_string(dir.join(TIMES_FILE)).unwrap_or_default();
    let log = std::fs::read(dir.join(LOG_FILE)).context("libvmaf wrote no log")?;
    let _ = std::fs::remove_file(dir.join(TIMES_FILE));
    let _ = std::fs::remove_file(dir.join(LOG_FILE));
    let Some(times) = Times::parse_with_shared_times(&text, info.time_base) else { return Ok(None) };
    if times.pts.len() != keys.len() || !times.uneven_steps_within(UNEVEN_STEPS_IN_A_HUNDRED) {
        return Ok(None);
    }
    // A stretch is cut from the source between the times of two GOP starts: where a GOP
    // starts on a frame that shares its time with the one before, no cut can part them.
    if gops.iter().any(|&g| g > 0 && times.pts[g] == times.pts[g - 1]) {
        return Ok(None);
    }
    let scores: Vec<(usize, f64)> = crate::tools::parse_vmaf_frames(&log)?.into_iter().map(|(n, v)| (n as usize, v)).collect();
    if scores.last().is_none_or(|&(n, _)| n >= keys.len() || n + (VMAF_STRIDE as usize) < keys.len()) {
        return Ok(None);
    }
    Ok(Some(Survey { scores, times, gops }))
}

/// How many steps in a hundred between frames may be uneven (under half the usual step, or
/// over one and a half times it; two frames at one time count) for a video to be scored
/// along its whole length. The scores are counted a frame each, not by how long each frame
/// is shown: with a tenth of the frames shown twice as long, the mean read is off by under a
/// tenth of what those frames score apart from the rest. Past that (a screen recording that
/// holds frames for seconds) the frames that matter most to the eye would weigh least, and
/// spot checks, scored along time, judge instead.
///
/// It stood at 1 before, which sent two of three real videos copied into Matroska to spot
/// checks: one with 3.15% of its frames shown twice as long, one with 10 pairs of frames in
/// 53,841 sharing a millisecond. Neither was repaired: their lowest 5 seconds read VMAF 87.0
/// and 84.5 against a floor of 89.
const UNEVEN_STEPS_IN_A_HUNDRED: usize = 10;

/// Whether the survey will go by frames shown at these times, sorted. What it cannot know
/// before the encode is where the GOPs start: a video whose GOP starts on a frame sharing
/// its time with the one before is still sent to spot checks afterwards.
fn times_can_be_gone_by(pts: Vec<i64>, tb: (i64, i64)) -> bool {
    pts.len() < 2 || Times { pts, tb }.uneven_steps_within(UNEVEN_STEPS_IN_A_HUNDRED)
}

/// Whether the source's frames can be set against the encode's one for one, as the survey
/// will ask, read off its packets without decoding. Only then can a one-pass encode be scored
/// along its whole length and repaired, which the search counts on where it can. A source
/// whose packets can't be read so counts as steady: the survey decides then.
pub fn steady_source(tools: &Tools, input: &Path, info: &MediaInfo) -> bool {
    let mut command = crate::tools::probe_command(&tools.ffprobe);
    command.args(["-v", "error", "-select_streams", &info.video_index.to_string(), "-show_entries", "packet=pts", "-of", "csv=p=0"]).arg(crate::tools::probe_arg(input));
    let Ok(out) = crate::probe::run_ffprobe(command, crate::probe::Reads::all_of(input)) else { return true };
    if !out.status.success() {
        return true;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut pts: Vec<i64> = text.lines().filter_map(|l| l.trim().trim_end_matches(',').parse().ok()).collect();
    // Packets are stored in decoding order; a frame is shown in the order of its timestamp.
    pts.sort_unstable();
    times_can_be_gone_by(pts, info.time_base)
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

/// The scored frames among frames [from, to) that lie more than [`FAR_BELOW`] under the median
/// of the scored frames around them (see [`AROUND_SECS`]): the place of each in `scores`, and
/// that median. A scene of low scores is not found: the frames around its frames are low too.
fn far_below(secs: &[f64], scores: &[(usize, f64)], from: usize, to: usize) -> Vec<(usize, f64)> {
    let end = secs[secs.len() - 1];
    let mut found = Vec::new();
    let mut around: Vec<f64> = Vec::new();
    for i in scores.partition_point(|s| s.0 < from)..scores.partition_point(|s| s.0 < to) {
        // Near either end of the video the frames around are the ones on the other side.
        let first = (secs[scores[i].0] - AROUND_SECS).min(end - 2.0 * AROUND_SECS).max(0.0);
        let (a, b) = (scores.partition_point(|s| secs[s.0] < first - 1e-9), scores.partition_point(|s| secs[s.0] <= first + 2.0 * AROUND_SECS + 1e-9));
        around.clear();
        around.extend(scores[a..b].iter().filter(|s| s.0 != scores[i].0).map(|s| s.1));
        if around.len() < LEAST_AROUND {
            continue;
        }
        around.sort_by(f64::total_cmp);
        // Of two middle ones the lower: the first frame of a low scene has as many high
        // frames before it as low ones after it, and belongs with the low ones.
        let median = around[(around.len() - 1) / 2];
        if scores[i].1 < median - FAR_BELOW {
            found.push((i, median));
        }
    }
    found
}

/// How many scored frames of `run` lie far below those around them (see [`far_below`]), with
/// `new` in place of the run's own frames in `scores`.
fn far_below_in(secs: &[f64], scores: &[(usize, f64)], new: Option<&[(usize, f64)]>, run: Run) -> usize {
    match new {
        Some(new) => far_below(secs, &with_run(scores, new, run), run.start, run.end).len(),
        None => far_below(secs, scores, run.start, run.end).len(),
    }
}

/// `scores` with `new` in place of the frames of `run`.
fn with_run(scores: &[(usize, f64)], new: &[(usize, f64)], run: Run) -> Vec<(usize, f64)> {
    let (at, after) = (scores.partition_point(|s| s.0 < run.start), scores.partition_point(|s| s.0 < run.end));
    scores[..at].iter().chain(new).chain(&scores[after..]).copied().collect()
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

/// What a stretch is encoded again for.
#[derive(Debug, Clone, Copy)]
struct Goal {
    /// The score asked for.
    line: f64,
    /// A new encode counts as reaching it from here.
    holds_from: f64,
    /// Where tries aim.
    aim: f64,
    /// A new encode above this spent bits nothing asked for.
    too_much: f64,
    /// The smallest step down in CRF a try takes.
    least_step: f64,
    /// Whether the stretch's own mean is what counts, rather than its lowest window.
    by_mean: bool,
    /// The stretch holds frames far below those around them (see [`FAR_BELOW`]): it counts as
    /// short of the floor while it does, whatever its windows come to.
    wrecked: bool,
    /// The first try is at the CRF the stretch stands at.
    same_first: bool,
}

impl Goal {
    /// Holding `floor` in the lowest window over the stretch.
    fn holding(floor: f64) -> Goal {
        let aim = floor + AIM_MARGIN;
        Goal { line: floor, holds_from: floor + ACCEPT, aim, too_much: aim + OVERSHOOT, least_step: 1.0, by_mean: false, wrecked: false, same_first: false }
    }

    /// Bringing the stretch's own mean to `level`, and no further than that needs.
    fn mean(level: f64) -> Goal {
        Goal { line: level, holds_from: level - LANDING_SHORT, aim: level, too_much: level + LANDING_OVER, least_step: 0.25, by_mean: true, wrecked: false, same_first: false }
    }
}

/// The CRF to try after the encodes so far, or `None` where another try has nothing to
/// offer. `known` holds (CRF, score) of what was measured, the last being the latest, the
/// score being what `goal` goes by; `lo` is the lowest CRF allowed. The second value says the
/// goal is out of reach.
fn next_try(known: &[(f64, f64)], goal: Goal, lo: f64) -> (Option<f64>, bool) {
    let &(crf, vmaf) = known.last().expect("a first point is always known");
    // Two scores a CRF step or two apart say little about the gain per step (0.25 was read
    // off 1.5 steps of a stretch that then gained 0.7 a step, and its next try went four
    // steps too far): no less than this is gone by.
    let slope_to = |other: (f64, f64)| ((vmaf - other.1) / (other.0 - crf)).clamp(LEAST_SLOPE, 2.0);
    if vmaf < goal.holds_from {
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
        // At the gain it showed over two CRF steps or more, the goal lies below the lowest
        // CRF allowed.
        if let Some(a) = above
            && a.0 - crf >= 2.0
            && crf - (goal.line - vmaf) / ((vmaf - a.1) / (a.0 - crf)).max(1e-6) < lo
        {
            return (None, true);
        }
        let slope = above.map_or(SLOPE, slope_to);
        let next = search::quarter(crf - ((goal.aim - vmaf) / slope).max(goal.least_step)).max(lo);
        ((next < crf).then_some(next), false)
    } else if vmaf > goal.too_much {
        // More than asked for: higher, toward the nearest higher CRF that fell short.
        let Some(a) = known.iter().copied().filter(|k| k.0 > crf && k.1 < goal.aim).min_by(|a, b| a.0.total_cmp(&b.0)) else { return (None, false) };
        if a.0 - crf < 1.0 {
            return (None, false);
        }
        let next = search::quarter(crf + (vmaf - goal.aim) / slope_to(a)).clamp(crf + 0.5, a.0 - 0.5);
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
    /// Its lowest window before any repair.
    before: f64,
    bytes: u64,
    out_of_reach: bool,
    /// Encoded again for the video's mean, not for the floor.
    for_mean: bool,
    /// Encoded again for frames far below those around them, its windows holding the floor.
    for_frames: bool,
}

/// One stretch that was encoded again, for the report.
#[derive(Debug, Clone, Serialize)]
pub struct Stretch {
    pub start_secs: f64,
    pub secs: f64,
    pub frames: usize,
    pub crf: f64,
    /// VMAF of the lowest of the video's five-second windows that lie over it, before and
    /// after.
    pub vmaf_before: f64,
    pub vmaf: f64,
    pub bytes_before: u64,
    pub bytes: u64,
    /// No CRF brought it up to the floor (see [`LEVEL_OFF`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub out_of_reach: bool,
    /// It stood above the floor, and was encoded again to bring the video's mean to the target.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub for_mean: bool,
    /// It stood above the floor, and was encoded again for frames that scored far below those
    /// around them (see [`FarBelow`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub for_frames: bool,
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
    /// Five-second windows left just under the floor (by [`NEAR`] at most) when the rounds
    /// ran out.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub near: usize,
    /// Lowest five seconds of each stretch below the floor that no CRF tried brought
    /// nearer to it, and that stays as the one pass made it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub left: Vec<f64>,
    /// Stretches further below the floor than [`NEAR`] when the rounds ran out, kept as they
    /// came out because they are few (see [`MOST_LEFT_LOW`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub still_low: Vec<LowStretch>,
    /// Frames the check found far below those around them, by the GOP they are in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub far_below: Vec<FarBelow>,
    /// Mean VMAF of the whole video before anything was encoded again.
    pub mean_before: f64,
    /// What was done beyond the floor to bring the mean to the target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_up: Option<TopUp>,
    /// Seconds the scoring and the encoding again took.
    pub secs: f64,
}

/// GOPs above the floor encoded again to bring the video's mean to the target.
#[derive(Debug, Clone, Serialize)]
pub struct TopUp {
    /// Mean VMAF of the video before them, with the stretches below the floor repaired.
    pub mean_from: f64,
    /// The highest level a GOP was lifted to.
    pub level: f64,
    pub stretches: usize,
    pub secs: f64,
    /// What they added to the file.
    pub bytes: i64,
}

/// Scored frames of one GOP that the check found far below the frames around them: not how a
/// hard scene scores, so the GOP was encoded again whatever its windows came to.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct FarBelow {
    /// When the first of them is shown, in seconds from the first frame.
    pub start_secs: f64,
    /// How many of the scored frames (every [`VMAF_STRIDE`]-th is scored).
    pub frames: usize,
    /// The lowest of them, and the median of the scored frames around that one.
    pub vmaf: f64,
    pub around: f64,
    /// How many scored frames of that GOP are far below those around them in the video as kept.
    pub still: usize,
}

/// A stretch of the video that stays below the floor: windows that do, run together.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct LowStretch {
    /// Seconds from the first frame, and how long.
    pub start_secs: f64,
    pub secs: f64,
    /// Its lowest five seconds.
    pub vmaf: f64,
}

/// Stretches named in a note; the report has them all.
const NAMED_IN_NOTE: usize = 8;

impl Summary {
    /// What the report says of frames found far below those around them, if there were any.
    pub fn far_below_note(&self) -> Option<String> {
        let first = self.far_below.first()?;
        let frames: usize = self.far_below.iter().map(|f| f.frames).sum();
        let still: usize = self.far_below.iter().map(|f| f.still).sum();
        let lowest = self.far_below.iter().copied().min_by(|a, b| a.vmaf.total_cmp(&b.vmaf)).unwrap_or(*first);
        let places = self.far_below.len();
        let named: Vec<String> = self.far_below.iter().take(NAMED_IN_NOTE).map(|f| hms(f.start_secs)).collect();
        let more = if places > NAMED_IN_NOTE { format!(" and {} more places", places - NAMED_IN_NOTE) } else { String::new() };
        Some(format!(
            "{frames} frame{} at {}{more} scored far below the frames around {} (VMAF {:.1}{} among frames at {:.1}), which is not how a hard scene looks: {} encoded again{}",
            if frames == 1 { "" } else { "s" },
            named.join(", "),
            if frames == 1 { "it" } else { "them" },
            lowest.vmaf,
            if frames == 1 { "" } else { " at the lowest" },
            lowest.around,
            if places == 1 { "that stretch was" } else { "those stretches were" },
            match still {
                0 => String::new(),
                1 => ", and 1 frame still scores so: kept as the picture is there".to_string(),
                n => format!(", and {n} frames still score so: kept as the picture is there"),
            }
        ))
    }

    /// What the report says of the stretches kept far below the floor, if there are any.
    pub fn still_low_note(&self) -> Option<String> {
        let count = self.still_low.len();
        if count == 0 {
            return None;
        }
        let named: Vec<String> = self.still_low.iter().take(NAMED_IN_NOTE).map(|s| format!("{} for {:.0} s (VMAF {:.1})", hms(s.start_secs), s.secs, s.vmaf)).collect();
        let more = if count > NAMED_IN_NOTE { format!(" and {} more", count - NAMED_IN_NOTE) } else { String::new() };
        Some(format!(
            "{count} stretch{} could not be brought up to VMAF {:.0}: {}{more}; the video is kept with {} at the best encode tried rather than searched and encoded again whole",
            if count == 1 { "" } else { "es" },
            self.floor,
            named.join(", "),
            if count == 1 { "it" } else { "them" }
        ))
    }
}

/// What the windows left below the floor (start, length, VMAF) come to.
#[derive(Debug, PartialEq)]
struct Remaining {
    /// Windows within [`NEAR`] of the floor that lie in no stretch going lower.
    near: usize,
    /// The stretches that go lower than that.
    still_low: Vec<LowStretch>,
}

impl Remaining {
    fn of(still: &[(f64, f64, f64)], floor: f64) -> Remaining {
        let mut by_time = still.to_vec();
        by_time.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut stretches: Vec<(LowStretch, usize)> = Vec::new();
        for (start, secs, vmaf) in by_time {
            match stretches.last_mut() {
                Some((s, n)) if start <= s.start_secs + s.secs + 1e-9 => {
                    s.secs = s.secs.max(start + secs - s.start_secs);
                    s.vmaf = s.vmaf.min(vmaf);
                    *n += 1;
                }
                _ => stretches.push((LowStretch { start_secs: start, secs, vmaf }, 1)),
            }
        }
        let is_far = |s: &LowStretch| s.vmaf < floor - NEAR;
        let near = stretches.iter().filter(|(s, _)| !is_far(s)).map(|(_, n)| n).sum();
        Remaining { near, still_low: stretches.into_iter().map(|(s, _)| s).filter(is_far).collect() }
    }

    /// Whether the stretches far below the floor take up so much of a video `length` seconds
    /// long that it is better searched and encoded again whole (see [`MOST_LEFT_LOW`]).
    fn too_much(&self, length: f64) -> bool {
        self.still_low.iter().map(|s| s.secs).sum::<f64>() > MOST_LEFT_LOW * length
    }
}

/// Whether, after [`ROUNDS`], the windows `low` below the floor get another round (only for
/// those further below it than [`NEAR`]): while there are such windows and they are few, as
/// more than that goes to a second search anyway. Windows over a `held` stretch count for
/// nothing.
fn more_rounds_for(low: &[Window], held: &[Run], secs: &[f64], floor: f64) -> bool {
    let still: Vec<(f64, f64, f64)> = low.iter().filter(|w| !held.iter().any(|s| s.overlaps(w.start, w.end))).map(|w| (secs[w.start], secs[w.end] - secs[w.start], w.vmaf)).collect();
    let rest = Remaining::of(&still, floor);
    !rest.still_low.is_empty() && !rest.too_much(secs[secs.len() - 1])
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// The lowest of the whole video's windows that lie over `run`, with `new` in place of the
/// run's own frames in `scores`. A stretch is judged as it will sit in the video: a window
/// across its edge holds its neighbour's frames too, and can fall short where every window
/// inside the stretch holds. What a stretch answers for is its own part of such a window:
/// where that part scores more than the window does, the part's score is taken, so a
/// stretch is not sent after bits to make up for the frames next to it (those are put down
/// to the neighbour, see [`blame`]).
fn lowest_over(secs: &[f64], scores: &[(usize, f64)], new: Option<&[(usize, f64)]>, run: Run) -> f64 {
    let merged: Vec<(usize, f64)>;
    let scores = match new {
        Some(new) => {
            merged = with_run(scores, new, run);
            &merged
        }
        None => scores,
    };
    let scores = Scores::new(scores);
    let over: Vec<Window> = windows(secs, &scores, 0, secs.len() - 1).into_iter().filter(|w| run.overlaps(w.start, w.end)).collect();
    // A window that takes in less of the stretch than lies between two scored frames says
    // nothing of it: what it scores is its neighbour's.
    let own = over
        .iter()
        .filter_map(|w| scores.mean(w.start.max(run.start), w.end.min(run.end)).map(|own| own.max(w.vmaf)))
        .fold(f64::INFINITY, f64::min);
    if own.is_finite() { own } else { over.iter().map(|w| w.vmaf).fold(f64::INFINITY, f64::min) }
}

pub struct Outcome {
    pub summary: Summary,
    /// The video with the stretches replaced, as IVF, where any were.
    pub video: Option<PathBuf>,
    /// Frame scores of the video as it is kept, and when each frame is shown (seconds from
    /// the first; one more entry than frames).
    scores: Vec<(usize, f64)>,
    secs: Vec<f64>,
    /// What the stretches encoded again say otherwise than the video in their sequence
    /// headers, of what they may (see [`sequence_header::joinable`]).
    pub differences: Vec<sequence_header::Difference>,
}

impl Outcome {
    /// Mean VMAF of the `secs` seconds starting `start` seconds after the first frame.
    /// How long the video is, in seconds from its first frame to the end of its last.
    pub fn length(&self) -> f64 {
        self.secs.last().copied().unwrap_or(0.0)
    }

    pub fn at(&self, start: f64, secs: f64) -> Option<f64> {
        let n = self.secs.len() - 1;
        let frame = |t: f64| self.secs[..n].partition_point(|&s| s < t - 1e-9);
        Scores::new(&self.scores).mean(frame(start), frame(start + secs))
    }
}

/// Windows that stay below the floor although more bits would have helped, the tries having
/// run out, over too much of the video to keep it so (see [`MOST_LEFT_LOW`]).
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
    /// What the mean of the whole video has to reach.
    pub target: f64,
    pub min_crf: u8,
    pub threads: usize,
    pub dir: &'a Path,
    /// Sections removed from the output: the stretches are cut from its picture (see [`survey`]).
    pub cut: Option<&'a Sections>,
}

/// What repairs at the floor would make of a video, imitated on its scores.
#[derive(Debug, Clone, PartialEq)]
struct Imitated {
    scores: Vec<(usize, f64)>,
    /// What is left of the GOPs: repaired runs count as one.
    units: Vec<usize>,
    repaired: Vec<Run>,
    /// Seconds encoded again.
    secs: f64,
}

/// Imitates the rounds of [`repair`] at `floor` on the frame scores: the same windows, the same
/// blame, the same runs. A run encoded again is taken to rise evenly toward 100
/// (`v + k * (100 - v)`, which is how real stretches rose) until its lowest window, judged as
/// [`lowest_over`] judges it, is [`IMITATED_LANDING`] above the floor.
fn imitate(secs: &[f64], scores: &[(usize, f64)], gops: &[usize], floor: f64, max_secs: f64) -> Imitated {
    let frames = secs.len() - 1;
    let mut scores = scores.to_vec();
    let mut units = gops.to_vec();
    let mut repaired: Vec<Run> = Vec::new();
    // Where the windows lie does not depend on what the frames score.
    let all = windows(secs, &Scores::new(&scores), 0, frames);
    for _ in 0..ROUNDS {
        let sc = Scores::new(&scores);
        let low: Vec<Window> = all.iter().filter_map(|w| sc.mean(w.start, w.end).filter(|v| *v < floor).map(|vmaf| Window { vmaf, ..*w })).collect();
        let todo = runs(&blame(&low, &units, frames, &sc, floor, &[]), &units, frames, secs, max_secs);
        if todo.is_empty() {
            break;
        }
        for run in todo {
            if lift_run(&mut scores, &all, run, floor) {
                units.retain(|&u| u <= run.start || u >= run.end);
                repaired.retain(|r| !r.overlaps(run.start, run.end));
                repaired.push(run);
            }
        }
    }
    repaired.sort_by_key(|r| r.start);
    let seconds = repaired.iter().map(|r| secs[r.end] - secs[r.start]).sum();
    Imitated { scores, units, repaired, secs: seconds }
}

/// Raises the scores of `run` as an encode for `floor` would. False where it stands at the
/// floor already.
fn lift_run(scores: &mut [(usize, f64)], all: &[Window], run: Run, floor: f64) -> bool {
    let sc = Scores::new(scores);
    let index = |f: usize| sc.at.partition_point(|&x| x < f);
    let (first, last) = (index(run.start), index(run.end));
    // For each window over the run: its score and its own part's, as sums that the lift adds
    // to in proportion. Windows with no scored frame of the run say nothing of it.
    struct Over {
        count: f64,
        sum: f64,
        own_count: f64,
        own_sum: f64,
    }
    let over: Vec<Over> = all
        .iter()
        .filter(|w| run.overlaps(w.start, w.end))
        .filter_map(|w| {
            let (a, b) = (index(w.start), index(w.end));
            let (i, j) = (a.max(first), b.min(last));
            (j > i && b > a).then(|| Over { count: (b - a) as f64, sum: sc.cum[b] - sc.cum[a], own_count: (j - i) as f64, own_sum: sc.cum[j] - sc.cum[i] })
        })
        .collect();
    let lowest = |k: f64| {
        over.iter()
            .map(|w| {
                let gain = k * (100.0 * w.own_count - w.own_sum);
                ((w.own_sum + gain) / w.own_count).max((w.sum + gain) / w.count)
            })
            .fold(f64::INFINITY, f64::min)
    };
    if over.is_empty() || lowest(0.0) >= floor {
        return false;
    }
    let aim = floor + IMITATED_LANDING;
    let (mut below, mut above) = (0.0, 1.0);
    for _ in 0..40 {
        let k = (below + above) / 2.0;
        if lowest(k) < aim {
            below = k;
        } else {
            above = k;
        }
    }
    for s in &mut scores[first..last] {
        s.1 += above * (100.0 - s.1);
    }
    true
}

/// The GOPs of `units` nothing has been done to, that can be encoded again by themselves.
fn spare_gops(units: &[usize], frames: usize, taken: &[Run], secs: &[f64], longest: f64) -> Vec<Run> {
    let end_of = |i: usize| units.get(i + 1).copied().unwrap_or(frames);
    (0..units.len())
        .map(|i| Run { start: units[i], end: end_of(i) })
        .filter(|r| !taken.iter().any(|t| t.overlaps(r.start, r.end)) && secs[r.end] - secs[r.start] <= longest)
        .collect()
}

/// Which GOPs to lift for the mean, and how far.
#[derive(Debug, Clone, PartialEq)]
struct Lifts {
    /// What each of them is brought to.
    level: f64,
    /// Lowest first.
    runs: Vec<Run>,
    /// Their length in seconds.
    secs: f64,
    /// Whether that brings the mean to what was asked. If not, it is as near as `cap` and
    /// `budget` allow.
    reaches: bool,
}

/// The lowest of the `spare` GOPs, lifted to one common level, just far enough for the mean of
/// `scores` to come to `goal`: each GOP gets what it takes to stand level with the next, so none
/// is pushed further than the ones after it would cost. The level stays within `cap`, the GOPs
/// within `budget` seconds. Pushing stretches already repaired further cost 13 to 21% of the
/// file per point of mean, 1.2 to 1.6 times what the CRF would; these GOPs are cheaper.
fn plan_lifts(scores: &[(usize, f64)], spare: &[Run], secs: &[f64], goal: f64, cap: f64, budget: f64) -> Option<Lifts> {
    let sc = Scores::new(scores);
    let total: f64 = sc.cum[scores.len()];
    let missing = goal * scores.len() as f64 - total;
    if missing <= 0.0 {
        return None;
    }
    let index = |f: usize| sc.at.partition_point(|&x| x < f);
    // (mean, scored frames, run), lowest first.
    let mut gops: Vec<(f64, f64, Run)> = spare
        .iter()
        .filter_map(|&r| {
            let (i, j) = (index(r.start), index(r.end));
            (j > i).then(|| ((sc.cum[j] - sc.cum[i]) / (j - i) as f64, (j - i) as f64, r))
        })
        .filter(|g| g.0 <= cap - LEAST_LIFT)
        .collect();
    gops.sort_by(|a, b| a.0.total_cmp(&b.0));
    let (mut count, mut sum, mut seconds) = (0.0, 0.0, 0.0);
    let mut chosen = Vec::new();
    let mut level = f64::INFINITY;
    for (i, &(mean, n, run)) in gops.iter().enumerate() {
        let length = secs[run.end] - secs[run.start];
        if seconds + length > budget {
            break;
        }
        seconds += length;
        count += n;
        sum += n * mean;
        chosen.push(run);
        level = (missing + sum) / count;
        if level <= cap && gops.get(i + 1).is_none_or(|next| level <= next.0 + LEAST_LIFT) {
            break;
        }
    }
    let lowest = gops.first()?.0;
    if chosen.is_empty() {
        return None;
    }
    let reaches = level <= cap;
    Some(Lifts { level: level.min(cap).max(lowest + LEAST_LIFT), runs: chosen, secs: seconds, reaches })
}

/// What the survey says of the mean the video will end with.
#[derive(Debug, Clone, PartialEq)]
pub struct MeanPlan {
    /// The mean after the repairs below the floor, which are done in any case.
    pub mean: f64,
    /// Seconds those repairs encode again.
    pub secs: f64,
    /// What it would take to bring that mean to the target, where it falls short.
    top_up: Option<Lifts>,
    /// Length of the video in seconds.
    pub length: f64,
}

impl MeanPlan {
    /// Seconds of GOPs that would be encoded again for the mean.
    pub fn top_up_secs(&self) -> f64 {
        self.top_up.as_ref().map_or(0.0, |t| t.secs)
    }
}

/// Foretells, from the survey, the mean after the repairs at `floor` and what would make up the
/// difference to `target`.
pub fn plan_mean(survey: &Survey, info: &MediaInfo, floor: f64, target: f64) -> MeanPlan {
    plan_from(&timeline(&survey.times), &survey.scores, &survey.gops, floor, target, scenes::Limits::for_video(info).max_secs)
}

fn plan_from(secs: &[f64], scores: &[(usize, f64)], gops: &[usize], floor: f64, target: f64, max_secs: f64) -> MeanPlan {
    let length = secs[secs.len() - 1];
    // A frame far below those around it has its GOP encoded again whatever its windows come
    // to, and is taken to land among them.
    let mut lifted = scores.to_vec();
    for (i, median) in far_below(secs, scores, 0, secs.len() - 1) {
        lifted[i].1 = median;
    }
    let done = imitate(secs, &lifted, gops, floor, max_secs);
    let mean = done.scores.iter().map(|s| s.1).sum::<f64>() / done.scores.len().max(1) as f64;
    let top_up = (mean < target).then(|| {
        let spare = spare_gops(&done.units, secs.len() - 1, &done.repaired, secs, MAX_GOP_SECS.max(max_secs));
        plan_lifts(&done.scores, &spare, secs, target + MEAN_EXTRA, target, MOST_TOPPED_UP * length)
    });
    MeanPlan { mean, secs: done.secs, top_up: top_up.flatten(), length }
}

/// What a further encode of the whole video would go by.
#[derive(Debug, Clone, Copy)]
pub struct MeanRules {
    pub target: f64,
    /// VMAF the mean loses per CRF step around the CRF of the encode.
    pub slope: f64,
    pub crf: f64,
    pub min_crf: f64,
    pub max_crf: f64,
    /// False once the video has been encoded a second time: it is not done a third.
    pub may_encode_again: bool,
}

/// The CRF to encode the whole video at once more, if the plan calls for that: its mean would
/// end more than [`MOST_OVER`] above the target, or below the target by more than encoding
/// [`MOST_TOPPED_UP`] of the video again could make up. A CRF that comes out the same as the
/// encode's is no reason for an encode. Nor is a high mean with much of the video being repaired:
/// what those stretches end at does not follow the video's CRF (a generated 24 s clip, 20 s of
/// it repaired, was foretold 94.33 and after three CRF steps up 94.22).
pub fn encode_again_at(plan: &MeanPlan, rules: &MeanRules) -> Option<f64> {
    let over = plan.mean - rules.target;
    let moved = search::nearest_quarter(rules.crf + over / rules.slope).clamp(rules.min_crf, rules.max_crf);
    let called_for = if over > 0.0 { over > MOST_OVER && plan.secs <= MOST_TOPPED_UP * plan.length } else { over < 0.0 && !plan.top_up.as_ref().is_some_and(|t| t.reaches) };
    (called_for && rules.may_encode_again && moved != rules.crf).then_some(moved)
}

/// Brings the mean of `scores` to `target` by lifting the lowest `spare` GOPs, one at a time and
/// lowest first. `lift` encodes a GOP for a level and gives its new scores, `None` where nothing
/// came of that. The plan is made afresh after each GOP from what it really scored, so that one
/// landing off is made up by the next, and the last is as small as what is still missing. GOPs
/// may take up [`MOST_TOPPED_UP`] of the video. If they run out with the mean more than
/// [`MEAN_SLACK`] short, one further round may lift them [`LEVEL_RAISE`] higher. Gives the
/// highest level a GOP was lifted to, if any was.
fn top_up(
    secs: &[f64],
    scores: &mut Vec<(usize, f64)>,
    mut spare: Vec<Run>,
    target: f64,
    mut lift: impl FnMut(Run, f64, &[(usize, f64)]) -> Result<Option<Vec<(usize, f64)>>>,
) -> Result<Option<f64>> {
    let mean = |s: &[(usize, f64)]| s.iter().map(|x| x.1).sum::<f64>() / s.len().max(1) as f64;
    let (mut left, mut cap) = (MOST_TOPPED_UP * secs[secs.len() - 1], target);
    let (mut highest, mut raised): (Option<f64>, bool) = (None, false);
    while mean(scores) < target {
        let Some(plan) = plan_lifts(scores, &spare, secs, target + MEAN_EXTRA, cap, left) else {
            if raised || mean(scores) >= target - MEAN_SLACK {
                break;
            }
            raised = true;
            cap += LEVEL_RAISE;
            continue;
        };
        let run = plan.runs[0];
        spare.retain(|r| *r != run);
        left -= secs[run.end] - secs[run.start];
        let Some(new) = lift(run, plan.level, scores)? else { continue };
        scores.retain(|s| s.0 < run.start || s.0 >= run.end);
        scores.extend(new);
        scores.sort_by_key(|s| s.0);
        highest = Some(highest.map_or(plan.level, |h: f64| h.max(plan.level)));
    }
    Ok(highest)
}

/// Room for encoding stretches again, beside the encode of `output_bytes` that is already
/// there: first its video stream alone and that stream with the stretches replaced, then the
/// latter and the file joined from it, each a little over the encode at worst; and a lossless
/// stretch of the source with its tries, and slack.
fn room_to_repair(output_bytes: u64) -> u64 {
    (output_bytes as f64 * 2.2) as u64 + 1_000_000_000 + 500_000_000
}

/// Looks for stretches of `output` below the floor and encodes them again. A stretch no CRF
/// brings up is kept at the best it was given, and so is one the tries ran out on, which the
/// report names; fails with [`StillLow`] where those come to too much of the video. Where the mean of the video is then below
/// the target, the lowest GOPs not yet touched are encoded again as well, each as far as it
/// takes (see [`top_up`]); what was repaired stays as it landed.
pub fn repair(c: &Ctx, output: &Path, survey: &Survey) -> Result<Outcome> {
    let frames = survey.times.pts.len();
    let secs = timeline(&survey.times);
    let mut scores = survey.scores.clone();
    let lowest_of = |ws: &[Window]| ws.iter().map(|w| w.vmaf).fold(f64::INFINITY, f64::min);
    let all = windows(&secs, &Scores::new(&scores), 0, frames);
    let lowest_before = lowest_of(&all);
    let mean = |scores: &[(usize, f64)]| scores.iter().map(|s| s.1).sum::<f64>() / scores.len().max(1) as f64;
    let mean_before = mean(&scores);
    let low_count = all.iter().filter(|w| w.vmaf < c.floor).count();
    eprintln!(
        "whole video: mean VMAF {mean_before:.2}, lowest {WINDOW_SECS:.0} s {lowest_before:.2}{}",
        if low_count > 0 { format!(", {low_count} of {} stretches below {:.0}", all.len(), c.floor) } else { String::new() }
    );
    // The frames far below those around them, by the GOP they are in.
    let gop_of = |frame: usize| survey.gops.partition_point(|&g| g <= frame).saturating_sub(1);
    let mut far: Vec<(usize, FarBelow)> = Vec::new();
    for (i, median) in far_below(&secs, &scores, 0, frames) {
        let (frame, vmaf) = scores[i];
        match far.last_mut() {
            Some((gop, f)) if *gop == gop_of(frame) => {
                f.frames += 1;
                if vmaf < f.vmaf {
                    (f.vmaf, f.around) = (vmaf, median);
                }
            }
            _ => far.push((gop_of(frame), FarBelow { start_secs: secs[frame], frames: 1, vmaf, around: median, still: 0 })),
        }
    }
    for (_, f) in &far {
        eprintln!(
            "{} frame{} at {} scored far below the frames around {} (VMAF {:.1}{} among frames at {:.1}), which is not how a hard scene looks: that stretch is encoded again",
            f.frames,
            if f.frames == 1 { "" } else { "s" },
            hms(f.start_secs),
            if f.frames == 1 { "it" } else { "them" },
            f.vmaf,
            if f.frames == 1 { "" } else { " at the lowest" },
            f.around
        );
    }
    if low_count == 0 && mean_before >= c.target && far.is_empty() {
        let summary = Summary { mean_vmaf: mean_before, lowest_vmaf: lowest_before, lowest_before, floor: c.floor, stretches: Vec::new(), near: 0, left: Vec::new(), still_low: Vec::new(), far_below: Vec::new(), mean_before, top_up: None, secs: 0.0 };
        return Ok(Outcome { summary, video: None, scores, secs, differences: Vec::new() });
    }

    let room = room_to_repair(std::fs::metadata(output).context("the encode is gone")?.len());
    crate::encode::require_room(c.dir, crate::encode::free_bytes(c.dir), room)?;

    let plain = c.dir.join("repair.source.ivf");
    let args: Vec<String> = ["-y", "-v", "error", "-i", &path_arg(output), "-map", "0:v:0", "-c", "copy", "-f", "ivf", &path_arg(&plain)].map(String::from).to_vec();
    c.tools.run(&args)?;
    let (head, index) = ivf_index(&plain)?;
    if index.len() != frames {
        bail!("reading the encode frame by frame gave {} frames where it has {frames}; {WENT_WRONG_INSIDE}", index.len());
    }
    if index.windows(2).any(|w| w[1].pts <= w[0].pts) {
        bail!("the frames of the encode did not come back in the order they are shown; {WENT_WRONG_INSIDE}");
    }

    let max_secs = scenes::Limits::for_video(c.info).max_secs;
    let mut units = survey.gops.clone();
    let mut fixed: Vec<Fixed> = Vec::new();
    // Stretches no CRF helped, and replacements, each with the number of the try it was.
    let mut settled: Vec<(Run, usize)> = Vec::new();
    let mut replaced: Vec<(Run, usize)> = Vec::new();
    // The stretches among them that stay as the one pass made them.
    let mut left: Vec<Run> = Vec::new();
    let mut serial = 0;
    for round in 0..ROUNDS + MORE_ROUNDS {
        // Whether there are more rounds is decided on the video as the usual ones left it.
        if round >= ROUNDS {
            let held: Vec<Run> = settled.iter().map(|s| s.0).collect();
            let low: Vec<Window> = windows(&secs, &Scores::new(&scores), 0, frames).into_iter().filter(|w| w.vmaf < c.floor).collect();
            if !more_rounds_for(&low, &held, &secs, c.floor) {
                break;
            }
        }
        // A stretch given up beside frames that have since been replaced is looked at
        // afresh: what it was judged among is no longer there.
        let stale = |s: &(Run, usize)| replaced.iter().any(|r| r.1 > s.1 && (r.0.end == s.0.start || r.0.start == s.0.end));
        for (run, _) in settled.iter().filter(|s| stale(s)) {
            left.retain(|l| l != run);
            fixed.iter_mut().filter(|f| f.run == *run).for_each(|f| f.out_of_reach = false);
        }
        settled.retain(|s| !stale(s));
        let held: Vec<Run> = settled.iter().map(|s| s.0).collect();
        let sc = Scores::new(&scores);
        let mut low: Vec<Window> = windows(&secs, &sc, 0, frames).into_iter().filter(|w| w.vmaf < c.floor).collect();
        if round >= ROUNDS {
            low.retain(|w| w.vmaf < c.floor - NEAR);
        }
        let mut blamed = blame(&low, &units, frames, &sc, c.floor, &held);
        // A frame far below those around it marks its unit as a low window does, in the usual
        // rounds: one that is still so after those is how the picture is.
        let mut for_frames = false;
        if round < ROUNDS {
            for (i, _) in far_below(&secs, &scores, 0, frames) {
                let u = units.partition_point(|&s| s <= scores[i].0).saturating_sub(1);
                if !blamed[u] && !held.iter().any(|s| s.overlaps(units[u], units.get(u + 1).copied().unwrap_or(frames))) {
                    (blamed[u], for_frames) = (true, true);
                }
            }
        }
        let todo = runs(&blamed, &units, frames, &secs, max_secs);
        if todo.is_empty() {
            break;
        }
        // (By its frames: a hole in the source's timestamps makes a stretch long on the
        // clock without making it any more to encode.)
        let most = MAX_GOP_SECS.max(max_secs) * c.info.fps.max(1.0);
        if let Some(r) = todo.iter().find(|r| (r.end - r.start) as f64 > most) {
            bail!("{} frames in a row have no key frame to cut at, too many to encode again on their own; a smaller keyint in --svt-params gives the video more key frames", r.end - r.start);
        }
        let total: f64 = todo.iter().map(|r| secs[r.end] - secs[r.start]).sum();
        let detail = format!(
            "{} stretch{} ({}) below VMAF {:.0}{}{}",
            todo.len(),
            if todo.len() == 1 { "" } else { "es" },
            hms(total),
            c.floor,
            if for_frames { " or with frames far below those around them" } else { "" },
            if round > 0 { ", again" } else { "" }
        );
        eprintln!("encoding again: {detail}");
        c.tools.emit(Event::Phase { phase: "repairing".into(), detail });
        for (k, run) in todo.iter().enumerate() {
            c.tools.emit(Event::Progress { label: "repairing".into(), frac: k as f64 / todo.len() as f64, fps: 0.0, eta_secs: 0.0 });
            // What stands there now: the one-pass encode, or an earlier round's.
            let earlier: Vec<&Fixed> = fixed.iter().filter(|f| f.run.overlaps(run.start, run.end)).collect();
            let from_crf = earlier.iter().map(|f| f.crf).fold(c.crf, f64::min);
            let now = lowest_over(&secs, &scores, None, *run);
            let wrecked = round < ROUNDS && far_below_in(&secs, &scores, None, *run) > 0;
            if now >= c.floor && !wrecked {
                continue;
            }
            let before = earlier.iter().map(|f| f.before).fold(now, f64::min);
            // A stretch as the one pass made it is first encoded as it was: frames wrecked by
            // a fault come out whole at the same CRF, for no more bytes.
            let goal = Goal { wrecked, same_first: wrecked && earlier.is_empty(), ..Goal::holding(c.floor) };
            let for_frames = wrecked && now >= c.floor && earlier.iter().all(|e| e.for_frames);
            let f = fix(c, goal, &survey.times, &secs, &scores, *run, serial, from_crf, now, before)?;
            serial += 1;
            // Nothing lower is allowed, or nothing lower helps: what is there stays.
            let Some(mut f) = f else {
                settled.push((*run, serial));
                match fixed.iter_mut().find(|f| f.run == *run) {
                    Some(f) => f.out_of_reach = true,
                    None => left.push(*run),
                }
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
                settled.push((*run, serial));
            }
            f.for_frames = for_frames;
            replaced.push((*run, serial));
            fixed.push(f);
        }
    }
    let settled: Vec<Run> = settled.into_iter().map(|s| s.0).collect();
    let short_of_floor = |after: &[Window]| -> Vec<(f64, f64, f64)> {
        after.iter().filter(|w| w.vmaf < c.floor && !settled.iter().any(|s| s.overlaps(w.start, w.end))).map(|w| (secs[w.start], secs[w.end] - secs[w.start], w.vmaf)).collect()
    };
    let still = short_of_floor(&windows(&secs, &Scores::new(&scores), 0, frames));
    // A few stretches the tries ran out on keep what was made for them, which is nearer the
    // floor than what the one pass gave them: throwing it all away for a second search lowers
    // the CRF of the whole video for their sake.
    if Remaining::of(&still, c.floor).too_much(secs[frames]) {
        // What was made here is of no use to the search and the encode that follow.
        let _ = std::fs::remove_file(&plain);
        for f in &fixed {
            let _ = std::fs::remove_file(&f.file);
        }
        return Err(StillLow { windows: still }.into());
    }

    // The floor holds, but for what was just let be. What the mean still lacks comes from
    // GOPs nothing was done to yet.
    let mean_from = mean(&scores);
    let mut level = None;
    if mean_from < c.target {
        let taken: Vec<Run> = fixed.iter().map(|f| f.run).chain(settled.iter().copied()).chain(left.iter().copied()).collect();
        let spare = spare_gops(&units, frames, &taken, &secs, MAX_GOP_SECS.max(max_secs));
        let detail = format!("the lowest GOPs, for a mean of VMAF {} (now {mean_from:.2})", c.target);
        eprintln!("encoding again: {detail}");
        c.tools.emit(Event::Phase { phase: "repairing".into(), detail });
        level = top_up(&secs, &mut scores, spare, c.target, |run, level, now| {
            let Some(stands) = Scores::new(now).mean(run.start, run.end) else { return Ok(None) };
            let before = lowest_over(&secs, now, None, run);
            let f = fix(c, Goal::mean(level), &survey.times, &secs, now, run, serial, c.crf, stands, before)?;
            serial += 1;
            Ok(f.map(|mut f| {
                f.for_mean = true;
                let new = std::mem::take(&mut f.scores);
                fixed.push(f);
                new
            }))
        })?;
    }

    fixed.sort_by_key(|f| f.run.start);
    let after = windows(&secs, &Scores::new(&scores), 0, frames);
    // What the video's own windows say of a stretch in the end: one given up on can sit
    // among windows that hold, once its neighbours were encoded again.
    let lowest_at = |run: &Run| after.iter().filter(|w| run.overlaps(w.start, w.end)).map(|w| w.vmaf).fold(f64::INFINITY, f64::min);
    let left: Vec<f64> = left.iter().map(lowest_at).filter(|&v| v < c.floor).collect();
    let Remaining { near, still_low } = Remaining::of(&short_of_floor(&after), c.floor);
    let far_below: Vec<FarBelow> = far
        .into_iter()
        .map(|(gop, f)| {
            let run = Run { start: survey.gops[gop], end: survey.gops.get(gop + 1).copied().unwrap_or(frames) };
            FarBelow { still: far_below_in(&secs, &scores, None, run), ..f }
        })
        .collect();
    // Nothing was put in place of anything: the encode stays as it is.
    if fixed.is_empty() {
        let _ = std::fs::remove_file(&plain);
        let summary = Summary { mean_vmaf: mean(&scores), lowest_vmaf: lowest_before, lowest_before, floor: c.floor, stretches: Vec::new(), near: 0, left, still_low, far_below, mean_before, top_up: None, secs: 0.0 };
        return Ok(Outcome { summary, video: None, scores, secs, differences: Vec::new() });
    }

    let video = c.dir.join("repaired.ivf");
    let differences = splice(&plain, &head, &index, &fixed, &video)?;
    let _ = std::fs::remove_file(&plain);
    let stretches: Vec<Stretch> = fixed
        .iter()
        .map(|f| Stretch {
            start_secs: secs[f.run.start],
            secs: secs[f.run.end] - secs[f.run.start],
            frames: f.run.end - f.run.start,
            crf: f.crf,
            vmaf_before: all.iter().filter(|w| f.run.overlaps(w.start, w.end)).map(|w| w.vmaf).fold(f.before, f64::min),
            vmaf: lowest_at(&f.run),
            bytes_before: index[f.run.start..f.run.end].iter().map(|x| x.size as u64).sum(),
            bytes: f.bytes,
            out_of_reach: f.out_of_reach && lowest_at(&f.run) < c.floor,
            for_mean: f.for_mean,
            for_frames: f.for_frames,
        })
        .collect();
    for f in &fixed {
        let _ = std::fs::remove_file(&f.file);
    }
    let top_up = level.map(|level| {
        let lifted = || stretches.iter().filter(|s| s.for_mean);
        TopUp {
            mean_from,
            level,
            stretches: lifted().count(),
            secs: lifted().map(|s| s.secs).sum(),
            bytes: lifted().map(|s| s.bytes as i64 - s.bytes_before as i64).sum(),
        }
    });
    let summary = Summary { mean_vmaf: mean(&scores), lowest_vmaf: lowest_of(&after), lowest_before, floor: c.floor, stretches, near, left, still_low, far_below, mean_before, top_up, secs: 0.0 };
    Ok(Outcome { summary, video: Some(video), scores, secs, differences })
}

/// Encodes the frames of `run` again, at the highest CRF found that meets `goal`: the lowest
/// window over it holding the floor (see [`lowest_over`]), or its own mean reaching a level.
/// `all` holds the scores of the video as it stands. `from_crf` and `now` are the CRF the
/// stretch stands at and its score there by that goal; `before`, its lowest window in the
/// one-pass encode. `None` where it stands at the lowest CRF allowed already, or nothing tried
/// was better than what is there.
#[allow(clippy::too_many_arguments)]
fn fix(c: &Ctx, goal: Goal, times: &Times, secs: &[f64], all: &[(usize, f64)], run: Run, serial: usize, from_crf: f64, now: f64, before: f64) -> Result<Option<Fixed>> {
    struct Try {
        crf: f64,
        score: f64,
        /// It leaves the windows over the stretch no worse than the floor allows.
        safe: bool,
        file: PathBuf,
        scores: Vec<(usize, f64)>,
    }
    let at = hms(secs[run.start]);
    let lo = lowest_try(c.crf, c.min_crf);
    let counted = |lowest: f64, far: usize| counted_with(lowest, far, goal.line);
    let stood = now;
    let now = if goal.wrecked { counted(now, 1) } else { now };
    // Nothing to try (it stands at the lowest CRF already): nothing to cut from the source.
    if !goal.same_first && next_try(&[(from_crf, now)], goal, lo).0.is_none() {
        return Ok(None);
    }
    let reference = c.dir.join(format!("repair{serial:04}.ref.mkv"));
    // The frames between two of the source's own timestamps, as scene mode cuts a source
    // whose frames keep their times: the encode holds every source frame once.
    let b = Bounds { start: (run.start > 0).then(|| times.pts[run.start]), end: times.pts.get(run.end).copied() };
    let got = scenes::extract_chunk(c.tools, c.input, c.info, b, Some(times.tb), c.cut, &reference)? as usize;
    if got != run.end - run.start {
        bail!("the stretch at {at} holds {got} frames cut from the source, {} in the encode", run.end - run.start);
    }
    let (model, pix) = (search::vmaf_model(c.info), search::reference_pix_fmt(c.info));
    // A stretch encoded again for the mean must not leave a window lower than the floor allows.
    let lowest_now = lowest_over(secs, all, None, run);
    let measure = if goal.by_mean { "as its mean".to_string() } else { format!("in its lowest {WINDOW_SECS:.0} s") };
    let mut tries: Vec<Try> = Vec::new();
    let mut known = vec![(from_crf, now)];
    let out_of_reach = loop {
        let (next, out_of_reach) = if goal.same_first && tries.is_empty() { (Some(from_crf), false) } else { next_try(&known, goal, lo) };
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
        let lowest = lowest_over(secs, all, Some(&scores), run);
        if !lowest.is_finite() {
            bail!("the stretch at {at} could not be scored");
        }
        // Frames far below those around them in a new encode are no better than in the old.
        let far = far_below_in(secs, all, Some(&scores), run);
        let shown = if goal.by_mean { scores.iter().map(|s| s.1).sum::<f64>() / scores.len().max(1) as f64 } else { lowest };
        let score = if goal.by_mean { shown } else { counted(lowest, far) };
        let safe = !goal.by_mean || (lowest >= c.floor.min(lowest_now) && far == 0);
        let frames = match far {
            _ if !goal.wrecked && (goal.by_mean || far == 0) => String::new(),
            0 => ", no frame far below those around it".to_string(),
            1 => ", 1 frame far below those around it".to_string(),
            n => format!(", {n} frames far below those around them"),
        };
        eprintln!("  {at} ({:.1} s): CRF {crf} gives VMAF {shown:.2} {measure}{frames} (CRF {from_crf}: {stood:.2})", secs[run.end] - secs[run.start]);
        known.push((crf, score));
        tries.push(Try { crf, score, safe, file, scores });
    };
    let _ = std::fs::remove_file(&reference);
    let holds = |t: &Try| t.safe && t.score >= goal.holds_from;
    let seen: Vec<(f64, f64, bool)> = tries.iter().map(|t| (t.crf, t.score, t.safe)).collect();
    let Some(pick) = try_to_keep(&seen, goal, now, out_of_reach) else {
        for t in &tries {
            let _ = std::fs::remove_file(&t.file);
        }
        return Ok(None);
    };
    let kept = tries.swap_remove(pick);
    for t in &tries {
        let _ = std::fs::remove_file(&t.file);
    }
    let bytes = ivf_index(&kept.file)?.1.iter().map(|f| f.size as u64).sum();
    let out_of_reach = out_of_reach && !holds(&kept);
    Ok(Some(Fixed { run, crf: kept.crf, file: kept.file, scores: kept.scores, before, bytes, out_of_reach, for_mean: false, for_frames: false }))
}

/// What the lowest window over a stretch counts as when `far` of its scored frames lie far
/// below those around them: short of `floor` by [`FAR_BELOW_COUNTS`] at least.
fn counted_with(lowest: f64, far: usize, floor: f64) -> f64 {
    if far > 0 { lowest.min(floor - FAR_BELOW_COUNTS) } else { lowest }
}

/// The lowest CRF a stretch of a video encoded at `crf` is tried at.
fn lowest_try(crf: f64, min_crf: u8) -> f64 {
    (crf - CRF_SPREAD).max(min_crf as f64)
}

/// Which of the encodes of a stretch to keep, by its place in `tries`: (CRF, score, and whether
/// it leaves the windows over the stretch no worse than the floor allows) of each. `now` is the
/// score of what stands there; `out_of_reach`, that the tries found the goal to be.
///
/// The smallest encode that meets the goal. Failing that, the best one, for the next round to go
/// on from; or, where the goal is out of reach, the smallest within half a point of the best,
/// which can be what stands there already: bits past it buy nothing.
fn try_to_keep(tries: &[(f64, f64, bool)], goal: Goal, now: f64, out_of_reach: bool) -> Option<usize> {
    let holds = |&(_, score, safe): &(f64, f64, bool)| safe && score >= goal.holds_from;
    let best = tries.iter().filter(|t| t.2).map(|t| t.1).fold(f64::NEG_INFINITY, f64::max);
    let by_crf = |a: &usize, b: &usize| tries[*a].0.total_cmp(&tries[*b].0);
    (0..tries.len()).filter(|&i| holds(&tries[i])).max_by(by_crf).or_else(|| {
        if out_of_reach {
            let stays = now >= best - LEVEL_OFF;
            (0..tries.len()).filter(|&i| !stays && tries[i].2 && tries[i].1 >= best - LEVEL_OFF).max_by(by_crf)
        } else {
            // (For the mean, only what adds to it is worth its bytes.)
            (0..tries.len()).filter(|&i| tries[i].2 && (!goal.by_mean || tries[i].1 > now)).max_by(|&a, &b| tries[a].1.total_cmp(&tries[b].1))
        }
    })
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
    r.read_exact(&mut head).with_context(|| format!("a video stream av1opt made could not be read back frame by frame; {WENT_WRONG_INSIDE}"))?;
    if &head[..4] != b"DKIF" || u16::from_le_bytes([head[6], head[7]]) != 32 {
        bail!("a video stream av1opt made could not be read back frame by frame; {WENT_WRONG_INSIDE}");
    }
    let mut frames = Vec::new();
    let mut offset = 32u64;
    // A read that fails says which file, and where in it: the bare error says neither.
    let unread = |n: usize| format!("the video stream av1opt made in {} could not be read at frame {n}; {WENT_WRONG_INSIDE}", path.display());
    // Each frame: its size in 4 bytes, its timestamp in 8, the frame.
    while !r.fill_buf().with_context(|| unread(frames.len() + 1))?.is_empty() {
        let mut f = [0u8; 12];
        if let Err(e) = r.read_exact(&mut f) {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                bail!("the video stream av1opt made in {} stops inside the header of frame {}; {WENT_WRONG_INSIDE}", path.display(), frames.len() + 1);
            }
            return Err(e).with_context(|| unread(frames.len() + 1));
        }
        let size = u32::from_le_bytes([f[0], f[1], f[2], f[3]]);
        let pts = u64::from_le_bytes(f[4..12].try_into().expect("eight bytes"));
        frames.push(IvfFrame { offset: offset + 12, size, pts });
        r.seek_relative(size as i64).with_context(|| unread(frames.len()))?;
        offset += 12 + size as u64;
    }
    if offset != std::fs::metadata(path).with_context(|| unread(frames.len()))?.len() {
        bail!("the video stream av1opt made in {} stops partway through frame {}; {WENT_WRONG_INSIDE}", path.display(), frames.len());
    }
    Ok((head, frames))
}

/// Writes the video with the frames of each fixed run taken from its new encode. Every frame
/// keeps the timestamp the one-pass encode gave it, counted from the first. Each new encode
/// has to say in its sequence header what the video's says, but for what changes nothing in
/// how a frame is decoded (see [`sequence_header::joinable`]): those differences are returned.
fn splice(source: &Path, head: &[u8; 32], index: &[IvfFrame], fixed: &[Fixed], out: &Path) -> Result<Vec<sequence_header::Difference>> {
    use std::io::Write;
    use std::os::unix::fs::FileExt;
    let src = std::fs::File::open(source)?;
    let read = |from: &std::fs::File, f: &IvfFrame| -> Result<Vec<u8>> {
        let mut buf = vec![0; f.size as usize];
        from.read_exact_at(&mut buf, f.offset)?;
        Ok(buf)
    };
    // Before anything is written: a piece that cannot go in leaves the encode as it was.
    let first = index.first().map(|f| read(&src, f)).transpose()?.unwrap_or_default();
    let Some(header) = sequence_header::first_in(&first).map(<[u8]>::to_vec) else {
        bail!("the encode's first frame holds no sequence header; {WENT_WRONG_INSIDE}");
    };
    let mut differences: Vec<sequence_header::Difference> = Vec::new();
    let mut pieces = Vec::new();
    for f in fixed {
        let (_, new) = ivf_index(&f.file)?;
        let file = std::fs::File::open(&f.file)?;
        let start = new.first().map(|x| read(&file, x)).transpose()?.unwrap_or_default();
        let piece_header = sequence_header::first_in(&start);
        let joined = piece_header.ok_or_else(|| "it starts without a sequence header".to_string()).and_then(|h| sequence_header::joinable(&header, h));
        // MP4's av1C box and Matroska's codec data, which tell a player the level before any
        // frame, are written from the first sequence header in the stream (seen in both with
        // a stream whose first GOP says 2.0 and the next 4.0: the box says 2.0). A piece that
        // begins the video and says less than the rest would leave them saying too little for
        // the frames after it. Anywhere else, or saying more, it goes in.
        if f.run.start == 0
            && let Some(lower) = piece_header.and_then(|h| sequence_header::lower_level(&header, h))
        {
            bail!(
                "the stretch encoded again from frame 0 cannot be put into the video: it begins the video and says a lower level ({}, where the stream it goes into says {}), which the container would then give for all of it",
                lower.piece,
                lower.stream
            );
        }
        match joined {
            // A piece that differs in what does not change how its own frames decode still
            // leaves its header in force until the next one: the GOP after it decodes right
            // only if its key frame carries the stream's header again. SVT-AV1 writes one at
            // every key frame, and ffmpeg keeps them through MP4 and Matroska; that is held
            // here rather than trusted, as a frame that is another piece's has one of its own.
            Ok(found) if !found.is_empty() && f.run.end < index.len() && !fixed.iter().any(|g| g.run.start == f.run.end) && sequence_header::first_in(&read(&src, &index[f.run.end])?).is_none() => {
                let what: Vec<&str> = found.iter().map(|d| d.what).collect();
                bail!(
                    "the stretch encoded again from frame {} cannot be put into the video: its {} differs, and frame {}, the first after it, carries no sequence header of its own to set the stream's back",
                    f.run.start,
                    what.join(" and "),
                    f.run.end
                );
            }
            Ok(found) => {
                for d in found {
                    if !differences.iter().any(|e| e.what == d.what) {
                        differences.push(d);
                    }
                }
            }
            Err(why) => bail!("the stretch encoded again from frame {} cannot be put into the video: {why}", f.run.start),
        }
        pieces.push((file, new));
    }
    let mut w = std::io::BufWriter::new(crate::files::create_new(out)?);
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
    let mut next = fixed.iter().zip(&pieces).peekable();
    while i < index.len() {
        match next.peek() {
            Some((f, (file, new))) if f.run.start == i => {
                if new.len() != f.run.end - f.run.start {
                    bail!("a stretch encoded again came back with {} frames where {} were to be replaced; {WENT_WRONG_INSIDE}", new.len(), f.run.end - f.run.start);
                }
                for (k, frame) in new.iter().enumerate() {
                    copy(&mut w, file, frame, index[i + k].pts)?;
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
        bail!("a stretch encoded again does not begin at a frame of the video; {WENT_WRONG_INSIDE}");
    }
    w.flush()?;
    Ok(differences)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn next_crf(known: &[(f64, f64)], floor: f64, lo: f64) -> (Option<f64>, bool) {
        next_try(known, Goal::holding(floor), lo)
    }

    #[test]
    fn a_source_is_counted_on_for_repairs_only_when_the_survey_will_take_its_times() {
        let even: Vec<i64> = (0..1000).map(|i| i * 33).collect();
        assert!(times_can_be_gone_by(even.clone(), (1, 1000)));
        assert!(times_can_be_gone_by(vec![7], (1, 1000)));
        // Two frames at one time, as Matroska's milliseconds leave them: a few are let by.
        let mut doubled = even.clone();
        doubled[500] = doubled[499];
        assert!(times_can_be_gone_by(doubled, (1, 1000)));
        // Every `nth` frame shown twice as long: 3 in a hundred pass, 12 in a hundred do not.
        let held = |nth: i64| {
            let mut at = 0;
            (0..1000).map(|i| { at += if i % nth == 0 { 66 } else { 33 }; at }).collect::<Vec<i64>>()
        };
        assert!(times_can_be_gone_by(held(33), (1, 1000)));
        assert!(!times_can_be_gone_by(held(8), (1, 1000)));
    }

    #[test]
    fn the_smallest_try_that_holds_is_kept_and_short_of_that_what_is_worth_its_bytes() {
        let floor = Goal::holding(89.0);
        // Above, at and below the floor: the highest CRF of those that hold it.
        assert_eq!(try_to_keep(&[(36.0, 91.0, true), (35.0, 92.0, true), (37.0, 89.3, true), (38.0, 88.5, true)], floor, 85.0, false), Some(2));
        // Just short of where an encode counts as holding it: none does, so the best.
        assert_eq!(try_to_keep(&[(37.0, 89.2, true), (36.0, 89.1, true)], floor, 85.0, false), Some(0));
        // Out of reach: the smallest within half a point of the best...
        assert_eq!(try_to_keep(&[(34.0, 87.0, true), (32.0, 87.4, true), (30.0, 87.8, true)], floor, 85.0, true), Some(1));
        // ...unless what stands there is that close already.
        assert_eq!(try_to_keep(&[(34.0, 87.0, true), (32.0, 87.4, true)], floor, 87.0, true), None);
        // A try that would leave a window below the floor is never the one kept, however it scores.
        assert_eq!(try_to_keep(&[(36.0, 88.9, false), (37.0, 88.0, true)], floor, 85.0, false), Some(1));
        assert_eq!(try_to_keep(&[(34.0, 87.0, true), (36.0, 87.0, false)], floor, 85.0, true), Some(0));
        // For the mean, a try that would leave a window below the floor does not count however
        // well it scores, and one that does not add to the mean is not worth its bytes.
        let mean = Goal::mean(93.0);
        assert_eq!(try_to_keep(&[(32.0, 93.2, false), (31.0, 92.9, true)], mean, 92.0, false), Some(1));
        assert_eq!(try_to_keep(&[(31.0, 91.5, true)], mean, 92.0, false), None);
        assert_eq!(try_to_keep(&[], floor, 85.0, false), None);
        // Never more than 16 below the video's CRF, nor below the lowest allowed.
        assert_eq!((lowest_try(38.0, 20), lowest_try(30.0, 20)), (22.0, 20.0));
    }

    #[test]
    fn encoding_stretches_again_asks_for_two_copies_and_a_stretch() {
        assert_eq!(room_to_repair(0), 1_500_000_000);
        assert_eq!(room_to_repair(10_000_000_000), 23_500_000_000);
    }

    #[test]
    fn stretches_are_left_alone_when_there_is_no_room_to_encode_them() {
        let dir = std::env::temp_dir().join(format!("av1opt-repair-room-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // An encode of a gigabyte, on a drive said to have a gigabyte and a half free: less
        // than two copies and a stretch.
        let output = dir.join("out.mp4");
        std::fs::File::create(&output).unwrap().set_len(1_000_000_000).unwrap();
        crate::encode::FREE_IN_TESTS.set(Some(1_500_000_000));
        let tools = Tools::new("false".into(), "false".into(), None);
        let (info, enc) = (MediaInfo::default(), EncodeSettings { preset: 8, pix_fmt_out: "yuv420p10le", params: String::new(), keyint_frames: None });
        let c = Ctx { tools: &tools, input: &dir.join("in.mkv"), info: &info, enc: &enc, crf: 30.0, floor: 90.0, target: 70.0, min_crf: 20, threads: 1, dir: &dir, cut: None };
        let survey = Survey { scores: scored(300, |_| 70.0), times: Times { pts: (0..300).collect(), tb: (1, 30) }, gops: vec![0] };
        let why = repair(&c, &output, &survey).err().expect("no room").to_string();
        // One line: it ends up in the note that says the stretches were left as they are.
        assert_eq!(why, format!("not enough free space in {}: 1.5 GB free, ~3.7 GB needed", dir.display()));
        // With nothing to encode again, no room is asked for.
        let fine = Survey { scores: scored(300, |_| 95.0), times: Times { pts: (0..300).collect(), tb: (1, 30) }, gops: vec![0] };
        assert!(repair(&c, &output, &fine).is_ok_and(|o| o.video.is_none()));
        // With the room there, the repair goes on to its first ffmpeg, which here cannot run.
        crate::encode::FREE_IN_TESTS.set(Some(room_to_repair(1_000_000_000)));
        let past = repair(&c, &output, &survey).err().expect("no ffmpeg").to_string();
        assert!(!past.contains("free space"), "{past}");
        crate::encode::FREE_IN_TESTS.set(None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stretch_is_judged_with_the_frames_around_it() {
        // 40 s; the GOP at 10-20 s is weak, most of all in its last two seconds.
        let secs = secs(1200);
        let old = scored(1200, |f| if (540..600).contains(&f) { 80.0 } else if (300..540).contains(&f) { 88.0 } else { 90.0 });
        let run = Run { start: 300, end: 600 };
        // As it stands, its last five seconds are the lowest: 3 s at 88 and 2 s at 80.
        let now = lowest_over(&secs, &old, None, run);
        assert!((now - (88.0 * 3.0 + 80.0 * 2.0) / 5.0).abs() < 0.2, "{now}");
        // Encoded again: fine by itself, its last two seconds still low.
        let new = |tail: f64| scored(1200, |f| if (540..600).contains(&f) { tail } else { 93.0 }).into_iter().filter(|s| (300..600).contains(&s.0)).collect::<Vec<_>>();
        let alone = windows(&secs, &Scores::new(&new(84.0)), 300, 600).iter().map(|w| w.vmaf).fold(f64::INFINITY, f64::min);
        assert!(alone > 89.0, "{alone}");
        // Among the neighbour's frames, 2 s at 84 and 3 s at 90 are a window at 87.6, and
        // the part of it that is this stretch's is the 2 s at 84.
        let placed = lowest_over(&secs, &old, Some(&new(84.0)), run);
        assert!((placed - 87.6).abs() < 0.2, "{placed}");
        // With its own part of every window at the floor it has done what it can do.
        assert!(lowest_over(&secs, &old, Some(&new(89.5)), run) >= 89.5);
        // Frames outside the run keep their scores.
        assert_eq!(lowest_over(&secs, &old, None, Run { start: 900, end: 1200 }), 90.0);
    }

    #[test]
    fn a_weak_neighbour_is_not_made_up_for() {
        // The last 2 s before frame 600 are at 80; the stretch after it scores 90 all along.
        let secs = secs(1200);
        let all = scored(1200, |f| if (540..600).contains(&f) { 80.0 } else { 90.0 });
        // Windows across the edge are at 86 and less, and none of that is this stretch's.
        assert!(lowest_over(&secs, &all, None, Run { start: 600, end: 900 }) >= 90.0 - 1e-9);
        assert!(lowest_over(&secs, &all, None, Run { start: 300, end: 600 }) < 87.0);
    }

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
        // 85 at CRF 38, floor 89: aim 90, five points at 0.8 a step.
        let (next, out) = next_crf(&[(38.0, 85.0)], 89.0, 20.0);
        assert_eq!((next, out), (Some(31.75), false));
        // Never less than a whole step.
        assert_eq!(next_crf(&[(38.0, 88.9)], 89.0, 20.0).0, Some(36.5));
        assert_eq!(next_crf(&[(38.0, 89.1)], 89.0, 20.0).0, Some(36.75));
    }

    #[test]
    fn a_gop_lifted_for_the_mean_takes_small_steps_and_stops_near_its_level() {
        let goal = Goal::mean(92.0);
        // 0.5 to go at 0.8 a step: less than a CRF step, where the floor would take a whole one.
        assert_eq!(next_try(&[(38.0, 91.5)], goal, 20.0), (Some(37.25), false));
        // Close enough under the level, and not far over it: done.
        assert_eq!(next_try(&[(38.0, 91.6), (37.5, 91.9)], goal, 20.0), (None, false));
        assert_eq!(next_try(&[(38.0, 91.6), (37.0, 92.4)], goal, 20.0), (None, false));
        // Well over it, with room back toward the CRF that fell short: a higher CRF.
        assert_eq!(next_try(&[(38.0, 91.6), (36.0, 93.0)], goal, 20.0), (Some(37.25), false));
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
        // 0.6 gained over four steps: the 2.3 still missing would take sixteen more.
        assert_eq!(next_crf(&[(36.5, 86.13), (32.5, 86.71)], 89.0, 20.5), (None, true));
        // A gain read off a step and a half is not taken at its word: 0.25 a step would
        // send the next try to CRF 31.25.
        assert_eq!(next_crf(&[(36.5, 88.67), (35.0, 89.05)], 89.0, 20.5), (Some(33.0), false));
        // The same gain with room below is tried.
        assert!(next_crf(&[(36.5, 86.13), (34.5, 87.4)], 89.0, 20.5).0.is_some());
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

    use sequence_header::sample::{header as header_with, starting as starting_with};

    fn starting(header: &[u8], rest: Vec<u8>) -> Vec<u8> {
        starting_with(header, &rest)
    }

    /// `rest` after the start of a key frame of the full-range video these tests splice into.
    fn key(rest: Vec<u8>) -> Vec<u8> {
        starting(&header_with("color_range", 1), rest)
    }

    /// How many bytes [`key`] puts in front.
    const KEY: u32 = 4 + 15;

    fn fixed(run: Run, file: PathBuf) -> Fixed {
        Fixed { run, crf: 30.0, file, scores: Vec::new(), before: 80.0, bytes: 0, out_of_reach: false, for_mean: false, for_frames: false }
    }

    #[test]
    fn splice_swaps_frames_and_keeps_their_times() {
        let dir = std::env::temp_dir().join(format!("av1opt-splice-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Six frames from timestamp 100 on, uneven steps; frames 2-3 and 5 are replaced.
        let mut src: Vec<(u64, Vec<u8>)> = [100u64, 110, 125, 130, 150, 151].iter().enumerate().map(|(i, &p)| (p, vec![i as u8; i + 1])).collect();
        src[0].1 = key(src[0].1.clone());
        write_ivf(&dir.join("src.ivf"), &src);
        write_ivf(&dir.join("a.ivf"), &[(0, key(vec![0xaa; 7])), (1, vec![0xab; 2])]);
        write_ivf(&dir.join("b.ivf"), &[(0, key(vec![0xbb; 3]))]);
        let (head, index) = ivf_index(&dir.join("src.ivf")).unwrap();
        assert_eq!(index.len(), 6);
        let fx = vec![fixed(Run { start: 2, end: 4 }, dir.join("a.ivf")), fixed(Run { start: 5, end: 6 }, dir.join("b.ivf"))];
        assert_eq!(splice(&dir.join("src.ivf"), &head, &index, &fx, &dir.join("out.ivf")).unwrap(), []);
        let (head2, out) = ivf_index(&dir.join("out.ivf")).unwrap();
        assert_eq!(head, head2);
        assert_eq!(out.iter().map(|f| f.pts).collect::<Vec<_>>(), vec![0, 10, 25, 30, 50, 51]);
        assert_eq!(out.iter().map(|f| f.size).collect::<Vec<_>>(), vec![KEY + 1, 2, KEY + 7, 2, 5, KEY + 3]);
        let bytes = std::fs::read(dir.join("out.ivf")).unwrap();
        assert_eq!(&bytes[(out[2].offset + KEY as u64) as usize..][..7], &[0xaa; 7]);
        assert_eq!(&bytes[(out[5].offset + KEY as u64) as usize..][..3], &[0xbb; 3]);
        assert_eq!(&bytes[out[4].offset as usize..][..5], &[4; 5]);

        // A new encode with the wrong number of frames is refused.
        let fx = vec![fixed(Run { start: 2, end: 5 }, dir.join("a.ivf"))];
        let err = splice(&dir.join("src.ivf"), &head, &index, &fx, &dir.join("bad.ivf")).unwrap_err();
        assert!(err.to_string().contains("were to be replaced"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_cut_off_ivf_is_refused() {
        let dir = std::env::temp_dir().join(format!("av1opt-ivf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        write_ivf(&dir.join("x.ivf"), &[(0, vec![1; 10]), (1, vec![2; 10])]);
        let mut b = std::fs::read(dir.join("x.ivf")).unwrap();
        b.truncate(b.len() - 4);
        std::fs::write(dir.join("x.ivf"), &b).unwrap();
        let e = ivf_index(&dir.join("x.ivf")).err().unwrap().to_string();
        assert!(e.contains("x.ivf stops partway through frame 2;"), "{e}");
        // Cut inside the 12 bytes before the second frame: the bare error said "failed to
        // fill whole buffer", with neither the file nor the frame.
        b.truncate(32 + 12 + 10 + 5);
        std::fs::write(dir.join("x.ivf"), &b).unwrap();
        let e = ivf_index(&dir.join("x.ivf")).err().unwrap().to_string();
        assert!(e.contains(&format!("{} stops inside the header of frame 2;", dir.join("x.ivf").display())), "{e}");
        std::fs::write(dir.join("y.ivf"), b"not an ivf file at all, just text that is long enough").unwrap();
        assert!(ivf_index(&dir.join("y.ivf")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Every third frame of 30 fps video, each GOP of ten seconds at its own score.
    fn gops_scoring(by_gop: &[f64]) -> (Vec<f64>, Vec<(usize, f64)>, Vec<usize>) {
        let frames = by_gop.len() * 300;
        (secs(frames), scored(frames, |f| by_gop[f / 300]), (0..by_gop.len()).map(|g| g * 300).collect())
    }

    fn average(scores: &[(usize, f64)]) -> f64 {
        scores.iter().map(|s| s.1).sum::<f64>() / scores.len() as f64
    }

    fn gop(i: usize) -> Run {
        Run { start: i * 300, end: (i + 1) * 300 }
    }

    #[test]
    fn repairs_are_imitated_on_the_scores() {
        let (s, scores, gops) = gops_scoring(&[94.0, 86.0, 94.0, 94.0, 94.0, 94.0]);
        // The weak GOP alone is blamed, and rises to where repairs land above the floor.
        let done = imitate(&s, &scores, &gops, 89.0, 20.0);
        assert!((average(&done.scores) - (94.0 * 5.0 + 89.0 + IMITATED_LANDING) / 6.0).abs() < 1e-6, "{}", average(&done.scores));
        assert_eq!((done.repaired.clone(), done.secs.round()), (vec![gop(1)], 10.0));
        // Nothing below the floor: nothing changes.
        let done = imitate(&s, &scores, &gops, 85.0, 20.0);
        assert!(done.repaired.is_empty() && done.secs == 0.0 && done.scores == scores);
        // Frames already high rise too, though less: the GOP is lifted as one piece.
        let (s, scores, _) = gops_scoring(&[94.0, 94.0]);
        let scores: Vec<(usize, f64)> = scores.into_iter().map(|(f, v)| (f, if (300..450).contains(&f) { 80.0 } else { v })).collect();
        let mut lifted = scores.clone();
        assert!(lift_run(&mut lifted, &windows(&s, &Scores::new(&scores), 0, 600), gop(1), 89.0));
        assert!(lifted.iter().filter(|x| x.0 >= 450).all(|x| x.1 > 94.0), "{lifted:?}");
    }

    #[test]
    fn neighbouring_gops_are_counted_once_and_joined_up_to_the_longest_run() {
        let (s, scores, gops) = gops_scoring(&[80.0, 80.0, 80.0, 94.0]);
        let done = imitate(&s, &scores, &gops, 89.0, 20.0);
        assert!((done.secs - 30.0).abs() < 1e-9, "{}", done.secs);
        assert_eq!(done.repaired, vec![Run { start: 0, end: 600 }, gop(2)]);
        assert!((average(&done.scores) - ((89.0 + IMITATED_LANDING) * 3.0 + 94.0) / 4.0).abs() < 1e-6, "{}", average(&done.scores));
    }

    #[test]
    fn the_lowest_gops_left_are_lifted_to_one_level_just_high_enough() {
        // Three points of GOP mean to find, over six GOPs.
        let (s, scores, _) = gops_scoring(&[80.0, 90.0, 91.0, 95.0, 99.0, 100.0]);
        let spare: Vec<Run> = (1..6).map(gop).collect();
        let goal = average(&scores) + 3.0 / 6.0;
        let lifts = plan_lifts(&scores, &spare, &s, goal, 93.0, 60.0).expect("something to lift");
        assert!(lifts.reaches, "{lifts:?}");
        assert_eq!(lifts.runs, vec![gop(1), gop(2)], "{lifts:?}");
        // 90 and 91 brought level: the three points missing leave them at 92.
        assert!((lifts.level - 92.0).abs() < 1e-9, "{lifts:?}");
        assert!((lifts.secs - 20.0).abs() < 1e-9, "{lifts:?}");
    }

    #[test]
    fn a_gop_is_lifted_by_a_quarter_at_least_and_never_past_the_cap() {
        let (s, scores, _) = gops_scoring(&[90.0, 95.0]);
        let spare = vec![gop(0), gop(1)];
        // A hundredth of a point of the video's mean, that the lowest GOP could give at 90.02.
        let goal = average(&scores) + 0.01;
        let lifts = plan_lifts(&scores, &spare, &s, goal, 93.0, 20.0).unwrap();
        assert_eq!((lifts.runs.clone(), lifts.level), (vec![gop(0)], 90.25), "{lifts:?}");
        // More than the cap and the budget allow: as far as they do, and it says so.
        let lifts = plan_lifts(&scores, &spare, &s, 94.5, 93.0, 10.0).unwrap();
        assert_eq!((lifts.runs.clone(), lifts.level, lifts.reaches), (vec![gop(0)], 93.0, false), "{lifts:?}");
        // A GOP that does not fit the budget, or stands at the cap, is not taken.
        assert_eq!(plan_lifts(&scores, &spare, &s, 94.0, 93.0, 5.0), None);
        assert_eq!(plan_lifts(&scores, &[gop(1)], &s, 96.0, 93.0, 20.0), None);
        // Nothing missing, nothing to do.
        assert_eq!(plan_lifts(&scores, &spare, &s, 92.0, 93.0, 20.0), None);
    }

    #[test]
    fn repaired_stretches_stay_out_of_the_plan_for_the_mean() {
        // The GOP below the floor is repaired, to its landing; the mean is still short, and what makes
        // it up are the next-lowest GOPs, not the repaired one.
        let by_gop = [86.0, 92.0, 92.4, 93.7, 93.7, 93.7, 93.7, 93.7];
        let (s, scores, gops) = gops_scoring(&by_gop);
        let plan = plan_from(&s, &scores, &gops, 89.0, 93.0, 20.0);
        assert!((plan.mean - (89.0 + IMITATED_LANDING + 92.0 + 92.4 + 93.7 * 5.0) / 8.0).abs() < 1e-6, "{plan:?}");
        let lifts = plan.top_up.clone().expect("short of the target");
        assert!(lifts.reaches && !lifts.runs.contains(&gop(0)), "{plan:?}");
        assert_eq!(lifts.runs[0], gop(1), "{plan:?}");
        assert_eq!(plan.top_up_secs(), lifts.secs);
        // Already there: nothing to plan.
        let plan = plan_from(&s, &scores, &gops, 89.0, 92.0, 20.0);
        assert_eq!(plan.top_up, None, "{plan:?}");
    }

    #[test]
    fn the_mean_is_made_up_gop_by_gop_going_by_what_each_really_scored() {
        let (s, start, _) = gops_scoring(&[90.0, 90.5, 91.0, 91.5, 94.0, 94.0, 94.0, 94.0, 94.0, 94.0, 94.0, 94.0]);
        let spare: Vec<Run> = (0..12).map(gop).collect();
        let mut scores = start.clone();
        let mut asked = Vec::new();
        // Every encode lands a little short of what it was asked for.
        let level = top_up(&s, &mut scores, spare, 93.0, |run, level, _| {
            asked.push(run);
            Ok(Some(scored(run.end, |_| level - 0.1).into_iter().filter(|x| x.0 >= run.start).collect()))
        })
        .unwrap();
        assert!(average(&scores) >= 93.0, "{}", average(&scores));
        assert!(level.unwrap() <= 93.0, "{level:?}");
        // Lowest first, and only as many as it took.
        assert_eq!(asked[..2], [gop(0), gop(1)], "{asked:?}");
        assert!(asked.len() <= 3, "{asked:?}");
        assert!(scores.iter().filter(|x| x.0 >= 1200).all(|x| x.1 == 94.0));
    }

    #[test]
    fn a_mean_still_short_gets_one_higher_round_and_then_is_left() {
        // GOPs that give nothing, whatever is asked. The two below the cap are tried, then the
        // cap rises once and takes in the next two, which is all the quarter of the video allows.
        let mut by_gop = vec![92.9; 16];
        (by_gop[0], by_gop[1]) = (88.0, 92.0);
        let (s, start, _) = gops_scoring(&by_gop);
        let spare: Vec<Run> = (0..16).map(gop).collect();
        let mut scores = start.clone();
        let mut levels = Vec::new();
        let level = top_up(&s, &mut scores, spare, 93.0, |run, level, now| {
            levels.push((run, level));
            Ok(Some(now.iter().copied().filter(|x| x.0 >= run.start && x.0 < run.end).collect()))
        })
        .unwrap();
        assert_eq!(levels.len(), 4, "{levels:?}");
        assert!(levels[..2].iter().all(|l| l.1 <= 93.0) && levels[2..].iter().all(|l| l.1 > 93.0 && l.1 <= 93.5), "{levels:?}");
        assert_eq!(level, Some(93.5), "{levels:?}");
        // Close under the target there is no further round: this GOP, too near the cap to be
        // lifted, is not asked for.
        let (s, start, _) = gops_scoring(&[90.0, 93.0, 93.0, 93.0, 93.0, 93.0, 93.0, 93.0]);
        let mut scores = start.iter().map(|&(f, v)| (f, if f < 300 { 92.8 } else { v })).collect::<Vec<_>>();
        let mut asked = 0;
        top_up(&s, &mut scores, vec![gop(0)], 93.0, |_, _, _| {
            asked += 1;
            Ok(None)
        })
        .unwrap();
        assert_eq!(asked, 0);
    }

    fn plan(mean: f64, secs: f64, top_up: Option<(f64, bool)>) -> MeanPlan {
        let lifts = |(level, reaches)| Lifts { level, runs: Vec::new(), secs: 30.0, reaches };
        MeanPlan { mean, secs, top_up: top_up.map(lifts), length: 1000.0 }
    }

    #[test]
    fn the_whole_video_is_encoded_again_only_where_stretches_will_not_do() {
        let rules = MeanRules { target: 93.0, slope: 0.5, crf: 38.0, min_crf: 20.0, max_crf: 55.0, may_encode_again: true };
        // On target after the repairs, or above by less than a second encode is worth: nothing more.
        assert_eq!(encode_again_at(&plan(93.1, 100.0, None), &rules), None);
        assert_eq!(encode_again_at(&plan(93.4, 100.0, None), &rules), None);
        assert_eq!(encode_again_at(&plan(94.4, 100.0, None), &rules), None);
        // Short, and the GOPs can make it up.
        assert_eq!(encode_again_at(&plan(92.8, 100.0, Some((92.0, true))), &rules), None);
        // Short beyond what they can: the CRF moves by what is missing.
        assert_eq!(encode_again_at(&plan(92.8, 100.0, Some((93.0, false))), &rules), Some(37.5));
        assert_eq!(encode_again_at(&plan(92.0, 100.0, None), &rules), Some(36.0));
        // Well over the target: a higher CRF.
        assert_eq!(encode_again_at(&plan(94.8, 100.0, None), &rules), Some(41.5));
        // Not when much of the video is being repaired: its CRF hardly moves that mean.
        assert_eq!(encode_again_at(&plan(95.3, 800.0, None), &rules), None);
        // Once only.
        let late = MeanRules { may_encode_again: false, ..rules };
        assert_eq!(encode_again_at(&plan(94.8, 100.0, None), &late), None);
        assert_eq!(encode_again_at(&plan(92.0, 100.0, None), &late), None);
        // A correction too small to change the CRF is no reason for an encode.
        assert_eq!(encode_again_at(&plan(92.95, 100.0, None), &rules), None);
        // The CRF stays within the range allowed.
        assert_eq!(encode_again_at(&plan(95.0, 100.0, None), &MeanRules { max_crf: 40.0, ..rules }), Some(40.0));
    }

    #[test]
    fn a_few_stretches_left_far_below_the_floor_keep_their_repairs() {
        // Floor 89: two windows run together at 10 s, far below; one just
        // under the floor at 40 s. Before, any window more than a point under the floor sent
        // the video to a second search.
        let still = [(11.0, 5.0, 85.5), (10.0, 5.0, 86.0), (40.0, 5.0, 88.5)];
        let left = Remaining::of(&still, 89.0);
        assert_eq!(left, Remaining { near: 1, still_low: vec![LowStretch { start_secs: 10.0, secs: 6.0, vmaf: 85.5 }] });
        assert!(!left.too_much(200.0));
        // In a video of 100 s those 6 s are more than a twentieth of it.
        assert!(left.too_much(100.0));
    }

    #[test]
    fn what_stays_far_below_the_floor_decides_between_keeping_and_searching_again() {
        let row = |n: usize, vmaf: f64| (0..n).map(|i| (i as f64, 5.0, vmaf)).collect::<Vec<_>>();
        // Windows a second apart: 21 of them run together over 25 s, a twentieth of 500 s, kept;
        // 22 over 26 s are more.
        let kept = Remaining::of(&row(21, 85.0), 89.0);
        assert_eq!(kept, Remaining { near: 0, still_low: vec![LowStretch { start_secs: 0.0, secs: 25.0, vmaf: 85.0 }] });
        assert!(!kept.too_much(500.0));
        assert!(Remaining::of(&row(22, 85.0), 89.0).too_much(500.0));
        // Windows just under the floor are let be however many there are.
        let near = Remaining::of(&row(60, 88.5), 89.0);
        assert_eq!(near, Remaining { near: 60, still_low: vec![] });
        assert!(!near.too_much(100.0));
        // Stretches apart add up; one only just under the floor is not named, and a window
        // just under it beside one far under belongs to that stretch.
        let apart = [(0.0, 5.0, 85.0), (20.0, 5.0, 88.5), (40.0, 5.0, 87.0), (44.0, 5.0, 88.9)];
        let kept = Remaining::of(&apart, 89.0);
        assert_eq!(kept.near, 1);
        assert_eq!(kept.still_low.iter().map(|s| (s.start_secs, s.secs)).collect::<Vec<_>>(), [(0.0, 5.0), (40.0, 9.0)]);
        assert!(!kept.too_much(280.0) && kept.too_much(279.0));
        assert_eq!(Remaining::of(&[], 89.0), Remaining { near: 0, still_low: vec![] });
    }

    #[test]
    fn stretches_left_far_below_the_floor_get_more_rounds_while_they_are_few() {
        // 200 s at 10 fps: a frame a tenth of a second.
        let secs: Vec<f64> = (0..=2000).map(|i| i as f64 / 10.0).collect();
        let w = |at: usize, vmaf: f64| Window { start: at * 10, end: at * 10 + 50, vmaf };
        // Two windows far below the floor, run together over 6 s, and one just under it.
        let low = vec![w(10, 85.0), w(11, 85.5), w(40, 88.5)];
        assert!(more_rounds_for(&low, &[], &secs, 89.0));
        // Windows just under the floor are let be after the usual rounds.
        assert!(!more_rounds_for(&[w(40, 88.5)], &[], &secs, 89.0));
        // Low over more than a twentieth of the video: it is searched again instead.
        let wide: Vec<Window> = (0..30).map(|i| w(i, 85.0)).collect();
        assert!(!more_rounds_for(&wide, &[], &secs, 89.0));
        // A stretch given up on holds its windows down, and they ask for nothing more.
        let held = [Run { start: 100, end: 160 }];
        assert!(!more_rounds_for(&low, &held, &secs, 89.0));
    }

    #[test]
    fn the_note_names_the_stretches_kept_far_below_the_floor() {
        let summary = |still_low: Vec<LowStretch>| Summary {
            mean_vmaf: 0.0, lowest_vmaf: 0.0, lowest_before: 0.0, floor: 89.0, stretches: Vec::new(), near: 0, left: Vec::new(), still_low, far_below: Vec::new(), mean_before: 0.0, top_up: None, secs: 0.0,
        };
        assert_eq!(summary(Vec::new()).still_low_note(), None);
        let one = summary(vec![LowStretch { start_secs: 725.0, secs: 8.0, vmaf: 85.96 }]).still_low_note().unwrap();
        assert_eq!(one, "1 stretch could not be brought up to VMAF 89: 0:12:05 for 8 s (VMAF 86.0); the video is kept with it at the best encode tried rather than searched and encoded again whole");
        let many: Vec<LowStretch> = (0..10).map(|i| LowStretch { start_secs: i as f64 * 60.0, secs: 5.0, vmaf: 86.0 }).collect();
        let note = summary(many).still_low_note().unwrap();
        assert!(note.starts_with("10 stretches could not") && note.contains("0:07:00 for 5 s") && !note.contains("0:08:00") && note.contains(" and 2 more;"), "{note}");
        // The report lists them all.
        let json = serde_json::to_value(summary(vec![LowStretch { start_secs: 1.0, secs: 5.0, vmaf: 86.0 }])).unwrap();
        assert_eq!(json["still_low"][0]["vmaf"], 86.0);
        assert!(serde_json::to_value(summary(Vec::new())).unwrap().get("still_low").is_none());
    }

    #[test]
    fn frames_far_below_the_frames_around_them_are_found_and_a_low_scene_is_not() {
        let s = secs(900);
        // One scored frame wrecked among frames at 93 takes 1.6 off its windows: none shows it.
        let one = scored(900, |i| if i == 300 { 15.0 } else { 93.0 });
        let lowest = windows(&s, &Scores::new(&one), 0, 900).iter().map(|w| w.vmaf).fold(f64::INFINITY, f64::min);
        assert!(lowest > 91.0, "{lowest}");
        assert_eq!(far_below(&s, &one, 0, 900), [(100, 93.0)]);
        // Three in a row.
        let burst = scored(900, |i| if (300..309).contains(&i) { 15.0 } else { 93.0 });
        let found = far_below(&s, &burst, 0, 900);
        assert_eq!(found.iter().map(|f| burst[f.0].0).collect::<Vec<_>>(), [300, 303, 306]);
        assert!(found.iter().all(|f| f.1 == 93.0), "{found:?}");
        // Only the frames asked about are looked at; the frames around them are all there.
        assert_eq!(far_below(&s, &burst, 303, 306).len(), 1);
        assert_eq!(far_below_in(&s, &burst, None, Run { start: 0, end: 300 }), 0);
        assert_eq!(far_below_in(&s, &burst, None, Run { start: 300, end: 600 }), 3);
        // Eight in a row, as the run that showed this had them.
        assert_eq!(far_below(&s, &scored(900, |i| if (300..324).contains(&i) { 15.0 } else { 93.0 }), 0, 900).len(), 8);
        // 25 under is the line: just over it counts, just under it does not.
        assert_eq!(far_below(&s, &scored(900, |i| if i == 300 { 67.9 } else { 93.0 }), 0, 900).len(), 1);
        assert_eq!(far_below(&s, &scored(900, |i| if i == 300 { 68.1 } else { 93.0 }), 0, 900).len(), 0);
        // A scene of low scores, however low and however sharp its edges: the frames around
        // its frames are low too, at its first and last frame as many as are high.
        for scene in [300..360, 300..336, 0..60, 840..900] {
            let low = scored(900, |i| if scene.contains(&i) { 40.0 } else { 95.0 });
            assert_eq!(far_below(&s, &low, 0, 900), [], "{scene:?}");
        }
        // One of a second or less cannot be told from a burst: it is encoded again, and
        // kept as it was if it scores the same.
        assert_eq!(far_below(&s, &scored(900, |i| if (300..330).contains(&i) { 40.0 } else { 95.0 }), 0, 900).len(), 10);
        // Scores that swing by 20 from frame to frame, as flat pictures with cuts do.
        assert_eq!(far_below(&s, &scored(900, |i| if i % 2 == 0 { 100.0 } else { 80.0 }), 0, 900), []);
    }

    #[test]
    fn the_first_and_last_frames_are_judged_by_the_two_seconds_beside_them() {
        let s = secs(900);
        let found = |f: &dyn Fn(usize) -> f64| far_below(&s, &scored(900, f), 0, 900).iter().map(|x| x.0 * 3).collect::<Vec<_>>();
        assert_eq!(found(&|i| if i == 0 { 20.0 } else { 93.0 }), [0]);
        assert_eq!(found(&|i| if i < 9 { 20.0 } else { 93.0 }), [0, 3, 6]);
        assert_eq!(found(&|i| if i == 897 { 20.0 } else { 93.0 }), [897]);
        assert_eq!(found(&|i| if i >= 885 { 20.0 } else { 93.0 }), [885, 888, 891, 894, 897]);
        // A video too short to hold six scored frames beside one says nothing of it.
        assert_eq!(far_below(&secs(18), &scored(18, |i| if i == 9 { 20.0 } else { 93.0 }), 0, 18), []);
        assert_eq!(far_below(&secs(21), &scored(21, |i| if i == 9 { 20.0 } else { 93.0 }), 0, 21).len(), 1);
        // Where frames are scored far apart (a hole in the timestamps), likewise.
        let holes: Vec<f64> = (0..=900).map(|i| i as f64).collect();
        assert_eq!(far_below(&holes, &scored(900, |i| if i == 300 { 20.0 } else { 93.0 }), 0, 900), []);
    }

    #[test]
    fn a_stretch_encoded_again_is_judged_for_such_frames_with_the_frames_beside_it() {
        let s = secs(900);
        let old = scored(900, |i| if (300..309).contains(&i) { 15.0 } else { 93.0 });
        let run = Run { start: 300, end: 600 };
        let whole: Vec<(usize, f64)> = scored(900, |_| 93.0).into_iter().filter(|f| (300..600).contains(&f.0)).collect();
        assert_eq!(far_below_in(&s, &old, Some(&whole), run), 0);
        // Its own first frame wrecked, the frames before it being the neighbour's.
        let worse: Vec<(usize, f64)> = whole.iter().map(|&(f, v)| (f, if f == 300 { 30.0 } else { v })).collect();
        assert_eq!(far_below_in(&s, &old, Some(&worse), run), 1);
        // While it holds one it counts as three under the floor, whatever its windows say,
        // and as what they say once it holds none.
        assert_eq!(counted_with(92.0, 1, 89.0), 86.0);
        assert_eq!(counted_with(80.0, 1, 89.0), 80.0);
        assert_eq!(counted_with(92.0, 0, 89.0), 92.0);
    }

    #[test]
    fn a_frame_that_stays_far_below_after_five_crf_steps_is_how_the_picture_is() {
        let goal = Goal { wrecked: true, same_first: true, ..Goal::holding(89.0) };
        // Encoded as it was and still there: five steps down next.
        assert_eq!(next_try(&[(38.0, 86.0), (38.0, 86.0)], goal, 22.0), (Some(33.0), false));
        // No better there: out of reach, and what stood there stays.
        assert_eq!(next_try(&[(38.0, 86.0), (38.0, 86.0), (33.0, 86.0)], goal, 22.0), (None, true));
        assert_eq!(try_to_keep(&[(38.0, 86.0, true), (33.0, 86.0, true)], goal, 86.0, true), None);
        // Whole at the same CRF: that encode is kept, and nothing lower or higher is tried.
        assert_eq!(next_try(&[(38.0, 86.0), (38.0, 92.0)], goal, 22.0), (None, false));
        assert_eq!(try_to_keep(&[(38.0, 92.0, true)], goal, 86.0, false), Some(0));
        // Whole five steps down only: that one is kept.
        assert_eq!(try_to_keep(&[(38.0, 86.0, true), (33.0, 93.5, true)], goal, 86.0, false), Some(1));
    }

    #[test]
    fn frames_far_below_are_foretold_as_landing_among_the_frames_around_them() {
        let s = secs(900);
        let gops = [0, 300, 600];
        let clean = plan_from(&s, &scored(900, |_| 93.0), &gops, 89.0, 93.0, 30.0);
        let burst = plan_from(&s, &scored(900, |i| if (300..309).contains(&i) { 15.0 } else { 93.0 }), &gops, 89.0, 93.0, 30.0);
        assert_eq!(burst, clean);
        assert!((clean.mean - 93.0).abs() < 1e-9 && clean.top_up.is_none(), "{clean:?}");
    }

    #[test]
    fn the_note_says_where_frames_scored_far_below_those_around_them() {
        let summary = |far_below: Vec<FarBelow>| Summary {
            mean_vmaf: 0.0, lowest_vmaf: 0.0, lowest_before: 0.0, floor: 89.0, stretches: Vec::new(), near: 0, left: Vec::new(), still_low: Vec::new(), far_below, mean_before: 0.0, top_up: None, secs: 0.0,
        };
        assert_eq!(summary(Vec::new()).far_below_note(), None);
        let one = FarBelow { start_secs: 283.6, frames: 8, vmaf: 8.56, around: 93.4, still: 0 };
        assert_eq!(
            summary(vec![one]).far_below_note().unwrap(),
            "8 frames at 0:04:43 scored far below the frames around them (VMAF 8.6 at the lowest among frames at 93.4), which is not how a hard scene looks: that stretch was encoded again"
        );
        let flash = FarBelow { start_secs: 12.0, frames: 1, vmaf: 60.0, around: 95.0, still: 1 };
        assert_eq!(
            summary(vec![flash]).far_below_note().unwrap(),
            "1 frame at 0:00:12 scored far below the frames around it (VMAF 60.0 among frames at 95.0), which is not how a hard scene looks: that stretch was encoded again, and 1 frame still scores so: kept as the picture is there"
        );
        let both = summary(vec![flash, one]).far_below_note().unwrap();
        assert!(both.starts_with("9 frames at 0:00:12, 0:04:43 scored far below") && both.contains("(VMAF 8.6 at the lowest among frames at 93.4)") && both.contains("those stretches were encoded again, and 1 frame still"), "{both}");
        let json = serde_json::to_value(summary(vec![one])).unwrap();
        assert_eq!(json["far_below"][0]["frames"], 8);
        assert!(serde_json::to_value(summary(Vec::new())).unwrap().get("far_below").is_none());
    }

    #[test]
    fn outcome_reads_any_stretch() {
        let o = Outcome {
            summary: Summary { mean_vmaf: 0.0, lowest_vmaf: 0.0, lowest_before: 0.0, floor: 89.0, stretches: Vec::new(), near: 0, left: Vec::new(), still_low: Vec::new(), far_below: Vec::new(), mean_before: 0.0, top_up: None, secs: 0.0 },
            video: None,
            scores: scored(900, |i| if i < 300 { 90.0 } else { 96.0 }),
            secs: secs(900),
            differences: Vec::new(),
        };
        assert_eq!(o.at(0.0, 5.0), Some(90.0));
        assert_eq!(o.at(10.0, 5.0), Some(96.0));
        assert!((o.at(7.5, 5.0).unwrap() - 93.0).abs() < 0.1);
        assert_eq!(o.at(40.0, 5.0), None);
    }

    #[test]
    fn the_timeline_counts_from_the_first_frame_and_ends_a_frame_after_the_last() {
        let line = |pts: Vec<i64>, tb| timeline(&Times { pts, tb });
        assert_eq!(line(vec![100, 103, 106], (1, 30)), vec![0.0, 0.1, 0.2, 0.30000000000000004]);
        // The last frame lasts the average step.
        assert_eq!(line(vec![0, 1, 4], (1, 1)), vec![0.0, 1.0, 4.0, 6.0]);
        // A single frame has no step to go by.
        assert_eq!(line(vec![7], (1, 1000)), vec![0.0, 0.04]);
    }

    #[test]
    fn a_mean_takes_the_first_frame_of_a_range_and_not_the_one_after_it() {
        let sc = Scores::new(&[(0, 10.0), (3, 20.0), (6, 60.0)]);
        assert_eq!(sc.mean(0, 3), Some(10.0));
        assert_eq!(sc.mean(0, 4), Some(15.0));
        assert_eq!(sc.mean(3, 7), Some(40.0));
        assert_eq!(sc.mean(0, 100), Some(30.0));
        // Ranges holding no scored frame.
        assert_eq!(sc.mean(1, 3), None);
        assert_eq!(sc.mean(3, 3), None);
        assert_eq!(sc.mean(7, 100), None);
        assert_eq!(sc.mean(5, 2), None);
        assert_eq!(Scores::new(&[]).mean(0, 100), None);
    }

    #[test]
    fn five_seconds_exactly_are_one_window_and_a_frame_more_are_two() {
        let at = |frames: usize| {
            let s = secs(frames);
            let sc = Scores::new(&(0..frames).map(|i| (i, 90.0)).collect::<Vec<_>>());
            windows(&s, &sc, 0, frames).iter().map(|w| (w.start, w.end)).collect::<Vec<_>>()
        };
        assert_eq!(at(150), vec![(0, 150)]);
        assert_eq!(at(151), vec![(0, 150), (1, 151)]);
        assert_eq!(at(180), vec![(0, 150), (30, 180)]);
        assert_eq!(at(1), vec![(0, 1)]);
    }

    #[test]
    fn windows_stay_inside_the_range_they_were_asked_for() {
        let s = secs(900);
        let sc = Scores::new(&scored(900, |_| 90.0));
        let w = windows(&s, &sc, 300, 600);
        assert_eq!((w[0].start, w[0].end), (300, 450));
        assert_eq!((w[w.len() - 1].start, w[w.len() - 1].end), (450, 600));
        assert!(w.iter().all(|x| x.start >= 300 && x.end <= 600));
    }

    #[test]
    fn a_window_without_a_scored_frame_is_left_out() {
        let s = secs(300);
        assert!(windows(&s, &Scores::new(&[]), 0, 300).is_empty());
        // Scores only in the last second: the early windows hold none of them.
        let late: Vec<(usize, f64)> = (270..300).map(|i| (i, 90.0)).collect();
        let w = windows(&s, &Scores::new(&late), 0, 300);
        assert!(w.iter().all(|x| x.end > 270), "{w:?}");
        assert_eq!(w[w.len() - 1].end, 300);
    }

    #[test]
    fn windows_cover_a_video_at_any_frame_rate_without_gaps() {
        for fps in [24000.0 / 1001.0, 25.0, 30000.0 / 1001.0, 59.94, 120.0, 1.0] {
            let frames = (fps * 37.3) as usize;
            let s: Vec<f64> = (0..=frames).map(|i| i as f64 / fps).collect();
            let sc = Scores::new(&(0..frames).map(|i| (i, 90.0)).collect::<Vec<_>>());
            let w = windows(&s, &sc, 0, frames);
            assert_eq!((w[0].start, w[w.len() - 1].end), (0, frames), "{fps}");
            for pair in w.windows(2) {
                assert!(pair[0].start < pair[1].start && pair[0].end < pair[1].end && pair[1].start < pair[0].end, "{fps}: {pair:?}");
            }
            for x in &w {
                let long = s[x.end] - s[x.start];
                assert!((long - WINDOW_SECS).abs() <= 1.0 / fps + 1e-9, "{fps}: {x:?} is {long} s");
            }
        }
    }

    fn low_window(start: usize, end: usize) -> Window {
        Window { start, end, vmaf: 80.0 }
    }

    #[test]
    fn with_no_window_low_nothing_is_blamed() {
        let sc = Scores::new(&scored(300, |_| 70.0));
        assert_eq!(blame(&[], &[0, 150], 300, &sc, 89.0, &[]), vec![false, false]);
    }

    #[test]
    fn a_window_that_ends_where_a_gop_starts_does_not_touch_it() {
        let sc = Scores::new(&scored(450, |_| 70.0));
        assert_eq!(blame(&[low_window(0, 150)], &[0, 150, 300], 450, &sc, 89.0, &[]), vec![true, false, false]);
        assert_eq!(blame(&[low_window(150, 300)], &[0, 150, 300], 450, &sc, 89.0, &[]), vec![false, true, false]);
        assert_eq!(blame(&[low_window(300, 450)], &[0, 150, 300], 450, &sc, 89.0, &[]), vec![false, false, true]);
    }

    #[test]
    fn a_window_over_a_settled_stretch_is_let_be_on_both_sides_of_it() {
        // Frames 120-270 are low, over the first two GOPs; the first is settled.
        let sc = Scores::new(&scored(450, |_| 70.0));
        let settled = [Run { start: 0, end: 150 }];
        assert_eq!(blame(&[low_window(120, 270)], &[0, 150, 300], 450, &sc, 89.0, &settled), vec![false, false, false]);
        // A stretch that only touches the window's GOPs at their edge changes nothing.
        let elsewhere = [Run { start: 300, end: 450 }];
        assert_eq!(blame(&[low_window(120, 270)], &[0, 150, 300], 450, &sc, 89.0, &elsewhere), vec![true, true, false]);
    }

    #[test]
    fn when_no_part_of_a_low_window_is_low_the_gop_with_most_of_it_answers() {
        let sc = Scores::new(&scored(450, |_| 95.0));
        assert_eq!(blame(&[low_window(100, 190)], &[0, 150, 300], 450, &sc, 89.0, &[]), vec![true, false, false]);
        assert_eq!(blame(&[low_window(140, 290)], &[0, 150, 300], 450, &sc, 89.0, &[]), vec![false, true, false]);
    }

    #[test]
    fn a_run_may_be_exactly_as_long_as_allowed_and_a_gop_alone_may_be_longer() {
        let (frames, gops) = (900, vec![0, 150, 300, 450, 600, 750]);
        let s = secs(frames);
        assert_eq!(runs(&[false; 6], &gops, frames, &s, 20.0), vec![]);
        // Two GOPs are 10 s.
        assert_eq!(runs(&[true, true, false, false, false, false], &gops, frames, &s, 10.0), vec![Run { start: 0, end: 300 }]);
        // Limits below a GOP's length leave each by itself; the last one ends with the video.
        let one_each = runs(&[false, false, false, false, true, true], &gops, frames, &s, 1.0);
        assert_eq!(one_each, vec![Run { start: 600, end: 750 }, Run { start: 750, end: 900 }]);
    }

    #[test]
    fn a_score_from_the_acceptance_line_up_to_the_overshoot_is_kept() {
        // Floor 89: accepted from 89.25, aimed at 90, too good above 91.5.
        for v in [89.25, 90.0, 91.5] {
            assert_eq!(next_crf(&[(40.0, 80.0), (38.0, v)], 89.0, 20.0), (None, false), "{v}");
        }
        assert_eq!(next_crf(&[(38.0, 89.24)], 89.0, 20.0), (Some(37.0), false));
    }

    #[test]
    fn a_score_too_good_goes_back_only_toward_a_crf_that_fell_short() {
        // Nothing above it fell short: there is nowhere to go back to.
        assert_eq!(next_crf(&[(38.0, 92.0)], 89.0, 20.0), (None, false));
        assert_eq!(next_crf(&[(40.0, 90.5), (38.0, 92.0)], 89.0, 20.0), (None, false));
        // Less than a step away: not worth another encode.
        assert_eq!(next_crf(&[(38.5, 85.0), (37.75, 95.0)], 89.0, 20.0), (None, false));
        // A step away: halfway, and never onto the CRF that fell short.
        assert_eq!(next_crf(&[(31.0, 89.5), (30.0, 100.0)], 89.0, 20.0), (Some(30.5), false));
    }

    #[test]
    fn the_next_try_stops_at_the_lowest_crf_allowed() {
        assert_eq!(next_crf(&[(38.0, 85.0)], 89.0, 36.0), (Some(36.0), false));
        assert_eq!(next_crf(&[(36.25, 85.0)], 89.0, 36.0), (Some(36.0), false));
        assert_eq!(next_crf(&[(36.0, 85.0)], 89.0, 36.0), (None, true));
        assert_eq!(next_crf(&[(30.0, 85.0)], 89.0, 36.0), (None, true));
    }

    #[test]
    fn half_a_point_over_three_steps_is_the_line_of_levelling_off() {
        assert_eq!(next_crf(&[(38.0, 85.0), (35.0, 85.49)], 89.0, 10.0), (None, true));
        // At exactly half a point it may go on: half the usual gain a step, 4.5 points to go.
        assert_eq!(next_crf(&[(38.0, 85.0), (35.0, 85.5)], 89.0, 10.0), (Some(26.0), false));
        // Over two steps, a gain that would not reach the floor by the lowest CRF ends it.
        assert_eq!(next_crf(&[(37.0, 85.0), (35.0, 85.2)], 89.0, 10.0), (None, true));
        // Over one step it proves nothing yet: half the usual gain is gone by.
        assert_eq!(next_crf(&[(36.0, 85.0), (35.0, 85.2)], 89.0, 10.0), (Some(25.25), false));
    }

    #[test]
    fn every_try_for_a_short_stretch_is_lower_on_quarter_steps_and_inside_the_range() {
        for crf in (84..=200).map(|q| q as f64 / 4.0) {
            for vmaf in [60.0, 75.5, 85.0, 88.1, 89.24] {
                for lo in [10.0, 21.0, 30.5] {
                    if crf <= lo {
                        continue;
                    }
                    if let (Some(next), _) = next_crf(&[(crf, vmaf)], 89.0, lo) {
                        assert!(next < crf && next >= lo, "{crf} {vmaf} {lo}: {next}");
                        assert_eq!(next * 4.0, (next * 4.0).floor(), "{crf} {vmaf} {lo}: {next}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_stretch_is_judged_by_its_new_scores_where_there_are_any() {
        let s = secs(300);
        let old = scored(300, |_| 90.0);
        let whole = Run { start: 0, end: 300 };
        assert_eq!(lowest_over(&s, &old, None, whole), 90.0);
        assert_eq!(lowest_over(&s, &old, Some(&scored(300, |_| 70.0)), whole), 70.0);
        // Frame 1 is not scored: only the windows over it can speak for it.
        assert_eq!(lowest_over(&s, &old, None, Run { start: 1, end: 2 }), 90.0);
        // Replacing a part leaves the frames around it as they were: ten scores of 70 among
        // forty of 90 in the worst window, where replacing them all would say 70.
        let part: Vec<(usize, f64)> = old.iter().copied().filter(|f| (150..180).contains(&f.0)).map(|(f, _)| (f, 70.0)).collect();
        let low = lowest_over(&s, &old, Some(&part), Run { start: 150, end: 180 });
        assert!((low - 86.0).abs() < 1e-9, "{low}");
    }

    fn kept(frames: usize) -> Outcome {
        Outcome {
            summary: Summary { mean_vmaf: 0.0, lowest_vmaf: 0.0, lowest_before: 0.0, floor: 89.0, stretches: Vec::new(), near: 0, left: Vec::new(), still_low: Vec::new(), far_below: Vec::new(), mean_before: 0.0, top_up: None, secs: 0.0 },
            video: None,
            scores: scored(frames, |_| 90.0),
            secs: secs(frames),
            differences: Vec::new(),
        }
    }

    #[test]
    fn an_outcome_knows_its_length_and_reads_up_to_the_end() {
        let o = kept(900);
        assert_eq!(o.length(), 30.0);
        assert_eq!(o.at(25.0, 100.0), Some(90.0));
        assert_eq!(o.at(-5.0, 6.0), Some(90.0));
        assert_eq!(o.at(5.0, 0.0), None);
        assert_eq!(o.at(30.0, 5.0), None);
    }

    fn ivf_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("av1opt-repair-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_ivf_file_without_frames_or_with_empty_ones_is_read() {
        let dir = ivf_dir("empty");
        let p = dir.join("x.ivf");
        write_ivf(&p, &[]);
        assert!(ivf_index(&p).unwrap().1.is_empty());
        write_ivf(&p, &[(5, Vec::new()), (u64::MAX, vec![1, 2])]);
        let (head, index) = ivf_index(&p).unwrap();
        assert_eq!(&head[..4], b"DKIF");
        assert_eq!(index.iter().map(|f| (f.offset, f.size, f.pts)).collect::<Vec<_>>(), vec![(44, 0, 5), (56, 2, u64::MAX)]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_ivf_file_that_does_not_add_up_is_refused() {
        let dir = ivf_dir("bad");
        let p = dir.join("x.ivf");
        write_ivf(&p, &[(0, vec![1; 10])]);
        let good = std::fs::read(&p).unwrap();
        let refused = |bytes: &[u8]| {
            std::fs::write(&p, bytes).unwrap();
            ivf_index(&p).err().map(|e| e.to_string())
        };
        // Shorter than its header, another header length, another signature.
        assert!(refused(&good[..31]).is_some());
        let mut other = good.clone();
        other[6] = 16;
        assert!(refused(&other).is_some());
        let mut other = good.clone();
        other[0] = b'X';
        assert!(refused(&other).is_some());
        // Half a frame header after the last frame.
        let mut other = good.clone();
        other.extend([0; 5]);
        assert!(refused(&other).is_some());
        // A frame that says it is longer than what is left.
        let e = refused(&good[..good.len() - 1]).unwrap();
        assert!(e.contains("stops partway through frame 1"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn with_nothing_to_replace_only_the_times_move_to_zero() {
        let dir = ivf_dir("copy");
        let frames: Vec<(u64, Vec<u8>)> = vec![(50, key(vec![1; 3])), (60, vec![2; 4]), (75, vec![3])];
        write_ivf(&dir.join("src.ivf"), &frames);
        let (head, index) = ivf_index(&dir.join("src.ivf")).unwrap();
        splice(&dir.join("src.ivf"), &head, &index, &[], &dir.join("out.ivf")).unwrap();
        let (_, out) = ivf_index(&dir.join("out.ivf")).unwrap();
        assert_eq!(out.iter().map(|f| (f.pts, f.size)).collect::<Vec<_>>(), vec![(0, KEY + 3), (10, 4), (25, 1)]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_first_and_last_frames_can_be_replaced_too() {
        let dir = ivf_dir("ends");
        write_ivf(&dir.join("src.ivf"), &[(0, key(vec![1])), (1, vec![2]), (2, vec![3])]);
        write_ivf(&dir.join("a.ivf"), &[(9, key(vec![0xa; 5]))]);
        write_ivf(&dir.join("b.ivf"), &[(9, key(vec![0xb; 6]))]);
        let (head, index) = ivf_index(&dir.join("src.ivf")).unwrap();
        let fx = [fixed(Run { start: 0, end: 1 }, dir.join("a.ivf")), fixed(Run { start: 2, end: 3 }, dir.join("b.ivf"))];
        splice(&dir.join("src.ivf"), &head, &index, &fx, &dir.join("out.ivf")).unwrap();
        let (_, out) = ivf_index(&dir.join("out.ivf")).unwrap();
        assert_eq!(out.iter().map(|f| (f.pts, f.size)).collect::<Vec<_>>(), vec![(0, KEY + 5), (1, 1), (2, KEY + 6)]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_stretch_that_lies_outside_the_frames_left_is_refused() {
        let dir = ivf_dir("outside");
        write_ivf(&dir.join("src.ivf"), &[(0, key(vec![1])), (1, vec![2]), (2, vec![3])]);
        write_ivf(&dir.join("a.ivf"), &[(0, key(vec![9])), (1, vec![9])]);
        let (head, index) = ivf_index(&dir.join("src.ivf")).unwrap();
        for fx in [
            vec![fixed(Run { start: 7, end: 8 }, dir.join("a.ivf"))],
            // The second begins inside the first.
            vec![fixed(Run { start: 0, end: 2 }, dir.join("a.ivf")), fixed(Run { start: 1, end: 3 }, dir.join("a.ivf"))],
        ] {
            let e = splice(&dir.join("src.ivf"), &head, &index, &fx, &dir.join("out.ivf")).unwrap_err().to_string();
            assert!(e.contains("does not begin at a frame"), "{e}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_stretch_whose_sequence_header_says_other_frames_is_refused_and_one_of_another_level_goes_in() {
        let dir = ivf_dir("headers");
        write_ivf(&dir.join("src.ivf"), &[(0, key(vec![1])), (1, vec![2]), (2, vec![3])]);
        let (head, index) = ivf_index(&dir.join("src.ivf")).unwrap();
        let piece = |name: &str, first: Vec<u8>| {
            write_ivf(&dir.join(name), &[(0, first)]);
            vec![fixed(Run { start: 2, end: 3 }, dir.join(name))]
        };
        // Limited range in a full-range video, as the cuts made it before; and no header.
        for (name, first, why) in [
            ("limited.ivf", starting(&header_with("color_range", 0), vec![7]), "its range differs (color_range limited, where the stream it goes into says full)"),
            ("eight-bit.ivf", starting(&header_with("high_bitdepth", 0), vec![7]), "its bit depth differs"),
            ("bare.ivf", vec![0x12, 0x00, 0x32, 0x01, 7], "it starts without a sequence header"),
        ] {
            let out = dir.join(format!("{name}.out"));
            let e = splice(&dir.join("src.ivf"), &head, &index, &piece(name, first), &out).unwrap_err().to_string();
            assert!(e.starts_with("the stretch encoded again from frame 2 cannot be put into the video: ") && e.contains(why), "{name}: {e}");
            // Refused before anything is written.
            assert!(!out.exists(), "{name}");
        }
        // Another level goes in, and is said.
        let fx = piece("level.ivf", starting(&header_with("seq_level_idx[0]", 4), vec![7]));
        let d = splice(&dir.join("src.ivf"), &head, &index, &fx, &dir.join("out.ivf")).unwrap();
        assert_eq!(d, [sequence_header::Difference { what: "level", stream: "2.1".into(), piece: "3.0".into() }]);
        assert_eq!(ivf_index(&dir.join("out.ivf")).unwrap().1.len(), 3);
        // Past a piece of another level, the frame after it has to set the stream's header
        // back: a key frame that holds none leaves the piece's in force.
        write_ivf(&dir.join("bare-after.ivf"), &[(0, key(vec![1])), (1, vec![2]), (2, vec![3]), (3, vec![0x12, 0x00, 0x32, 0x01, 4]), (4, vec![5])]);
        let (head, index) = ivf_index(&dir.join("bare-after.ivf")).unwrap();
        let e = splice(&dir.join("bare-after.ivf"), &head, &index, &fx, &dir.join("bare-after.out")).unwrap_err().to_string();
        assert_eq!(
            e,
            "the stretch encoded again from frame 2 cannot be put into the video: its level differs, and frame 3, the first after it, carries no sequence header of its own to set the stream's back"
        );
        assert!(!dir.join("bare-after.out").exists());
        // One that holds it, or another piece starting there, or the end of the video: it goes in.
        write_ivf(&dir.join("keyed-after.ivf"), &[(0, key(vec![1])), (1, vec![2]), (2, vec![3]), (3, key(vec![4])), (4, vec![5])]);
        let (head, index) = ivf_index(&dir.join("keyed-after.ivf")).unwrap();
        assert_eq!(splice(&dir.join("keyed-after.ivf"), &head, &index, &fx, &dir.join("keyed-after.out")).unwrap().len(), 1);
        let (head, index) = ivf_index(&dir.join("bare-after.ivf")).unwrap();
        write_ivf(&dir.join("next.ivf"), &[(0, key(vec![9])), (1, vec![9])]);
        let both = [fixed(Run { start: 2, end: 3 }, dir.join("level.ivf")), fixed(Run { start: 3, end: 5 }, dir.join("next.ivf"))];
        assert_eq!(splice(&dir.join("bare-after.ivf"), &head, &index, &both, &dir.join("both.out")).unwrap().len(), 1);
        // A piece that differs in nothing needs nothing of the frame after it.
        write_ivf(&dir.join("same.ivf"), &[(0, key(vec![7]))]);
        let same = [fixed(Run { start: 2, end: 3 }, dir.join("same.ivf"))];
        assert_eq!(splice(&dir.join("bare-after.ivf"), &head, &index, &same, &dir.join("same.out")).unwrap(), []);
        // At the start of the video, a piece that says a lower level would be what the
        // container says of all of it; one that says more goes in there too.
        let gop = |name: &str, level: u64| write_ivf(&dir.join(name), &[(0, starting(&header_with("seq_level_idx[0]", level), vec![7])), (1, vec![8]), (2, vec![9])]);
        gop("lower.ivf", 0);
        gop("higher.ivf", 4);
        let at_start = |name: &str| [fixed(Run { start: 0, end: 3 }, dir.join(name))];
        let (head, index) = ivf_index(&dir.join("keyed-after.ivf")).unwrap();
        let e = splice(&dir.join("keyed-after.ivf"), &head, &index, &at_start("lower.ivf"), &dir.join("lower.out")).unwrap_err().to_string();
        assert_eq!(
            e,
            "the stretch encoded again from frame 0 cannot be put into the video: it begins the video and says a lower level (2.0, where the stream it goes into says 2.1), which the container would then give for all of it"
        );
        assert!(!dir.join("lower.out").exists());
        assert_eq!(splice(&dir.join("keyed-after.ivf"), &head, &index, &at_start("higher.ivf"), &dir.join("higher.out")).unwrap().len(), 1);
        let later = [fixed(Run { start: 3, end: 5 }, dir.join("lower.ivf"))];
        write_ivf(&dir.join("lower.ivf"), &[(0, starting(&header_with("seq_level_idx[0]", 0), vec![7])), (1, vec![8])]);
        assert_eq!(splice(&dir.join("keyed-after.ivf"), &head, &index, &later, &dir.join("lower-later.out")).unwrap().len(), 1);
        // A video whose own first frame holds none is not spliced into.
        write_ivf(&dir.join("plain.ivf"), &[(0, vec![1]), (1, vec![2]), (2, vec![3])]);
        let (head, index) = ivf_index(&dir.join("plain.ivf")).unwrap();
        let e = splice(&dir.join("plain.ivf"), &head, &index, &[], &dir.join("plain.out")).unwrap_err().to_string();
        assert!(e.contains("first frame holds no sequence header"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
