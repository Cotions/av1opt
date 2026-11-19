//! Runs ffprobe on a file and extracts the fields the scanner needs.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// How long ffprobe gets for one file. It reads little of a file, so this is only ever reached
/// when the file cannot be read at all: a network or FUSE mount that has stalled.
const PROBE_LIMIT: Duration = Duration::from_secs(120);

/// What a run over a whole file gets on top of [`PROBE_LIMIT`] per gigabyte, where it cannot be
/// seen reading (see [`READ_STALL`]).
const LIMIT_PER_GIGABYTE: Duration = Duration::from_secs(60);

/// A run over a whole file is stopped when it has read nothing for this long. A slow disk that
/// is getting through the file is not a stalled one, however long the file takes.
const READ_STALL: Duration = Duration::from_secs(120);

/// The slowest a run over a whole file is expected to read, in bytes per second. A demuxer going
/// round in a damaged file reads for ever, so going on reading does not earn time without end.
const SLOWEST_READ: u64 = 1_000_000;

/// What the system says when reading failed for a reason outside the file. ffprobe prints
/// these as they are, and they can be over by the next try.
const NOT_THE_FILES_FAULT: &[&str] = &[
    "Input/output error", "Stale file handle", "Transport endpoint is not connected", "Connection timed out", "Connection reset", "Host is down", "No route to host",
    "Network is unreachable", "Cannot allocate memory", "Too many open files", "Resource temporarily unavailable", "No such file or directory", "Permission denied",
];

/// A probe that ended without an opinion on the file: ffprobe could not be started, was stopped
/// or ran out of time, or the system could not read the file for it. Unlike a file ffprobe read
/// and rejected, this says nothing that will still be true next time.
#[derive(Debug)]
struct NotJudged(String);

impl std::fmt::Display for NotJudged {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotJudged {}

/// The error of a reader of videos that ended without an opinion on the file.
pub fn unjudged(why: String) -> anyhow::Error {
    NotJudged(why).into()
}

/// Whether a probe's error leaves the file unjudged, to be tried again later.
pub fn left_unjudged(e: &anyhow::Error) -> bool {
    e.downcast_ref::<NotJudged>().is_some()
}

/// Technical facts about one video file, as reported by ffprobe.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MediaInfo {
    /// Absolute stream index of the main video (cover art can come first, so `0:v:0` is wrong).
    #[serde(default)]
    pub video_index: usize,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// The rate the stream is declared at (ffprobe's r_frame_rate); 0 when unknown. `fps` is
    /// the average, which a webcam's drifting frame times pull a little below this.
    #[serde(default)]
    pub nominal_fps: f64,
    /// Time base of the video stream's timestamps as (numerator, denominator), e.g. (1, 90000);
    /// (0, 0) when unknown.
    #[serde(default)]
    pub time_base: (i64, i64),
    /// Frame rate varies (average differs from the nominal rate), e.g. phone recordings.
    #[serde(default)]
    pub vfr: bool,
    pub duration_s: f64,
    /// Length of the video stream itself, from its first frame to the end of its last, 0 when
    /// the container doesn't say. A recording whose sound runs on after the picture stops has
    /// a `duration_s` far past its last frame.
    #[serde(default)]
    pub video_s: f64,
    /// Container start time and the video stream's own start time, in seconds.
    #[serde(default)]
    pub start_time: f64,
    #[serde(default)]
    pub video_start: f64,
    /// Bits per second of the video stream alone (estimated if the container doesn't say).
    pub video_bitrate: f64,
    pub pix_fmt: String,
    /// HDR10 (PQ), HLG or Dolby Vision.
    pub hdr: bool,
    #[serde(default)]
    pub dolby_vision: bool,
    /// ffprobe's color_transfer, e.g. "smpte2084" for HDR10 or "arib-std-b67" for HLG.
    #[serde(default)]
    pub color_transfer: String,
    pub interlaced: bool,
    pub audio_streams: u32,
    pub subtitle_streams: u32,
    /// Per-stream details, used by the encoder to plan copy vs re-encode.
    #[serde(default)]
    pub audio: Vec<AudioInfo>,
    #[serde(default)]
    pub subtitle_codecs: Vec<String>,
    /// What each subtitle track is marked as (see [`AudioInfo::disposition`]).
    #[serde(default)]
    pub subtitle_dispositions: Vec<String>,
    /// Streams an encode has no place for: a second video, a camera's timecode or sensor
    /// track. Each as "kind (format)".
    #[serde(default)]
    pub other_streams: Vec<String>,
    /// Cover images: attached pictures, and still-image streams next to the real video.
    #[serde(default)]
    pub covers: Vec<Cover>,
    /// Non-image attachments (fonts for ASS subtitles, ...).
    #[serde(default)]
    pub attachments: u32,
    /// Global container tags. Not cached by scans (descriptions can be long); encodes probe fresh.
    #[serde(skip)]
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cover {
    pub index: usize,
    pub codec: String,
    /// Original attachment file name (Matroska), if any.
    pub filename: Option<String>,
}

impl MediaInfo {
    /// Seconds of picture: what samples, spot checks and frame counts can come from.
    pub fn picture_secs(&self) -> f64 {
        if self.video_s > 0.0 && self.video_s < self.duration_s { self.video_s } else { self.duration_s }
    }

    /// When the picture starts, counted from the start of the file (sound can start earlier).
    pub fn picture_start(&self) -> f64 {
        (self.video_start - self.start_time).max(0.0)
    }

    /// When the picture stops, counted from the start of the file.
    pub fn picture_end(&self) -> f64 {
        self.picture_start() + self.picture_secs()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioInfo {
    pub codec: String,
    pub channels: u32,
    /// Bits per second; 0 when the file doesn't say.
    #[serde(default)]
    pub bitrate: f64,
    /// What the track is marked as, the way ffmpeg's `-disposition` takes it
    /// ("default+forced"); empty for no mark.
    #[serde(default)]
    pub disposition: String,
}

/// Why a file in a format outside `tools::VIDEO_FORMATS` is not read.
const NOT_A_VIDEO: &str = "this is not a video file in a format av1opt reads, whatever its name says: it may be a playlist or manifest that leads to other files, and those are not opened";

/// What ffprobe said in the end, on one line: the lines before it are the
/// demuxer's detail, and the last one starts with the path the caller already has.
pub(crate) fn reason(stderr: &str, path: &Path) -> String {
    if stderr.contains("Format not on whitelist") {
        return NOT_A_VIDEO.into();
    }
    let last = one_line(stderr);
    let last = last.strip_prefix(&*path.to_string_lossy()).map_or(last, |rest| rest.trim_start_matches(':').trim_start());
    if last.is_empty() { "could not read the file".into() } else { last.to_string() }
}

/// The last line of a message that has something on it, for places with room for one line.
pub fn one_line(message: &str) -> &str {
    message.lines().rev().map(str::trim).find(|l| !l.is_empty()).unwrap_or("")
}

/// Runs a program to its end and returns what it printed, unless `stop`, asked now and then
/// with its process id, gives a reason to end it first.
fn run_until(mut command: Command, mut stop: impl FnMut(u32) -> Option<String>) -> std::io::Result<Result<Output, String>> {
    let mut child = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    // Read as it comes: a program whose output fills the pipe would otherwise never end.
    fn collect(mut from: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = from.read_to_end(&mut bytes);
            bytes
        })
    }
    let stdout = child.stdout.take().map(collect);
    let stderr = child.stderr.take().map(collect);
    let mut pause = Duration::from_millis(1);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if let Some(why) = stop(child.id()) {
            // Looked up before the kill: a child whose parent is gone cannot be found any more,
            // and one left behind would go on reading.
            let family = process_and_descendants(child.id());
            let _ = child.kill();
            for process in family.into_iter().skip(1) {
                // SAFETY: kill takes plain numbers and touches no memory of ours.
                unsafe { libc::kill(process as libc::pid_t, libc::SIGKILL) };
            }
            // A process stuck in a stalled mount ends only once the mount answers. It is
            // reaped then, by this thread, and nobody waits for that here.
            std::thread::spawn(move || drop(child.wait()));
            return Ok(Err(why));
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(50));
    };
    let bytes = |reader: Option<std::thread::JoinHandle<Vec<u8>>>| reader.and_then(|r| r.join().ok()).unwrap_or_default();
    Ok(Ok(Output { status, stdout: bytes(stdout), stderr: bytes(stderr) }))
}

/// Runs a program to its end and returns what it printed, or None if `limit` ran out first.
fn run_within(command: Command, limit: Duration) -> std::io::Result<Option<Output>> {
    let started = Instant::now();
    Ok(run_until(command, |_| (started.elapsed() >= limit).then(String::new))?.ok())
}

fn too_long(limit: Duration) -> String {
    format!("the probe took too long (over {} s) and was stopped; the drive may have stalled", limit.as_secs_f64().round())
}

/// A process and those it started, the process first. Only itself where the system does not say.
fn process_and_descendants(process: u32) -> Vec<u32> {
    // More than any wrapper around ffprobe starts, so that a tree changing under this walk ends it.
    const MOST: usize = 256;
    let mut family = vec![process];
    let mut next = 0;
    while next < family.len() && family.len() < MOST {
        let parent = family[next];
        next += 1;
        let Ok(threads) = std::fs::read_dir(format!("/proc/{parent}/task")) else { continue };
        for thread in threads.flatten() {
            let Ok(children) = std::fs::read_to_string(thread.path().join("children")) else { continue };
            for child in children.split_ascii_whitespace().filter_map(|c| c.parse().ok()) {
                if !family.contains(&child) {
                    family.push(child);
                }
            }
        }
    }
    family
}

/// How many bytes one process has read so far, from any file.
fn bytes_read_by_one(process: u32) -> Option<u64> {
    let io = std::fs::read_to_string(format!("/proc/{process}/io")).ok()?;
    io.lines().find_map(|l| l.strip_prefix("rchar:"))?.trim().parse().ok()
}

/// How many bytes a running process and those it started have read so far: what is called ffprobe
/// may be a script that waits for the real one while that does the reading. None where the system
/// does not say.
fn bytes_read_by(process: u32) -> Option<u64> {
    let own = bytes_read_by_one(process)?;
    let others = process_and_descendants(process).into_iter().skip(1).filter_map(bytes_read_by_one);
    Some(others.fold(own, u64::saturating_add))
}

/// Decides when a run that reads a whole file has stopped getting anywhere.
struct ReadWatch {
    /// How long it may go without reading anything.
    quiet: Duration,
    /// How long it gets in all, for when its reading cannot be seen.
    limit: Duration,
    /// How long it gets in all however much it reads.
    most: Duration,
    started: Instant,
    read: Option<u64>,
    since: Instant,
}

impl ReadWatch {
    fn new(quiet: Duration, limit: Duration, most: Duration) -> ReadWatch {
        let now = Instant::now();
        ReadWatch { quiet, limit, most, started: now, read: None, since: now }
    }

    /// Why to stop it, now that it has read `read` bytes (None: that cannot be seen).
    fn stop(&mut self, read: Option<u64>) -> Option<String> {
        if self.started.elapsed() >= self.most {
            return Some(format!("the probe was still reading after {} s, far longer than a file of this size takes, and was stopped; the file may be damaged, or the drive very slow", self.most.as_secs_f64().round()));
        }
        if read.is_none() {
            return (self.started.elapsed() >= self.limit).then(|| too_long(self.limit));
        }
        if read != self.read {
            (self.read, self.since) = (read, Instant::now());
        }
        (self.since.elapsed() >= self.quiet).then(|| format!("the probe took too long: it read nothing for {} s and was stopped; the drive may have stalled", self.quiet.as_secs_f64().round()))
    }
}

/// How much a run of ffprobe has to read, which sets how long it gets.
pub enum Reads {
    /// The headers, or some bounded part of the file.
    Little,
    /// The packets of the whole of this file.
    AllOf(u64),
}

impl Reads {
    /// The packets of the whole file at `path`.
    pub fn all_of(path: &Path) -> Reads {
        Reads::AllOf(std::fs::metadata(path).map_or(0, |m| m.len()))
    }

    fn limit(&self) -> Duration {
        match self {
            Reads::Little => PROBE_LIMIT,
            Reads::AllOf(bytes) => PROBE_LIMIT + LIMIT_PER_GIGABYTE * u32::try_from(bytes.div_ceil(1_000_000_000)).unwrap_or(u32::MAX / 120),
        }
    }

    /// How long a run gets even while it goes on reading.
    fn most(&self) -> Duration {
        match self {
            Reads::Little => PROBE_LIMIT,
            Reads::AllOf(bytes) => self.limit().max(PROBE_LIMIT + Duration::from_secs(bytes / SLOWEST_READ)),
        }
    }
}

#[cfg(test)]
thread_local! {
    /// The time limit of ffprobe runs started by this thread, and how long one that reads a
    /// whole file may read nothing, for the tests of what happens when one runs out.
    pub(crate) static LIMIT_IN_TESTS: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
}

/// Runs ffprobe with a limit, so that a stalled mount costs the file and not the whole run, and
/// returns what it printed.
pub fn run_ffprobe(command: Command, reads: Reads) -> Result<Output> {
    let (limit, most) = (reads.limit(), reads.most());
    let quiet = READ_STALL;
    #[cfg(test)]
    let (limit, quiet) = LIMIT_IN_TESTS.get().map_or((limit, quiet), |l| (l, l));
    let run = match reads {
        Reads::Little => run_within(command, limit).map(|out| out.ok_or_else(|| too_long(limit))),
        Reads::AllOf(_) => {
            run_watched(command, ReadWatch::new(quiet, limit, most))
        }
    };
    match run {
        Ok(Ok(out)) => Ok(out),
        Ok(Err(why)) => Err(NotJudged(why).into()),
        Err(e) => Err(NotJudged(format!("failed to run ffprobe: {e}")).into()),
    }
}

/// The mastering display of an HDR video, as ten numbers: the red, green, blue and white points
/// (x and y each), then the lowest and highest brightness in nits.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MasteringDisplay([f64; 10]);

/// What an HDR video says of its brightness besides its transfer function.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HdrData {
    pub mastering: Option<MasteringDisplay>,
    /// Content light level: the brightest pixel and the brightest frame on average, in nits.
    /// Zeros mean "not known", and count as nothing said.
    pub light_level: Option<(u64, u64)>,
}

impl HdrData {
    /// Read from a `side_data_list` of ffprobe. A mastering display given in part is left out:
    /// MP4 has no way to store one, so it could not be asked of an output.
    fn from_side_data(list: &Value) -> HdrData {
        let mut data = HdrData::default();
        for entry in list.as_array().into_iter().flatten() {
            match entry["side_data_type"].as_str() {
                Some("Mastering display metadata") => {
                    let fields = ["red_x", "red_y", "green_x", "green_y", "blue_x", "blue_y", "white_point_x", "white_point_y", "min_luminance", "max_luminance"];
                    let mut numbers = [0.0; 10];
                    let all = fields.iter().zip(&mut numbers).all(|(field, number)| fraction(&entry[*field]).map(|n| *number = n).is_some());
                    data.mastering = all.then_some(MasteringDisplay(numbers));
                }
                Some("Content light level metadata") => {
                    let level = (entry["max_content"].as_u64().unwrap_or(0), entry["max_average"].as_u64().unwrap_or(0));
                    data.light_level = (level != (0, 0)).then_some(level);
                }
                _ => {}
            }
        }
        data
    }

    /// What this video says that `other` has lost or says differently, in words. The numbers are
    /// stored differently by each format (MP4 in fixed steps, Matroska as decimals, AV1 itself
    /// in powers of two), so they are compared to within what those steps can move them.
    pub fn not_kept_in(&self, other: &HdrData) -> Option<&'static str> {
        let same_display = |a: &MasteringDisplay, b: &MasteringDisplay| {
            let colours = a.0[..8].iter().zip(&b.0[..8]).all(|(x, y)| (x - y).abs() <= 0.002);
            let brightness = a.0[8..].iter().zip(&b.0[8..]).all(|(x, y)| (x - y).abs() <= 0.001 + x.abs() * 0.01);
            colours && brightness
        };
        match (&self.mastering, &other.mastering) {
            (Some(_), None) => return Some("lost the mastering display data"),
            (Some(a), Some(b)) if !same_display(a, b) => return Some("has other mastering display data"),
            _ => {}
        }
        match (self.light_level, other.light_level) {
            (Some(_), None) => Some("lost the content light level"),
            (Some(a), Some(b)) if a != b => Some("has another content light level"),
            _ => None,
        }
    }
}

/// A number ffprobe writes as a fraction ("34000/50000"), or plainly.
fn fraction(value: &Value) -> Option<f64> {
    let text = value.as_str()?;
    let number = match text.split_once('/') {
        Some((top, bottom)) => top.parse::<f64>().ok()? / bottom.parse::<f64>().ok()?,
        None => text.parse().ok()?,
    };
    number.is_finite().then_some(number)
}

/// The HDR data of stream `stream`: what its container says, and what its first frame says. A
/// source can have it in either place, while a player goes by the container.
pub fn hdr_data(ffprobe: &str, path: &Path, stream: usize) -> Result<(HdrData, HdrData)> {
    let mut command = crate::tools::probe_command(ffprobe);
    command.args(["-v", "error", "-select_streams", &stream.to_string(), "-read_intervals", "%+#1", "-show_streams", "-show_frames", "-of", "json"]).arg(crate::tools::probe_arg(path));
    let out = run_ffprobe(command, Reads::Little)?;
    if !out.status.success() {
        bail!("ffprobe: {}", reason(&String::from_utf8_lossy(&out.stderr), path));
    }
    let j: Value = serde_json::from_slice(&out.stdout).context("ffprobe returned invalid JSON")?;
    Ok((HdrData::from_side_data(&j["streams"][0]["side_data_list"]), HdrData::from_side_data(&j["frames"][0]["side_data_list"])))
}

fn run_watched(command: Command, mut watch: ReadWatch) -> std::io::Result<Result<Output, String>> {
    run_until(command, |process| watch.stop(bytes_read_by(process)))
}

pub fn probe(ffprobe: &str, path: &Path, file_size: u64) -> Result<MediaInfo> {
    probe_within(ffprobe, path, file_size, PROBE_LIMIT)
}

fn probe_within(ffprobe: &str, path: &Path, file_size: u64, limit: Duration) -> Result<MediaInfo> {
    let mut command = crate::tools::probe_command(ffprobe);
    command.args(["-v", "error", "-show_format", "-show_streams", "-of", "json"]).arg(crate::tools::probe_arg(path));
    let out = match run_within(command, limit) {
        Ok(Some(out)) => out,
        Ok(None) => return Err(NotJudged(too_long(limit)).into()),
        Err(e) => return Err(NotJudged(format!("failed to run {ffprobe}: {e}")).into()),
    };
    if !out.status.success() {
        let why = reason(&String::from_utf8_lossy(&out.stderr), path);
        if out.status.code().is_none() {
            return Err(NotJudged(format!("ffprobe was stopped before it finished ({})", out.status)).into());
        }
        if NOT_THE_FILES_FAULT.iter().any(|text| why.contains(text)) {
            return Err(NotJudged(format!("ffprobe: {why}")).into());
        }
        // ffprobe names what it dislikes in a file. Failing without a word points at the
        // program, so the file is not to be remembered as unreadable.
        if one_line(&String::from_utf8_lossy(&out.stderr)).is_empty() {
            return Err(NotJudged(format!("ffprobe failed without saying why ({})", out.status)).into());
        }
        bail!("ffprobe: {why}");
    }
    // It said it succeeded, so an answer that does not parse is one that did not all arrive.
    let Ok(j) = serde_json::from_slice::<Value>(&out.stdout) else { return Err(NotJudged("ffprobe returned invalid JSON".into()).into()) };
    let streams = j["streams"].as_array().cloned().unwrap_or_default();

    // Cover art is a video stream with attached_pic=1, but some files (often MKV) carry it
    // as a plain still-image stream. Prefer real video codecs; fall back to any video.
    const STILL_IMAGE: &[&str] = &["mjpeg", "png", "bmp", "webp", "tiff", "jpegls"];
    let candidates: Vec<&Value> = streams
        .iter()
        .filter(|s| s["codec_type"] == "video" && s["disposition"]["attached_pic"].as_i64() != Some(1))
        .collect();
    let video = candidates
        .iter()
        .find(|s| !STILL_IMAGE.contains(&s["codec_name"].as_str().unwrap_or("")))
        .or(candidates.first())
        .copied()
        .context("no video stream")?;

    let num = |v: &Value| -> Option<f64> {
        v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    };
    let start_time = num(&j["format"]["start_time"]).unwrap_or(0.0);
    let avg_rate = parse_rate(video["avg_frame_rate"].as_str());
    let nominal_rate = parse_rate(video["r_frame_rate"].as_str());
    let fps = avg_rate.or(nominal_rate).unwrap_or(0.0);
    let vfr = matches!((avg_rate, nominal_rate), (Some(a), Some(r)) if (a - r).abs() / r > 0.005);
    let mut video_start = num(&video["start_time"]).unwrap_or(start_time);

    // Matroska keeps a stream's length in a tag ("00:07:39.966000000"), MP4 in the stream.
    let tagged_length = video["tags"]["DURATION"].as_str().and_then(clock_secs);
    let stated = num(&j["format"]["duration"]);
    // What a Matroska file calls its duration depends on who wrote it, which only shows when
    // the file does not start at zero: ffmpeg writes when its last frame ends, mkvmerge how
    // long it is. Nearly every file does start at zero, and pays nothing for this.
    let frame = if fps > 0.0 { 1.0 / fps } else { 0.1 };
    let starts_late = j["format"]["format_name"].as_str().is_some_and(|f| f.contains("matroska")) && (start_time > frame || video_start - start_time > frame);
    let from_end = |end: f64| if starts_late { length_from_end(end, start_time) } else { end };
    let mut duration_s = stated.map(from_end).or_else(|| num(&video["duration"])).unwrap_or(0.0);
    let mut video_s = num(&video["duration"]).or_else(|| tagged_length.map(from_end)).unwrap_or(0.0);
    if starts_late
        && let Some(ends) = stated.and_then(|d| matroska_ends(ffprobe, path, video["index"].as_u64().unwrap_or(0), frame, start_time + d))
        && ends.file > start_time
    {
        video_start = ends.first_frame.unwrap_or(video_start);
        duration_s = ends.file - start_time;
        // A picture that stopped long before the sound is not in the end of the file: its tag
        // says how long it is the way the file's duration does.
        let duration_is_an_end = stated.is_some_and(|d| (d - ends.file).abs() <= (d - duration_s).abs());
        let by_tag = tagged_length.map(|t| if duration_is_an_end { t - video_start } else { t });
        video_s = ends.picture.map(|end| end - video_start).or(by_tag).filter(|s| *s > 0.0).unwrap_or(video_s);
    }
    let audio: Vec<&Value> = streams.iter().filter(|s| s["codec_type"] == "audio").collect();
    let total_bitrate = if duration_s > 0.0 { file_size as f64 * 8.0 / duration_s } else { 0.0 };
    // Matroska has no field for a stream's rate; mkvmerge writes it as a tag. A file encoded
    // again by a program that copies tags carries the tag of what it was made from, which
    // shows where the tags ask for more than the file holds.
    let tagged = |s: &Value| num(&s["tags"]["BPS"]).or_else(|| num(&s["tags"]["BPS-eng"])).filter(|r| *r > 0.0);
    let tags_hold = streams.iter().filter_map(tagged).sum::<f64>() <= total_bitrate;
    let rate = |s: &Value| num(&s["bit_rate"]).or_else(|| tagged(s).filter(|_| tags_hold));
    let audio_bitrate: f64 = audio.iter().filter_map(|s| rate(s)).sum();
    // Without a stated rate: the bytes that aren't sound, over the seconds that have picture.
    let picture_s = if video_s > 0.0 && video_s < duration_s { video_s } else { duration_s };
    let video_bitrate = rate(video).unwrap_or_else(|| {
        let spread = if picture_s > 0.0 { duration_s / picture_s } else { 1.0 };
        (total_bitrate - audio_bitrate).max(total_bitrate * 0.5) * spread
    });

    let transfer = video["color_transfer"].as_str().unwrap_or("");
    let dolby_vision = video["side_data_list"]
        .as_array()
        .is_some_and(|l| l.iter().any(|d| d["side_data_type"].as_str().is_some_and(|t| t.contains("DOVI"))));
    let field_order = video["field_order"].as_str().unwrap_or("progressive");
    // Phones store portrait video as landscape plus a 90/270 degree display rotation. ffmpeg
    // applies the rotation when decoding, so report the dimensions as displayed.
    let rotation = video["side_data_list"]
        .as_array()
        .and_then(|l| l.iter().find_map(|d| d["rotation"].as_f64()))
        .unwrap_or(0.0);
    let (mut width, mut height) = (video["width"].as_u64().unwrap_or(0) as u32, video["height"].as_u64().unwrap_or(0) as u32);
    if (rotation.abs() - 90.0).abs() < 1.0 || (rotation.abs() - 270.0).abs() < 1.0 {
        std::mem::swap(&mut width, &mut height);
    }

    let main_index = video["index"].as_u64().unwrap_or(0);
    let covers: Vec<Cover> = streams
        .iter()
        .filter(|s| s["codec_type"] == "video" && s["index"].as_u64() != Some(main_index))
        .filter(|s| s["disposition"]["attached_pic"].as_i64() == Some(1) || STILL_IMAGE.contains(&s["codec_name"].as_str().unwrap_or("")))
        .map(|s| Cover {
            index: s["index"].as_u64().unwrap_or(0) as usize,
            codec: s["codec_name"].as_str().unwrap_or("unknown").to_string(),
            filename: s["tags"]["filename"].as_str().map(String::from),
        })
        .collect();

    // The marks a player goes by to pick a track.
    const MARKS: &[&str] = &["default", "dub", "original", "comment", "lyrics", "karaoke", "forced", "hearing_impaired", "visual_impaired", "clean_effects", "captions", "descriptions"];
    let marks = |s: &Value| MARKS.iter().filter(|m| s["disposition"][**m].as_i64() == Some(1)).copied().collect::<Vec<_>>().join("+");
    let cover_at: Vec<usize> = covers.iter().map(|c| c.index).collect();
    let other_streams = streams
        .iter()
        .filter(|s| !matches!(s["codec_type"].as_str(), Some("audio" | "subtitle" | "attachment")))
        .filter(|s| s["index"].as_u64().is_some_and(|i| i != main_index && !cover_at.contains(&(i as usize))))
        .map(|s| {
            let kind = s["codec_type"].as_str().unwrap_or("unknown");
            let format = s["codec_name"].as_str().or(s["codec_tag_string"].as_str()).filter(|f| !f.is_empty() && !f.starts_with('['));
            format.map_or(kind.to_string(), |f| format!("{kind} ({f})"))
        })
        .collect();

    Ok(MediaInfo {
        video_index: main_index as usize,
        codec: video["codec_name"].as_str().unwrap_or("unknown").to_string(),
        width,
        height,
        fps,
        nominal_fps: nominal_rate.unwrap_or(0.0),
        time_base: video["time_base"]
            .as_str()
            .and_then(|s| s.split_once('/'))
            .and_then(|(n, d)| Some((n.parse().ok()?, d.parse().ok()?)))
            .filter(|&(n, d): &(i64, i64)| n > 0 && d > 0)
            .unwrap_or((0, 0)),
        vfr,
        duration_s,
        video_s,
        start_time,
        video_start,
        video_bitrate,
        pix_fmt: video["pix_fmt"].as_str().unwrap_or("").to_string(),
        hdr: matches!(transfer, "smpte2084" | "arib-std-b67") || dolby_vision,
        dolby_vision,
        color_transfer: transfer.to_string(),
        interlaced: !matches!(field_order, "progressive" | "unknown"),
        audio_streams: audio.len() as u32,
        subtitle_streams: streams.iter().filter(|s| s["codec_type"] == "subtitle").count() as u32,
        audio: audio
            .iter()
            .map(|s| AudioInfo {
                codec: s["codec_name"].as_str().unwrap_or("unknown").to_string(),
                channels: s["channels"].as_u64().unwrap_or(2) as u32,
                // Matroska has no per-stream bitrate field; mkvmerge writes it as a tag.
                bitrate: num(&s["bit_rate"]).or_else(|| num(&s["tags"]["BPS"])).unwrap_or(0.0),
                disposition: marks(s),
            })
            .collect(),
        subtitle_codecs: streams
            .iter()
            .filter(|s| s["codec_type"] == "subtitle")
            .map(|s| s["codec_name"].as_str().unwrap_or("unknown").to_string())
            .collect(),
        subtitle_dispositions: streams.iter().filter(|s| s["codec_type"] == "subtitle").map(marks).collect(),
        other_streams,
        covers,
        attachments: streams.iter().filter(|s| s["codec_type"] == "attachment").count() as u32,
        tags: j["format"]["tags"]
            .as_object()
            .map(|t| t.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect())
            .unwrap_or_default(),
    })
}

/// Bits per second of audio stream number `index` (among the audio streams), measured from
/// the packets of its first two minutes. For containers that don't state it.
pub fn audio_bitrate(ffprobe: &str, path: &Path, index: usize) -> Option<f64> {
    let mut command = crate::tools::probe_command(ffprobe);
    command
        .args(["-v", "error", "-read_intervals", "%+120", "-select_streams", &format!("a:{index}")])
        .args(["-show_entries", "packet=pts_time,duration_time,size", "-of", "csv=p=0"])
        .arg(crate::tools::probe_arg(path));
    let out = run_ffprobe(command, Reads::Little).ok()?;
    let (mut bytes, mut first, mut last) = (0.0, f64::INFINITY, f64::NEG_INFINITY);
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut f = line.split(',').map(|x| x.trim().parse::<f64>().ok());
        let (Some(Some(pts)), Some(dur), Some(Some(size))) = (f.next(), f.next(), f.next()) else { continue };
        bytes += size;
        first = first.min(pts);
        last = last.max(pts + dur.unwrap_or(0.0));
    }
    (last - first > 1.0).then(|| bytes * 8.0 / (last - first))
}

/// Bytes of stream number `index` of a file, counted from its packets: what the picture
/// takes in a container that states no rate for it (Matroska), where going by the size of
/// the file would count the sound in.
pub fn stream_bytes(ffprobe: &str, path: &Path, index: usize) -> Option<u64> {
    let mut command = crate::tools::probe_command(ffprobe);
    command.args(["-v", "error", "-select_streams", &index.to_string(), "-show_entries", "packet=size", "-of", "csv=p=0"]).arg(crate::tools::probe_arg(path));
    let out = run_ffprobe(command, Reads::all_of(path)).ok().filter(|o| o.status.success())?;
    let bytes: u64 = String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.trim().trim_end_matches(',').parse::<u64>().ok()).sum();
    (bytes > 0).then_some(bytes)
}

/// Parses ffprobe rates like "30000/1001".
fn parse_rate(s: Option<&str>) -> Option<f64> {
    let (n, d) = s?.split_once('/')?;
    let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
    (d > 0.0 && n > 0.0).then(|| n / d)
}

/// How long a Matroska file is if what it calls its duration is the time its last frame
/// ends, as ffmpeg writes it: for a file that starts at 5 s, 5 s less than that. What is
/// left to go by where the packets could not be read (see [`matroska_ends`]).
fn length_from_end(end: f64, start: f64) -> f64 {
    if start > 0.0 && end > start { end - start } else { end }
}

/// Where a Matroska file's packets say it starts and ends, in the file's timestamps.
#[derive(Debug, PartialEq)]
struct PacketEnds {
    /// Where the last packet of any stream ends.
    file: f64,
    /// Where the last packet of the video ends, if it is among the file's last.
    picture: Option<f64>,
    /// When the video's first frame is shown.
    first_frame: Option<f64>,
}

/// How much of the end of a file is read for [`matroska_ends`]: enough for the last packet
/// of every stream that goes on to the end, and little enough to take no time.
const END_SECS: f64 = 10.0;

/// A disk that has gone quiet should not stop a scan: past this the file's own word stands.
const END_READ_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Reads the first packets of the video and the last packets of the file. Only those: this
/// costs the same on a file of 50 GB. `latest_end` is the latest the file can end; a seek past
/// the real end lands on the last place the file's index knows, which is what is wanted.
fn matroska_ends(ffprobe: &str, path: &Path, video: u64, frame: f64, latest_end: f64) -> Option<PacketEnds> {
    let index = video.to_string();
    let first = packets(ffprobe, path, &["-select_streams", &index, "-read_intervals", "%+#16", "-show_entries", "packet=pts_time"])?;
    let first_frame = first.lines().filter_map(|l| l.trim().trim_end_matches(',').parse::<f64>().ok()).min_by(f64::total_cmp);
    // An end far past any file: ffprobe printed no packets for an interval without one as soon
    // as another option came before it.
    let from = format!("{:.3}%+1000000", (latest_end - END_SECS).max(0.0));
    let last = packets(ffprobe, path, &["-read_intervals", &from, "-show_entries", "packet=stream_index,pts_time,duration_time"])?;
    let ends = last_ends(&last, &index, frame);
    Some(PacketEnds { file: ends.0?, picture: ends.1, first_frame })
}

/// From lines of "stream,time,duration": where the last packet of any stream ends, and where
/// the last of stream `video` does. A video packet without a duration lasts one `frame`.
fn last_ends(lines: &str, video: &str, frame: f64) -> (Option<f64>, Option<f64>) {
    let (mut file, mut picture) = (None::<f64>, None::<f64>);
    for line in lines.lines() {
        let mut fields = line.trim().trim_end_matches(',').split(',');
        let (Some(stream), Some(Ok(pts))) = (fields.next(), fields.next().map(str::parse::<f64>)) else { continue };
        let lasts = fields.next().and_then(|d| d.parse::<f64>().ok());
        let is_video = stream == video;
        let end = pts + lasts.unwrap_or(if is_video { frame } else { 0.0 });
        file = Some(file.map_or(end, |f| f.max(end)));
        if is_video {
            picture = Some(picture.map_or(end, |p| p.max(end)));
        }
    }
    (file, picture)
}

/// The packet lines ffprobe prints for `what`, or None if it fails or takes too long.
fn packets(ffprobe: &str, path: &Path, what: &[&str]) -> Option<String> {
    let mut command = crate::tools::probe_command(ffprobe);
    command.args(["-v", "error"]).args(what).args(["-of", "csv=p=0"]).arg(crate::tools::probe_arg(path));
    let limit = END_READ_LIMIT;
    #[cfg(test)]
    let limit = LIMIT_IN_TESTS.get().unwrap_or(limit);
    let out = run_within(command, limit).ok()??;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// Seconds in a "hh:mm:ss.fraction" clock time.
fn clock_secs(s: &str) -> Option<f64> {
    let mut parts = s.trim().split(':');
    let (h, m, sec) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let (h, m, sec): (f64, f64, f64) = (h.parse().ok()?, m.parse().ok()?, sec.parse().ok()?);
    (h >= 0.0 && m >= 0.0 && sec >= 0.0).then_some(h * 3600.0 + m * 60.0 + sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A short Matroska file made by ffmpeg with the picture at `video_at`, and sound from
    /// `sound_at` to the end of the picture if asked for.
    fn matroska(ffmpeg: &str, path: &Path, video_at: f64, sound_at: Option<f64>, secs: u32) {
        let mut c = Command::new(ffmpeg);
        c.args(["-v", "error", "-nostdin", "-y", "-itsoffset", &video_at.to_string(), "-f", "lavfi", "-i"]).arg(format!("testsrc2=s=64x64:r=25:d={secs}"));
        if let Some(at) = sound_at {
            let length = video_at + f64::from(secs) - at;
            c.args(["-itsoffset", &at.to_string(), "-f", "lavfi", "-i"]).arg(format!("sine=d={length}"));
        }
        let made = c.args(["-c:v", "libx264", "-preset", "ultrafast", "-g", "25", "-pix_fmt", "yuv420p"]).arg(path).output().unwrap();
        assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
    }

    /// Writes `secs` into the file's Duration element and `tag` over its stream length, as a
    /// program that states lengths instead of ends would have them.
    fn state_length(path: &Path, secs: f64, tag: (&str, &str)) {
        let mut bytes = std::fs::read(path).unwrap();
        let at = bytes.windows(3).position(|w| w == [0x44, 0x89, 0x88]).expect("a Duration element") + 3;
        bytes[at..at + 8].copy_from_slice(&(secs * 1000.0).to_be_bytes());
        let at = bytes.windows(tag.0.len()).position(|w| w == tag.0.as_bytes()).expect("a DURATION tag");
        bytes[at..at + tag.1.len()].copy_from_slice(tag.1.as_bytes());
        std::fs::write(path, bytes).unwrap();
    }

    fn close(a: f64, b: f64, what: &str) {
        assert!((a - b).abs() < 0.05, "{what}: {a} where {b} was expected");
    }

    #[test]
    fn hdr_data_counts_as_kept_only_when_it_is_there_and_the_same() {
        let side = |max: &str, content: u64| {
            serde_json::json!([
                {"side_data_type": "Mastering display metadata", "red_x": "34000/50000", "red_y": "16000/50000", "green_x": "13250/50000", "green_y": "34500/50000",
                 "blue_x": "7500/50000", "blue_y": "3000/50000", "white_point_x": "15635/50000", "white_point_y": "16450/50000", "min_luminance": "1/10000", "max_luminance": max},
                {"side_data_type": "Content light level metadata", "max_content": content, "max_average": 400},
            ])
        };
        let source = HdrData::from_side_data(&side("10000000/10000", 1000));
        assert!(source.mastering.is_some());
        assert_eq!(source.light_level, Some((1000, 400)));
        assert_eq!(source.not_kept_in(&source), None);
        // The same value as Matroska and AV1 write it.
        assert_eq!(source.not_kept_in(&HdrData::from_side_data(&side("1000/1", 1000))), None);
        assert_eq!(source.not_kept_in(&HdrData::from_side_data(&side("256000/256", 1000))), None);
        // What a video joined from bare streams looked like: nothing in its container.
        assert_eq!(source.not_kept_in(&HdrData::default()), Some("lost the mastering display data"));
        assert_eq!(source.not_kept_in(&HdrData { light_level: None, ..source }), Some("lost the content light level"));
        assert_eq!(source.not_kept_in(&HdrData::from_side_data(&side("4000/1", 1000))), Some("has other mastering display data"));
        assert_eq!(source.not_kept_in(&HdrData::from_side_data(&side("1000/1", 4000))), Some("has another content light level"));
        // A source with nothing to lose, or only "not known" or half a display, asks nothing.
        assert_eq!(HdrData::default().not_kept_in(&HdrData::default()), None);
        let empty = serde_json::json!([
            {"side_data_type": "Content light level metadata", "max_content": 0, "max_average": 0},
            {"side_data_type": "Mastering display metadata", "min_luminance": "1/10000", "max_luminance": "1000/1"},
        ]);
        assert_eq!(HdrData::from_side_data(&empty), HdrData::default());
    }

    #[test]
    fn a_matroska_file_is_measured_from_its_packets_when_it_does_not_start_at_zero() {
        let Some(tools) = crate::tools::real_tools("a_matroska_file_is_measured_from_its_packets") else { return };
        let ffmpeg = std::env::var("AV1OPT_FFMPEG").unwrap();
        let dir = std::env::temp_dir().join(format!("av1opt-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // ffprobe behind a script that counts its runs.
        let runs = dir.join("runs");
        let counting = dir.join("ffprobe");
        std::fs::write(&counting, format!("#!/bin/sh\necho run >> '{}'\nexec '{}' \"$@\"\n", runs.display(), tools.ffprobe)).unwrap();
        std::os::unix::fs::PermissionsExt::set_mode(&mut std::fs::metadata(&counting).unwrap().permissions(), 0o755);
        std::fs::set_permissions(&counting, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let read = |path: &Path| {
            let _ = std::fs::remove_file(&runs);
            let info = probe(&counting.to_string_lossy(), path, std::fs::metadata(path).unwrap().len()).unwrap();
            (info, std::fs::read_to_string(&runs).unwrap().lines().count())
        };

        // At zero: what the file says, from the one run every file costs.
        let plain = dir.join("plain.mkv");
        matroska(&ffmpeg, &plain, 0.0, None, 4);
        let (info, cost) = read(&plain);
        close(info.duration_s, 4.0, "plain");
        assert_eq!(cost, 1);

        // Starting at 5 s, for 4 s, with a duration that is the end and one that is the length.
        let ends = dir.join("ends.mkv");
        matroska(&ffmpeg, &ends, 5.0, None, 4);
        let lengths = dir.join("lengths.mkv");
        std::fs::copy(&ends, &lengths).unwrap();
        state_length(&lengths, 4.0, ("00:00:09.000000000", "00:00:04.000000000"));
        for path in [&ends, &lengths] {
            let (info, cost) = read(path);
            close(info.duration_s, 4.0, "duration");
            close(info.video_s, 4.0, "picture");
            close(info.video_start, 5.0, "first frame");
            assert!(cost > 1);
        }
        // A length close to the start, which taken for an end left next to nothing.
        let close_by = dir.join("close-by.mkv");
        matroska(&ffmpeg, &close_by, 6.0, None, 6);
        state_length(&close_by, 6.04, ("00:00:12.000000000", "00:00:06.040000000"));
        let (info, _) = read(&close_by);
        close(info.duration_s, 6.0, "length near the start");
        close(info.picture_secs(), 6.0, "picture near the start");

        // The file starts at zero with sound, the picture at 2 s: a picture of 4 s, not 6.
        let late_picture = dir.join("late-picture.mkv");
        matroska(&ffmpeg, &late_picture, 2.0, Some(0.0), 4);
        let (info, cost) = read(&late_picture);
        close(info.video_s, 4.0, "late picture");
        close(info.picture_start(), 2.0, "where the picture starts");
        close(info.picture_end(), 6.0, "where the picture ends");
        assert!(cost > 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_last_packets_say_where_a_file_and_its_picture_end() {
        let lines = "0,10.000000,0.040000\n1,10.010000,0.023000,\n0,10.080000,N/A\n1,N/A,0.023000\n1,12.000000,N/A\n";
        assert_eq!(last_ends(lines, "0", 0.04), (Some(12.0), Some(10.12)));
        // The picture stopped long before: only the sound is at the end.
        assert_eq!(last_ends("1,99.000000,1.000000\n", "0", 0.04), (Some(100.0), None));
        assert_eq!(last_ends("", "0", 0.04), (None, None));
    }

    #[test]
    fn a_matroska_length_is_counted_from_where_the_file_starts() {
        assert_eq!(length_from_end(9.0, 5.0), 4.0);
        assert_eq!(length_from_end(9.0, 0.0), 9.0);
        // Early timestamps, or an end that makes no sense: the file's word stands.
        assert_eq!(length_from_end(9.0, -0.5), 9.0);
        assert_eq!(length_from_end(3.0, 5.0), 3.0);
    }

    #[test]
    fn a_probe_that_hangs_is_stopped_and_says_so() {
        let dir = std::env::temp_dir().join(format!("av1opt-probe-limit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (ffprobe, pid_file) = (dir.join("ffprobe"), dir.join("pid"));
        crate::files::write_script(&ffprobe, &format!("echo $$ > '{}'\nexec sleep 60\n", pid_file.display()));
        let started = Instant::now();
        let e = probe_within(ffprobe.to_str().unwrap(), &dir.join("a.mkv"), 1, Duration::from_millis(300)).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        assert!(left_unjudged(&e) && e.to_string().contains("took too long"), "{e}");
        // Killed and reaped: not even a zombie stays behind. On a busy machine the limit can
        // come before the script has got as far as saying who it is.
        if let Some(pid) = std::fs::read_to_string(&pid_file).ok().filter(|p| p.ends_with('\n')) {
            let process = PathBuf::from(format!("/proc/{}", pid.trim()));
            let waited = Instant::now();
            while process.exists() {
                assert!(waited.elapsed() < Duration::from_secs(5), "{} is still there", process.display());
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        // More output than a pipe holds does not get in the way of a probe that ends.
        let long = "x".repeat(300_000);
        crate::files::write_script(&ffprobe, &format!("echo '{{\"streams\":[],\"note\":\"{long}\"}}'\n"));
        let e = probe_within(ffprobe.to_str().unwrap(), &dir.join("a.mkv"), 1, Duration::from_secs(20)).unwrap_err();
        assert!(!left_unjudged(&e) && e.to_string().contains("no video stream"), "{e}");
        // Failing in silence says nothing of the file; naming a fault in it does.
        crate::files::write_script(&ffprobe, "exit 1\n");
        let e = probe_within(ffprobe.to_str().unwrap(), &dir.join("a.mkv"), 1, Duration::from_secs(20)).unwrap_err();
        assert!(left_unjudged(&e) && e.to_string().contains("without saying why"), "{e}");
        crate::files::write_script(&ffprobe, "echo 'Invalid data found when processing input' >&2\nexit 1\n");
        let e = probe_within(ffprobe.to_str().unwrap(), &dir.join("a.mkv"), 1, Duration::from_secs(20)).unwrap_err();
        assert!(!left_unjudged(&e), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_other_call_of_ffprobe_is_stopped_when_it_hangs() {
        let dir = std::env::temp_dir().join(format!("av1opt-probe-others-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ffprobe = dir.join("ffprobe");
        crate::files::write_script(&ffprobe, "exec sleep 60\n");
        let name = ffprobe.to_str().unwrap();
        let file = dir.join("a.mkv");
        let tools = crate::tools::Tools::new("false".into(), name.into(), None);
        LIMIT_IN_TESTS.set(Some(Duration::from_millis(100)));
        let started = Instant::now();
        assert!(audio_bitrate(name, &file, 0).is_none());
        assert!(stream_bytes(name, &file, 0).is_none());
        let errors = [
            tools.key_packets(&file).map(drop).unwrap_err(),
            tools.count_frames(&file).map(drop).unwrap_err(),
            crate::sections::read_chapters(name, &file).map(drop).unwrap_err(),
        ];
        LIMIT_IN_TESTS.set(None);
        assert!(started.elapsed() < Duration::from_secs(30), "{:?}", started.elapsed());
        for e in errors {
            assert!(e.to_string().contains("took too long"), "{e}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_short_read_at_the_end_of_a_file_does_not_wait_for_a_stuck_program() {
        let dir = std::env::temp_dir().join(format!("av1opt-probe-packets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ffprobe = dir.join("ffprobe");
        // The sleep outlives the script and keeps its output open, as a process stuck in a
        // stalled mount does: waiting for the end of that output would wait for it.
        crate::files::write_script(&ffprobe, "sleep 60 &\nwait\n");
        LIMIT_IN_TESTS.set(Some(Duration::from_millis(100)));
        let started = Instant::now();
        assert!(packets(ffprobe.to_str().unwrap(), &dir.join("a.mkv"), &["-show_packets"]).is_none());
        LIMIT_IN_TESTS.set(None);
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        crate::files::write_script(&ffprobe, "echo 1,2,3\n");
        assert_eq!(packets(ffprobe.to_str().unwrap(), &dir.join("a.mkv"), &[]).as_deref(), Some("1,2,3\n"));
        crate::files::write_script(&ffprobe, "echo 1,2,3\nexit 1\n");
        assert!(packets(ffprobe.to_str().unwrap(), &dir.join("a.mkv"), &[]).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_run_over_a_whole_file_goes_on_for_as_long_as_it_reads() {
        let dir = std::env::temp_dir().join(format!("av1opt-probe-slow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (ffprobe, data) = (dir.join("ffprobe"), dir.join("data"));
        std::fs::write(&data, "line\n".repeat(100)).unwrap();
        // Three times as long in all as it may go without reading.
        let script = format!("exec 3< '{}'\ni=0\nwhile [ $i -lt 30 ]; do read -r line <&3; sleep 0.1; i=$((i+1)); done\necho read\n", data.display());
        crate::files::write_script(&ffprobe, &script);
        let run = || {
            let mut command = Command::new(&ffprobe);
            command.arg("a.mkv");
            run_ffprobe(command, Reads::AllOf(1))
        };
        LIMIT_IN_TESTS.set(Some(Duration::from_secs(1)));
        assert_eq!(run().unwrap().stdout, b"read\n");
        // One that stops reading is stopped, well before anything like a limit on the whole.
        crate::files::write_script(&ffprobe, "exec sleep 60\n");
        let started = Instant::now();
        let e = run().unwrap_err();
        LIMIT_IN_TESTS.set(None);
        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
        assert!(left_unjudged(&e) && e.to_string().contains("read nothing for 1 s"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn progress_decides_where_it_can_be_seen_and_the_clock_where_it_cannot() {
        let short = Duration::from_millis(30);
        let long = Duration::from_secs(600);
        let mut watch = ReadWatch::new(short, long, long);
        assert!(watch.stop(Some(1)).is_none());
        std::thread::sleep(short * 2);
        assert!(watch.stop(Some(2)).is_none(), "it read something");
        assert!(watch.stop(Some(2)).is_none(), "and has only just stopped");
        std::thread::sleep(short * 2);
        assert!(watch.stop(Some(2)).is_some_and(|why| why.contains("read nothing")));
        let mut blind = ReadWatch::new(Duration::ZERO, short, long);
        assert!(blind.stop(None).is_none());
        std::thread::sleep(short * 2);
        assert!(blind.stop(None).is_some_and(|why| why.contains("took too long")));
        // This system does say what a process has read.
        assert!(bytes_read_by(std::process::id()).is_some());
        assert_eq!(bytes_read_by(u32::MAX), None);
    }

    /// Waits for a process to be gone, or a zombie nobody has collected yet.
    fn ends_soon(process: &str) -> bool {
        let gone = || std::fs::read_to_string(format!("/proc/{process}/stat")).map_or(true, |stat| stat.rsplit(')').next().is_some_and(|rest| rest.trim_start().starts_with('Z')));
        let started = Instant::now();
        while !gone() && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(20));
        }
        gone()
    }

    #[test]
    fn a_run_that_never_stops_reading_is_stopped_with_what_it_started() {
        let dir = std::env::temp_dir().join(format!("av1opt-probe-reader-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (ffprobe, pid_file) = (dir.join("ffprobe"), dir.join("reader"));
        crate::files::write_script(&ffprobe, &format!("cat /dev/zero > /dev/null &\necho $! > '{}'\nwait\n", pid_file.display()));
        let long = Duration::from_secs(600);
        let started = Instant::now();
        let why = run_watched(Command::new(&ffprobe), ReadWatch::new(long, long, Duration::from_millis(400))).unwrap().unwrap_err();
        assert!(why.contains("still reading"), "{why}");
        assert!(started.elapsed() < Duration::from_secs(60), "{:?}", started.elapsed());
        let reader = std::fs::read_to_string(&pid_file).unwrap();
        assert!(ends_soon(reader.trim()), "the reader the script started is still running");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_a_wrapper_waits_for_counts_as_its_reading() {
        let dir = std::env::temp_dir().join(format!("av1opt-probe-wrapper-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (ffprobe, reader, data) = (dir.join("ffprobe"), dir.join("reader"), dir.join("data"));
        std::fs::write(&data, "line\n".repeat(100)).unwrap();
        crate::files::write_script(&reader, &format!("exec 3< '{}'\ni=0\nwhile [ $i -lt 30 ]; do read -r line <&3; sleep 0.1; i=$((i+1)); done\n", data.display()));
        // It does not exec, so that the process watched reads nothing while its child goes on,
        // for three times as long as a run may go without reading.
        crate::files::write_script(&ffprobe, &format!("'{}'\necho read\n", reader.display()));
        let long = Duration::from_secs(600);
        let out = run_watched(Command::new(&ffprobe), ReadWatch::new(Duration::from_secs(1), long, long)).unwrap().unwrap();
        assert_eq!(out.stdout, b"read\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_time_a_run_gets_while_reading_grows_with_the_file_at_a_megabyte_a_second() {
        assert_eq!(Reads::Little.most(), Duration::from_secs(120));
        assert_eq!(Reads::AllOf(0).most(), Duration::from_secs(120));
        // Never less than a run gets that cannot be seen reading.
        assert_eq!(Reads::AllOf(1).most(), Reads::AllOf(1).limit());
        assert_eq!(Reads::AllOf(40_000_000_000).most(), Duration::from_secs(120 + 40_000));
        assert!(Reads::AllOf(u64::MAX).most() > Duration::from_secs(120));
    }

    #[test]
    fn a_run_over_a_whole_file_gets_longer_the_larger_the_file() {
        assert_eq!(Reads::Little.limit(), Duration::from_secs(120));
        assert_eq!(Reads::AllOf(0).limit(), Duration::from_secs(120));
        assert_eq!(Reads::AllOf(1).limit(), Duration::from_secs(180));
        assert_eq!(Reads::AllOf(1_000_000_000).limit(), Duration::from_secs(180));
        assert_eq!(Reads::AllOf(40_500_000_000).limit(), Duration::from_secs(120 + 41 * 60));
        // No size is so large that the sum overflows.
        assert!(Reads::AllOf(u64::MAX).limit() > Duration::from_secs(120));
    }

    #[test]
    fn picture_length_is_the_video_streams_own_when_sound_runs_on() {
        assert_eq!(clock_secs("00:07:39.966000000"), Some(459.966));
        assert_eq!(clock_secs("01:00:00"), Some(3600.0));
        assert_eq!((clock_secs("459.9"), clock_secs("a:b:c"), clock_secs("1:2:3:4")), (None, None, None));
        let info = |duration_s, video_s| MediaInfo { duration_s, video_s, ..Default::default() };
        assert_eq!(info(1052.8, 460.0).picture_secs(), 460.0);
        // Unknown, or a stream length past the container's: the container's word stands.
        assert_eq!(info(600.0, 0.0).picture_secs(), 600.0);
        assert_eq!(info(600.0, 600.4).picture_secs(), 600.0);
    }

    #[test]
    fn an_unreadable_file_gets_a_one_line_reason() {
        let path = Path::new("/v/a b.mkv");
        let said = "[matroska,webm @ 0x55] EBML header parsing failed\n/v/a b.mkv: Invalid data found when processing input\n\n";
        assert_eq!(reason(said, path), "Invalid data found when processing input");
        assert_eq!(reason("moov atom not found\n", path), "moov atom not found");
        assert_eq!(reason("", path), "could not read the file");
        assert_eq!(one_line("first\nsecond\n  \n"), "second");
    }

    #[test]
    fn a_rate_is_a_fraction_of_two_positive_numbers() {
        let rate = |s: &str| parse_rate(Some(s));
        assert_eq!(rate("25/1"), Some(25.0));
        assert_eq!(rate("30000/1001"), Some(30000.0 / 1001.0));
        assert_eq!(rate("23.976/1"), Some(23.976));
        for bad in ["0/0", "0/1", "1/0", "-25/1", "25/-1", "25", "", "/", "25/", "/1", "a/b", "NaN/1", "1/NaN"] {
            assert_eq!(rate(bad), None, "{bad:?}");
        }
        assert_eq!(parse_rate(None), None);
    }

    #[test]
    fn the_latest_end_counts_whatever_order_the_packets_come_in() {
        // Frames are stored out of display order.
        assert_eq!(last_ends("0,2.000000,0.5\n0,1.000000,0.5\n", "0", 0.04), (Some(2.5), Some(2.5)));
        // Stream 10 is not stream 1.
        assert_eq!(last_ends("10,5.000000,1.000000\n1,2.000000,0.500000\n", "1", 0.04), (Some(6.0), Some(2.5)));
        // Without a duration a frame lasts a frame, and a sound packet ends where it starts.
        assert_eq!(last_ends("0,1.000000\n1,1.500000\n", "0", 0.25), (Some(1.5), Some(1.25)));
    }

    #[test]
    fn packet_lines_that_say_nothing_are_passed_over() {
        for lines in ["\n\n", "side data\n", "0\n", "0,N/A,0.04\n", ",\n"] {
            assert_eq!(last_ends(lines, "0", 0.04), (None, None), "{lines:?}");
        }
        assert_eq!(last_ends("  0,1.000000,0.500000,  \r\n", "0", 0.04), (Some(1.5), Some(1.5)));
    }

    #[test]
    fn a_clock_time_has_three_parts_none_of_them_negative() {
        assert_eq!(clock_secs(" 00:00:01.500 "), Some(1.5));
        assert_eq!(clock_secs("00:00:00"), Some(0.0));
        assert_eq!(clock_secs("100:00:00"), Some(360_000.0));
        for bad in ["", "1:2", "-1:00:00", "00:-1:00", "00:00:-1", "::", "N/A", "NaN:0:0"] {
            assert_eq!(clock_secs(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_picture_is_placed_from_the_start_of_the_file() {
        let info = |start_time, video_start| MediaInfo { start_time, video_start, duration_s: 100.0, video_s: 40.0, ..Default::default() };
        assert_eq!(info(2.0, 5.0).picture_start(), 3.0);
        assert_eq!(info(5.0, 5.0).picture_start(), 0.0);
        // The picture cannot start before the file does.
        assert_eq!(info(5.0, 2.0).picture_start(), 0.0);
        assert_eq!(info(2.0, 5.0).picture_end(), 43.0);
    }

    #[test]
    fn a_stream_as_long_as_the_file_is_the_files_length() {
        let info = MediaInfo { duration_s: 60.0, video_s: 60.0, ..Default::default() };
        assert_eq!(info.picture_secs(), 60.0);
        assert_eq!(MediaInfo { video_s: -1.0, ..info.clone() }.picture_secs(), 60.0);
        assert_eq!(MediaInfo { video_s: f64::NAN, ..info }.picture_secs(), 60.0);
    }

    #[test]
    fn only_a_leading_path_is_taken_off_the_reason() {
        let path = Path::new("/v/a.mkv");
        assert_eq!(reason("/v/a.mkv: No such file", path), "No such file");
        assert_eq!(reason("/v/a.mkv:: odd", path), "odd");
        assert_eq!(reason("cannot open /v/a.mkv: denied", path), "cannot open /v/a.mkv: denied");
        // Nothing but the path: nothing was said.
        assert_eq!(reason("/v/a.mkv:", path), "could not read the file");
        assert_eq!(reason(" \n\t\n", path), "could not read the file");
    }

    #[test]
    fn the_last_line_with_something_on_it_is_the_message() {
        assert_eq!(one_line(""), "");
        assert_eq!(one_line("\n \n"), "");
        assert_eq!(one_line("only"), "only");
        assert_eq!(one_line("a\r\n  b  \r\n\r\n"), "b");
    }

    #[test]
    fn only_a_probe_without_an_opinion_leaves_a_file_unjudged() {
        assert!(left_unjudged(&NotJudged("timed out".into()).into()));
        assert!(left_unjudged(&anyhow::Error::from(NotJudged("timed out".into())).context("reading a.mkv")));
        assert!(!left_unjudged(&anyhow::anyhow!("Invalid data found when processing input")));
    }

    #[test]
    fn a_gigabyte_begun_counts_as_a_whole_one() {
        assert_eq!(Reads::AllOf(1_000_000_001).limit(), Duration::from_secs(120 + 2 * 60));
        assert_eq!(Reads::AllOf(2_000_000_000).limit(), Duration::from_secs(120 + 2 * 60));
    }
}
