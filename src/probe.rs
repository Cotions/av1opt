//! Runs ffprobe on a file and extracts the fields the scanner needs.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::process::Command;

/// Technical facts about one video file, as reported by ffprobe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaInfo {
    /// Absolute stream index of the main video (cover art can come first, so `0:v:0` is wrong).
    #[serde(default)]
    pub video_index: usize,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// Frame rate varies (average differs from the nominal rate), e.g. phone recordings.
    #[serde(default)]
    pub vfr: bool,
    pub duration_s: f64,
    /// Container start time and the video stream's own start time, in seconds.
    #[serde(default)]
    pub start_time: f64,
    #[serde(default)]
    pub video_start: f64,
    /// Bits per second of the video stream alone (estimated if the container doesn't say).
    pub video_bitrate: f64,
    pub pix_fmt: String,
    pub hdr: bool,
    pub interlaced: bool,
    pub audio_streams: u32,
    pub subtitle_streams: u32,
    /// Per-stream details, used by the encoder to plan copy vs re-encode.
    #[serde(default)]
    pub audio: Vec<AudioInfo>,
    #[serde(default)]
    pub subtitle_codecs: Vec<String>,
    /// Cover images: attached pictures, and still-image streams next to the real video.
    #[serde(default)]
    pub covers: Vec<Cover>,
    /// Non-image attachments (fonts for ASS subtitles, ...).
    #[serde(default)]
    pub attachments: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cover {
    pub index: usize,
    pub codec: String,
    /// Original attachment file name (Matroska), if any.
    pub filename: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioInfo {
    pub codec: String,
    pub channels: u32,
}

pub fn probe(ffprobe: &str, path: &Path, file_size: u64) -> Result<MediaInfo> {
    let out = Command::new(ffprobe)
        .args(["-v", "error", "-show_format", "-show_streams", "-of", "json"])
        .arg(path)
        .output()
        .with_context(|| format!("failed to run {ffprobe}"))?;
    if !out.status.success() {
        bail!("ffprobe: {}", String::from_utf8_lossy(&out.stderr).trim());
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
    let duration_s = num(&j["format"]["duration"])
        .or_else(|| num(&video["duration"]))
        .unwrap_or(0.0);
    let avg_rate = parse_rate(video["avg_frame_rate"].as_str());
    let nominal_rate = parse_rate(video["r_frame_rate"].as_str());
    let fps = avg_rate.or(nominal_rate).unwrap_or(0.0);
    let vfr = matches!((avg_rate, nominal_rate), (Some(a), Some(r)) if (a - r).abs() / r > 0.005);
    let start_time = num(&j["format"]["start_time"]).unwrap_or(0.0);
    let video_start = num(&video["start_time"]).unwrap_or(start_time);

    let audio: Vec<&Value> = streams.iter().filter(|s| s["codec_type"] == "audio").collect();
    let audio_bitrate: f64 = audio.iter().filter_map(|s| num(&s["bit_rate"])).sum();
    let total_bitrate = if duration_s > 0.0 { file_size as f64 * 8.0 / duration_s } else { 0.0 };
    let video_bitrate = num(&video["bit_rate"])
        .unwrap_or_else(|| (total_bitrate - audio_bitrate).max(total_bitrate * 0.5));

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
    let covers = streams
        .iter()
        .filter(|s| s["codec_type"] == "video" && s["index"].as_u64() != Some(main_index))
        .filter(|s| s["disposition"]["attached_pic"].as_i64() == Some(1) || STILL_IMAGE.contains(&s["codec_name"].as_str().unwrap_or("")))
        .map(|s| Cover {
            index: s["index"].as_u64().unwrap_or(0) as usize,
            codec: s["codec_name"].as_str().unwrap_or("unknown").to_string(),
            filename: s["tags"]["filename"].as_str().map(String::from),
        })
        .collect();

    Ok(MediaInfo {
        video_index: main_index as usize,
        codec: video["codec_name"].as_str().unwrap_or("unknown").to_string(),
        width,
        height,
        fps,
        vfr,
        duration_s,
        start_time,
        video_start,
        video_bitrate,
        pix_fmt: video["pix_fmt"].as_str().unwrap_or("").to_string(),
        hdr: matches!(transfer, "smpte2084" | "arib-std-b67") || dolby_vision,
        interlaced: !matches!(field_order, "progressive" | "unknown"),
        audio_streams: audio.len() as u32,
        subtitle_streams: streams.iter().filter(|s| s["codec_type"] == "subtitle").count() as u32,
        audio: audio
            .iter()
            .map(|s| AudioInfo {
                codec: s["codec_name"].as_str().unwrap_or("unknown").to_string(),
                channels: s["channels"].as_u64().unwrap_or(2) as u32,
            })
            .collect(),
        subtitle_codecs: streams
            .iter()
            .filter(|s| s["codec_type"] == "subtitle")
            .map(|s| s["codec_name"].as_str().unwrap_or("unknown").to_string())
            .collect(),
        covers,
        attachments: streams.iter().filter(|s| s["codec_type"] == "attachment").count() as u32,
    })
}

/// Parses ffprobe rates like "30000/1001".
fn parse_rate(s: Option<&str>) -> Option<f64> {
    let (n, d) = s?.split_once('/')?;
    let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
    (d > 0.0 && n > 0.0).then(|| n / d)
}
