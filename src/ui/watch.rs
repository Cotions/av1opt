//! Watched folders: videos that turn up in them are queued without being asked for.
//!
//! A folder is looked at every few minutes (no file-change notifications: they don't reach
//! network and FUSE mounts). A video is queued once it has been seen unchanged at two looks
//! and not written to for a while, so a download in progress is left alone, and only if a scan
//! would call it a candidate. Nothing but queueing happens here: the queue's own pause and
//! hours decide when it is encoded, and originals are never touched.

use super::{App, jobs, now};
use crate::estimate::{Model, Verdict};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Seconds between two looks at a folder. A folder that takes long to walk (a big tree on a
/// slow mount) is looked at less often: see [`due`].
const LOOK_EVERY: u64 = 300;
/// A video is left alone until this long after it was last written to.
const SETTLED_AFTER: u64 = 120;
/// A video gone this long is forgotten. Not at once: a drive that is not mounted looks like
/// an empty folder, and its videos must not all be new when it is back.
const FORGET_AFTER: u64 = 30 * 86400;
/// The state file is written when something changed, and this often otherwise.
const SAVE_EVERY: u64 = 86400;
/// Version of `ui-watch.json`.
const STATE_FORMAT: u64 = 1;

#[derive(Serialize, Deserialize, Clone)]
pub struct Watch {
    pub id: u64,
    pub path: PathBuf,
    pub out_dir: PathBuf,
    pub options: jobs::JobOptions,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
enum Mark {
    /// Seen, not judged yet: it may still be written to.
    Fresh,
    Queued,
    /// Judged and not worth encoding (AV1 already, too little to gain, unreadable).
    Left,
    /// It was there when the folder was added, and only new videos were asked for.
    Before,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
struct Seen {
    size: u64,
    mtime: u64,
    mark: Mark,
    /// When a look last found it.
    seen: u64,
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct Folder {
    #[serde(default)]
    files: BTreeMap<PathBuf, Seen>,
    #[serde(default)]
    looked: Option<u64>,
    #[serde(default)]
    error: Option<String>,
    /// The videos found at the first look are not queued.
    #[serde(default)]
    skip_existing: bool,
    /// How long the last walk through the folder took.
    #[serde(default)]
    walk_secs: f64,
}

#[derive(Default)]
pub struct State {
    folders: Mutex<BTreeMap<u64, Folder>>,
    /// One look at a time, whoever asks.
    looking: Mutex<()>,
    /// Set to have the watcher look again without waiting.
    wake: (Mutex<bool>, Condvar),
    saved: Mutex<u64>,
}

/// Brings what is known of a folder up to date with the videos `found` in it now (path, size,
/// modification time), and returns those ready to be judged: seen unchanged at an earlier
/// look and not written to lately.
fn settle(files: &mut BTreeMap<PathBuf, Seen>, found: Vec<(PathBuf, u64, u64)>, now: u64, first: Mark) -> (Vec<PathBuf>, bool) {
    let (mut ready, mut changed) = (Vec::new(), false);
    for (path, size, mtime) in found {
        match files.get_mut(&path) {
            Some(s) if s.size == size && s.mtime == mtime => {
                s.seen = now;
                // A date in the future (a clock that was wrong) would never be long ago.
                if s.mark == Mark::Fresh && (mtime > now || now - mtime >= SETTLED_AFTER) {
                    ready.push(path);
                }
            }
            // Another file under the same name is a new video, whatever became of the old one.
            Some(s) => {
                *s = Seen { size, mtime, mark: Mark::Fresh, seen: now };
                changed = true;
            }
            None => {
                files.insert(path, Seen { size, mtime, mark: first, seen: now });
                changed = true;
            }
        }
    }
    let before = files.len();
    files.retain(|_, s| now.saturating_sub(s.seen) < FORGET_AFTER);
    (ready, changed || files.len() != before)
}

/// Every video below `root`, leaving out the output folder, hidden folders (temp and work
/// folders, trash) and what a file system keeps for itself.
fn videos(root: &Path, out_dir: &Path) -> Result<Vec<(PathBuf, u64, u64)>> {
    std::fs::read_dir(root).with_context(|| format!("cannot read {}", root.display()))?;
    let mut found = Vec::new();
    let walk = walkdir::WalkDir::new(root).into_iter().filter_entry(|e| {
        let name = e.file_name().to_string_lossy();
        let hidden = e.depth() > 0 && name.starts_with('.');
        let system = matches!(name.as_ref(), "System Volume Information" | "$RECYCLE.BIN" | "lost+found");
        !hidden && !(e.file_type().is_dir() && (system || e.path().starts_with(out_dir)))
    });
    for e in walk.flatten() {
        // A name that is not text could not be written to the state file.
        if !e.file_type().is_file() || !crate::scan::is_video(e.path()) || e.path().to_str().is_none() {
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        let mtime = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
        found.push((e.into_path(), meta.len(), mtime));
    }
    Ok(found)
}

/// Whether it is time to look at a folder again: every [`LOOK_EVERY`] seconds, or ten times
/// as long as the last walk took if that is more, so walking never takes a tenth of the time.
fn due(f: Option<&Folder>, now: u64) -> bool {
    match f.and_then(|f| f.looked.map(|t| (t, f.walk_secs))) {
        None => true,
        Some((looked, walk)) => now.saturating_sub(looked) >= LOOK_EVERY.max((walk * 10.0) as u64),
    }
}

fn state_path() -> Option<PathBuf> {
    super::library::settings_path().map(|p| p.with_file_name("ui-watch.json"))
}

fn save(app: &App, force: bool) {
    let mut saved = app.watch.saved.lock().unwrap();
    if !force && now().saturating_sub(*saved) < SAVE_EVERY {
        return;
    }
    let Some(p) = state_path() else { return };
    let doc = json!({ "format": STATE_FORMAT, "folders": *app.watch.folders.lock().unwrap() });
    let written = p.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| crate::files::replace_with(&p, &serde_json::to_vec(&doc).unwrap_or_default()));
    match written {
        Ok(()) => *saved = now(),
        Err(e) => super::note(&format!("watched folders not saved: cannot write {}: {e}", p.display())),
    }
}

fn load(app: &App) {
    let Some(doc) = state_path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else { return };
    // A file from a newer av1opt is set aside and not used: what it means may have changed.
    let format = doc["format"].as_u64().unwrap_or(0);
    if format > STATE_FORMAT {
        eprintln!("warning: ui-watch.json is from a newer av1opt; watched folders start from what is in them now");
        if let Some(p) = state_path() {
            let _ = std::fs::rename(&p, p.with_file_name(format!("ui-watch.format{format}.json")));
        }
        return;
    }
    if let Ok(folders) = serde_json::from_value::<BTreeMap<u64, Folder>>(doc["folders"].clone()) {
        *app.watch.folders.lock().unwrap() = folders;
    }
}

fn find(app: &App, id: u64) -> Result<Watch> {
    app.settings.lock().unwrap().watches.iter().find(|w| w.id == id).cloned().context("not found: no such watched folder")
}

/// The videos among `ready` that are worth encoding, and the rest.
fn judge(app: &App, w: &Watch, ready: Vec<PathBuf>) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    // With broken tools every video would look unreadable and be left for good.
    if let Some(p) = app.encode_problem() {
        bail!("{p} (see Settings)");
    }
    let model = Model::fit(&crate::history::load(), w.options.target_vmaf);
    let (results, _) = crate::scan::collect(&ready, &app.ffprobe(), app.cfg.jobs, w.options.min_savings, &model, &|_, _| {})?;
    // "Encode anyway" is about the gain expected. AV1 is left even so: an output that took
    // its original's place in this folder would be encoded again, and again after that.
    let worth = |v: Verdict| v == Verdict::Candidate || (w.options.force && !matches!(v, Verdict::AlreadyAv1 | Verdict::Unreadable));
    let (worth, rest): (Vec<_>, Vec<_>) = results.into_iter().partition(|r| worth(r.verdict));
    Ok((worth.into_iter().map(|r| r.path).collect(), rest.into_iter().map(|r| r.path).collect()))
}

/// Looks at one folder now: notes what is new, and queues what has settled and is worth it.
pub fn look(app: &App, id: u64) -> Result<()> {
    let _one = app.watch.looking.lock().unwrap();
    let w = find(app, id)?;
    let started = std::time::Instant::now();
    let found = videos(&w.path, &w.out_dir);
    let walk_secs = started.elapsed().as_secs_f64();
    let at = now();
    let (ready, mut changed) = {
        let mut folders = app.watch.folders.lock().unwrap();
        // Nothing known of a folder that is watched: the state file was lost. Its videos
        // are then taken as there before, not as so many new ones to encode.
        let f = folders.entry(id).or_insert_with(|| Folder { skip_existing: true, ..Default::default() });
        f.looked = Some(at);
        f.walk_secs = walk_secs;
        match found {
            Ok(found) => {
                let first = if std::mem::take(&mut f.skip_existing) { Mark::Before } else { Mark::Fresh };
                let had_error = f.error.take().is_some();
                let (ready, changed) = settle(&mut f.files, found, at, first);
                (ready, changed || had_error)
            }
            Err(e) => {
                f.error = Some(format!("{e:#}"));
                (Vec::new(), true)
            }
        }
    };
    if !ready.is_empty() {
        // Already waiting in the queue, put there by hand.
        let (waiting, ready): (Vec<_>, Vec<_>) = ready.into_iter().partition(|p| jobs::waiting_on(app, p));
        let judged = judge(app, &w, ready).and_then(|(worth, rest)| {
            if !worth.is_empty() {
                jobs::add(app, &json!({ "inputs": worth, "out_dir": w.out_dir, "roots": [w.path], "options": w.options, "watch": w.path }))?;
            }
            Ok((worth, rest))
        });
        let mut folders = app.watch.folders.lock().unwrap();
        let f = folders.entry(id).or_default();
        let mut mark = |paths: Vec<PathBuf>, m: Mark| {
            for p in paths {
                if let Some(s) = f.files.get_mut(&p) {
                    s.mark = m;
                }
            }
        };
        mark(waiting, Mark::Queued);
        match judged {
            Ok((worth, rest)) => {
                mark(worth, Mark::Queued);
                mark(rest, Mark::Left);
            }
            // They stay fresh and are tried again at the next look.
            Err(e) => f.error = Some(format!("{e:#}")),
        }
        changed = true;
    }
    save(app, changed);
    app.broadcast(json!({ "kind": "watch" }));
    Ok(())
}

/// Starts the watcher: looks at every enabled folder when it is due.
pub fn start(app: Arc<App>) {
    load(&app);
    std::thread::spawn(move || {
        loop {
            if jobs::shutting_down(&app) {
                return;
            }
            let watches: Vec<Watch> = app.settings.lock().unwrap().watches.iter().filter(|w| w.enabled).cloned().collect();
            for w in watches {
                let is_due = due(app.watch.folders.lock().unwrap().get(&w.id), now());
                if is_due && !jobs::shutting_down(&app) {
                    let _ = look(&app, w.id);
                }
            }
            let (flag, cond) = &app.watch.wake;
            let mut woken = cond.wait_timeout_while(flag.lock().unwrap(), Duration::from_secs(20), |w| !*w).unwrap().0;
            *woken = false;
        }
    });
}

fn wake(app: &App) {
    let (flag, cond) = &app.watch.wake;
    *flag.lock().unwrap() = true;
    cond.notify_all();
}

pub fn list(app: &App) -> Value {
    let watches = app.settings.lock().unwrap().watches.clone();
    let folders = app.watch.folders.lock().unwrap();
    let at = now();
    let rows: Vec<Value> = watches
        .iter()
        .map(|w| {
            let f = folders.get(&w.id);
            let count = |m: Mark| f.map_or(0, |f| f.files.values().filter(|s| s.mark == m).count());
            let every = f.map_or(LOOK_EVERY, |f| LOOK_EVERY.max((f.walk_secs * 10.0) as u64));
            json!({
                "id": w.id, "path": w.path, "out_dir": w.out_dir, "options": w.options, "enabled": w.enabled,
                "looked": f.and_then(|f| f.looked),
                "next_secs": f.and_then(|f| f.looked).map(|t| (t + every).saturating_sub(at)),
                "error": f.and_then(|f| f.error.clone()),
                "videos": f.map_or(0, |f| f.files.len()),
                "fresh": count(Mark::Fresh), "queued": count(Mark::Queued), "left": count(Mark::Left), "before": count(Mark::Before),
            })
        })
        .collect();
    json!({ "watches": rows, "every_secs": LOOK_EVERY, "settled_secs": SETTLED_AFTER })
}

#[derive(Deserialize)]
struct AddRequest {
    path: PathBuf,
    out_dir: PathBuf,
    #[serde(default)]
    options: Option<jobs::JobOptions>,
    /// Queue the videos already there too, not only those that come later.
    #[serde(default)]
    existing: bool,
}

pub fn add(app: &App, body: &Value) -> Result<Value> {
    let r: AddRequest = serde_json::from_value(body.clone())?;
    let path = std::fs::canonicalize(&r.path).with_context(|| format!("{} does not exist", r.path.display()))?;
    if !path.is_dir() {
        bail!("{} is not a folder", path.display());
    }
    if !r.out_dir.is_absolute() {
        bail!("pick an output folder");
    }
    crate::fsguard::ensure_writable_location(&r.out_dir, false)?;
    if r.out_dir == path {
        bail!("the output folder cannot be the watched folder itself");
    }
    let options = r.options.unwrap_or_else(|| serde_json::from_value(json!({})).expect("defaults"));
    if !options.remove.is_empty() {
        bail!("sections can only be removed when encoding a single video");
    }
    jobs::check_options(app, &options)?;
    let id = {
        let mut s = app.settings.lock().unwrap();
        if s.watches.iter().any(|w| w.path == path) {
            bail!("{} is watched already", path.display());
        }
        let id = s.watches.iter().map(|w| w.id).max().unwrap_or(0) + 1;
        s.watches.push(Watch { id, path, out_dir: r.out_dir, options, enabled: true });
        id
    };
    app.watch.folders.lock().unwrap().insert(id, Folder { skip_existing: !r.existing, ..Default::default() });
    app.save_settings();
    save(app, true);
    wake(app);
    app.broadcast(json!({ "kind": "watch" }));
    Ok(json!({ "id": id }))
}

pub fn remove(app: &App, id: u64) -> Result<Value> {
    find(app, id)?;
    app.settings.lock().unwrap().watches.retain(|w| w.id != id);
    app.watch.folders.lock().unwrap().remove(&id);
    app.save_settings();
    save(app, true);
    app.broadcast(json!({ "kind": "watch" }));
    Ok(list(app))
}

pub fn set_enabled(app: &App, id: u64, body: &Value) -> Result<Value> {
    let enabled = body["enabled"].as_bool().context("enabled: true or false")?;
    find(app, id)?;
    if let Some(w) = app.settings.lock().unwrap().watches.iter_mut().find(|w| w.id == id) {
        w.enabled = enabled;
    }
    app.save_settings();
    if enabled {
        wake(app);
    }
    app.broadcast(json!({ "kind": "watch" }));
    Ok(list(app))
}

/// Looks now, whatever the clock says, and answers when it has looked.
pub fn look_now(app: &App, id: u64) -> Result<Value> {
    look(app, id)?;
    Ok(list(app))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(marks: &BTreeMap<PathBuf, Seen>) -> Vec<(&str, Mark)> {
        marks.iter().map(|(p, s)| (p.to_str().unwrap(), s.mark)).collect()
    }

    #[test]
    fn a_video_is_ready_once_it_has_stopped_changing() {
        let mut known = BTreeMap::new();
        let v = |size, mtime| vec![(PathBuf::from("/w/a.mp4"), size, mtime)];
        // Still downloading: larger at every look.
        assert_eq!(settle(&mut known, v(10, 1000), 1010, Mark::Fresh), (vec![], true));
        assert_eq!(settle(&mut known, v(20, 1300), 1310, Mark::Fresh), (vec![], true));
        // Unchanged since the last look, but written to a moment ago.
        assert_eq!(settle(&mut known, v(20, 1300), 1400, Mark::Fresh), (vec![], false));
        assert_eq!(settle(&mut known, v(20, 1300), 1420, Mark::Fresh), (vec![PathBuf::from("/w/a.mp4")], false));
        // Once queued or left it is not ready again, until another file takes its name.
        known.get_mut(Path::new("/w/a.mp4")).unwrap().mark = Mark::Queued;
        assert_eq!(settle(&mut known, v(20, 1300), 2000, Mark::Fresh), (vec![], false));
        assert_eq!(settle(&mut known, v(30, 2500), 2600, Mark::Fresh), (vec![], true));
        assert_eq!(files(&known), [("/w/a.mp4", Mark::Fresh)]);
        assert_eq!(settle(&mut known, v(30, 2500), 2700, Mark::Fresh).0, [PathBuf::from("/w/a.mp4")]);
    }

    #[test]
    fn videos_there_before_are_not_queued_and_a_missing_drive_forgets_nothing() {
        let mut known = BTreeMap::new();
        let old = vec![(PathBuf::from("/w/old.mp4"), 5, 100)];
        assert_eq!(settle(&mut known, old.clone(), 10_000, Mark::Before), (vec![], true));
        assert_eq!(settle(&mut known, old.clone(), 10_300, Mark::Fresh), (vec![], false));
        assert_eq!(files(&known), [("/w/old.mp4", Mark::Before)]);
        // The drive is away for a week: the folder is empty, then everything is back.
        assert_eq!(settle(&mut known, vec![], 20_000, Mark::Fresh), (vec![], false));
        assert_eq!(settle(&mut known, old.clone(), 10_000 + 7 * 86400, Mark::Fresh), (vec![], false));
        assert_eq!(files(&known), [("/w/old.mp4", Mark::Before)]);
        // Gone for good: forgotten after a month, and new if it ever comes back.
        assert_eq!(settle(&mut known, vec![], 10_000 + 40 * 86400, Mark::Fresh), (vec![], true));
        assert!(known.is_empty());
    }

    #[test]
    fn a_slow_folder_is_looked_at_less_often() {
        let f = |looked, walk_secs| Folder { looked: Some(looked), walk_secs, ..Default::default() };
        assert!(due(None, 50));
        assert!(due(Some(&Folder::default()), 50));
        assert!(!due(Some(&f(1000, 0.2)), 1299));
        assert!(due(Some(&f(1000, 0.2)), 1300));
        // A walk of two minutes: every twenty.
        assert!(!due(Some(&f(1000, 120.0)), 2199));
        assert!(due(Some(&f(1000, 120.0)), 2200));
    }
}
