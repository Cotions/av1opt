//! Single-file optimize: CRF search, full encode, verification. Never modifies the input.

use crate::events::Event;
use crate::probe::{self, MediaInfo};
use crate::history;
use crate::scenes;
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
    /// Per-scene CRF (chunked encode) instead of one CRF for the whole file.
    pub scenes: bool,
    /// Chunks encoded at the same time in scene mode.
    pub workers: usize,
}

/// Name prefix of scene-mode work folders. Unlike temp folders they survive crashes on purpose:
/// the next run resumes from them.
pub const WORK_PREFIX: &str = ".av1opt-work-";

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
    /// "whole" (one CRF) or "scenes" (CRF per chunk; `crf` is then the whole-file starting point).
    mode: &'a str,
    crf: f64,
    preset: u8,
    target_vmaf: f64,
    vmaf_model: &'a str,
    /// Frame-weighted VMAF of the final chunks (scene mode only).
    mean_vmaf: Option<f64>,
    search: &'a [Trial],
    chunks: Option<&'a [scenes::ChunkResult]>,
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
    if info.dolby_vision {
        skip!("Dolby Vision is not supported: re-encoding would drop its dynamic metadata");
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
    tools.emit(Event::Source {
        codec: info.codec.clone(), width: info.width, height: info.height, fps: info.fps, duration_s: info.duration_s,
        video_mbps: info.video_bitrate / 1e6, hdr: info.hdr,
    });
    // Scene mode joins constant-rate chunks; a variable-rate source would be retimed, so it
    // keeps the single-pass path.
    let mut notes = Vec::new();
    if info.hdr {
        // libvmaf's models are trained on SDR; on PQ/HLG pixels the score still tracks
        // encoding damage, but its absolute meaning differs from SDR.
        notes.push(format!("HDR ({}): colour and HDR metadata kept; VMAF measured on the HDR signal", info.color_transfer));
    }
    let scenes = o.scenes && !info.vfr;
    if o.scenes && info.vfr {
        notes.push("variable frame rate source: encoded in one pass instead of per scene".to_string());
    }
    let work = scenes.then(|| work_dir(input, out_dir, size, o)).transpose()?;
    let saved: Option<scenes::Plan> = work.as_ref().and_then(|w| {
        let p: scenes::Plan = serde_json::from_slice(&std::fs::read(w.path.join("plan.json")).ok()?).ok()?;
        (p.key == w.key).then_some(p)
    });

    let t0 = Instant::now();
    let (best, trials, mut samples) = match &saved {
        Some(p) => {
            eprintln!("resuming earlier run: crf {} from the saved search", p.best.crf);
            (Some(p.best), p.trials.clone(), Vec::new())
        }
        None => {
            eprintln!("searching CRF for VMAF >= {} ({model}, preset {})", o.target_vmaf, o.preset);
            tools.emit(Event::Phase { phase: "sampling".into(), detail: format!("{} samples", o.samples) });
            let samples = search::extract_samples(tools, input, &info, &tmp.path, o.samples, o.sample_secs)?;
            let params = SearchParams { target_vmaf: o.target_vmaf, min_crf: o.min_crf, max_crf: o.max_crf, vmaf_threads };
            tools.emit(Event::Phase { phase: "searching".into(), detail: format!("VMAF >= {} ({model}, preset {})", o.target_vmaf, o.preset) });
            let (best, trials) = search::search(tools, &info, &samples, &tmp.path, &enc, &params)?;
            (best, trials, samples)
        }
    };
    let Some(best) = best else {
        skip!("even CRF {} misses VMAF {}; source is too lean to re-encode at this target", o.min_crf, o.target_vmaf);
    };
    let predicted_savings = 1.0 - best.size_ratio;
    tools.emit(Event::Chosen { crf: best.crf, vmaf: best.mean_vmaf, predicted_savings });
    eprintln!("chose crf {} (VMAF {:.2}), predicted video savings {:.0}%", best.crf, best.mean_vmaf, predicted_savings * 100.0);
    if predicted_savings < o.min_savings && !o.force {
        skip!("predicted savings {:.0}% below --min-savings {:.0}%", predicted_savings * 100.0, o.min_savings * 100.0);
    }

    // Needs room for the encode plus slack; predicted size can be off for unusual content.
    // Scene mode holds the chunks, the joined stream and the final file at once.
    // Lossless chunk references add up to ~2 GB each (4K) per worker.
    let copies = if scenes { 3.0 } else { 1.5 };
    let refs = if scenes { o.workers as u64 * 2_000_000_000 } else { 0 };
    let need = (size as f64 * best.size_ratio.max(0.3) * copies) as u64 + 500_000_000 + refs;
    if let Some(free) = free_bytes(out_dir)
        && free < need
    {
        bail!("not enough free space in {}: {:.1} GB free, ~{:.1} GB needed", out_dir.display(), free as f64 / 1e9, need as f64 / 1e9);
    }

    let partial = tmp.path.join(format!("partial.{}", o.container.ext()));
    let covers = prepare_covers(tools, input, &info, o.container, &tmp.path, &mut notes);
    let t1 = Instant::now();
    let mut search_secs = t0.elapsed().as_secs_f64();
    let (streams, chunks, expect_frames) = if let Some(work) = &work {
        let vf = search::source_filters(&info);
        if !vf.is_empty() {
            notes.push(format!("video filters: {}", vf.join(",")));
        }
        let probe = EncodeSettings { preset: (o.preset + 4).clamp(o.preset, 12), pix_fmt_out: enc.pix_fmt_out };
        let plan = match saved {
            Some(p) => p,
            None => {
                // How much better the real preset scores than the fast probe preset at the
                // same CRF; probe scores are shifted by this to predict the real encode.
                let pt = search::trial(tools, &info, &samples, &tmp.path, &probe, best.crf, vmaf_threads, 1.0)?;
                let bias = best.mean_vmaf - pt.mean_vmaf;
                samples.clear();
                std::fs::create_dir_all(&work.path)?;
                tools.emit(Event::Phase { phase: "scenes".into(), detail: "finding scene changes".into() });
                let (cuts, total_frames) = scenes::detect(tools, input, &info, &work.path)?;
                let (first_pts, chunks) = scenes::bounds(&info, &cuts, total_frames);
                eprintln!("{} scene cuts, {total_frames} frames, {} chunks", cuts.len(), chunks.len());
                let plan = scenes::Plan { key: work.key.clone(), best, trials: trials.clone(), bias, first_pts, total_frames, chunks };
                std::fs::write(work.path.join("plan.json"), serde_json::to_vec_pretty(&plan)?)?;
                plan
            }
        };
        search_secs = t0.elapsed().as_secs_f64();
        let ctx = scenes::Ctx {
            tools, input, info: &info, work: &work.path, enc: &enc, probe: &probe, target: o.target_vmaf,
            min_crf: o.min_crf, max_crf: o.max_crf, vmaf_threads, workers: o.workers,
        };
        let chunks = scenes::encode_all(&ctx, &plan)?;
        tools.emit(Event::Phase { phase: "muxing".into(), detail: format!("joining {} chunks", chunks.len()) });
        let video = scenes::concat(&work.path, chunks.len())?;
        let (args, streams) = mux_args(&video, input, &info, o.container, &covers, &partial, &mut notes);
        tools.run(&args)?;
        let _ = std::fs::remove_file(&video);
        (streams, Some(chunks), Some(plan.total_frames))
    } else {
        let (args, streams) = full_encode_args(input, &info, &enc, best.crf, o.container, &covers, &partial, &mut notes);
        for n in &notes {
            eprintln!("note: {n}");
            tools.emit(Event::Note { text: n.clone() });
        }
        tools.emit(Event::Phase { phase: "encoding".into(), detail: format!("crf {}, preset {}", best.crf, o.preset) });
        tools.run_with_progress(&args, info.duration_s, "encoding")?;
        (streams, None, None)
    };
    let encode_secs = t1.elapsed().as_secs_f64();
    if chunks.is_some() {
        for n in &notes {
            eprintln!("note: {n}");
            tools.emit(Event::Note { text: n.clone() });
        }
    }

    eprintln!("verifying");
    tools.emit(Event::Phase { phase: "verifying".into(), detail: "streams, duration, VMAF spot checks".into() });
    let (verify_vmaf, out_info) = verify(tools, input, &info, &partial, &streams, expect_frames, o, vmaf_threads, &tmp.path)?;

    std::fs::rename(&partial, &output).or_else(|_| std::fs::copy(&partial, &output).map(|_| ()))?;
    let output_bytes = std::fs::metadata(&output)?.len();
    let abs_input = std::fs::canonicalize(input)?;
    let abs_output = std::fs::canonicalize(&output)?;
    let report = Report {
        input: &abs_input,
        output: &abs_output,
        source_bytes: size,
        output_bytes,
        savings: 1.0 - output_bytes as f64 / size as f64,
        mode: if chunks.is_some() { "scenes" } else { "whole" },
        crf: best.crf,
        preset: o.preset,
        target_vmaf: o.target_vmaf,
        vmaf_model: model,
        mean_vmaf: chunks.as_deref().map(scenes::mean_vmaf),
        search: &trials,
        chunks: chunks.as_deref(),
        verify_vmaf: &verify_vmaf,
        search_secs,
        encode_secs,
        notes: &notes,
    };
    std::fs::write(output.with_extension("av1opt.json"), serde_json::to_vec_pretty(&report)?)?;
    // Chunk sizes are exact; container-reported bitrates can include audio (Matroska).
    let video_bitrate = match &chunks {
        Some(c) => c.iter().map(|r| r.bytes).sum::<u64>() as f64 * 8.0 / info.duration_s,
        None => out_info.video_bitrate,
    };
    let record = history::Record::new(&info, video_bitrate, o.target_vmaf, o.preset, report.mode);
    if let Err(e) = history::append(&record) {
        eprintln!("warning: could not record this encode in the history: {e:#}");
    }
    if let Some(w) = &work
        && !o.keep_temp
    {
        let _ = std::fs::remove_dir_all(&w.path);
    }
    if let Some(c) = &chunks {
        let crfs = c.iter().map(|r| r.crf);
        eprintln!(
            "chunk crf {}-{}, mean VMAF {:.2}, worst chunk {:.2}",
            crfs.clone().fold(f64::INFINITY, f64::min),
            crfs.fold(0.0, f64::max),
            scenes::mean_vmaf(c),
            c.iter().map(|r| r.vmaf).fold(f64::INFINITY, f64::min)
        );
    }
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

/// Resumable work folder for scene mode, named after everything that affects its contents.
struct WorkDir {
    path: PathBuf,
    key: String,
}

fn work_dir(input: &Path, out_dir: &Path, size: u64, o: &Options) -> Result<WorkDir> {
    let mtime = std::fs::metadata(input)?
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    let key = format!(
        "v1|{}|{size}|{mtime}|{}|{}|{}|{}|{}|{}",
        std::path::absolute(input)?.display(), o.target_vmaf, o.preset, o.min_crf, o.max_crf, o.samples, o.sample_secs
    );
    // Created only when chunk work starts, so skipped files leave nothing behind.
    let path = out_dir.join(format!("{WORK_PREFIX}{:016x}", scenes::fnv1a(&key)));
    Ok(WorkDir { path, key })
}

#[allow(clippy::too_many_arguments)]
fn full_encode_args(
    input: &Path,
    info: &MediaInfo,
    enc: &EncodeSettings,
    crf: f64,
    container: Container,
    covers: &[CoverFile],
    out: &Path,
    notes: &mut Vec<String>,
) -> (Vec<String>, Streams) {
    let map = format!("0:{}", info.video_index);
    let mut a: Vec<String> = ["-y", "-i", &path_arg(input), "-map", &map].map(String::from).to_vec();
    let vf = search::source_filters(info);
    if !vf.is_empty() {
        a.extend(["-filter:v:0".into(), vf.join(",")]);
        notes.push(format!("video filters: {}", vf.join(",")));
    }
    a.extend(enc.video_args(crf));
    a.extend(["-fps_mode:v:0", "passthrough"].map(String::from));
    let streams = stream_args(info, 0, container, covers, notes);
    a.extend(streams.args.iter().cloned());
    a.push(path_arg(out));
    (a, streams)
}

/// Joined scene-chunk video plus audio, subtitles and metadata from the source.
fn mux_args(
    video: &Path,
    input: &Path,
    info: &MediaInfo,
    container: Container,
    covers: &[CoverFile],
    out: &Path,
    notes: &mut Vec<String>,
) -> (Vec<String>, Streams) {
    // Keep the source's video/audio offset; the joined video itself starts at zero.
    let offset = info.video_start - info.start_time;
    let mut a: Vec<String> = [
        "-y", "-v", "error", "-itsoffset", &format!("{offset:.6}"), "-framerate", &search::cfr(info),
        "-i", &path_arg(video), "-i", &path_arg(input), "-map", "0:v", "-c:v", "copy",
    ]
    .map(String::from)
    .to_vec();
    let streams = stream_args(info, 1, container, covers, notes);
    a.extend(streams.args.iter().cloned());
    a.push(path_arg(out));
    (a, streams)
}

/// Audio, subtitle, cover, attachment and metadata arguments for source input number `src`.
struct Streams {
    args: Vec<String>,
    audio: usize,
    subs: usize,
    covers: usize,
}

/// A cover image extracted for Matroska output, which stores covers as file attachments.
struct CoverFile {
    path: PathBuf,
    mime: &'static str,
    name: String,
}

const COVER_TYPES: &[(&str, &str, &str)] = &[("mjpeg", "image/jpeg", "jpg"), ("png", "image/png", "png"), ("bmp", "image/bmp", "bmp"), ("webp", "image/webp", "webp")];

/// MP4 keeps covers as attached-picture streams (copied as they are); Matroska keeps them as
/// attachments, which ffmpeg can only add from files, so they are extracted first.
fn prepare_covers(tools: &Tools, input: &Path, info: &MediaInfo, container: Container, tmp: &Path, notes: &mut Vec<String>) -> Vec<CoverFile> {
    if container != Container::Mkv {
        return Vec::new();
    }
    let mut files = Vec::new();
    for (k, c) in info.covers.iter().enumerate() {
        let Some(&(_, mime, ext)) = COVER_TYPES.iter().find(|t| t.0 == c.codec) else {
            notes.push(format!("cover {k} ({}) dropped: unsupported image type", c.codec));
            continue;
        };
        let path = tmp.join(format!("cover{k}.{ext}"));
        let args: Vec<String> = ["-y", "-v", "error", "-i", &path_arg(input), "-map", &format!("0:{}", c.index), "-c", "copy", "-frames:v", "1", "-f", "image2", &path_arg(&path)]
            .map(String::from)
            .to_vec();
        match tools.run(&args) {
            Ok(_) => files.push(CoverFile { path, mime, name: c.filename.clone().unwrap_or_else(|| format!("cover.{ext}")) }),
            Err(e) => notes.push(format!("cover {k} dropped: {}", e.to_string().lines().last().unwrap_or(""))),
        }
    }
    files
}

fn stream_args(info: &MediaInfo, src: usize, container: Container, cover_files: &[CoverFile], notes: &mut Vec<String>) -> Streams {
    let mut a = Vec::new();
    for (i, s) in info.audio.iter().enumerate() {
        a.extend(["-map".into(), format!("{src}:a:{i}")]);
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
                a.extend(["-map".into(), format!("{src}:s:{i}")]);
                a.extend([format!("-c:s:{kept_subs}"), codec.into()]);
                kept_subs += 1;
            }
            Container::Mp4 if TEXT_SUBS.contains(&codec.as_str()) => {
                a.extend(["-map".into(), format!("{src}:s:{i}")]);
                a.extend([format!("-c:s:{kept_subs}"), "mov_text".into()]);
                kept_subs += 1;
            }
            Container::Mp4 => notes.push(format!("subtitle {i} ({codec}) dropped: image subtitles don't fit in MP4; use --container mkv")),
        }
    }

    let mut covers = 0;
    match container {
        Container::Mp4 => {
            for c in &info.covers {
                if !["mjpeg", "png"].contains(&c.codec.as_str()) {
                    notes.push(format!("cover ({}) dropped: MP4 covers must be JPEG or PNG", c.codec));
                    continue;
                }
                covers += 1;
                a.extend(["-map".into(), format!("{src}:{}", c.index), format!("-c:v:{covers}"), "copy".into()]);
                a.extend([format!("-disposition:v:{covers}"), "attached_pic".into()]);
            }
        }
        Container::Mkv => {
            // Fonts used by ASS subtitles live in attachments; covers are re-attached after them.
            a.extend(["-map".into(), format!("{src}:t?"), "-c:t".into(), "copy".into()]);
            for (k, f) in cover_files.iter().enumerate() {
                let t = info.attachments as usize + k;
                a.extend(["-attach".into(), path_arg(&f.path)]);
                a.extend([format!("-metadata:s:t:{t}"), format!("mimetype={}", f.mime)]);
                a.extend([format!("-metadata:s:t:{t}"), format!("filename={}", f.name)]);
            }
            covers = cover_files.len();
        }
    }
    a.extend(["-map_metadata".into(), src.to_string(), "-map_chapters".into(), src.to_string()]);
    if container == Container::Mp4 {
        a.extend(["-movflags", "+faststart"].map(String::from));
    }
    Streams { args: a, audio: info.audio.len(), subs: kept_subs, covers }
}

/// Checks the encode before it replaces anything: stream layout, duration, and VMAF spot checks
/// at positions the search did not sample.
#[allow(clippy::too_many_arguments)]
fn verify(
    tools: &Tools,
    input: &Path,
    info: &MediaInfo,
    out: &Path,
    streams: &Streams,
    expect_frames: Option<u64>,
    o: &Options,
    threads: usize,
    tmp: &Path,
) -> Result<(Vec<f64>, MediaInfo)> {
    let out_info = probe::probe(&tools.ffprobe, out, std::fs::metadata(out)?.len())?;
    if out_info.codec != "av1" {
        bail!("verify: output video is {}, expected av1", out_info.codec);
    }
    let tolerance = (info.duration_s * 0.005).max(1.0);
    if (out_info.duration_s - info.duration_s).abs() > tolerance {
        bail!("verify: duration {:.1}s vs source {:.1}s", out_info.duration_s, info.duration_s);
    }
    if out_info.audio_streams as usize != streams.audio || out_info.subtitle_streams as usize != streams.subs {
        bail!(
            "verify: output has {} audio / {} subtitle streams, expected {} / {}",
            out_info.audio_streams, out_info.subtitle_streams, streams.audio, streams.subs
        );
    }
    if out_info.color_transfer != info.color_transfer && info.hdr {
        bail!("verify: output transfer is '{}', source is '{}' (HDR signalling lost)", out_info.color_transfer, info.color_transfer);
    }
    if out_info.covers.len() != streams.covers {
        bail!("verify: output has {} cover images, expected {}", out_info.covers.len(), streams.covers);
    }
    if let Some(want) = expect_frames {
        let got = tools.count_frames(out)?;
        if got != want {
            bail!("verify: output has {got} frames, expected {want}");
        }
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
        let mut v = tools.vmaf(&d, &r, model, pix, threads, 0, 1)?;
        // Seeking two different files can land a frame or two apart; retry nearby alignments
        // before calling the encode bad.
        if v < o.target_vmaf - 4.0 {
            for off in [-2, -1, 1, 2] {
                v = v.max(tools.vmaf(&d, &r, model, pix, threads, off, 1)?);
            }
        }
        eprintln!("  spot check at {}: VMAF {v:.2}", hms(start));
        tools.emit(Event::SpotCheck { at_secs: start, vmaf: v });
        scores.push(v);
        let _ = std::fs::remove_file(&r);
        let _ = std::fs::remove_file(&d);
    }
    // Short spot checks are noisier than the multi-sample search, so allow some slack.
    let floor = o.target_vmaf - 4.0;
    if let Some(bad) = scores.iter().find(|&&v| v < floor) {
        bail!("verify: spot check VMAF {bad:.2} below {floor:.1}; keeping nothing");
    }
    Ok((scores, out_info))
}

fn free_bytes(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: valid NUL-terminated path and a properly sized out-parameter.
    (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0).then(|| st.f_bavail as u64 * st.f_frsize as u64)
}
