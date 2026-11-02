//! Decides whether a file is worth re-encoding and roughly how much it would save.
//!
//! The size model is calibrated on the 2026-11-01 pilot (8 clips, SVT-AV1 preset 6,
//! CRF 36, VMAF ~91-95). AV1 needed about 0.035 bits per pixel per frame at 1080p,
//! falling at higher resolutions. Spread between files was large (0.018-0.096 at
//! 1080p), so every estimate is a range. Real CRF searches will replace this later.

use crate::probe::MediaInfo;
use serde::Serialize;

const BPP_1080P: f64 = 0.035;
const PIXELS_1080P: f64 = 1920.0 * 1080.0;
/// Bigger frames compress better per pixel. Pilot data was noisy (one 4K clip needed ~1/4
/// the 1080p bpp, small clips only slightly more), so this stays mild and conservative.
const RES_EXPONENT: f64 = -0.2;
const OPTIMISTIC: f64 = 0.6;
const PESSIMISTIC: f64 = 2.5;

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
    let px_per_s = m.width as f64 * m.height as f64 * m.fps;
    if px_per_s > 0.0 { m.video_bitrate / px_per_s } else { 0.0 }
}

/// Returns the verdict and the estimated new size of the *whole file* in bytes.
pub fn judge(m: &MediaInfo, file_size: u64, min_savings: f64) -> (Verdict, SizeRange) {
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
    let target_bpp = BPP_1080P * (pixels / PIXELS_1080P).powf(RES_EXPONENT);
    let video_bytes = m.video_bitrate * m.duration_s / 8.0;
    let other_bytes = (file_size as f64 - video_bytes).max(0.0);
    let new_size = |factor: f64| {
        let av1 = target_bpp * factor * pixels * m.fps * m.duration_s / 8.0;
        other_bytes + av1.min(video_bytes)
    };
    let est = SizeRange { low: new_size(OPTIMISTIC), likely: new_size(1.0), high: new_size(PESSIMISTIC) };

    let savings = 1.0 - est.likely / file_size as f64;
    let verdict = if savings >= min_savings { Verdict::Candidate } else { Verdict::LowGain };
    (verdict, est)
}
