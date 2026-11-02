//! Single-file optimize: CRF search, full encode, verification. Never modifies the input.

use crate::probe::{self, MediaInfo};
use crate::search::{self, EncodeSettings, SearchParams, Trial};
use crate::tools::{Tools, hms, path_arg};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub struct Options {
    pub container: Container,
    pub target_vmaf: f64,
    pub preset: u8,
    pub min_crf: u8,
    pub max_crf: u8,
    pub samples: usize,
    pub sample_secs: f64,
    pub min_savings: f64,
    pub force: bool,
    pub overwrite: bool,
    pub keep_temp: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Container {
    /// Plays in Chrome and Firefox. Text subtitles become mov_text; image subtitles are dropped.
    Mp4,
    /// Keeps every subtitle and attachment, but browsers don't play it natively.
    Mkv,
}

impl Container {
    pub fn ext(self) -> &'static str {
        match self {
            Container::Mp4 => "mp4",
            Container::Mkv => "mkv",
        }
    }
}

/// Written next to the output so later tuning can learn from real results.
#[derive(Serialize)]
struct Report<'a> {
    input: &'a Path,
    output: &'a Path,
    source_bytes: u64,
    output_bytes: u64,
    savings: f64,
    crf: u8,
    preset: u8,
    target_vmaf: f64,
    vmaf_model: &'a str,
    search: &'a [Trial],
    verify_vmaf: &'a [f64],
    search_secs: f64,
    encode_secs: f64,
    notes: &'a [String],
}

/// Deletes the temp folder when dropped, unless asked to keep it.
struct TempDir {
    path: PathBuf,
    keep: bool,
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// A deliberate decision not to encode (as opposed to a failure). Batch runs remember these
/// so resumed runs don't redo the CRF search.
#[derive(Debug)]
pub struct Skip(pub String);

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Skip {}

macro_rules! skip {
    ($($arg:tt)*) => { return Err(Skip(format!($($arg)*)).into()) };
}

const COPY_AUDIO: &[&str] = &["aac", "opus", "mp3"];
const TEXT_SUBS: &[&str] = &["subrip", "ass", "ssa", "mov_text", "webvtt", "text"];

/// Removes temp folders left by runs that crashed or were interrupted (their PID is gone).
pub fn clean_stale_temp(out_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(out_dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(pid) = name.strip_prefix(".av1opt-tmp-").and_then(|p| p.parse::<u32>().ok()) else { continue };
        if pid != std::process::id() && !Path::new(&format!("/proc/{pid}")).exists() {
            eprintln!("removing stale temp folder {}", e.path().display());
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Re-encodes one file into `out_dir`. Returns the output path.
pub fn optimize(tools: &Tools, input: &Path, out_dir: &Path, o: &Options) -> Result<PathBuf> {
    let size = std::fs::metadata(input).with_context(|| format!("cannot read {}", input.display()))?.len();
    let info = probe::probe(&tools.ffprobe, input, size)?;
    if info.codec == "av1" && !o.force {
        skip!("already AV1 (use --force to re-encode anyway)");
    }
    if info.hdr {
        skip!("HDR input is not supported yet");
    }
    if info.duration_s < 1.0 {
        skip!("no usable duration");
    }

    let stem = input.file_stem().context("input has no file name")?.to_string_lossy();
    let output = out_dir.join(format!("{stem}.{}", o.container.ext()));
    if output.exists() && !o.overwrite {
        bail!("{} already exists (use --overwrite)", output.display());
    }
    if std::fs::canonicalize(input)? == std::path::absolute(&output)? {
        bail!("output would overwrite the input");
    }
    std::fs::create_dir_all(out_dir)?;
    let tmp = TempDir { path: out_dir.join(format!(".av1opt-tmp-{}", std::process::id())), keep: o.keep_temp };
    std::fs::create_dir_all(&tmp.path)?;

    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    // 4K VMAF with many threads was part of the memory blow-up in the first pilot.
    let vmaf_threads = if info.width.max(info.height) >= 3200 { threads.min(4) } else { threads };
    let enc = EncodeSettings { preset: o.preset, pix_fmt_out: "yuv420p10le" };
    let model = search::vmaf_model(&info);

    eprintln!(
        "{}: {} {}x{} {:.2} fps, {}, {:.1} Mbps video",
        input.display(), info.codec, info.width, info.height, info.fps, hms(info.duration_s), info.video_bitrate / 1e6
    );
    eprintln!("searching CRF for VMAF >= {} ({model}, preset {})", o.target_vmaf, o.preset);
    let t0 = Instant::now();
    let samples = search::extract_samples(tools, input, &info, &tmp.path, o.samples, o.sample_secs)?;
    let params = SearchParams { target_vmaf: o.target_vmaf, min_crf: o.min_crf, max_crf: o.max_crf, vmaf_threads };
    let (best, trials) = search::search(tools, &info, &samples, &tmp.path, &enc, &params)?;
    let search_secs = t0.elapsed().as_secs_f64();
    let Some(best) = best else {
        skip!("even CRF {} misses VMAF {}; source is too lean to re-encode at this target", o.min_crf, o.target_vmaf);
    };
    let predicted_savings = 1.0 - best.size_ratio;
    eprintln!("chose crf {} (VMAF {:.2}), predicted video savings {:.0}%", best.crf, best.mean_vmaf, predicted_savings * 100.0);
    if predicted_savings < o.min_savings && !o.force {
        skip!("predicted savings {:.0}% below --min-savings {:.0}%", predicted_savings * 100.0, o.min_savings * 100.0);
    }

    // Needs room for the encode plus slack; predicted size can be off for unusual content.
    let need = (size as f64 * best.size_ratio.max(0.3) * 1.5) as u64 + 500_000_000;
    if let Some(free) = free_bytes(out_dir)
        && free < need
    {
        bail!("not enough free space in {}: {:.1} GB free, ~{:.1} GB needed", out_dir.display(), free as f64 / 1e9, need as f64 / 1e9);
    }

    let mut notes = Vec::new();
    let partial = tmp.path.join(format!("partial.{}", o.container.ext()));
    let args = full_encode_args(input, &info, &enc, best.crf, o.container, &partial, &mut notes);
    for n in &notes {
        eprintln!("note: {n}");
    }
    let t1 = Instant::now();
    tools.run_with_progress(&args, info.duration_s, "encoding")?;
    let encode_secs = t1.elapsed().as_secs_f64();

    eprintln!("verifying");
    let verify_vmaf = verify(tools, input, &info, &partial, &args, o, vmaf_threads, &tmp.path)?;

    std::fs::rename(&partial, &output).or_else(|_| std::fs::copy(&partial, &output).map(|_| ()))?;
    let output_bytes = std::fs::metadata(&output)?.len();
    let report = Report {
        input,
        output: &output,
        source_bytes: size,
        output_bytes,
        savings: 1.0 - output_bytes as f64 / size as f64,
        crf: best.crf,
        preset: o.preset,
        target_vmaf: o.target_vmaf,
        vmaf_model: model,
        search: &trials,
        verify_vmaf: &verify_vmaf,
        search_secs,
        encode_secs,
        notes: &notes,
    };
    std::fs::write(output.with_extension("av1opt.json"), serde_json::to_vec_pretty(&report)?)?;
    eprintln!(
        "done: {:.1} MB -> {:.1} MB ({:.0}% smaller), search {}, encode {}",
        size as f64 / 1e6,
        output_bytes as f64 / 1e6,
        report.savings * 100.0,
        hms(search_secs),
        hms(encode_secs)
    );
    Ok(output)
}

fn full_encode_args(
    input: &Path,
    info: &MediaInfo,
    enc: &EncodeSettings,
    crf: u8,
    container: Container,
    out: &Path,
    notes: &mut Vec<String>,
) -> Vec<String> {
    let map = format!("0:{}", info.video_index);
    let mut a: Vec<String> = ["-y", "-i", &path_arg(input), "-map", &map].map(String::from).to_vec();
    let vf = search::source_filters(info);
    if !vf.is_empty() {
        a.extend(["-vf".into(), vf.join(",")]);
        notes.push(format!("video filters: {}", vf.join(",")));
    }
    a.extend(enc.video_args(crf));
    a.extend(["-fps_mode", "passthrough"].map(String::from));

    for (i, s) in info.audio.iter().enumerate() {
        a.extend(["-map".into(), format!("0:a:{i}")]);
        if COPY_AUDIO.contains(&s.codec.as_str()) {
            a.extend([format!("-c:a:{i}"), "copy".into()]);
        } else {
            let kbps = (s.channels.max(1) * 64).clamp(64, 384);
            a.extend([format!("-c:a:{i}"), "libopus".into(), format!("-b:a:{i}"), format!("{kbps}k")]);
            notes.push(format!("audio {i}: {} -> Opus {kbps} kbps", s.codec));
        }
    }

    let mut kept_subs = 0;
    for (i, codec) in info.subtitle_codecs.iter().enumerate() {
        match container {
            Container::Mkv => {
                // MP4-style text subtitles can't be stored in Matroska; convert them to SRT.
                let codec = if codec == "mov_text" { "srt" } else { "copy" };
                a.extend(["-map".into(), format!("0:s:{i}")]);
                a.extend([format!("-c:s:{kept_subs}"), codec.into()]);
                kept_subs += 1;
            }
            Container::Mp4 if TEXT_SUBS.contains(&codec.as_str()) => {
                a.extend(["-map".into(), format!("0:s:{i}")]);
                a.extend([format!("-c:s:{kept_subs}"), "mov_text".into()]);
                kept_subs += 1;
            }
            Container::Mp4 => notes.push(format!("subtitle {i} ({codec}) dropped: image subtitles don't fit in MP4; use --container mkv")),
        }
    }

    if container == Container::Mkv {
        // Fonts used by ASS subtitles live in attachments.
        a.extend(["-map", "0:t?", "-c:t", "copy"].map(String::from));
    }
    a.extend(["-map_metadata", "0", "-map_chapters", "0"].map(String::from));
    if container == Container::Mp4 {
        a.extend(["-movflags", "+faststart"].map(String::from));
    }
    a.push(path_arg(out));
    a
}

/// Checks the encode before it replaces anything: stream layout, duration, and VMAF spot checks
/// at positions the search did not sample.
#[allow(clippy::too_many_arguments)]
fn verify(
    tools: &Tools,
    input: &Path,
    info: &MediaInfo,
    out: &Path,
    args: &[String],
    o: &Options,
    threads: usize,
    tmp: &Path,
) -> Result<Vec<f64>> {
    let out_info = probe::probe(&tools.ffprobe, out, std::fs::metadata(out)?.len())?;
    if out_info.codec != "av1" {
        bail!("verify: output video is {}, expected av1", out_info.codec);
    }
    let tolerance = (info.duration_s * 0.005).max(1.0);
    if (out_info.duration_s - info.duration_s).abs() > tolerance {
        bail!("verify: duration {:.1}s vs source {:.1}s", out_info.duration_s, info.duration_s);
    }
    let mapped_audio = args.iter().filter(|a| a.starts_with("0:a:")).count();
    let mapped_subs = args.iter().filter(|a| a.starts_with("0:s:")).count();
    if out_info.audio_streams as usize != mapped_audio || out_info.subtitle_streams as usize != mapped_subs {
        bail!(
            "verify: output has {} audio / {} subtitle streams, expected {mapped_audio} / {mapped_subs}",
            out_info.audio_streams, out_info.subtitle_streams
        );
    }

    let secs = 5.0_f64.min(info.duration_s);
    let model = search::vmaf_model(info);
    let pix = search::reference_pix_fmt(info);
    let mut scores = Vec::new();
    for (i, frac) in [0.3, 0.7].into_iter().enumerate() {
        let start = ((info.duration_s - secs) * frac).max(0.0);
        let r = tmp.join(format!("verify{i}.ref.mkv"));
        let d = tmp.join(format!("verify{i}.out.mkv"));
        search::extract_reference(tools, input, info, true, start, secs, &r)?;
        search::extract_reference(tools, out, info, false, start, secs, &d)?;
        let mut v = tools.vmaf(&d, &r, model, pix, threads, 0)?;
        // Seeking two different files can land a frame or two apart; retry nearby alignments
        // before calling the encode bad.
        if v < o.target_vmaf - 4.0 {
            for off in [-2, -1, 1, 2] {
                v = v.max(tools.vmaf(&d, &r, model, pix, threads, off)?);
            }
        }
        eprintln!("  spot check at {}: VMAF {v:.2}", hms(start));
        scores.push(v);
        let _ = std::fs::remove_file(&r);
        let _ = std::fs::remove_file(&d);
    }
    // Short spot checks are noisier than the multi-sample search, so allow some slack.
    let floor = o.target_vmaf - 4.0;
    if let Some(bad) = scores.iter().find(|&&v| v < floor) {
        bail!("verify: spot check VMAF {bad:.2} below {floor:.1}; keeping nothing");
    }
    Ok(scores)
}

fn free_bytes(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: valid NUL-terminated path and a properly sized out-parameter.
    (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0).then(|| st.f_bavail as u64 * st.f_frsize as u64)
}
