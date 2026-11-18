//! Results of past encodes, one JSON line each, so `scan` estimates can learn from real data.
//! Stored in `$XDG_DATA_HOME/av1opt/history.jsonl` (or `$AV1OPT_HISTORY` if set).

use crate::probe::MediaInfo;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
    /// The CRF the search chose (absent in lines from before it was recorded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crf: Option<f64>,
    /// SVT-AV1 parameters besides the CRF (absent: unknown).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub svt_params: Option<String>,
    /// Seconds the CRF search and the encode of the whole video took (absent: not recorded,
    /// or the encode was not of the whole video). Trials known from an earlier run make a
    /// search short, so only the encode's time says how fast the machine is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_secs: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encode_secs: Option<f64>,
    /// What `--scenes` was set to: "off", "auto" or "always" (absent: not recorded). With
    /// `mode` it says how often "auto" ends in a CRF per scene.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scenes: Option<String>,
}

impl Record {
    pub fn new(info: &MediaInfo, output_bitrate: f64, target_vmaf: f64, preset: u8, mode: &str, crf: f64, svt_params: &str) -> Self {
        let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        Record {
            format: FORMAT,
            time,
            codec: info.codec.clone(),
            width: info.width,
            height: info.height,
            fps: info.fps,
            duration_s: info.picture_secs(),
            source_bitrate: info.video_bitrate,
            output_bitrate,
            target_vmaf,
            preset,
            mode: mode.to_string(),
            crf: Some(crf),
            svt_params: Some(svt_params.to_string()),
            search_secs: None,
            encode_secs: None,
            scenes: None,
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
    Ok(crate::files::append_line(&p, &serde_json::to_string(r)?, true)?)
}

/// All readable records; a missing or partly corrupt file just yields fewer.
pub fn load() -> Vec<Record> {
    path().map_or_else(Vec::new, |p| read(&p))
}

/// Read as bytes and line by line: one damaged line costs that line, not the whole history.
fn read(path: &Path) -> Vec<Record> {
    let Ok(bytes) = std::fs::read(path) else { return Vec::new() };
    bytes.split(|b| *b == b'\n').filter_map(|l| serde_json::from_slice::<Record>(l).ok()).filter(|r| r.format <= FORMAT).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_damaged_line_costs_only_that_line() {
        let info = MediaInfo { codec: "h264".into(), width: 640, height: 360, fps: 24.0, duration_s: 60.0, ..Default::default() };
        let line = |crf| serde_json::to_vec(&Record::new(&info, 1e6, 93.0, 5, "whole", crf, "tune=0")).unwrap();
        let mut file = line(30.0);
        file.extend_from_slice(b"\n{\"format\":1,\"codec\":\"h\xff\"}\nnot json\n\n");
        file.extend(line(31.0));
        file.push(b'\n');
        let path = std::env::temp_dir().join(format!("av1opt-history-{}.jsonl", std::process::id()));
        std::fs::write(&path, file).unwrap();
        let crfs: Vec<_> = read(&path).iter().map(|r| r.crf).collect();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(crfs, [Some(30.0), Some(31.0)]);
    }
}
