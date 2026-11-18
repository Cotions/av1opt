//! Duplicate search for the page: runs in the background with progress events, keeps the last
//! results, serves frame thumbnails and describes a pair of files for the comparison player.

use super::{App, library, now};
use crate::dups::{self, File, Progress, Report, matching::Prefs};
use crate::events::Hooks;
use crate::scan::is_video;
use crate::tools::path_arg;
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Instant, UNIX_EPOCH};

pub struct Run {
    id: u64,
    paths: Vec<PathBuf>,
    /// running, done, failed, cancelled
    status: String,
    error: Option<String>,
    /// probing, fingerprinting, comparing
    phase: &'static str,
    done: usize,
    total: usize,
    started: u64,
    report: Option<Arc<Report>>,
    /// Files of the result moved to the trash since, in that order.
    trashed: Vec<PathBuf>,
    hooks: Arc<Hooks>,
}

#[derive(Deserialize)]
struct Request {
    paths: Vec<PathBuf>,
    #[serde(default)]
    prefer: Vec<PathBuf>,
    #[serde(default)]
    avoid: Vec<String>,
    #[serde(default = "d_tolerance")]
    tolerance: u32,
    #[serde(default)]
    refresh: bool,
}

fn d_tolerance() -> u32 {
    dups::DEFAULT_MAX_DIST
}

fn head(r: &Run) -> Value {
    json!({
        "id": r.id, "paths": r.paths, "status": r.status, "error": r.error, "phase": r.phase,
        "done": r.done, "total": r.total, "started": r.started,
    })
}

pub fn start(app: &Arc<App>, body: &Value) -> Result<Value> {
    let r: Request = serde_json::from_value(body.clone())?;
    if r.paths.is_empty() {
        bail!("add at least one folder");
    }
    if r.tolerance > 20 {
        bail!("tolerance must be 0-20");
    }
    if super::jobs::shutting_down(app) {
        bail!("av1opt is shutting down");
    }
    let paths: Vec<PathBuf> = r
        .paths
        .iter()
        .map(|p| std::fs::canonicalize(p).with_context(|| format!("cannot open {}", p.display())).and_then(super::text_path))
        .collect::<Result<_>>()?;
    let prefs = Prefs { prefer: r.prefer.clone(), avoid: r.avoid.iter().map(|a| a.trim().to_string()).filter(|a| !a.is_empty()).collect() };
    let hooks = Arc::new(Hooks::new(|_| {}));
    let id = {
        let mut runs = app.dups.lock().unwrap();
        if runs.iter().any(|x| x.status == "running") {
            bail!("a duplicate search is already running");
        }
        let id = runs.iter().map(|x| x.id).max().unwrap_or(0) + 1;
        // Keep memory bounded: only the last few results are kept.
        while runs.len() >= 3 {
            runs.remove(0);
        }
        runs.push(Run { id, paths: paths.clone(), status: "running".into(), error: None, phase: "probing", done: 0, total: 0, started: now(), report: None, trashed: Vec::new(), hooks: hooks.clone() });
        id
    };
    {
        let mut s = app.settings.lock().unwrap();
        s.dup_paths = paths.clone();
        s.dup_prefer = prefs.prefer.clone();
        s.dup_avoid = prefs.avoid.clone();
    }
    app.save_settings();
    trim_thumbs(4000);

    let app = app.clone();
    std::thread::spawn(move || {
        let jobs = app.cfg.jobs.clamp(1, 64);
        let mut tools = app.tools(jobs as u8);
        tools.hooks = Some(hooks);
        let last = Mutex::new(Instant::now());
        let progress = |p: Progress| {
            let (phase, done, total) = match p {
                Progress::Probing { done, total } => ("probing", done, total),
                Progress::Fingerprinting { done, total } => ("fingerprinting", done, total),
                Progress::Comparing { videos } => ("comparing", videos, videos),
            };
            let changed = {
                let mut runs = app.dups.lock().unwrap();
                let Some(run) = runs.iter_mut().find(|x| x.id == id) else { return };
                let changed = run.phase != phase;
                (run.phase, run.done, run.total) = (phase, done, total);
                changed
            };
            let mut l = last.lock().unwrap();
            if changed || done == total || l.elapsed().as_millis() > 250 {
                *l = Instant::now();
                app.broadcast(json!({ "kind": "dups", "id": id, "phase": phase, "done": done, "total": total }));
            }
        };
        let opts = dups::Options { jobs, max_dist: r.tolerance, prefs, refresh: r.refresh };
        let result = dups::find(&paths, &tools, &opts, &progress);
        {
            let mut runs = app.dups.lock().unwrap();
            if let Some(run) = runs.iter_mut().find(|x| x.id == id) {
                match result {
                    Ok(report) => {
                        for f in &report.files {
                            app.allow_video(&f.path);
                        }
                        run.report = Some(Arc::new(report));
                        run.status = "done".into();
                    }
                    Err(e) if crate::events::is_cancelled(&e) => run.status = "cancelled".into(),
                    Err(e) => {
                        run.status = "failed".into();
                        run.error = Some(format!("{e:#}"));
                    }
                }
            }
        }
        app.broadcast(json!({ "kind": "dups_done", "id": id }));
    });
    Ok(json!({ "id": id }))
}

/// The runs still remembered, newest last, without their results.
pub fn list(app: &App) -> Value {
    json!({ "runs": app.dups.lock().unwrap().iter().map(head).collect::<Vec<_>>() })
}

pub fn get(app: &App, id: u64) -> Result<Value> {
    let (mut v, report, trashed) = {
        let runs = app.dups.lock().unwrap();
        let r = runs.iter().find(|x| x.id == id).ok_or_else(|| anyhow!("not found: duplicate search {id}"))?;
        (head(r), r.report.clone(), r.trashed.clone())
    };
    // Serialized without the lock held: the result of a big library is large.
    v["report"] = json!(report.as_deref());
    v["trashed"] = json!(trashed);
    Ok(v)
}

#[derive(Deserialize)]
struct TrashRequest {
    paths: Vec<PathBuf>,
    /// Also trash files stored on ZFS (asked for in a dialog of its own, per request).
    #[serde(default)]
    allow_zfs: bool,
}

/// Whether `paths` may go to the trash: only files of the result that are still as the search
/// saw them, and never the last copy of a video. `gone` are the files trashed earlier;
/// `present` says whether a file is still there unchanged.
fn check_trash(report: &Report, gone: &HashSet<PathBuf>, paths: &[PathBuf], present: &dyn Fn(&File) -> bool) -> Result<()> {
    if paths.is_empty() {
        bail!("nothing is marked");
    }
    let marked: HashSet<&Path> = paths.iter().map(|p| p.as_path()).collect();
    for p in paths {
        let Some(f) = report.files.iter().find(|f| f.path == *p) else {
            bail!("{} is not a file of this result", p.display());
        };
        if gone.contains(p) {
            bail!("{} is in the trash already", p.display());
        }
        if !present(f) {
            bail!("{} is gone or has changed since the search; search again", p.display());
        }
    }
    for g in &report.groups {
        let members: Vec<&File> = g.members.iter().filter_map(|&i| report.files.get(i)).collect();
        let Some(one) = members.iter().find(|f| marked.contains(f.path.as_path())) else { continue };
        let left: Vec<&&File> = members.iter().filter(|f| !marked.contains(f.path.as_path()) && !gone.contains(&f.path)).collect();
        if left.is_empty() {
            bail!("every copy of {} is marked: nothing would be left of that video. Untick one", one.path.display());
        }
        if !left.iter().any(|f| present(f)) {
            bail!("the copy to keep of {} ({}) is gone or has changed since the search; search again", one.path.display(), left[0].path.display());
        }
    }
    Ok(())
}

/// Moves marked files of a result to the trash of the drive each is on. Nothing is deleted:
/// the file manager can put them back until the trash is emptied.
pub fn trash(app: &App, id: u64, body: &Value) -> Result<Value> {
    let r: TrashRequest = serde_json::from_value(body.clone())?;
    // One at a time, so two requests can't each take "the other" copy of a video.
    static ONE: Mutex<()> = Mutex::new(());
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let (report, gone) = {
        let runs = app.dups.lock().unwrap();
        let run = runs.iter().find(|x| x.id == id).ok_or_else(|| anyhow!("not found: duplicate search {id}"))?;
        let report = run.report.clone().ok_or_else(|| anyhow!("that search has no result"))?;
        (report, run.trashed.iter().cloned().collect::<HashSet<PathBuf>>())
    };
    let mut paths = r.paths;
    paths.sort();
    paths.dedup();
    let present = |f: &File| std::fs::symlink_metadata(&f.path).is_ok_and(|m| m.is_file() && m.len() == f.size);
    check_trash(&report, &gone, &paths, &present)?;
    // A trash on ZFS is a write to the pool like any other: asked about first, and nothing
    // is moved before the answer.
    if !r.allow_zfs {
        let on_zfs: Vec<&PathBuf> = paths.iter().filter(|p| crate::fsguard::on_zfs(p)).collect();
        if !on_zfs.is_empty() {
            return Ok(json!({ "needs_zfs": on_zfs, "trashed": [], "failed": [], "bytes": 0 }));
        }
    }

    let (mut trashed, mut failed, mut bytes) = (Vec::new(), Vec::new(), 0u64);
    for p in &paths {
        match crate::trash::trash(p) {
            Ok(_) => {
                bytes += report.files.iter().find(|f| f.path == *p).map_or(0, |f| f.size);
                trashed.push(p.clone());
            }
            Err(e) => failed.push(json!({ "path": p, "error": format!("{e:#}") })),
        }
    }
    if let Some(run) = app.dups.lock().unwrap().iter_mut().find(|x| x.id == id) {
        run.trashed.extend(trashed.iter().cloned());
    }
    Ok(json!({ "trashed": trashed, "failed": failed, "bytes": bytes }))
}

pub fn cancel(app: &App, id: u64) -> Result<Value> {
    let runs = app.dups.lock().unwrap();
    let r = runs.iter().find(|x| x.id == id).ok_or_else(|| anyhow!("not found: duplicate search {id}"))?;
    if r.status == "running" {
        r.hooks.cancel();
    }
    Ok(json!({ "ok": true }))
}

/// Stops a running search so no ffmpeg outlives the server.
pub fn shutdown(app: &App) {
    for r in app.dups.lock().unwrap().iter() {
        r.hooks.cancel();
    }
}

#[derive(Deserialize)]
struct PairRequest {
    left: PathBuf,
    right: PathBuf,
}

/// Two files of a result, described the way the comparison player expects an encode report:
/// `right` takes the place of the output (the master clock, with sound).
pub fn pair(app: &App, body: &Value) -> Result<Value> {
    let r: PairRequest = serde_json::from_value(body.clone())?;
    let pick = |p: &Path| app.is_allowed(p).filter(|c| is_video(c)).ok_or_else(|| anyhow!("{} is not a file from a duplicate search", p.display()));
    let (left, right) = (pick(&r.left)?, pick(&r.right)?);
    let (li, ri) = (library::media_info(app, &left), library::media_info(app, &right));
    Ok(json!({
        "pair": true,
        "input_path": left, "output_path": right,
        "source_bytes": li["size"], "output_bytes": ri["size"],
        "input_info": li, "output_info": ri,
    }))
}

fn thumb_dir() -> Option<PathBuf> {
    let d = library::cache_dir()?.join("thumbs");
    std::fs::create_dir_all(&d).ok()?;
    Some(d)
}

/// Keeps the thumbnail folder to the `keep` most recent files.
fn trim_thumbs(keep: usize) {
    let Some(dir) = thumb_dir() else { return };
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

/// A small JPEG of the frame at `t` seconds of a file from a result (made once, then cached).
pub fn thumb(app: &App, path: &Path, t: f64) -> Result<Vec<u8>> {
    let src = app.is_allowed(path).filter(|c| is_video(c)).ok_or_else(|| anyhow!(library::NOT_LISTED))?;
    let meta = std::fs::metadata(&src)?;
    let mtime = meta.modified().ok().and_then(|m| m.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
    let t = if t.is_finite() { t.max(0.0) } else { 0.0 };
    let key = crate::scenes::fnv1a(&format!("{}|{}|{mtime}|{t:.2}", src.display(), meta.len()));
    let dir = thumb_dir().context("no cache folder for thumbnails")?;
    crate::fsguard::ensure_writable_location(&dir, false)?;
    let out = dir.join(format!("{key:016x}.jpg"));
    // A page of results asks for dozens at once; three ffmpeg runs at a time is plenty.
    static SLOTS: [Mutex<()>; 3] = [Mutex::new(()), Mutex::new(()), Mutex::new(())];
    let _slot = SLOTS[(key % 3) as usize].lock().unwrap_or_else(|e| e.into_inner());
    if !out.exists() {
        let part = dir.join(format!("{key:016x}.part.jpg"));
        let mut a: Vec<String> = ["-y", "-v", "error", "-threads", "1", "-skip_loop_filter", "all", "-skip_frame", "noref", "-ss"].map(String::from).to_vec();
        a.extend([format!("{t:.3}"), "-i".into(), path_arg(&src)]);
        // Cover art can be the first video stream; the probe knows the real one.
        let info = crate::probe::probe(&app.ffprobe(), &src, meta.len())?;
        a.extend(["-map".into(), format!("0:{}", info.video_index)]);
        a.extend(["-frames:v", "1", "-vf", "scale=320:-2:flags=bilinear", "-q:v", "6", "-f", "mjpeg"].map(String::from));
        a.push(path_arg(&part));
        let mut tools = app.tools(1);
        tools.hooks = Some(app.preview_hooks.clone());
        tools.run(&a).context("could not read a frame")?;
        if std::fs::metadata(&part).map_or(0, |m| m.len()) == 0 {
            let _ = std::fs::remove_file(&part);
            bail!("no frame at {t:.1} s");
        }
        std::fs::rename(&part, &out)?;
    }
    Ok(std::fs::read(&out)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dups::matching::{Group, Kind};

    fn file(name: &str) -> File {
        File { path: PathBuf::from(name), root: PathBuf::from("/"), size: 10, codec: "h264".into(), width: 1280, height: 720, fps: 30.0, duration_s: 60.0, video_mbps: 2.0, identical: None }
    }

    #[test]
    fn the_last_copy_of_a_video_never_goes() {
        let report = Report {
            files: vec![file("/a/1.mkv"), file("/b/1.mp4"), file("/c/1.webm"), file("/a/2.mkv"), file("/b/2.mkv")],
            groups: vec![
                Group { kind: Kind::Same, score: 1.0, members: vec![0, 1, 2], reason: String::new() },
                Group { kind: Kind::SameStart, score: 1.0, members: vec![3, 4], reason: String::new() },
            ],
            ..Default::default()
        };
        let paths = |names: &[&str]| names.iter().map(PathBuf::from).collect::<Vec<_>>();
        let all = |_: &File| true;
        let none = HashSet::new();
        let check = |gone: &HashSet<PathBuf>, names: &[&str], present: &dyn Fn(&File) -> bool| check_trash(&report, gone, &paths(names), present).map_err(|e| e.to_string());

        assert!(check(&none, &["/b/1.mp4", "/c/1.webm", "/b/2.mkv"], &all).is_ok());
        assert!(check(&none, &[], &all).unwrap_err().contains("nothing is marked"));
        assert!(check(&none, &["/etc/passwd"], &all).unwrap_err().contains("not a file of this result"));
        assert!(check(&none, &["/a/1.mkv", "/b/1.mp4", "/c/1.webm"], &all).unwrap_err().contains("every copy"));
        assert!(check(&none, &["/a/2.mkv", "/b/2.mkv"], &all).unwrap_err().contains("every copy"));

        // Copies trashed earlier don't count as kept, and can't be trashed twice.
        let gone: HashSet<PathBuf> = paths(&["/a/1.mkv"]).into_iter().collect();
        assert!(check(&gone, &["/b/1.mp4"], &all).is_ok());
        assert!(check(&gone, &["/b/1.mp4", "/c/1.webm"], &all).unwrap_err().contains("every copy"));
        assert!(check(&gone, &["/a/1.mkv"], &all).unwrap_err().contains("in the trash already"));

        // A marked file that changed, or a keeper that is no longer there, stops everything.
        let without = |missing: &'static str| move |f: &File| f.path != Path::new(missing);
        assert!(check(&none, &["/b/1.mp4"], &without("/b/1.mp4")).unwrap_err().contains("gone or has changed"));
        assert!(check(&none, &["/b/2.mkv"], &without("/a/2.mkv")).unwrap_err().contains("copy to keep"));
        // One keeper missing is fine while another is there.
        assert!(check(&none, &["/b/1.mp4"], &without("/a/1.mkv")).is_ok());
    }
}
