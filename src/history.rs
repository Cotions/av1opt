//! Results of past encodes, one JSON line each, so `scan` estimates can learn from real data.
//! Stored in `$XDG_DATA_HOME/av1opt/history.jsonl` (or `$AV1OPT_HISTORY` if set).

use crate::probe::MediaInfo;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

/// Version of a history line. Bump it when a field changes meaning or is removed (adding
/// fields is fine); lines newer than this are ignored rather than misread. Lines from
/// before versioning have no `format` and count as 1.
pub const FORMAT: u32 = 1;

fn format_one() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    #[serde(default = "format_one")]
    pub format: u32,
    pub time: u64,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub duration_s: f64,
    /// Video stream bitrates in bits per second.
    pub source_bitrate: f64,
    pub output_bitrate: f64,
    pub target_vmaf: f64,
    pub preset: u8,
    pub mode: String,
}

impl Record {
    pub fn new(info: &MediaInfo, output_bitrate: f64, target_vmaf: f64, preset: u8, mode: &str) -> Self {
        let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        Record {
            format: FORMAT,
            time,
            codec: info.codec.clone(),
            width: info.width,
            height: info.height,
            fps: info.fps,
            duration_s: info.duration_s,
            source_bitrate: info.video_bitrate,
            output_bitrate,
            target_vmaf,
            preset,
            mode: mode.to_string(),
        }
    }
}

fn path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("AV1OPT_HISTORY") {
        return Some(PathBuf::from(p));
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(base.join("av1opt/history.jsonl"))
}

pub fn append(r: &Record) -> Result<()> {
    let p = path().context("no HOME for the encode history")?;
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&p)?;
    writeln!(f, "{}", serde_json::to_string(r)?)?;
    Ok(())
}

/// All readable records; a missing or partly corrupt file just yields fewer.
pub fn load() -> Vec<Record> {
    let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) else { return Vec::new() };
    text.lines().filter_map(|l| serde_json::from_str::<Record>(l).ok()).filter(|r| r.format <= FORMAT).collect()
}
