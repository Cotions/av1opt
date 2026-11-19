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
/// Looks in a row at which a video whose probe did not get through is tried. After these it
/// is tried at growing intervals, and never given up on: a mount with read errors says nothing
/// about the video.
const PROBE_TRIES: u32 = 3;
/// The longest wait between two tries of such a video.
const LONGEST_WAIT: u64 = 86400;
/// How many of those videos the page is told of by name.
const UNREAD_LISTED: usize = 20;
/// How many of those videos are tried again at once. Each try can take as long as a probe may, and
/// one watcher looks at every folder: thousands of them on a drive still away would hold it for
/// hours. A look goes on to the next ones while these get through.
const RETRIES_PER_ROUND: usize = 20;
/// How long a look goes on with such rounds. Thousands of videos come back from a drive that is
/// back again within a few looks, and the other watched folders still get their turn.
const RETRY_TIME: Duration = Duration::from_secs(120);
/// The same for a look someone asked for and is waiting for.
const RETRY_TIME_BY_HAND: Duration = Duration::from_secs(30);

#[derive(Serialize, Deserialize, Clone)]
pub struct Watch {
    pub id: u64,
    pub path: PathBuf,
    pub out_dir: PathBuf,
    pub options: jobs::JobOptions,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// The output folder was made for this watch once. After that a missing one means a drive
    /// that is away. Watches saved before folders were made when added lack this.
    #[serde(default)]
    pub out_dir_made: bool,
    /// Where the output folder was when it was made or first seen: see `batch::OutDirPlace`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_dir_place: Option<crate::batch::OutDirPlace>,
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

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
struct Seen {
    size: u64,
    mtime: u64,
    mark: Mark,
    /// When a look last found it.
    seen: u64,
    /// Probes of this file that could not run or were cut short.
    #[serde(default)]
    tries: u32,
    /// Why the last of them did not get through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    why: Option<String>,
    /// Not tried again before this time.
    #[serde(default)]
    next_try: u64,
}

impl Seen {
    fn new(size: u64, mtime: u64, mark: Mark, seen: u64) -> Seen {
        Seen { size, mtime, mark, seen, tries: 0, why: None, next_try: 0 }
    }

    /// Waiting to be read: no probe of it has got through yet.
    fn unread(&self) -> bool {
        self.mark == Mark::Fresh && self.tries > 0
    }
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
    /// When the folder was last walked through to its end.
    #[serde(default)]
    found_at: Option<u64>,
    /// The error is that the output folder is away or cannot be relied on.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    out_dir_away: bool,
    /// It is there, and only a person can say that it is the right one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    out_dir_to_confirm: bool,
}

impl Folder {
    /// Notes what the last look ran into, or that it ran into nothing.
    fn note_problem(&mut self, problem: Option<&anyhow::Error>) {
        self.error = problem.map(|e| format!("{e:#}"));
        self.out_dir_away = problem.is_some_and(crate::batch::is_out_dir_problem);
        self.out_dir_to_confirm = problem.is_some_and(crate::batch::out_dir_needs_confirming);
    }

    /// The videos waiting to be read. Only those the latest look found: what is known of the
    /// others is kept for a drive that comes back, but nothing of theirs is being waited for.
    fn unread(&self) -> impl Iterator<Item = (&PathBuf, &Seen)> {
        self.found().filter(|(_, s)| s.unread())
    }

    /// The videos the latest look found.
    fn found(&self) -> impl Iterator<Item = (&PathBuf, &Seen)> {
        self.files.iter().filter(|(_, s)| self.found_at.is_none_or(|at| s.seen >= at))
    }
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
    // A folder with nothing in it at all is more likely a drive that is not mounted than a
    // library that was emptied (as in a scan): what is known of it stays, however long.
    let forget = !found.is_empty();
    for (path, size, mtime) in found {
        match files.get_mut(&path) {
            Some(s) if s.size == size && s.mtime == mtime => {
                s.seen = now;
                // A date in the future (a clock that was wrong) would never be long ago.
                if s.mark == Mark::Fresh && now >= s.next_try && (mtime > now || now - mtime >= SETTLED_AFTER) {
                    ready.push(path);
                }
            }
            // Another file under the same name is a new video, whatever became of the old one.
            Some(s) => {
                *s = Seen::new(size, mtime, Mark::Fresh, now);
                changed = true;
            }
            None => {
                files.insert(path, Seen::new(size, mtime, first, now));
                changed = true;
            }
        }
    }
    let before = files.len();
    if forget {
        files.retain(|_, s| now.saturating_sub(s.seen) < FORGET_AFTER);
    }
    (ready, changed || files.len() != before)
}

/// The videos a look is to judge.
#[derive(Debug, PartialEq, Default)]
struct Tries {
    /// Never tried.
    first_time: Vec<PathBuf>,
    /// Their probe did not get through before, those that have waited longest first.
    again: Vec<PathBuf>,
}

/// Sorts `ready` into videos never tried and those whose probe did not get through. Those not
/// reached by a look keep their place and their waits, and come up at a later one. Asked for by
/// hand (`by_hand`), videos still within their wait count as ready too, so that "Look now" is of
/// use.
fn tries_of(files: &BTreeMap<PathBuf, Seen>, ready: Vec<PathBuf>, now: u64, by_hand: bool) -> Tries {
    let (mut retries, first_time): (Vec<_>, Vec<_>) = ready.into_iter().partition(|p| files.get(p).is_some_and(Seen::unread));
    if by_hand {
        retries.extend(files.iter().filter(|(_, s)| s.unread() && s.seen == now && s.next_try > now).map(|(p, _)| p.clone()));
    }
    // When a video's wait began is when it was last tried.
    let last_tried = |p: &PathBuf| files.get(p).map_or(0, |s| s.next_try.saturating_sub(wait_after(s.tries)));
    retries.sort_by_cached_key(last_tried);
    Tries { first_time, again: retries }
}

/// Judges `tries` in rounds: those never tried with the first videos to try again, then more of
/// those for as long as most of the last round got through, `time` lasts and `stop` does not say
/// otherwise. What the rounds before a failing one found out is kept, and returned with the error.
fn in_rounds(tries: Tries, time: Duration, stop: impl Fn() -> bool, mut judge_round: impl FnMut(Vec<PathBuf>) -> Result<Judged>) -> (Judged, Option<anyhow::Error>) {
    let started = std::time::Instant::now();
    let Tries { first_time, again } = tries;
    let mut round = first_time;
    let mut rounds = again.chunks(RETRIES_PER_ROUND);
    let mut retried = rounds.next().unwrap_or_default();
    round.extend_from_slice(retried);
    let mut all = Judged::default();
    loop {
        let judged = match judge_round(round) {
            Ok(judged) => judged,
            Err(e) => return (all, Some(e)),
        };
        let still_failing = retried.iter().filter(|p| judged.unjudged.iter().any(|(failed, _)| failed == *p)).count();
        all.waiting.extend(judged.waiting);
        all.worth.extend(judged.worth);
        all.left.extend(judged.left);
        all.unjudged.extend(judged.unjudged);
        // A drive still away answers slowly if at all: the first round tells, and the look ends.
        if still_failing * 2 >= retried.len() || started.elapsed() >= time || stop() {
            return (all, None);
        }
        let Some(next) = rounds.next() else { return (all, None) };
        retried = next;
        round = next.to_vec();
    }
}

/// How long a video waits after its probe failed `tries` times: nothing for the first looks,
/// then twice as long each time, up to [`LONGEST_WAIT`].
fn wait_after(tries: u32) -> u64 {
    match tries.checked_sub(PROBE_TRIES) {
        None => 0,
        Some(over) => LOOK_EVERY.saturating_mul(2u64.saturating_pow(over.saturating_add(1))).min(LONGEST_WAIT),
    }
}

/// Counts a probe that did not get through for each of these videos, with the reason. They stay
/// fresh, and are tried again when their wait is over.
fn count_failed_probes(files: &mut BTreeMap<PathBuf, Seen>, failed: Vec<(PathBuf, String)>, now: u64) {
    for (path, why) in failed {
        if let Some(s) = files.get_mut(&path) {
            s.tries = s.tries.saturating_add(1);
            s.why = Some(why);
            s.next_try = now + wait_after(s.tries);
        }
    }
}

/// Earlier versions left a video for good after its tries. It goes back to being tried.
fn take_back_given_up(folders: &mut BTreeMap<u64, Folder>) {
    for s in folders.values_mut().flat_map(|f| f.files.values_mut()) {
        if s.mark == Mark::Left && s.tries >= PROBE_TRIES {
            s.mark = Mark::Fresh;
            s.next_try = 0;
        }
    }
}

/// Why videos could not go from `watched` into `out_dir`, if so: each one found would count as
/// an output already, or be written over its own folder.
fn output_problem(watched: &Path, out_dir: &Path) -> Result<()> {
    let out = crate::fsguard::resolved(out_dir).with_context(|| format!("cannot make out where {} is", out_dir.display()))?;
    let watched = crate::fsguard::resolved(watched).with_context(|| format!("cannot make out where {} is", watched.display()))?;
    if out == watched {
        bail!("the output folder cannot be the watched folder itself");
    }
    if watched.starts_with(&out) {
        bail!("the output folder {} contains the watched folder, so every video found there would count as an output; pick a folder outside it", out.display());
    }
    Ok(())
}

/// Every video below `root`, leaving out the output folder, hidden folders (temp and work
/// folders, trash) and what a file system keeps for itself. A root inside the output folder
/// does not get here: see [`output_problem`].
fn videos(root: &Path, out_dir: &Path) -> Result<Vec<(PathBuf, u64, u64)>> {
    std::fs::read_dir(root).with_context(|| format!("cannot read {}", root.display()))?;
    let out_dir = crate::fsguard::resolved(out_dir).unwrap_or_else(|_| out_dir.to_path_buf());
    let mut found = Vec::new();
    let walk = walkdir::WalkDir::new(root).into_iter().filter_entry(|e| {
        let name = e.file_name().to_string_lossy();
        let hidden = e.depth() > 0 && name.starts_with('.');
        let system = matches!(name.as_ref(), "System Volume Information" | "$RECYCLE.BIN" | "lost+found");
        let output = e.path().starts_with(&out_dir);
        !hidden && !(e.file_type().is_dir() && (system || output))
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
        Err(e) => eprintln!("watched folders not saved: cannot write {}: {e}", p.display()),
    }
}

fn load(app: &App) {
    let Some(path) = state_path() else { return };
    let why = match std::fs::read(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => e.to_string(),
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Err(e) => e.to_string(),
            Ok(doc) => {
                // A file from a newer av1opt is set aside and not used: what it means may have changed.
                let format = doc["format"].as_u64().unwrap_or(0);
                if format > STATE_FORMAT {
                    eprintln!("warning: ui-watch.json is from a newer av1opt; watched folders start from what is in them now");
                    let _ = std::fs::rename(&path, path.with_file_name(format!("ui-watch.format{format}.json")));
                    return;
                }
                match serde_json::from_value::<BTreeMap<u64, Folder>>(doc["folders"].clone()) {
                    Ok(mut folders) => {
                        take_back_given_up(&mut folders);
                        *app.watch.folders.lock().unwrap() = folders;
                        return;
                    }
                    Err(e) => e.to_string(),
                }
            }
        },
    };
    // Without it the first look takes what is in the folders for what was there before, so
    // nothing gets queued for this; videos that were waiting to be queued are passed over.
    let told = super::set_aside(&path, "the record of what is in the watched folders", &why, "the videos now in them count as seen before and are not queued");
    app.notices.lock().unwrap().push(told);
}

fn find(app: &App, id: u64) -> Result<Watch> {
    app.settings.lock().unwrap().watches.iter().find(|w| w.id == id).cloned().context("not found: no such watched folder")
}

/// What became of the videos that were ready.
#[derive(Default)]
struct Judged {
    /// Already waiting in the queue, put there by hand.
    waiting: Vec<PathBuf>,
    worth: Vec<PathBuf>,
    left: Vec<PathBuf>,
    /// Their probe did not get through, and why.
    unjudged: Vec<(PathBuf, String)>,
}

/// Makes the output folder of a watch saved before folders were made when a watch was added,
/// and notes where it is. Once: from then on a folder missing or elsewhere means a drive that is
/// away.
fn make_output_once(app: &App, w: &mut Watch) -> Result<()> {
    let saved_place = w.out_dir_place.take();
    w.out_dir_place = saved_place.clone().filter(crate::batch::OutDirPlace::can_be_relied_on);
    if w.out_dir_made && w.out_dir_place.is_some() && w.out_dir_place == saved_place {
        return Ok(());
    }
    let made_now = !w.out_dir_made;
    let mut made_here = false;
    if made_now {
        crate::fsguard::ensure_writable_location(&w.out_dir, false)?;
        made_here = crate::batch::make_out_dir_once(&w.out_dir)?;
        w.out_dir_made = true;
    }
    if w.out_dir_place.is_none() {
        w.out_dir_place = if made_here { crate::batch::OutDirPlace::confirmed_of(&w.out_dir) } else { crate::batch::OutDirPlace::first_seen(&w.out_dir) };
    }
    // A folder that was made once and says nothing now is looked at again next time.
    if !made_now && w.out_dir_place == saved_place {
        return Ok(());
    }
    remember_output(app, w);
    Ok(())
}

/// Saves what is known of a watch's output folder.
fn remember_output(app: &App, w: &Watch) {
    if let Some(saved) = app.settings.lock().unwrap().watches.iter_mut().find(|s| s.id == w.id) {
        saved.out_dir_made = w.out_dir_made;
        saved.out_dir_place.clone_from(&w.out_dir_place);
    }
    app.save_settings();
}

/// A person says the output folder of a watch is the right one, wherever it is now.
pub fn use_output_folder(app: &App, id: u64) -> Result<Value> {
    let mut w = find(app, id)?;
    crate::fsguard::ensure_writable_location(&w.out_dir, false)?;
    let Some(place) = crate::batch::confirm_out_dir(&w.out_dir)? else {
        return Err(crate::batch::out_dir_problem(&w.out_dir, None).unwrap_or_else(|| anyhow::anyhow!("the output folder {} cannot be used now", w.out_dir.display())));
    };
    w.out_dir_made = true;
    w.out_dir_place = Some(place);
    remember_output(app, &w);
    look(app, id, false)?;
    Ok(list(app))
}

/// Sorts `ready` into the videos worth encoding, those to leave and those it cannot tell of yet.
fn judge(app: &App, w: &Watch, model: &Model, ready: Vec<PathBuf>) -> Result<Judged> {
    // With broken tools every video would look unreadable and be left for good.
    if let Some(p) = app.encode_problem() {
        bail!("{p} (see Settings)");
    }
    // A job queued now could only fail. The videos wait until the folder is back.
    if let Some(problem) = crate::batch::out_dir_problem(&w.out_dir, w.out_dir_place.as_ref()) {
        return Err(problem);
    }
    let (waiting, ready): (Vec<_>, Vec<_>) = ready.into_iter().partition(|p| jobs::waiting_on(app, p));
    let (results, _) = crate::scan::collect(&ready, &app.ffprobe(), app.cfg.jobs, w.options.min_savings, model, &|_, _| {})?;
    // "Encode anyway" is about the gain expected. AV1 is left even so: an output that took
    // its original's place in this folder would be encoded again, and again after that.
    let worth = |v: Verdict| v.will_be_tried() || (w.options.force && !matches!(v, Verdict::AlreadyAv1 | Verdict::Unreadable));
    let mut judged = Judged { waiting, ..Default::default() };
    for r in results {
        if r.unjudged {
            judged.unjudged.push((r.path, r.error.unwrap_or_else(|| "the probe did not finish".into())));
        } else if worth(r.verdict) {
            judged.worth.push(r.path);
        } else {
            judged.left.push(r.path);
        }
    }
    Ok(judged)
}

/// Looks at one folder now: notes what is new, and queues what has settled and is worth it.
/// Asked for `by_hand`, those that have waited longest for their next try get it now.
fn look(app: &App, id: u64, by_hand: bool) -> Result<()> {
    let _one = app.watch.looking.lock().unwrap();
    let mut w = find(app, id)?;
    let started = std::time::Instant::now();
    // Also for a watch saved before this was refused: it stays listed and says what is wrong.
    let found = output_problem(&w.path, &w.out_dir).and_then(|()| make_output_once(app, &mut w)).and_then(|()| videos(&w.path, &w.out_dir));
    let walk_secs = started.elapsed().as_secs_f64();
    // Said even with nothing to queue, so that the drive is seen to be away before it matters.
    let output_away = found.as_ref().ok().and_then(|_| crate::batch::out_dir_problem(&w.out_dir, w.out_dir_place.as_ref()));
    let at = now();
    let (tries, mut changed) = {
        let mut folders = app.watch.folders.lock().unwrap();
        // Nothing known of a folder that is watched: the state file was lost. Its videos
        // are then taken as there before, not as so many new ones to encode.
        let f = folders.entry(id).or_insert_with(|| Folder { skip_existing: true, ..Default::default() });
        f.looked = Some(at);
        f.walk_secs = walk_secs;
        match found {
            Ok(found) => {
                let first = if std::mem::take(&mut f.skip_existing) { Mark::Before } else { Mark::Fresh };
                let had_error = f.error.take();
                let (ready, changed) = settle(&mut f.files, found, at, first);
                f.found_at = Some(at);
                f.note_problem(output_away.as_ref());
                // With the output folder away the videos wait, as a job queued now could only fail.
                let tries = if f.error.is_some() { Tries::default() } else { tries_of(&f.files, ready, at, by_hand) };
                (tries, changed || had_error != f.error)
            }
            Err(e) => {
                f.note_problem(Some(&e));
                (Tries::default(), true)
            }
        }
    };
    if tries != Tries::default() {
        let model = Model::fit_at_preset(&crate::history::load(), w.options.target_vmaf, w.options.preset);
        let time = if by_hand { RETRY_TIME_BY_HAND } else { RETRY_TIME };
        let (judged, problem) = in_rounds(tries, time, || jobs::shutting_down(app), |round| judge(app, &w, &model, round));
        // One job for all the rounds, so that one look does not fill the queue with many.
        let queued = if judged.worth.is_empty() {
            Ok(())
        } else {
            jobs::add(app, &json!({ "inputs": judged.worth, "out_dir": w.out_dir, "roots": [w.path], "options": w.options, "watch": w.path, "out_dir_place": w.out_dir_place })).map(drop)
        };
        let mut folders = app.watch.folders.lock().unwrap();
        let f = folders.entry(id).or_default();
        let mut mark = |paths: Vec<PathBuf>, m: Mark| {
            for p in paths {
                if let Some(s) = f.files.get_mut(&p) {
                    *s = Seen::new(s.size, s.mtime, m, s.seen);
                }
            }
        };
        mark(judged.waiting, Mark::Queued);
        mark(judged.left, Mark::Left);
        // Those not queued stay fresh and are tried again at the next look.
        let problem = match queued {
            Ok(()) => {
                mark(judged.worth, Mark::Queued);
                problem
            }
            Err(e) => Some(e),
        };
        // Listed with their reasons by `list`: nothing is wrong with the folder.
        count_failed_probes(&mut f.files, judged.unjudged, at);
        if let Some(e) = problem {
            f.note_problem(Some(&e));
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
                    let _ = look(&app, w.id, false);
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
            let next_look = f.and_then(|f| f.looked).map_or(at, |t| t + every);
            let unread = || f.into_iter().flat_map(Folder::unread);
            let fresh = f.map_or(0, |f| f.found().filter(|(_, s)| s.mark == Mark::Fresh && !s.unread()).count());
            // A try happens at the first look after its wait is over.
            let unread_files: Vec<Value> = unread()
                .take(UNREAD_LISTED)
                .map(|(path, s)| json!({ "path": path, "tries": s.tries, "why": s.why, "next_secs": s.next_try.max(next_look).saturating_sub(at) }))
                .collect();
            json!({
                "id": w.id, "path": w.path, "out_dir": w.out_dir, "options": w.options, "enabled": w.enabled,
                "looked": f.and_then(|f| f.looked),
                "next_secs": f.and_then(|f| f.looked).map(|t| (t + every).saturating_sub(at)),
                "error": f.and_then(|f| f.error.clone()),
                "out_dir_away": f.is_some_and(|f| f.out_dir_away),
                "out_dir_to_confirm": f.is_some_and(|f| f.out_dir_to_confirm),
                "videos": f.map_or(0, |f| f.found().count()),
                "fresh": fresh, "unread": unread().count(), "unread_files": unread_files,
                "queued": count(Mark::Queued), "left": count(Mark::Left), "before": count(Mark::Before),
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
    let path = std::fs::canonicalize(&r.path).with_context(|| format!("{} does not exist", r.path.display())).and_then(super::text_path)?;
    if !path.is_dir() {
        bail!("{} is not a folder", path.display());
    }
    if !r.out_dir.is_absolute() {
        bail!("pick an output folder");
    }
    crate::fsguard::ensure_writable_location(&r.out_dir, false)?;
    output_problem(&path, &r.out_dir)?;
    let options = r.options.unwrap_or_else(|| serde_json::from_value(json!({})).expect("defaults"));
    if !options.remove.is_empty() {
        bail!("sections can only be removed when encoding a single video");
    }
    jobs::check_options(app, &options)?;
    if app.settings.lock().unwrap().watches.iter().any(|w| w.path == path) {
        bail!("{} is watched already", path.display());
    }
    // Made now, while someone is here to choose it. From here on a missing folder means a
    // drive that is away, and the jobs of this watch will not make one in its place.
    if let Some(problem) = crate::batch::under_unmounted_mount_point(&r.out_dir) {
        return Err(problem);
    }
    std::fs::create_dir_all(&r.out_dir).with_context(|| format!("cannot create {}", r.out_dir.display()))?;
    let out_dir_place = crate::batch::OutDirPlace::confirmed_of(&r.out_dir);
    let id = {
        let mut s = app.settings.lock().unwrap();
        if s.watches.iter().any(|w| w.path == path) {
            bail!("{} is watched already", path.display());
        }
        let id = s.watches.iter().map(|w| w.id).max().unwrap_or(0) + 1;
        s.watches.push(Watch { id, path, out_dir: r.out_dir, options, enabled: true, out_dir_made: true, out_dir_place });
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

/// Looks now, whatever the clock says, videos waiting for another try included, and answers
/// when it has looked.
pub fn look_now(app: &App, id: u64) -> Result<Value> {
    look(app, id, true)?;
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
        // Away for longer than a month, with nothing else there: still nothing is forgotten.
        assert_eq!(settle(&mut known, vec![], 10_000 + 400 * 86400, Mark::Fresh), (vec![], false));
        assert_eq!(settle(&mut known, old.clone(), 10_000 + 401 * 86400, Mark::Fresh), (vec![], false));
        assert_eq!(files(&known), [("/w/old.mp4", Mark::Before)]);
        // Gone for good while the folder lives on: forgotten after a month.
        let other = vec![(PathBuf::from("/w/other.mp4"), 5, 100)];
        assert_eq!(settle(&mut known, other, 10_000 + 440 * 86400, Mark::Before), (vec![], true));
        assert_eq!(files(&known), [("/w/other.mp4", Mark::Before)]);
    }

    #[test]
    fn a_video_whose_probe_fails_is_tried_less_and_less_often_and_never_left() {
        let path = PathBuf::from("/w/a.mp4");
        let found = || vec![(PathBuf::from("/w/a.mp4"), 5, 100)];
        let failed = || vec![(PathBuf::from("/w/a.mp4"), "read error".to_string())];
        let mut known = BTreeMap::new();
        settle(&mut known, found(), 1000, Mark::Fresh);
        // The first looks follow each other.
        let mut at = 1000;
        for look in 1..PROBE_TRIES {
            at += LOOK_EVERY;
            assert_eq!(settle(&mut known, found(), at, Mark::Fresh).0, std::slice::from_ref(&path), "look {look}");
            count_failed_probes(&mut known, failed(), at);
            assert_eq!(known[&path].next_try, at, "look {look}");
        }
        // After them the waits grow, up to a day, and it is still there to be tried.
        let mut waits = Vec::new();
        for _ in 0..12 {
            at += LOOK_EVERY;
            while settle(&mut known, found(), at, Mark::Fresh).0.is_empty() {
                at += LOOK_EVERY;
            }
            count_failed_probes(&mut known, failed(), at);
            waits.push(known[&path].next_try - at);
            assert!(known[&path].unread());
            assert_eq!(known[&path].why.as_deref(), Some("read error"));
        }
        assert_eq!(waits[..4], [600, 1200, 2400, 4800]);
        assert!(waits.is_sorted());
        assert_eq!(*waits.last().unwrap(), LONGEST_WAIT);
        assert_eq!(known[&path].tries, PROBE_TRIES - 1 + 12);
        assert_eq!(wait_after(u32::MAX), LONGEST_WAIT);
        // Saved before tries were counted, it starts from none.
        let old: Seen = serde_json::from_value(json!({ "size": 1, "mtime": 2, "mark": "fresh", "seen": 3 })).unwrap();
        assert_eq!((old.tries, old.next_try, old.why), (0, 0, None));
        // Another file under the name has its own tries.
        settle(&mut known, vec![(path.clone(), 9, at)], at + 100, Mark::Fresh);
        assert_eq!((known[&path].tries, known[&path].next_try, known[&path].why.is_none()), (0, 0, true));
    }

    #[test]
    fn videos_an_earlier_version_gave_up_on_are_tried_again() {
        let seen = |mark, tries| Seen { tries, ..Seen::new(1, 2, mark, 3) };
        let (a, b, c) = (PathBuf::from("/w/a.mp4"), PathBuf::from("/w/b.mp4"), PathBuf::from("/w/c.mp4"));
        // Left after its tries, left for what it is, and left after a probe that got through.
        let files = BTreeMap::from([(a.clone(), seen(Mark::Left, PROBE_TRIES)), (b.clone(), seen(Mark::Left, 0)), (c.clone(), seen(Mark::Left, 1))]);
        let mut folders = BTreeMap::from([(1, Folder { files, ..Default::default() })]);
        take_back_given_up(&mut folders);
        let files = &folders[&1].files;
        assert!(files[&a].unread());
        assert_eq!((files[&b].mark, files[&c].mark), (Mark::Left, Mark::Left));
    }

    #[test]
    fn the_watched_folder_is_walked_wherever_the_output_folder_is() {
        let dir = std::env::temp_dir().join(format!("av1opt-watch-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("out/watched");
        std::fs::create_dir_all(root.join("made")).unwrap();
        std::fs::write(root.join("a.mp4"), b"x").unwrap();
        std::fs::write(root.join("made/b.mp4"), b"x").unwrap();
        let names = |out: &Path| -> Vec<String> {
            let mut v: Vec<String> = videos(&root, out).unwrap().into_iter().map(|f| f.0.strip_prefix(&root).unwrap().display().to_string()).collect();
            v.sort();
            v
        };
        assert_eq!(names(&dir.join("elsewhere")), ["a.mp4", "made/b.mp4"]);
        // The output folder lies below the root, however its path is written.
        assert_eq!(names(&root.join("made")), ["a.mp4"]);
        assert_eq!(names(&root.join("made/../made")), ["a.mp4"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_output_folder_is_judged_by_where_it_really_is() {
        let dir = std::env::temp_dir().join(format!("av1opt-watch-out-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("w");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::os::unix::fs::symlink(&root, dir.join("link")).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        for same in [dir.join("w"), dir.join("w/../w"), dir.join("w/sub/.."), dir.join("link")] {
            let e = output_problem(&root, &same).unwrap_err().to_string();
            assert!(e.contains("cannot be the watched folder"), "{}: {e}", same.display());
        }
        for above in [dir.clone(), dir.join("w/.."), PathBuf::from("/")] {
            let e = output_problem(&root, &above).unwrap_err().to_string();
            assert!(e.contains("contains the watched folder"), "{}: {e}", above.display());
        }
        for fine in [dir.join("out"), dir.join("w/sub"), dir.join("w/new/deeper"), dir.join("w2")] {
            output_problem(&root, &fine).unwrap();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn failed_at(tries: u32, last_tried: u64, seen: u64) -> Seen {
        Seen { tries, why: Some("no".into()), next_try: last_tried + wait_after(tries), ..Seen::new(1, 1, Mark::Fresh, seen) }
    }

    #[test]
    fn videos_gone_from_a_folder_are_not_shown_as_waiting_to_be_read() {
        let files = BTreeMap::from([(PathBuf::from("/w/here.mkv"), failed_at(1, 90, 100)), (PathBuf::from("/w/gone.mkv"), failed_at(1, 50, 90))]);
        let mut f = Folder { files, found_at: Some(100), ..Default::default() };
        assert_eq!(f.unread().map(|(p, _)| p.to_str().unwrap()).collect::<Vec<_>>(), ["/w/here.mkv"]);
        assert_eq!(f.found().count(), 1, "what the last look did not find is not counted either");
        // Nothing found at all, as with a drive that is away: what is known stays, unlisted.
        f.found_at = Some(200);
        assert_eq!(f.unread().count(), 0);
        assert_eq!(f.files.len(), 2);
        // A state file of an earlier version does not say. The next look will.
        f.found_at = None;
        assert_eq!(f.unread().count(), 2);
    }

    fn numbered(i: usize) -> PathBuf {
        PathBuf::from(format!("/w/{i:04}.mkv"))
    }

    #[test]
    fn a_look_tries_first_what_was_never_tried_and_then_what_waited_longest() {
        let now = 1_000_000;
        // Due again, the lowest numbers tried longest ago.
        let mut files: BTreeMap<PathBuf, Seen> = (0..1000).map(|i| (numbered(i), failed_at(1, now - 5000 + i as u64, now))).collect();
        let first_time = PathBuf::from("/w/new.mkv");
        files.insert(first_time.clone(), Seen::new(1, 1, Mark::Fresh, now));
        let before = files.clone();
        let ready: Vec<PathBuf> = files.keys().cloned().rev().collect();
        let tries = tries_of(&files, ready, now, false);
        assert_eq!(tries.first_time, [first_time]);
        assert_eq!(tries.again, (0..1000).map(numbered).collect::<Vec<_>>());
        assert_eq!(files, before, "the waits are left alone");

        // Those whose wait is not over come up only when asked for.
        let waiting: BTreeMap<PathBuf, Seen> = (0..100).map(|i| (numbered(i), failed_at(9, now - 10 + i as u64, now))).collect();
        assert_eq!(tries_of(&waiting, Vec::new(), now, false), Tries::default());
        assert_eq!(tries_of(&waiting, Vec::new(), now, true).again, (0..100).map(numbered).collect::<Vec<_>>());
        // A video the look did not find is not tried.
        let gone = BTreeMap::from([(numbered(0), failed_at(9, now - 10, now - 300))]);
        assert_eq!(tries_of(&gone, Vec::new(), now, true), Tries::default());
    }

    /// Runs rounds in which `fails` says whose probe does not get through, and returns what the
    /// rounds were given and what came of them.
    fn rounds_of(first_time: usize, again: usize, time: Duration, stop: impl Fn() -> bool, fails: impl Fn(&PathBuf) -> bool) -> (Vec<Vec<PathBuf>>, Judged, Option<anyhow::Error>) {
        let tries = Tries { first_time: (5000..5000 + first_time).map(numbered).collect(), again: (0..again).map(numbered).collect() };
        let mut given = Vec::new();
        let (judged, problem) = in_rounds(tries, time, stop, |round| {
            given.push(round.clone());
            let (failed, worth): (Vec<_>, Vec<_>) = round.into_iter().partition(|p| fails(p));
            Ok(Judged { worth, unjudged: failed.into_iter().map(|p| (p, "no".to_string())).collect(), ..Default::default() })
        });
        (given, judged, problem)
    }

    #[test]
    fn a_look_goes_through_all_retries_while_they_get_through() {
        // The 2000 videos of a drive that is back: one look, not a hundred.
        let (given, judged, problem) = rounds_of(3, 2000, RETRY_TIME, || false, |_| false);
        assert!(problem.is_none());
        assert_eq!(given.len(), 100);
        assert_eq!(given[0].len(), 3 + RETRIES_PER_ROUND, "those never tried are not held back");
        assert!(given[1..].iter().all(|round| round.len() == RETRIES_PER_ROUND));
        assert_eq!(judged.worth.len(), 2003);
    }

    #[test]
    fn a_look_ends_when_retries_fail_or_its_time_is_up() {
        // The drive is still away: one round, as it would cost a probe's limit for each.
        let (given, judged, _) = rounds_of(3, 2000, RETRY_TIME, || false, |p| *p < numbered(5000));
        assert_eq!(given.len(), 1);
        assert_eq!((judged.worth.len(), judged.unjudged.len()), (3, RETRIES_PER_ROUND));
        // A few damaged files among many that read do not end it.
        let (given, ..) = rounds_of(0, 100, RETRY_TIME, || false, |p| p.to_str().unwrap().ends_with("0.mkv"));
        assert_eq!(given.len(), 5);
        // Half of a round failing does.
        let (given, ..) = rounds_of(0, 100, RETRY_TIME, || false, |p| *p < numbered(10));
        assert_eq!(given.len(), 1);
        // Out of time, or the server stopping.
        assert_eq!(rounds_of(0, 100, Duration::ZERO, || false, |_| false).0.len(), 1);
        assert_eq!(rounds_of(0, 100, RETRY_TIME, || true, |_| false).0.len(), 1);
        // Nothing to try again, and those never tried are still judged.
        assert_eq!(rounds_of(4, 0, RETRY_TIME, || false, |_| false).0, [(5000..5004).map(numbered).collect::<Vec<_>>()]);
    }

    #[test]
    fn what_rounds_found_out_is_kept_when_a_later_one_fails() {
        let tries = Tries { first_time: Vec::new(), again: (0..100).map(numbered).collect() };
        let mut rounds = 0;
        let (judged, problem) = in_rounds(tries, RETRY_TIME, || false, |round| {
            rounds += 1;
            anyhow::ensure!(rounds < 3, "the drive went");
            Ok(Judged { left: round, ..Default::default() })
        });
        assert_eq!(judged.left.len(), 2 * RETRIES_PER_ROUND);
        assert_eq!(problem.unwrap().to_string(), "the drive went");
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
