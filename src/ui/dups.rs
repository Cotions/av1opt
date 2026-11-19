//! Duplicate search for the page: runs in the background with progress events, keeps the last
//! results, serves frame thumbnails and describes a pair of files for the comparison player.

use super::turns::Turns;
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
use std::time::{Duration, Instant, UNIX_EPOCH};

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

/// Whether `paths` may go to the trash at all: only files of the result, not moved yet, and
/// never every copy of a video. `gone` are the files trashed earlier. Whether each file is still
/// what the search saw is asked when its turn comes, in [`move_marked`].
fn check_trash(report: &Report, gone: &HashSet<PathBuf>, paths: &[PathBuf]) -> Result<()> {
    if paths.is_empty() {
        bail!("nothing is marked");
    }
    let marked: HashSet<&Path> = paths.iter().map(|p| p.as_path()).collect();
    for p in paths {
        if !report.files.iter().any(|f| f.path == *p) {
            bail!("{} is not a file of this result", p.display());
        }
        if gone.contains(p) {
            bail!("{} is in the trash already", p.display());
        }
    }
    for g in &report.groups {
        let members: Vec<&File> = g.members.iter().filter_map(|&i| report.files.get(i)).collect();
        let Some(one) = members.iter().find(|f| marked.contains(f.path.as_path())) else { continue };
        // A copy in a trash goes when the trash is emptied: the one thing it cannot be is what
        // stays of the video. Walks keep out of trash folders, so this takes a file moved there
        // after the search, or a result made some other way.
        if let Some(keeper) = members.first().filter(|keeper| crate::trash::in_a_trash(&keeper.path)) {
            bail!("the copy to keep of {} ({}) lies in a trash folder, so nothing would stay of that video; search again", one.path.display(), keeper.path.display());
        }
        let left: Vec<&&File> = members.iter().filter(|f| !marked.contains(f.path.as_path()) && !gone.contains(&f.path)).collect();
        let Some(first) = left.first() else {
            bail!("every copy of {} is marked: nothing would be left of that video. Untick one", one.path.display());
        };
        if left.iter().all(|f| crate::trash::in_a_trash(&f.path)) {
            bail!("the copy to keep of {} ({}) lies in a trash folder, so nothing would stay of that video; search again", one.path.display(), first.path.display());
        }
    }
    Ok(())
}

/// The copies that stay when `file` goes, one for each group it is in, all of them still what
/// the search saw. The copy a group keeps comes first, so it is the one relied on while it
/// is fit for that.
fn copies_that_stay<'a>(report: &'a Report, gone: &HashSet<PathBuf>, marked: &HashSet<&Path>, file: &File, unchanged: &dyn Fn(&File) -> bool) -> Result<Vec<&'a File>> {
    let mut stay = Vec::new();
    for g in &report.groups {
        let members: Vec<&File> = g.members.iter().filter_map(|&i| report.files.get(i)).collect();
        if !members.iter().any(|f| f.path == file.path) {
            continue;
        }
        let left: Vec<&File> = members.into_iter().filter(|f| !marked.contains(f.path.as_path()) && !gone.contains(&f.path) && !crate::trash::in_a_trash(&f.path)).collect();
        match left.iter().find(|f| unchanged(f)) {
            Some(copy) => stay.push(*copy),
            None => bail!("the copy to keep ({}) is gone or has changed since the search, so {} was left where it is; search again", left.first().map_or(file.path.as_path(), |f| f.path.as_path()).display(), file.path.display()),
        }
    }
    Ok(stay)
}

/// What became of the files of one request.
#[derive(Default)]
struct Moved {
    trashed: Vec<PathBuf>,
    failed: Vec<Value>,
    bytes: u64,
}

/// Moves the marked files one at a time with `to_trash`. Each is looked at again right before
/// its move, with the copy that is to stay: what changed since the search stays where it is,
/// with a message, and the others still go.
fn move_marked(report: &Report, gone: &HashSet<PathBuf>, paths: &[PathBuf], unchanged: &dyn Fn(&File) -> bool, to_trash: &dyn Fn(&Path) -> Result<PathBuf>) -> Moved {
    let marked: HashSet<&Path> = paths.iter().map(|p| p.as_path()).collect();
    let mut moved = Moved::default();
    for p in paths {
        let Some(file) = report.files.iter().find(|f| f.path == *p) else { continue };
        let one = || -> Result<()> {
            if !unchanged(file) {
                bail!("{} is gone or has changed since the search, so it was left alone; search again", p.display());
            }
            let stay = copies_that_stay(report, gone, &marked, file, unchanged)?;
            let dest = to_trash(p)?;
            // Device and inode cannot tell a network mount's view of a folder from a second
            // copy. If the copy that stays went with this one, they were the same file.
            match stay.iter().find(|copy| !unchanged(copy)) {
                None => Ok(()),
                Some(copy) => {
                    let what = format!("moving {} to the trash took {} with it, so they are one file seen through two paths (a network mount, perhaps)", p.display(), copy.path.display());
                    match crate::trash::restore(&dest, p) {
                        Ok(()) => bail!("{what}. It was put back at once"),
                        Err(e) => bail!("{what}, and it could not be put back: it is at {} ({e:#})", dest.display()),
                    }
                }
            }
        };
        match one() {
            Ok(()) => {
                moved.bytes += file.size;
                moved.trashed.push(p.clone());
            }
            Err(e) => moved.failed.push(json!({ "path": p, "error": format!("{e:#}") })),
        }
    }
    moved
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
    check_trash(&report, &gone, &paths)?;
    // A trash on ZFS is a write to the pool like any other: asked about first, and nothing
    // is moved before the answer.
    if !r.allow_zfs {
        let on_zfs: Vec<&PathBuf> = paths.iter().filter(|p| crate::fsguard::on_zfs(p)).collect();
        if !on_zfs.is_empty() {
            return Ok(json!({ "needs_zfs": on_zfs, "trashed": [], "failed": [], "bytes": 0 }));
        }
    }

    let Moved { trashed, failed, bytes } = move_marked(&report, &gone, &paths, &File::unchanged, &crate::trash::trash);
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
    crate::files::private_folder(&d).ok()?;
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

/// How long ffmpeg gets to find one frame.
const THUMB_LIMIT: Duration = Duration::from_secs(60);

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
    // A page of results asks for dozens at once; three ffmpeg runs at a time is plenty, and
    // the same picture is made once.
    static TURNS: Turns = Turns::new(3);
    let picture = TURNS.clip(&format!("{key:016x}"));
    let _same_picture = picture.lock().unwrap_or_else(|e| e.into_inner());
    if !out.exists() {
        let _turn = TURNS.wait();
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
        if let Err(e) = tools.run_capture(&a, library::time_limit(THUMB_LIMIT)) {
            let _ = std::fs::remove_file(&part);
            return Err(e.context("could not read a frame"));
        }
        if std::fs::metadata(&part).map_or(0, |m| m.len()) == 0 {
            let _ = std::fs::remove_file(&part);
            bail!("no frame at {t:.1} s");
        }
        crate::files::make_private(&part)?;
        std::fs::rename(&part, &out)?;
    }
    Ok(std::fs::read(&out)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dups::matching::{Group, Kind};

    fn file(name: &str) -> File {
        File { path: PathBuf::from(name), root: PathBuf::from("/"), size: 10, codec: "h264".into(), width: 1280, height: 720, fps: 30.0, duration_s: 60.0, video_mbps: 2.0, identical: None, damaged: false, modified: 0, digest: None }
    }

    /// A file on disk, as a search that ended now would describe it.
    fn seen(path: &Path) -> File {
        let meta = std::fs::metadata(path).unwrap();
        let modified = meta.modified().unwrap().duration_since(UNIX_EPOCH).unwrap().as_secs();
        File { size: meta.len(), modified, digest: crate::files::sample_digest(path, meta.len(), 64 * 1024).ok(), ..file(path.to_str().unwrap()) }
    }

    /// A folder with the videos `names` in it, all of one size, and a private trash for them.
    fn library(tag: &str, names: &[&str]) -> (PathBuf, Vec<PathBuf>) {
        let dir = std::env::temp_dir().join(format!("av1opt-ui-dups-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        crate::trash::TEST_HOME_TRASH.with(|h| *h.borrow_mut() = Some(dir.join("Trash")));
        let paths: Vec<PathBuf> = names.iter().map(|n| dir.join("lib").join(n)).collect();
        for (i, p) in paths.iter().enumerate() {
            std::fs::write(p, format!("contents of copy {i}")).unwrap();
        }
        (dir, paths)
    }

    fn pair_report(paths: &[PathBuf]) -> Report {
        Report {
            files: paths.iter().map(|p| seen(p)).collect(),
            groups: vec![Group { kind: Kind::Same, score: 1.0, members: (0..paths.len()).collect(), reason: String::new() }],
            ..Default::default()
        }
    }

    fn move_all(report: &Report, paths: &[PathBuf]) -> Moved {
        move_marked(report, &HashSet::new(), paths, &File::unchanged, &crate::trash::trash)
    }

    fn only_error(moved: &Moved) -> String {
        assert_eq!(moved.failed.len(), 1, "{:?}", moved.failed);
        moved.failed[0]["error"].as_str().unwrap().to_string()
    }

    #[test]
    fn a_file_that_changed_since_the_search_stays_where_it_is() {
        let (dir, p) = library("stale", &["keep.mkv", "spare.mkv"]);
        let (keep, spare) = (&p[0], &p[1]);
        let report = pair_report(&p);
        let when = std::fs::metadata(keep).unwrap().modified().unwrap();
        let set_time = |path: &Path| std::fs::File::options().write(true).open(path).unwrap().set_modified(when).unwrap();

        // The two have changed places: same sizes, same dates, other contents.
        let parked = dir.join("parked");
        std::fs::rename(keep, &parked).unwrap();
        std::fs::rename(spare, keep).unwrap();
        std::fs::rename(&parked, spare).unwrap();
        set_time(keep);
        set_time(spare);
        let moved = move_all(&report, std::slice::from_ref(spare));
        assert!(moved.trashed.is_empty());
        assert!(only_error(&moved).contains("changed since the search"), "{:?}", moved.failed);
        assert!(spare.exists());
        std::fs::rename(keep, &parked).unwrap();
        std::fs::rename(spare, keep).unwrap();
        std::fs::rename(&parked, spare).unwrap();

        // The copy to keep was overwritten with zeros of its size, at the same second.
        let original = std::fs::read(keep).unwrap();
        std::fs::write(keep, vec![0; original.len()]).unwrap();
        set_time(keep);
        let moved = move_all(&report, std::slice::from_ref(spare));
        let error = only_error(&moved);
        assert!(error.contains("copy to keep") && error.contains("changed since the search"), "{error}");
        assert!(spare.exists());
        std::fs::write(keep, &original).unwrap();
        set_time(keep);

        // Only touched: the date is part of what the search saw.
        std::fs::File::options().write(true).open(spare).unwrap().set_modified(when + std::time::Duration::from_secs(5)).unwrap();
        assert!(only_error(&move_all(&report, std::slice::from_ref(spare))).contains("changed since the search"));
        set_time(spare);

        // As the search saw them, it goes.
        let moved = move_all(&report, std::slice::from_ref(spare));
        assert_eq!((moved.trashed.as_slice(), moved.failed.len(), moved.bytes), (std::slice::from_ref(spare), 0, report.files[1].size));
        assert!(!spare.exists() && keep.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn one_changed_file_does_not_stop_the_others() {
        let (dir, p) = library("some", &["a.mkv", "b.mkv", "c.mkv", "d.mkv"]);
        let mut report = pair_report(&p);
        report.groups = vec![Group { kind: Kind::Same, score: 1.0, members: vec![0, 1], reason: String::new() }, Group { kind: Kind::Same, score: 1.0, members: vec![2, 3], reason: String::new() }];
        std::fs::write(&p[1], "another file altogether").unwrap();
        let moved = move_all(&report, &[p[1].clone(), p[3].clone()]);
        assert_eq!(moved.trashed, [p[3].clone()]);
        assert!(only_error(&moved).contains("b.mkv"));
        assert!(p[1].exists() && !p[3].exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_keeper_that_leaves_with_the_spare_brings_the_spare_back() {
        let (dir, p) = library("one-file", &["keep.mkv", "spare.mkv"]);
        let (keep, spare) = (&p[0], &p[1]);
        let report = pair_report(&p);
        let contents = std::fs::read(spare).unwrap();
        // The two names are one file on a server: when one goes, so does the other.
        let moved = move_marked(&report, &HashSet::new(), std::slice::from_ref(spare), &File::unchanged, &|path| {
            let dest = crate::trash::trash(path)?;
            std::fs::remove_file(keep).unwrap();
            Ok(dest)
        });
        assert!(moved.trashed.is_empty() && moved.bytes == 0);
        let error = only_error(&moved);
        assert!(error.contains("one file seen through two paths") && error.ends_with("put back at once"), "{error}");
        assert_eq!(std::fs::read(spare).unwrap(), contents);
        assert_eq!(std::fs::read_dir(dir.join("Trash/files")).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(dir.join("Trash/info")).unwrap().count(), 0);

        // Where it cannot go back the message says where it is.
        let report = pair_report(&[spare.clone()]);
        std::fs::write(keep, "again").unwrap();
        let report = Report { files: vec![seen(keep), report.files[0].clone()], groups: vec![Group { kind: Kind::Same, score: 1.0, members: vec![0, 1], reason: String::new() }], ..Default::default() };
        let moved = move_marked(&report, &HashSet::new(), std::slice::from_ref(spare), &File::unchanged, &|path| {
            let dest = crate::trash::trash(path)?;
            std::fs::remove_file(keep).unwrap();
            std::fs::write(path, "something new in its place").unwrap();
            Ok(dest)
        });
        let error = only_error(&moved);
        assert!(error.contains("could not be put back") && error.contains("Trash/files/spare.mkv"), "{error}");
        assert_eq!(std::fs::read(dir.join("Trash/files/spare.mkv")).unwrap(), contents);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_second_copy_stands_in_for_a_keeper_that_changed() {
        let (dir, p) = library("stand-in", &["keep.mkv", "other.mkv", "spare.mkv"]);
        let report = pair_report(&p);
        std::fs::remove_file(&p[0]).unwrap();
        let moved = move_all(&report, std::slice::from_ref(&p[2]));
        assert_eq!((moved.trashed.len(), moved.failed.len()), (1, 0));
        // With that one marked as well, nothing is left to rely on.
        let (dir2, q) = library("stand-in-2", &["keep.mkv", "other.mkv", "spare.mkv"]);
        let report = pair_report(&q);
        std::fs::remove_file(&q[0]).unwrap();
        let moved = move_all(&report, &q[1..]);
        assert!(moved.trashed.is_empty() && moved.failed.len() == 2, "{:?}", moved.failed);
        assert!(q[1].exists() && q[2].exists());
        for d in [dir, dir2] {
            std::fs::remove_dir_all(d).unwrap();
        }
    }

    #[test]
    fn a_copy_in_a_trash_is_nothing_to_keep() {
        crate::trash::TEST_HOME_TRASH.with(|h| *h.borrow_mut() = Some(PathBuf::from("/data/Trash")));
        let report = |names: &[&str]| Report {
            files: names.iter().map(|n| file(n)).collect(),
            groups: vec![Group { kind: Kind::Same, score: 1.0, members: (0..names.len()).collect(), reason: String::new() }],
            ..Default::default()
        };
        let live = PathBuf::from("/lib/only.mkv");
        for keeper in ["/data/Trash/files/a.mkv", "/drive/.Trash-1000/files/a.mkv", "/drive/.Trash/1000/files/a.mkv"] {
            let e = check_trash(&report(&[keeper, "/lib/only.mkv"]), &HashSet::new(), std::slice::from_ref(&live)).unwrap_err().to_string();
            assert!(e.contains("lies in a trash folder") && e.contains(keeper), "{e}");
        }
        // Another copy in a trash does not count as what stays either.
        let e = check_trash(&report(&["/lib/only.mkv", "/lib/b.mkv", "/data/Trash/files/a.mkv"]), &HashSet::new(), &[live.clone(), "/lib/b.mkv".into()]).unwrap_err().to_string();
        assert!(e.contains("copy to keep"), "{e}");
        assert!(check_trash(&report(&["/lib/b.mkv", "/lib/only.mkv", "/data/Trash/files/a.mkv"]), &HashSet::new(), &[live]).is_ok());
        crate::trash::TEST_HOME_TRASH.with(|h| *h.borrow_mut() = None);
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
        let none = HashSet::new();
        let check = |gone: &HashSet<PathBuf>, names: &[&str]| check_trash(&report, gone, &paths(names)).map_err(|e| e.to_string());

        assert!(check(&none, &["/b/1.mp4", "/c/1.webm", "/b/2.mkv"]).is_ok());
        assert!(check(&none, &[]).unwrap_err().contains("nothing is marked"));
        assert!(check(&none, &["/etc/passwd"]).unwrap_err().contains("not a file of this result"));
        assert!(check(&none, &["/a/1.mkv", "/b/1.mp4", "/c/1.webm"]).unwrap_err().contains("every copy"));
        assert!(check(&none, &["/a/2.mkv", "/b/2.mkv"]).unwrap_err().contains("every copy"));

        // Copies trashed earlier don't count as kept, and can't be trashed twice.
        let gone: HashSet<PathBuf> = paths(&["/a/1.mkv"]).into_iter().collect();
        assert!(check(&gone, &["/b/1.mp4"]).is_ok());
        assert!(check(&gone, &["/b/1.mp4", "/c/1.webm"]).unwrap_err().contains("every copy"));
        assert!(check(&gone, &["/a/1.mkv"]).unwrap_err().contains("in the trash already"));
    }
}
