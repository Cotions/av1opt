//! Single-file optimize: CRF search, full encode, verification. Never modifies the input.

use crate::events::Event;
use crate::probe::{self, MediaInfo};
use crate::history;
use crate::repair;
use crate::metadata::{self, MetaOptions, Resolved};
use crate::scenes;
use crate::sections::{self, Sections};
use crate::source::{Recorded, WholeHash};
use crate::search::{self, EncodeSettings, SearchParams, Trial};
use crate::tools::{FAILED_INSIDE, NOT_WRITTEN, Tools, hms, path_arg};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub struct Options {
    /// The top output folder of the batch may not be made: a job nobody just asked for, whose
    /// folder gone means a drive that is away. Checked before each file; folders below it are
    /// still made.
    pub out_dir_must_exist: bool,
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
    /// Sample clips the CRF search goes by; `None` for as many as the video's length asks
    /// for ([`search::sample_count`]).
    pub samples: Option<usize>,
    /// None: 4 s, or what a search for the mean alone takes (see `search::MEAN_SAMPLE_SECS`).
    pub sample_secs: Option<f64>,
    pub min_savings: f64,
    pub force: bool,
    pub overwrite: bool,
    pub keep_temp: bool,
    /// Per-scene CRF (chunked encode) instead of one CRF for the whole file.
    pub scenes: Scenes,
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

/// Whether each scene gets its own CRF.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug, clap::ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scenes {
    /// One CRF for the whole video.
    #[default]
    Off,
    /// A CRF per scene where the search's samples differ in quality at one CRF, one CRF
    /// for the whole video where they agree.
    Auto,
    /// A CRF per scene.
    Always,
}

impl Scenes {
    pub fn possible(self) -> bool {
        self != Scenes::Off
    }
}

// Queues and settings saved before `auto` existed hold true or false.
impl<'de> serde::Deserialize<'de> for Scenes {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Old {
            Flag(bool),
            Name(String),
        }
        match Old::deserialize(d)? {
            Old::Flag(true) => Ok(Scenes::Always),
            Old::Flag(false) => Ok(Scenes::Off),
            Old::Name(n) => match n.as_str() {
                "off" => Ok(Scenes::Off),
                "auto" => Ok(Scenes::Auto),
                "always" => Ok(Scenes::Always),
                other => Err(serde::de::Error::custom(format!("scenes: off, auto or always, not {other}"))),
            },
        }
    }
}

/// How far below the samples' mean the worst sample may score before a video counts as
/// uneven. Whole encodes of eight clips, every frame scored: where the gap at the chosen CRF
/// was 0.3 to 1.6 VMAF, no five seconds of the encode fell more than 3 below the target;
/// where it was 2.9 and 4.1, stretches fell 8.5 and 5.5 below, and 3-4% of five-second
/// windows would have failed verification. Encoded per scene, the worst five seconds of
/// those two came out 3.4 and 3.2 below the target.
///
/// That was with four samples of eight seconds, and 2 was the line. Four seconds score
/// lower where a video is hard for a moment, and the worst of eight is lower than the worst
/// of four: on 13 real recordings the gap went from 2.1 to 4.4 for the same video. Of
/// those 13, the three that had failed a spot check in one pass came out at 4.4, 4.6 and
/// 5.1, one whose spot check had passed 1.3 above the line at 5.5, one that four long
/// samples had marked as well at 4.2, and the other eight at 3.3 or less. 4 is also where
/// a sample is as far below the mean as verification lets a spot check be below the target.
pub const UNEVEN_GAP: f64 = 4.0;

/// How far below the target a spot check of the finished encode may score.
pub const VERIFY_SLACK: f64 = 4.0;
/// How far above that line the search keeps the worst sample (see `hold_worst_sample`).
const HOLD_MARGIN: f64 = 1.0;

/// Where the finished encode is compared with the source, as (start, length) in seconds of
/// the output: five seconds at 30% and at 70% of the way, or all of a video too short for
/// the two to be different stretches. A video of 25 minutes or more gets one for every ten
/// minutes, eight at most, spread the same way (the first as far from the start as the last
/// from the end, one and a third times as far from each other): two checks say little
/// about two hours.
pub fn spot_checks(duration: f64) -> Vec<(f64, f64)> {
    const SECS: f64 = 5.0;
    if duration <= 2.0 * SECS {
        return vec![(0.0, duration.max(0.0))];
    }
    let n = ((duration / 600.0).round() as usize).clamp(2, 8);
    (0..n).map(|i| ((duration - SECS) * (i as f64 + 0.75) / (n as f64 + 0.5), SECS)).collect()
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
    /// When the source was last changed, in seconds since 1970. Reports from before this was
    /// recorded have none, which their readers take as not known.
    source_mtime: Option<u64>,
    /// A hash of the source's size and of samples of its contents: what tells a file only
    /// touched from one written to. See `source`.
    source_fingerprint: Option<&'a str>,
    /// A hash of the whole source, which settles whether a file with a new date is still this
    /// one. None when it could not be read to its end during the encode.
    source_hash: Option<&'a str>,
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
    /// Frame-weighted VMAF of the final chunks (scene mode), or of the whole video where a
    /// one-pass encode was scored along its whole length.
    mean_vmaf: Option<f64>,
    /// One pass: the whole video scored against the source, and the stretches below the
    /// floor that were encoded again.
    #[serde(skip_serializing_if = "Option::is_none")]
    whole: Option<&'a repair::Summary>,
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

/// Whether the source is still what the encode began reading. A download that had only stalled
/// goes on growing under the encode, and what comes out is then a copy of part of it. Here any
/// other size or time fails the encode, to the exact value the file system gives: the file was
/// being written to or at least touched, and hours of reading it may have seen either version.
/// Chunks made from the earlier state are of no use to the next run and go with it, but only
/// when the file was seen to hold something else: a drive that dropped out or a file only
/// touched says nothing against them, and they may be days of work.
fn check_source_unchanged(input: &Path, before: &Recorded, work: Option<&Path>, keep_temp: bool) -> Result<()> {
    let shown = input.display();
    let meta = match std::fs::metadata(input) {
        Ok(meta) => meta,
        Err(e) => bail!("{shown} could not be looked at after its encode ({e}), so the result was thrown away; its drive may have dropped out. Finished chunks are kept, and encoding it again goes on from them"),
    };
    if before.size == Some(meta.len()) && before.modified_exactly == meta.modified().ok() {
        return Ok(());
    }
    let changed = format!("{shown} changed or went away while it was being encoded, so the result was thrown away; encode it again once it is complete");
    match crate::source::samples_match(before, input, &meta) {
        Some(false) => {
            if let Some(work) = work.filter(|_| !keep_temp) {
                let _ = std::fs::remove_dir_all(work);
            }
            bail!("{changed}")
        }
        Some(true) => bail!("{changed}"),
        None => bail!("{changed}. Whether its contents differ could not be checked, so finished chunks were kept"),
    }
}

/// The hash of the source begun with the encode, once it is there. A source that cannot be read
/// to its end costs the report that value and not the encode.
fn finish_source_hash(tools: &Tools, input: &Path, hash: WholeHash, notes: &mut Vec<String>) -> Result<Option<String>> {
    if !hash.all_read() {
        tools.emit(Event::Phase { phase: "verifying".into(), detail: "reading the rest of the source for its report".into() });
        eprintln!("reading the rest of {} for the report", input.display());
    }
    match hash.finish(&|| tools.check_cancel().is_err()) {
        Ok(hash) => Ok(Some(hash)),
        Err(e) => {
            tools.check_cancel()?;
            let n = format!("the source could not be read to its end for the report ({e}): if its date changes later, av1opt will not be able to tell whether it is still this file");
            eprintln!("note: {n}");
            tools.emit(Event::Note { text: n.clone() });
            notes.push(n);
            Ok(None)
        }
    }
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

/// Whether two paths lead to one file, however they are spelled: through a link, through
/// `..`, in other letter case on a drive that does not tell case apart, or as two names of
/// one file. Two strings that differ say nothing about that; the file's own number does.
/// (A path that is not there is no file, so it is not the same one.)
pub(crate) fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// What to do with a finished encode, going by its size (see [`size_verdict`]).
#[derive(Debug, PartialEq)]
enum SizeVerdict {
    Keep,
    /// Kept, with a note: smaller than the source, by less than was asked.
    KeepShort(String),
    /// Not kept, and why.
    Drop(String),
}

/// A finished file against its source. The search skips what its samples say would save
/// less than `min_savings`, but it only sees the video stream at one CRF: stretches encoded
/// again, the other streams and the container come on top. A file no smaller than its
/// source is not kept (replacing the original with it would lose quality for nothing); one
/// that is smaller by less than was asked is, since the work is done and the bytes are real.
/// `anyway`: the size was not what the encode was for (`--force`, removed sections).
fn size_verdict(made: u64, source: u64, min_savings: f64, anyway: bool) -> SizeVerdict {
    if anyway {
        return SizeVerdict::Keep;
    }
    let pct = |part: f64| format!("{:.0}%", part * 100.0);
    let savings = 1.0 - made as f64 / source.max(1) as f64;
    // (A --min-savings below nothing says a larger file is welcome.)
    if made >= source && min_savings >= 0.0 {
        SizeVerdict::Drop(format!(
            "the finished file is no smaller than its source ({:.1} MB against {:.1} MB): not kept",
            made as f64 / 1e6,
            source as f64 / 1e6
        ))
    } else if made < source && savings < min_savings {
        SizeVerdict::KeepShort(format!("the finished file is {} smaller than its source, less than the --min-savings of {} the samples had promised", pct(savings), pct(min_savings)))
    } else {
        SizeVerdict::Keep
    }
}

macro_rules! skip {
    ($($arg:tt)*) => { return Err(Skip(format!($($arg)*)).into()) };
}

const COPY_AUDIO: &[&str] = &["aac", "opus", "mp3"];
const TEXT_SUBS: &[&str] = &["subrip", "ass", "ssa", "mov_text", "webvtt", "text"];

/// One CRF has to carry the video's worst stretch: verification wants every spot check
/// within `VERIFY_SLACK` of the target, and a sample below that says some would not be.
/// Found here, it costs a trial or two; found by verification, the whole encode. Lowers
/// `best` to the CRF that keeps the worst sample up, where there is one.
fn hold_worst_sample(
    lab: &mut search::Lab,
    enc: &EncodeSettings,
    params: SearchParams,
    o: &Options,
    best: &mut search::Trial,
    trials: &mut Vec<search::Trial>,
    notes: &mut Vec<String>,
) -> Result<()> {
    // A spot check of the same stretch reads a little above or below the samples (0.9 below,
    // on a generated clip): a worst sample on the line, or just above it, can still fail.
    let aim = o.target_vmaf - VERIFY_SLACK + HOLD_MARGIN;
    if best.min_vmaf >= aim {
        return Ok(());
    }
    eprintln!("worst sample VMAF {:.2} at CRF {}: looking for the CRF that keeps it at {aim} or above", best.min_vmaf, best.crf);
    let (held, all) = search::search(lab, enc, &SearchParams { worst_sample: Some(aim), start_crf: best.crf.floor() as u8, ..params })?;
    *trials = all;
    match held {
        search::Outcome::Found(b) => {
            notes.push(format!("CRF {} instead of {} to keep the worst sample at VMAF {aim} (it was {:.1})", b.crf, best.crf, best.min_vmaf));
            *best = b;
        }
        search::Outcome::TooLarge(t) => skip!(
            "CRF {} leaves the worst sample at VMAF {:.2} (wanted: {aim}) at {:.0}% of the source's size already: savings would be below --min-savings {:.0}%",
            t.crf, t.min_vmaf, t.size_ratio * 100.0, o.min_savings * 100.0
        ),
        // Nothing to gain from a lower CRF that still misses: verification decides.
        search::Outcome::OutOfReach => notes.push(format!("the worst sample stays below VMAF {aim} down to CRF {}", o.min_crf)),
    }
    Ok(())
}

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

/// Whether the output's report can have its name, tried in `tmp`, which is on the same
/// filesystem. It is the longest name a run gives anything, and the last thing written: found
/// out then, the encode would be done and stand there without a report.
fn fits_beside_output(output: &Path, tmp: &Path) -> Result<()> {
    let report = output.with_extension("av1opt.json");
    let trial = tmp.join(report.file_name().context("output has no file name")?);
    match std::fs::write(&trial, b"") {
        Err(e) if e.kind() == std::io::ErrorKind::InvalidFilename => {
            bail!("the file name is too long: the report beside the output would be called {}, more than this filesystem takes; rename the file", report.display())
        }
        written => written?,
    }
    Ok(std::fs::remove_file(trial)?)
}

/// Re-encodes one file into `out_dir`. Returns the output path.
/// `may_write` is asked before folders are made: a job's output folder can be gone by then, and
/// what is made in its place would land under the mount point of a drive.
pub fn optimize(tools: &Tools, input: &Path, out_dir: &Path, may_write: &dyn Fn() -> Result<()>, o: &Options) -> Result<PathBuf> {
    let (size, source) = Recorded::of(input).with_context(|| format!("cannot read {}", input.display()))?;
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
    if info.picture_secs() < 1.0 {
        skip!("no usable duration");
    }
    // Removing sections is an edit the user asked for, so the output is wanted even when the
    // re-encode alone wouldn't be worth it.
    let cut = if o.remove.is_empty() { None } else { Some(Sections::new(&o.remove, info.duration_s)?) };
    // (The parts kept are counted along the whole file; samples and checks taken where there
    // is sound and no picture would have nothing in them.)
    if cut.is_some() && info.duration_s - info.picture_end() > 1.0 {
        bail!(
            "the picture stops at {}, sound or subtitles run on to {}: sections cannot be removed from such a file",
            hms(info.picture_end()), hms(info.duration_s)
        );
    }
    if info.codec == "av1" && !o.force && cut.is_none() {
        skip!("already AV1 (use --force to re-encode anyway)");
    }
    let out_duration = cut.as_ref().map_or(info.duration_s, Sections::kept);

    let stem = input.file_stem().context("input has no file name")?.to_string_lossy();
    let output = out_dir.join(format!("{stem}.{}", o.container.ext()));
    if output.exists() && !o.overwrite {
        bail!("{} already exists (use --overwrite)", output.display());
    }
    if std::fs::canonicalize(input)? == std::path::absolute(&output)? || same_file(input, &output) {
        bail!("the output would be written over this same video, so nothing was written; choose another output folder");
    }
    may_write()?;
    std::fs::create_dir_all(out_dir)?;
    let tmp = TempDir { path: out_dir.join(format!(".av1opt-tmp-{}", std::process::id())), keep: o.keep_temp };
    std::fs::create_dir_all(&tmp.path)?;
    fits_beside_output(&output, &tmp.path)?;
    // Read beside the encode, which takes far longer, so that nobody waits for it.
    let whole_hash = WholeHash::start(input, size);

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
    let mut notes = Vec::new();
    if info.duration_s - info.picture_end() > 1.0 {
        notes.push(format!("the picture stops at {}, sound or subtitles run on to {}: the tracks that are kept keep their length", hms(info.picture_end()), hms(info.duration_s)));
    }
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
    // Why scenes can't be encoded apart here, if they can't.
    let one_pass = if cut.is_some() { Some("sections removed") } else { None };
    if o.scenes == Scenes::Always && let Some(why) = one_pass {
        notes.push(format!("{why}: encoded in one pass instead of per scene"));
    }
    let work = (o.scenes.possible() && one_pass.is_none()).then(|| work_dir(input, out_dir, size, o, preset, svt_params)).transpose()?;
    let saved: Option<scenes::Plan> = work.as_ref().and_then(|w| {
        let p: scenes::Plan = serde_json::from_slice(&std::fs::read(w.path.join("plan.json")).ok()?).ok()?;
        (p.key == w.key).then_some(p)
    });

    // A one-pass encode that is checked along its whole length afterwards (see `whole_check`
    // below) asks only the video's mean of the search: many short clips find that better than
    // a few long ones (see `search::MEAN_SAMPLE_SECS`). Clips asked for by number or length
    // are laid out as they always were.
    let mean_only = o.scenes == Scenes::Off && cut.is_none() && !info.hdr && o.samples.is_none() && o.sample_secs.is_none();
    let t0 = tools.stopwatch();
    let (best, mut trials, mut lab, params, positions) = match &saved {
        Some(p) => {
            eprintln!("resuming earlier run: crf {} from the saved search", p.best.crf);
            (search::Outcome::Found(p.best), p.trials.clone(), None, None, None)
        }
        None => {
            // A trial that misses the target while already too large to be worth it settles
            // the matter: the CRF the target needs is lower, and its encode larger.
            let max_size_ratio = (!o.force && cut.is_none()).then_some(1.0 - o.min_savings);
            let guess = crate::estimate::CrfModel::fit(&history::load(), o.target_vmaf, svt_params);
            let start_crf = guess.start(&info, o.min_crf, o.max_crf);
            let learned = if guess.samples == 0 { String::new() } else { format!(", going by {} past encode{}", guess.samples, if guess.samples == 1 { "" } else { "s" }) };
            // The stretches below the floor are encoded again after a one-pass encode, which
            // lifts its mean: the search aims at the mean there will be then.
            let lift_to = mean_only.then_some(o.target_vmaf - VERIFY_SLACK + search::LIFT_ABOVE_FLOOR);
            let counted = if mean_only { format!(" once stretches below {} are encoded again", o.target_vmaf - VERIFY_SLACK) } else { String::new() };
            eprintln!("searching CRF for VMAF >= {}{counted} ({model}, preset {preset}), from CRF {start_crf}{learned}", o.target_vmaf);
            let count = |secs: f64| o.samples.unwrap_or_else(|| search::sample_count(secs));
            let secs = search::clip_secs(&info, o.sample_secs.unwrap_or(search::SAMPLE_SECS));
            // What a search that has to mind the worst stretch goes by.
            let lasting = || search::in_file_time(&info, search::sample_positions(info.picture_secs(), count(info.picture_secs()), secs));
            let pos = match &cut {
                Some(c) => c.sample_positions(count(c.kept()), secs),
                None if mean_only => search::in_file_time(&info, search::mean_sample_positions(info.picture_secs(), search::mean_sample_count(info.picture_secs()), search::clip_secs(&info, search::MEAN_SAMPLE_SECS))),
                None => lasting(),
            };
            // A stretch that fails after all is searched for among samples long enough to
            // score like it.
            let again = if mean_only { lasting() } else { pos.clone() };
            let cache = search::TrialCache::open(input, size, &pos, tools.encoder_version(), !o.fresh_search, lift_to);
            let mut lab = search::Lab::new(tools, input, &info, &tmp.path, pos.clone(), vmaf_threads, cache).lifting(lift_to);
            let params = SearchParams { target_vmaf: o.target_vmaf, min_crf: o.min_crf, max_crf: o.max_crf, start_crf, max_size_ratio, worst_sample: None };
            tools.emit(Event::Phase { phase: "searching".into(), detail: format!("VMAF >= {} ({model}, preset {preset}), from CRF {start_crf}", o.target_vmaf) });
            let (best, trials) = search::search(&mut lab, &enc, &params)?;
            (best, trials, Some(lab), Some(params), Some(again))
        }
    };
    // With sections removed the output is wanted anyway: settle for the best quality tried.
    let reached = matches!(best, search::Outcome::Found(_));
    let mut best = match best {
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
    // Samples far apart at one CRF: stretches of a one-CRF encode fall well below the target
    // while others are better than they need to be.
    // (Not asked of a search for the mean alone: its clips are too short to stand for a
    // stretch, and the weak ones are found in the encode itself.)
    let gap = best.mean_vmaf - best.min_vmaf;
    let uneven = gap >= UNEVEN_GAP && !mean_only;
    let differs = format!("quality differs along the video (worst sample VMAF {:.1}, {:.1} below the samples' mean, at CRF {})", best.min_vmaf, gap, best.crf);
    // A run that had got to its chunks goes on with them.
    let scenes = match o.scenes {
        Scenes::Off => false,
        Scenes::Always => one_pass.is_none(),
        Scenes::Auto => one_pass.is_none() && (uneven || saved.is_some()),
    };
    if uneven {
        match (o.scenes, one_pass) {
            (Scenes::Off, _) => notes.push(format!("{differs}; a CRF per scene (--scenes) would even it out")),
            (Scenes::Auto, None) => notes.push(format!("{differs}: each scene gets its own CRF")),
            (Scenes::Auto, Some(why)) => notes.push(format!("{differs}; {why}, so encoded in one pass all the same")),
            (Scenes::Always, _) => {}
        }
    }
    let work = work.filter(|_| scenes);
    // A one-pass encode is checked along its whole length afterwards, and what falls short
    // is encoded again by itself (see `repair`): the video's one CRF need not carry its
    // worst stretch then. Not with sections removed (the frames of source and encode no
    // longer pair up one to one) nor in HDR (the stretches would lose the mastering data
    // the one pass hands to the encoder).
    let whole_check = !scenes && cut.is_none() && !info.hdr;
    if !scenes
        && !whole_check
        && reached
        && let (Some(lab), Some(params)) = (lab.as_mut(), params)
    {
        hold_worst_sample(lab, &enc, params, o, &mut best, &mut trials, &mut notes)?;
    }
    let predicted_savings = 1.0 - best.size_ratio;
    tools.emit(Event::Chosen { crf: best.crf, vmaf: best.mean_vmaf, predicted_savings });
    eprintln!("chose crf {} (VMAF {:.2}), predicted video savings {:.0}%", best.crf, best.mean_vmaf, predicted_savings * 100.0);
    if predicted_savings < o.min_savings && !o.force && cut.is_none() {
        skip!("predicted savings {:.0}% below --min-savings {:.0}%", predicted_savings * 100.0, o.min_savings * 100.0);
    }

    // What encoding again weak stretches takes is looked at when it is about to start, and
    // only if there are any (see `repair::repair`).
    require_room(out_dir, free_bytes(out_dir), room_to_encode(size, best.size_ratio, scenes, o.workers))?;

    // The sample clips are lossless and large; only scene mode measures on them again.
    if !scenes && let Some(lab) = lab.as_mut() {
        lab.discard_samples();
    }
    let partial = tmp.path.join(format!("partial.{}", o.container.ext()));
    let covers = prepare_covers(tools, input, &info, o.container, &tmp.path, &mut notes);
    let mut t1 = tools.stopwatch();
    let mut search_secs = t0.secs(tools);
    let probe = EncodeSettings { preset: (preset + 4).clamp(preset, 12), pix_fmt_out: enc.pix_fmt_out, params: enc.params.clone() };
    let mut work = work;
    let mut plan = saved;
    if plan.is_none()
        && let Some(w) = &work
    {
        let lab = lab.as_mut().expect("a search that wasn't resumed has its lab");
        may_write()?;
        std::fs::create_dir_all(&w.path)?;
        tools.emit(Event::Phase { phase: "scenes".into(), detail: "finding scene changes".into() });
        let found = scenes::detect(tools, input, &info, &w.path)?;
        // The frames don't keep the constant rate chunks are cut at (see `Pace`). They are
        // cut by their own timestamps then, if those can be gone by and the frames are
        // shown about as long as each other (see `Times::steady`).
        let uneven_pace = found.pace.filter(scenes::Pace::uneven);
        match (uneven_pace, found.times.as_ref().filter(|t| uneven_pace.is_some() && t.steady())) {
            (Some(p), None) => {
                notes.push(format!(
                    "frames come at uneven times ({} frames; at a constant {} fps {} would be dropped and {} shown twice): encoded in one pass instead of per scene",
                    p.source_frames, search::cfr_label(&info), p.dropped, p.repeated
                ));
                let _ = std::fs::remove_dir_all(&w.path);
                work = None;
                // One CRF after all, so it is the one that has to carry the worst stretch.
                let was = best.crf;
                if reached && let Some(params) = params {
                    hold_worst_sample(lab, &enc, params, o, &mut best, &mut trials, &mut notes)?;
                }
                lab.discard_samples();
                if best.crf != was {
                    let predicted_savings = 1.0 - best.size_ratio;
                    tools.emit(Event::Chosen { crf: best.crf, vmaf: best.mean_vmaf, predicted_savings });
                    eprintln!("chose crf {} (VMAF {:.2}), predicted video savings {:.0}%", best.crf, best.mean_vmaf, predicted_savings * 100.0);
                    if predicted_savings < o.min_savings && !o.force {
                        skip!("predicted savings {:.0}% below --min-savings {:.0}%", predicted_savings * 100.0, o.min_savings * 100.0);
                    }
                }
            }
            (pace, times) => {
                // How much better the real preset scores than the fast probe preset at the
                // same CRF; probe scores are shifted by this to predict the real encode.
                let (pt, _) = lab.trial(&probe, best.crf)?;
                let bias = best.mean_vmaf - pt.mean_vmaf;
                lab.discard_samples();
                let (first_pts, chunks, total_frames, timed) = match (pace, times) {
                    (Some(p), Some(t)) => {
                        eprintln!(
                            "frames come at uneven times (at a constant {} fps {} of {} would be dropped and {} shown twice): each keeps its own",
                            search::cfr_label(&info), p.dropped, p.source_frames, p.repeated
                        );
                        let (first, chunks) = scenes::bounds_timed(&info, &found.cuts, t);
                        (first, chunks, t.pts.len() as u64, Some(t.tb))
                    }
                    _ => {
                        let (first, chunks) = scenes::bounds(&info, &found.cuts, found.total);
                        (first, chunks, found.total, None)
                    }
                };
                eprintln!("{} scene cuts, {total_frames} frames, {} chunks", found.cuts.len(), chunks.len());
                let p = scenes::Plan { key: w.key.clone(), best, trials: trials.clone(), bias, first_pts, total_frames, chunks, timed };
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
        if plan.timed.is_some() {
            notes.push("frames come at uneven times: scenes are cut and joined by the source's own timestamps, every frame where it was".into());
        }
        // Finding the scenes belongs to the search; the encode's clock starts here, so that
        // stretch is not counted twice.
        search_secs = t0.secs(tools);
        t1 = tools.stopwatch();
        let ctx = scenes::Ctx {
            tools, input, info: &info, work: &work.path, enc: &enc, probe: &probe, target: o.target_vmaf,
            min_crf: o.min_crf, max_crf: o.max_crf, vmaf_threads, workers: o.workers,
        };
        let chunks = scenes::encode_all(&ctx, &plan)?;
        let short: Vec<f64> = chunks.iter().filter(|c| c.out_of_reach).map(|c| c.vmaf).collect();
        if let Some(lowest) = short.iter().copied().min_by(f64::total_cmp) {
            notes.push(format!(
                "{} of {} scenes stay below VMAF {} at any CRF (lowest: {lowest:.1}): encoded where more bits stop adding quality",
                short.len(), chunks.len(), o.target_vmaf
            ));
        }
        tools.emit(Event::Phase { phase: "muxing".into(), detail: format!("joining {} chunks", chunks.len()) });
        let joined = scenes::concat(&work.path, chunks.len())?;
        let video = match plan.timed {
            Some(_) => {
                let stamped = scenes::stamp(tools, &work.path, &joined, &info)?;
                let _ = std::fs::remove_file(&joined);
                stamped
            }
            None => joined,
        };
        let (args, streams) = mux_args(&video, plan.timed.is_none(), input, &info, o.container, o.audio, &covers, &meta, &o.meta, &partial, &mut notes);
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
    let mut encode_secs = t1.secs(tools);
    if chunks.is_some() {
        for n in &notes {
            eprintln!("note: {n}");
            tools.emit(Event::Note { text: n.clone() });
        }
    }

    let lifted_crf = mean_only.then_some(best.crf);
    let floor_for = |best: &search::Trial| o.target_vmaf.min(best.mean_vmaf) - VERIFY_SLACK;
    let remux = |video: &Path, out: &Path| mux_args(video, false, input, &info, o.container, o.audio, &covers, &meta, &o.meta, out, &mut Vec::new()).0;
    // The CRF of a search that counted weak samples as encoded again (see `search::lifted_mean`).
    // The whole video where it can be gone by, spot checks where not.
    let check = |best: &search::Trial, notes: &mut Vec<String>| -> Result<(Vec<f64>, MediaInfo, Option<repair::Summary>)> {
        if whole_check {
            eprintln!("verifying: the whole video against its source");
            tools.emit(Event::Phase { phase: "verifying".into(), detail: "streams, duration, VMAF of the whole video".into() });
            if let Some((scores, out_info, summary)) =
                verify_whole(tools, input, &info, &partial, &streams, &enc, best.crf, floor_for(best), o.min_crf, vmaf_threads, &tmp.path, &remux, notes)?
            {
                return Ok((scores, out_info, Some(summary)));
            }
        }
        eprintln!("verifying");
        tools.emit(Event::Phase { phase: "verifying".into(), detail: "streams, duration, VMAF spot checks".into() });
        verify(tools, input, &info, &partial, &streams, expect_frames, cut.as_ref(), o.target_vmaf.min(best.mean_vmaf), vmaf_threads, &tmp.path).map(|(v, i)| (v, i, None))
    };
    let t_check = tools.stopwatch();
    let mut verified = check(&best, &mut notes);
    if whole_check {
        encode_secs += t_check.secs(tools);
    }
    // A stretch the samples missed: now that it is known, it is a sample. One CRF found
    // with it, one more encode; a second failure stands.
    if let Err(e) = &verified
        && let Some(low) = e.downcast_ref::<LowSpot>()
        && chunks.is_none()
        && cut.is_none()
        && let (Some(pos), Some(params)) = (&positions, params)
    {
        let t2 = tools.stopwatch();
        // Twice the margin: this stretch has read lower as a spot check than samples like
        // it promised, and a third encode is not on offer.
        let aim = o.target_vmaf - VERIFY_SLACK + 2.0 * HOLD_MARGIN;
        let (at, lowest) = low.worst();
        eprintln!("spot check at {} scored VMAF {lowest:.2}, a stretch the samples missed: searching again with it as a sample", hms(at));
        tools.emit(Event::Phase { phase: "searching".into(), detail: format!("again, with the stretch at {} (VMAF {lowest:.2}) as a sample", hms(at)) });
        let mut pos = pos.clone();
        pos.extend(apart(&low.windows, AGAIN_SAMPLES).iter().map(|w| (w.0, w.1)));
        let cache = search::TrialCache::open(input, size, &pos, tools.encoder_version(), !o.fresh_search, None);
        let mut lab = search::Lab::new(tools, input, &info, &tmp.path, pos, vmaf_threads, cache);
        let start_crf = (best.crf - ((aim - lowest) / search::VMAF_PER_CRF).max(1.0)).floor().max(o.min_crf as f64) as u8;
        let (found, all) = search::search(&mut lab, &enc, &SearchParams { worst_sample: Some(aim), start_crf, ..params })?;
        lab.discard_samples();
        match found {
            search::Outcome::Found(b) if b.crf < best.crf => {
                let predicted_savings = 1.0 - b.size_ratio;
                tools.emit(Event::Chosen { crf: b.crf, vmaf: b.mean_vmaf, predicted_savings });
                eprintln!("chose crf {} (VMAF {:.2}), predicted video savings {:.0}%", b.crf, b.mean_vmaf, predicted_savings * 100.0);
                if predicted_savings < o.min_savings && !o.force {
                    skip!(
                        "a spot check at {} scored VMAF {lowest:.2} at CRF {}; the CRF that holds that stretch ({}) saves {:.0}%, below --min-savings {:.0}%",
                        hms(at), best.crf, b.crf, predicted_savings * 100.0, o.min_savings * 100.0
                    );
                }
                let n = format!(
                    "CRF {} instead of {}: encoded a second time, after a spot check at {} scored VMAF {lowest:.1} (a stretch the samples had missed)",
                    b.crf, best.crf, hms(at)
                );
                eprintln!("note: {n}");
                tools.emit(Event::Note { text: n.clone() });
                notes.push(n);
                best = b;
                trials = all;
                search_secs += t2.secs(tools);
                let t3 = tools.stopwatch();
                let (args, _) = full_encode_args(input, &info, &enc, best.crf, o.container, o.audio, &covers, &meta, &o.meta, None, &partial, &mut Vec::new());
                tools.emit(Event::Phase { phase: "encoding".into(), detail: format!("crf {}, preset {preset}", best.crf) });
                tools.run_with_progress(&args, out_duration, "encoding")?;
                verified = check(&best, &mut notes);
                encode_secs = t3.secs(tools);
            }
            search::Outcome::TooLarge(t) => skip!(
                "a spot check at {} scored VMAF {lowest:.2} at CRF {}; CRF {} still leaves that stretch at {:.2} at {:.0}% of the source's size: savings would be below --min-savings {:.0}%",
                hms(at), best.crf, t.crf, t.min_vmaf, t.size_ratio * 100.0, o.min_savings * 100.0
            ),
            // The samples have nothing against this CRF, or nothing lower helps: the check stands.
            search::Outcome::Found(_) | search::Outcome::OutOfReach => {}
        }
    }
    // A CRF per scene is what evens a video out, where it was there to be had.
    let hint = o.scenes == Scenes::Off && one_pass.is_none();
    let (verify_vmaf, out_info, repaired) = verified.map_err(|e| match e.downcast::<LowSpot>() {
        Ok(low) => LowSpot { hint, ..low }.into(),
        Err(e) => e,
    })?;
    // The search had counted its weak samples as encoded again, and they were not.
    if whole_check && repaired.is_none() && lifted_crf == Some(best.crf) {
        let n = format!(
            "the CRF was chosen for the mean the video would have with its stretches below VMAF {:.0} encoded again, which could not be done here: its mean may be below {}",
            floor_for(&best),
            o.target_vmaf
        );
        eprintln!("note: {n}");
        tools.emit(Event::Note { text: n.clone() });
        notes.push(n);
    }
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

    // In this order: a change while the hash was being read is then seen by the check.
    let source_hash = finish_source_hash(tools, input, whole_hash, &mut notes)?;
    check_source_unchanged(input, &source, work.as_ref().map(|w| w.path.as_path()), o.keep_temp)?;

    // What the samples promised is the video stream at one CRF; the file is that, the
    // stretches encoded again, the other streams and the container.
    let made = std::fs::metadata(&partial)?.len();
    match size_verdict(made, size, o.min_savings, o.force || cut.is_some()) {
        SizeVerdict::Keep => {}
        SizeVerdict::KeepShort(n) => {
            eprintln!("note: {n}");
            tools.emit(Event::Note { text: n.clone() });
            notes.push(n);
        }
        SizeVerdict::Drop(why) => {
            // Chunks kept for a run that is to go on would only lead here again.
            if let Some(w) = &work
                && !o.keep_temp
            {
                let _ = std::fs::remove_dir_all(&w.path);
            }
            skip!("{why}")
        }
    }

    // The place was free when the encode began, hours ago perhaps. A file that has turned up
    // there since is not this run's to write over: the encode is kept under another name.
    if o.overwrite {
        std::fs::rename(&partial, &output).or_else(|_| std::fs::copy(&partial, &output).map(|_| ()))?;
    } else if let Err(e) = crate::trash::rename_new(&partial, &output) {
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(e).with_context(|| format!("cannot put the encode at {}", output.display()));
        }
        let aside = out_dir.join(format!("{stem}.av1opt-{}.{}", std::process::id(), o.container.ext()));
        crate::trash::rename_new(&partial, &aside).with_context(|| format!("{} appeared during the encode, and the encode could not be kept", output.display()))?;
        bail!("{} appeared during the encode; the finished encode is at {}", output.display(), aside.display());
    }
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
        source_mtime: source.modified,
        source_fingerprint: source.fingerprint.as_deref(),
        source_hash: source_hash.as_deref(),
        output_bytes,
        savings: 1.0 - output_bytes as f64 / size as f64,
        mode: if chunks.is_some() { "scenes" } else { "whole" },
        crf: best.crf,
        preset,
        svt_params,
        target_vmaf: o.target_vmaf,
        vmaf_model: model,
        mean_vmaf: chunks.as_deref().map(scenes::mean_vmaf).or(repaired.as_ref().map(|r| r.mean_vmaf)),
        whole: repaired.as_ref(),
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
    let report_at = output.with_extension("av1opt.json");
    // A report that says where an original went (it is in the trash, and can be put back) is
    // the only record of that: it is set aside, not written over.
    let records_original = std::fs::read(&report_at).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()).is_some_and(|r| r.get("replaced").is_some_and(|v| !v.is_null()));
    if records_original {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        match crate::replace::set_aside(&report_at, &output, now) {
            Ok(aside) => eprintln!("note: the report of an earlier encode, which says where its original is, was kept as {}", aside.display()),
            Err(e) => eprintln!("warning: the report of an earlier encode, which says where its original is, could not be kept: {e}"),
        }
    }
    crate::files::write_new(&report_at, &serde_json::to_vec_pretty(&report)?)?;
    // Chunk sizes are exact; container-reported bitrates can include audio (Matroska).
    let video_bitrate = match &chunks {
        Some(c) => c.iter().map(|r| r.bytes).sum::<u64>() as f64 * 8.0 / info.picture_secs(),
        None if o.container == Container::Mkv => probe::stream_bytes(&tools.ffprobe, &output, out_info.video_index)
            .map_or(out_info.video_bitrate, |bytes| bytes as f64 * 8.0 / out_info.picture_secs().max(0.001)),
        None => out_info.video_bitrate,
    };
    let mut record = history::Record::new(&info, video_bitrate, o.target_vmaf, preset, report.mode, best.crf, svt_params);
    // With sections removed the times are for a shorter video than the record describes.
    if cut.is_none() {
        (record.search_secs, record.encode_secs) = (Some(search_secs), Some(encode_secs));
    }
    record.scenes = Some(match o.scenes { Scenes::Off => "off", Scenes::Auto => "auto", Scenes::Always => "always" }.to_string());
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
        std::path::absolute(input)?.display(), o.target_vmaf, o.min_crf, o.max_crf, o.samples.map_or("auto".to_string(), |n| n.to_string()), o.sample_secs.unwrap_or(search::SAMPLE_SECS)
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
    // Passed through, a frame's time is still counted in the encoder's units, and those are
    // one frame of the declared rate unless it is told otherwise: a frame that comes a
    // quarter of a frame late was moved to the nearest whole one. In the source's own units
    // every frame keeps the time it had.
    if let (n, d) = info.time_base
        && n > 0
        && d > 0
    {
        a.extend(["-enc_time_base:v:0".into(), format!("{n}:{d}")]);
    }
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

/// Joined scene-chunk video plus audio, subtitles and metadata from the source. `grid`: the
/// video is a bare stream at the constant rate; otherwise it carries a timestamp per frame.
#[allow(clippy::too_many_arguments)]
fn mux_args(
    video: &Path,
    grid: bool,
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
    let mut a: Vec<String> = ["-y", "-v", "error", "-itsoffset", &format!("{offset:.6}")].map(String::from).to_vec();
    if grid {
        a.extend(["-framerate".into(), search::cfr(info)]);
    }
    a.extend(["-i", &path_arg(video), "-i", &path_arg(input), "-map", "0:v", "-c:v", "copy"].map(String::from));
    // A bare stream says nothing about fields and the header would say "unknown"; what an
    // encode in one pass says of itself holds here too.
    a.extend(["-field_order:v:0", "progressive"].map(String::from));
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
    if !info.other_streams.is_empty() {
        notes.push(format!("left out: {} (the video, its sound, subtitles, covers and attachments are what is kept)", info.other_streams.join(", ")));
    }
    // What a track is marked as (the default one, forced subtitles) comes along with a track
    // that is mapped from the source; one that comes out of the cut has to be told.
    let marked = |marks: Option<&String>| marks.filter(|m| !m.is_empty()).map_or("0".to_string(), String::clone);
    for (i, s) in info.audio.iter().enumerate() {
        let kbps = (s.channels.max(1) * 64).clamp(64, 384);
        if cut_subs.is_some() {
            a.extend(["-map".into(), format!("[ao{i}]")]);
            a.extend([format!("-c:a:{i}"), "libopus".into(), format!("-b:a:{i}"), format!("{kbps}k")]);
            a.extend([format!("-disposition:a:{i}"), marked(Some(&s.disposition))]);
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
        if cut_subs.is_some() {
            a.extend([format!("-disposition:s:{kept_subs}"), marked(info.subtitle_dispositions.get(i))]);
        }
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

/// Spot checks of the finished encode that scored below what verification allows.
#[derive(Debug)]
struct LowSpot {
    /// Where in the source, how long, and the score.
    windows: Vec<(f64, f64, f64)>,
    floor: f64,
    /// Whether a CRF per scene would have been the way out.
    hint: bool,
}

/// At most this many of the checks that failed become samples of the search that follows.
const AGAIN_SAMPLES: usize = 8;

/// The lowest `most` of `windows` (start, length, score) that do not overlap one another, in
/// order of time. The check of a whole video is a window every second, five seconds long: one
/// weak stretch is there several times over, and each time as a sample is each time encoded.
fn apart(windows: &[(f64, f64, f64)], most: usize) -> Vec<(f64, f64, f64)> {
    let mut by_score = windows.to_vec();
    by_score.sort_by(|a, b| a.2.total_cmp(&b.2));
    let mut kept: Vec<(f64, f64, f64)> = Vec::new();
    for w in by_score {
        if kept.len() < most && !kept.iter().any(|k| w.0 < k.0 + k.1 && k.0 < w.0 + w.1) {
            kept.push(w);
        }
    }
    kept.sort_by(|a, b| a.0.total_cmp(&b.0));
    kept
}

impl LowSpot {
    /// Start and score of the lowest check.
    fn worst(&self) -> (f64, f64) {
        self.windows.iter().min_by(|a, b| a.2.total_cmp(&b.2)).map_or((0.0, 0.0), |w| (w.0, w.2))
    }
}

impl std::fmt::Display for LowSpot {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let hint = if self.hint { "; a CRF per scene (--scenes) evens quality out" } else { "" };
        write!(f, "the finished encode scores VMAF {:.2} in the weakest stretch checked, below the {:.1} it has to reach; {NOT_WRITTEN}{hint}", self.worst().1, self.floor)
    }
}

impl std::error::Error for LowSpot {}

/// Checks the finished file before it replaces anything: its video codec, duration, streams
/// and, where the number is known, frames.
fn check_layout(tools: &Tools, info: &MediaInfo, out: &Path, streams: &Streams, expect_frames: Option<u64>, cut: Option<&Sections>) -> Result<MediaInfo> {
    let out_info = probe::probe(&tools.ffprobe, out, std::fs::metadata(out)?.len())?;
    if out_info.codec != "av1" {
        bail!("the finished video is {} where it should be AV1; {NOT_WRITTEN}", out_info.codec);
    }
    // (A share of the length, for containers that round it; never so much that a film could
    // lose its last half minute unnoticed.)
    let (length, expected) = lengths_to_compare(info, &out_info, cut);
    let tolerance = (expected * 0.005).clamp(1.0, 5.0);
    // A source that understates its picture gives an output longer than expected: nothing
    // was lost if the whole files still agree. A cut that was not made is longer too, and
    // is not let through this way.
    let whole = (out_info.duration_s - info.duration_s).abs() <= (info.duration_s * 0.005).clamp(1.0, 5.0);
    let understated = cut.is_none() && length > expected && whole;
    if (length - expected).abs() > tolerance && !understated {
        let hint = if length < expected { " (the source may be cut off or damaged: check that it plays to the end)" } else { "" };
        bail!("the finished video lasts {length:.1} seconds where it should last {expected:.1}; {NOT_WRITTEN}{hint}");
    }
    if out_info.audio_streams as usize != streams.audio || out_info.subtitle_streams as usize != streams.subs {
        bail!(
            "the finished file has {} audio and {} subtitle streams where it should have {} and {}; {NOT_WRITTEN}",
            out_info.audio_streams, out_info.subtitle_streams, streams.audio, streams.subs
        );
    }
    if out_info.color_transfer != info.color_transfer && info.hdr {
        bail!("the finished video lost its HDR signalling (its transfer is '{}', the source's is '{}'); {NOT_WRITTEN}", out_info.color_transfer, info.color_transfer);
    }
    if out_info.covers.len() != streams.covers {
        bail!("the finished file has {} cover images where it should have {}; {NOT_WRITTEN}", out_info.covers.len(), streams.covers);
    }
    if let Some(want) = expect_frames {
        let got = tools.count_frames(out)?;
        if got != want {
            bail!("the finished video has {got} frames where it should have {want}; {NOT_WRITTEN}");
        }
    }
    Ok(out_info)
}

/// How long a finished file is and how long it should be. The picture is what was encoded:
/// the length of the container can come from a track that was not kept (image subtitles do
/// not go into MP4) or be an estimate (TS, AVI). Where either file does not say how long its
/// picture is, the containers are compared.
fn lengths_to_compare(info: &MediaInfo, out: &MediaInfo, cut: Option<&Sections>) -> (f64, f64) {
    if info.video_s > 0.0 && out.video_s > 0.0 {
        // Sections are times in the file, and the picture may start after the file does.
        let kept = |c: &Sections| c.kept_within(info.picture_end()) - c.kept_within(info.picture_start());
        (out.picture_secs(), cut.map_or(info.picture_secs(), kept))
    } else {
        (out.duration_s, cut.map_or(info.duration_s, Sections::kept))
    }
}

/// VMAF of `secs` seconds of the output from `start` against the source from `src_start`,
/// both cut out and compared by themselves. `name` keeps the cuts of different checks apart.
#[allow(clippy::too_many_arguments)]
fn spot_check(
    tools: &Tools,
    input: &Path,
    info: &MediaInfo,
    out: &Path,
    out_info: &MediaInfo,
    (start, src_start, secs): (f64, f64, f64),
    retry_below: f64,
    threads: usize,
    tmp: &Path,
    name: &str,
) -> Result<f64> {
    let (model, pix) = (search::vmaf_model(info), search::reference_pix_fmt(info));
    let r = tmp.join(format!("{name}.ref.mkv"));
    let d = tmp.join(format!("{name}.out.mkv"));
    search::extract_reference(tools, input, info, true, search::first_frame(info), src_start, secs, &r)?;
    search::extract_reference(tools, out, info, false, search::first_frame(out_info), start, secs, &d)?;
    let mut v = tools.vmaf(&d, &r, model, pix, threads, 0, 1)?;
    // Seeking two different files can land a frame or two apart; retry nearby alignments
    // before calling the encode bad.
    if v < retry_below {
        for off in [-2, -1, 1, 2] {
            v = v.max(tools.vmaf(&d, &r, model, pix, threads, off, 1)?);
        }
    }
    let _ = std::fs::remove_file(&r);
    let _ = std::fs::remove_file(&d);
    Ok(v)
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
    let out_info = check_layout(tools, info, out, streams, expect_frames, cut)?;
    let mut scores = Vec::new();
    let mut low = Vec::new();
    let floor = target_vmaf - VERIFY_SLACK;
    for (i, (start, secs)) in spot_checks(cut.map_or(info.picture_secs(), Sections::kept)).into_iter().enumerate() {
        // The same picture sits at different times in the source when sections were removed,
        // and each file's picture starts where that file puts it.
        let along = (start + search::first_frame(&out_info), start + search::first_frame(info), secs);
        let (start, src_start, secs) = cut.map_or(along, |c| c.window(start, secs));
        let v = spot_check(tools, input, info, out, &out_info, (start, src_start, secs), floor, threads, tmp, &format!("verify{i}"))?;
        eprintln!("  spot check at {}: VMAF {v:.2}", hms(start));
        tools.emit(Event::SpotCheck { at_secs: start, vmaf: v });
        scores.push(v);
        if v < floor {
            low.push((src_start, secs, v));
        }
    }
    // Short spot checks are noisier than the multi-sample search, hence the slack.
    if !low.is_empty() {
        // One CRF for a video whose stretches need different ones; the same run would end
        // the same way.
        return Err(LowSpot { windows: low, floor, hint: false }.into());
    }
    Ok((scores, out_info))
}

/// How far below what a stretch scored by itself it may read in the joined video before the
/// join is taken for broken: frames a step apart read ten or more lower.
const JOIN_SLACK: f64 = 3.0;

/// Verification of a one-pass encode along its whole length: every frame is scored against
/// the source's, and stretches below the floor are encoded again and put in place (see
/// [`repair`]). The scores returned are those of the usual spot-check places. `None` where
/// the whole video can't be gone by, for the spot checks to decide.
///
/// `remux` gives the ffmpeg arguments that put a bare video stream together with the
/// source's other streams.
#[allow(clippy::too_many_arguments)]
fn verify_whole(
    tools: &Tools,
    input: &Path,
    info: &MediaInfo,
    out: &Path,
    streams: &Streams,
    enc: &EncodeSettings,
    crf: f64,
    floor: f64,
    min_crf: u8,
    threads: usize,
    tmp: &Path,
    remux: &dyn Fn(&Path, &Path) -> Vec<String>,
    notes: &mut Vec<String>,
) -> Result<Option<(Vec<f64>, MediaInfo, repair::Summary)>> {
    let took = tools.stopwatch();
    let mut out_info = check_layout(tools, info, out, streams, None, None)?;
    let why_not = |e: &anyhow::Error| e.to_string().lines().last().unwrap_or("").to_string();
    let survey = match repair::survey(tools, input, info, out, threads, tmp) {
        Ok(Some(s)) => s,
        Ok(None) => {
            eprintln!("the frames of the encode can't be set against the source's one for one: spot checks instead of the whole video");
            return Ok(None);
        }
        Err(e) if crate::events::is_cancelled(&e) => return Err(e),
        Err(e) => {
            eprintln!("warning: the whole video could not be checked ({}): spot checks instead", why_not(&e));
            return Ok(None);
        }
    };
    let frames = survey.times.pts.len() as u64;
    let ctx = repair::Ctx { tools, input, info, enc, crf, floor, min_crf, threads, dir: tmp };
    let outcome = match repair::repair(&ctx, out, &survey) {
        Ok(o) => o,
        Err(e) if crate::events::is_cancelled(&e) => return Err(e),
        Err(e) => match e.downcast::<repair::StillLow>() {
            // In the source's time, as the search takes its samples.
            Ok(low) => return Err(LowSpot { windows: low.windows.iter().map(|w| (w.0 + search::first_frame(info), w.1, w.2)).collect(), floor, hint: false }.into()),
            Err(e) => {
                let n = format!("stretches below VMAF {floor:.0} could not be encoded again ({}): judged by spot checks", why_not(&e));
                eprintln!("note: {n}");
                tools.emit(Event::Note { text: n.clone() });
                notes.push(n);
                return Ok(None);
            }
        },
    };
    let s = &outcome.summary;
    if let Some(video) = &outcome.video {
        tools.emit(Event::Phase { phase: "muxing".into(), detail: format!("joining {} stretches encoded again", s.stretches.len()) });
        let joined = tmp.join(format!("repaired.{}", out.extension().map_or("mp4".into(), |e| e.to_string_lossy())));
        tools.run(&remux(video, &joined))?;
        let _ = std::fs::remove_file(video);
        std::fs::rename(&joined, out)?;
        out_info = check_layout(tools, info, out, streams, Some(frames), None)?;
        // The stretches were scored by themselves. Read once more where they now are, the
        // first and the last say whether every frame sits where it should.
        let mut ends = vec![&s.stretches[0]];
        if s.stretches.len() > 1 {
            ends.push(&s.stretches[s.stretches.len() - 1]);
        }
        for (i, st) in ends.into_iter().enumerate() {
            // Half the clip before the stretch and half in it (or, for a stretch the video
            // begins with, across its end): were the stretch's frames a step off, no one shift
            // of the clip would set both halves right, as it would a clip from inside it.
            let half = repair::WINDOW_SECS / 2.0;
            let across = if st.start_secs >= half { st.start_secs - half } else { (st.start_secs + st.secs - half).max(0.0) };
            let span = repair::WINDOW_SECS.min(outcome.length() - across);
            let (begin, secs, alone) = match outcome.at(across, span).filter(|_| span >= half) {
                Some(alone) => (across, span, alone),
                None => {
                    let secs = st.secs.min(repair::WINDOW_SECS);
                    let Some(alone) = outcome.at(st.start_secs, secs) else { continue };
                    (st.start_secs, secs, alone)
                }
            };
            let at = (begin + search::first_frame(&out_info), begin + search::first_frame(info), secs);
            let v = spot_check(tools, input, info, out, &out_info, at, alone - JOIN_SLACK, threads, tmp, &format!("joined{i}"))?;
            eprintln!("  joined video at {}: VMAF {v:.2} ({alone:.2} by itself)", hms(at.0));
            if v < alone - JOIN_SLACK {
                bail!("the stretch at {} was encoded again but scores VMAF {v:.2} once put back into the video, against {alone:.2} on its own; {FAILED_INSIDE}", hms(st.start_secs));
            }
        }
        let (count, total) = (s.stretches.len(), s.stretches.iter().map(|x| x.secs).sum::<f64>());
        let (lo, hi) = s.stretches.iter().fold((f64::INFINITY, 0.0_f64), |(lo, hi), x| (lo.min(x.crf), hi.max(x.crf)));
        let crfs = if lo == hi { format!("{lo}") } else { format!("{lo}-{hi}") };
        let grown = s.stretches.iter().map(|x| x.bytes as f64 - x.bytes_before as f64).sum::<f64>() / 1e6;
        let n = format!(
            "{count} stretch{} ({} in all) scored below VMAF {floor:.0} and {} encoded again at CRF {crfs}: the lowest {:.0} s of the video went from VMAF {:.1} to {:.1}, for {grown:+.1} MB",
            if count == 1 { "" } else { "es" },
            hms(total),
            if count == 1 { "was" } else { "were" },
            repair::WINDOW_SECS,
            s.lowest_before,
            s.lowest_vmaf,
        );
        eprintln!("note: {n}");
        tools.emit(Event::Note { text: n.clone() });
        notes.push(n);
    }
    let short: Vec<f64> = s.stretches.iter().filter(|x| x.out_of_reach).map(|x| x.vmaf).chain(s.left.iter().copied()).collect();
    if let Some(lowest) = short.iter().copied().min_by(f64::total_cmp) {
        let n = format!(
            "{} stretch{} below VMAF {floor:.0} at any CRF worth its bytes (lowest: {lowest:.1}): kept where more bits stop adding quality",
            short.len(),
            if short.len() == 1 { " stays" } else { "es stay" }
        );
        eprintln!("note: {n}");
        tools.emit(Event::Note { text: n.clone() });
        notes.push(n);
    }
    if s.near > 0 {
        let n = format!(
            "{} of the video's {:.0} s windows stay{} just under VMAF {floor:.0} after three rounds of encoding again (lowest of the video: {:.1}): kept as {}",
            s.near,
            repair::WINDOW_SECS,
            if s.near == 1 { "s" } else { "" },
            s.lowest_vmaf,
            if s.near == 1 { "it is" } else { "they are" }
        );
        eprintln!("note: {n}");
        tools.emit(Event::Note { text: n.clone() });
        notes.push(n);
    }
    eprintln!("whole video as kept: mean VMAF {:.2}, lowest {:.0} s {:.2}", s.mean_vmaf, repair::WINDOW_SECS, s.lowest_vmaf);
    let mut scores = Vec::new();
    for (start, secs) in spot_checks(info.picture_secs()) {
        if let Some(v) = outcome.at(start, secs) {
            tools.emit(Event::SpotCheck { at_secs: start, vmaf: v });
            scores.push(v);
        }
    }
    let mut summary = outcome.summary;
    summary.secs = took.secs(tools);
    Ok(Some((scores, out_info, summary)))
}

/// Room an encode needs in the output folder: the predicted file and slack, as the size
/// can be off for unusual content. Scene mode holds the chunks, the joined stream and the
/// final file at once, and lossless chunk references of up to ~4 GB each (4K) per worker.
fn room_to_encode(size: u64, size_ratio: f64, scenes: bool, workers: usize) -> u64 {
    let copies = if scenes { 3.0 } else { 1.5 };
    let references = if scenes { workers as u64 * 4_000_000_000 } else { 0 };
    (size as f64 * size_ratio.max(0.3) * copies) as u64 + 500_000_000 + references
}

/// Fails when `free` bytes (if the system could tell) are fewer than `need`.
pub(crate) fn require_room(dir: &Path, free: Option<u64>, need: u64) -> Result<()> {
    if let Some(free) = free
        && free < need
    {
        bail!("not enough free space in {}: {:.1} GB free, ~{:.1} GB needed", dir.display(), free as f64 / 1e9, need as f64 / 1e9);
    }
    Ok(())
}

pub(crate) fn free_bytes(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: valid NUL-terminated path and a properly sized out-parameter.
    (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0).then(|| st.f_bavail as u64 * st.f_frsize as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_change_to_a_source_during_its_encode_fails_it_and_only_new_contents_cost_the_chunks() {
        use std::io::Write;
        use std::time::{Duration, SystemTime};
        let dir = std::env::temp_dir().join(format!("av1opt-source-stamp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("a.mkv");
        let work = dir.join(".av1opt-work-1");
        let chunks = || std::fs::create_dir_all(&work).unwrap();
        std::fs::write(&input, b"video").unwrap();
        let then = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        std::fs::File::options().write(true).open(&input).unwrap().set_modified(then).unwrap();
        let before = Recorded::of(&input).unwrap().1;
        chunks();
        let check = |keep| check_source_unchanged(&input, &before, Some(work.as_path()), keep).map_err(|e| e.to_string());
        check(false).unwrap();
        assert_eq!(before.modified, Some(1_700_000_000));

        // A new date alone, even within the two seconds a comparison later would allow: what
        // was read may be from before or after whatever touched it. Its chunks are not at fault.
        for off in [1, 3600] {
            std::fs::File::options().write(true).open(&input).unwrap().set_modified(then + Duration::from_secs(off)).unwrap();
            assert!(check(false).unwrap_err().contains("changed or went away"));
            assert!(work.is_dir());
        }
        // Nothing to compare with: said so, and the chunks stay.
        let no_samples = Recorded { fingerprint: None, ..before.clone() };
        let e = check_source_unchanged(&input, &no_samples, Some(work.as_path()), false).unwrap_err().to_string();
        assert!(e.contains("changed or went away") && e.contains("could not be checked") && e.contains("kept"), "{e}");
        assert!(work.is_dir());

        // Written in place at the same size, as into a file allocated ahead: the fingerprint
        // tells. Asked to keep its work, the encode keeps its chunks.
        std::fs::write(&input, b"VIDEO").unwrap();
        std::fs::File::options().write(true).open(&input).unwrap().set_modified(then + Duration::from_secs(60)).unwrap();
        assert!(check(true).unwrap_err().contains("changed or went away"));
        assert!(work.is_dir());
        assert!(check(false).unwrap_err().contains("changed or went away"));
        assert!(!work.exists());

        // Grown, with its date put back: the size tells.
        chunks();
        let mut file = std::fs::File::options().append(true).open(&input).unwrap();
        file.write_all(b"more").unwrap();
        file.set_modified(then).unwrap();
        assert!(check(false).unwrap_err().contains("changed or went away"));
        assert!(!work.exists());

        // Out of reach, as on a drive that dropped out: nothing shows that it changed, so
        // the encode fails and the chunks stay.
        chunks();
        std::fs::remove_file(&input).unwrap();
        let e = check(false).unwrap_err();
        assert!(e.contains("could not be looked at") && e.contains("kept"), "{e}");
        assert!(work.is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_encode_asks_for_the_room_it_needs_itself() {
        const GB: u64 = 1_000_000_000;
        // One and a half times the predicted file and slack, with or without a check after.
        assert_eq!(room_to_encode(10 * GB, 0.5, false, 1), 8 * GB);
        // A prediction of next to nothing is not believed.
        assert_eq!(room_to_encode(10 * GB, 0.01, false, 1), 5 * GB);
        assert_eq!(room_to_encode(10 * GB, 0.5, true, 2), 15 * GB + GB / 2 + 8 * GB);
        assert!(require_room(Path::new("/out"), Some(8 * GB), 8 * GB).is_ok());
        assert!(require_room(Path::new("/out"), None, 8 * GB).is_ok());
        let e = require_room(Path::new("/out"), Some(GB), 8 * GB).unwrap_err().to_string();
        assert_eq!(e, "not enough free space in /out: 1.0 GB free, ~8.0 GB needed");
    }

    #[test]
    fn a_finished_file_is_measured_by_its_picture() {
        let media = |duration_s, video_s| MediaInfo { duration_s, video_s, ..info(&[]) };
        let fits = |source: &MediaInfo, out: &MediaInfo, cut: Option<&Sections>| {
            let (length, expected) = lengths_to_compare(source, out, cut);
            (length - expected).abs() <= (expected * 0.005).clamp(1.0, 5.0)
        };
        // An image subtitle track runs an hour past the picture and does not go into MP4.
        assert!(fits(&media(7200.0, 3600.0), &media(3600.0, 3600.0), None));
        // Sound that runs on is kept, and does not count either.
        assert!(fits(&media(130.0, 100.0), &media(130.0, 100.0), None));
        // A container's length that is only an estimate.
        assert!(fits(&media(5400.0, 5000.0), &media(5000.0, 5000.0), None));
        // A film that lost its end does not pass for any of these reasons.
        assert!(!fits(&media(7200.0, 3600.0), &media(3500.0, 3500.0), None));
        assert!(!fits(&media(130.0, 100.0), &media(130.0, 90.0), None));

        // What a cut removes comes off, and past the end of the picture it removes nothing.
        let cut = Sections::new(&[(10.0, 20.0), (110.0, f64::INFINITY)], 130.0).unwrap();
        assert_eq!(lengths_to_compare(&media(130.0, 100.0), &media(90.0, 90.0), Some(&cut)), (90.0, 90.0));
        // The picture runs from 20 s to 70 s of the file: the section before it takes nothing off.
        let late = MediaInfo { video_start: 20.0, ..media(120.0, 50.0) };
        assert_eq!(lengths_to_compare(&late, &media(50.0, 50.0), Some(&cut)), (50.0, 50.0));

        // Where either picture's length is not known, the containers are what is known.
        assert_eq!(lengths_to_compare(&media(600.0, 0.0), &media(599.0, 599.0), None), (599.0, 600.0));
        assert_eq!(lengths_to_compare(&media(130.0, 100.0), &media(130.0, 0.0), None), (130.0, 130.0));
        assert_eq!(lengths_to_compare(&media(130.0, 0.0), &media(100.0, 0.0), Some(&cut)), (100.0, cut.kept()));
    }

    #[test]
    fn one_file_under_two_names_is_one_file() {
        let dir = std::env::temp_dir().join(format!("av1opt-samefile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("videos/sub")).unwrap();
        let a = dir.join("videos/a.mkv");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(dir.join("videos/b.mkv"), b"a").unwrap();
        std::os::unix::fs::symlink(dir.join("videos"), dir.join("same")).unwrap();
        std::fs::hard_link(&a, dir.join("twin.mkv")).unwrap();
        assert!(same_file(&a, &a));
        // The cases a comparison of the two strings let through.
        assert!(same_file(&a, &dir.join("same/a.mkv")));
        assert!(same_file(&a, &dir.join("videos/sub/../a.mkv")));
        assert!(same_file(&a, &dir.join("twin.mkv")));
        // Another file with the same bytes is another file; one that is not there is none.
        assert!(!same_file(&a, &dir.join("videos/b.mkv")));
        assert!(!same_file(&a, &dir.join("videos/c.mkv")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn checks_that_overlap_make_one_sample() {
        // One weak stretch seen by five windows a second apart, and another far from it.
        let low: Vec<(f64, f64, f64)> = vec![(10.0, 5.0, 88.0), (11.0, 5.0, 86.5), (12.0, 5.0, 85.0), (13.0, 5.0, 86.0), (14.0, 5.0, 88.5), (300.0, 5.0, 87.0)];
        assert_eq!(apart(&low, 8), [(12.0, 5.0, 85.0), (300.0, 5.0, 87.0)]);
        // Windows that only touch are apart; the lowest come first when there are too many.
        let row: Vec<(f64, f64, f64)> = (0..6).map(|i| (i as f64 * 5.0, 5.0, 80.0 + i as f64)).collect();
        assert_eq!(apart(&row, 8).len(), 6);
        assert_eq!(apart(&row, 2), [(0.0, 5.0, 80.0), (5.0, 5.0, 81.0)]);
        assert!(apart(&[], 8).is_empty());
    }

    #[test]
    fn a_file_no_smaller_than_its_source_is_not_kept() {
        assert_eq!(size_verdict(700, 1000, 0.15, false), SizeVerdict::Keep);
        assert_eq!(size_verdict(850, 1000, 0.15, false), SizeVerdict::Keep);
        // Smaller, by less than asked: the stretches encoded again were not in the forecast.
        assert!(matches!(size_verdict(900, 1000, 0.15, false), SizeVerdict::KeepShort(n) if n.contains("10% smaller") && n.contains("15%")));
        assert!(matches!(size_verdict(1000, 1000, 0.15, false), SizeVerdict::Drop(_)));
        assert!(matches!(size_verdict(1200, 1000, 0.0, false), SizeVerdict::Drop(_)));
        // Asked for whatever the size: --force, removed sections, a --min-savings below zero.
        assert_eq!(size_verdict(1200, 1000, 0.15, true), SizeVerdict::Keep);
        assert_eq!(size_verdict(1200, 1000, -1.0, false), SizeVerdict::Keep);
        assert_eq!(size_verdict(1200, 1000, -0.1, false), SizeVerdict::Keep);
        assert!(matches!(size_verdict(1, 0, 0.15, false), SizeVerdict::Drop(_)));
    }

    #[test]
    fn spot_checks_do_not_look_at_the_same_stretch_twice() {
        assert_eq!(spot_checks(100.0), [(28.5, 5.0), (66.5, 5.0)]);
        assert_eq!(spot_checks(15.0), [(3.0, 5.0), (7.0, 5.0)]);
        // Six seconds used to be checked from 0.3 s and again from 0.7 s.
        assert_eq!(spot_checks(6.0), [(0.0, 6.0)]);
        assert_eq!(spot_checks(10.0), [(0.0, 10.0)]);
        assert_eq!(spot_checks(0.4), [(0.0, 0.4)]);
        // Two up to 25 minutes, then one for every ten, eight at most.
        assert_eq!([600.0, 1499.0, 1501.0, 3600.0, 4500.0, 7200.0, 36000.0].map(|d| spot_checks(d).len()), [2, 2, 3, 6, 8, 8, 8]);
        let hour = spot_checks(3605.0);
        assert_eq!(hour.iter().map(|c| c.0.round()).collect::<Vec<_>>(), [415.0, 969.0, 1523.0, 2077.0, 2631.0, 3185.0]);
        assert!(hour.iter().all(|c| c.1 == 5.0));
    }

    #[test]
    fn scenes_setting_reads_what_older_queues_saved() {
        let read = |j: &str| serde_json::from_str::<Scenes>(j).unwrap();
        assert_eq!((read("true"), read("false")), (Scenes::Always, Scenes::Off));
        assert_eq!((read("\"off\""), read("\"auto\""), read("\"always\"")), (Scenes::Off, Scenes::Auto, Scenes::Always));
        assert!(serde_json::from_str::<Scenes>("\"sometimes\"").is_err());
        assert_eq!(serde_json::to_string(&Scenes::Auto).unwrap(), "\"auto\"");
    }
    use crate::probe::AudioInfo;

    fn info(audio: &[(&str, u32, f64)]) -> MediaInfo {
        MediaInfo {
            video_index: 0, codec: "h264".into(), width: 1920, height: 1080, fps: 30.0, duration_s: 60.0,
            video_bitrate: 5e6, pix_fmt: "yuv420p".into(), hdr: false, dolby_vision: false, color_transfer: String::new(), interlaced: false, vfr: false,
            start_time: 0.0, video_start: 0.0, covers: vec![], attachments: 0, tags: Default::default(), video_s: 0.0, nominal_fps: 0.0, time_base: (0, 0),
            audio_streams: audio.len() as u32, subtitle_streams: 0,
            audio: audio.iter().map(|&(codec, channels, bitrate)| AudioInfo { codec: codec.into(), channels, bitrate, disposition: String::new() }).collect(),
            subtitle_codecs: vec![], subtitle_dispositions: vec![], other_streams: vec![],
        }
    }

    #[test]
    fn a_cut_keeps_what_tracks_are_marked_as_and_says_what_is_left_out() {
        let mut i = info(&[("aac", 2, 128_000.0), ("aac", 2, 128_000.0)]);
        i.audio[1].disposition = "default+original".into();
        i.subtitle_codecs = vec!["subrip".into(), "hdmv_pgs_subtitle".into(), "ass".into()];
        i.subtitle_dispositions = vec![String::new(), "default".into(), "forced".into()];
        i.other_streams = vec!["data (tmcd)".into(), "video (h264)".into()];
        let value = |s: &Streams, key: &str| s.args.iter().position(|a| a == key).map(|p| s.args[p + 1].clone());
        // Mapped from the source, tracks keep their marks without being told.
        let mut notes = Vec::new();
        let plain = stream_args(&i, 0, Container::Mkv, Audio::Keep, &[], None, &mut notes);
        assert!(!plain.args.iter().any(|a| a.starts_with("-disposition")), "{:?}", plain.args);
        assert!(notes.iter().any(|n| n.starts_with("left out: data (tmcd), video (h264)")), "{notes:?}");
        // Out of a cut they are told: the second sound track is the default one, and the
        // third subtitle track (the second kept: the image one cannot be cut) is forced.
        let cut = stream_args(&i, 0, Container::Mkv, Audio::Keep, &[], Some(&[Some(1), None, Some(2)]), &mut Vec::new());
        assert_eq!(value(&cut, "-disposition:a:0").as_deref(), Some("0"));
        assert_eq!(value(&cut, "-disposition:a:1").as_deref(), Some("default+original"));
        assert_eq!(value(&cut, "-disposition:s:0").as_deref(), Some("0"));
        assert_eq!(value(&cut, "-disposition:s:1").as_deref(), Some("forced"));
        assert_eq!(cut.subs, 2);
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
