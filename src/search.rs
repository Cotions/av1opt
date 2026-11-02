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

pub struct EncodeSettings {
    pub preset: u8,
    pub pix_fmt_out: &'static str,
}

impl EncodeSettings {
    /// SVT-AV1 takes CRF in quarter steps through its own parameters (ffmpeg's -crf is integer).
    pub fn video_args(&self, crf: f64) -> Vec<String> {
        // Stream specifiers keep these off copied cover-art streams.
        [
            "-c:v:0", "libsvtav1", "-preset:v:0", &self.preset.to_string(), "-pix_fmt:v:0", self.pix_fmt_out,
            "-svtav1-params:v:0", &format!("tune=0:crf={}", quarter(crf)),
        ]
        .map(String::from)
        .to_vec()
    }
}

/// Rounds down to SVT-AV1's quarter-step CRF grid.
pub fn quarter(crf: f64) -> f64 {
    (crf * 4.0).floor() / 4.0
}

/// Lossless reference pixel format matching the source bit depth.
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
    // Closest standard rate, not the first within tolerance: 24 must not become 23.976,
    // which drops a frame every ~42 and misaligns VMAF comparisons.
    STD.iter()
        .min_by(|a, b| (a.0 - info.fps).abs().total_cmp(&(b.0 - info.fps).abs()))
        .filter(|(f, _)| (f - info.fps).abs() / f < 0.01)
        .map_or_else(|| format!("{:.3}", info.fps), |(_, s)| s.to_string())
}

/// Numeric value of [`cfr`], i.e. frames per second of the constant-rate stream.
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

/// Score every 3rd frame: within ~0.01 of scoring all of them, ~45% less VMAF time. Must be odd:
/// SVT-AV1's hierarchical GOP alternates better and worse frames, so an even stride sees only
/// one kind (measured +0.2 VMAF with a stride of 2).
pub const VMAF_STRIDE: u32 = 3;

pub const PREROLL_SECS: f64 = 2.0;

/// Lossless, constant-frame-rate cut of `input` starting at `start` seconds.
/// `is_source` applies [`source_filters`]; pass false when cutting from an already-encoded output.
pub fn extract_reference(
    tools: &Tools,
    input: &Path,
    info: &MediaInfo,
    is_source: bool,
    start: f64,
    secs: f64,
    out: &Path,
) -> Result<()> {
    let mut vf = if is_source { source_filters(info) } else { Vec::new() };
    vf.push(format!("fps={}", cfr(info)));
    vf.push("setpts=PTS-STARTPTS".into());
    // Seeking straight to `start` can begin decoding mid-GOP (MPEG-2/TS, open-GOP H.264) and
    // return frames from the wrong place. Seek a little earlier and discard the warm-up after
    // decoding (output-side -ss), so the decoder has a clean run-in.
    let preroll = start.min(PREROLL_SECS);
    let args: Vec<String> = [
        "-y", "-v", "error", "-ss", &format!("{:.3}", start - preroll), "-i", &path_arg(input),
        "-ss", &format!("{preroll:.3}"), "-t", &format!("{secs:.3}"),
        "-map", &format!("0:{}", if is_source { info.video_index } else { 0 }), "-an", "-sn",
        "-vf", &vf.join(","), "-c:v", "ffv1", "-pix_fmt", reference_pix_fmt(info), &path_arg(out),
    ]
    .map(String::from)
    .to_vec();
    tools.run(&args).map(|_| ())
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

pub fn extract_samples(tools: &Tools, input: &Path, info: &MediaInfo, tmp: &Path, count: usize, secs: f64) -> Result<Vec<Sample>> {
    let pos = sample_positions(info.duration_s, count, secs);
    let n = pos.len();
    pos.into_iter()
        .enumerate()
        .map(|(i, (start, secs))| {
            let reference = tmp.join(format!("sample{i}.ref.mkv"));
            eprintln!("  sample {}/{n} at {} ({secs:.0}s)", i + 1, crate::tools::hms(start));
            extract_reference(tools, input, info, true, start, secs, &reference)?;
            Ok(Sample { reference, secs })
        })
        .collect()
}

pub struct SearchParams {
    pub target_vmaf: f64,
    pub min_crf: u8,
    pub max_crf: u8,
    pub vmaf_threads: usize,
}

/// First guess and typical VMAF drop per CRF step for SVT-AV1 (pilot data: ~0.36 at 720p-1080p).
const START_CRF: u8 = 35;

/// Winning CRF (None if even the minimum fails) and every (crf, score) evaluated.
pub type CrfSearch = (Option<u8>, Vec<(u8, f64)>);
pub const VMAF_PER_CRF: f64 = 0.4;

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
        let step = |v: f64| ((v - target).abs() / VMAF_PER_CRF).round().max(2.0) as u8;
        let next = match (pass, fail) {
            (Some((p, _)), Some((f, _))) if f <= p + 1 => None,
            (Some((p, vp)), Some((f, vf))) => {
                let frac = if vp > vf { (vp - target) / (vp - vf) } else { 0.5 };
                let c = p as f64 + frac * (f - p) as f64;
                Some((c.round() as u8).clamp(p + 1, f - 1))
            }
            (Some((p, _)), None) if p >= max => None,
            (Some((p, vp)), None) => Some(p.saturating_add(step(vp)).min(max)),
            (None, Some((f, _))) if f <= min => None,
            (None, Some((f, vf))) => Some(f.saturating_sub(step(vf)).max(min)),
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

/// Runs [`find_crf`] with real sample encodes. Returns the chosen trial and all trials.
pub fn search(
    tools: &Tools,
    info: &MediaInfo,
    samples: &[Sample],
    tmp: &Path,
    enc: &EncodeSettings,
    p: &SearchParams,
) -> Result<(Option<Trial>, Vec<Trial>)> {
    let source_bps = info.video_bitrate / 8.0;
    let mut trials: BTreeMap<u8, Trial> = BTreeMap::new();
    let (best, _) = find_crf(START_CRF, p.min_crf, p.max_crf, p.target_vmaf, |crf| {
        let t = trial(tools, info, samples, tmp, enc, crf as f64, p.vmaf_threads, source_bps)?;
        eprintln!(
            "  crf {crf:2}: VMAF {:.2} (worst sample {:.2}), size ~{:.0}% of source",
            t.mean_vmaf, t.min_vmaf, t.size_ratio * 100.0
        );
        trials.insert(crf, t);
        Ok(t.mean_vmaf)
    })?;
    // The answer lies between the best passing CRF and the next one up; interpolate to the
    // quarter step where the score crosses the target (rounded down, toward quality).
    let chosen = best.map(|c| match trials.get(&(c + 1)) {
        Some(next) if trials[&c].mean_vmaf > next.mean_vmaf => {
            let (a, b) = (trials[&c], *next);
            let frac = quarter((a.mean_vmaf - p.target_vmaf) / (a.mean_vmaf - b.mean_vmaf)).clamp(0.0, 0.75);
            let mix = |x: f64, y: f64| x + (y - x) * frac;
            Trial {
                crf: c as f64 + frac,
                mean_vmaf: mix(a.mean_vmaf, b.mean_vmaf),
                min_vmaf: mix(a.min_vmaf, b.min_vmaf),
                size_ratio: mix(a.size_ratio, b.size_ratio),
            }
        }
        _ => trials[&c],
    });
    Ok((chosen, trials.into_values().collect()))
}

/// Encodes every sample at `crf` and scores it against its reference.
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
    let (mut scores, mut bytes, mut secs) = (Vec::new(), 0u64, 0.0);
    for (i, s) in samples.iter().enumerate() {
        let out = tmp.join(format!("sample{i}.crf{crf}.mkv"));
        let mut args: Vec<String> = ["-y", "-v", "error", "-i", &path_arg(&s.reference)].map(String::from).to_vec();
        args.extend(enc.video_args(crf));
        args.push(path_arg(&out));
        tools.run(&args)?;
        scores.push(tools.vmaf(&out, &s.reference, vmaf_model(info), reference_pix_fmt(info), threads, 0, VMAF_STRIDE)?);
        bytes += std::fs::metadata(&out)?.len();
        secs += s.secs;
        std::fs::remove_file(&out)?;
    }
    let mean = scores.iter().sum::<f64>() / scores.len() as f64;
    let min = scores.iter().copied().fold(f64::INFINITY, f64::min);
    let ratio = if source_bps > 0.0 { bytes as f64 / secs / source_bps } else { 1.0 };
    Ok(Trial { crf, mean_vmaf: mean, min_vmaf: min, size_ratio: ratio })
}

#[cfg(test)]
mod tests {
    use super::*;

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
            start_time: 0.0, video_start: 0.0, covers: vec![], attachments: 0,
            audio_streams: 0, subtitle_streams: 0, audio: vec![], subtitle_codecs: vec![],
        }
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
}
