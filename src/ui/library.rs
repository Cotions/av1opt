//! Folder browsing, scans, encode history (the `.av1opt.json` reports) and playback previews.

use super::turns::Turns;
use super::{App, now, text_path};
use crate::estimate::{Model, TimeModel, Verdict};
use crate::history;
use crate::ledger::{Entry, Ledger};
use crate::replace::{self, Check};
use crate::scan::{self, FileResult, is_video};
use crate::search;
use crate::tools::path_arg;
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

fn base_dir(var: &str, fallback: &str) -> Option<PathBuf> {
    std::env::var_os(var).map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(fallback)))
}

pub fn settings_path() -> Option<PathBuf> {
    Some(base_dir("XDG_DATA_HOME", ".local/share")?.join("av1opt/ui.json"))
}

pub fn cache_dir() -> Option<PathBuf> {
    let d = base_dir("XDG_CACHE_HOME", ".cache")?.join("av1opt");
    crate::files::private_folder(&d).ok()?;
    Some(d)
}

/// Closes av1opt's data and cache folders to other users, with what is in them: folder lists,
/// the queue and previews of someone's videos are nobody else's business. Versions before this
/// one left all of that as open as the umask made it.
pub fn close_private_folders() {
    let data = settings_path().and_then(|p| p.parent().map(Path::to_path_buf));
    for folder in data.into_iter().chain(cache_dir()) {
        if let Err(e) = crate::files::private_folder(&folder) {
            eprintln!("warning: cannot close {} to other users: {e}", folder.display());
        }
        crate::files::tighten_below(&folder);
    }
}

/// Short playback previews live in the user's cache, never next to the source, in a folder
/// named after this process: servers with data folders of their own are not kept apart by
/// their lock, and may share the cache. What servers that are gone left there is removed.
pub fn preview_dir() -> Result<PathBuf> {
    let all = cache_dir().context("no HOME for the preview folder")?.join("previews");
    crate::files::private_folder(&all)?;
    let own = all.join(std::process::id().to_string());
    remove_left_previews(&all, &own);
    crate::files::private_folder(&own)?;
    crate::fsguard::ensure_writable_location(&own, false)?;
    Ok(std::fs::canonicalize(&own)?)
}

/// Clears `all` of everything but the folders of other running processes. `own` may be there
/// from an earlier process of the same number. Plain files are from versions that kept their
/// previews directly in `all`.
fn remove_left_previews(all: &Path, own: &Path) {
    for entry in std::fs::read_dir(all).into_iter().flatten().flatten() {
        let path = entry.path();
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            let _ = std::fs::remove_file(&path);
        } else if path == own || !entry.file_name().to_str().and_then(|n| n.parse().ok()).is_some_and(process_is_running) {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

fn process_is_running(id: i32) -> bool {
    // SAFETY: signal 0 is not sent, the call only asks whether the process could be reached.
    id > 0 && (unsafe { libc::kill(id, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
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
        "tools": tools_state(app),
        "version": env!("CARGO_PKG_VERSION"),
        "api": super::API_VERSION,
        "meta_presets": crate::metadata::PRESETS,
        "tuning": super::bench::tuning_json(&crate::tuning::Tuning::load().unwrap_or_default()),
        "default_svt_params": crate::search::DEFAULT_PARAMS,
        "notices": *app.notices.lock().unwrap(),
    })
}

#[derive(Deserialize)]
struct MetaPreviewRequest {
    input: PathBuf,
    #[serde(default)]
    options: Option<super::jobs::JobOptions>,
}

/// The tags an encode of `input` would write with these options, next to the source's own.
pub fn meta_preview(app: &App, body: &Value) -> Result<Value> {
    let r: MetaPreviewRequest = serde_json::from_value(body.clone())?;
    let input = std::fs::canonicalize(&r.input).with_context(|| format!("{} does not exist", r.input.display())).and_then(text_path)?;
    if !input.is_file() || !is_video(&input) {
        bail!("not a video file");
    }
    let options = r.options.unwrap_or_else(|| serde_json::from_value(json!({})).expect("defaults"));
    let meta = options.meta()?;
    let size = std::fs::metadata(&input)?.len();
    let info = crate::probe::probe(&app.ffprobe(), &input, size)?;
    let resolved = crate::metadata::resolve(&meta, &input, &info.tags);
    Ok(json!({
        "input": input,
        "source": info.tags,
        "keep": meta.keep,
        "tags": resolved.tags,
        "info_json": resolved.info_json,
    }))
}

fn tools_state(app: &App) -> Value {
    let caps = app.capabilities();
    json!({
        "ffmpeg": app.ffmpeg(),
        "ffprobe": app.ffprobe(),
        "default_ffmpeg": app.cfg.ffmpeg,
        "default_ffprobe": app.cfg.ffprobe,
        "problem": caps.problem(&app.ffmpeg(), &app.ffprobe()),
        "capabilities": caps,
    })
}

/// Whether a file is the user's or the system's, and writable by nobody else.
fn mine_alone(m: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: getuid has no failure mode and touches no memory.
    let uid = unsafe { libc::getuid() };
    // (Where each user has a group to themselves, their files are made writable by it.)
    let group_ok = m.mode() & 0o020 == 0 || (m.uid() == uid && group_is_mine_alone(m.gid(), uid));
    (m.uid() == 0 || m.uid() == uid) && m.mode() & 0o002 == 0 && group_ok
}

/// Whether the system's own lists name nobody but this user as a member of group `gid`.
fn group_is_mine_alone(gid: u32, uid: u32) -> bool {
    let (Ok(passwd), Ok(group)) = (std::fs::read_to_string("/etc/passwd"), std::fs::read_to_string("/etc/group")) else { return false };
    alone_in_group(&passwd, &group, gid, uid)
}

/// The same from the text of `/etc/passwd` and `/etc/group`.
fn alone_in_group(passwd: &str, group: &str, gid: u32, uid: u32) -> bool {
    let mut me = None;
    for line in passwd.lines() {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() < 4 {
            continue;
        }
        if f[2].parse() == Ok(uid) {
            me = Some(f[0]);
        } else if f[3].parse() == Ok(gid) {
            return false;
        }
    }
    // A group these files do not list (kept in LDAP, say) may have any members.
    let mut lines = group.lines().map(|l| l.split(':').collect::<Vec<&str>>()).filter(|f| f.len() >= 4 && f[2].parse() == Ok(gid)).peekable();
    lines.peek().is_some() && lines.all(|f| f[3].split(',').map(str::trim).filter(|m| !m.is_empty()).all(|m| Some(m) == me))
}

/// The file a program's path leads to in the end, where nobody but the user or the system
/// could put another in its place: the file is theirs, and so is every folder on the way to
/// it. (The path as given could go through a link in a folder of someone else's, who then
/// points it elsewhere after the check; what is kept and run is the path the links led to.)
pub(super) fn safe_program(p: &Path) -> std::result::Result<PathBuf, &'static str> {
    use std::os::unix::fs::MetadataExt;
    const OTHERS: &str = "belongs to another user or can be changed by others";
    let real = std::fs::canonicalize(p).map_err(|_| "is not there")?;
    let m = std::fs::metadata(&real).map_err(|_| "is not there")?;
    if !m.is_file() {
        return Err("is not a program");
    }
    if !mine_alone(&m) {
        return Err(OTHERS);
    }
    for dir in real.ancestors().skip(1) {
        let m = std::fs::metadata(dir).map_err(|_| "is not there")?;
        // A folder the system set up for a group (/usr/local on some systems) or one with
        // the sticky bit (/tmp), where a name can only be changed by who owns it, is the
        // system's doing; a folder of the user's own has to be theirs alone.
        // SAFETY: getuid has no failure mode and touches no memory.
        let uid = unsafe { libc::getuid() };
        let ok = match m.uid() {
            0 => m.mode() & 0o002 == 0 || m.mode() & 0o1000 != 0,
            u if u == uid => mine_alone(&m),
            _ => false,
        };
        if !ok {
            return Err(OTHERS);
        }
    }
    Ok(real)
}

/// Whether a program's file name starts with the name of the tool it stands for.
fn named_like(p: &Path, tool: &str) -> bool {
    p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.to_ascii_lowercase().starts_with(tool))
}

/// The file to run for a path chosen as `tool`: safe, and itself named like the tool. A link
/// called `ffmpeg` can lead to a program that goes by what it was called (a launcher such as
/// `/usr/bin/snap`), and that program started under its own name is something else.
pub(super) fn chosen_program(tool: &str, p: &Path) -> std::result::Result<PathBuf, String> {
    let real = safe_program(p).map_err(|why| format!("{} {why}", p.display()))?;
    if !named_like(&real, tool) {
        return Err(format!("{} is a link that leads to {}, a program with another name", p.display(), real.display()));
    }
    Ok(real)
}

/// Sets the ffmpeg and ffprobe programs (empty = back to the command-line default) and
/// reports what they can do.
pub fn set_tools(app: &App, body: &Value) -> Result<Value> {
    let pick = |k: &str| -> Result<Option<String>> {
        match body.get(k).and_then(Value::as_str).map(str::trim) {
            None | Some("") => Ok(None),
            Some(p) if p.contains('/') && !Path::new(p).is_absolute() => bail!("{k}: use an absolute path or a program name"),
            // This page starts what is named here. Whoever can reach the page can already
            // encode and trash; running any program of the machine is not part of that. So:
            // a program named like the tool, and one nobody else could have put there.
            Some(p) if !named_like(Path::new(p), k) => bail!("{k}: the program's name has to start with {k}"),
            Some(p) if Path::new(p).is_absolute() => match chosen_program(k, Path::new(p)) {
                Ok(real) => Ok(Some(real.to_string_lossy().into_owned())),
                Err(why) => bail!("{k}: {why}"),
            },
            Some(p) => Ok(Some(p.to_string())),
        }
    };
    let (ffmpeg, ffprobe) = (pick("ffmpeg")?, pick("ffprobe")?);
    {
        let mut s = app.settings.lock().unwrap();
        s.ffmpeg = ffmpeg;
        s.ffprobe = ffprobe;
    }
    app.save_settings();
    app.recheck_tools();
    super::jobs::kick(app);
    app.broadcast(json!({ "kind": "jobs" }));
    Ok(tools_state(app))
}

/// Lists sub-folders and video files of a folder (names and sizes only, never contents).
pub fn browse(path: Option<&str>, hidden: bool) -> Result<Value> {
    let dir = match path {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => home(),
    };
    let dir = std::fs::canonicalize(&dir).with_context(|| format!("cannot open {}", dir.display())).and_then(text_path)?;
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let mut not_listed = 0;
    for e in std::fs::read_dir(&dir).with_context(|| format!("cannot list {}", dir.display()))?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') && !hidden {
            continue;
        }
        let Ok(meta) = std::fs::metadata(e.path()) else { continue };
        // Shown as text it would be another name, which picking it would then ask for.
        if (meta.is_dir() || meta.is_file() && is_video(&e.path())) && e.file_name().to_str().is_none() {
            not_listed += 1;
            continue;
        }
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
            let mut v: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir() && p.to_str().is_some()).collect();
            v.sort();
            places.extend(v.into_iter().map(|p| json!({ "name": p.display().to_string(), "path": p })));
        }
    }
    Ok(json!({ "path": dir, "parent": dir.parent(), "dirs": dirs, "files": files, "not_listed": not_listed, "places": places }))
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
    /// How long encodes take here, for the preset the scan was asked about.
    time: TimeModel,
    results: Vec<FileResult>,
    /// Videos found whose paths are not text, which the page cannot show or name.
    not_listed: usize,
    /// Folders that could not be read: what is in them is missing from `results`.
    unreadable: Vec<PathBuf>,
}

#[derive(Deserialize)]
struct ScanRequest {
    paths: Vec<PathBuf>,
    #[serde(default = "d_savings")]
    min_savings: f64,
    #[serde(default = "d_vmaf")]
    target_vmaf: f64,
    /// Preset the encode time estimates are for.
    #[serde(default = "d_preset")]
    preset: u8,
}

fn d_preset() -> u8 {
    crate::search::DEFAULT_PRESET
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
        .map(|p| std::fs::canonicalize(p).with_context(|| format!("cannot open {}", p.display())).and_then(text_path))
        .collect::<Result<_>>()?;
    if r.preset > 13 {
        bail!("preset must be 0-13");
    }
    let past = history::load();
    let model = Model::fit(&past, r.target_vmaf);
    let time = TimeModel::fit(&past, r.preset);
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
            time,
            results: Vec::new(),
            not_listed: 0,
            unreadable: Vec::new(),
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
        let r = scan::collect(&paths, &app.ffprobe(), app.cfg.jobs, r.min_savings, &model, &progress);
        {
            let mut scans = app.scans.lock().unwrap();
            if let Some(s) = scans.iter_mut().find(|s| s.id == id) {
                match r {
                    Ok((mut results, stats)) => {
                        s.not_listed = scan::drop_unnamed(&mut results);
                        s.unreadable = stats.unreadable;
                        for f in &results {
                            app.allow_video(&f.path);
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

/// The candidates of a scan that save the most in `hours` of encoding, best value first.
pub fn scan_pick(app: &App, id: u64, body: &Value) -> Result<Value> {
    let hours = body["hours"].as_f64().filter(|h| *h > 0.0 && *h <= 10_000.0).context("hours: a number above 0")?;
    let scans = app.scans.lock().unwrap();
    let s = scans.iter().find(|s| s.id == id).ok_or_else(|| anyhow!("not found: scan {id}"))?;
    let picked = scan::pick_for_time(&s.results, &s.time, hours * 3600.0);
    let secs: f64 = picked.iter().filter_map(|r| r.info.as_ref()).map(|i| s.time.secs(i)).sum();
    let saved: f64 = picked.iter().map(|r| r.size as f64 - r.estimate.likely).sum();
    Ok(json!({ "paths": picked.iter().map(|r| &r.path).collect::<Vec<_>>(), "secs": secs, "saved": saved }))
}

pub fn scan_result(app: &App, id: u64) -> Result<Value> {
    let scans = app.scans.lock().unwrap();
    let s = scans.iter().find(|s| s.id == id).ok_or_else(|| anyhow!("not found: scan {id}"))?;
    let mut totals = json!({ "files": 0, "bytes": 0.0, "candidates": 0, "candidate_bytes": 0.0, "saved_likely": 0.0, "saved_low": 0.0, "saved_high": 0.0 });
    let (mut files, mut bytes, mut cands, mut cbytes, mut lk, mut lo, mut hi) = (0, 0.0, 0, 0.0, 0.0, 0.0, 0.0);
    let mut encode_secs = 0.0;
    let rows: Vec<Value> = s
        .results
        .iter()
        .map(|r| {
            files += 1;
            bytes += r.size as f64;
            let est_secs = r.info.as_ref().map(|i| s.time.secs(i)).filter(|t| *t > 0.0);
            if r.verdict == Verdict::Candidate {
                cands += 1;
                encode_secs += est_secs.unwrap_or(0.0);
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
                "est_secs": est_secs,
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
        totals = json!({ "files": files, "bytes": bytes, "candidates": cands, "candidate_bytes": cbytes, "saved_likely": lk, "saved_low": lo, "saved_high": hi, "encode_secs": encode_secs });
    }
    Ok(json!({
        "id": s.id, "paths": s.paths, "status": s.status, "error": s.error, "done": s.done, "total": s.total,
        "unreadable_folders": s.unreadable.len(),
        "unreadable_paths": s.unreadable.iter().take(scan::UNREADABLE_SHOWN).map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
        "started": s.started, "model_samples": s.model_samples, "time_samples": s.time.samples, "scenes_factor": s.time.scenes, "scene_samples": s.time.scene_samples, "auto_factor": s.time.auto_factor(), "auto_share": s.time.auto_share, "auto_samples": s.time.auto_samples, "preset": s.time.preset, "totals": totals, "rows": rows, "not_listed": s.not_listed,
    }))
}

/// A path from a report, to show. Reports of older versions hold paths relative to where
/// `encode` ran. Nothing is looked up: what a report names is not touched for being named.
fn shown_path(report: &Path, p: &str, same_dir: bool) -> PathBuf {
    let p = PathBuf::from(p);
    if p.is_absolute() {
        return p;
    }
    let dir = report.parent().unwrap_or(Path::new("/"));
    match p.file_name() {
        Some(name) if same_dir => dir.join(name),
        _ => dir.join(p),
    }
}

/// What `replace` would do with a report, for the history table and the compare view. A report
/// of an output not on record is not looked into: its paths are only somebody's claim.
fn replace_state(ledger: &Ledger, report: &Path, on_record: bool) -> Value {
    if !on_record {
        return json!({ "state": "not_recorded", "why": ledger.refusal() });
    }
    let no = |why: String| json!({ "state": "no", "why": why });
    match replace::check_for_list(report) {
        Check::Ready(p) => match ledger.check_ready(&p, false) {
            Ok(()) => json!({ "state": "ready", "dest": p.dest, "copy": p.copy, "zfs": p.zfs, "removed": p.removed }),
            Err(why) => no(why),
        },
        Check::Replaced(d) => match ledger.check_replaced(&d) {
            Ok(()) => json!({ "state": "replaced", "trash": d.trash, "from": d.from, "can_undo": d.can_undo, "zfs": d.zfs }),
            Err(why) => no(why),
        },
        Check::No(why) => no(why),
    }
}

/// The file to show as the source of an output on record: the original, or its copy in the
/// trash once the output has taken its place.
fn source_on_record(entry: &Entry) -> &Path {
    entry.replaced.as_ref().map_or(&entry.source, |m| &m.trash)
}

/// A report's contents, if it is a plain file of a sensible size.
fn read_report_file(p: &Path) -> Result<Value> {
    let bytes = crate::files::read_regular(p, replace::REPORT_LIMIT).with_context(|| format!("cannot read the report {}", p.display()))?;
    serde_json::from_slice(&bytes).context("unreadable report")
}

pub fn history(app: &App) -> Value {
    let dirs = app.settings.lock().unwrap().history_dirs.clone();
    let ledger = app.ledger.current();
    let mut rows: Vec<Value> = replace::report_files(&dirs)
        .into_iter()
        .filter_map(|p| {
            p.to_str()?;
            let r = read_report_file(&p).ok()?;
            // Reports from before versioning have no `format`; newer layouts are skipped.
            if r["format"].as_u64().unwrap_or(1) > u64::from(crate::encode::REPORT_FORMAT) {
                return None;
            }
            let input = shown_path(&p, r["input"].as_str()?, false);
            let output = shown_path(&p, r["output"].as_str()?, true);
            let mtime = std::fs::metadata(&p).ok()?.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs();
            let entry = ledger.entry(&output, &input);
            Some(json!({
                "report": p,
                "input": input,
                "output": output,
                "on_record": entry.is_some(),
                "replace": replace_state(&ledger, &p, entry.is_some()),
                // Not asked of paths only a report names (null: not looked at).
                "input_exists": entry.map(|e| source_on_record(e).exists()),
                "output_exists": entry.map(|_| output.exists()),
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
    let p = std::fs::canonicalize(&r.path).with_context(|| format!("cannot open {}", r.path.display())).and_then(text_path)?;
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
    let c = std::fs::canonicalize(p).with_context(|| format!("not found: {}", p.display())).and_then(text_path)?;
    if !c.to_string_lossy().ends_with(".av1opt.json") {
        bail!("not a report");
    }
    // Asked of the name, which cannot hang: opening a pipe would wait for someone to write to it.
    let meta = std::fs::metadata(&c).with_context(|| format!("not found: {}", c.display()))?;
    if !meta.is_file() {
        bail!("{} is not a plain file, so it is not a report", c.display());
    }
    if meta.len() > replace::REPORT_LIMIT {
        bail!("{} is larger than the {} MiB a report can be", c.display(), replace::REPORT_LIMIT >> 20);
    }
    let dirs = app.settings.lock().unwrap().history_dirs.clone();
    let in_history = dirs.iter().any(|d| std::fs::canonicalize(d).is_ok_and(|d| c.starts_with(d)));
    if !in_history {
        bail!("report is outside the history folders");
    }
    Ok(c)
}

/// Refuses a report whose output and original are not a pair on record, before anything it
/// names is looked at.
fn refuse_unless_on_record(ledger: &Ledger, report: &Path) -> Result<()> {
    let r = read_report_file(report)?;
    let named = |key: &str| r[key].as_str().map(PathBuf::from);
    match (named("output"), named("input")) {
        (Some(output), Some(input)) if ledger.entry(&output, &input).is_some() => Ok(()),
        _ => bail!("{}", ledger.refusal()),
    }
}

#[derive(Deserialize)]
struct ReplaceRequest {
    report: PathBuf,
    /// Also replace an original stored on ZFS (asked for in the dialog, per run).
    #[serde(default)]
    allow_zfs: bool,
}

/// Moves the original of one report to the trash and its output into its place. One file
/// per request: the page shows progress and failures file by file.
pub fn replace_original(app: &App, body: &Value) -> Result<Value> {
    let r: ReplaceRequest = serde_json::from_value(body.clone())?;
    let report = known_report(app, &r.report)?;
    refuse_unless_on_record(&app.ledger.current(), &report)?;
    if let Check::Ready(p) = replace::check_for_list(&report)
        && crate::ui::jobs::waiting_on(app, &p.source)
    {
        bail!("the original is in the encode queue");
    }
    // The record is what says this output and this original belong together, and the output is
    // read through here, as it is about to take the original's place.
    let done = replace::apply_if(&report, r.allow_zfs, &|p| app.ledger.current().check_ready(p, true))?;
    Ok(json!({ "dest": done.dest, "trash": done.trash, "source_bytes": done.source_bytes, "warnings": done.warnings }))
}

/// Takes a replaced original back out of the trash and returns the AV1 file to where it was.
pub fn restore_original(app: &App, body: &Value) -> Result<Value> {
    let r: ReplaceRequest = serde_json::from_value(body.clone())?;
    let report = known_report(app, &r.report)?;
    refuse_unless_on_record(&app.ledger.current(), &report)?;
    let d = replace::undo_if(&report, r.allow_zfs, &|d| app.ledger.current().check_replaced(d))?;
    Ok(json!({ "source": d.source, "output": d.from }))
}

pub fn media_info(app: &App, p: &Path) -> Value {
    let size = std::fs::metadata(p).map_or(0, |m| m.len());
    match crate::probe::probe(&app.ffprobe(), p, size) {
        Ok(i) => json!({
            "codec": i.codec, "width": i.width, "height": i.height, "fps": i.fps, "duration_s": i.duration_s,
            "start_time": i.start_time, "video_start": i.video_start, "hdr": i.hdr, "size": size,
            "container": p.extension().map(|e| e.to_string_lossy().to_lowercase()),
        }),
        Err(e) => json!({ "error": format!("{e:#}"), "size": size }),
    }
}

/// What the compare view gets to know of a file: only one the page may serve is opened.
fn listed_media_info(app: &App, p: &Path, on_record: bool, gone: &str) -> Value {
    match app.is_allowed(p) {
        Some(file) => media_info(app, &file),
        None if on_record && !p.exists() => json!({ "error": gone }),
        None => json!({ "error": NOT_OPENED }),
    }
}

pub fn report(app: &App, path: &str) -> Result<Value> {
    let p = known_report(app, Path::new(path))?;
    let mut r = read_report_file(&p)?;
    let ledger = app.ledger.current();
    let input = shown_path(&p, r["input"].as_str().unwrap_or(""), false);
    let output = shown_path(&p, r["output"].as_str().unwrap_or(""), true);
    let entry = ledger.entry(&output, &input);
    let shown_input = entry.map_or_else(|| input.clone(), |e| source_on_record(e).to_path_buf());
    r["on_record"] = json!(entry.is_some());
    r["replace"] = replace_state(&ledger, &p, entry.is_some());
    r["report_path"] = json!(p);
    r["input_path"] = json!(shown_input);
    r["output_path"] = json!(output);
    r["input_info"] = listed_media_info(app, &shown_input, entry.is_some(), "source file is gone");
    r["output_info"] = listed_media_info(app, &output, entry.is_some(), "output file is gone");
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

/// What the compare view says of a file only a report names.
const NOT_OPENED: &str = "this file was not opened: only a report names it, and av1opt has no record of it. Pick it in a scan or the encode dialog to see it here";

/// What the page gets for a path it did not list in this session, or one that has since gone.
pub const NOT_LISTED: &str = "this file is not among those the page has listed, or it has moved; reload the page or pick the file again";

/// How long ffmpeg gets to make a preview. It is cut to two minutes of video at most, so one
/// taking longer has met a file or a drive that does not answer.
const PREVIEW_LIMIT: Duration = Duration::from_secs(600);

/// `normal`, or the time set in `AV1OPT_PREVIEW_LIMIT_SECS` for what the page asks ffmpeg to
/// make while someone waits.
pub fn time_limit(normal: Duration) -> Duration {
    std::env::var("AV1OPT_PREVIEW_LIMIT_SECS").ok().and_then(|s| s.parse().ok()).filter(|s| *s > 0).map_or(normal, Duration::from_secs)
}

/// Builds (or reuses) a playable stand-in for a file the browser can't play directly.
pub fn preview(app: &App, body: &Value) -> Result<Value> {
    let r: PreviewRequest = serde_json::from_value(body.clone())?;
    let src = app.is_allowed(&r.path).ok_or_else(|| anyhow!(NOT_LISTED))?;
    let meta = std::fs::metadata(&src)?;
    let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
    let start = r.start.max(0.0);
    let secs = r.secs.clamp(0.5, 120.0);
    let key = format!("{}|{}|{}|{mtime}|{start:.3}|{secs:.3}", r.kind, src.display(), meta.len());
    let out = app.preview_dir.join(format!("{:016x}.mp4", crate::scenes::fnv1a(&key)));
    let url = |p: &Path| format!("/media?path={}", percent_encode(&p.to_string_lossy()));
    // A second request for the same clip waits for the first instead of encoding it twice.
    // Other clips only wait for a turn: previews are CPU-heavy, and one that hangs until its
    // time limit leaves the other turn free.
    static TURNS: Turns = Turns::new(2);
    let clip = TURNS.clip(&key);
    let _same_clip = clip.lock().unwrap_or_else(|e| e.into_inner());
    if out.exists() {
        return Ok(json!({ "url": url(&out), "start": start, "secs": secs, "kind": r.kind }));
    }
    let _turn = TURNS.wait();
    let info = crate::probe::probe(&app.ffprobe(), &src, meta.len())?;
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
    let mut tools = app.tools(1);
    // Tracked so shutting down stops a preview transcode too.
    tools.hooks = Some(app.preview_hooks.clone());
    if let Err(e) = tools.run_capture(&a, time_limit(PREVIEW_LIMIT)) {
        let _ = std::fs::remove_file(&part);
        return Err(e.context("preview encode failed"));
    }
    crate::files::make_private(&part)?;
    std::fs::rename(&part, &out)?;
    Ok(json!({ "url": url(&out), "start": start, "secs": secs, "kind": r.kind }))
}

/// A video about to be encoded, for the encode dialog's section picker: its details and a
/// URL the player can stream it from.
pub fn source(app: &App, body: &Value) -> Result<Value> {
    let r: DirRequest = serde_json::from_value(body.clone())?;
    let p = std::fs::canonicalize(&r.path).with_context(|| format!("not found: {}", r.path.display())).and_then(text_path)?;
    if !p.is_file() || !is_video(&p) {
        bail!("{} is not a video file", p.display());
    }
    app.allow(&p);
    let mut info = media_info(app, &p);
    info["url"] = json!(format!("/media?path={}", percent_encode(&p.to_string_lossy())));
    info["path"] = json!(p);
    Ok(info)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_group_is_the_users_own_when_nobody_else_is_in_it() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\nme:x:1000:1000:Me:/home/me:/bin/bash\nyou:x:1001:1001::/home/you:/bin/sh\nshared:x:1002:100::/home/shared:/bin/sh\n";
        let group = "root:x:0:\nusers:x:100:me\nme:x:1000:\nyou:x:1001:\nmedia:x:1500:me,you\nmine:x:1600:me\n";
        // A group to oneself, by being its only user or its only member.
        assert!(alone_in_group(passwd, group, 1000, 1000));
        assert!(alone_in_group(passwd, group, 1600, 1000));
        // Another member, another user whose group it is, someone else's group.
        assert!(!alone_in_group(passwd, group, 1500, 1000));
        assert!(!alone_in_group(passwd, group, 100, 1000));
        assert!(!alone_in_group(passwd, group, 1001, 1000));
        // A group the file does not list: its members are not known, so it is not one's own.
        assert!(!alone_in_group(passwd, group, 4242, 1000));
    }

    #[test]
    fn a_program_is_safe_where_nobody_else_can_swap_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("av1opt-program-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("mine")).unwrap();
        std::fs::create_dir_all(dir.join("open")).unwrap();
        for f in ["mine/ffmpeg", "open/ffmpeg"] {
            std::fs::write(dir.join(f), b"x").unwrap();
            std::fs::set_permissions(dir.join(f), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::set_permissions(dir.join("mine"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(dir.join("open"), std::fs::Permissions::from_mode(0o777)).unwrap();
        std::os::unix::fs::symlink(dir.join("mine/ffmpeg"), dir.join("open/link")).unwrap();
        // (The folder the test is in is the system's sticky one, or the user's own.)
        let real = std::fs::canonicalize(dir.join("mine/ffmpeg")).unwrap();
        assert_eq!(safe_program(&dir.join("mine/ffmpeg")), Ok(real.clone()));
        // A link in a folder open to all leads to a safe file: the file is what is kept.
        assert_eq!(safe_program(&dir.join("open/link")), Ok(real));
        assert!(safe_program(&dir.join("open/ffmpeg")).is_err());
        assert_eq!(safe_program(&dir.join("mine/nothing")), Err("is not there"));
        assert_eq!(safe_program(&dir.join("mine")), Err("is not a program"));
        // A file others can write to is not safe wherever it lies.
        std::fs::set_permissions(dir.join("mine/ffmpeg"), std::fs::Permissions::from_mode(0o757)).unwrap();
        assert!(safe_program(&dir.join("mine/ffmpeg")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_chosen_program_is_judged_by_the_file_its_path_leads_to() {
        let dir = std::env::temp_dir().join(format!("av1opt-chosen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        for f in ["ffmpeg-7.1", "launcher"] {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        std::os::unix::fs::symlink(dir.join("ffmpeg-7.1"), dir.join("ffmpeg")).unwrap();
        std::os::unix::fs::symlink(dir.join("launcher"), dir.join("ffprobe")).unwrap();
        assert_eq!(chosen_program("ffmpeg", &dir.join("ffmpeg")), Ok(dir.join("ffmpeg-7.1")));
        let refused = chosen_program("ffprobe", &dir.join("ffprobe")).unwrap_err();
        assert!(refused.contains("a program with another name"), "{refused}");
        assert!(chosen_program("ffmpeg", &dir.join("nothing")).unwrap_err().ends_with("is not there"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
