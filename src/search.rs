//! Target-quality CRF search: encode short sample clips at several CRFs and pick the
//! highest CRF (smallest file) whose mean VMAF still meets the target.

use crate::probe::MediaInfo;
use crate::tools::{Tools, path_arg};
use anyhow::{Result, bail};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct Sample {
    pub reference: PathBuf,
    pub secs: f64,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Trial {
    pub crf: u8,
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
    pub fn video_args(&self, crf: u8) -> Vec<String> {
        [
            "-c:v", "libsvtav1", "-preset", &self.preset.to_string(), "-crf", &crf.to_string(),
            "-pix_fmt", self.pix_fmt_out, "-svtav1-params", "tune=0",
        ]
        .map(String::from)
        .to_vec()
    }
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
    STD.iter()
        .find(|(f, _)| (f - info.fps).abs() / f < 0.01)
        .map_or_else(|| format!("{:.3}", info.fps), |(_, s)| s.to_string())
}

/// Lossless, constant-frame-rate cut of `input` starting at `start` seconds.
pub fn extract_reference(tools: &Tools, input: &Path, info: &MediaInfo, start: f64, secs: f64, out: &Path) -> Result<()> {
    let args: Vec<String> = [
        "-y", "-v", "error", "-ss", &format!("{start:.3}"), "-i", &path_arg(input), "-t", &format!("{secs:.3}"),
        "-map", "0:v:0", "-an", "-sn", "-vf", &format!("fps={},setpts=PTS-STARTPTS", cfr(info)),
        "-c:v", "ffv1", "-pix_fmt", reference_pix_fmt(info), &path_arg(out),
    ]
    .map(String::from)
    .to_vec();
    tools.run(&args).map(|_| ())
}

/// Picks `count` clips of `secs` seconds spread over the video, skipping the first and last 5%.
pub fn extract_samples(tools: &Tools, input: &Path, info: &MediaInfo, tmp: &Path, count: usize, secs: f64) -> Result<Vec<Sample>> {
    let dur = info.duration_s;
    let secs = secs.min(dur);
    let n = ((dur / (secs * 2.0)) as usize).clamp(1, count.max(1));
    let span = (dur * 0.9 - secs).max(0.0);
    (0..n)
        .map(|i| {
            let start = if n == 1 && span == 0.0 { 0.0 } else { dur * 0.05 + span * (i as f64 + 0.5) / n as f64 };
            let reference = tmp.join(format!("sample{i}.ref.mkv"));
            eprintln!("  sample {}/{n} at {} ({secs:.0}s)", i + 1, crate::tools::hms(start));
            extract_reference(tools, input, info, start, secs, &reference)?;
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

/// Binary search for the highest CRF whose mean sample VMAF >= target.
/// Returns the chosen trial (None if even `min_crf` misses the target) and all trials run.
pub fn search(
    tools: &Tools,
    info: &MediaInfo,
    samples: &[Sample],
    tmp: &Path,
    enc: &EncodeSettings,
    p: &SearchParams,
) -> Result<(Option<Trial>, Vec<Trial>)> {
    let mut trials: BTreeMap<u8, Trial> = BTreeMap::new();
    let (mut lo, mut hi) = (p.min_crf, p.max_crf);
    let mut best: Option<Trial> = None;
    let source_bps = info.video_bitrate / 8.0;
    while lo <= hi {
        let crf = lo + (hi - lo) / 2;
        let t = trial(tools, info, samples, tmp, enc, crf, p.vmaf_threads, source_bps)?;
        eprintln!(
            "  crf {crf:2}: VMAF {:.2} (worst sample {:.2}), size ~{:.0}% of source",
            t.mean_vmaf, t.min_vmaf, t.size_ratio * 100.0
        );
        trials.insert(crf, t);
        if t.mean_vmaf >= p.target_vmaf {
            best = Some(t);
            if crf == u8::MAX { break; }
            lo = crf + 1;
        } else {
            if crf == 0 { break; }
            hi = crf - 1;
        }
    }
    Ok((best, trials.into_values().collect()))
}

#[allow(clippy::too_many_arguments)]
fn trial(
    tools: &Tools,
    info: &MediaInfo,
    samples: &[Sample],
    tmp: &Path,
    enc: &EncodeSettings,
    crf: u8,
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
        scores.push(tools.vmaf(&out, &s.reference, vmaf_model(info), reference_pix_fmt(info), threads, 0)?);
        bytes += std::fs::metadata(&out)?.len();
        secs += s.secs;
        std::fs::remove_file(&out)?;
    }
    let mean = scores.iter().sum::<f64>() / scores.len() as f64;
    let min = scores.iter().copied().fold(f64::INFINITY, f64::min);
    let ratio = if source_bps > 0.0 { bytes as f64 / secs / source_bps } else { 1.0 };
    Ok(Trial { crf, mean_vmaf: mean, min_vmaf: min, size_ratio: ratio })
}
