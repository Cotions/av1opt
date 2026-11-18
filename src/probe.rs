//! Runs ffprobe on a file and extracts the fields the scanner needs.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

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
    /// Length of the video stream itself, 0 when the container doesn't say. A recording whose
    /// sound runs on after the picture stops has a `duration_s` far past its last frame.
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

/// What ffprobe said in the end, on one line: the lines before it are the
/// demuxer's detail, and the last one starts with the path the caller already has.
fn reason(stderr: &str, path: &Path) -> String {
    let last = one_line(stderr);
    let last = last.strip_prefix(&*path.to_string_lossy()).map_or(last, |rest| rest.trim_start_matches(':').trim_start());
    if last.is_empty() { "could not read the file".into() } else { last.to_string() }
}

/// The last line of a message that has something on it, for places with room for one line.
pub fn one_line(message: &str) -> &str {
    message.lines().rev().map(str::trim).find(|l| !l.is_empty()).unwrap_or("")
}

pub fn probe(ffprobe: &str, path: &Path, file_size: u64) -> Result<MediaInfo> {
    let out = Command::new(ffprobe)
        .args(["-v", "error", "-show_format", "-show_streams", "-of", "json"])
        .arg(crate::tools::probe_arg(path))
        .output()
        .with_context(|| format!("failed to run {ffprobe}"))?;
    if !out.status.success() {
        bail!("ffprobe: {}", reason(&String::from_utf8_lossy(&out.stderr), path));
    }
    let j: Value = serde_json::from_slice(&out.stdout).context("ffprobe returned invalid JSON")?;
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
    let matroska = j["format"]["format_name"].as_str().is_some_and(|f| f.contains("matroska"));
    let from_end = |end: f64| if matroska { length_from_end(end, start_time) } else { end };
    let duration_s = num(&j["format"]["duration"]).map(from_end)
        .or_else(|| num(&video["duration"]))
        .unwrap_or(0.0);
    // Matroska keeps a stream's length in a tag ("00:07:39.966000000"), MP4 in the stream.
    let video_s = num(&video["duration"])
        .or_else(|| video["tags"]["DURATION"].as_str().and_then(clock_secs).map(from_end))
        .unwrap_or(0.0);
    let avg_rate = parse_rate(video["avg_frame_rate"].as_str());
    let nominal_rate = parse_rate(video["r_frame_rate"].as_str());
    let fps = avg_rate.or(nominal_rate).unwrap_or(0.0);
    let vfr = matches!((avg_rate, nominal_rate), (Some(a), Some(r)) if (a - r).abs() / r > 0.005);
    let video_start = num(&video["start_time"]).unwrap_or(start_time);

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
    let out = Command::new(ffprobe)
        .args(["-v", "error", "-read_intervals", "%+120", "-select_streams", &format!("a:{index}")])
        .args(["-show_entries", "packet=pts_time,duration_time,size", "-of", "csv=p=0"])
        .arg(crate::tools::probe_arg(path))
        .output()
        .ok()?;
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
    let out = Command::new(ffprobe)
        .args(["-v", "error", "-select_streams", &index.to_string(), "-show_entries", "packet=size", "-of", "csv=p=0"])
        .arg(crate::tools::probe_arg(path))
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let bytes: u64 = String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.trim().trim_end_matches(',').parse::<u64>().ok()).sum();
    (bytes > 0).then_some(bytes)
}

/// Parses ffprobe rates like "30000/1001".
fn parse_rate(s: Option<&str>) -> Option<f64> {
    let (n, d) = s?.split_once('/')?;
    let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
    (d > 0.0 && n > 0.0).then(|| n / d)
}

/// Seconds in a "hh:mm:ss.fraction" clock time.
/// How long a Matroska file is, from what it calls its duration: the time its last frame
/// ends, which for a file that starts at 5 s is 5 s more than it holds. Samples placed along
/// that length would fall past the end.
fn length_from_end(end: f64, start: f64) -> f64 {
    if start > 0.0 && end > start { end - start } else { end }
}

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

    #[test]
    fn a_matroska_length_is_counted_from_where_the_file_starts() {
        assert_eq!(length_from_end(9.0, 5.0), 4.0);
        assert_eq!(length_from_end(9.0, 0.0), 9.0);
        // Early timestamps, or an end that makes no sense: the file's word stands.
        assert_eq!(length_from_end(9.0, -0.5), 9.0);
        assert_eq!(length_from_end(3.0, 5.0), 3.0);
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
}
