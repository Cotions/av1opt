//! Decides whether a file is worth re-encoding and roughly how much it would save.
//!
//! Predicts the AV1 bits per pixel needed at the target quality from the source's bits per
//! pixel and resolution, with a log-linear model fitted to past encodes (see [`crate::history`]).
//! Without history it falls back to the 2026-11-01 pilot (8 clips, preset 6, VMAF ~91-95):
//! ~0.035 bits per pixel at 1080p, falling mildly at higher resolutions and for sources that
//! spend few bits themselves. A few encodes only
//! nudge that prior; many encodes replace it. Every estimate is a range from the data's spread.

use crate::history::{Record, TIMING_VERSION};
use crate::probe::MediaInfo;
use serde::Serialize;

/// The share of its video stream a file has to be expected to save to be worth encoding: the
/// line `encode` itself holds a video to, so that a scan does not turn away what an encode
/// would keep. At 20% a library of 34,578 files had 5,259 turned away, about one in three of
/// which would have saved 15% or more.
pub const MIN_SAVINGS: f64 = 0.15;
/// The shortest picture worth judging, in seconds: `encode` refuses what is shorter.
const MIN_PICTURE_SECS: f64 = 1.0;
/// What a video that turns out not worth encoding costs, as a share of encoding it: it stops
/// after the search, which takes 5 to 13% of an encode.
const COST_OF_A_SKIP: f64 = 0.1;

const BPP_1080P: f64 = 0.035;
/// The preset of the pilot the first guess comes from.
const PILOT_PRESET: u8 = 6;
const PIXELS_1080P: f64 = 1920.0 * 1080.0;
/// Bigger frames compress better per pixel. Pilot data was noisy (one 4K clip needed ~1/4
/// the 1080p bpp, small clips only slightly more), so the prior stays mild and conservative.
const RES_EXPONENT: f64 = -0.2;
/// A source that spends few bits per pixel needs fewer in AV1 too: the detail it would cost is
/// already gone, or (at 60 fps) each frame differs little from the last. 73 encodes at VMAF 93
/// (2026-11-15, mostly 1080p webcam recordings at 5 Mbps, 30 and 60 fps) fit a slope of 0.22.
/// With a flat prior the 60 fps half of them (0.04 bpp) were predicted to save nothing and
/// marked not worth encoding; they came out 37% smaller.
const SOURCE_EXPONENT: f64 = 0.2;
/// Prior strength in "encodes": with 3 real encodes, data and prior weigh the same.
const PRIOR_WEIGHT: f64 = 3.0;
/// Spread of the prior in natural-log units (pilot: 0.018-0.096 bpp around 0.035).
const PRIOR_SIGMA: f64 = 0.6;
/// Reference source bpp the source-bpp feature is centred on.
const SOURCE_BPP_REF: f64 = 0.1;
/// z for an 80% interval.
const Z80: f64 = 1.28;
/// What a video unlike those in the history adds to the spread (log units). Fitted on one
/// library and tried on 22 other videos, only 64% of them fell inside the "80%" range, with
/// errors of x1.65 where the library's own were x1.29.
const UNKNOWN_CONTENT_SIGMA: f64 = 0.4;
/// Squared distance from the history (in standard deviations of its two measures) at which a
/// video counts as wholly unlike it.
const FAR_FROM_HISTORY: f64 = 4.0;
/// Below this many encodes the history says too little of what is like it.
const MIN_FOR_DISTANCE: usize = 5;
/// Size of a video at each SVT-AV1 preset against preset 6, at equal VMAF. Measured with SVT-AV1
/// 4.2 (2026-11-18): preset 3 is 0.84 (eight clips of 10 s only), 4 is 0.906 and 5 is 0.942 (14
/// clips, every one smaller). The others are not measured: slower ones lose a little more per
/// step, faster ones gain 4% a step, less than the 6% from 6 to 5 so that a saving is not
/// overstated.
const PRESET_SIZE: [f64; 14] = [0.78, 0.79, 0.81, 0.84, 0.906, 0.942, 1.0, 1.04, 1.08, 1.12, 1.16, 1.2, 1.24, 1.28];
/// One point of VMAF costs this much in the log of the size: 17% of the bytes, measured over
/// the same clips (2026-11-18).
const LOG_SIZE_PER_VMAF: f64 = 0.157;

/// The least a measure of the history is taken to vary (log units): a history of one frame
/// size has no spread of its own, and a video of the same size is then no distance from it.
const MIN_HISTORY_SD: f64 = 0.1;

/// ln(av1 bpp) = beta[0] + beta[1]·ln(source bpp / 0.1) + beta[2]·ln(pixels / 1080p).
#[derive(Debug, Clone, Copy)]
pub struct Model {
    pub beta: [f64; 3],
    /// Residual spread (log units), blended with the prior's.
    pub sigma: f64,
    /// Encodes the model was fitted on.
    pub samples: usize,
    /// Mean and standard deviation over those encodes of the two measures of a video, for
    /// how far a video is from what the model has seen.
    seen: [(f64, f64); 2],
}

fn features(source_bpp: f64, pixels: f64) -> [f64; 3] {
    [1.0, (source_bpp / SOURCE_BPP_REF).ln(), (pixels / PIXELS_1080P).ln()]
}

fn bpp(bitrate: f64, width: u32, height: u32, fps: f64) -> f64 {
    let px_per_s = width as f64 * height as f64 * fps;
    if px_per_s > 0.0 { bitrate / px_per_s } else { 0.0 }
}

fn preset_size(preset: u8) -> f64 {
    PRESET_SIZE[(preset as usize).min(PRESET_SIZE.len() - 1)]
}

impl Model {
    /// The first guess, for the preset the pilot was made with.
    pub fn prior() -> Self {
        Model { beta: [BPP_1080P.ln(), SOURCE_EXPONENT, RES_EXPONENT], sigma: PRIOR_SIGMA, samples: 0, seen: [(0.0, 0.0); 2] }
    }

    /// The model for encodes at the default preset, for those to whom the preset does not
    /// matter.
    pub fn fit(records: &[Record], target_vmaf: f64) -> Self {
        Self::fit_at_preset(records, target_vmaf, crate::search::DEFAULT_PRESET)
    }

    /// Ridge regression toward the prior, on encodes made for a similar VMAF target. Each is
    /// first brought to the size it would have had at `preset` and exactly at the target.
    /// Encodes with a CRF per scene came out 0.14 larger in the log on five files: too few to
    /// correct for, so they count as they are.
    pub fn fit_at_preset(records: &[Record], target_vmaf: f64, preset: u8) -> Self {
        let mut prior = Self::prior();
        prior.beta[0] += (preset_size(preset) / preset_size(PILOT_PRESET)).ln();
        let pts: Vec<([f64; 3], f64)> = records
            .iter()
            .filter(|r| (r.target_vmaf - target_vmaf).abs() <= 1.5)
            .filter_map(|r| {
                let src = bpp(r.source_bitrate, r.width, r.height, r.fps);
                let out = bpp(r.output_bitrate, r.width, r.height, r.fps);
                let moved = LOG_SIZE_PER_VMAF * (target_vmaf - r.target_vmaf) - (preset_size(r.preset) / preset_size(preset)).ln();
                (src > 0.0 && out > 0.0).then(|| (features(src, r.width as f64 * r.height as f64), out.ln() + moved))
            })
            .collect();
        if pts.is_empty() {
            return prior;
        }
        let mut a = [[0.0; 3]; 3];
        let mut rhs = [0.0; 3];
        for i in 0..3 {
            a[i][i] = PRIOR_WEIGHT;
            rhs[i] = PRIOR_WEIGHT * prior.beta[i];
        }
        for (x, y) in &pts {
            for i in 0..3 {
                for j in 0..3 {
                    a[i][j] += x[i] * x[j];
                }
                rhs[i] += x[i] * y;
            }
        }
        let beta = solve3(a, rhs).unwrap_or(prior.beta);
        let sse: f64 = pts.iter().map(|(x, y)| (y - dot(&beta, x)).powi(2)).sum();
        let n = pts.len() as f64;
        let sigma = ((sse + PRIOR_WEIGHT * PRIOR_SIGMA.powi(2)) / (n + PRIOR_WEIGHT)).sqrt().max(0.1);
        let seen = [1, 2].map(|k| {
            let mean = pts.iter().map(|(x, _)| x[k]).sum::<f64>() / n;
            (mean, (pts.iter().map(|(x, _)| (x[k] - mean).powi(2)).sum::<f64>() / n).sqrt())
        });
        Model { beta, sigma, samples: pts.len(), seen }
    }

    /// The spread to expect for a video with these features (log units): the history's own,
    /// and more the further the video is from what the history holds.
    fn sigma_for(&self, x: &[f64; 3]) -> f64 {
        let unlike = if self.samples < MIN_FOR_DISTANCE {
            1.0
        } else {
            let distance: f64 = [1, 2].iter().zip(self.seen).map(|(&k, (mean, sd))| ((x[k] - mean) / sd.max(MIN_HISTORY_SD)).powi(2)).sum();
            (distance / FAR_FROM_HISTORY).min(1.0)
        };
        (self.sigma.powi(2) + UNKNOWN_CONTENT_SIGMA.powi(2) * unlike).sqrt()
    }

    /// The spread to expect around the prediction for this video (log units).
    fn sigma_at(&self, source_bpp: f64, pixels: f64) -> f64 {
        self.sigma_for(&features(source_bpp.max(1e-4), pixels))
    }

    /// Predicted AV1 bits per pixel: (low, likely, high), the outer two an 80% interval.
    pub fn predict_bpp(&self, source_bpp: f64, pixels: f64) -> (f64, f64, f64) {
        let x = features(source_bpp.max(1e-4), pixels);
        let mid = dot(&self.beta, &x);
        let d = Z80 * self.sigma_for(&x);
        ((mid - d).exp(), mid.exp(), (mid + d).exp())
    }
}

/// Predicts the CRF that meets a VMAF target, so the CRF search can start near its answer:
/// crf = beta[0] + beta[1]·ln(source bpp / 0.1) + beta[2]·ln(pixels / 1080p).
/// A source that already spends few bits per pixel has lost its fine detail, and reaches the
/// same score at a higher CRF. The guess only has to land within a few steps: the search
/// measures from there.
#[derive(Debug, Clone, Copy)]
pub struct CrfModel {
    pub beta: [f64; 3],
    /// Encodes the model was fitted on.
    pub samples: usize,
}

/// CRF for VMAF 93 at 1080p from a 0.1 bpp source, and how it moves with the source's bits
/// per pixel and frame size. Flat: the measured curves of eight clips (see the tests of
/// `search`) cross 93 between CRF 28 and 39, 35 on average, and neither bits per pixel nor
/// frame size told one clip's crossing better than that average. Past encodes may still
/// teach the model a lean.
const CRF_PRIOR: [f64; 3] = [PRIOR_CRF_0, PRIOR_CRF_BPP, PRIOR_CRF_PIXELS];
const PRIOR_CRF_0: f64 = 35.0;
const PRIOR_CRF_BPP: f64 = 0.0;
const PRIOR_CRF_PIXELS: f64 = 0.0;
/// VMAF target the prior is for; other targets shift it by the usual slope.
const PRIOR_CRF_TARGET: f64 = 93.0;
/// Encodes for targets this far from the wanted one still count, shifted by the usual slope.
const CRF_TARGET_REACH: f64 = 3.0;

impl CrfModel {
    /// Ridge regression toward the prior, on past encodes made with the same SVT-AV1
    /// parameters for a nearby VMAF target.
    pub fn fit(records: &[Record], target_vmaf: f64, svt_params: &str) -> Self {
        let per_vmaf = 1.0 / crate::search::VMAF_PER_CRF;
        let mut prior = CRF_PRIOR;
        prior[0] -= (target_vmaf - PRIOR_CRF_TARGET) * per_vmaf;
        let pts: Vec<([f64; 3], f64)> = records
            .iter()
            .filter(|r| (r.target_vmaf - target_vmaf).abs() <= CRF_TARGET_REACH && r.svt_params.as_deref() == Some(svt_params))
            .filter_map(|r| {
                let src = bpp(r.source_bitrate, r.width, r.height, r.fps);
                let crf = r.crf? + (r.target_vmaf - target_vmaf) * per_vmaf;
                (src > 0.0).then(|| (features(src, r.width as f64 * r.height as f64), crf))
            })
            .collect();
        let mut a = [[0.0; 3]; 3];
        let mut rhs = [0.0; 3];
        for i in 0..3 {
            a[i][i] = PRIOR_WEIGHT;
            rhs[i] = PRIOR_WEIGHT * prior[i];
        }
        for (x, y) in &pts {
            for i in 0..3 {
                for j in 0..3 {
                    a[i][j] += x[i] * x[j];
                }
                rhs[i] += x[i] * y;
            }
        }
        CrfModel { beta: solve3(a, rhs).unwrap_or(prior), samples: pts.len() }
    }

    /// The CRF to try first for this video, within `min..=max`.
    pub fn start(&self, m: &MediaInfo, min: u8, max: u8) -> u8 {
        let guess = self.predict(bits_per_pixel(m), m.width as f64 * m.height as f64);
        (guess.round().clamp(0.0, 63.0) as u8).clamp(min, max.max(min))
    }

    /// The CRF expected to meet the target; a source of unknown bitrate counts as average.
    fn predict(&self, source_bpp: f64, pixels: f64) -> f64 {
        if source_bpp > 0.0 && pixels > 0.0 { dot(&self.beta, &features(source_bpp, pixels)) } else { self.beta[0] }
    }
}

/// Predicts how long a file takes to encode on this machine:
/// ln(seconds per frame) = beta[0] + beta[1]·ln(pixels / 1080p), for one preset.
/// Without history it is a guess from one machine (a 6-core Ryzen 5 5600X: 22 ms per 1080p
/// frame at preset 6, 9 ms at 720p, 62 ms at 4K, for the encode alone, and
/// [`CHECK_AND_REPAIR`] times that with what follows it); every whole-file encode recorded
/// here pulls it toward this machine's real speed.
#[derive(Debug, Clone, Copy)]
pub struct TimeModel {
    pub beta: [f64; 2],
    pub preset: u8,
    /// Encodes the model was fitted on.
    pub samples: usize,
    /// How many times as long a video takes with a CRF per scene: every chunk is cut into a
    /// lossless reference, probed two or three times, encoded and scored.
    pub scenes: f64,
    /// Per-scene encodes `scenes` was learned from.
    pub scene_samples: usize,
    /// The share of videos that get a CRF per scene where `--scenes=auto` decides.
    pub auto_share: f64,
    /// Encodes made with `--scenes=auto` that share was learned from.
    pub auto_samples: usize,
    /// What `secs` multiplies by: 1, `scenes` for the model [`TimeModel::per_scene`] gives,
    /// or what lies between for [`TimeModel::auto`].
    factor: f64,
}

/// Of 13 real recordings searched with eight samples of four seconds, five were uneven
/// enough for a CRF per scene.
const AUTO_SHARE: f64 = 1.0 / 3.0;

/// How many times the encode alone an encode takes with the check of the whole video and the
/// repairs that follow it, which `encode_secs` has held since they came: the middle of the 1.4
/// to 2.2 measured on whole runs of this build (2026-11-18), against 1.0 before. It stands in
/// for lines older than that, and for the first guess, until there are lines of this build.
const CHECK_AND_REPAIR: f64 = 1.7;
/// Lines of this build that are enough to go by alone.
const ENOUGH_CURRENT: usize = 3;
/// A line counts half as much for every this many newer lines, while lines of this build are
/// too few to go by alone.
const HALF_WEIGHT_AFTER: f64 = 5.0;

/// Six 15-minute 1080p recordings (2026-11-15) took 180 minutes per scene where one pass was
/// predicted, and for 55 others measured, at 58. Per-scene encodes have no check of the whole
/// video, so against a pass that now includes one they are the smaller multiple.
const SCENES_FACTOR: f64 = 3.0 / CHECK_AND_REPAIR;

const SPF_1080P: f64 = 0.022;
const SPF_EXPONENT: f64 = 0.85;
/// Encode time of each SVT-AV1 preset against preset 6, roughly (5, 4 and 3 as measured with
/// SVT-AV1 4.2 on 1080p: 28%, about 60% and 200% more processor time). Encodes made at another
/// preset still tell how fast the machine is once scaled by this.
const PRESET_TIME: [f64; 14] = [40.0, 20.0, 9.0, 3.0, 1.6, 1.3, 1.0, 0.75, 0.55, 0.45, 0.35, 0.3, 0.27, 0.25];
/// The CRF search costs about as much as encoding its sample clips this many times.
const SEARCH_ENCODES: f64 = 2.0;

/// Encodes shorter than this say little about seconds per frame.
const MIN_TIMED_SECS: f64 = 10.0;

fn preset_time(preset: u8) -> f64 {
    PRESET_TIME[(preset as usize).min(PRESET_TIME.len() - 1)]
}

impl TimeModel {
    pub fn fit(records: &[Record], preset: u8) -> Self {
        let prior = [(SPF_1080P * CHECK_AND_REPAIR * preset_time(preset)).ln(), SPF_EXPONENT];
        let timed: Vec<(&Record, f64, f64)> = records
            .iter()
            .filter(|r| r.mode == "whole")
            .filter_map(|r| {
                let frames = r.fps * r.duration_s;
                // Very short encodes are mostly start-up and muxing, not frames.
                let secs = r.encode_secs.filter(|s| *s >= MIN_TIMED_SECS)?;
                let pixels = r.width as f64 * r.height as f64;
                let per_frame = secs / frames * preset_time(preset) / preset_time(r.preset);
                (frames >= 1.0 && pixels > 0.0).then(|| (r, (pixels / PIXELS_1080P).ln(), per_frame.ln()))
            })
            .collect();
        let pts = time_points(&timed);
        // Ridge regression toward the prior, two unknowns.
        let (mut a, mut b, mut d) = (PRIOR_WEIGHT, 0.0, PRIOR_WEIGHT);
        let (mut r0, mut r1) = (PRIOR_WEIGHT * prior[0], PRIOR_WEIGHT * prior[1]);
        for (x, y, w) in &pts {
            a += w;
            b += w * x;
            d += w * x * x;
            r0 += w * y;
            r1 += w * x * y;
        }
        let det = a * d - b * b;
        let beta = if det.abs() > 1e-9 { [(r0 * d - b * r1) / det, (a * r1 - b * r0) / det] } else { prior };
        let whole = TimeModel { beta, preset, samples: pts.len(), scenes: SCENES_FACTOR, scene_samples: 0, auto_share: AUTO_SHARE, auto_samples: 0, factor: 1.0 };
        // Per-scene encodes against what one pass would have taken, search included on both
        // sides: the geometric mean, pulled toward the first guess like everything else here.
        let ratios: Vec<f64> = records
            .iter()
            .filter(|r| r.mode == "scenes")
            .filter_map(|r| {
                let took = r.search_secs? + r.encode_secs.filter(|s| *s >= MIN_TIMED_SECS)?;
                let info = MediaInfo { width: r.width, height: r.height, fps: r.fps, duration_s: r.duration_s, ..Default::default() };
                let one_pass = whole.secs(&info) * preset_time(r.preset) / preset_time(preset);
                (one_pass > 0.0).then(|| (took / one_pass).ln())
            })
            .collect();
        let scenes = ((PRIOR_WEIGHT * SCENES_FACTOR.ln() + ratios.iter().sum::<f64>()) / (PRIOR_WEIGHT + ratios.len() as f64)).exp();
        // How often "auto" ended in a CRF per scene, of the encodes that say they were made with it.
        let auto: Vec<bool> = records.iter().filter(|r| r.scenes.as_deref() == Some("auto")).map(|r| r.mode == "scenes").collect();
        let auto_share = (PRIOR_WEIGHT * AUTO_SHARE + auto.iter().filter(|s| **s).count() as f64) / (PRIOR_WEIGHT + auto.len() as f64);
        TimeModel { scenes, scene_samples: ratios.len(), auto_share, auto_samples: auto.len(), ..whole }
    }

    /// How many times one pass a video takes on average where `--scenes=auto` decides: one
    /// pass for most, a CRF per scene for the share that turn out uneven.
    pub fn auto_factor(&self) -> f64 {
        1.0 + self.auto_share * (self.scenes - 1.0)
    }

    /// The same model for encodes with `--scenes=auto`. Which videos get a CRF per scene
    /// shows only once their samples are scored, so each is given the average.
    #[cfg(any(feature = "ui", test))]
    pub fn auto(self) -> Self {
        TimeModel { factor: self.auto_factor(), ..self }
    }

    /// The same model for encodes that give every scene its own CRF.
    #[cfg(any(feature = "ui", test))]
    pub fn per_scene(self) -> Self {
        TimeModel { factor: self.scenes, ..self }
    }

    pub fn secs_per_frame(&self, pixels: f64) -> f64 {
        (self.beta[0] + self.beta[1] * (pixels / PIXELS_1080P).ln()).exp()
    }

    /// Seconds to search and encode this video; 0 when its size or length isn't known.
    pub fn secs(&self, m: &MediaInfo) -> f64 {
        let pixels = m.width as f64 * m.height as f64;
        let secs = m.picture_secs();
        if pixels <= 0.0 || m.fps <= 0.0 || secs <= 0.0 {
            return 0.0;
        }
        // What the search's sample clips add up to, unless the video is shorter.
        let clips = crate::search::mean_sample_count(secs) as f64 * crate::search::MEAN_SAMPLE_SECS;
        let searched = secs.min(clips) * SEARCH_ENCODES;
        self.secs_per_frame(pixels) * m.fps * (secs + searched) * self.factor
    }
}

/// What the time model is fitted on, as (x, y, weight), from (record, x, ln seconds per frame).
/// Enough lines of this build are used alone: older ones timed something else, and 20 new
/// lines among 60 old ones only moved the estimate 1.18 times where the truth was near 2.
/// Otherwise every line counts, the newer the more, and the ones from before the check of the
/// whole video are raised by what it adds.
fn time_points(timed: &[(&Record, f64, f64)]) -> Vec<(f64, f64, f64)> {
    let current = |r: &Record| r.timing_version == Some(TIMING_VERSION);
    if timed.iter().filter(|(r, ..)| current(r)).count() >= ENOUGH_CURRENT {
        return timed.iter().filter(|(r, ..)| current(r)).map(|(_, x, y)| (*x, *y, 1.0)).collect();
    }
    // Newest first; lines of the same second count as written, the later one newer.
    let mut newest: Vec<usize> = (0..timed.len()).collect();
    newest.sort_by_key(|&i| (std::cmp::Reverse(timed[i].0.time), std::cmp::Reverse(i)));
    newest
        .iter()
        .enumerate()
        .map(|(newer, &i)| {
            let (r, x, y) = timed[i];
            let raised = if r.timing_version.is_none() { CHECK_AND_REPAIR.ln() } else { 0.0 };
            (x, y + raised, 0.5_f64.powf(newer as f64 / HALF_WEIGHT_AFTER))
        })
        .collect()
}

fn dot(a: &[f64; 3], b: &[f64; 3]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Gaussian elimination with partial pivoting; None if singular.
fn solve3(mut a: [[f64; 3]; 3], mut b: [f64; 3]) -> Option<[f64; 3]> {
    for col in 0..3 {
        let piv = (col..3).max_by(|&i, &j| a[i][col].abs().total_cmp(&a[j][col].abs()))?;
        if a[piv][col].abs() < 1e-12 {
            return None;
        }
        a.swap(col, piv);
        b.swap(col, piv);
        let (pivot_row, pivot_b) = (a[col], b[col]);
        for row in col + 1..3 {
            let f = a[row][col] / pivot_row[col];
            for (x, p) in a[row].iter_mut().zip(pivot_row).skip(col) {
                *x -= f * p;
            }
            b[row] -= f * pivot_b;
        }
    }
    let mut x = [0.0; 3];
    for row in (0..3).rev() {
        let s: f64 = (row + 1..3).map(|k| a[row][k] * x[k]).sum();
        x[row] = (b[row] - s) / a[row][row];
    }
    Some(x)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub enum Verdict {
    Candidate,
    /// Expected to save too little, but the kind end of the estimate would be enough: the
    /// search will tell, and it comes after the sure ones.
    Maybe,
    AlreadyAv1,
    LowGain,
    DolbyVision,
    TooShort,
    Unreadable,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Candidate => "candidate",
            Verdict::Maybe => "may save too little: tried last",
            Verdict::AlreadyAv1 => "skip: already AV1",
            Verdict::LowGain => "skip: low expected gain",
            Verdict::DolbyVision => "skip: Dolby Vision",
            Verdict::TooShort => "skip: too short or no duration",
            Verdict::Unreadable => "skip: unreadable",
        }
    }

    /// Whether a file with this verdict is worth the search that settles it.
    pub fn will_be_tried(self) -> bool {
        matches!(self, Verdict::Candidate | Verdict::Maybe)
    }
}

/// Estimated video stream size after re-encoding, in bytes: (optimistic, likely, pessimistic).
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct SizeRange {
    pub low: f64,
    pub likely: f64,
    pub high: f64,
    /// The chance that the video saves enough to be kept, 0 to 1; 0 when it was not judged.
    pub chance: f64,
}

impl SizeRange {
    fn unchanged(file_size: u64) -> SizeRange {
        let size = file_size as f64;
        SizeRange { low: size, likely: size, high: size, chance: 0.0 }
    }

    /// How much of its expected saving counts when files are put in order. A file that fails
    /// the line still cost its search, so per hour spent it brings
    /// `P / (P + COST_OF_A_SKIP * (1 - P))` of what a sure one would.
    pub fn order_weight(&self) -> f64 {
        let p = self.chance;
        if p > 0.0 { p / (p + COST_OF_A_SKIP * (1.0 - p)) } else { 0.0 }
    }

    /// The bytes this file is expected to save, weighted by its chance of being kept.
    pub fn weighted_saving(&self, file_size: u64) -> f64 {
        (file_size as f64 - self.likely) * self.order_weight()
    }
}

/// What a number of files are expected to save together, with what can be said of its range.
#[derive(Debug, Clone, Copy, Default)]
pub struct SavingsTotal {
    pub likely: f64,
    /// The sum of each file's variance, in bytes squared.
    variance: f64,
}

impl SavingsTotal {
    pub fn add(&mut self, file_size: u64, estimate: &SizeRange) {
        self.likely += file_size as f64 - estimate.likely;
        // A file's range is 1.28 standard deviations either side.
        self.variance += ((estimate.high - estimate.low) / 2.0 / Z80).powi(2);
    }

    /// The 80% range of the total, least first. The errors of single files mostly cancel out,
    /// so adding up their ranges said "98.6 to 415.8 GB" of one library. What does not cancel
    /// is the model being off for all of them at once, which it knows only as well as its
    /// history lets it.
    pub fn range(&self, model: &Model) -> (f64, f64) {
        let shared = model.sigma / (model.samples as f64 + 3.0).sqrt() * self.likely;
        let half = Z80 * (self.variance + shared.powi(2)).sqrt();
        ((self.likely - half).max(0.0), self.likely + half)
    }
}

/// The share of a normal distribution below `z`, from the complementary error function
/// (Numerical Recipes' fit, good to about one part in ten million).
fn normal_share_below(z: f64) -> f64 {
    let x = z.abs() / std::f64::consts::SQRT_2;
    let t = 1.0 / (1.0 + 0.5 * x);
    let fit = [0.17087277, -0.82215223, 1.48851587, -1.13520398, 0.27886807, -0.18628806, 0.09678418, 0.37409196, 1.00002368, -1.26551223];
    let beyond = t * (fit.iter().fold(0.0, |sum, c| sum * t + c) - x * x).exp() / 2.0;
    if z > 0.0 { 1.0 - beyond } else { beyond }
}

pub fn bits_per_pixel(m: &MediaInfo) -> f64 {
    bpp(m.video_bitrate, m.width, m.height, m.fps)
}

/// Returns the verdict and the estimated new size of the *whole file* in bytes.
pub fn judge(m: &MediaInfo, file_size: u64, min_savings: f64, model: &Model) -> (Verdict, SizeRange) {
    let keep = SizeRange::unchanged(file_size);
    if m.codec == "av1" {
        return (Verdict::AlreadyAv1, keep);
    }
    if m.picture_secs() < MIN_PICTURE_SECS || m.width == 0 || m.fps <= 0.0 {
        return (Verdict::TooShort, keep);
    }
    if m.dolby_vision {
        return (Verdict::DolbyVision, keep);
    }

    let pixels = m.width as f64 * m.height as f64;
    let (low, likely, high) = model.predict_bpp(bits_per_pixel(m), pixels);
    let video_bytes = m.video_bitrate * m.picture_secs() / 8.0;
    let other_bytes = (file_size as f64 - video_bytes).max(0.0);
    let av1_bytes = |bpp: f64| bpp * pixels * m.fps * m.picture_secs() / 8.0;
    let new_size = |bpp: f64| other_bytes + av1_bytes(bpp).min(video_bytes);
    let mut est = SizeRange { low: new_size(low), likely: new_size(likely), high: new_size(high), chance: 0.0 };

    // The line is about the video stream, as it is in `encode`. A bitrate that claims more
    // than the file holds says nothing about the stream, so the file's size caps it.
    let ratio = av1_bytes(likely) / video_bytes.min(file_size as f64);
    if !(ratio.is_finite() && ratio > 0.0) {
        return (Verdict::LowGain, est);
    }
    let sigma = model.sigma_at(bits_per_pixel(m), pixels);
    let enough = 1.0 - min_savings;
    est.chance = normal_share_below((enough.ln() - ratio.ln()) / sigma.max(1e-9));
    let verdict = if ratio <= enough {
        Verdict::Candidate
    } else if ratio * (-Z80 * sigma).exp() <= enough {
        Verdict::Maybe
    } else {
        Verdict::LowGain
    };
    (verdict, est)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(src_bpp: f64, out_bpp: f64, w: u32, h: u32) -> Record {
        let px = w as f64 * h as f64 * 30.0;
        Record {
            format: 1,
            time: 0, codec: "h264".into(), width: w, height: h, fps: 30.0, duration_s: 60.0,
            source_bitrate: src_bpp * px, output_bitrate: out_bpp * px, target_vmaf: 93.0, preset: 6,
            mode: "whole".into(), crf: None, svt_params: None, search_secs: None, encode_secs: None, scenes: None,
            version: None, timing_version: None, check_secs: None,
        }
    }

    /// The model for the preset of the records these tests make.
    fn fit(records: &[Record], target: f64) -> Model {
        Model::fit_at_preset(records, target, 6)
    }

    /// The first guess of seconds per 1080p frame at preset 6.
    const GUESS_1080P: f64 = SPF_1080P * CHECK_AND_REPAIR;

    #[test]
    fn no_history_gives_prior() {
        let m = fit(&[], 93.0);
        assert_eq!(m.samples, 0);
        let (lo, mid, hi) = m.predict_bpp(0.1, PIXELS_1080P);
        assert!((mid - BPP_1080P).abs() < 1e-9 && lo < mid && mid < hi);
    }

    #[test]
    fn a_lean_60_fps_source_is_still_a_candidate_without_history() {
        // 1080p60 at 5 Mbps: 0.04 bits per pixel, already near what AV1 needs at 30 fps.
        let m = MediaInfo { codec: "h264".into(), width: 1920, height: 1080, fps: 60.0, duration_s: 900.0, video_bitrate: 5e6, ..Default::default() };
        let size = (5.13e6 * 900.0 / 8.0) as u64;
        let (verdict, range) = judge(&m, size, 0.2, &Model::prior());
        assert_eq!(verdict, Verdict::Candidate);
        let saved = 1.0 - range.likely / size as f64;
        assert!((0.2..0.4).contains(&saved), "{saved}");
        // The same picture at 30 fps and 5 Mbps spends twice the bits per pixel: more to save.
        let (_, slower) = judge(&MediaInfo { fps: 30.0, ..m }, size, 0.2, &Model::prior());
        assert!(slower.likely < range.likely);
    }

    #[test]
    fn learns_from_many_encodes() {
        // Truth: av1 bpp = 0.25 * source bpp^0.8 * (pixels/1080p)^-0.3, with +-10% noise.
        let truth = |s: f64, px: f64| 0.25 * s.powf(0.8) * (px / PIXELS_1080P).powf(-0.3);
        let sizes = [(1280, 720), (1920, 1080), (3840, 2160), (854, 480)];
        let recs: Vec<Record> = (0..200)
            .map(|i| {
                let (w, h) = sizes[i % 4];
                let s = 0.02 + 0.3 * ((i * 37 % 100) as f64 / 100.0);
                let noise = 1.0 + 0.1 * (((i * 53) % 21) as f64 / 10.0 - 1.0);
                rec(s, truth(s, w as f64 * h as f64) * noise, w, h)
            })
            .collect();
        let m = fit(&recs, 93.0);
        assert!((m.beta[1] - 0.8).abs() < 0.05, "{:?}", m.beta);
        assert!((m.beta[2] + 0.3).abs() < 0.05, "{:?}", m.beta);
        assert!(m.sigma < 0.15, "sigma {}", m.sigma);
        let (_, mid, _) = m.predict_bpp(0.15, PIXELS_1080P);
        assert!((mid / truth(0.15, PIXELS_1080P) - 1.0).abs() < 0.05);
    }

    #[test]
    fn few_encodes_only_nudge_the_prior() {
        let m = fit(&[rec(0.1, 0.07, 1920, 1080)], 93.0);
        let (_, mid, _) = m.predict_bpp(0.1, PIXELS_1080P);
        assert!(mid > BPP_1080P && mid < 0.07, "{mid}");
    }

    fn crf_rec(src_bpp: f64, w: u32, h: u32, crf: f64) -> Record {
        Record { crf: Some(crf), svt_params: Some("tune=0".into()), ..rec(src_bpp, src_bpp / 3.0, w, h) }
    }

    #[test]
    fn first_crf_without_history_is_the_prior() {
        let m = CrfModel::fit(&[], 93.0, "tune=0");
        assert_eq!(m.samples, 0);
        assert!((m.predict(SOURCE_BPP_REF, PIXELS_1080P) - PRIOR_CRF_0).abs() < 1e-9);
        // A stricter target starts lower, by the usual VMAF per CRF step.
        let strict = CrfModel::fit(&[], 95.0, "tune=0");
        let shift = 2.0 / crate::search::VMAF_PER_CRF;
        assert!((m.predict(0.1, PIXELS_1080P) - strict.predict(0.1, PIXELS_1080P) - shift).abs() < 1e-9);
        // Unknown bitrate: the average video. Always inside the allowed range.
        assert!((m.predict(0.0, PIXELS_1080P) - PRIOR_CRF_0).abs() < 1e-9);
        let info = |bitrate: f64| MediaInfo { width: 1920, height: 1080, fps: 30.0, video_bitrate: bitrate, ..Default::default() };
        assert_eq!(m.start(&info(0.0), 20, 55), PRIOR_CRF_0.round() as u8);
        assert_eq!(m.start(&info(6e6), 50, 55), 50);
        assert_eq!(m.start(&info(6e6), 20, 24), 24);
    }

    #[test]
    fn first_crf_learns_from_past_encodes() {
        // Truth: crf = 41 - 4·ln(bpp/0.1) - 2·ln(pixels/1080p), within half a step.
        let truth = |s: f64, px: f64| 41.0 - 4.0 * (s / 0.1).ln() - 2.0 * (px / PIXELS_1080P).ln();
        let sizes = [(1280, 720), (1920, 1080), (3840, 2160), (854, 480)];
        let recs: Vec<Record> = (0..120)
            .map(|i| {
                let (w, h) = sizes[i % 4];
                let s = 0.02 + 0.3 * ((i * 37 % 100) as f64 / 100.0);
                let noise = ((i * 53) % 21) as f64 / 20.0 - 0.5;
                crf_rec(s, w, h, truth(s, w as f64 * h as f64) + noise)
            })
            .collect();
        let m = CrfModel::fit(&recs, 93.0, "tune=0");
        assert_eq!(m.samples, 120);
        for (s, px) in [(0.05, PIXELS_1080P), (0.2, 1280.0 * 720.0), (0.1, 3840.0 * 2160.0)] {
            assert!((m.predict(s, px) - truth(s, px)).abs() < 0.75, "{s} {px}: {} vs {}", m.predict(s, px), truth(s, px));
        }
        // Encodes for a nearby target count, moved by the usual slope; far ones don't.
        let near = CrfModel::fit(&recs, 95.0, "tune=0");
        assert_eq!(near.samples, 120);
        let shift = 2.0 / crate::search::VMAF_PER_CRF;
        assert!((m.predict(0.1, PIXELS_1080P) - near.predict(0.1, PIXELS_1080P) - shift).abs() < 0.5);
        assert_eq!(CrfModel::fit(&recs, 97.0, "tune=0").samples, 0);
        // Other encoder parameters reach the score at other CRFs: not counted. Nor are
        // lines from before the CRF was recorded.
        assert_eq!(CrfModel::fit(&recs, 93.0, "tune=0:enable-qm=1").samples, 0);
        assert_eq!(CrfModel::fit(&[rec(0.1, 0.03, 1920, 1080)], 93.0, "tune=0").samples, 0);
        // One encode only nudges the first guess.
        let one = CrfModel::fit(&[crf_rec(0.1, 1920, 1080, PRIOR_CRF_0 + 8.0)], 93.0, "tune=0");
        let p = one.predict(0.1, PIXELS_1080P);
        assert!(p > PRIOR_CRF_0 && p < PRIOR_CRF_0 + 4.0, "{p}");
    }

    fn timed(w: u32, h: u32, preset: u8, per_frame: f64) -> Record {
        let mut r = rec(0.1, 0.03, w, h);
        r.duration_s = 600.0;
        (r.preset, r.encode_secs, r.timing_version) = (preset, Some(per_frame * r.fps * r.duration_s), Some(TIMING_VERSION));
        r
    }

    /// The same, as a line from before timings had a version.
    fn timed_long_ago(w: u32, h: u32, preset: u8, per_frame: f64) -> Record {
        Record { timing_version: None, ..timed(w, h, preset, per_frame) }
    }

    #[test]
    fn encode_time_starts_as_a_guess_and_learns_the_machine() {
        let info = |w: u32, h: u32, secs: f64| MediaInfo { width: w, height: h, fps: 30.0, duration_s: secs, ..Default::default() };
        let guess = TimeModel::fit(&[], 6);
        assert_eq!(guess.samples, 0);
        assert!((guess.secs_per_frame(PIXELS_1080P) - GUESS_1080P).abs() < 1e-12);
        // Bigger frames and slower presets take longer; an hour of 1080p is hours, not minutes.
        assert!(guess.secs_per_frame(3840.0 * 2160.0) > 2.5 * GUESS_1080P);
        assert!(TimeModel::fit(&[], 4).secs_per_frame(PIXELS_1080P) > 1.5 * GUESS_1080P);
        assert!(TimeModel::fit(&[], 8).secs_per_frame(PIXELS_1080P) < 0.7 * GUESS_1080P);
        let hour = guess.secs(&info(1920, 1080, 3600.0));
        // 64 sample clips of a second, encoded about twice.
        assert!((hour / (GUESS_1080P * 30.0 * 3600.0) - 1.0 - 128.0 / 3600.0).abs() < 1e-9, "{hour}");
        // A short video is searched on all of it: the search outweighs the encode.
        let short = guess.secs(&info(1920, 1080, 10.0));
        assert!((short / (GUESS_1080P * 300.0) - 3.0).abs() < 1e-9, "{short}");
        assert_eq!(guess.secs(&info(0, 0, 60.0)), 0.0);
        assert_eq!(guess.secs(&info(1920, 1080, 0.0)), 0.0);

        // A machine three times faster, seen through encodes at two presets and four sizes.
        let truth = |px: f64| GUESS_1080P / 3.0 * (px / PIXELS_1080P).powf(0.7);
        let sizes = [(1280, 720), (1920, 1080), (3840, 2160), (854, 480)];
        let recs: Vec<Record> = (0..40)
            .map(|i| {
                let (w, h) = sizes[i % 4];
                let preset = if i % 3 == 0 { 8 } else { 6 };
                let noise = 1.0 + 0.1 * (((i * 53) % 21) as f64 / 10.0 - 1.0);
                timed(w, h, preset, truth(w as f64 * h as f64) * preset_time(preset) * noise)
            })
            .collect();
        let m = TimeModel::fit(&recs, 6);
        assert_eq!(m.samples, 40);
        for (w, h) in sizes {
            let px = w as f64 * h as f64;
            assert!((m.secs_per_frame(px) / truth(px) - 1.0).abs() < 0.12, "{w}x{h}: {} vs {}", m.secs_per_frame(px), truth(px));
        }
        // Asked about another preset, the same encodes answer scaled.
        let slow = TimeModel::fit(&recs, 4);
        assert!((slow.secs_per_frame(PIXELS_1080P) / m.secs_per_frame(PIXELS_1080P) - preset_time(4)).abs() < 1e-6);
        // Scene-mode encodes, cuts and lines without a time don't count; one encode only nudges.
        let mut scene = timed(1920, 1080, 6, 0.001);
        scene.mode = "scenes".into();
        assert_eq!(TimeModel::fit(&[scene, rec(0.1, 0.03, 1920, 1080)], 6).samples, 0);
        let mut blink = timed(1920, 1080, 6, 0.2);
        (blink.duration_s, blink.encode_secs) = (1.0, Some(6.0));
        assert_eq!(TimeModel::fit(&[blink], 6).samples, 0);
        let one = TimeModel::fit(&[timed(1920, 1080, 6, GUESS_1080P / 4.0)], 6).secs_per_frame(PIXELS_1080P);
        assert!(one < GUESS_1080P && one > GUESS_1080P / 2.0, "{one}");
    }

    #[test]
    fn lines_of_this_build_are_gone_by_alone_once_there_are_three() {
        let at = |recs: &[Record]| TimeModel::fit(recs, 6).secs_per_frame(PIXELS_1080P);
        // Sixty old lines say 20 ms a frame, three new ones 40 ms.
        let mut recs = vec![timed_long_ago(1920, 1080, 6, 0.02); 60];
        recs.extend(vec![timed(1920, 1080, 6, 0.04); 3]);
        let m = TimeModel::fit(&recs, 6);
        assert_eq!(m.samples, 3);
        // Pulled toward the first guess as usual, and far from what the old lines say.
        let alone = at(&vec![timed(1920, 1080, 6, 0.04); 3]);
        assert!(near(m.secs_per_frame(PIXELS_1080P), alone), "{}", m.secs_per_frame(PIXELS_1080P));
        // A line of another timing version is not of this build.
        let other = Record { timing_version: Some(TIMING_VERSION + 1), ..timed(1920, 1080, 6, 0.04) };
        recs.truncate(60);
        recs.extend(vec![other; 3]);
        assert_eq!(TimeModel::fit(&recs, 6).samples, 63);
    }

    #[test]
    fn with_few_lines_of_this_build_old_ones_count_raised_and_less_the_older_they_are() {
        // Old lines alone: what they say, times what the check and repairs add. Their weights
        // add up to about eight lines, so the first guess still has a say.
        let many = vec![timed_long_ago(1920, 1080, 6, 0.02); 200];
        let m = TimeModel::fit(&many, 6);
        assert!((m.secs_per_frame(PIXELS_1080P) / (0.02 * CHECK_AND_REPAIR) - 1.0).abs() < 0.05, "{}", m.secs_per_frame(PIXELS_1080P));
        // The newest lines weigh most: the same lines in the other order give another answer,
        // and the time written in them decides what is new, not the place in the file.
        let dated = |time, per_frame| Record { time, ..timed_long_ago(1920, 1080, 6, per_frame) };
        let (slow_last, slow_first): (Vec<Record>, Vec<Record>) =
            ((0..20).map(|i| dated(i, if i < 10 { 0.01 } else { 0.04 })).collect(), (0..20).map(|i| dated(i, if i < 10 { 0.04 } else { 0.01 })).collect());
        let at = |recs: &[Record]| TimeModel::fit(recs, 6).secs_per_frame(PIXELS_1080P);
        assert!(at(&slow_last) > 1.5 * at(&slow_first), "{} {}", at(&slow_last), at(&slow_first));
        let mut shuffled = slow_last.clone();
        shuffled.reverse();
        assert!(near(at(&shuffled), at(&slow_last)));
        // Weights halve every five lines, newest first.
        let lines: Vec<Record> = (0..6).map(|i| dated(i, 0.02)).collect();
        let timed_lines: Vec<(&Record, f64, f64)> = lines.iter().map(|r| (r, 0.0, 0.0)).collect();
        let weights: Vec<f64> = time_points(&timed_lines).iter().map(|p| p.2).collect();
        assert!(near(weights[0], 1.0) && near(weights[5], 0.5), "{weights:?}");
        // Two lines of this build are too few, and are not raised.
        let two = vec![timed(1920, 1080, 6, 0.02); 2];
        let points: Vec<(&Record, f64, f64)> = two.iter().map(|r| (r, 0.0, 0.0)).collect();
        assert!(time_points(&points).iter().all(|p| p.1 == 0.0));
    }

    #[test]
    fn a_crf_per_scene_costs_its_own_multiple_of_one_pass() {
        let info = MediaInfo { width: 1920, height: 1080, fps: 30.0, duration_s: 900.0, ..Default::default() };
        let guess = TimeModel::fit(&[], 6);
        assert!((guess.scenes - SCENES_FACTOR).abs() < 1e-9 && guess.scene_samples == 0);
        assert!((guess.per_scene().secs(&info) / guess.secs(&info) - SCENES_FACTOR).abs() < 1e-9);
        // Per-scene encodes that took twice one pass, search and encode together, at two presets.
        let took = |preset: u8| {
            let mut r = rec(0.1, 0.03, 1920, 1080);
            let one_pass = TimeModel::fit(&[], preset).secs(&MediaInfo { duration_s: r.duration_s, ..info.clone() });
            (r.mode, r.preset, r.search_secs, r.encode_secs) = ("scenes".into(), preset, Some(one_pass * 0.5), Some(one_pass * 1.5));
            r
        };
        let few = TimeModel::fit(&[took(6)], 6);
        assert!(few.scenes > SCENES_FACTOR && few.scenes < 2.0, "{}", few.scenes);
        let many: Vec<Record> = (0..60).map(|i| took(if i % 2 == 0 { 6 } else { 8 })).collect();
        let learned = TimeModel::fit(&many, 6);
        assert_eq!(learned.scene_samples, 60);
        assert!((learned.scenes - 2.0).abs() < 0.05, "{}", learned.scenes);
        // They say nothing about the speed of one pass.
        assert_eq!(learned.samples, 0);
        assert_eq!(learned.secs(&info), guess.secs(&info));
    }

    #[test]
    fn auto_costs_what_its_share_of_per_scene_encodes_costs() {
        let info = MediaInfo { width: 1920, height: 1080, fps: 30.0, duration_s: 900.0, ..Default::default() };
        // A third per scene at their multiple of one pass, before anything is known.
        let guess = TimeModel::fit(&[], 6);
        assert_eq!(guess.auto_samples, 0);
        assert!((guess.auto_factor() - (1.0 + (SCENES_FACTOR - 1.0) / 3.0)).abs() < 1e-9);
        assert!((guess.auto().secs(&info) / guess.secs(&info) - guess.auto_factor()).abs() < 1e-9);
        // 60 encodes made with "auto", every tenth per scene; others don't count, whatever their mode.
        let made = |setting: &str, mode: &str| Record { scenes: Some(setting.into()), mode: mode.into(), search_secs: None, encode_secs: None, ..rec(0.1, 0.03, 1920, 1080) };
        let mut recs: Vec<Record> = (0..60).map(|i| made("auto", if i % 10 == 0 { "scenes" } else { "whole" })).collect();
        recs.extend((0..40).map(|_| made("always", "scenes")));
        recs.extend((0..40).map(|_| made("off", "whole")));
        recs.push(Record { mode: "scenes".into(), ..rec(0.1, 0.03, 1920, 1080) });
        let learned = TimeModel::fit(&recs, 6);
        assert_eq!(learned.auto_samples, 60);
        assert!((learned.auto_share - 7.0 / 63.0).abs() < 1e-9, "{}", learned.auto_share);
        // The search's clips are as many for a longer video: they weigh less on it.
        let long = MediaInfo { duration_s: 3600.0, ..info.clone() };
        let per_sec = |m: &MediaInfo| guess.secs(m) / m.duration_s;
        assert!((per_sec(&info) - guess.secs_per_frame(PIXELS_1080P) * 30.0 * (900.0 + 128.0) / 900.0).abs() < 1e-9);
        assert!((per_sec(&long) - guess.secs_per_frame(PIXELS_1080P) * 30.0 * (3600.0 + 128.0) / 3600.0).abs() < 1e-9);
    }

    #[test]
    fn ignores_other_vmaf_targets() {
        let mut r = rec(0.1, 0.07, 1920, 1080);
        r.target_vmaf = 97.0;
        assert_eq!(fit(&[r], 93.0).samples, 0);
    }

    fn video(secs: f64, bitrate: f64) -> MediaInfo {
        MediaInfo { codec: "h264".into(), width: 1000, height: 1000, fps: 10.0, duration_s: secs, video_bitrate: bitrate, ..Default::default() }
    }

    /// A model that says `bpp` whatever the source.
    fn says(bpp: f64) -> Model {
        Model { beta: [bpp.ln(), 0.0, 0.0], sigma: 0.1, samples: 0, seen: [(0.0, 0.0); 2] }
    }

    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-6 * b.abs().max(1.0)
    }

    #[test]
    fn bits_per_pixel_are_nothing_when_the_frames_are_unknown() {
        assert_eq!(bpp(1e6, 1000, 1000, 10.0), 0.1);
        assert_eq!(bpp(1e6, 0, 1000, 10.0), 0.0);
        assert_eq!(bpp(1e6, 1000, 0, 10.0), 0.0);
        assert_eq!(bpp(1e6, 1000, 1000, 0.0), 0.0);
        assert_eq!(bpp(1e6, 1000, 1000, f64::NAN), 0.0);
    }

    #[test]
    fn what_cannot_be_encoded_is_said_before_what_it_would_save() {
        let size = 12_500_000;
        let verdict = |m: &MediaInfo| judge(m, size, 0.2, &says(0.01)).0;
        let av1 = MediaInfo { codec: "av1".into(), dolby_vision: true, duration_s: 1.0, ..video(100.0, 1e6) };
        assert_eq!(verdict(&av1), Verdict::AlreadyAv1);
        let short_dolby = MediaInfo { dolby_vision: true, ..video(0.5, 1e6) };
        assert_eq!(verdict(&short_dolby), Verdict::TooShort);
        let dolby = MediaInfo { dolby_vision: true, ..video(100.0, 1e6) };
        assert_eq!(verdict(&dolby), Verdict::DolbyVision);
        assert_eq!(verdict(&video(100.0, 1e6)), Verdict::Candidate);
        // A file that is skipped stays the size it is.
        let (_, r) = judge(&dolby, size, 0.2, &says(0.01));
        assert_eq!((r.low, r.likely, r.high), (size as f64, size as f64, size as f64));
    }

    #[test]
    fn a_video_is_too_short_below_the_second_an_encode_asks_for() {
        let verdict = |m: &MediaInfo| judge(m, 625_000, 0.2, &says(0.001)).0;
        assert_eq!(verdict(&video(1.0, 1e6)), Verdict::Candidate);
        assert_eq!(verdict(&video(0.99, 1e6)), Verdict::TooShort);
        // Sound that runs on does not make the picture longer.
        assert_eq!(verdict(&MediaInfo { video_s: 0.5, ..video(100.0, 1e6) }), Verdict::TooShort);
        assert_eq!(verdict(&MediaInfo { width: 0, ..video(100.0, 1e6) }), Verdict::TooShort);
        assert_eq!(verdict(&MediaInfo { fps: 0.0, ..video(100.0, 1e6) }), Verdict::TooShort);
    }

    #[test]
    fn savings_of_exactly_the_minimum_are_enough() {
        let (m, size, model) = (video(100.0, 1e6), 12_500_000, says(0.05));
        let (_, r) = judge(&m, size, 0.2, &model);
        let saved = 1.0 - r.likely / size as f64;
        assert!(near(saved, 0.5), "{saved}");
        assert_eq!(judge(&m, size, saved, &model).0, Verdict::Candidate);
        assert_eq!(judge(&m, size, saved + 1e-9, &model).0, Verdict::Maybe);
    }

    /// The spread `says` gives a video: its own 0.1 and all of what unknown content adds.
    fn spread_of_says() -> f64 {
        (0.1f64.powi(2) + UNKNOWN_CONTENT_SIGMA.powi(2)).sqrt()
    }

    #[test]
    fn the_likely_size_makes_a_candidate_and_the_kind_end_of_the_range_a_maybe() {
        // The source spends 0.1 bits per pixel, so what the model says over 0.1 is the ratio.
        let verdict = |ratio: f64| judge(&video(100.0, 1e6), 12_500_000, MIN_SAVINGS, &says(ratio * 0.1)).0;
        let last_maybe = (1.0 - MIN_SAVINGS) * (Z80 * spread_of_says()).exp();
        assert_eq!(verdict(0.5), Verdict::Candidate);
        assert_eq!(verdict(0.849_99), Verdict::Candidate);
        assert_eq!(verdict(0.850_01), Verdict::Maybe);
        assert_eq!(verdict(last_maybe * 0.999_99), Verdict::Maybe);
        assert_eq!(verdict(last_maybe * 1.000_01), Verdict::LowGain);
    }

    #[test]
    fn the_line_is_about_the_video_stream_and_not_the_whole_file() {
        // Half the file is sound: the picture shrinks by 20%, the file by 10%.
        let (m, model) = (video(100.0, 1e6), says(0.08));
        assert_eq!(judge(&m, 25_000_000, MIN_SAVINGS, &model).0, Verdict::Candidate);
        // A bitrate claiming twice what the file holds: half of the claim is no saving at all.
        assert_eq!(judge(&m, 6_250_000, MIN_SAVINGS, &says(0.05)).0, Verdict::Maybe);
    }

    #[test]
    fn the_range_of_a_total_is_narrower_than_the_ranges_added_up() {
        // Files of 1000 expected to come out at 500, give or take 128.
        let file = SizeRange { low: 372.0, likely: 500.0, high: 628.0, chance: 1.0 };
        let total = |files: usize| {
            let mut t = SavingsTotal::default();
            (0..files).for_each(|_| t.add(1000, &file));
            t
        };
        let settled = Model { sigma: 0.25, samples: 97, ..Model::prior() };
        // sqrt(100 * 100^2 + (0.025 * 50000)^2) = 1600.8, times 1.28.
        let t = total(100);
        let (low, high) = t.range(&settled);
        assert_eq!(t.likely, 50_000.0);
        assert!((high - 50_000.0 - 2049.0).abs() < 1.0, "{high}");
        assert!((50_000.0 - low - 2049.0).abs() < 1.0, "{low}");
        assert!(high - 50_000.0 < 12_800.0 / 6.0, "the ranges added up would be 12800 either side");
        // With little history the error all files share is most of it, and a range reaches
        // no lower than nothing saved.
        let (low, high) = t.range(&Model { sigma: 3.0, samples: 0, ..Model::prior() });
        assert_eq!(low, 0.0);
        assert!(high > 150_000.0, "{high}");
        // Of one file with a well known model, the total's range is the file's own.
        let (low, high) = total(1).range(&Model { sigma: 0.01, samples: 10_000, ..Model::prior() });
        assert!((low - 372.0).abs() < 0.5 && (high - 628.0).abs() < 0.5, "{low} {high}");
        assert_eq!(SavingsTotal::default().range(&settled), (0.0, 0.0));
    }

    #[test]
    fn the_share_below_a_normal_value_is_the_known_one() {
        for (z, share) in [(0.0, 0.5), (1.0, 0.841_345), (1.28, 0.899_727), (1.96, 0.975_002), (-1.28, 0.100_273), (-3.0, 0.001_350)] {
            assert!((normal_share_below(z) - share).abs() < 1e-5, "{z}: {}", normal_share_below(z));
        }
        assert_eq!(normal_share_below(f64::NEG_INFINITY), 0.0);
        assert_eq!(normal_share_below(f64::INFINITY), 1.0);
    }

    #[test]
    fn the_chance_of_being_kept_puts_a_likely_file_before_a_doubtful_one() {
        let judged = |ratio: f64| judge(&video(100.0, 1e6), 12_500_000, MIN_SAVINGS, &says(ratio * 0.1));
        // On the line it is one chance in two, and at the kind end of the range one in ten.
        assert!((judged(0.85).1.chance - 0.5).abs() < 1e-3);
        let (verdict, at_the_end) = judged((1.0 - MIN_SAVINGS) * (Z80 * spread_of_says()).exp() * 0.999);
        assert_eq!(verdict, Verdict::Maybe);
        assert!((at_the_end.chance - 0.1).abs() < 2e-3, "{}", at_the_end.chance);
        assert!((at_the_end.order_weight() - 0.526).abs() < 0.01, "{}", at_the_end.order_weight());
        assert!(judged(0.2).1.order_weight() > 0.999);
        // A file saving more than another comes after it when its saving is unlikely to hold.
        let sure = SizeRange { likely: 500.0, chance: 0.99, ..Default::default() };
        let doubtful = SizeRange { likely: 400.0, chance: 0.2, ..Default::default() };
        assert!(1000.0 - doubtful.likely > 1000.0 - sure.likely);
        assert!(doubtful.weighted_saving(1000) < sure.weighted_saving(1000));
        // What was not judged counts for nothing.
        assert_eq!(SizeRange::unchanged(1000).order_weight(), 0.0);
    }

    #[test]
    fn only_the_video_stream_of_a_file_gets_smaller() {
        // 12.5 MB of picture and 5 MB of sound; the picture halves.
        let (_, r) = judge(&video(100.0, 1e6), 17_500_000, 0.2, &says(0.05));
        assert!(near(r.likely, 11_250_000.0), "{}", r.likely);
        assert!(r.low < r.likely && r.likely < r.high);
    }

    #[test]
    fn the_estimate_never_says_a_video_stream_grows() {
        let (v, r) = judge(&video(100.0, 1e6), 17_500_000, 0.2, &says(5.0));
        assert_eq!(v, Verdict::LowGain);
        assert_eq!((r.low, r.likely, r.high), (17_500_000.0, 17_500_000.0, 17_500_000.0));
    }

    #[test]
    fn a_file_without_numbers_is_no_candidate_and_gives_no_nonsense() {
        // Unknown bitrate: nothing known to save.
        let (v, r) = judge(&video(100.0, 0.0), 1_000_000, 0.2, &says(0.05));
        assert_eq!((v, r.likely), (Verdict::LowGain, 1_000_000.0));
        // An empty file, with and without a bitrate.
        assert_eq!(judge(&video(100.0, 0.0), 0, 0.2, &says(0.05)).0, Verdict::LowGain);
        assert_eq!(judge(&video(100.0, 1e6), 0, 0.2, &says(0.05)).0, Verdict::LowGain);
        // No height: the width alone does not make it a picture.
        assert_ne!(judge(&MediaInfo { height: 0, ..video(100.0, 1e6) }, 1_000_000, 0.2, &Model::prior()).0, Verdict::Candidate);
    }

    #[test]
    fn the_range_is_the_same_factor_either_side_of_the_likely_size() {
        for m in [Model::prior(), says(0.03)] {
            let (lo, mid, hi) = m.predict_bpp(0.1, PIXELS_1080P);
            let factor = (Z80 * m.sigma_for(&features(0.1, PIXELS_1080P))).exp();
            assert!(near(hi / mid, factor) && near(mid / lo, factor), "{lo} {mid} {hi}");
        }
    }

    /// Encodes of sources from 0.05 to 0.2 bits per pixel, at 720p and 1080p.
    fn varied_history(n: usize) -> Vec<Record> {
        (0..n).map(|i| {
            let (w, h) = if i % 2 == 0 { (1920, 1080) } else { (1280, 720) };
            rec(0.05 * 4f64.powf((i % 7) as f64 / 6.0), 0.03, w, h)
        }).collect()
    }

    /// How many times the likely size the top of the range is.
    fn top_of_range(m: &Model, source_bpp: f64, pixels: f64) -> f64 {
        let (_, mid, hi) = m.predict_bpp(source_bpp, pixels);
        hi / mid
    }

    #[test]
    fn the_range_widens_away_from_what_the_history_holds() {
        let m = fit(&varied_history(60), 93.0);
        let middle = (0.1, 1600.0 * 900.0);
        let beside = top_of_range(&m, middle.0, middle.1);
        // At the middle of the history nothing is added.
        let x = features(m.seen[0].0.exp() * SOURCE_BPP_REF, m.seen[1].0.exp() * PIXELS_1080P);
        assert!(near(m.sigma_for(&x), m.sigma), "{} {}", m.sigma_for(&x), m.sigma);
        // 4K, far from any frame size seen: the whole addition, and no more further out.
        let far = |px: f64| m.sigma_for(&features(0.1, px));
        let whole = (m.sigma.powi(2) + UNKNOWN_CONTENT_SIGMA.powi(2)).sqrt();
        assert!(near(far(3840.0 * 2160.0), whole) && near(far(7680.0 * 4320.0), whole));
        assert!(top_of_range(&m, 0.1, 3840.0 * 2160.0) > beside);
        // So does a source of far fewer or more bits per pixel than any seen.
        assert!(near(m.sigma_for(&features(0.001, middle.1)), whole));
    }

    #[test]
    fn with_few_encodes_every_video_counts_as_unlike_them() {
        let whole = |m: &Model| (m.sigma.powi(2) + UNKNOWN_CONTENT_SIGMA.powi(2)).sqrt();
        for n in [0, 1, 4] {
            let m = fit(&varied_history(n), 93.0);
            assert!(near(m.sigma_for(&features(0.1, PIXELS_1080P)), whole(&m)), "{n}");
        }
        // Five are enough to tell, and a history of one frame size and one kind of source
        // finds the same video close and another frame size far.
        let same = vec![rec(0.1, 0.03, 1920, 1080); 5];
        let m = fit(&same, 93.0);
        assert!(near(m.sigma_for(&features(0.1, PIXELS_1080P)), m.sigma));
        assert!(near(m.sigma_for(&features(0.1, 1280.0 * 720.0)), whole(&m)));
    }

    #[test]
    fn the_first_guess_falls_with_the_picture_and_the_sources_bits() {
        let m = Model::prior();
        let at = |bpp: f64, px: f64| m.predict_bpp(bpp, px).1;
        assert!(near(at(0.1, PIXELS_1080P * 4.0) / BPP_1080P, 4f64.powf(RES_EXPONENT)));
        assert!(near(at(0.2, PIXELS_1080P) / BPP_1080P, 2f64.powf(SOURCE_EXPONENT)));
        // A source of no known bits is taken as a very lean one, not as nothing.
        assert!(near(at(0.0, PIXELS_1080P), at(1e-4, PIXELS_1080P)));
        assert!(at(0.0, PIXELS_1080P).is_finite() && at(0.0, PIXELS_1080P) > 0.0);
    }

    #[test]
    fn the_size_model_learns_from_encodes_of_other_presets_and_targets() {
        let likely = |m: &Model| m.predict_bpp(0.1, PIXELS_1080P).1;
        let at_six = varied_history(60);
        // Asked about preset 5, a history of preset 6 and the first guess both say 5.8% less.
        for records in [&at_six[..], &[]] {
            let ratio = likely(&Model::fit_at_preset(records, 93.0, 5)) / likely(&Model::fit_at_preset(records, 93.0, 6));
            assert!(near(ratio, 0.942), "{ratio}");
        }
        assert!(near(likely(&Model::fit_at_preset(&at_six, 93.0, 4)) / likely(&fit(&at_six, 93.0)), 0.906));
        // Encodes at preset 4, 9.4% smaller, teach the same as those at 6 did.
        let at_four: Vec<Record> = at_six.iter().map(|r| Record { preset: 4, output_bitrate: r.output_bitrate * 0.906, ..r.clone() }).collect();
        assert!(near(likely(&fit(&at_four, 93.0)), likely(&fit(&at_six, 93.0))));
        // Encodes made for a point below the target stand for larger files at it.
        let shift = LOG_SIZE_PER_VMAF.exp();
        let lower: Vec<Record> = at_six.iter().map(|r| Record { target_vmaf: 92.0, output_bitrate: r.output_bitrate / shift, ..r.clone() }).collect();
        assert!(near(likely(&fit(&lower, 93.0)), likely(&fit(&at_six, 93.0))));
        // Presets past the last known size count as the last.
        assert_eq!(preset_size(14), preset_size(13));
        assert!((1..14).all(|p| preset_size(p) > preset_size(p - 1)));
        assert_eq!(preset_size(PILOT_PRESET), 1.0);
    }

    #[test]
    fn encodes_count_within_a_point_and_a_half_of_the_target() {
        let at = |target: f64| Record { target_vmaf: target, ..rec(0.1, 0.07, 1920, 1080) };
        assert_eq!(fit(&[at(91.5), at(94.5), at(93.0)], 93.0).samples, 3);
        assert_eq!(fit(&[at(91.4), at(94.6)], 93.0).samples, 0);
    }

    #[test]
    fn encodes_without_usable_numbers_teach_nothing() {
        let bad = [
            Record { source_bitrate: 0.0, ..rec(0.1, 0.07, 1920, 1080) },
            Record { output_bitrate: 0.0, ..rec(0.1, 0.07, 1920, 1080) },
            Record { fps: 0.0, ..rec(0.1, 0.07, 1920, 1080) },
            Record { width: 0, ..rec(0.1, 0.07, 1920, 1080) },
            Record { fps: f64::NAN, ..rec(0.1, 0.07, 1920, 1080) },
        ];
        let m = fit(&bad, 93.0);
        assert_eq!(m.samples, 0);
        assert_eq!(m.beta, Model::prior().beta);
        assert_eq!(m.sigma, PRIOR_SIGMA);
    }

    #[test]
    fn encodes_that_agree_with_the_prior_leave_it_and_the_spread_has_a_floor() {
        let prior = Model::prior();
        let sizes = [(1280, 720), (1920, 1080), (3840, 2160)];
        let recs: Vec<Record> = (0..300)
            .map(|i| {
                let (w, h) = sizes[i % 3];
                let s = 0.02 + 0.001 * i as f64;
                rec(s, prior.predict_bpp(s, w as f64 * h as f64).1, w, h)
            })
            .collect();
        let m = fit(&recs, 93.0);
        assert_eq!(m.samples, 300);
        assert!(m.beta.iter().zip(prior.beta).all(|(a, b)| (a - b).abs() < 1e-9), "{:?}", m.beta);
        // However well they agree, a video can still surprise.
        assert_eq!(m.sigma, 0.1);
    }

    #[test]
    fn the_first_crf_stays_in_the_range_whatever_the_model_says() {
        let info = MediaInfo { width: 1920, height: 1080, fps: 30.0, video_bitrate: 6e6, ..Default::default() };
        let says_crf = |crf: f64| CrfModel { beta: [crf, 0.0, 0.0], samples: 0 };
        assert_eq!(says_crf(100.0).start(&info, 20, 63), 63);
        assert_eq!(says_crf(-5.0).start(&info, 20, 63), 20);
        assert_eq!(says_crf(-5.0).start(&info, 0, 63), 0);
        assert_eq!(says_crf(34.5).start(&info, 20, 63), 35);
        assert_eq!(says_crf(f64::NAN).start(&info, 20, 63), 20);
        // A range turned around gives its lower end rather than a panic.
        assert_eq!(says_crf(35.0).start(&info, 40, 30), 40);
    }

    #[test]
    fn a_video_of_unknown_size_starts_from_the_plain_guess() {
        let m = CrfModel { beta: [30.0, 2.0, 3.0], samples: 1 };
        assert_eq!(m.predict(0.0, PIXELS_1080P), 30.0);
        assert_eq!(m.predict(0.1, 0.0), 30.0);
        assert_eq!(m.predict(f64::NAN, PIXELS_1080P), 30.0);
        assert!(near(m.predict(0.1 * std::f64::consts::E, PIXELS_1080P), 32.0));
    }

    #[test]
    fn encodes_count_for_the_first_crf_within_three_points_of_the_target() {
        let at = |target: f64| Record { target_vmaf: target, ..crf_rec(0.1, 1920, 1080, 30.0) };
        assert_eq!(CrfModel::fit(&[at(90.0), at(96.0)], 93.0, "tune=0").samples, 2);
        assert_eq!(CrfModel::fit(&[at(89.9), at(96.1)], 93.0, "tune=0").samples, 0);
        assert_eq!(CrfModel::fit(&[Record { source_bitrate: 0.0, ..crf_rec(0.1, 1920, 1080, 30.0) }], 93.0, "tune=0").samples, 0);
    }

    #[test]
    fn presets_get_faster_as_their_number_rises_and_stop_at_the_last() {
        assert_eq!(preset_time(6), 1.0);
        assert!((1..=13).all(|p| preset_time(p) < preset_time(p - 1)));
        assert_eq!(preset_time(14), preset_time(13));
        assert_eq!(preset_time(255), preset_time(13));
    }

    #[test]
    fn an_encode_counts_for_the_speed_from_ten_seconds_on() {
        let lasting = |secs: f64| Record { encode_secs: Some(secs), ..timed(1920, 1080, 6, 0.01) };
        assert_eq!(TimeModel::fit(&[lasting(10.0)], 6).samples, 1);
        assert_eq!(TimeModel::fit(&[lasting(9.99)], 6).samples, 0);
        for bad in [Record { fps: 0.0, ..lasting(60.0) }, Record { duration_s: 0.0, ..lasting(60.0) }, Record { width: 0, ..lasting(60.0) }, Record { fps: f64::NAN, ..lasting(60.0) }] {
            assert_eq!(TimeModel::fit(&[bad], 6).samples, 0);
        }
    }

    #[test]
    fn the_time_of_a_video_goes_by_its_picture_and_is_nothing_when_unknown() {
        let m = TimeModel::fit(&[], 6);
        let info = MediaInfo { width: 1920, height: 1080, fps: 30.0, duration_s: 1000.0, ..Default::default() };
        // The sound runs on; 200 s of picture are what is encoded.
        assert_eq!(m.secs(&MediaInfo { video_s: 200.0, ..info.clone() }), m.secs(&MediaInfo { duration_s: 200.0, ..info.clone() }));
        assert_eq!(m.secs(&MediaInfo { fps: 0.0, ..info.clone() }), 0.0);
        assert_eq!(m.secs(&MediaInfo { height: 0, ..info.clone() }), 0.0);
        // Shorter than its sample clips: searched on all of it, twice.
        let tiny = m.secs(&MediaInfo { duration_s: 4.0, ..info });
        assert!(near(tiny, GUESS_1080P * 30.0 * 12.0), "{tiny}");
    }

    #[test]
    fn per_scene_encodes_without_both_times_teach_nothing_of_their_cost() {
        let scene = |search: Option<f64>, encode: Option<f64>| Record { mode: "scenes".into(), search_secs: search, encode_secs: encode, ..rec(0.1, 0.03, 1920, 1080) };
        let m = TimeModel::fit(&[scene(None, Some(600.0)), scene(Some(60.0), None), scene(Some(60.0), Some(9.0))], 6);
        assert_eq!(m.scene_samples, 0);
        assert!(near(m.scenes, SCENES_FACTOR));
        assert_eq!(TimeModel::fit(&[scene(Some(60.0), Some(600.0))], 6).scene_samples, 1);
    }

    #[test]
    fn the_share_of_auto_encodes_per_scene_moves_toward_what_was_seen() {
        let made = |mode: &str| Record { scenes: Some("auto".into()), mode: mode.into(), ..rec(0.1, 0.03, 1920, 1080) };
        let all = TimeModel::fit(&vec![made("scenes"); 997], 6);
        assert!(near(all.auto_share, 998.0 / 1000.0), "{}", all.auto_share);
        let none = TimeModel::fit(&vec![made("whole"); 997], 6);
        assert!(near(none.auto_share, 1.0 / 1000.0), "{}", none.auto_share);
        assert!(near(none.auto_factor(), 1.0 + (none.scenes - 1.0) / 1000.0));
    }

    #[test]
    fn three_equations_are_solved_whatever_their_order() {
        let x = solve3([[2.0, 1.0, 1.0], [1.0, 3.0, 2.0], [1.0, 0.0, 0.0]], [4.0, 5.0, 6.0]).unwrap();
        assert_eq!(x.map(|v| v.round()), [6.0, 15.0, -23.0]);
        // A zero where the first pivot would be.
        let x = solve3([[0.0, 1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 2.0]], [3.0, 4.0, 10.0]).unwrap();
        assert_eq!(x, [4.0, 3.0, 5.0]);
        assert_eq!(solve3([[1.0, 2.0, 3.0], [2.0, 4.0, 6.0], [1.0, 0.0, 1.0]], [1.0, 2.0, 3.0]), None);
        assert_eq!(solve3([[0.0; 3]; 3], [0.0; 3]), None);
    }

    #[test]
    fn every_verdict_but_the_two_tried_reads_as_a_skip() {
        assert_eq!(Verdict::Candidate.label(), "candidate");
        assert_eq!(Verdict::Maybe.label(), "may save too little: tried last");
        assert!(Verdict::Candidate.will_be_tried() && Verdict::Maybe.will_be_tried());
        for v in [Verdict::AlreadyAv1, Verdict::LowGain, Verdict::DolbyVision, Verdict::TooShort, Verdict::Unreadable] {
            assert!(v.label().starts_with("skip: ") && !v.will_be_tried(), "{v:?}");
        }
    }
}
