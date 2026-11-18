//! Decides whether a file is worth re-encoding and roughly how much it would save.
//!
//! Predicts the AV1 bits per pixel needed at the target quality from the source's bits per
//! pixel and resolution, with a log-linear model fitted to past encodes (see [`crate::history`]).
//! Without history it falls back to the 2026-11-01 pilot (8 clips, preset 6, VMAF ~91-95):
//! ~0.035 bits per pixel at 1080p, falling mildly at higher resolutions and for sources that
//! spend few bits themselves. A few encodes only
//! nudge that prior; many encodes replace it. Every estimate is a range from the data's spread.

use crate::history::Record;
use crate::probe::MediaInfo;
use serde::Serialize;

const BPP_1080P: f64 = 0.035;
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

/// ln(av1 bpp) = beta[0] + beta[1]·ln(source bpp / 0.1) + beta[2]·ln(pixels / 1080p).
#[derive(Debug, Clone, Copy)]
pub struct Model {
    pub beta: [f64; 3],
    /// Residual spread (log units), blended with the prior's.
    pub sigma: f64,
    /// Encodes the model was fitted on.
    pub samples: usize,
}

fn features(source_bpp: f64, pixels: f64) -> [f64; 3] {
    [1.0, (source_bpp / SOURCE_BPP_REF).ln(), (pixels / PIXELS_1080P).ln()]
}

fn bpp(bitrate: f64, width: u32, height: u32, fps: f64) -> f64 {
    let px_per_s = width as f64 * height as f64 * fps;
    if px_per_s > 0.0 { bitrate / px_per_s } else { 0.0 }
}

impl Model {
    pub fn prior() -> Self {
        Model { beta: [BPP_1080P.ln(), SOURCE_EXPONENT, RES_EXPONENT], sigma: PRIOR_SIGMA, samples: 0 }
    }

    /// Ridge regression toward the prior, on encodes made for a similar VMAF target.
    pub fn fit(records: &[Record], target_vmaf: f64) -> Self {
        let prior = Self::prior();
        let pts: Vec<([f64; 3], f64)> = records
            .iter()
            .filter(|r| (r.target_vmaf - target_vmaf).abs() <= 1.5)
            .filter_map(|r| {
                let src = bpp(r.source_bitrate, r.width, r.height, r.fps);
                let out = bpp(r.output_bitrate, r.width, r.height, r.fps);
                (src > 0.0 && out > 0.0).then(|| (features(src, r.width as f64 * r.height as f64), out.ln()))
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
        Model { beta, sigma, samples: pts.len() }
    }

    /// Predicted AV1 bits per pixel: (low, likely, high), the outer two an 80% interval.
    pub fn predict_bpp(&self, source_bpp: f64, pixels: f64) -> (f64, f64, f64) {
        let mid = dot(&self.beta, &features(source_bpp.max(1e-4), pixels));
        let d = Z80 * self.sigma;
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
/// frame at preset 6, 9 ms at 720p, 62 ms at 4K); every whole-file encode recorded here
/// pulls it toward this machine's real speed.
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

/// Six 15-minute 1080p recordings (2026-11-15) took 180 minutes per scene where one pass was
/// predicted, and for 55 others measured, at 58.
const SCENES_FACTOR: f64 = 3.0;

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
        let prior = [(SPF_1080P * preset_time(preset)).ln(), SPF_EXPONENT];
        let pts: Vec<(f64, f64)> = records
            .iter()
            .filter(|r| r.mode == "whole")
            .filter_map(|r| {
                let frames = r.fps * r.duration_s;
                // Very short encodes are mostly start-up and muxing, not frames.
                let secs = r.encode_secs.filter(|s| *s >= MIN_TIMED_SECS)?;
                let pixels = r.width as f64 * r.height as f64;
                let per_frame = secs / frames * preset_time(preset) / preset_time(r.preset);
                (frames >= 1.0 && pixels > 0.0).then(|| ((pixels / PIXELS_1080P).ln(), per_frame.ln()))
            })
            .collect();
        // Ridge regression toward the prior, two unknowns.
        let (mut a, mut b, mut d) = (PRIOR_WEIGHT, 0.0, PRIOR_WEIGHT);
        let (mut r0, mut r1) = (PRIOR_WEIGHT * prior[0], PRIOR_WEIGHT * prior[1]);
        for (x, y) in &pts {
            a += 1.0;
            b += x;
            d += x * x;
            r0 += y;
            r1 += x * y;
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
            Verdict::AlreadyAv1 => "skip: already AV1",
            Verdict::LowGain => "skip: low expected gain",
            Verdict::DolbyVision => "skip: Dolby Vision",
            Verdict::TooShort => "skip: too short or no duration",
            Verdict::Unreadable => "skip: unreadable",
        }
    }
}

/// Estimated video stream size after re-encoding, in bytes: (optimistic, likely, pessimistic).
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct SizeRange {
    pub low: f64,
    pub likely: f64,
    pub high: f64,
}

pub fn bits_per_pixel(m: &MediaInfo) -> f64 {
    bpp(m.video_bitrate, m.width, m.height, m.fps)
}

/// Returns the verdict and the estimated new size of the *whole file* in bytes.
pub fn judge(m: &MediaInfo, file_size: u64, min_savings: f64, model: &Model) -> (Verdict, SizeRange) {
    let keep = SizeRange { low: file_size as f64, likely: file_size as f64, high: file_size as f64 };
    if m.codec == "av1" {
        return (Verdict::AlreadyAv1, keep);
    }
    if m.picture_secs() < 5.0 || m.width == 0 || m.fps <= 0.0 {
        return (Verdict::TooShort, keep);
    }
    if m.dolby_vision {
        return (Verdict::DolbyVision, keep);
    }

    let pixels = m.width as f64 * m.height as f64;
    let (low, likely, high) = model.predict_bpp(bits_per_pixel(m), pixels);
    let video_bytes = m.video_bitrate * m.picture_secs() / 8.0;
    let other_bytes = (file_size as f64 - video_bytes).max(0.0);
    let new_size = |bpp: f64| {
        let av1 = bpp * pixels * m.fps * m.picture_secs() / 8.0;
        other_bytes + av1.min(video_bytes)
    };
    let est = SizeRange { low: new_size(low), likely: new_size(likely), high: new_size(high) };

    let savings = 1.0 - est.likely / file_size as f64;
    let verdict = if savings >= min_savings { Verdict::Candidate } else { Verdict::LowGain };
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
        }
    }

    #[test]
    fn no_history_gives_prior() {
        let m = Model::fit(&[], 93.0);
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
        let m = Model::fit(&recs, 93.0);
        assert!((m.beta[1] - 0.8).abs() < 0.05, "{:?}", m.beta);
        assert!((m.beta[2] + 0.3).abs() < 0.05, "{:?}", m.beta);
        assert!(m.sigma < 0.15, "sigma {}", m.sigma);
        let (_, mid, _) = m.predict_bpp(0.15, PIXELS_1080P);
        assert!((mid / truth(0.15, PIXELS_1080P) - 1.0).abs() < 0.05);
    }

    #[test]
    fn few_encodes_only_nudge_the_prior() {
        let m = Model::fit(&[rec(0.1, 0.07, 1920, 1080)], 93.0);
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
        (r.preset, r.encode_secs) = (preset, Some(per_frame * r.fps * r.duration_s));
        r
    }

    #[test]
    fn encode_time_starts_as_a_guess_and_learns_the_machine() {
        let info = |w: u32, h: u32, secs: f64| MediaInfo { width: w, height: h, fps: 30.0, duration_s: secs, ..Default::default() };
        let guess = TimeModel::fit(&[], 6);
        assert_eq!(guess.samples, 0);
        assert!((guess.secs_per_frame(PIXELS_1080P) - SPF_1080P).abs() < 1e-12);
        // Bigger frames and slower presets take longer; an hour of 1080p is hours, not minutes.
        assert!(guess.secs_per_frame(3840.0 * 2160.0) > 2.5 * SPF_1080P);
        assert!(TimeModel::fit(&[], 4).secs_per_frame(PIXELS_1080P) > 1.5 * SPF_1080P);
        assert!(TimeModel::fit(&[], 8).secs_per_frame(PIXELS_1080P) < 0.7 * SPF_1080P);
        let hour = guess.secs(&info(1920, 1080, 3600.0));
        // 64 sample clips of a second, encoded about twice.
        assert!((hour / (SPF_1080P * 30.0 * 3600.0) - 1.0 - 128.0 / 3600.0).abs() < 1e-9, "{hour}");
        // A short video is searched on most of it: the search outweighs the encode.
        let short = guess.secs(&info(1920, 1080, 10.0));
        assert!((short / (SPF_1080P * 300.0) - 2.6).abs() < 1e-9, "{short}");
        assert_eq!(guess.secs(&info(0, 0, 60.0)), 0.0);
        assert_eq!(guess.secs(&info(1920, 1080, 0.0)), 0.0);

        // A machine three times faster, seen through encodes at two presets and four sizes.
        let truth = |px: f64| SPF_1080P / 3.0 * (px / PIXELS_1080P).powf(0.7);
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
        let one = TimeModel::fit(&[timed(1920, 1080, 6, SPF_1080P / 4.0)], 6).secs_per_frame(PIXELS_1080P);
        assert!(one < SPF_1080P && one > SPF_1080P / 2.0, "{one}");
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
        assert!(few.scenes < SCENES_FACTOR && few.scenes > 2.5, "{}", few.scenes);
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
        // A third per scene at three times one pass, before anything is known.
        let guess = TimeModel::fit(&[], 6);
        assert_eq!(guess.auto_samples, 0);
        assert!((guess.auto_factor() - (1.0 + 2.0 / 3.0)).abs() < 1e-9);
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
        assert_eq!(Model::fit(&[r], 93.0).samples, 0);
    }
}
