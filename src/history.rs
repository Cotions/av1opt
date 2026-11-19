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

/// What `encode_secs` covers. Raise it whenever that changes, so that the time estimates stop
/// taking older lines for the same thing. 1: the encode, the check of the whole video and the
/// repairs it led to. Lines without a number timed the encode alone.
pub const TIMING_VERSION: u32 = 1;

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
    /// The av1opt that made the line (absent in lines from before it was recorded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// [`TIMING_VERSION`] of the av1opt that made the line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing_version: Option<u32>,
    /// Seconds of `encode_secs` that went to the check of the whole video and to the repairs
    /// it led to, which are timed together (absent: no such check was made).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_secs: Option<f64>,
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
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
            timing_version: Some(TIMING_VERSION),
            check_secs: None,
        }
    }
}

fn path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("AV1OPT_HISTORY") {
        return Some(PathBuf::from(p));
    }
    let base = crate::files::xdg_folder("XDG_DATA_HOME", ".local/share")?;
    Some(base.join("av1opt/history.jsonl"))
}

pub fn append(r: &Record) -> Result<()> {
    let p = path().context("no HOME for the encode history")?;
    if let Some(dir) = p.parent() {
        // A file named by the user may be anywhere; the usual place is av1opt's own.
        if std::env::var_os("AV1OPT_HISTORY").is_some() { std::fs::create_dir_all(dir)? } else { crate::files::private_folder(dir)? }
    }
    Ok(crate::files::append_line(&p, &serde_json::to_string(r)?, true)?)
}

/// All readable records; a missing or partly corrupt file just yields fewer.
pub fn load() -> Vec<Record> {
    let Some(path) = path() else { return Vec::new() };
    if path.is_dir() {
        // Said once: a run reads the history more than once.
        static TOLD: std::sync::Once = std::sync::Once::new();
        TOLD.call_once(|| eprintln!("warning: the encode history {} is a folder, so no past encodes are used; AV1OPT_HISTORY should name a file", path.display()));
    }
    read(&path)
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

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("av1opt-history-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A line with what every record has, plus `more`.
    fn line(more: serde_json::Value) -> String {
        let mut v = serde_json::json!({ "time": 1, "codec": "h264", "width": 640, "height": 360, "fps": 24.0, "duration_s": 60.0,
            "source_bitrate": 1e6, "output_bitrate": 5e5, "target_vmaf": 93.0, "preset": 5, "mode": "whole" });
        v.as_object_mut().unwrap().extend(more.as_object().unwrap().clone());
        v.to_string()
    }

    /// What `read` makes of a file with these bytes.
    fn read_bytes(name: &str, bytes: &[u8]) -> Vec<Record> {
        let dir = scratch(name);
        let path = dir.join("history.jsonl");
        std::fs::write(&path, bytes).unwrap();
        let records = read(&path);
        std::fs::remove_dir_all(&dir).unwrap();
        records
    }

    #[test]
    fn a_new_record_holds_the_length_of_the_picture_and_no_times_yet() {
        // The sound runs on for a minute after the picture ends.
        let info = MediaInfo { codec: "hevc".into(), width: 1920, height: 1080, fps: 25.0, duration_s: 100.0, video_s: 40.0, video_bitrate: 4e6, ..Default::default() };
        let r = Record::new(&info, 2e6, 93.0, 5, "scenes", 31.5, "tune=0");
        assert_eq!((r.format, r.codec.as_str(), r.width, r.height, r.fps), (FORMAT, "hevc", 1920, 1080, 25.0));
        assert_eq!((r.duration_s, r.source_bitrate, r.output_bitrate, r.target_vmaf), (40.0, 4e6, 2e6, 93.0));
        assert_eq!((r.preset, r.mode.as_str(), r.crf, r.svt_params.as_deref()), (5, "scenes", Some(31.5), Some("tune=0")));
        assert_eq!((r.search_secs, r.encode_secs, r.scenes, r.check_secs), (None, None, None, None));
        assert_eq!((r.version.as_deref(), r.timing_version), (Some(env!("CARGO_PKG_VERSION")), Some(TIMING_VERSION)));
        assert!(r.time > 1_700_000_000);
    }

    #[test]
    fn lines_of_a_newer_version_are_left_out_and_old_ones_count_as_the_first() {
        let file = [
            line(serde_json::json!({ "format": FORMAT + 1, "preset": 1 })),
            line(serde_json::json!({ "format": FORMAT, "preset": 2 })),
            line(serde_json::json!({ "preset": 3 })),
        ]
        .join("\n");
        let got = read_bytes("versions", file.as_bytes());
        assert_eq!(got.iter().map(|r| (r.preset, r.format)).collect::<Vec<_>>(), [(2, FORMAT), (3, 1)]);
    }

    #[test]
    fn what_a_line_does_not_say_stays_unknown_and_what_it_adds_is_passed_over() {
        let got = read_bytes("fields", line(serde_json::json!({ "from_the_future": [1, 2] })).as_bytes());
        assert_eq!(got.len(), 1);
        let r = &got[0];
        assert_eq!((r.crf, r.svt_params.as_deref(), r.search_secs, r.encode_secs, r.scenes.as_deref()), (None, None, None, None, None));
    }

    #[test]
    fn a_line_from_before_timings_had_a_version_still_loads_and_has_none() {
        let old = r#"{"format":1,"time":1763000000,"codec":"h264","width":1920,"height":1080,"fps":30.0,"duration_s":900.0,"source_bitrate":5000000.0,"output_bitrate":1500000.0,"target_vmaf":93.0,"preset":6,"mode":"whole","crf":35.0,"svt_params":"tune=0","search_secs":120.0,"encode_secs":900.0,"scenes":"off"}"#;
        let got = read_bytes("untimed", old.as_bytes());
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].version.as_deref(), got[0].timing_version, got[0].check_secs, got[0].encode_secs), (None, None, None, Some(900.0)));
    }

    #[test]
    fn times_not_known_are_not_written_and_known_ones_come_back() {
        let info = MediaInfo { codec: "h264".into(), width: 640, height: 360, fps: 24.0, duration_s: 60.0, ..Default::default() };
        let mut r = Record::new(&info, 1e6, 93.0, 5, "whole", 30.0, "tune=0");
        let text = serde_json::to_string(&r).unwrap();
        for absent in ["search_secs", "encode_secs", "scenes", "check_secs"] {
            assert!(!text.contains(absent), "{text}");
        }
        (r.search_secs, r.encode_secs, r.scenes, r.check_secs) = (Some(12.5), Some(600.0), Some("auto".into()), Some(200.0));
        let got = read_bytes("times", serde_json::to_string(&r).unwrap().as_bytes());
        assert_eq!((got[0].search_secs, got[0].encode_secs, got[0].scenes.as_deref(), got[0].check_secs), (Some(12.5), Some(600.0), Some("auto"), Some(200.0)));
        assert_eq!(got[0].timing_version, Some(TIMING_VERSION));
    }

    #[test]
    fn a_line_cut_short_by_a_crash_costs_that_line_and_others_may_end_in_any_way() {
        let whole = line(serde_json::json!({ "preset": 7 }));
        let file = format!("{whole}\r\n{}", &whole[..whole.len() / 2]);
        assert_eq!(read_bytes("cut", file.as_bytes()).iter().map(|r| r.preset).collect::<Vec<_>>(), [7]);
        assert_eq!(read_bytes("last", whole.as_bytes()).len(), 1);
    }

    #[test]
    fn numbers_out_of_range_cost_their_line() {
        for more in [serde_json::json!({ "preset": 300 }), serde_json::json!({ "width": -1 }), serde_json::json!({ "fps": "fast" }), serde_json::json!({ "format": -1 })] {
            assert!(read_bytes("range", line(more.clone()).as_bytes()).is_empty(), "{more}");
        }
    }

    #[test]
    fn no_file_or_an_empty_one_is_an_empty_history() {
        let dir = scratch("none");
        assert!(read(&dir.join("not-there.jsonl")).is_empty());
        // A folder where the file should be.
        assert!(read(&dir).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(read_bytes("empty", b"").is_empty());
        assert!(read_bytes("blank", b"\n\n\n").is_empty());
    }
}
