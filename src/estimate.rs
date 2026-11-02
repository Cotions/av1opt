//! Decides whether a file is worth re-encoding and roughly how much it would save.
//!
//! Predicts the AV1 bits per pixel needed at the target quality from the source's bits per
//! pixel and resolution, with a log-linear model fitted to past encodes (see [`crate::history`]).
//! Without history it falls back to the 2026-11-01 pilot (8 clips, preset 6, VMAF ~91-95):
//! ~0.035 bits per pixel at 1080p, falling mildly at higher resolutions. A few encodes only
//! nudge that prior; many encodes replace it. Every estimate is a range from the data's spread.

use crate::history::Record;
use crate::probe::MediaInfo;
use serde::Serialize;

const BPP_1080P: f64 = 0.035;
const PIXELS_1080P: f64 = 1920.0 * 1080.0;
/// Bigger frames compress better per pixel. Pilot data was noisy (one 4K clip needed ~1/4
/// the 1080p bpp, small clips only slightly more), so the prior stays mild and conservative.
const RES_EXPONENT: f64 = -0.2;
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
        Model { beta: [BPP_1080P.ln(), 0.0, RES_EXPONENT], sigma: PRIOR_SIGMA, samples: 0 }
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
        for row in col + 1..3 {
            let f = a[row][col] / a[col][col];
            for k in col..3 {
                a[row][k] -= f * a[col][k];
            }
            b[row] -= f * b[col];
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
    if m.duration_s < 5.0 || m.width == 0 || m.fps <= 0.0 {
        return (Verdict::TooShort, keep);
    }
    if m.dolby_vision {
        return (Verdict::DolbyVision, keep);
    }

    let pixels = m.width as f64 * m.height as f64;
    let (low, likely, high) = model.predict_bpp(bits_per_pixel(m), pixels);
    let video_bytes = m.video_bitrate * m.duration_s / 8.0;
    let other_bytes = (file_size as f64 - video_bytes).max(0.0);
    let new_size = |bpp: f64| {
        let av1 = bpp * pixels * m.fps * m.duration_s / 8.0;
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
            time: 0, codec: "h264".into(), width: w, height: h, fps: 30.0, duration_s: 60.0,
            source_bitrate: src_bpp * px, output_bitrate: out_bpp * px, target_vmaf: 93.0, preset: 6,
            mode: "whole".into(),
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

    #[test]
    fn ignores_other_vmaf_targets() {
        let mut r = rec(0.1, 0.07, 1920, 1080);
        r.target_vmaf = 97.0;
        assert_eq!(Model::fit(&[r], 93.0).samples, 0);
    }
}
