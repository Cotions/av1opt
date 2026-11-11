//! Single-file optimize: CRF search, full encode, verification. Never modifies the input.

use crate::events::Event;
use crate::probe::{self, MediaInfo};
use crate::history;
use crate::metadata::{self, MetaOptions, Resolved};
use crate::scenes;
use crate::sections::{self, Sections};
use crate::search::{self, EncodeSettings, SearchParams, Trial};
use crate::tools::{Tools, hms, path_arg};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub struct Options {
    pub container: Container,
    pub target_vmaf: f64,
    pub preset: u8,
    /// SVT-AV1 parameters besides the CRF (`key=value:...`), normally [`search::DEFAULT_PARAMS`].
    pub svt_params: String,
    /// Benchmark picks per resolution class; where a class has one, it replaces `preset` and
    /// `svt_params`.
    pub tuning: Option<crate::tuning::Tuning>,
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
    /// Source tags kept or dropped, tags added, file date.
    pub meta: MetaOptions,
    /// Parts of the video to leave out, as (start, end) seconds of the source.
    pub remove: Vec<(f64, f64)>,
    pub audio: Audio,
    /// Measure every CRF trial again instead of reusing trials from earlier runs.
    pub fresh_search: bool,
}

/// What happens to sound tracks that could be copied as they are.
#[derive(Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Audio {
    /// AAC, Opus and MP3 tracks are copied untouched; other formats become Opus.
    #[default]
    Keep,
    /// As keep, but a track spending well over what Opus needs for its channels (stereo:
    /// above 144 kbps) is re-encoded to Opus (stereo: 96 kbps). Lossy a second time.
    Shrink,
}

/// Opus bitrate that is hard to tell from the original for this many channels, in kbps:
/// 56 mono, 96 stereo, 256 for 5.1.
fn lean_opus_kbps(channels: u32) -> u32 {
    (channels.max(1) * 40 + 16).min(384)
}

/// A copyable track is only re-encoded when it spends this many times the lean bitrate:
/// below that the saving isn't worth a second lossy pass.
const SHRINK_ABOVE: f64 = 1.5;

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

/// Version of the `.av1opt.json` report layout. Bump it when a field changes meaning or
/// is removed (adding fields is fine); readers skip reports newer than they understand.
pub const REPORT_FORMAT: u32 = 1;

/// Version of the scene-mode work folder (`plan.json` and the chunk files). Bump it when
/// their layout or meaning changes: a work folder from another format is started over
/// instead of resumed, since mixing chunks from two versions could give a bad output.
const PLAN_FORMAT: u32 = 1;

/// Written next to the output so later tuning can learn from real results.
#[derive(Serialize)]
struct Report<'a> {
    format: u32,
    /// The av1opt version that wrote it.
    av1opt: &'static str,
    input: &'a Path,
    output: &'a Path,
    source_bytes: u64,
    output_bytes: u64,
    savings: f64,
    /// "whole" (one CRF) or "scenes" (CRF per chunk; `crf` is then the whole-file starting point).
    mode: &'a str,
    crf: f64,
    preset: u8,
    /// SVT-AV1 parameters besides the CRF.
    svt_params: &'a str,
    target_vmaf: f64,
    vmaf_model: &'a str,
    /// Frame-weighted VMAF of the final chunks (scene mode only).
    mean_vmaf: Option<f64>,
    search: &'a [Trial],
    chunks: Option<&'a [scenes::ChunkResult]>,
    verify_vmaf: &'a [f64],
    /// Source sections left out of the output, (start, end) seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    removed: Option<&'a [(f64, f64)]>,
    /// Tags av1opt wrote on top of (or instead of) the source's.
    tags: std::collections::BTreeMap<&'a str, &'a str>,
    /// The yt-dlp info file the tags were filled from.
    info_json: Option<&'a Path>,
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
    let mut info = probe::probe(&tools.ffprobe, input, size)?;
    if o.audio == Audio::Shrink {
        // Matroska rarely says what a track spends; measure the ones it leaves open.
        for (i, s) in info.audio.iter_mut().enumerate() {
            if s.bitrate <= 0.0 && COPY_AUDIO.contains(&s.codec.as_str()) {
                s.bitrate = probe::audio_bitrate(&tools.ffprobe, input, i).unwrap_or(0.0);
            }
        }
    }
    let info = info;
    if info.dolby_vision {
        skip!("Dolby Vision is not supported: re-encoding would drop its dynamic metadata");
    }
    if info.duration_s < 1.0 {
        skip!("no usable duration");
    }
    // Removing sections is an edit the user asked for, so the output is wanted even when the
    // re-encode alone wouldn't be worth it.
    let cut = if o.remove.is_empty() { None } else { Some(Sections::new(&o.remove, info.duration_s)?) };
    if info.codec == "av1" && !o.force && cut.is_none() {
        skip!("already AV1 (use --force to re-encode anyway)");
    }
    let out_duration = cut.as_ref().map_or(info.duration_s, Sections::kept);

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
    let tuned = o.tuning.as_ref().and_then(|t| t.pick(info.width, info.height));
    let (preset, svt_params) = tuned.map_or((o.preset, o.svt_params.as_str()), |c| (c.preset, c.params.as_str()));
    let enc = EncodeSettings { preset, pix_fmt_out: "yuv420p10le", params: svt_params.to_string() };
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
    if o.tuning.is_some() {
        let class = crate::tuning::Class::of(info.width, info.height).label();
        notes.push(match tuned {
            Some(_) => format!("tuned settings for {class}: preset {preset}, {svt_params}"),
            None => format!("no tuned settings for {class}: preset {preset}, {svt_params}"),
        });
    } else if svt_params != search::DEFAULT_PARAMS {
        notes.push(format!("SVT-AV1 parameters: {svt_params}"));
    }
    if info.hdr {
        // libvmaf's models are trained on SDR; on PQ/HLG pixels the score still tracks
        // encoding damage, but its absolute meaning differs from SDR.
        notes.push(format!("HDR ({}): colour and HDR metadata kept; VMAF measured on the HDR signal", info.color_transfer));
    }
    let meta = metadata::resolve(&o.meta, input, &info.tags);
    if !meta.tags.is_empty() {
        let from = meta.info_json.as_ref().and_then(|p| p.file_name()).map(|n| format!(" (from {})", n.to_string_lossy())).unwrap_or_default();
        notes.push(format!("metadata: set {}{from}", meta.tags.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(", ")));
    }
    if !o.meta.keep {
        notes.push("metadata: source tags and chapters dropped".to_string());
    }
    if let Some(c) = &cut {
        let list: Vec<String> = c.removed().iter().map(|&(a, b)| format!("{}-{}", sections::fmt_time(a), sections::fmt_time(b))).collect();
        notes.push(format!(
            "removed {} section{}: {}; {} of {} kept",
            list.len(), if list.len() == 1 { "" } else { "s" }, list.join(", "), sections::fmt_time(c.kept()), sections::fmt_time(info.duration_s)
        ));
    }
    let scenes = o.scenes && !info.vfr && cut.is_none();
    if o.scenes && cut.is_some() {
        notes.push("sections removed: encoded in one pass instead of per scene".to_string());
    } else if o.scenes && info.vfr {
        notes.push("variable frame rate source: encoded in one pass instead of per scene".to_string());
    }
    let work = scenes.then(|| work_dir(input, out_dir, size, o, preset, svt_params)).transpose()?;
    let saved: Option<scenes::Plan> = work.as_ref().and_then(|w| {
        let p: scenes::Plan = serde_json::from_slice(&std::fs::read(w.path.join("plan.json")).ok()?).ok()?;
        (p.key == w.key).then_some(p)
    });

    let t0 = tools.stopwatch();
    let (best, trials, mut lab) = match &saved {
        Some(p) => {
            eprintln!("resuming earlier run: crf {} from the saved search", p.best.crf);
            (search::Outcome::Found(p.best), p.trials.clone(), None)
        }
        None => {
            // A trial that misses the target while already too large to be worth it settles
            // the matter: the CRF the target needs is lower, and its encode larger.
            let max_size_ratio = (!o.force && cut.is_none()).then_some(1.0 - o.min_savings);
            let guess = crate::estimate::CrfModel::fit(&history::load(), o.target_vmaf, svt_params);
            let start_crf = guess.start(&info, o.min_crf, o.max_crf);
            let learned = if guess.samples == 0 { String::new() } else { format!(", going by {} past encode{}", guess.samples, if guess.samples == 1 { "" } else { "s" }) };
            eprintln!("searching CRF for VMAF >= {} ({model}, preset {preset}), from CRF {start_crf}{learned}", o.target_vmaf);
            let pos = match &cut {
                Some(c) => c.sample_positions(o.samples, o.sample_secs),
                None => search::sample_positions(info.duration_s, o.samples, o.sample_secs),
            };
            let cache = search::TrialCache::open(input, size, &pos, tools.encoder_version(), !o.fresh_search);
            let mut lab = search::Lab::new(tools, input, &info, &tmp.path, pos, vmaf_threads, cache);
            let params = SearchParams { target_vmaf: o.target_vmaf, min_crf: o.min_crf, max_crf: o.max_crf, start_crf, max_size_ratio };
            tools.emit(Event::Phase { phase: "searching".into(), detail: format!("VMAF >= {} ({model}, preset {preset}), from CRF {start_crf}", o.target_vmaf) });
            let (best, trials) = search::search(&mut lab, &enc, &params)?;
            (best, trials, Some(lab))
        }
    };
    // With sections removed the output is wanted anyway: settle for the best quality tried.
    let reached = matches!(best, search::Outcome::Found(_));
    let best = match best {
        search::Outcome::Found(b) => b,
        search::Outcome::TooLarge(t) => skip!(
            "CRF {} misses VMAF {} ({:.2}) at {:.0}% of the source's size already: savings would be below --min-savings {:.0}%",
            t.crf, o.target_vmaf, t.mean_vmaf, t.size_ratio * 100.0, o.min_savings * 100.0
        ),
        search::Outcome::OutOfReach if cut.is_some() && let Some(t) = trials.iter().min_by(|a, b| a.crf.total_cmp(&b.crf)) => {
            notes.push(format!("VMAF {} is out of reach (CRF {} gives {:.2}); encoded at CRF {} for the cut", o.target_vmaf, t.crf, t.mean_vmaf, t.crf));
            *t
        }
        search::Outcome::OutOfReach => skip!("even CRF {} misses VMAF {}; source is too lean to re-encode at this target", o.min_crf, o.target_vmaf),
    };
    let predicted_savings = 1.0 - best.size_ratio;
    tools.emit(Event::Chosen { crf: best.crf, vmaf: best.mean_vmaf, predicted_savings });
    eprintln!("chose crf {} (VMAF {:.2}), predicted video savings {:.0}%", best.crf, best.mean_vmaf, predicted_savings * 100.0);
    if predicted_savings < o.min_savings && !o.force && cut.is_none() {
        skip!("predicted savings {:.0}% below --min-savings {:.0}%", predicted_savings * 100.0, o.min_savings * 100.0);
    }

    // Needs room for the encode plus slack; predicted size can be off for unusual content.
    // Scene mode holds the chunks, the joined stream and the final file at once.
    // Lossless chunk references add up to ~4 GB each (4K) per worker.
    let copies = if scenes { 3.0 } else { 1.5 };
    let refs = if scenes { o.workers as u64 * 4_000_000_000 } else { 0 };
    let need = (size as f64 * best.size_ratio.max(0.3) * copies) as u64 + 500_000_000 + refs;
    if let Some(free) = free_bytes(out_dir)
        && free < need
    {
        bail!("not enough free space in {}: {:.1} GB free, ~{:.1} GB needed", out_dir.display(), free as f64 / 1e9, need as f64 / 1e9);
    }

    // The sample clips are lossless and large; only scene mode measures on them again.
    if !scenes && let Some(lab) = lab.as_mut() {
        lab.discard_samples();
    }
    let partial = tmp.path.join(format!("partial.{}", o.container.ext()));
    let covers = prepare_covers(tools, input, &info, o.container, &tmp.path, &mut notes);
    let t1 = tools.stopwatch();
    let mut search_secs = t0.secs(tools);
    let probe = EncodeSettings { preset: (preset + 4).clamp(preset, 12), pix_fmt_out: enc.pix_fmt_out, params: enc.params.clone() };
    let mut work = work;
    let mut plan = saved;
    if plan.is_none()
        && let Some(w) = &work
    {
        // How much better the real preset scores than the fast probe preset at the
        // same CRF; probe scores are shifted by this to predict the real encode.
        let lab = lab.as_mut().expect("a search that wasn't resumed has its lab");
        let (pt, _) = lab.trial(&probe, best.crf)?;
        let bias = best.mean_vmaf - pt.mean_vmaf;
        lab.discard_samples();
        std::fs::create_dir_all(&w.path)?;
        tools.emit(Event::Phase { phase: "scenes".into(), detail: "finding scene changes".into() });
        let found = scenes::detect(tools, input, &info, &w.path)?;
        match found.pace.filter(scenes::Pace::uneven) {
            // The header named a constant rate the frames don't keep (see `Pace`).
            Some(p) => {
                notes.push(format!(
                    "frames come at uneven times ({} frames; at a constant {} fps {} would be dropped and {} shown twice): encoded in one pass instead of per scene",
                    p.source_frames, search::cfr(&info), p.dropped, p.repeated
                ));
                let _ = std::fs::remove_dir_all(&w.path);
                work = None;
            }
            None => {
                let (first_pts, chunks) = scenes::bounds(&info, &found.cuts, found.total);
                eprintln!("{} scene cuts, {} frames, {} chunks", found.cuts.len(), found.total, chunks.len());
                let p = scenes::Plan { key: w.key.clone(), best, trials: trials.clone(), bias, first_pts, total_frames: found.total, chunks };
                std::fs::write(w.path.join("plan.json"), serde_json::to_vec_pretty(&p)?)?;
                plan = Some(p);
            }
        }
    }
    let (streams, chunks, expect_frames) = if let (Some(work), Some(plan)) = (&work, plan) {
        let vf = search::source_filters(&info);
        if !vf.is_empty() {
            notes.push(format!("video filters: {}", vf.join(",")));
        }
        search_secs = t0.secs(tools);
        let ctx = scenes::Ctx {
            tools, input, info: &info, work: &work.path, enc: &enc, probe: &probe, target: o.target_vmaf,
            min_crf: o.min_crf, max_crf: o.max_crf, vmaf_threads, workers: o.workers,
        };
        let chunks = scenes::encode_all(&ctx, &plan)?;
        tools.emit(Event::Phase { phase: "muxing".into(), detail: format!("joining {} chunks", chunks.len()) });
        let video = scenes::concat(&work.path, chunks.len())?;
        let (args, streams) = mux_args(&video, input, &info, o.container, o.audio, &covers, &meta, &o.meta, &partial, &mut notes);
        tools.run(&args)?;
        let _ = std::fs::remove_file(&video);
        (streams, Some(chunks), Some(plan.total_frames))
    } else {
        let cut = cut.as_ref().map(|c| prepare_cut(tools, input, &info, c, o.meta.keep, &tmp.path, &mut notes)).transpose()?;
        let (args, streams) = full_encode_args(input, &info, &enc, best.crf, o.container, o.audio, &covers, &meta, &o.meta, cut.as_ref(), &partial, &mut notes);
        for n in &notes {
            eprintln!("note: {n}");
            tools.emit(Event::Note { text: n.clone() });
        }
        tools.emit(Event::Phase { phase: "encoding".into(), detail: format!("crf {}, preset {preset}", best.crf) });
        tools.run_with_progress(&args, out_duration, "encoding")?;
        (streams, None, None)
    };
    let encode_secs = t1.secs(tools);
    if chunks.is_some() {
        for n in &notes {
            eprintln!("note: {n}");
            tools.emit(Event::Note { text: n.clone() });
        }
    }

    eprintln!("verifying");
    tools.emit(Event::Phase { phase: "verifying".into(), detail: "streams, duration, VMAF spot checks".into() });
    let (verify_vmaf, out_info) = verify(tools, input, &info, &partial, &streams, expect_frames, cut.as_ref(), o.target_vmaf.min(best.mean_vmaf), vmaf_threads, &tmp.path)?;
    let lost = metadata::missing(&meta, &out_info.tags);
    if !lost.is_empty() {
        let n = format!(
            "metadata: {} not stored: {} has no field for {} (MKV keeps any tag)",
            lost.join(", "), o.container.ext().to_uppercase(), if lost.len() == 1 { "it" } else { "them" }
        );
        eprintln!("note: {n}");
        tools.emit(Event::Note { text: n.clone() });
        notes.push(n);
    }

    std::fs::rename(&partial, &output).or_else(|_| std::fs::copy(&partial, &output).map(|_| ()))?;
    if o.meta.keep_dates
        && let Err(e) = metadata::copy_mtime(input, &output)
    {
        eprintln!("warning: could not copy the file date: {e}");
    }
    let output_bytes = std::fs::metadata(&output)?.len();
    let abs_input = std::fs::canonicalize(input)?;
    let abs_output = std::fs::canonicalize(&output)?;
    let report = Report {
        format: REPORT_FORMAT,
        av1opt: env!("CARGO_PKG_VERSION"),
        input: &abs_input,
        output: &abs_output,
        source_bytes: size,
        output_bytes,
        savings: 1.0 - output_bytes as f64 / size as f64,
        mode: if chunks.is_some() { "scenes" } else { "whole" },
        crf: best.crf,
        preset,
        svt_params,
        target_vmaf: o.target_vmaf,
        vmaf_model: model,
        mean_vmaf: chunks.as_deref().map(scenes::mean_vmaf),
        search: &trials,
        chunks: chunks.as_deref(),
        verify_vmaf: &verify_vmaf,
        removed: cut.as_ref().map(Sections::removed),
        tags: meta.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect(),
        info_json: meta.info_json.as_deref(),
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
    let mut record = history::Record::new(&info, video_bitrate, o.target_vmaf, preset, report.mode, best.crf, svt_params);
    // With sections removed the times are for a shorter video than the record describes.
    if cut.is_none() {
        (record.search_secs, record.encode_secs) = (Some(search_secs), Some(encode_secs));
    }
    // A cut encoded below the target would teach the size model wrong sizes for it.
    if let Err(e) = if reached { history::append(&record) } else { Ok(()) } {
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

fn work_dir(input: &Path, out_dir: &Path, size: u64, o: &Options, preset: u8, svt_params: &str) -> Result<WorkDir> {
    let mtime = std::fs::metadata(input)?
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    let mut key = format!(
        "v{PLAN_FORMAT}|{}|{size}|{mtime}|{}|{preset}|{}|{}|{}|{}",
        std::path::absolute(input)?.display(), o.target_vmaf, o.min_crf, o.max_crf, o.samples, o.sample_secs
    );
    // Only when they differ, so work folders from before this setting existed still resume.
    if svt_params != search::DEFAULT_PARAMS {
        key.push_str(&format!("|{svt_params}"));
    }
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
    audio: Audio,
    covers: &[CoverFile],
    tags: &Resolved,
    mo: &MetaOptions,
    cut: Option<&Cut>,
    out: &Path,
    notes: &mut Vec<String>,
) -> (Vec<String>, Streams) {
    let mut a: Vec<String> = ["-y", "-i", &path_arg(input)].map(String::from).to_vec();
    let vf = search::source_filters(info);
    if !vf.is_empty() {
        notes.push(format!("video filters: {}", vf.join(",")));
    }
    let mut sub_inputs = Vec::new();
    let mut chapters = 0;
    if let Some(c) = cut {
        // Retimed subtitles and chapters come from their own files, as inputs 1, 2, ...
        let mut n = 1;
        for s in &c.subs {
            sub_inputs.push(s.as_ref().map(|p| {
                a.extend(["-i".into(), path_arg(p)]);
                n += 1;
                n - 1
            }));
        }
        if let Some(p) = &c.chapters {
            a.extend(["-f".into(), "ffmetadata".into(), "-i".into(), path_arg(p)]);
            chapters = n;
        }
        a.extend(["-filter_complex".into(), c.sections.filter_graph(info.video_index, info.audio.len(), &vf, info.duration_s)]);
        a.extend(["-map".into(), "[vout]".into()]);
    } else {
        a.extend(["-map".into(), format!("0:{}", info.video_index)]);
        if !vf.is_empty() {
            a.extend(["-filter:v:0".into(), vf.join(",")]);
        }
    }
    a.extend(enc.video_args(crf));
    a.extend(["-fps_mode:v:0", "passthrough"].map(String::from));
    let streams = stream_args(info, 0, container, audio, covers, cut.map(|_| sub_inputs.as_slice()), notes);
    let mut meta = metadata::ffmpeg_args(mo, tags, 0);
    if cut.is_some() {
        // Chapters at their new times, or none if the source had none.
        if let Some(i) = meta.iter().position(|x| x == "-map_chapters") {
            meta[i + 1] = if chapters > 0 { chapters.to_string() } else { "-1".into() };
        }
        // Filter outputs and the subtitle files carry no stream tags of their own, and naming
        // any stream's source turns off ffmpeg's automatic copying for all of them.
        meta.extend(["-map_metadata:s:v:0".into(), format!("0:s:{}", info.video_index)]);
        meta.extend(streams.meta_from.iter().flat_map(|(o, i)| [format!("-map_metadata:s:{o}"), format!("0:s:{i}")]));
    }
    a.extend(meta);
    a.extend(streams.args.iter().cloned());
    a.push(path_arg(out));
    (a, streams)
}

/// Joined scene-chunk video plus audio, subtitles and metadata from the source.
#[allow(clippy::too_many_arguments)]
fn mux_args(
    video: &Path,
    input: &Path,
    info: &MediaInfo,
    container: Container,
    audio: Audio,
    covers: &[CoverFile],
    tags: &Resolved,
    mo: &MetaOptions,
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
    let streams = stream_args(info, 1, container, audio, covers, None, notes);
    a.extend(metadata::ffmpeg_args(mo, tags, 1));
    a.extend(streams.args.iter().cloned());
    a.push(path_arg(out));
    (a, streams)
}

/// Audio, subtitle, cover and attachment arguments for source input number `src`.
struct Streams {
    args: Vec<String>,
    /// (output, source) stream specifiers whose tags must be copied explicitly (cut mode).
    meta_from: Vec<(String, String)>,
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

/// `cut_subs` is set when sections are removed: audio then comes from the cut filter graph
/// (`[ao0]`, ...) and subtitle `i` from input `cut_subs[i]` (None: dropped).
fn stream_args(
    info: &MediaInfo,
    src: usize,
    container: Container,
    audio: Audio,
    cover_files: &[CoverFile],
    cut_subs: Option<&[Option<usize>]>,
    notes: &mut Vec<String>,
) -> Streams {
    let mut a = Vec::new();
    let mut meta_from = Vec::new();
    for (i, s) in info.audio.iter().enumerate() {
        let kbps = (s.channels.max(1) * 64).clamp(64, 384);
        if cut_subs.is_some() {
            a.extend(["-map".into(), format!("[ao{i}]")]);
            a.extend([format!("-c:a:{i}"), "libopus".into(), format!("-b:a:{i}"), format!("{kbps}k")]);
            notes.push(format!("audio {i}: {} -> Opus {kbps} kbps (cut audio is re-encoded)", s.codec));
            meta_from.push((format!("a:{i}"), format!("a:{i}")));
        } else if COPY_AUDIO.contains(&s.codec.as_str()) {
            a.extend(["-map".into(), format!("{src}:a:{i}")]);
            let lean = lean_opus_kbps(s.channels);
            if audio == Audio::Shrink && s.bitrate > lean as f64 * 1000.0 * SHRINK_ABOVE {
                a.extend([format!("-c:a:{i}"), "libopus".into(), format!("-b:a:{i}"), format!("{lean}k")]);
                notes.push(format!("audio {i}: {} {:.0} kbps -> Opus {lean} kbps", s.codec, s.bitrate / 1000.0));
            } else {
                a.extend([format!("-c:a:{i}"), "copy".into()]);
            }
        } else {
            a.extend(["-map".into(), format!("{src}:a:{i}")]);
            a.extend([format!("-c:a:{i}"), "libopus".into(), format!("-b:a:{i}"), format!("{kbps}k")]);
            notes.push(format!("audio {i}: {} -> Opus {kbps} kbps", s.codec));
        }
    }

    let mut kept_subs = 0;
    for (i, codec) in info.subtitle_codecs.iter().enumerate() {
        let text = TEXT_SUBS.contains(&codec.as_str());
        if container == Container::Mp4 && !text {
            notes.push(format!("subtitle {i} ({codec}) dropped: image subtitles don't fit in MP4; use --container mkv"));
            continue;
        }
        let from = match cut_subs {
            None => format!("{src}:s:{i}"),
            Some(inputs) => match inputs.get(i).copied().flatten() {
                Some(n) => format!("{n}:s:0"),
                None => continue,
            },
        };
        // MP4-style text subtitles can't be stored in Matroska; convert them to SRT.
        let codec = match container {
            Container::Mkv if codec == "mov_text" => "srt",
            Container::Mkv => "copy",
            Container::Mp4 => "mov_text",
        };
        a.extend(["-map".into(), from]);
        a.extend([format!("-c:s:{kept_subs}"), codec.into()]);
        meta_from.push((format!("s:{kept_subs}"), format!("s:{i}")));
        kept_subs += 1;
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
                meta_from.push((format!("v:{covers}"), c.index.to_string()));
                a.extend([format!("-disposition:v:{covers}"), "attached_pic".into()]);
            }
        }
        Container::Mkv => {
            // Fonts used by ASS subtitles live in attachments; covers are re-attached after them.
            a.extend(["-map".into(), format!("{src}:t?"), "-c:t".into(), "copy".into()]);
            // Matroska needs each attachment's file name and type.
            meta_from.extend((0..info.attachments).map(|t| (format!("t:{t}"), format!("t:{t}"))));
            for (k, f) in cover_files.iter().enumerate() {
                let t = info.attachments as usize + k;
                a.extend(["-attach".into(), path_arg(&f.path)]);
                a.extend([format!("-metadata:s:t:{t}"), format!("mimetype={}", f.mime)]);
                a.extend([format!("-metadata:s:t:{t}"), format!("filename={}", f.name)]);
            }
            covers = cover_files.len();
        }
    }
    if container == Container::Mp4 {
        a.extend(["-movflags", "+faststart"].map(String::from));
    }
    Streams { args: a, meta_from, audio: info.audio.len(), subs: kept_subs, covers }
}

/// Inputs for an encode that leaves sections out: retimed copies of the text subtitles and
/// chapters (the filter graph cuts the picture and sound).
struct Cut<'a> {
    sections: &'a Sections,
    /// Per source subtitle stream; None if it is dropped.
    subs: Vec<Option<PathBuf>>,
    /// FFMETADATA file with the chapters at their new times.
    chapters: Option<PathBuf>,
}

#[allow(clippy::too_many_arguments)]
fn prepare_cut<'a>(
    tools: &Tools,
    input: &Path,
    info: &MediaInfo,
    sections: &'a Sections,
    keep_meta: bool,
    tmp: &Path,
    notes: &mut Vec<String>,
) -> Result<Cut<'a>> {
    let mut subs = Vec::new();
    for (i, codec) in info.subtitle_codecs.iter().enumerate() {
        if !TEXT_SUBS.contains(&codec.as_str()) {
            // MP4 drops these anyway (stream_args says so).
            notes.push(format!("subtitle {i} ({codec}) dropped: image subtitles can't be cut"));
            subs.push(None);
            continue;
        }
        let ass = matches!(codec.as_str(), "ass" | "ssa");
        let ext = if ass { "ass" } else { "srt" };
        let raw = tmp.join(format!("sub{i}.source.{ext}"));
        let args: Vec<String> = ["-y", "-v", "error", "-i", &path_arg(input), "-map", &format!("0:s:{i}"), "-c:s", if ass { "ass" } else { "srt" }, &path_arg(&raw)]
            .map(String::from)
            .to_vec();
        if let Err(e) = tools.run(&args) {
            notes.push(format!("subtitle {i} dropped: could not read it ({})", e.to_string().lines().last().unwrap_or("")));
            subs.push(None);
            continue;
        }
        let text = String::from_utf8_lossy(&std::fs::read(&raw)?).into_owned();
        let retimed = if ass { sections.retime_ass(&text) } else { sections.retime_srt(&text) };
        if !retimed.contains(if ass { "Dialogue:" } else { "-->" }) {
            notes.push(format!("subtitle {i} dropped: all of its lines are in removed sections"));
            subs.push(None);
            continue;
        }
        let path = tmp.join(format!("sub{i}.{ext}"));
        std::fs::write(&path, retimed)?;
        subs.push(Some(path));
    }
    let mut chapters = None;
    if keep_meta {
        let list = sections::read_chapters(&tools.ffprobe, input)?;
        if !list.is_empty() {
            let path = tmp.join("chapters.ffmeta");
            std::fs::write(&path, sections.chapters(&list))?;
            chapters = Some(path);
        }
    }
    Ok(Cut { sections, subs, chapters })
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
    cut: Option<&Sections>,
    target_vmaf: f64,
    threads: usize,
    tmp: &Path,
) -> Result<(Vec<f64>, MediaInfo)> {
    let duration = cut.map_or(info.duration_s, Sections::kept);
    let out_info = probe::probe(&tools.ffprobe, out, std::fs::metadata(out)?.len())?;
    if out_info.codec != "av1" {
        bail!("verify: output video is {}, expected av1", out_info.codec);
    }
    let tolerance = (duration * 0.005).max(1.0);
    if (out_info.duration_s - duration).abs() > tolerance {
        bail!("verify: duration {:.1}s, expected {:.1}s", out_info.duration_s, duration);
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

    let model = search::vmaf_model(info);
    let pix = search::reference_pix_fmt(info);
    let mut scores = Vec::new();
    for (i, frac) in [0.3, 0.7].into_iter().enumerate() {
        let secs = 5.0_f64.min(duration);
        let start = ((duration - secs) * frac).max(0.0);
        // The same picture sits at different times in the source when sections were removed.
        let (start, src_start, secs) = cut.map_or((start, start, secs), |c| c.window(start, secs));
        let r = tmp.join(format!("verify{i}.ref.mkv"));
        let d = tmp.join(format!("verify{i}.out.mkv"));
        search::extract_reference(tools, input, info, true, search::first_frame(info), src_start, secs, &r)?;
        search::extract_reference(tools, out, info, false, search::first_frame(&out_info), start, secs, &d)?;
        let mut v = tools.vmaf(&d, &r, model, pix, threads, 0, 1)?;
        // Seeking two different files can land a frame or two apart; retry nearby alignments
        // before calling the encode bad.
        if v < target_vmaf - 4.0 {
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
    let floor = target_vmaf - 4.0;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::AudioInfo;

    fn info(audio: &[(&str, u32, f64)]) -> MediaInfo {
        MediaInfo {
            video_index: 0, codec: "h264".into(), width: 1920, height: 1080, fps: 30.0, duration_s: 60.0,
            video_bitrate: 5e6, pix_fmt: "yuv420p".into(), hdr: false, dolby_vision: false, color_transfer: String::new(), interlaced: false, vfr: false,
            start_time: 0.0, video_start: 0.0, covers: vec![], attachments: 0, tags: Default::default(),
            audio_streams: audio.len() as u32, subtitle_streams: 0,
            audio: audio.iter().map(|&(codec, channels, bitrate)| AudioInfo { codec: codec.into(), channels, bitrate }).collect(),
            subtitle_codecs: vec![],
        }
    }

    /// The codec and bitrate arguments of each audio stream, e.g. "copy" or "libopus 96k".
    fn audio_plan(i: &MediaInfo, mode: Audio) -> (Vec<String>, Vec<String>) {
        let mut notes = Vec::new();
        let s = stream_args(i, 0, Container::Mp4, mode, &[], None, &mut notes);
        let value = |key: String| s.args.iter().position(|a| *a == key).map(|p| s.args[p + 1].clone());
        let plan = (0..i.audio.len())
            .map(|n| [value(format!("-c:a:{n}")), value(format!("-b:a:{n}"))].into_iter().flatten().collect::<Vec<_>>().join(" "))
            .collect();
        (plan, notes)
    }

    #[test]
    fn sound_is_copied_unless_asked_to_shrink() {
        let i = info(&[("aac", 2, 256_000.0), ("mp3", 2, 320_000.0), ("opus", 6, 510_000.0), ("flac", 2, 900_000.0)]);
        let (plan, notes) = audio_plan(&i, Audio::Keep);
        assert_eq!(plan, ["copy", "copy", "copy", "libopus 128k"]);
        assert_eq!(notes.len(), 1, "{notes:?}");
        let (plan, notes) = audio_plan(&i, Audio::Shrink);
        assert_eq!(plan, ["libopus 96k", "libopus 96k", "libopus 256k", "libopus 128k"]);
        assert!(notes[0].contains("aac 256 kbps -> Opus 96 kbps"), "{notes:?}");
    }

    #[test]
    fn lean_and_unmeasured_sound_is_left_alone() {
        // 144 kbps is the line for stereo; a track whose bitrate isn't known is never touched.
        let i = info(&[("aac", 2, 128_000.0), ("aac", 2, 144_000.0), ("aac", 2, 145_000.0), ("aac", 2, 0.0), ("aac", 1, 64_000.0), ("aac", 1, 96_000.0)]);
        let (plan, _) = audio_plan(&i, Audio::Shrink);
        assert_eq!(plan, ["copy", "copy", "libopus 96k", "copy", "copy", "libopus 56k"]);
        assert_eq!([1, 2, 6, 8, 20].map(lean_opus_kbps), [56, 96, 256, 336, 384]);
    }
}
