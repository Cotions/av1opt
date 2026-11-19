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

/// Name prefix of the folder a run keeps its temporary files in: see
/// [`crate::files::make_run_folder`].
const TEMP_PREFIX: &str = ".av1opt-tmp-";

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
/// More than the plan of any video takes.
const PLAN_LIMIT: u64 = 64 << 20;

/// Written next to the output so later tuning can learn from real results.
#[derive(Serialize)]
struct Report<'a> {
    format: u32,
    /// The av1opt version that wrote it.
    av1opt: &'static str,
    input: &'a Path,
    output: &'a Path,
    source_bytes: u64,
    #[serde(flatten)]
    source: ReportedSource<'a>,
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

/// What a report keeps to tell later whether a file is still the source of its encode.
#[derive(Serialize)]
struct ReportedSource<'a> {
    /// When the source was last changed, in seconds since 1970. Reports from before this was
    /// recorded have none, which their readers take as not known.
    source_mtime: Option<u64>,
    /// A hash of the source's size and of samples of its contents: what tells a file only
    /// touched from one written to. See `source`.
    source_fingerprint: Option<&'a str>,
    /// A hash of the whole source, which settles whether a file with a new date is still this
    /// one. None when it could not be read to its end during the encode.
    source_hash: Option<&'a str>,
}

impl<'a> ReportedSource<'a> {
    fn of(source: &'a Recorded, whole_hash: Option<&'a str>) -> ReportedSource<'a> {
        ReportedSource { source_mtime: source.modified, source_fingerprint: source.fingerprint.as_deref(), source_hash: whole_hash }
    }
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

/// Waits for the hash of the source and then looks whether the source is still what was encoded.
/// In this order: a change while the hash was being read is then seen by the check.
fn hash_then_check<T>(hash: impl FnOnce() -> Result<T>, check: impl FnOnce() -> Result<()>) -> Result<T> {
    let hashed = hash()?;
    check()?;
    Ok(hashed)
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

/// Another run has this output. Not a decision about the file, so it is not remembered: the next
/// run looks at the file again.
#[derive(Debug)]
pub struct InUse(pub &'static str);

impl std::fmt::Display for InUse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for InUse {}

/// Keeps other runs, from the command line or the page, off this output while it is made: they
/// would do the same hours of work, and per-scene runs share their chunk files. It goes by the
/// name without its extension, since `x.mp4` and `x.mkv` share one report.
fn claim_output(out_dir: &Path, stem: &str) -> Result<Option<crate::files::Claim>> {
    let path = out_dir.join(format!(".av1opt-lock-{:016x}", scenes::fnv1a(stem)));
    match crate::files::Claim::take(&path) {
        Ok(Some(claim)) => Ok(Some(claim)),
        Ok(None) => Err(InUse("another av1opt is encoding this now").into()),
        Err(e) => {
            // A filesystem without locks: carry on as a single run would.
            eprintln!("warning: cannot lock {}: {e}; a second run on this folder would not be noticed", path.display());
            Ok(None)
        }
    }
}

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

/// Removes temp folders left by runs that crashed or were interrupted (their process is gone).
pub fn clean_stale_temp(out_dir: &Path) {
    crate::files::remove_abandoned_folders(out_dir, TEMP_PREFIX);
}

/// Whether the output's report can have its name, tried in `tmp`, which is on the same
/// filesystem. It is the longest name a run gives anything, and the last thing written: found
/// out then, the encode would be done and stand there without a report.
fn fits_beside_output(output: &Path, tmp: &Path) -> Result<()> {
    let report = output.with_extension("av1opt.json");
    let trial = tmp.join(report.file_name().context("output has no file name")?);
    match crate::files::write_new(&trial, b"") {
        Err(e) if e.kind() == std::io::ErrorKind::InvalidFilename => {
            bail!("the file name is too long: the report beside the output would be called {}, more than this filesystem takes; rename the file", report.display())
        }
        written => written?,
    }
    Ok(std::fs::remove_file(trial)?)
}

/// The preset chunks are tried at: four steps faster, up to 12, and never slower than the
/// encode itself (presets go to 13).
fn probe_preset(preset: u8) -> u8 {
    (preset + 4).min(12).max(preset)
}

/// Why an encode may not end at `output`: the report it would write is that of an output of
/// another name. `x.mp4` and `x.mkv` in one folder share `x.av1opt.json`, and the second would
/// take the report from the first, which could then never be replaced. A report that says where
/// an original went is exempt: it is kept under a name of its own when the new one is written.
pub fn report_of_another_output(output: &Path) -> Option<String> {
    let at = output.with_extension("av1opt.json");
    // Beside the output, where anybody who can write to the folder can leave a pipe or a link.
    let report: serde_json::Value = serde_json::from_slice(&crate::files::read_regular(&at, crate::replace::REPORT_LIMIT).ok()?).ok()?;
    if report.get("replaced").is_some_and(|v| !v.is_null()) {
        return None;
    }
    let name = Path::new(report["output"].as_str()?).file_name()?;
    let other = output.with_file_name(name);
    (Some(name) != output.file_name() && other.exists()).then(|| {
        format!(
            "{} would need the report {}, which belongs to {}; choose another output folder, or remove {} and its report first",
            output.display(), at.display(), other.display(), other.display()
        )
    })
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
    // A track that cannot be kept is found before the search, not when the encode fails.
    if let Err(why) = plan_sound(&info, o.container, o.audio, cut.is_some()) {
        bail!("{why}; {NOT_WRITTEN}");
    }

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
    let _only_this_run = claim_output(out_dir, &stem)?;
    if output.exists() && !o.overwrite {
        return Err(InUse("another av1opt finished this a moment ago").into());
    }
    let tmp = TempDir { path: crate::files::make_run_folder(out_dir, TEMP_PREFIX).with_context(|| format!("cannot make a temporary folder in {}", out_dir.display()))?, keep: o.keep_temp };
    fits_beside_output(&output, &tmp.path)?;

    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    // 4K VMAF with many threads was part of the memory blow-up in the first pilot.
    let vmaf_threads = if info.width.max(info.height) >= 3200 { threads.min(4) } else { threads };
    // The command line has already left out what is out of date; a job from the page has not.
    let saved = o.tuning.as_ref().and_then(|t| t.pick(info.width, info.height));
    let out_of_date = saved.and_then(|c| c.out_of_date(o.target_vmaf));
    let tuned = saved.filter(|_| out_of_date.is_none());
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
        notes.push(match (tuned, out_of_date) {
            (Some(_), _) => format!("tuned settings for {class}: preset {preset}, {svt_params}"),
            (None, Some(why)) => format!("tuned settings for {class} not used, {why}; `av1opt bench` measures them again: preset {preset}, {svt_params}"),
            (None, None) => format!("no tuned settings for {class}: preset {preset}, {svt_params}"),
        });
    } else if svt_params != search::DEFAULT_PARAMS {
        notes.push(format!("SVT-AV1 parameters: {svt_params}"));
    }
    notes.extend(picture_notes(&info, probe::frame_marks(&tools.ffprobe, input, info.video_index)));
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
    // Why scenes can't be encoded apart here, if they can't. An encode's mastering display and
    // light level reach the container through the encoder, and scenes are joined from bare
    // streams, which have no place for them: the file would keep them only inside its video,
    // where players that go by the container do not look.
    let one_pass = if cut.is_some() {
        Some("sections removed")
    } else if info.hdr {
        Some("an HDR video joined from scenes would lose its mastering data")
    } else {
        None
    };
    if o.scenes == Scenes::Always && let Some(why) = one_pass {
        notes.push(format!("{why}: encoded in one pass instead of per scene"));
    }
    let work = (o.scenes.possible() && one_pass.is_none()).then(|| work_dir(input, out_dir, size, o, preset, svt_params, &mut notes)).transpose()?;
    let saved = work.as_ref().and_then(|w| saved_plan(w, &source));
    if saved.as_ref().is_some_and(|p| p.source_mtime.is_some() && p.source_mtime != source.modified) {
        let n = "the source has another date than when its finished chunks were made, and the same contents where it was sampled, so the chunks are used".to_string();
        eprintln!("note: {n}");
        notes.push(n);
    }

    // A one-pass encode that is checked along its whole length afterwards (see `whole_check`
    // below) asks only the video's mean of the search: many short clips find that better than
    // a few long ones (see `search::MEAN_SAMPLE_SECS`). Clips asked for by number or length
    // are laid out as they always were.
    let mean_only = o.scenes == Scenes::Off && cut.is_none() && !info.hdr && o.samples.is_none() && o.sample_secs.is_none();
    // Such an encode is scored along its whole length and repaired only where no more than
    // a tenth of the source's frames are shown for unequal times; spot checks judge it
    // otherwise. Only where repairs will follow does the search count on them: it counts the
    // weak samples as repaired, and aims low and rounds to the nearest step because the mean
    // is made up after. (A source that was sent to spot checks was encoded 0.2 low and never
    // repaired before this was looked at.)
    let repaired_after = mean_only && repair::steady_source(tools, input, &info);
    if mean_only && !repaired_after {
        eprintln!("more than a tenth of the source's frames are shown for unequal times: the encode will get spot checks, and the search aims at the target itself");
    }
    // How far below the target such a search aims (see `search::AIM_LOW`): the scores it hands
    // on are that much below what the video is expected to end at.
    let aimed_low = if repaired_after { search::AIM_LOW } else { 0.0 };
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
            let lift_to = repaired_after.then_some(o.target_vmaf - VERIFY_SLACK + search::LIFT_ABOVE_FLOOR);
            let counted = if repaired_after { format!(" once stretches below {} are encoded again", o.target_vmaf - VERIFY_SLACK) } else { String::new() };
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
            let params = SearchParams { target_vmaf: o.target_vmaf, min_crf: o.min_crf, max_crf: o.max_crf, start_crf, max_size_ratio, worst_sample: None, landing: search::Landing::SafeSide };
            // Where the mean is measured and brought to the target afterwards, the answer need
            // not lean toward quality, and can start a little short of what it will end at.
            let first = if repaired_after { SearchParams { target_vmaf: o.target_vmaf - search::AIM_LOW, landing: search::Landing::Nearest, ..params } } else { params };
            tools.emit(Event::Phase { phase: "searching".into(), detail: format!("VMAF >= {} ({model}, preset {preset}), from CRF {start_crf}", o.target_vmaf) });
            let (best, trials) = search::search(&mut lab, &enc, &first)?;
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
            (Scenes::Off, None) => notes.push(format!("{differs}; a CRF per scene (--scenes) would even it out")),
            (Scenes::Off, Some(_)) => notes.push(differs),
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
    tools.emit(Event::Chosen { crf: best.crf, vmaf: best.mean_vmaf + aimed_low, predicted_savings });
    eprintln!("chose crf {} (VMAF {:.2}), predicted video savings {:.0}%", best.crf, best.mean_vmaf + aimed_low, predicted_savings * 100.0);
    if predicted_savings < o.min_savings && !o.force && cut.is_none() {
        skip!("predicted savings {:.0}% below --min-savings {:.0}%", predicted_savings * 100.0, o.min_savings * 100.0);
    }

    // What encoding again weak stretches takes is looked at when it is about to start, and
    // only if there are any (see `repair::repair`).
    require_room(out_dir, free_bytes(out_dir), room_to_encode(size, best.size_ratio, scenes, o.workers))?;
    // Read beside the encode, which takes far longer, so that nobody waits for it. Not before
    // this point: a search that ends in a skip would have had it compete for the drive for nothing.
    let whole_hash = WholeHash::start(input, size);

    // The sample clips are lossless and large; only scene mode measures on them again.
    if !scenes && let Some(lab) = lab.as_mut() {
        lab.discard_samples();
    }
    let partial = tmp.path.join(format!("partial.{}", o.container.ext()));
    let covers = prepare_covers(tools, input, &info, o.container, &tmp.path, &mut notes);
    let mut t1 = tools.stopwatch();
    let mut search_secs = t0.secs(tools);
    let probe = EncodeSettings { preset: probe_preset(preset), pix_fmt_out: enc.pix_fmt_out, params: enc.params.clone() };
    let mut work = work;
    let mut plan = saved;
    if plan.is_none()
        && let Some(w) = &mut work
    {
        let lab = lab.as_mut().expect("a search that wasn't resumed has its lab");
        may_write()?;
        w.start_afresh()?;
        if !o.keep_temp {
            w.remove_outdated_folders();
        }
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
                let p = scenes::Plan { key: w.key.clone(), best, trials: trials.clone(), bias, first_pts, total_frames, chunks, timed, source_fingerprint: source.fingerprint.clone(), source_mtime: source.modified };
                crate::files::write_new(&w.path.join("plan.json"), &serde_json::to_vec_pretty(&p)?)?;
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
        let first = if plan.timed.is_some() { first_video_time(tools, &video)? } else { 0.0 };
        let (args, streams) = mux_args(&video, plan.timed.is_none(), first, input, &info, o.container, o.audio, &covers, &meta, &o.meta, &partial, &mut notes);
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

    let mut lifted_crf = repaired_after.then_some(best.crf);
    // (A search aimed low and rounded to the nearest step says little about what the video can
    // reach: its target stands.)
    let goal_for = |best: &search::Trial| if repaired_after { o.target_vmaf } else { o.target_vmaf.min(best.mean_vmaf) };
    let floor_for = |best: &search::Trial| goal_for(best) - VERIFY_SLACK;
    // What the mean loses per CRF step, as the search measured it. A further encode is allowed
    // only in answer to the first check.
    let mean_slope = search::slope_at(&trials, best.crf);
    let may_encode_again = std::cell::Cell::new(whole_check);
    let remux = |video: &Path, out: &Path, said: &mut Vec<String>| -> Result<(Vec<String>, Streams)> {
        Ok(mux_args(video, false, first_video_time(tools, video)?, input, &info, o.container, o.audio, &covers, &meta, &o.meta, out, said))
    };
    // The CRF of a search that counted weak samples as encoded again (see `search::lifted_mean`).
    // The whole video where it can be gone by, spot checks where not.
    let check = |best: &search::Trial, notes: &mut Vec<String>| -> Result<(Vec<f64>, MediaInfo, Option<repair::Summary>)> {
        if whole_check {
            eprintln!("verifying: the whole video against its source");
            tools.emit(Event::Phase { phase: "verifying".into(), detail: "streams, duration, VMAF of the whole video".into() });
            let rules = repair::MeanRules {
                target: goal_for(best),
                slope: mean_slope,
                crf: best.crf,
                min_crf: o.min_crf as f64,
                max_crf: o.max_crf as f64,
                may_encode_again: may_encode_again.get(),
            };
            if let Some((scores, out_info, summary)) =
                verify_whole(tools, input, &info, &partial, &streams, &enc, &rules, floor_for(best), o.min_crf, vmaf_threads, &tmp.path, &remux, notes)?
            {
                return Ok((scores, out_info, Some(summary)));
            }
        }
        eprintln!("verifying");
        tools.emit(Event::Phase { phase: "verifying".into(), detail: "streams, duration, VMAF spot checks".into() });
        verify(tools, input, &info, &partial, &streams, expect_frames, cut.as_ref(), goal_for(best), vmaf_threads, &tmp.path).map(|(v, i)| (v, i, None))
    };
    let t_check = tools.stopwatch();
    let mut verified = check(&best, &mut notes);
    if whole_check {
        encode_secs += t_check.secs(tools);
    }
    may_encode_again.set(false);
    // The scores of the whole video say that another CRF fits it better: one more encode.
    if let Some(again) = verified.as_ref().err().and_then(|e| e.downcast_ref::<EncodeAgain>()) {
        let (crf, why) = (again.crf, again.why.clone());
        let t = tools.stopwatch();
        let n = format!("CRF {crf} instead of {}: encoded a second time, as {why}", best.crf);
        eprintln!("note: {n}");
        tools.emit(Event::Note { text: n.clone() });
        notes.push(n);
        best = search::Trial { crf, ..best };
        lifted_crf = lifted_crf.map(|_| crf);
        tools.emit(Event::Chosen { crf, vmaf: goal_for(&best), predicted_savings: 1.0 - best.size_ratio });
        let (args, _) = full_encode_args(input, &info, &enc, crf, o.container, o.audio, &covers, &meta, &o.meta, None, &partial, &mut Vec::new());
        tools.emit(Event::Phase { phase: "encoding".into(), detail: format!("crf {crf}, preset {preset}") });
        tools.run_with_progress(&args, out_duration, "encoding")?;
        verified = check(&best, &mut notes);
        encode_secs += t.secs(tools);
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

    let source_hash = hash_then_check(|| finish_source_hash(tools, input, whole_hash, &mut notes), || check_source_unchanged(input, &source, work.as_ref().map(|w| w.path.as_path()), o.keep_temp))?;

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

    // Read here, before the file is in a folder others may be able to write to: what av1opt
    // records of its output has to be what it made.
    let output_sha256 = crate::ledger::sha256_of_file(&partial).inspect_err(|e| eprintln!("warning: could not read the encode back for av1opt's own record: {e}")).ok();

    // The place was free when the encode began, hours ago perhaps. A file that has turned up
    // there since is not this run's to write over: the encode is kept under another name.
    // Nor is one --overwrite would have refused at the start.
    if o.overwrite && crate::batch::overwrite_refused(&output, &std::fs::canonicalize(input)?).is_none() {
        // A rename takes the place of whatever has the name, a link included, and never writes
        // through one. The two are in one folder tree, so there is nothing to fall back on.
        std::fs::rename(&partial, &output).with_context(|| format!("cannot put the encode at {}", output.display()))?;
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
    // From the folder and the name this run wrote to: the name itself can have been made a link
    // since, and what the record says of it is what the page serves and moves.
    let abs_output = std::fs::canonicalize(out_dir)?.join(output.file_name().context("the output has no file name")?);
    let report = Report {
        format: REPORT_FORMAT,
        av1opt: env!("CARGO_PKG_VERSION"),
        input: &abs_input,
        output: &abs_output,
        source_bytes: size,
        source: ReportedSource::of(&source, source_hash.as_deref()),
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
    let records_original = crate::files::read_regular(&report_at, crate::replace::REPORT_LIMIT).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()).is_some_and(|r| r.get("replaced").is_some_and(|v| !v.is_null()));
    if records_original {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        match crate::replace::set_aside(&report_at, &output, now) {
            Ok(aside) => eprintln!("note: the report of an earlier encode, which says where its original is, was kept as {}", aside.display()),
            Err(e) => eprintln!("warning: the report of an earlier encode, which says where its original is, could not be kept: {e}"),
        }
    }
    // Whole or not at all: a report cut off by a crash would leave its output unverified.
    crate::files::replace_with(&report_at, &serde_json::to_vec_pretty(&report)?)?;
    let recorded = crate::ledger::note_encoded(&crate::ledger::Encode {
        output: &abs_output,
        report: &abs_output.with_extension("av1opt.json"),
        source: &abs_input,
        source_bytes: size,
        original: &Recorded { whole: source_hash.clone(), ..source.clone() },
        output_bytes,
        output_sha256,
    });
    if let Err(e) = recorded {
        eprintln!("warning: this encode could not be added to av1opt's own record ({e:#}), so the page will not replace its original; `av1opt replace` in a terminal can");
    }
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
        record.check_secs = repaired.as_ref().map(|r| r.secs);
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
#[derive(Default)]
struct WorkDir {
    path: PathBuf,
    key: String,
    /// What comes before and after the source's size and date in the key of this source with
    /// these settings.
    key_start: String,
    key_end: String,
}

impl WorkDir {
    /// Removes what earlier runs left for this same source and these same settings, which no
    /// run will go on from now that a new plan is begun: chunks of the file as it was when it
    /// had another size or, under the keys of earlier versions, another date. Only folders of
    /// this user's that pass the checks for going on from one, and only with a key to show.
    fn remove_outdated_folders(&self) {
        let Some(parent) = self.path.parent() else { return };
        for entry in std::fs::read_dir(parent).into_iter().flatten().flatten() {
            let path = entry.path();
            let is_work_folder = entry.file_name().to_string_lossy().starts_with(WORK_PREFIX);
            if !is_work_folder || path == self.path || !crate::files::check_kept_folder(&path).is_ok_and(|there| there) {
                continue;
            }
            if plan_key(&path).is_some_and(|key| self.is_same_work(&key)) {
                match std::fs::remove_dir_all(&path) {
                    Ok(()) => eprintln!("removed {}: the chunks in it were of an earlier state of this file", path.display()),
                    Err(e) => eprintln!("warning: could not remove the outdated work folder {}: {e}", path.display()),
                }
            }
        }
    }

    /// Whether `key` is that of this source and these settings: between their two parts it holds
    /// only a size, and in the keys of earlier versions a date as well.
    fn is_same_work(&self, key: &str) -> bool {
        let Some(between) = key.strip_prefix(&self.key_start).and_then(|rest| rest.strip_suffix(&self.key_end)) else { return false };
        let numbers: Vec<&str> = between.split('|').collect();
        numbers.len() <= 2 && numbers.iter().all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    }

    /// Readies the folder for a run that has nothing to go on from. What is there was checked
    /// but belongs to no plan of this encode, and chunks without one must not be taken for its
    /// own. Something turning up at the name meanwhile is left there for another name.
    fn start_afresh(&mut self) -> Result<()> {
        if crate::files::check_kept_folder(&self.path).is_ok_and(|there| there) {
            std::fs::remove_dir_all(&self.path)?;
        }
        if let Err(e) = crate::files::make_kept_folder(&self.path) {
            if e.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(e.into());
            }
            self.path = beside(&self.path)?;
            crate::files::make_kept_folder(&self.path)?;
        }
        Ok(())
    }
}

/// Another name for a work folder whose own cannot be used.
fn beside(path: &Path) -> Result<PathBuf> {
    let mut name = path.file_name().context("a work folder has a name")?.to_owned();
    name.push(format!("-{}", crate::files::random_text(8)?));
    Ok(path.with_file_name(name))
}

/// The folder to work in, given the one the settings lead to. That name follows from the
/// settings so that the next run finds the folder, and anyone can work it out too: what is
/// found there is only gone on from if it can be believed.
fn believed_work_path(path: PathBuf, notes: &mut Vec<String>) -> Result<PathBuf> {
    let Err(why) = crate::files::check_kept_folder(&path) else { return Ok(path) };
    let note = format!("the work folder {} was left alone and not gone on from: {why}; this encode starts over in a new one beside it", path.display());
    eprintln!("note: {note}");
    notes.push(note);
    beside(&path)
}

/// The key saved in a work folder, for looking at folders that may be someone else's.
fn plan_key(folder: &Path) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Saved {
        key: String,
    }
    let bytes = crate::files::read_regular(&folder.join("plan.json"), PLAN_LIMIT).ok()?;
    serde_json::from_slice::<Saved>(&bytes).ok().map(|saved| saved.key)
}

/// The plan in a work folder, if it is one for these settings and for the source as it is now.
fn saved_plan(w: &WorkDir, source: &Recorded) -> Option<scenes::Plan> {
    let p: scenes::Plan = serde_json::from_slice(&crate::files::read_regular(&w.path.join("plan.json"), PLAN_LIMIT).ok()?).ok()?;
    (p.key == w.key && plan_fits(&p, source)).then_some(p)
}

/// Whether what a plan was made from is the source as it is now: the same samples, or where
/// those are not known on either side the same date. A source only touched keeps its chunks,
/// which may be days of work. A plan of a version that noted neither has the date in its key.
fn plan_fits(plan: &scenes::Plan, now: &Recorded) -> bool {
    match (&plan.source_fingerprint, &now.fingerprint) {
        (Some(then), Some(now)) => then == now,
        _ => plan.source_mtime.is_none() || plan.source_mtime == now.modified,
    }
}

fn work_dir(input: &Path, out_dir: &Path, size: u64, o: &Options, preset: u8, svt_params: &str, notes: &mut Vec<String>) -> Result<WorkDir> {
    let key_start = format!("v{PLAN_FORMAT}|{}|", std::path::absolute(input)?.display());
    let mut key_end = format!("|{}|{preset}|{}|{}|{}|{}", o.target_vmaf, o.min_crf, o.max_crf, o.samples.map_or("auto".to_string(), |n| n.to_string()), o.sample_secs.unwrap_or(search::SAMPLE_SECS));
    // Only when they differ, so work folders from before this setting existed still resume.
    if svt_params != search::DEFAULT_PARAMS {
        key_end.push_str(&format!("|{svt_params}"));
    }
    let folder_for = |key: &str| out_dir.join(format!("{WORK_PREFIX}{:016x}", scenes::fnv1a(key)));
    let key = format!("{key_start}{size}{key_end}");
    // Created only when chunk work starts, so skipped files leave nothing behind.
    let path = believed_work_path(folder_for(&key), notes)?;
    let mut work = WorkDir { path, key, key_start, key_end };
    // A folder from when the key held the source's date goes on being used, for that same date.
    if !work.path.exists() {
        let mtime = std::fs::metadata(input)?.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
        let earlier_key = format!("{}{size}|{mtime}{}", work.key_start, work.key_end);
        let earlier = folder_for(&earlier_key);
        if crate::files::check_kept_folder(&earlier).is_ok_and(|there| there) && plan_key(&earlier).as_deref() == Some(earlier_key.as_str()) {
            work.path = earlier;
            work.key = earlier_key;
        }
    }
    Ok(work)
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
                // Written by `prepare_cut` as what its name ends in: `srt` or `ass`.
                let format = p.extension().map_or("srt".into(), |e| e.to_string_lossy().into_owned());
                a.extend(["-f".into(), format, "-i".into(), path_arg(p)]);
                n += 1;
                n - 1
            }));
        }
        if let Some(p) = &c.chapters {
            a.extend(["-f".into(), "ffmetadata".into(), "-i".into(), path_arg(p)]);
            chapters = n;
        }
        let mut graph = c.sections.filter_graph(info.video_index, info.audio.len(), &vf, info.duration_s);
        // Sound libopus would refuse is put into a layout it takes on its way out of the cut.
        for (i, (way, _)) in plan_sound(info, container, audio, true).unwrap_or_default().into_iter().enumerate() {
            if let Sound::Opus { layout: Some(OpusLayout { filter: Some(filter), .. }), .. } = way {
                graph = graph.replacen(&format!("[ao{i}]"), &format!("[ap{i}]"), 1) + &format!(";[ap{i}]{filter}[ao{i}]");
            }
        }
        a.extend(["-filter_complex".into(), graph]);
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
    video_first: f64,
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
    // The picture is put where it stands against the sound in the source, by the timestamps
    // both files hold: the source's moved so that it starts at zero, the video's so that its
    // first frame (at `video_first`) comes where the source's first frame does. Left to count
    // from the start, ffmpeg starts an MPEG-TS at the first packet of the streams it reads
    // from it, here everything but the picture: sound 0.28 s behind the picture came out with
    // none.
    let offset = info.video_start - info.start_time - video_first;
    let mut a: Vec<String> = ["-y", "-v", "error", "-copyts", "-itsoffset", &format!("{offset:.6}")].map(String::from).to_vec();
    if grid {
        a.extend(["-framerate".into(), search::cfr(info)]);
    }
    a.extend(["-i".into(), path_arg(video), "-itsoffset".into(), format!("{:.6}", -info.start_time)]);
    a.extend(["-i", &path_arg(input), "-map", "0:v", "-c:v", "copy"].map(String::from));
    // A bare stream says nothing about fields and the header would say "unknown"; what an
    // encode in one pass says of itself holds here too.
    a.extend(["-field_order:v:0", "progressive"].map(String::from));
    // Nor about the shape of its pixels, which the one pass takes along with its frames: a
    // DVD's 720x576 shown 4:3 came out square-pixelled at 5:4.
    if let Some(aspect) = display_aspect(info) {
        a.extend(["-aspect:v:0".into(), aspect]);
    }
    let mut streams = stream_args(info, 1, container, audio, covers, None, notes);
    // How the picture is shown came with the source's video stream on the one pass; a bare
    // stream has none of it, and ffmpeg can set the 3D layout but not the 360 degree mapping.
    if info.view_marks.iter().any(|m| m == "Stereo 3D") {
        match stereo_layout(&info.stereo3d) {
            Some(layout) => a.extend(["-stereo3d:v:0".into(), layout.into()]),
            None => notes.push(format!("the 3D mark ({}) is lost: ffmpeg cannot give it to a video joined from bare streams (after repair or --scenes)", info.stereo3d)),
        }
    }
    if info.view_marks.iter().any(|m| m == "Spherical Mapping") {
        notes.push("the 360 degree mark (spherical mapping) is lost: ffmpeg cannot give it to a video joined from bare streams (after repair or --scenes), so players show the flat picture".into());
    }
    streams.view_marks.retain(|m| m == "Stereo 3D" && stereo_layout(&info.stereo3d).is_some());
    a.extend(metadata::ffmpeg_args(mo, tags, 1));
    // The video's language and marks are the source's video's, not the bare stream's (which
    // has none): a repaired Matroska lost "Main picture", its language and its default mark.
    // Naming one stream's source turns off ffmpeg's copying for all of them.
    a.extend(["-map_metadata:s:v:0".into(), format!("1:s:{}", info.video_index)]);
    a.extend(streams.meta_from.iter().flat_map(|(o, i)| [format!("-map_metadata:s:{o}"), format!("1:s:{i}")]));
    if !info.video_disposition.is_empty() {
        a.extend(["-disposition:v:0".into(), info.video_disposition.clone()]);
    }
    a.extend(streams.args.iter().cloned());
    a.push(path_arg(out));
    (a, streams)
}

/// Channel layouts libopus takes as they are, with their channels in ffmpeg's order, fewest
/// channels first. It refuses any other: "Invalid channel layout 5.1(side) for specified
/// mapping family -1", which is what ffmpeg's AC-3, E-AC-3 and DTS decoders call ordinary 5.1.
const OPUS_LAYOUTS: &[(&str, &[&str])] = &[
    ("mono", &["FC"]),
    ("stereo", &["FL", "FR"]),
    ("3.0", &["FL", "FR", "FC"]),
    ("quad", &["FL", "FR", "BL", "BR"]),
    ("5.0", &["FL", "FR", "FC", "BL", "BR"]),
    ("5.1", &["FL", "FR", "FC", "LFE", "BL", "BR"]),
    ("6.1", &["FL", "FR", "FC", "LFE", "BC", "SL", "SR"]),
    ("7.1", &["FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR"]),
];

/// ffmpeg's other named layouts of up to eight channels, by their channels.
const OTHER_LAYOUTS: &[(&str, &[&str])] = &[
    ("downmix", &["DL", "DR"]),
    ("2.1", &["FL", "FR", "LFE"]),
    ("3.0(back)", &["FL", "FR", "BC"]),
    ("4.0", &["FL", "FR", "FC", "BC"]),
    ("quad(side)", &["FL", "FR", "SL", "SR"]),
    ("3.1", &["FL", "FR", "FC", "LFE"]),
    ("5.0(side)", &["FL", "FR", "FC", "SL", "SR"]),
    ("4.1", &["FL", "FR", "FC", "LFE", "BC"]),
    ("5.1(side)", &["FL", "FR", "FC", "LFE", "SL", "SR"]),
    ("6.0", &["FL", "FR", "FC", "BC", "SL", "SR"]),
    ("6.0(front)", &["FL", "FR", "FLC", "FRC", "SL", "SR"]),
    ("3.1.2", &["FL", "FR", "FC", "LFE", "TFL", "TFR"]),
    ("hexagonal", &["FL", "FR", "FC", "BL", "BR", "BC"]),
    ("6.1(back)", &["FL", "FR", "FC", "LFE", "BL", "BR", "BC"]),
    ("6.1(front)", &["FL", "FR", "LFE", "FLC", "FRC", "SL", "SR"]),
    ("7.0", &["FL", "FR", "FC", "BL", "BR", "SL", "SR"]),
    ("7.0(front)", &["FL", "FR", "FC", "FLC", "FRC", "SL", "SR"]),
    ("7.1(wide)", &["FL", "FR", "FC", "LFE", "BL", "BR", "FLC", "FRC"]),
    ("7.1(wide-side)", &["FL", "FR", "FC", "LFE", "FLC", "FRC", "SL", "SR"]),
    ("5.1.2", &["FL", "FR", "FC", "LFE", "BL", "BR", "TFL", "TFR"]),
    ("octagonal", &["FL", "FR", "FC", "BL", "BR", "BC", "SL", "SR"]),
];

/// The channels of a layout as ffprobe names it: a named one, or one written out
/// ("FL+FR+LFE", "3 channels (FL+FR+LFE)"). None where it does not say.
fn layout_channels(layout: &str) -> Option<Vec<&str>> {
    if let Some((_, channels)) = OPUS_LAYOUTS.iter().chain(OTHER_LAYOUTS).find(|(name, _)| *name == layout) {
        return Some(channels.to_vec());
    }
    let listed = layout.rsplit_once('(').map_or(layout, |(_, rest)| rest.trim_end_matches(')'));
    let channels: Vec<&str> = listed.split('+').map(str::trim).collect();
    (channels.len() > 1 && channels.iter().all(|c| !c.is_empty() && c.chars().all(|ch| ch.is_ascii_alphanumeric()))).then_some(channels)
}

/// How a sound track is made into one libopus takes, every channel where it was.
#[derive(Debug, PartialEq)]
struct OpusLayout {
    /// The layout the track is given.
    name: &'static str,
    /// The `pan` filter that puts each channel in its place there; None where libopus takes
    /// the track as it is.
    filter: Option<String>,
    /// Channels of `name` the source has nothing for, which stay silent.
    silent: usize,
}

/// The fewest-channel layout libopus takes that has a place for every channel of the source.
/// A surround channel can take the other surround pair's place: ffmpeg's decoders call the
/// same two speakers side (AC-3, DTS) or back (AAC) surrounds, and 5.1 is the one layout Opus
/// has for either. A source with a channel no Opus layout has (more than eight, front centre
/// pairs, heights) has none: it is copied or refused (see [`plan_sound`]).
fn opus_layout(layout: &str, channels: u32) -> Option<OpusLayout> {
    let named = OPUS_LAYOUTS.iter().find(|(name, _)| *name == layout);
    if let Some((name, _)) = named {
        return Some(OpusLayout { name, filter: None, silent: 0 });
    }
    if layout.is_empty() {
        // Unsaid: the decoder hands the track on in the layout of its number of channels,
        // as it always did.
        return OPUS_LAYOUTS.iter().find(|(_, places)| places.len() == channels as usize).map(|(name, _)| OpusLayout { name, filter: None, silent: 0 });
    }
    // A layout named that is none of these has channels Opus has no place for (heights, 22.2).
    let source = layout_channels(layout)?;
    let stands_for = |channel: &str| -> &'static [&'static str] {
        match channel {
            "SL" => &["BL"],
            "SR" => &["BR"],
            "BL" => &["SL"],
            "BR" => &["SR"],
            "DL" => &["FL"],
            "DR" => &["FR"],
            _ => &[],
        }
    };
    OPUS_LAYOUTS.iter().filter(|(_, places)| places.len() >= source.len()).find_map(|(name, places)| {
        let mut taken = vec![false; places.len()];
        let mut to = Vec::new();
        for channel in &source {
            let place = std::iter::once(*channel).chain(stands_for(channel).iter().copied()).find_map(|c| places.iter().position(|p| *p == c).filter(|&i| !taken[i]))?;
            taken[place] = true;
            to.push(place);
        }
        let map: Vec<String> = to.iter().enumerate().map(|(from, place)| format!("c{place}=c{from}")).collect();
        Some(OpusLayout { name, filter: Some(format!("pan={name}|{}", map.join("|"))), silent: places.len() - source.len() })
    })
}

/// Sound formats each container holds as they are, for a track Opus has no layout for.
const MKV_AUDIO: &[&str] = &[
    "aac", "ac3", "eac3", "dts", "truehd", "mlp", "flac", "opus", "vorbis", "mp3", "mp2", "alac", "wavpack", "tta",
    "pcm_s16le", "pcm_s24le", "pcm_s32le", "pcm_f32le", "pcm_f64le", "pcm_s16be", "pcm_s24be", "pcm_s32be", "pcm_u8",
];
const MP4_AUDIO: &[&str] = &["aac", "ac3", "eac3", "flac", "opus", "mp3", "alac"];

/// What becomes of one sound track.
#[derive(Debug, PartialEq)]
enum Sound {
    Copy,
    Opus { kbps: u32, layout: Option<OpusLayout> },
}

/// What becomes of each sound track, with what the report says of it, or why the video cannot
/// be encoded at all. Decided before any encoding time is spent: a track libopus refuses used
/// to fail the encode once the whole search was done. `cut`: the sound comes out of a cut,
/// which has to encode it again.
fn plan_sound(info: &MediaInfo, container: Container, audio: Audio, cut: bool) -> std::result::Result<Vec<(Sound, Option<String>)>, String> {
    let mut plan = Vec::new();
    for (i, s) in info.audio.iter().enumerate() {
        let kbps = (s.channels.max(1) * 64).clamp(64, 384);
        let lean = lean_opus_kbps(s.channels);
        let wants = if cut {
            Some((kbps, " (cut audio is re-encoded)"))
        } else if COPY_AUDIO.contains(&s.codec.as_str()) {
            (audio == Audio::Shrink && s.bitrate > lean as f64 * 1000.0 * SHRINK_ABOVE).then_some((lean, ""))
        } else {
            Some((kbps, ""))
        };
        let Some((kbps, why)) = wants else {
            plan.push((Sound::Copy, None));
            continue;
        };
        let layout = if s.layout.is_empty() { format!("{} channels", s.channels) } else { format!("{}, {} channels", s.layout, s.channels) };
        let from = if COPY_AUDIO.contains(&s.codec.as_str()) && !cut && audio == Audio::Shrink { format!("{} {:.0} kbps", s.codec, s.bitrate / 1000.0) } else { s.codec.clone() };
        match opus_layout(&s.layout, s.channels) {
            Some(l) => {
                let placed = match (&l.filter, l.silent) {
                    (None, _) => String::new(),
                    (Some(_), 0) => format!(", {} as {}", s.layout, l.name),
                    (Some(_), n) => format!(", {} as {}: its channels where they were, {n} silent", s.layout, l.name),
                };
                plan.push((Sound::Opus { kbps, layout: Some(l).filter(|l| l.filter.is_some()) }, Some(format!("audio {i}: {from} -> Opus {kbps} kbps{placed}{why}"))));
            }
            None if cut => {
                return Err(format!(
                    "audio {i} ({}, {layout}) cannot be cut: sound out of a cut is encoded again, and Opus has no layout for these channels; remove the sections with another tool, or convert the track to 5.1 or 7.1 first",
                    s.codec
                ));
            }
            // Copyable formats are only made Opus to save room: kept as they are instead.
            None if COPY_AUDIO.contains(&s.codec.as_str()) => plan.push((Sound::Copy, None)),
            None => {
                let holds = match container {
                    Container::Mkv => MKV_AUDIO,
                    Container::Mp4 => MP4_AUDIO,
                };
                if !holds.contains(&s.codec.as_str()) {
                    let instead = if container == Container::Mp4 && MKV_AUDIO.contains(&s.codec.as_str()) { "--container mkv keeps it as it is" } else { "convert the track to 5.1 or 7.1 first" };
                    return Err(format!(
                        "audio {i} ({}, {layout}) cannot be kept: Opus has no layout for these channels and {} does not hold {} as it is; {instead}",
                        s.codec, container.ext().to_uppercase(), s.codec
                    ));
                }
                plan.push((Sound::Copy, Some(format!("audio {i}: {} kept as it is: Opus has no layout for its channels ({layout})", s.codec))));
            }
        }
    }
    Ok(plan)
}

/// When the first frame of a bare video stream made here is shown, by its own timestamps.
fn first_video_time(tools: &Tools, video: &Path) -> Result<f64> {
    let out = crate::tools::probe_command(&tools.ffprobe)
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=start_time", "-of", "csv=p=0"])
        .arg(crate::tools::probe_arg(video))
        .output()
        .context("ffprobe did not run")?;
    let text = String::from_utf8_lossy(&out.stdout);
    match text.trim().parse::<f64>() {
        Ok(t) if out.status.success() && t.is_finite() => Ok(t),
        _ => bail!("ffprobe gave no start time for {} ({}); {FAILED_INSIDE}", video.display(), String::from_utf8_lossy(&out.stderr).trim()),
    }
}

/// Width and height of the picture an encode of this source makes: even, as 4:2:0 needs
/// (see [`search::source_filters`]).
fn encoded_size(info: &MediaInfo) -> (u32, u32) {
    (info.width & !1, info.height & !1)
}

/// How wide the encoded picture is shown against its height, as `-aspect` takes it ("4:3"),
/// for a source whose pixels are not square; None for square ones. ffmpeg reads each term up
/// to 255 and takes the nearest ratio within that, so a few shapes come out a hair off:
/// 640x360 of 16:15 pixels is 256:135, which became 201:106 (0.006 % narrower, pixels
/// 1809:1696), shown at the same 683 pixels wide in Matroska.
fn display_aspect(info: &MediaInfo) -> Option<String> {
    let (w, h) = info.sample_aspect;
    let (width, height) = encoded_size(info);
    if w == 0 || h == 0 || width == 0 || height == 0 {
        return None;
    }
    let (n, d) = (u64::from(w) * u64::from(width), u64::from(h) * u64::from(height));
    let gcd = |mut a: u64, mut b: u64| {
        while b != 0 {
            (a, b) = (b, a % b);
        }
        a
    };
    let g = gcd(n, d);
    Some(format!("{}:{}", n / g, d / g))
}

/// How wide a picture of `width` by `height` pixels of this shape is shown against its height.
fn shown_ratio((w, h): (u32, u32), width: u32, height: u32) -> f64 {
    let pixel = if w == 0 || h == 0 { 1.0 } else { f64::from(w) / f64::from(h) };
    pixel * f64::from(width) / f64::from(height.max(1))
}

/// Audio, subtitle, cover and attachment arguments for source input number `src`.
struct Streams {
    args: Vec<String>,
    /// (output, source) stream specifiers whose tags must be copied explicitly (cut mode).
    meta_from: Vec<(String, String)>,
    audio: usize,
    subs: usize,
    /// The source subtitle stream each kept one comes from.
    sub_sources: Vec<usize>,
    covers: usize,
    /// The marks for how the picture is shown (3D, 360 degree) the output must have.
    view_marks: Vec<String>,
}

/// What of the source's picture the output cannot have, and what it keeps of its HDR, as
/// notes. `marks`: what its first frames carry.
fn picture_notes(info: &MediaInfo, marks: probe::FrameMarks) -> Vec<String> {
    let mut notes = Vec::new();
    if info.interlaced {
        notes.push(format!("the source is interlaced: the output has {} progressive frames per second, one per pair of fields", search::cfr_label(info)));
    }
    if info.closed_captions || marks.closed_captions {
        notes.push("closed captions inside the picture are lost: they travel in the video stream, which is encoded anew; to keep them, take them out first (ffmpeg -f lavfi -i \"movie=FILE[out+subcc]\" -map 0:1 captions.srt)".into());
    }
    let reduced = picture_reduced(info);
    if !reduced.is_empty() {
        notes.push(format!("picture made 10-bit 4:2:0, which is what is encoded: {}; keep the original where these matter", reduced.join(", ")));
    }
    if info.hdr {
        // libvmaf's models are trained on SDR; on PQ/HLG pixels the score still tracks
        // encoding damage, but its absolute meaning differs from SDR.
        notes.push(if marks.hdr10_plus {
            // SVT-AV1 through ffmpeg is given no dynamic metadata, and nothing else here
            // writes it: measured on an HDR10+ HEVC source, the mastering display and light
            // level came through and the SMPTE 2094-40 frame data did not.
            format!("HDR ({}): colour, mastering display and light level kept, HDR10+ dynamic metadata lost (screens that use it show the static HDR10); VMAF measured on the HDR signal", info.color_transfer)
        } else {
            format!("HDR ({}): colour and HDR metadata kept; VMAF measured on the HDR signal", info.color_transfer)
        });
    }
    notes
}

/// What a 10-bit 4:2:0 encode loses of the source's picture, in words.
fn picture_reduced(info: &MediaInfo) -> Vec<String> {
    let p = info.pix_fmt.as_str();
    let mut lost = Vec::new();
    if search::is_rgb(info) {
        let matrix = if info.color_primaries == "bt2020" { "BT.2020" } else { "BT.709" };
        lost.push(format!("RGB became YUV by the {matrix} matrix, with a quarter of its colour detail"));
    } else if p.contains("444") {
        lost.push("4:4:4 colour became 4:2:0, a quarter of its colour detail".to_string());
    } else if p.contains("422") || p.contains("440") {
        lost.push(format!("4:{} colour became 4:2:0, half of its colour detail", if p.contains("422") { "2:2" } else { "4:0" }));
    } else if p.contains("411") {
        lost.push("4:1:1 colour became 4:2:0, its colour detail moved from across to down".to_string());
    }
    let bits = bit_depth(p);
    if bits > 10 {
        lost.push(format!("{bits}-bit became 10-bit"));
    }
    let alpha = ["yuva", "gbrap", "rgba", "bgra", "argb", "abgr", "ya8", "ya16"].iter().any(|a| p.starts_with(a));
    if alpha || info.alpha {
        lost.push("its transparency (alpha) is dropped".to_string());
    }
    lost
}

/// Bits per sample of an ffmpeg pixel format ("yuv420p12le": 12, "gray10le": 10, "p010le":
/// 10, "rgb48le": 16), 8 for the rest ("rgb24" and "nv12" name something else by their digits).
fn bit_depth(pix_fmt: &str) -> u32 {
    let name = pix_fmt.strip_suffix("le").or_else(|| pix_fmt.strip_suffix("be")).unwrap_or(pix_fmt);
    if ["rgb48", "bgr48", "rgba64", "bgra64", "ya16"].contains(&name) {
        return 16;
    }
    let head = name.trim_end_matches(|c: char| c.is_ascii_digit());
    let digits = &name[head.len()..];
    let bits = if head.ends_with('p') && head.len() > 1 || head.starts_with("gray") {
        digits.parse().ok()
    } else if head == "p" && digits.len() == 3 {
        // p010, p210, p410: the last two digits.
        digits[1..].parse().ok()
    } else {
        None
    };
    bits.filter(|b| (9..=16).contains(b)).unwrap_or(8)
}

/// What subtitle track `i`, of `codec`, becomes in `container`: the codec it is written with
/// ("copy" for as it is) or None for left out, and a note where something of it is lost.
fn plan_subtitle(i: usize, codec: &str, container: Container) -> (Option<&'static str>, Option<String>) {
    match (container, codec) {
        // 608 captions are text, which ffmpeg's decoder reads as ASS lines. Neither container
        // takes them as they are: MP4 "codec not currently supported in container", and
        // Matroska failed at the end of the encode.
        (Container::Mp4, "eia_608") => (
            Some("mov_text"),
            Some(format!("subtitle {i} (608 closed captions) became an MP4 text subtitle: MP4 has no place for 608 captions as they are, and where they stand on screen is lost")),
        ),
        (Container::Mkv, "eia_608") => (Some("ass"), Some(format!("subtitle {i} (608 closed captions) became an ASS subtitle: Matroska has no place for 608 captions as they are"))),
        // MP4 holds TTML as it is; ffmpeg has no TTML decoder to convert it for Matroska.
        (Container::Mp4, "ttml") => (Some("copy"), None),
        (Container::Mkv, "ttml") => (
            None,
            Some(format!("subtitle {i} (ttml) dropped: Matroska has no place for TTML subtitles and ffmpeg cannot read them to convert them; --container mp4 keeps it")),
        ),
        (Container::Mp4, "ass" | "ssa") => (Some("mov_text"), Some(format!("subtitle {i} ({codec}) became plain MP4 text: its styles, positions and fonts are lost; --container mkv keeps it as it is"))),
        (Container::Mp4, text) if TEXT_SUBS.contains(&text) => (Some("mov_text"), None),
        (Container::Mp4, _) => (None, Some(format!("subtitle {i} ({codec}) dropped: image subtitles don't fit in MP4; use --container mkv"))),
        // MP4-style text subtitles can't be stored in Matroska; convert them to SRT.
        (Container::Mkv, "mov_text") => (Some("srt"), None),
        (Container::Mkv, _) => (Some("copy"), None),
    }
}

/// The `-stereo3d` layout for ffprobe's name of a 3D picture's layout (see
/// `MediaInfo::stereo3d`); None for one ffmpeg cannot set.
fn stereo_layout(layout: &str) -> Option<&'static str> {
    match layout {
        "2D" => Some("2d"),
        "side by side" => Some("sbs"),
        "side by side inverted" => Some("sbsl"),
        "top and bottom" => Some("tb"),
        "top and bottom inverted" => Some("tbl"),
        _ => None,
    }
}

/// Names a kept track the way its source does, as `-metadata` arguments for output stream
/// `spec` ("a:1"). Copied tags alone lose it: ffmpeg reads an MP4 track's name as a tag
/// `name` and writes a title, so MP4 to MP4 lost every name and MP4 to Matroska put it in a
/// tag NAME, where players do not look.
fn title_args(spec: &str, track: &probe::Track, container: Container) -> Vec<String> {
    if track.title.is_empty() {
        return Vec::new();
    }
    let mut a = vec![format!("-metadata:s:{spec}"), format!("title={}", track.title)];
    if track.from_name && container == Container::Mkv {
        a.extend([format!("-metadata:s:{spec}"), "name=".into()]);
    }
    a
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
    // (`optimize` refused what cannot be planned before the search; what is left is kept.)
    let sound = plan_sound(info, container, audio, cut_subs.is_some()).unwrap_or_default();
    a.extend(title_args("v:0", &info.video_track, container));
    for (i, (s, (way, note))) in info.audio.iter().zip(sound).enumerate() {
        if cut_subs.is_some() {
            a.extend(["-map".into(), format!("[ao{i}]")]);
            a.extend([format!("-disposition:a:{i}"), marked(Some(&s.disposition))]);
        } else {
            a.extend(["-map".into(), format!("{src}:a:{i}")]);
        }
        meta_from.push((format!("a:{i}"), format!("a:{i}")));
        a.extend(title_args(&format!("a:{i}"), &s.track, container));
        match way {
            Sound::Copy => a.extend([format!("-c:a:{i}"), "copy".into()]),
            Sound::Opus { kbps, layout } => {
                a.extend([format!("-c:a:{i}"), "libopus".into(), format!("-b:a:{i}"), format!("{kbps}k")]);
                // Out of a cut, the filter goes into the cut's graph (see `full_encode_args`).
                if let Some(filter) = layout.and_then(|l| l.filter).filter(|_| cut_subs.is_none()) {
                    a.extend([format!("-filter:a:{i}"), filter]);
                }
            }
        }
        notes.extend(note);
    }

    let mut kept_subs = 0;
    let mut sub_sources = Vec::new();
    for (i, codec) in info.subtitle_codecs.iter().enumerate() {
        let (codec, note) = plan_subtitle(i, codec, container);
        notes.extend(note);
        let Some(codec) = codec else { continue };
        let from = match cut_subs {
            None => format!("{src}:s:{i}"),
            Some(inputs) => match inputs.get(i).copied().flatten() {
                Some(n) => format!("{n}:s:0"),
                None => continue,
            },
        };
        a.extend(["-map".into(), from]);
        a.extend([format!("-c:s:{kept_subs}"), codec.into()]);
        if cut_subs.is_some() {
            a.extend([format!("-disposition:s:{kept_subs}"), marked(info.subtitle_dispositions.get(i))]);
        }
        meta_from.push((format!("s:{kept_subs}"), format!("s:{i}")));
        a.extend(title_args(&format!("s:{kept_subs}"), &info.subtitle_tracks.get(i).cloned().unwrap_or_default(), container));
        sub_sources.push(i);
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
            if !info.attachment_names.is_empty() {
                notes.push(format!("attachments dropped: {}: MP4 has no place for them; --container mkv keeps them", info.attachment_names.join(", ")));
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
        // ffmpeg writes the 3D and 360 degree boxes (st3d, sv3d) into MP4 only when told it may
        // go past the standard. Compared on a file with Opus, AAC, mov_text and a cover, the
        // flag added those boxes and Apple's vexu and nothing else: every packet the same.
        if !info.view_marks.is_empty() {
            a.extend(["-strict", "unofficial"].map(String::from));
        }
    }
    Streams { args: a, meta_from, audio: info.audio.len(), subs: kept_subs, sub_sources, covers, view_marks: info.view_marks.clone() }
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
        if codec == "ttml" {
            notes.push(format!("subtitle {i} (ttml) dropped: ffmpeg cannot read TTML subtitles, so their lines cannot be moved with the cut"));
            subs.push(None);
            continue;
        }
        // 608 captions are read as text like the others.
        if !TEXT_SUBS.contains(&codec.as_str()) && codec != "eia_608" {
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
        crate::files::write_new(&path, retimed.as_bytes())?;
        subs.push(Some(path));
    }
    let mut chapters = None;
    if keep_meta {
        let list = sections::read_chapters(&tools.ffprobe, input)?;
        if !list.is_empty() {
            let path = tmp.join("chapters.ffmeta");
            crate::files::write_new(&path, sections.chapters(&list, info.start_time).as_bytes())?;
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
    // The shape the picture is shown at: the same, but for the column or row an odd size
    // loses to 4:2:0 (see `encoded_size`).
    let (width, height) = encoded_size(info);
    let (shown, should) = (shown_ratio(out_info.sample_aspect, out_info.width, out_info.height), shown_ratio(info.sample_aspect, width, height));
    if (shown / should - 1.0).abs() > 0.005 {
        bail!("the finished video is shown {shown:.4} times as wide as it is high where its source is shown {should:.4} times; {NOT_WRITTEN}");
    }
    if let Some(what) = track_lost(info, &out_info, streams) {
        bail!("the finished file's {what}; {NOT_WRITTEN}");
    }
    let lost: Vec<&str> = streams.view_marks.iter().filter(|m| !out_info.view_marks.contains(m)).map(String::as_str).collect();
    if !lost.is_empty() {
        bail!("the finished video lacks its source's marks for how it is shown ({}); {NOT_WRITTEN}", lost.join(", "));
    }
    if let Some(off) = sound_moved(info, &out_info, streams, cut) {
        bail!(
            "the sound of the finished video starts {:.3} s {} the picture than in its source; {NOT_WRITTEN}",
            off.abs(),
            if off > 0.0 { "later against" } else { "earlier against" }
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

/// Refuses an HDR output whose container does not say what the source says of its mastering
/// display and light level. The source can say it in its container or in its frames; players go
/// by the output's container.
fn check_hdr_data(tools: &Tools, input: &Path, info: &MediaInfo, out: &Path, out_info: &MediaInfo) -> Result<()> {
    let source = match probe::hdr_data(&tools.ffprobe, input, info.video_index) {
        Ok((container, frame)) => probe::HdrData { mastering: container.mastering.or(frame.mastering), light_level: container.light_level.or(frame.light_level) },
        // It was read a moment ago; a hiccup here is no reason to throw the encode away.
        Err(e) => {
            eprintln!("warning: could not read the source's HDR data again, so the output's was not compared with it: {e:#}");
            return Ok(());
        }
    };
    let (kept, _) = probe::hdr_data(&tools.ffprobe, out, out_info.video_index)?;
    if let Some(what) = source.not_kept_in(&kept) {
        bail!("the finished video {what} of its source; {NOT_WRITTEN}");
    }
    Ok(())
}

/// The first kept track whose name or language the output does not have as its source does,
/// said as what was lost ("audio track 2 has no name where its source's is \"Commentary\"").
fn track_lost(info: &MediaInfo, out: &MediaInfo, streams: &Streams) -> Option<String> {
    let mut pairs = vec![("video track".to_string(), &info.video_track, &out.video_track)];
    if out.audio.len() == info.audio.len() {
        pairs.extend(info.audio.iter().zip(&out.audio).enumerate().map(|(i, (s, o))| (format!("audio track {}", i + 1), &s.track, &o.track)));
    }
    let none = probe::Track::default();
    for (k, &i) in streams.sub_sources.iter().enumerate() {
        pairs.push((format!("subtitle track {}", k + 1), info.subtitle_tracks.get(i).unwrap_or(&none), out.subtitle_tracks.get(k).unwrap_or(&none)));
    }
    let said = |t: &str| if t.is_empty() { "none".to_string() } else { format!("\"{t}\"") };
    pairs.into_iter().find_map(|(what, source, kept)| {
        if !source.title.is_empty() && source.title != kept.title {
            Some(format!("{what} is named {} where its source's is named \"{}\"", said(&kept.title), source.title))
        } else if !source.language.is_empty() && source.language != kept.language {
            Some(format!("{what} has language {} where its source's has \"{}\"", said(&kept.language), source.language))
        } else {
            None
        }
    })
}

/// How much later against the picture the first sound track starts in the output than in the
/// source, where that is more than a frame or two (lip sync); None where it is not, or where
/// the two cannot be compared (no sound kept, a cut, a file that does not say).
fn sound_moved(info: &MediaInfo, out: &MediaInfo, streams: &Streams, cut: Option<&Sections>) -> Option<f64> {
    if cut.is_some() || streams.audio == 0 || streams.audio != info.audio.len() {
        return None;
    }
    let lead = |m: &MediaInfo| Some(m.audio.first()?.start? - m.video_start);
    let moved = lead(out)? - lead(info)?;
    // One frame, but not less than 40 ms: a track made Opus starts 6.5 ms early by its
    // encoder's delay, where AAC starts 21 ms early by its own, and a 120 fps frame is 8 ms.
    let frame = if info.fps > 0.0 { 1.0 / info.fps } else { 0.04 };
    (moved.abs() > frame.max(0.04) + 1e-6).then_some(moved)
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
    search::extract_reference(tools, input, info, true, search::first_frame(info), info.start_time, src_start, secs, &r)?;
    search::extract_reference(tools, out, info, false, search::first_frame(out_info), out_info.start_time, start, secs, &d)?;
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
    if info.hdr {
        check_hdr_data(tools, input, info, out, &out_info)?;
    }
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

/// The survey of a finished encode says the video should be encoded once more, at this CRF.
#[derive(Debug)]
struct EncodeAgain {
    crf: f64,
    /// Why, to finish the sentence of a note.
    why: String,
}

impl std::fmt::Display for EncodeAgain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the video is to be encoded again at CRF {}: {}", self.crf, self.why)
    }
}

impl std::error::Error for EncodeAgain {}

/// How far below what a stretch scored by itself it may read in the joined video before the
/// join is taken for broken: frames a step apart read ten or more lower.
const JOIN_SLACK: f64 = 3.0;

/// Puts a bare video stream (the first path) together with the source's other streams in
/// the second: the ffmpeg arguments and what the result holds, with notes of what it cannot keep.
type Remux<'a> = dyn Fn(&Path, &Path, &mut Vec<String>) -> Result<(Vec<String>, Streams)> + 'a;

/// Verification of a one-pass encode along its whole length: every frame is scored against
/// the source's, and stretches below the floor are encoded again and put in place (see
/// [`repair`]). The scores also tell what mean those repairs will leave: where that misses
/// the target the next-lowest GOPs are encoded again too, and where that would take too much of
/// the video, or the mean would end well above the target, this fails with [`EncodeAgain`] (see
/// [`repair::encode_again_at`]). The scores returned are those of the usual spot-check places.
/// `None` where the whole video can't be gone by, for the spot checks to decide.
///
/// `remux` gives the ffmpeg arguments that put a bare video stream together with the
/// source's other streams, and what the result holds; it says what it cannot keep.
#[allow(clippy::too_many_arguments)]
fn verify_whole(
    tools: &Tools,
    input: &Path,
    info: &MediaInfo,
    out: &Path,
    streams: &Streams,
    enc: &EncodeSettings,
    rules: &repair::MeanRules,
    floor: f64,
    min_crf: u8,
    threads: usize,
    tmp: &Path,
    remux: &Remux,
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
    let (crf, target) = (rules.crf, rules.target);
    let plan = repair::plan_mean(&survey, info, floor, target);
    eprintln!(
        "foretold after repairs below VMAF {floor:.0}: mean {:.2} ({} encoded again){}",
        plan.mean,
        hms(plan.secs),
        if plan.top_up_secs() > 0.0 { format!(", and {} more to bring it to {target}", hms(plan.top_up_secs())) } else { String::new() }
    );
    if let Some(again) = repair::encode_again_at(&plan, rules) {
        let why = if again > crf {
            format!("its mean would have ended at VMAF {:.2}, more than the target of {target} calls for", plan.mean)
        } else {
            format!("its mean would have ended at VMAF {:.2}, and encoding stretches again could not bring that to {target} without redoing more than a quarter of the video", plan.mean)
        };
        return Err(EncodeAgain { crf: again, why }.into());
    }
    let ctx = repair::Ctx { tools, input, info, enc, crf, floor, target, min_crf, threads, dir: tmp };
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
        let mut said = Vec::new();
        let (args, joined_streams) = remux(video, &joined, &mut said)?;
        tools.run(&args)?;
        let _ = std::fs::remove_file(video);
        std::fs::rename(&joined, out)?;
        out_info = check_layout(tools, info, out, &joined_streams, Some(frames), None)?;
        // The one pass said the rest already; what is new is what the second mux lost.
        for n in said {
            if !notes.contains(&n) {
                notes.push(n);
            }
        }
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
        // (What was encoded again for the mean has its own note.)
        let below: Vec<&repair::Stretch> = s.stretches.iter().filter(|x| !x.for_mean).collect();
        if !below.is_empty() {
            let (count, total) = (below.len(), below.iter().map(|x| x.secs).sum::<f64>());
            let (lo, hi) = below.iter().fold((f64::INFINITY, 0.0_f64), |(lo, hi), x| (lo.min(x.crf), hi.max(x.crf)));
            let crfs = if lo == hi { format!("{lo}") } else { format!("{lo}-{hi}") };
            let grown = below.iter().map(|x| x.bytes as f64 - x.bytes_before as f64).sum::<f64>() / 1e6;
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
    if let Some(t) = &s.top_up {
        let n = format!(
            "mean topped up from VMAF {:.2} to {:.2}: {} more stretch{} above the floor encoded again ({}), for {:+.1} MB",
            t.mean_from,
            s.mean_vmaf,
            t.stretches,
            if t.stretches == 1 { "" } else { "es" },
            hms(t.secs),
            t.bytes as f64 / 1e6
        );
        eprintln!("note: {n}");
        tools.emit(Event::Note { text: n.clone() });
        notes.push(n);
    }
    if s.mean_vmaf < target - repair::MEAN_SLACK {
        let n = format!("the whole video's mean is VMAF {:.2}, below the target of {target}: encoding more of it again was not worth what it would take", s.mean_vmaf);
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

    fn options() -> Options {
        Options {
            container: Container::Mp4, target_vmaf: 93.0, preset: 5, svt_params: search::DEFAULT_PARAMS.into(), tuning: None,
            min_crf: 20, max_crf: 55, samples: None, sample_secs: None, min_savings: 0.15, force: false, overwrite: false, keep_temp: false,
            scenes: Scenes::Off, workers: 1, meta: Default::default(), remove: Vec::new(), audio: Audio::Keep, fresh_search: false,
            out_dir_must_exist: false,
        }
    }

    fn folder_for_test(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("av1opt-encode-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_pipe_or_a_link_at_the_reports_name_is_not_waited_on_nor_read_through() {
        let dir = folder_for_test("other-report");
        let output = dir.join("clip.mp4");
        crate::files::tests::make_pipe(&dir.join("clip.av1opt.json"));
        let (tell, told) = std::sync::mpsc::channel();
        let asked = output.clone();
        std::thread::spawn(move || drop(tell.send(report_of_another_output(&asked))));
        assert_eq!(told.recv_timeout(std::time::Duration::from_secs(2)).expect("the report was waited on"), None);
        // A link to the report of another output is not gone through either.
        std::fs::remove_file(dir.join("clip.av1opt.json")).unwrap();
        std::fs::write(dir.join("clip.mkv"), b"other").unwrap();
        std::fs::write(dir.join("elsewhere.json"), br#"{"output":"clip.mkv"}"#).unwrap();
        std::os::unix::fs::symlink(dir.join("elsewhere.json"), dir.join("clip.av1opt.json")).unwrap();
        assert_eq!(report_of_another_output(&output), None);
        std::fs::remove_file(dir.join("clip.av1opt.json")).unwrap();
        std::fs::rename(dir.join("elsewhere.json"), dir.join("clip.av1opt.json")).unwrap();
        assert!(report_of_another_output(&output).is_some_and(|why| why.contains("belongs to")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn set_date(file: &Path, seconds: u64) {
        std::fs::File::options().write(true).open(file).unwrap().set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds)).unwrap();
    }

    fn plan(fingerprint: Option<&str>, mtime: Option<u64>) -> scenes::Plan {
        let best = Trial { crf: 30.0, mean_vmaf: 95.0, min_vmaf: 90.0, size_ratio: 0.5 };
        scenes::Plan { key: String::new(), best, trials: Vec::new(), bias: 0.0, first_pts: 0, total_frames: 0, chunks: Vec::new(), timed: None, source_fingerprint: fingerprint.map(Into::into), source_mtime: mtime }
    }

    #[test]
    fn a_touched_source_keeps_its_work_folder_and_a_rewritten_one_does_not_go_on_from_it() {
        let dir = folder_for_test("work-key");
        let input = dir.join("a.mkv");
        std::fs::write(&input, b"contents").unwrap();
        set_date(&input, 1_000_000);
        let folder = |o: &Options| work_dir(&input, &dir, 8, o, 5, search::DEFAULT_PARAMS, &mut Vec::new()).unwrap();
        let first = folder(&options());
        set_date(&input, 2_000_000);
        let touched = folder(&options());
        assert_eq!((&first.path, &first.key), (&touched.path, &touched.key), "the date is not part of it");
        assert!(!first.key.contains("1000000"), "{}", first.key);
        assert_ne!(first.path, folder(&Options { target_vmaf: 95.0, ..options() }).path, "settings are");

        let now = Recorded { fingerprint: Some("same".into()), modified: Some(2_000_000), ..Recorded::default() };
        assert!(plan_fits(&plan(Some("same"), Some(1_000_000)), &now), "only touched");
        assert!(!plan_fits(&plan(Some("other"), Some(2_000_000)), &now), "samples that differ settle it, whatever the date");
        // With no samples on one side only the date can tell.
        assert!(plan_fits(&plan(None, Some(2_000_000)), &now));
        assert!(!plan_fits(&plan(None, Some(1_000_000)), &now));
        let unread = Recorded { modified: Some(2_000_000), ..Recorded::default() };
        assert!(plan_fits(&plan(Some("same"), Some(2_000_000)), &unread));
        assert!(!plan_fits(&plan(Some("same"), Some(1_000_000)), &unread));
        // A plan of an earlier version: its key held the date, and the folder was found by it.
        assert!(plan_fits(&plan(None, None), &now));

        // What a run goes on from is the plan that fits, in the folder of its settings.
        std::fs::create_dir_all(&first.path).unwrap();
        let write_plan = |p: scenes::Plan| std::fs::write(first.path.join("plan.json"), serde_json::to_vec(&scenes::Plan { key: first.key.clone(), ..p }).unwrap()).unwrap();
        write_plan(plan(Some("same"), Some(1_000_000)));
        assert!(saved_plan(&first, &now).is_some(), "only touched");
        write_plan(plan(Some("other"), Some(2_000_000)));
        assert!(saved_plan(&first, &now).is_none(), "rewritten");
        std::fs::write(first.path.join("plan.json"), serde_json::to_vec(&scenes::Plan { key: "other settings".into(), ..plan(Some("same"), Some(2_000_000)) }).unwrap()).unwrap();
        assert!(saved_plan(&first, &now).is_none(), "made for other settings");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_work_folder_from_when_the_key_held_the_date_is_gone_on_from_for_that_date() {
        use std::os::unix::fs::PermissionsExt;
        let dir = folder_for_test("work-earlier");
        let input = dir.join("a.mkv");
        std::fs::write(&input, b"contents").unwrap();
        set_date(&input, 1_000_000);
        let w = work_dir(&input, &dir, 8, &options(), 5, search::DEFAULT_PARAMS, &mut Vec::new()).unwrap();
        let earlier_key = format!("{}8|1000000{}", w.key_start, w.key_end);
        let earlier = dir.join(format!("{WORK_PREFIX}{:016x}", scenes::fnv1a(&earlier_key)));
        std::fs::create_dir(&earlier).unwrap();
        std::fs::set_permissions(&earlier, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(earlier.join("plan.json"), serde_json::json!({ "key": earlier_key }).to_string()).unwrap();
        let found = work_dir(&input, &dir, 8, &options(), 5, search::DEFAULT_PARAMS, &mut Vec::new()).unwrap();
        assert_eq!((found.path, found.key), (earlier.clone(), earlier_key));
        // Touched since, it is another date: that folder is then one to clear away.
        set_date(&input, 2_000_000);
        assert_eq!(work_dir(&input, &dir, 8, &options(), 5, search::DEFAULT_PARAMS, &mut Vec::new()).unwrap().path, w.path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_work_folders_of_the_same_source_and_settings_are_cleared_away() {
        use std::os::unix::fs::PermissionsExt;
        let dir = folder_for_test("work-outdated");
        let input = dir.join("a.mkv");
        std::fs::write(&input, b"contents").unwrap();
        let mut w = work_dir(&input, &dir, 8, &options(), 5, search::DEFAULT_PARAMS, &mut Vec::new()).unwrap();
        let (start, end) = (w.key_start.clone(), w.key_end.clone());
        let keeper = |name: &str, key: Option<String>| {
            let f = dir.join(format!("{WORK_PREFIX}{name}"));
            std::fs::create_dir(&f).unwrap();
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o700)).unwrap();
            if let Some(key) = key {
                std::fs::write(f.join("plan.json"), serde_json::json!({ "key": key }).to_string()).unwrap();
            }
            f
        };
        let gone = [keeper("other-size", Some(format!("{start}9{end}"))), keeper("with-date", Some(format!("{start}8|1000000{end}")))];
        let kept = [
            keeper("other-settings", Some(format!("{start}8|95|5|20|55|auto|3"))),
            keeper("other-source", Some(format!("v1|/elsewhere/a.mkv|8{end}"))),
            keeper("no-plan", None),
            keeper("more-in-between", Some(format!("{start}8|1|2{end}"))),
            keeper("words-in-between", Some(format!("{start}eight{end}"))),
        ];
        // Not this user's to judge: others can write to it.
        let open = keeper("open", Some(format!("{start}7{end}")));
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        w.start_afresh().unwrap();
        w.remove_outdated_folders();
        assert!(w.path.is_dir(), "its own stays");
        for f in &gone {
            assert!(!f.exists(), "{}", f.display());
        }
        for f in kept.iter().chain([&open]) {
            assert!(f.exists(), "{}", f.display());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_is_made_for_an_output_folder_that_may_not_be_written_to() {
        let dir = folder_for_test("may-write");
        let (input, out, ffprobe) = (dir.join("a.mkv"), dir.join("away/out"), dir.join("ffprobe"));
        std::fs::write(&input, b"xx").unwrap();
        let video = r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264","width":64,"height":64,"pix_fmt":"yuv420p","r_frame_rate":"25/1","avg_frame_rate":"25/1","duration":"9"}],"format":{"format_name":"mov","duration":"9","size":"2"}}"#;
        crate::files::write_script(&ffprobe, &format!("echo '{video}'\n"));
        let tools = Tools::new("false".into(), ffprobe.to_str().unwrap().into(), None);
        let e = optimize(&tools, &input, &out, &|| anyhow::bail!("the drive is away"), &options()).unwrap_err();
        assert!(e.to_string().contains("the drive is away"), "{e:#}");
        assert!(!dir.join("away").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_report_holds_what_tells_its_source_later() {
        let source = Recorded { modified: Some(5), fingerprint: Some("samples".into()), ..Recorded::default() };
        let fields = serde_json::to_value(ReportedSource::of(&source, Some("whole"))).unwrap();
        assert_eq!(fields, serde_json::json!({ "source_mtime": 5, "source_fingerprint": "samples", "source_hash": "whole" }));
    }

    #[test]
    fn a_change_while_the_hash_was_read_is_seen_by_the_check_after_it() {
        let dir = folder_for_test("hash-order");
        let input = dir.join("a.mkv");
        std::fs::write(&input, b"contents").unwrap();
        set_date(&input, 1_000_000);
        let before = Recorded::of(&input).unwrap().1;
        let finished = hash_then_check(
            || {
                set_date(&input, 2_000_000);
                Ok("hash")
            },
            || check_source_unchanged(&input, &before, None, false),
        );
        assert!(finished.unwrap_err().to_string().contains("changed or went away"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chunks_are_tried_at_a_faster_preset_for_every_preset_there_is() {
        let probes: Vec<u8> = (0..=13).map(probe_preset).collect();
        assert_eq!(probes, [4, 5, 6, 7, 8, 9, 10, 11, 12, 12, 12, 12, 12, 13]);
    }

    #[test]
    fn a_work_folder_that_cannot_be_believed_is_left_alone_and_another_used() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = std::env::temp_dir().join(format!("av1opt-work-trust-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("elsewhere")).unwrap();
        std::fs::write(dir.join("elsewhere/plan.json"), b"precious").unwrap();
        let mut notes = Vec::new();

        // Nothing there yet, or a folder of this user's that only they can write to: the name
        // is kept, and a folder from before this was checked becomes private.
        let free = dir.join(".av1opt-work-free");
        assert_eq!(believed_work_path(free.clone(), &mut notes).unwrap(), free);
        let old = dir.join(".av1opt-work-old");
        std::fs::create_dir(&old).unwrap();
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(old.join("plan.json"), b"{}").unwrap();
        assert_eq!(believed_work_path(old.clone(), &mut notes).unwrap(), old);
        assert_eq!(std::fs::metadata(&old).unwrap().permissions().mode() & 0o777, 0o700);
        assert!(notes.is_empty(), "{notes:?}");

        // A link in the folder's place, a link among its files, a folder others can write to.
        let link = dir.join(".av1opt-work-link");
        symlink(dir.join("elsewhere"), &link).unwrap();
        let holding = dir.join(".av1opt-work-holding");
        std::fs::create_dir(&holding).unwrap();
        std::fs::set_permissions(&holding, std::fs::Permissions::from_mode(0o700)).unwrap();
        symlink(dir.join("elsewhere/plan.json"), holding.join("plan.json")).unwrap();
        let open = dir.join(".av1opt-work-open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        for (planted, why) in [(&link, "it is a link"), (&holding, "plan.json in it"), (&open, "other users can write")] {
            let mut said = Vec::new();
            let used = believed_work_path(planted.clone(), &mut said).unwrap();
            assert_ne!(&used, planted);
            assert_eq!(used.parent(), planted.parent());
            assert!(said[0].contains(why) && said[0].contains("left alone"), "{said:?}");
            // The new one is made private, and the planted one stays as it was.
            let mut w = WorkDir { path: used, ..WorkDir::default() };
            w.start_afresh().unwrap();
            assert_eq!(std::fs::symlink_metadata(&w.path).unwrap().permissions().mode() & 0o777, 0o700);
            assert!(std::fs::symlink_metadata(planted).is_ok());
        }
        assert_eq!(std::fs::read(dir.join("elsewhere/plan.json")).unwrap(), b"precious");

        // A run with no plan to go on from takes nothing over from what lies in its folder.
        let mut w = WorkDir { path: old.clone(), ..WorkDir::default() };
        std::fs::write(old.join("chunk00000.obu"), b"from another video").unwrap();
        w.start_afresh().unwrap();
        assert_eq!(w.path, old);
        assert_eq!(std::fs::read_dir(&old).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

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
            audio: audio.iter().map(|&(codec, channels, bitrate)| AudioInfo { codec: codec.into(), channels, bitrate, ..Default::default() }).collect(),
            subtitle_codecs: vec![], subtitle_dispositions: vec![], other_streams: vec![],
            ..Default::default()
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
    fn every_layout_reaches_opus_with_its_channels_in_place_or_is_kept_or_refused_before_the_search() {
        let pan = |l: &str, ch: u32| opus_layout(l, ch).map(|o| (o.name, o.filter.unwrap_or_default(), o.silent));
        // What libopus takes goes in as it is, and so does a layout not said.
        for l in ["mono", "stereo", "3.0", "quad", "5.0", "5.1", "6.1", "7.1"] {
            assert_eq!(pan(l, 0), Some((l, String::new(), 0)));
        }
        assert_eq!(pan("", 2), Some(("stereo", String::new(), 0)));
        assert_eq!(pan("", 6), Some(("5.1", String::new(), 0)));
        assert_eq!(pan("", 9), None);
        // AC-3's 5.1 has side surrounds where AAC's has back ones: the same speakers.
        assert_eq!(pan("5.1(side)", 6), Some(("5.1", "pan=5.1|c0=c0|c1=c1|c2=c2|c3=c3|c4=c4|c5=c5".into(), 0)));
        assert_eq!(pan("5.0(side)", 5), Some(("5.0", "pan=5.0|c0=c0|c1=c1|c2=c2|c3=c3|c4=c4".into(), 0)));
        assert_eq!(pan("quad(side)", 4), Some(("quad", "pan=quad|c0=c0|c1=c1|c2=c2|c3=c3".into(), 0)));
        // A layout Opus has no same-sized place for goes into the smallest that holds each of
        // its channels, the rest silent: 2.1's low frequencies stay low frequencies.
        assert_eq!(pan("2.1", 3), Some(("5.1", "pan=5.1|c0=c0|c1=c1|c3=c2".into(), 3)));
        assert_eq!(pan("4.0", 4), Some(("6.1", "pan=6.1|c0=c0|c1=c1|c2=c2|c4=c3".into(), 3)));
        assert_eq!(pan("FL+FR+LFE", 3), pan("2.1", 3));
        assert_eq!(pan("3 channels (FL+FR+LFE)", 3), pan("2.1", 3));
        assert_eq!(pan("hexagonal", 6), Some(("6.1", "pan=6.1|c0=c0|c1=c1|c2=c2|c5=c3|c6=c4|c4=c5".into(), 1)));
        assert_eq!(pan("downmix", 2), Some(("stereo", "pan=stereo|c0=c0|c1=c1".into(), 0)));
        // Channels Opus has no place for.
        for l in ["7.1(wide)", "7.1(wide-side)", "6.0(front)", "5.1.2", "hexadecagonal", "22.2"] {
            assert_eq!(pan(l, 8), None, "{l}");
        }

        let track = |codec: &str, layout: &str, channels: u32| AudioInfo { codec: codec.into(), layout: layout.into(), channels, ..Default::default() };
        let with = |tracks: Vec<AudioInfo>| MediaInfo { audio: tracks, ..info(&[]) };
        let plan = |i: &MediaInfo, c: Container, cut: bool| plan_sound(i, c, Audio::Keep, cut);
        let surround = with(vec![track("ac3", "5.1(side)", 6), track("aac", "5.1", 6)]);
        let p = plan(&surround, Container::Mp4, false).unwrap();
        assert!(matches!(&p[0].0, Sound::Opus { kbps: 384, layout: Some(OpusLayout { name: "5.1", .. }) }), "{p:?}");
        assert_eq!(p[0].1.as_deref(), Some("audio 0: ac3 -> Opus 384 kbps, 5.1(side) as 5.1"));
        assert_eq!(p[1], (Sound::Copy, None));
        let args = stream_args(&surround, 1, Container::Mp4, Audio::Keep, &[], None, &mut Vec::new()).args;
        let at = args.iter().position(|a| a == "-filter:a:0").expect("a filter");
        assert_eq!(args[at + 1], "pan=5.1|c0=c0|c1=c1|c2=c2|c3=c3|c4=c4|c5=c5");
        assert!(!args.iter().any(|a| a == "-filter:a:1"));
        let p = plan(&with(vec![track("eac3", "2.1", 3)]), Container::Mkv, false).unwrap();
        assert_eq!(p[0].1.as_deref(), Some("audio 0: eac3 -> Opus 192 kbps, 2.1 as 5.1: its channels where they were, 3 silent"));
        // No Opus layout: kept as it is where the container holds it; refused before any
        // encoding where it does not, saying which track and what to do.
        let wide = with(vec![track("eac3", "7.1(wide)", 8)]);
        let p = plan(&wide, Container::Mp4, false).unwrap();
        assert_eq!(p[0], (Sound::Copy, Some("audio 0: eac3 kept as it is: Opus has no layout for its channels (7.1(wide), 8 channels)".into())));
        let pcm = with(vec![track("aac", "stereo", 2), track("pcm_s24le", "7.1(wide)", 8)]);
        assert!(plan(&pcm, Container::Mkv, false).is_ok());
        let e = plan(&pcm, Container::Mp4, false).unwrap_err();
        assert!(e.starts_with("audio 1 (pcm_s24le, 7.1(wide), 8 channels) cannot be kept") && e.ends_with("--container mkv keeps it as it is"), "{e}");
        let e = plan(&with(vec![track("wmapro", "7.1(wide)", 8)]), Container::Mkv, false).unwrap_err();
        assert!(e.ends_with("convert the track to 5.1 or 7.1 first"), "{e}");
        // Sound out of a cut is encoded again, so it has to have a layout.
        assert!(plan(&wide, Container::Mkv, true).unwrap_err().contains("cannot be cut"));
        let p = plan(&surround, Container::Mkv, true).unwrap();
        assert!(matches!(&p[1].0, Sound::Opus { layout: None, .. }), "{p:?}");
        // An AAC track that would only be made smaller stays as it is where Opus has no layout.
        let big = with(vec![AudioInfo { bitrate: 900_000.0, ..track("aac", "7.1(wide)", 8) }]);
        assert_eq!(plan_sound(&big, Container::Mp4, Audio::Shrink, false).unwrap()[0], (Sound::Copy, None));
    }

    #[test]
    fn a_cut_puts_its_sound_into_a_layout_opus_takes() {
        let i = MediaInfo { audio: vec![AudioInfo { codec: "ac3".into(), layout: "5.1(side)".into(), channels: 6, ..Default::default() }], audio_streams: 1, ..info(&[]) };
        let sections = Sections::new(&[(10.0, 20.0)], 60.0).unwrap();
        let cut = Cut { sections: &sections, subs: Vec::new(), chapters: None };
        let (a, _) = full_encode_args(
            Path::new("in.mkv"), &i, &EncodeSettings { preset: 10, pix_fmt_out: "yuv420p10le", params: String::new() }, 30.0, Container::Mkv, Audio::Keep,
            &[], &metadata::Resolved::default(), &MetaOptions::default(), Some(&cut), Path::new("out.mkv"), &mut Vec::new(),
        );
        let graph = &a[a.iter().position(|x| x == "-filter_complex").unwrap() + 1];
        assert!(graph.contains("concat=n=2:v=1:a=1[vout][ap0];[ap0]pan=5.1|c0=c0|c1=c1|c2=c2|c3=c3|c4=c4|c5=c5[ao0]"), "{graph}");
        assert!(a.windows(2).any(|w| w == ["-map", "[ao0]"]));
    }

    #[test]
    fn a_video_joined_from_bare_streams_is_given_the_shape_of_its_pixels() {
        let shaped = |sample_aspect, width, height| MediaInfo { sample_aspect, width, height, ..info(&[]) };
        // A PAL DVD shown 4:3 and 16:9, and one an odd size loses a column of.
        assert_eq!(display_aspect(&shaped((16, 15), 720, 576)).as_deref(), Some("4:3"));
        assert_eq!(display_aspect(&shaped((64, 45), 720, 576)).as_deref(), Some("16:9"));
        assert_eq!(display_aspect(&shaped((964, 963), 321, 241)).as_deref(), Some("3856:2889"));
        assert_eq!(display_aspect(&shaped((0, 0), 720, 576)), None);
        let mut notes = Vec::new();
        let (args, _) = mux_args(Path::new("v.ivf"), false, 0.0, Path::new("in.mkv"), &shaped((16, 15), 720, 576), Container::Mkv, Audio::Keep, &[], &Resolved::default(), &MetaOptions::default(), Path::new("o.mkv"), &mut notes);
        assert!(args.windows(2).any(|w| w == ["-aspect:v:0", "4:3"]), "{args:?}");
        let (args, _) = mux_args(Path::new("v.ivf"), false, 0.0, Path::new("in.mkv"), &shaped((0, 0), 720, 576), Container::Mkv, Audio::Keep, &[], &Resolved::default(), &MetaOptions::default(), Path::new("o.mkv"), &mut notes);
        assert!(!args.iter().any(|a| a.starts_with("-aspect")), "{args:?}");
        // What the check compares: 4:3 against the 5:4 square pixels would show.
        assert!((shown_ratio((16, 15), 720, 576) - 4.0 / 3.0).abs() < 1e-9);
        assert!((shown_ratio((0, 0), 720, 576) - 1.25).abs() < 1e-9);
    }

    #[test]
    fn a_video_joined_from_bare_streams_keeps_its_place_against_the_sound() {
        // A transport stream whose picture starts at 1.7 s, 0.3 s after its sound, and a
        // repaired video whose first frame says 0.3 s: the source is moved to start at zero,
        // the video not at all. A video joined from chunks starts at zero and is moved 0.3 s.
        let ts = MediaInfo { start_time: 1.4, video_start: 1.7, ..info(&[("aac", 2, 128_000.0)]) };
        let mut notes = Vec::new();
        for (first, video_offset) in [(0.3, "0.000000"), (0.0, "0.300000")] {
            let (args, _) = mux_args(Path::new("v.ivf"), false, first, Path::new("in.ts"), &ts, Container::Mp4, Audio::Keep, &[], &Resolved::default(), &MetaOptions::default(), Path::new("o.mp4"), &mut notes);
            assert!(args.contains(&"-copyts".to_string()), "{args:?}");
            let at = |file: &str| args.iter().position(|a| a.ends_with(file)).unwrap();
            assert_eq!(args[at("/v.ivf") - 3..at("/v.ivf") - 1], ["-itsoffset", video_offset].map(String::from), "{args:?}");
            assert_eq!(args[at("/in.ts") - 3..at("/in.ts") - 1], ["-itsoffset", "-1.400000"].map(String::from), "{args:?}");
        }
    }

    #[test]
    fn every_track_keeps_its_name_and_language() {
        let named = |title: &str, from_name| probe::Track { title: title.into(), language: "eng".into(), from_name };
        let mut source = info(&[("aac", 2, 128_000.0), ("aac", 2, 128_000.0)]);
        source.video_track = named("Main picture", false);
        source.audio[1].track = named("English title", true);
        source.subtitle_codecs = vec!["hdmv_pgs_subtitle".into(), "subrip".into()];
        source.subtitle_tracks = vec![named("Signs", false), named("Full", true)];
        source.subtitle_streams = 2;
        source.video_disposition = "default".into();
        let mut notes = Vec::new();
        // An MP4 track's name reaches MP4 as a title, which its muxer writes as the name, and
        // Matroska as a title with the tag NAME taken off. The image subtitle stays out of MP4.
        let streams = stream_args(&source, 1, Container::Mp4, Audio::Keep, &[], None, &mut notes);
        let pairs: Vec<_> = streams.args.windows(2).filter(|w| w[0].starts_with("-metadata:s:")).map(|w| format!("{} {}", w[0], w[1])).collect();
        assert_eq!(pairs, ["-metadata:s:v:0 title=Main picture", "-metadata:s:a:1 title=English title", "-metadata:s:s:0 title=Full"]);
        assert_eq!(streams.sub_sources, [1]);
        let streams = stream_args(&source, 1, Container::Mkv, Audio::Keep, &[], None, &mut notes);
        let pairs: Vec<_> = streams.args.windows(2).filter(|w| w[0].starts_with("-metadata:s:")).map(|w| format!("{} {}", w[0], w[1])).collect();
        assert_eq!(pairs, ["-metadata:s:v:0 title=Main picture", "-metadata:s:a:1 title=English title", "-metadata:s:a:1 name=", "-metadata:s:s:0 title=Signs", "-metadata:s:s:1 title=Full", "-metadata:s:s:1 name="]);
        // A bare video stream takes the source video's tags and marks, and every other stream
        // its own, since naming one turns off ffmpeg's copying for all.
        let (args, _) = mux_args(Path::new("v.ivf"), false, 0.0, Path::new("in.mkv"), &source, Container::Mkv, Audio::Keep, &[], &Resolved::default(), &MetaOptions::default(), Path::new("o.mkv"), &mut notes);
        for pair in [["-map_metadata:s:v:0", "1:s:0"], ["-map_metadata:s:a:1", "1:s:a:1"], ["-map_metadata:s:s:1", "1:s:s:1"], ["-disposition:v:0", "default"]] {
            assert!(args.windows(2).any(|w| w == pair), "{pair:?} in {args:?}");
        }
        // The check: a name or language lost or changed is refused, one the source lacks is not.
        let streams = stream_args(&source, 0, Container::Mkv, Audio::Keep, &[], None, &mut notes);
        let mut out = source.clone();
        assert_eq!(track_lost(&source, &out, &streams), None);
        out.video_track.title.clear();
        assert_eq!(track_lost(&source, &out, &streams).as_deref(), Some("video track is named none where its source's is named \"Main picture\""));
        out = source.clone();
        out.subtitle_tracks[1].language = "fre".into();
        assert_eq!(track_lost(&source, &out, &streams).as_deref(), Some("subtitle track 2 has language \"fre\" where its source's has \"eng\""));
        out = source.clone();
        source.audio[0].track = probe::Track::default();
        out.audio[0].track = named("Added", false);
        assert_eq!(track_lost(&source, &out, &streams), None);
    }

    #[test]
    fn subtitles_are_kept_as_each_container_can_hold_them_and_losses_are_said() {
        let plan = |codec, container| plan_subtitle(3, codec, container);
        // Captions and TTML are text, not images.
        assert_eq!(plan("eia_608", Container::Mp4).0, Some("mov_text"));
        assert_eq!(plan("eia_608", Container::Mkv).0, Some("ass"));
        assert_eq!(plan("ttml", Container::Mp4), (Some("copy"), None));
        let (kept, note) = plan("ttml", Container::Mkv);
        assert_eq!(kept, None);
        assert!(note.is_some_and(|n| n.contains("ffmpeg cannot read them") && n.contains("--container mp4")));
        // Styled subtitles go flat into MP4, and that is said; plain text goes without a word.
        let (kept, note) = plan("ass", Container::Mp4);
        assert_eq!(kept, Some("mov_text"));
        assert!(note.is_some_and(|n| n.contains("styles, positions and fonts are lost") && n.contains("--container mkv")));
        assert_eq!(plan("subrip", Container::Mp4), (Some("mov_text"), None));
        assert_eq!(plan("ass", Container::Mkv), (Some("copy"), None));
        assert_eq!(plan("mov_text", Container::Mkv), (Some("srt"), None));
        let (kept, note) = plan("hdmv_pgs_subtitle", Container::Mp4);
        assert_eq!(kept, None);
        assert_eq!(note.as_deref(), Some("subtitle 3 (hdmv_pgs_subtitle) dropped: image subtitles don't fit in MP4; use --container mkv"));
        assert_eq!(plan("hdmv_pgs_subtitle", Container::Mkv), (Some("copy"), None));
        // Fonts are said to be left out of MP4.
        let mut source = info(&[]);
        (source.attachments, source.attachment_names) = (1, vec!["MyFont.ttf".into()]);
        let mut notes = Vec::new();
        stream_args(&source, 0, Container::Mp4, Audio::Keep, &[], None, &mut notes);
        assert_eq!(notes, ["attachments dropped: MyFont.ttf: MP4 has no place for them; --container mkv keeps them"]);
        notes.clear();
        stream_args(&source, 0, Container::Mkv, Audio::Keep, &[], None, &mut notes);
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn marks_for_how_the_picture_is_shown_are_kept_where_ffmpeg_can_write_them() {
        let mut source = info(&[]);
        source.view_marks = vec!["Stereo 3D".into(), "Spherical Mapping".into()];
        source.stereo3d = "top and bottom".into();
        let mut notes = Vec::new();
        // The one pass keeps both; MP4 has to be let past the standard for them.
        let streams = stream_args(&source, 0, Container::Mp4, Audio::Keep, &[], None, &mut notes);
        assert!(streams.args.windows(2).any(|w| w == ["-strict", "unofficial"]));
        assert_eq!(streams.view_marks, source.view_marks);
        let streams = stream_args(&source, 0, Container::Mkv, Audio::Keep, &[], None, &mut notes);
        assert!(!streams.args.iter().any(|a| a == "-strict"));
        assert!(notes.is_empty(), "{notes:?}");
        assert!(!stream_args(&info(&[]), 0, Container::Mp4, Audio::Keep, &[], None, &mut notes).args.iter().any(|a| a == "-strict"));
        // A bare stream can be given the 3D layout and not the 360 degree mapping, which is said.
        let (args, streams) = mux_args(Path::new("v.ivf"), false, 0.0, Path::new("in.mp4"), &source, Container::Mp4, Audio::Keep, &[], &Resolved::default(), &MetaOptions::default(), Path::new("o.mp4"), &mut notes);
        assert!(args.windows(2).any(|w| w == ["-stereo3d:v:0", "tb"]), "{args:?}");
        assert_eq!(streams.view_marks, ["Stereo 3D"]);
        assert!(notes.len() == 1 && notes[0].starts_with("the 360 degree mark (spherical mapping) is lost"), "{notes:?}");
        assert_eq!([stereo_layout("side by side inverted"), stereo_layout("2D"), stereo_layout("checkerboard")], [Some("sbsl"), Some("2d"), None]);
        source.stereo3d = "frame alternate".into();
        notes.clear();
        let (_, streams) = mux_args(Path::new("v.ivf"), false, 0.0, Path::new("in.mp4"), &source, Container::Mkv, Audio::Keep, &[], &Resolved::default(), &MetaOptions::default(), Path::new("o.mkv"), &mut notes);
        assert!(streams.view_marks.is_empty());
        assert!(notes[0].starts_with("the 3D mark (frame alternate) is lost"), "{notes:?}");
    }

    #[test]
    fn what_the_picture_loses_is_said() {
        let picture = |pix_fmt: &str| MediaInfo { pix_fmt: pix_fmt.into(), ..info(&[]) };
        assert!(picture_notes(&picture("yuv420p"), probe::FrameMarks::default()).is_empty());
        assert!(picture_notes(&picture("yuv420p10le"), probe::FrameMarks::default()).is_empty());
        assert_eq!(picture_reduced(&picture("yuv422p")), ["4:2:2 colour became 4:2:0, half of its colour detail"]);
        assert_eq!(picture_reduced(&picture("yuv444p12le")), ["4:4:4 colour became 4:2:0, a quarter of its colour detail", "12-bit became 10-bit"]);
        assert_eq!(picture_reduced(&MediaInfo { color_space: "gbr".into(), ..picture("gbrp") }), ["RGB became YUV by the BT.709 matrix, with a quarter of its colour detail"]);
        assert_eq!(picture_reduced(&picture("yuva420p")), ["its transparency (alpha) is dropped"]);
        // VP9 in WebM keeps its transparency beside a plain 4:2:0 picture.
        assert_eq!(picture_reduced(&MediaInfo { alpha: true, ..picture("yuv420p") }), ["its transparency (alpha) is dropped"]);
        assert_eq!(
            picture_notes(&picture("p010le"), probe::FrameMarks::default()).len() + picture_notes(&picture("nv12"), probe::FrameMarks::default()).len() + picture_notes(&picture("gray"), probe::FrameMarks::default()).len(),
            0
        );
        assert_eq!([bit_depth("yuv420p12le"), bit_depth("gray10le"), bit_depth("p016le"), bit_depth("rgb48le"), bit_depth("rgb24"), bit_depth("nv12")], [12, 10, 16, 16, 8, 8]);
        let interlaced = MediaInfo { interlaced: true, fps: 25.0, nominal_fps: 25.0, ..picture("yuv420p") };
        assert_eq!(picture_notes(&interlaced, probe::FrameMarks::default()), ["the source is interlaced: the output has 25 progressive frames per second, one per pair of fields"]);
        let captions = picture_notes(&MediaInfo { closed_captions: true, ..picture("yuv420p") }, probe::FrameMarks::default());
        assert!(captions.len() == 1 && captions[0].starts_with("closed captions inside the picture are lost"), "{captions:?}");
        let from_frames = picture_notes(&picture("yuv420p"), probe::FrameMarks { closed_captions: true, ..Default::default() });
        assert_eq!(from_frames, captions);
        let hdr = MediaInfo { hdr: true, color_transfer: "smpte2084".into(), ..picture("yuv420p10le") };
        assert_eq!(picture_notes(&hdr, probe::FrameMarks::default()), ["HDR (smpte2084): colour and HDR metadata kept; VMAF measured on the HDR signal"]);
        assert!(picture_notes(&hdr, probe::FrameMarks { hdr10_plus: true, ..Default::default() })[0].contains("mastering display and light level kept, HDR10+ dynamic metadata lost"));
    }

    #[test]
    fn sound_moved_against_the_picture_is_found() {
        let sound_at = |video_start: f64, audio: f64| MediaInfo {
            video_start,
            audio: vec![AudioInfo { codec: "aac".into(), channels: 2, start: Some(audio), ..Default::default() }],
            audio_streams: 1,
            ..info(&[])
        };
        let streams = |audio| Streams { args: Vec::new(), meta_from: Vec::new(), audio, subs: 0, sub_sources: Vec::new(), covers: 0, view_marks: Vec::new() };
        // The 0.28 s a joined TS lost, both ways; what Opus and AAC delays differ by is not.
        let source = sound_at(1.4, 1.6787);
        assert!(sound_moved(&source, &sound_at(0.0, 0.0), &streams(1), None).is_some_and(|m| (m + 0.2787).abs() < 1e-6));
        assert!(sound_moved(&source, &sound_at(0.0, 0.5574), &streams(1), None).is_some_and(|m| (m - 0.2787).abs() < 1e-6));
        assert_eq!(sound_moved(&source, &sound_at(0.0, 0.2787 - 0.0148), &streams(1), None), None);
        // Nothing to compare: no sound kept, a cut, a file that does not say.
        assert_eq!(sound_moved(&source, &sound_at(0.0, 0.0), &streams(0), None), None);
        assert_eq!(sound_moved(&source, &sound_at(0.0, 0.0), &streams(1), Some(&Sections::new(&[(1.0, 2.0)], 60.0).unwrap())), None);
        let unsaid = MediaInfo { audio: vec![AudioInfo::default()], ..sound_at(0.0, 0.0) };
        assert_eq!(sound_moved(&source, &unsaid, &streams(1), None), None);
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
