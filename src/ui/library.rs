//! Folder browsing, scans, encode history (the `.av1opt.json` reports) and playback previews.

use super::{App, now};
use crate::estimate::{Model, Verdict};
use crate::history;
use crate::scan::{self, FileResult, is_video};
use crate::search;
use crate::tools::path_arg;
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

fn base_dir(var: &str, fallback: &str) -> Option<PathBuf> {
    std::env::var_os(var).map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(fallback)))
}

pub fn settings_path() -> Option<PathBuf> {
    Some(base_dir("XDG_DATA_HOME", ".local/share")?.join("av1opt/ui.json"))
}

pub fn cache_dir() -> Option<PathBuf> {
    let d = base_dir("XDG_CACHE_HOME", ".cache")?.join("av1opt");
    std::fs::create_dir_all(&d).ok()?;
    Some(d)
}

/// Short playback previews live in the user's cache, never next to the source. Leftovers of
/// earlier sessions are removed at start.
pub fn preview_dir() -> Result<PathBuf> {
    let d = cache_dir().context("no HOME for the preview folder")?.join("previews");
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d)?;
    crate::fsguard::ensure_writable_location(&d, false)?;
    Ok(std::fs::canonicalize(&d)?)
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

pub fn state(app: &App) -> Value {
    let s = app.settings.lock().unwrap().clone();
    let model = Model::fit(&history::load(), 93.0);
    json!({
        "home": home(),
        "settings": s,
        "default_out_dir": s.out_dir.clone().unwrap_or_else(|| home().join("av1opt-out")),
        "history_encodes": model.samples,
        "ffmpeg": app.cfg.ffmpeg,
        "version": env!("CARGO_PKG_VERSION"),
    })
}

/// Lists sub-folders and video files of a folder (names and sizes only, never contents).
pub fn browse(path: Option<&str>, hidden: bool) -> Result<Value> {
    let dir = match path {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => home(),
    };
    let dir = std::fs::canonicalize(&dir).with_context(|| format!("cannot open {}", dir.display()))?;
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for e in std::fs::read_dir(&dir).with_context(|| format!("cannot list {}", dir.display()))?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') && !hidden {
            continue;
        }
        let Ok(meta) = std::fs::metadata(e.path()) else { continue };
        if meta.is_dir() {
            dirs.push(name);
        } else if meta.is_file() && is_video(&e.path()) {
            files.push(json!({ "name": name, "size": meta.len() }));
        }
    }
    dirs.sort_by_key(|d| d.to_lowercase());
    files.sort_by_key(|f| f["name"].as_str().unwrap_or("").to_lowercase());
    let mut places = vec![json!({ "name": "Home", "path": home() })];
    for base in ["/mnt", "/media"] {
        if let Ok(rd) = std::fs::read_dir(base) {
            let mut v: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
            v.sort();
            places.extend(v.into_iter().map(|p| json!({ "name": p.display().to_string(), "path": p })));
        }
    }
    Ok(json!({ "path": dir, "parent": dir.parent(), "dirs": dirs, "files": files, "places": places }))
}

pub struct Scan {
    id: u64,
    paths: Vec<PathBuf>,
    status: String,
    error: Option<String>,
    done: usize,
    total: usize,
    started: u64,
    model_samples: usize,
    results: Vec<FileResult>,
}

#[derive(Deserialize)]
struct ScanRequest {
    paths: Vec<PathBuf>,
    #[serde(default = "d_savings")]
    min_savings: f64,
    #[serde(default = "d_vmaf")]
    target_vmaf: f64,
}

fn d_savings() -> f64 {
    0.2
}
fn d_vmaf() -> f64 {
    93.0
}

pub fn start_scan(app: &Arc<App>, body: &Value) -> Result<Value> {
    let r: ScanRequest = serde_json::from_value(body.clone())?;
    if r.paths.is_empty() {
        bail!("add at least one folder");
    }
    let paths: Vec<PathBuf> = r
        .paths
        .iter()
        .map(|p| std::fs::canonicalize(p).with_context(|| format!("cannot open {}", p.display())))
        .collect::<Result<_>>()?;
    let model = Model::fit(&history::load(), r.target_vmaf);
    let id = {
        let mut scans = app.scans.lock().unwrap();
        let id = scans.iter().map(|s| s.id).max().unwrap_or(0) + 1;
        // Keep memory bounded: only the last few scans are kept.
        while scans.len() >= 5 {
            scans.remove(0);
        }
        scans.push(Scan {
            id,
            paths: paths.clone(),
            status: "running".into(),
            error: None,
            done: 0,
            total: 0,
            started: now(),
            model_samples: model.samples,
            results: Vec::new(),
        });
        id
    };
    {
        let mut s = app.settings.lock().unwrap();
        s.scan_paths = paths.clone();
    }
    app.save_settings();

    let app = app.clone();
    std::thread::spawn(move || {
        let last = std::sync::Mutex::new(std::time::Instant::now());
        let progress = |done: usize, total: usize| {
            {
                let mut scans = app.scans.lock().unwrap();
                if let Some(s) = scans.iter_mut().find(|s| s.id == id) {
                    s.done = done;
                    s.total = total;
                }
            }
            let mut l = last.lock().unwrap();
            if done == 0 || done == total || l.elapsed().as_millis() > 200 {
                *l = std::time::Instant::now();
                app.broadcast(json!({ "kind": "scan", "id": id, "done": done, "total": total }));
            }
        };
        let r = scan::collect(&paths, &app.cfg.ffprobe, app.cfg.jobs, r.min_savings, &model, &progress);
        {
            let mut scans = app.scans.lock().unwrap();
            if let Some(s) = scans.iter_mut().find(|s| s.id == id) {
                match r {
                    Ok((results, _)) => {
                        for f in &results {
                            app.allow(&f.path);
                        }
                        s.results = results;
                        s.status = "done".into();
                    }
                    Err(e) => {
                        s.status = "failed".into();
                        s.error = Some(format!("{e:#}"));
                    }
                }
            }
        }
        app.broadcast(json!({ "kind": "scan_done", "id": id }));
    });
    Ok(json!({ "id": id }))
}

pub fn scan_result(app: &App, id: u64) -> Result<Value> {
    let scans = app.scans.lock().unwrap();
    let s = scans.iter().find(|s| s.id == id).ok_or_else(|| anyhow!("not found: scan {id}"))?;
    let mut totals = json!({ "files": 0, "bytes": 0.0, "candidates": 0, "candidate_bytes": 0.0, "saved_likely": 0.0, "saved_low": 0.0, "saved_high": 0.0 });
    let (mut files, mut bytes, mut cands, mut cbytes, mut lk, mut lo, mut hi) = (0, 0.0, 0, 0.0, 0.0, 0.0, 0.0);
    let rows: Vec<Value> = s
        .results
        .iter()
        .map(|r| {
            files += 1;
            bytes += r.size as f64;
            if r.verdict == Verdict::Candidate {
                cands += 1;
                cbytes += r.size as f64;
                lk += r.size as f64 - r.estimate.likely;
                lo += r.size as f64 - r.estimate.high;
                hi += r.size as f64 - r.estimate.low;
            }
            let i = r.info.as_ref();
            json!({
                "path": r.path,
                "rel": r.path.strip_prefix(&r.root).unwrap_or(&r.path),
                "root": r.root,
                "size": r.size,
                "verdict": r.verdict,
                "verdict_label": r.verdict.label(),
                "estimate": r.estimate,
                "codec": i.map(|i| i.codec.as_str()),
                "width": i.map(|i| i.width),
                "height": i.map(|i| i.height),
                "fps": i.map(|i| i.fps),
                "duration_s": i.map(|i| i.duration_s),
                "video_mbps": i.map(|i| i.video_bitrate / 1e6),
                "hdr": i.map(|i| i.hdr),
                "error": r.error,
            })
        })
        .collect();
    if files > 0 {
        totals = json!({ "files": files, "bytes": bytes, "candidates": cands, "candidate_bytes": cbytes, "saved_likely": lk, "saved_low": lo, "saved_high": hi });
    }
    Ok(json!({
        "id": s.id, "paths": s.paths, "status": s.status, "error": s.error, "done": s.done, "total": s.total,
        "started": s.started, "model_samples": s.model_samples, "totals": totals, "rows": rows,
    }))
}

/// Report paths are relative to wherever `encode` ran in older versions; find the file
/// relative to the report's folder or one of its parents.
fn resolve(report: &Path, p: &str, same_dir: bool) -> PathBuf {
    let p = PathBuf::from(p);
    if p.is_absolute() {
        return p;
    }
    let dir = report.parent().unwrap_or(Path::new("/"));
    if same_dir && let Some(name) = p.file_name() {
        return dir.join(name);
    }
    dir.ancestors().map(|a| a.join(&p)).find(|c| c.exists()).unwrap_or_else(|| dir.join(&p))
}

/// Reports are plain JSON on disk; only let them unlock video files, never arbitrary ones.
fn allow_video(app: &App, p: &Path) {
    if is_video(p) && p.is_file() {
        app.allow(p);
    }
}

fn report_files(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for d in dirs {
        let walk = walkdir::WalkDir::new(d)
            .max_depth(8)
            .into_iter()
            .filter_entry(|e| !e.file_name().to_string_lossy().starts_with(".av1opt-"))
            .flatten();
        for e in walk {
            if e.file_type().is_file() && e.file_name().to_string_lossy().ends_with(".av1opt.json") {
                out.push(e.into_path());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

pub fn history(app: &App) -> Value {
    let dirs = app.settings.lock().unwrap().history_dirs.clone();
    let mut rows: Vec<Value> = report_files(&dirs)
        .into_iter()
        .filter_map(|p| {
            let r: Value = serde_json::from_slice(&std::fs::read(&p).ok()?).ok()?;
            let input = resolve(&p, r["input"].as_str()?, false);
            let output = resolve(&p, r["output"].as_str()?, true);
            let mtime = std::fs::metadata(&p).ok()?.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs();
            allow_video(app, &input);
            allow_video(app, &output);
            Some(json!({
                "report": p,
                "input": input,
                "output": output,
                "input_exists": input.exists(),
                "output_exists": output.exists(),
                "source_bytes": r["source_bytes"],
                "output_bytes": r["output_bytes"],
                "savings": r["savings"],
                "mode": r["mode"],
                "crf": r["crf"],
                "preset": r["preset"],
                "target_vmaf": r["target_vmaf"],
                "mean_vmaf": r["mean_vmaf"],
                "verify_vmaf": r["verify_vmaf"],
                "chunks": r["chunks"].as_array().map_or(0, |c| c.len()),
                "encode_secs": r["encode_secs"],
                "search_secs": r["search_secs"],
                "time": mtime,
            }))
        })
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r["time"].as_u64().unwrap_or(0)));
    json!({ "dirs": dirs, "reports": rows })
}

#[derive(Deserialize)]
struct DirRequest {
    path: PathBuf,
}

pub fn add_history_dir(app: &App, body: &Value) -> Result<Value> {
    let r: DirRequest = serde_json::from_value(body.clone())?;
    let p = std::fs::canonicalize(&r.path).with_context(|| format!("cannot open {}", r.path.display()))?;
    if !p.is_dir() {
        bail!("{} is not a folder", p.display());
    }
    {
        let mut s = app.settings.lock().unwrap();
        if !s.history_dirs.contains(&p) {
            s.history_dirs.push(p);
        }
    }
    app.save_settings();
    Ok(json!({ "ok": true }))
}

pub fn remove_history_dir(app: &App, body: &Value) -> Result<Value> {
    let r: DirRequest = serde_json::from_value(body.clone())?;
    app.settings.lock().unwrap().history_dirs.retain(|d| d != &r.path);
    app.save_settings();
    Ok(json!({ "ok": true }))
}

/// A report the user can reach: inside a history folder or written by a queued job.
fn known_report(app: &App, p: &Path) -> Result<PathBuf> {
    let c = std::fs::canonicalize(p).with_context(|| format!("not found: {}", p.display()))?;
    if !c.to_string_lossy().ends_with(".av1opt.json") {
        bail!("not a report");
    }
    let dirs = app.settings.lock().unwrap().history_dirs.clone();
    let in_history = dirs.iter().any(|d| std::fs::canonicalize(d).is_ok_and(|d| c.starts_with(d)));
    if !in_history {
        bail!("report is outside the history folders");
    }
    Ok(c)
}

fn media_info(app: &App, p: &Path) -> Value {
    let size = std::fs::metadata(p).map_or(0, |m| m.len());
    match crate::probe::probe(&app.cfg.ffprobe, p, size) {
        Ok(i) => json!({
            "codec": i.codec, "width": i.width, "height": i.height, "fps": i.fps, "duration_s": i.duration_s,
            "start_time": i.start_time, "video_start": i.video_start, "hdr": i.hdr, "size": size,
            "container": p.extension().map(|e| e.to_string_lossy().to_lowercase()),
        }),
        Err(e) => json!({ "error": format!("{e:#}"), "size": size }),
    }
}

pub fn report(app: &App, path: &str) -> Result<Value> {
    let p = known_report(app, Path::new(path))?;
    let mut r: Value = serde_json::from_slice(&std::fs::read(&p)?).context("unreadable report")?;
    let input = resolve(&p, r["input"].as_str().unwrap_or(""), false);
    let output = resolve(&p, r["output"].as_str().unwrap_or(""), true);
    allow_video(app, &input);
    allow_video(app, &output);
    r["report_path"] = json!(p);
    r["input_path"] = json!(input);
    r["output_path"] = json!(output);
    r["input_info"] = if input.exists() { media_info(app, &input) } else { json!({ "error": "source file is gone" }) };
    r["output_info"] = if output.exists() { media_info(app, &output) } else { json!({ "error": "output file is gone" }) };
    Ok(r)
}

#[derive(Deserialize)]
struct PreviewRequest {
    path: PathBuf,
    /// "transcode": near-lossless H.264 clip (for sources the browser can't decode);
    /// "remux": whole file copied into MP4 (for AV1 in containers the browser won't open).
    kind: String,
    #[serde(default)]
    start: f64,
    #[serde(default = "d_secs")]
    secs: f64,
}

fn d_secs() -> f64 {
    20.0
}

/// Builds (or reuses) a playable stand-in for a file the browser can't play directly.
pub fn preview(app: &App, body: &Value) -> Result<Value> {
    let r: PreviewRequest = serde_json::from_value(body.clone())?;
    let src = app.is_allowed(&r.path).ok_or_else(|| anyhow!("not a file you picked"))?;
    let meta = std::fs::metadata(&src)?;
    let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
    let start = r.start.max(0.0);
    let secs = r.secs.clamp(2.0, 120.0);
    let key = format!("{}|{}|{}|{mtime}|{start:.3}|{secs:.3}", r.kind, src.display(), meta.len());
    let out = app.preview_dir.join(format!("{:016x}.mp4", crate::scenes::fnv1a(&key)));
    let url = |p: &Path| format!("/media?path={}", percent_encode(&p.to_string_lossy()));
    // One preview at a time: they are CPU-heavy and a second request for the same clip waits
    // for the first instead of encoding it twice.
    let _guard = app.preview_lock.lock().unwrap_or_else(|e| e.into_inner());
    if out.exists() {
        return Ok(json!({ "url": url(&out), "start": start, "secs": secs, "kind": r.kind }));
    }
    let info = crate::probe::probe(&app.cfg.ffprobe, &src, meta.len())?;
    let part = out.with_extension("part.mp4");
    let mut a: Vec<String> = vec!["-y".into(), "-v".into(), "error".into()];
    match r.kind.as_str() {
        "transcode" => {
            a.extend(["-ss".into(), format!("{start:.3}"), "-i".into(), path_arg(&src), "-t".into(), format!("{secs:.3}")]);
            a.extend(["-map".into(), format!("0:{}", info.video_index), "-map".into(), "0:a:0?".into()]);
            // Same geometry fixes as the encode (deinterlace, even crop), so frames line up.
            let mut vf = search::source_filters(&info);
            vf.push("format=yuv420p".into());
            a.extend(["-vf".into(), vf.join(",")]);
            a.extend(["-c:v", "libx264", "-preset", "veryfast", "-crf", "12", "-c:a", "aac", "-b:a", "160k"].map(String::from));
        }
        "remux" => {
            a.extend(["-i".into(), path_arg(&src), "-map".into(), format!("0:{}", info.video_index), "-map".into(), "0:a:0?".into()]);
            a.extend(["-c:v", "copy", "-c:a", "aac", "-b:a", "160k"].map(String::from));
        }
        k => bail!("unknown preview kind {k}"),
    }
    a.extend(["-movflags".into(), "+faststart".into(), "-f".into(), "mp4".into(), path_arg(&part)]);
    trim_previews(&app.preview_dir, 12);
    let tools = app.tools(1);
    tools.run(&a).context("preview encode failed")?;
    std::fs::rename(&part, &out)?;
    Ok(json!({ "url": url(&out), "start": start, "secs": secs, "kind": r.kind }))
}

/// Keeps the preview folder to the `keep` most recent files.
fn trim_previews(dir: &Path, keep: usize) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(u64, PathBuf)> = rd
        .flatten()
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs(), e.path())))
        .collect();
    files.sort();
    let n = files.len();
    for (_, p) in files.into_iter().take(n.saturating_sub(keep)) {
        let _ = std::fs::remove_file(p);
    }
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}
