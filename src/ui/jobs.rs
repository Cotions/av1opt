//! Encode queue: jobs run one at a time through the same `batch::run` as `av1opt encode`.
//! Progress events update a per-file state (for page reloads) and go out on the event stream.

use super::{App, hold, now};
use crate::batch::{self, OutDirPlace};
use crate::encode::{Container, Options};
use crate::events::{Event, Hooks};
use crate::metadata::MetaOptions;
use crate::scan::is_video;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Deserialize, Serialize, Clone)]
pub struct JobOptions {
    #[serde(default = "d_container")]
    pub container: String,
    #[serde(default = "d_vmaf")]
    pub target_vmaf: f64,
    #[serde(default = "d_preset")]
    pub preset: u8,
    /// SVT-AV1 parameters besides the CRF; None for the defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub svt_params: Option<String>,
    /// Use the benchmark's picks per resolution class where there are any.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tuned: bool,
    #[serde(default)]
    pub scenes: crate::encode::Scenes,
    #[serde(default = "d_workers")]
    pub workers: u8,
    #[serde(default = "d_savings")]
    pub min_savings: f64,
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub retry_skipped: bool,
    /// Copy the source's tags and chapters.
    #[serde(default = "d_true")]
    pub keep_metadata: bool,
    #[serde(default)]
    pub keep_dates: bool,
    /// Fill tag templates from yt-dlp .info.json files.
    #[serde(default = "d_true")]
    pub info_json: bool,
    /// Preset the tags started from (shown in the queue; the tags themselves are in `meta_tags`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta_preset: Option<String>,
    /// Tag name and template pairs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub meta_tags: Vec<(String, String)>,
    /// Re-encode sound tracks far above what Opus needs.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shrink_audio: bool,
    /// Sections left out of the output, (start, end) seconds. Only for single-file jobs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<(f64, f64)>,
}

fn d_true() -> bool {
    true
}
fn d_container() -> String {
    "mp4".into()
}
fn d_vmaf() -> f64 {
    93.0
}
fn d_preset() -> u8 {
    crate::search::DEFAULT_PRESET
}
fn d_workers() -> u8 {
    1
}
fn d_savings() -> f64 {
    0.15
}

impl JobOptions {
    fn to_options(&self) -> Result<Options> {
        super::fields::TARGET_VMAF.check(self.target_vmaf)?;
        super::fields::PRESET.check(self.preset.into())?;
        super::fields::WORKERS.check(self.workers.into())?;
        for &(a, b) in &self.remove {
            if !(a.is_finite() && b.is_finite() && a >= 0.0 && b > a) {
                bail!("section {a}-{b} is not a valid range");
            }
        }
        let container = match self.container.as_str() {
            "mp4" => Container::Mp4,
            "mkv" => Container::Mkv,
            c => bail!("unknown container {c}"),
        };
        Ok(Options {
            container,
            target_vmaf: self.target_vmaf,
            preset: self.preset,
            svt_params: match self.svt_params.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
                Some(p) => crate::search::parse_params(p)?,
                None => crate::search::DEFAULT_PARAMS.to_string(),
            },
            tuning: if self.tuned { Some(crate::tuning::Tuning::load()?) } else { None },
            min_crf: 20,
            max_crf: 55,
            samples: None,
            sample_secs: None,
            min_savings: self.min_savings.clamp(0.0, 0.95),
            force: self.force,
            overwrite: false,
            keep_temp: false,
            scenes: self.scenes,
            workers: self.workers as usize,
            meta: self.meta()?,
            remove: self.remove.clone(),
            fresh_search: false,
            audio: if self.shrink_audio { crate::encode::Audio::Shrink } else { crate::encode::Audio::Keep },
            out_dir_must_exist: false,
        })
    }

    pub fn meta(&self) -> Result<MetaOptions> {
        let tags: Vec<(String, String)> =
            self.meta_tags.iter().map(|(k, v)| (k.trim().to_string(), v.clone())).filter(|(k, v)| !k.is_empty() && !v.trim().is_empty()).collect();
        crate::metadata::validate(&tags)?;
        Ok(MetaOptions { keep: self.keep_metadata, keep_dates: self.keep_dates, info_json: self.info_json, tags })
    }
}

#[derive(Serialize, Clone, Default)]
pub struct Progress {
    label: String,
    frac: f64,
    fps: f64,
    eta_secs: f64,
}

#[derive(Serialize, Clone, Default)]
pub struct FileState {
    input: PathBuf,
    output: PathBuf,
    report: PathBuf,
    /// pending, running, ok, skip, error, done_before, cancelled
    status: String,
    detail: String,
    source: Option<Value>,
    phase: String,
    phase_detail: String,
    progress: Option<Progress>,
    trials: Vec<Value>,
    chosen: Option<Value>,
    chunks_total: usize,
    chunks_resumed: usize,
    chunks: Vec<Value>,
    spot_checks: Vec<Value>,
    notes: Vec<String>,
    input_bytes: u64,
    output_bytes: u64,
    started: Option<u64>,
    ended: Option<u64>,
    /// Estimated seconds to search and encode it; unknown until the file was scanned or probed.
    est_secs: Option<f64>,
    /// Seconds its job had spent paused when the file started.
    #[serde(skip)]
    paused_before: f64,
}

#[derive(Serialize)]
pub struct Job {
    id: u64,
    created: u64,
    inputs: Vec<PathBuf>,
    #[serde(default)]
    roots: Vec<PathBuf>,
    out_dir: PathBuf,
    options: JobOptions,
    /// The watched folder that queued it, if one did.
    watch: Option<PathBuf>,
    /// queued, running, done, cancelled, failed
    status: String,
    error: Option<String>,
    current: Option<usize>,
    files: Vec<FileState>,
    /// How long encodes take here at the job's preset.
    #[serde(skip)]
    time: crate::estimate::TimeModel,
    #[serde(skip)]
    batch: Vec<batch::Job>,
    #[serde(skip)]
    hooks: Option<Arc<Hooks>>,
    /// Cancel arrived before the job's hooks existed.
    #[serde(skip)]
    cancel_requested: bool,
    /// Inputs of the saved job that were gone when it was queued. Kept with it, so that they
    /// rejoin the job if they are back at the next start.
    #[serde(skip)]
    dropped: Vec<PathBuf>,
    /// Where the output folder was when it was made or first seen. Before each file it is
    /// looked for there: a folder that is missing or on another drive means a drive that is
    /// away, and nothing is written under its mount point.
    #[serde(skip)]
    out_dir_place: Option<OutDirPlace>,
    /// It failed for that reason, and stays in the saved queue for when the drive is back.
    #[serde(skip)]
    out_dir_missing: bool,
    /// The output folder was made for this job once. Jobs saved before folders were made at
    /// queue time lack this, and get theirs made when they come back.
    #[serde(skip)]
    out_dir_made: bool,
    /// How many folders were queued together. Part of where outputs go, so it is kept as it
    /// was: with one of them away the others must not move.
    #[serde(skip)]
    folders_queued: usize,
    /// Videos of the selection left out because their names are not valid UTF-8.
    #[serde(skip)]
    not_text: usize,
}

/// Where a job comes from, which decides what is asked of its folders and files.
#[derive(Clone, Copy, PartialEq)]
enum Origin {
    /// A request made now: a file that is missing is an error to report.
    Asked,
    /// The saved queue, or a stopped job queued again: what is gone is left out if anything
    /// else of the job is still there.
    Saved,
}

/// What every job queued looks things up in, read once for a whole saved queue.
struct Known {
    history: Vec<crate::history::Record>,
    probes: crate::scan::ProbeCache,
}

impl Known {
    fn load() -> Known {
        Known { history: crate::history::load(), probes: crate::scan::ProbeCache::load() }
    }
}

#[derive(Default)]
pub struct Queue {
    jobs: Mutex<Vec<Job>>,
    wake: Condvar,
    next_id: Mutex<u64>,
    /// Set while the server stops: jobs interrupted now stay in the saved queue.
    shutting_down: AtomicBool,
    /// Saved jobs that could not be restored (say, a drive not mounted yet). Kept in the saved
    /// queue so they come back once their files are there again.
    unrestored: Mutex<Vec<(Value, String)>>,
    /// Saved jobs on their way back into the queue. They count as saved until they are in it.
    to_restore: Mutex<VecDeque<Value>>,
    /// One pass over `to_restore` at a time.
    restoring: Mutex<()>,
    saves: super::Saves,
}

/// Holds the running job where it is, or lets it go on, and wakes the worker: a queue that
/// is held starts nothing new.
pub fn set_held(app: &App, held: bool) {
    for h in app.jobs.jobs.lock().unwrap().iter().filter_map(|j| j.hooks.as_ref()) {
        if held { h.pause() } else { h.resume() }
    }
    app.jobs.wake.notify_all();
}

/// True while a job is encoding.
pub fn encoding(app: &App) -> bool {
    app.jobs.jobs.lock().unwrap().iter().any(|j| j.status == "running")
}

/// True while a queued or running job still has `input` to encode.
pub fn waiting_on(app: &App, input: &Path) -> bool {
    let queued = {
        let jobs = app.jobs.jobs.lock().unwrap();
        jobs.iter()
            .filter(|j| matches!(j.status.as_str(), "queued" | "running"))
            .any(|j| j.files.iter().any(|f| matches!(f.status.as_str(), "pending" | "running") && f.input == input))
    };
    // A saved job not back yet names its files or a folder above them.
    let names = |j: &Value| j["inputs"].as_array().is_some_and(|l| l.iter().filter_map(Value::as_str).any(|p| input.starts_with(p)));
    queued || app.jobs.to_restore.lock().unwrap().iter().any(names)
}

/// True once the server is stopping: no new work may start.
pub fn shutting_down(app: &App) -> bool {
    app.jobs.shutting_down.load(Ordering::SeqCst)
}

/// Stops running encodes, previews and duplicate searches so no ffmpeg outlives the server. Interrupted jobs stay
/// in the saved queue and resume on the next start.
pub fn shutdown(app: &App) {
    app.jobs.shutting_down.store(true, Ordering::SeqCst);
    app.preview_hooks.cancel();
    super::dups::shutdown(app);
    super::bench::shutdown(app);
    for j in app.jobs.jobs.lock().unwrap().iter_mut() {
        j.cancel_requested = true;
        if let Some(h) = &j.hooks {
            h.cancel();
        }
    }
    // Give the worker a moment to reap ffmpeg and clear its temp folder.
    for _ in 0..50 {
        if !app.jobs.jobs.lock().unwrap().iter().any(|j| j.hooks.is_some()) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// A job without its file list: status plus per-status counts and bytes saved, so the page can
/// keep a job's header current from one file at a time.
fn head(j: &Job) -> Value {
    let mut counts = std::collections::BTreeMap::<&str, usize>::new();
    let mut saved = 0u64;
    for f in &j.files {
        *counts.entry(f.status.as_str()).or_default() += 1;
        if f.status == "ok" {
            saved += f.input_bytes.saturating_sub(f.output_bytes);
        }
    }
    // Time left: files still waiting, plus what the running one has left of its estimate
    // (a tenth at least: an estimate that ran out doesn't mean the file is done). Time spent
    // paused is not time worked.
    let paused = j.hooks.as_ref().map_or(0.0, |h| h.paused_time().as_secs_f64());
    let (mut left, mut unknown) = (0.0, 0usize);
    if matches!(j.status.as_str(), "queued" | "running") {
        for f in &j.files {
            match (f.status.as_str(), f.est_secs) {
                ("pending", Some(s)) => left += s,
                ("running", Some(s)) => {
                    let worked = f.started.map_or(0.0, |t| now().saturating_sub(t) as f64) - (paused - f.paused_before).max(0.0);
                    left += (s - worked.max(0.0)).max(s * 0.1)
                }
                ("pending" | "running", None) => unknown += 1,
                _ => {}
            }
        }
    }
    json!({
        "id": j.id, "created": j.created, "out_dir": j.out_dir, "options": j.options, "watch": j.watch,
        "status": j.status, "error": j.error, "current": j.current,
        "total": j.files.len(), "counts": counts, "saved": saved,
        "left_secs": left, "left_unknown": unknown,
        "paused": j.hooks.as_ref().is_some_and(|h| h.is_paused()),
        "note": job_note(j),
    })
}

/// What the page says of what is not in a job.
fn job_note(j: &Job) -> Option<String> {
    let not_text = (j.not_text > 0).then(|| format!("{} video(s) left out: {}.", j.not_text, crate::tools::NOT_TEXT));
    let notes: Vec<String> = [dropped_note(&j.dropped), not_text].into_iter().flatten().collect();
    (!notes.is_empty()).then(|| notes.join(" "))
}

/// What the page says of inputs left out of a job. A long list is cut: this goes out with
/// every live event of the job.
fn dropped_note(dropped: &[PathBuf]) -> Option<String> {
    const NAMED: usize = 5;
    if dropped.is_empty() {
        return None;
    }
    let names: Vec<String> = dropped.iter().take(NAMED).map(|p| p.display().to_string()).collect();
    let more = if dropped.len() > NAMED { format!(" and {} more", dropped.len() - NAMED) } else { String::new() };
    Some(format!("Left out because they are gone: {}{more}. They stay saved, and are queued if they are back when av1opt next starts.", names.join(", ")))
}

fn full(j: &Job) -> Value {
    let mut v = head(j);
    v["files"] = serde_json::to_value(&j.files).unwrap_or_default();
    v
}

pub fn list(app: &App) -> Value {
    let jobs: Vec<Value> = app.jobs.jobs.lock().unwrap().iter().map(full).collect();
    // Queued jobs wait while a benchmark measures; the page says so.
    json!({ "jobs": jobs, "unrestored": unrestored_list(app), "restoring": restoring(app), "benchmark": super::bench::running(app), "queue": hold::json(app) })
}

/// Saved jobs still on their way back into the queue. While the last step runs, saving what
/// came of them, that counts as one: nothing is settled until it is done.
fn restoring(app: &App) -> usize {
    let waiting = app.jobs.to_restore.lock().unwrap().len();
    let under_way = matches!(app.jobs.restoring.try_lock(), Err(std::sync::TryLockError::WouldBlock));
    waiting.max(usize::from(under_way))
}

/// Headers only, for the queue badge.
pub fn heads(app: &App) -> Value {
    let jobs: Vec<Value> = app.jobs.jobs.lock().unwrap().iter().map(head).collect();
    json!({ "jobs": jobs, "unrestored": unrestored_list(app), "restoring": restoring(app), "queue": hold::json(app) })
}

fn find(jobs: &[Job], id: u64) -> Result<&Job> {
    jobs.iter().find(|j| j.id == id).ok_or_else(|| anyhow::anyhow!("not found: job {id}"))
}

pub fn get(app: &App, id: u64) -> Result<Value> {
    Ok(full(find(&app.jobs.jobs.lock().unwrap(), id)?))
}

/// One file of a job plus the job's header: what a live event changes, without resending a
/// folder of thousands of files.
pub fn get_file(app: &App, id: u64, index: usize) -> Result<Value> {
    let jobs = app.jobs.jobs.lock().unwrap();
    let j = find(&jobs, id)?;
    let f = j.files.get(index).ok_or_else(|| anyhow::anyhow!("not found: file {index} of job {id}"))?;
    Ok(json!({ "head": head(j), "index": index, "file": f }))
}

#[derive(Deserialize)]
struct AddRequest {
    inputs: Vec<PathBuf>,
    out_dir: PathBuf,
    /// Scan roots: selected files below one of them keep their sub-folder under `out_dir`.
    #[serde(default)]
    roots: Vec<PathBuf>,
    #[serde(default)]
    options: Option<JobOptions>,
    /// The watched folder that queued it, if one did.
    #[serde(default)]
    watch: Option<PathBuf>,
    /// In a saved job: inputs that were gone when it was last queued.
    #[serde(default)]
    dropped: Vec<PathBuf>,
    /// In a saved job: how many folders were queued together. Saved jobs of earlier versions
    /// do not say.
    #[serde(default)]
    folders_queued: Option<usize>,
    /// In a saved job: its output folder was made once. See `Job::out_dir_made`.
    #[serde(default)]
    out_dir_made: bool,
    /// In a saved job, or one a watch queues: where its output folder was.
    #[serde(default)]
    out_dir_place: Option<OutDirPlace>,
}

/// ffmpeg carries on without parameters SVT-AV1 doesn't know; better said when the settings
/// are chosen than found in a finished encode.
pub fn check_options(app: &App, options: &JobOptions) -> Result<()> {
    let o = options.to_options()?;
    if o.svt_params != crate::search::DEFAULT_PARAMS {
        crate::search::check_params(&app.tools(1), o.preset, &o.svt_params)?;
    }
    Ok(())
}

pub fn add(app: &App, body: &Value) -> Result<Value> {
    add_checked(app, body, Origin::Asked)
}

fn add_checked(app: &App, body: &Value, origin: Origin) -> Result<Value> {
    if app.jobs.shutting_down.load(Ordering::SeqCst) {
        bail!("av1opt is shutting down");
    }
    if let Some(p) = app.encode_problem() {
        bail!("{p} (see Settings)");
    }
    // (Jobs restored at startup were checked when they were added.)
    if let Some(o) = body.get("options").filter(|o| !o.is_null()) {
        check_options(app, &serde_json::from_value::<JobOptions>(o.clone())?)?;
    }
    let id_and_files = enqueue(app, prepare(app, body, origin, &Known::load())?);
    app.save_settings();
    persist(app);
    app.jobs.wake.notify_all();
    app.broadcast(json!({ "kind": "jobs" }));
    Ok(id_and_files)
}

/// Wakes the worker, e.g. after the tools were fixed.
pub fn kick(app: &App) {
    // Under the lock, so the worker can't miss it between its check and its wait.
    let _jobs = app.jobs.jobs.lock().unwrap();
    app.jobs.wake.notify_all();
}

/// Makes a job of a request or a saved job, without touching the queue.
fn prepare(app: &App, body: &Value, origin: Origin, known: &Known) -> Result<Job> {
    let r: AddRequest = serde_json::from_value(body.clone())?;
    let options = r.options.unwrap_or_else(|| serde_json::from_value(json!({})).expect("defaults"));
    let opts = options.to_options()?;
    // What was gone last time is looked for again: a drive may be back.
    let earlier = if origin == Origin::Saved { r.dropped.as_slice() } else { &[] };
    if r.inputs.is_empty() && earlier.is_empty() {
        bail!("nothing to encode");
    }
    if !r.out_dir.is_absolute() {
        bail!("pick an output folder");
    }
    // Same safety rule as the command line, without the override.
    crate::fsguard::ensure_writable_location(&r.out_dir, false)?;
    // Canonical paths: no `..` tricks when output folders are derived from scan roots.
    let canon = |p: &PathBuf| std::fs::canonicalize(p).with_context(|| format!("{} does not exist", p.display())).and_then(super::text_path);
    let (mut inputs, mut dropped, mut first_gone) = (Vec::new(), Vec::new(), None);
    for p in r.inputs.iter().chain(earlier) {
        match canon(p) {
            Ok(real) => inputs.push(real),
            Err(e) if origin == Origin::Saved => {
                dropped.push(p.clone());
                first_gone.get_or_insert(e);
            }
            Err(e) => return Err(e),
        }
    }
    if let (true, Some(e)) = (inputs.is_empty(), first_gone) {
        return Err(e);
    }
    // What decides where outputs go stays as it was when the job was queued, or a job that
    // comes back with part of it away would write what is there to other places, and encode
    // again what it had finished. A root that is away is kept for the files that rejoin.
    let roots: Vec<PathBuf> = r.roots.iter().map(|p| canon(p).unwrap_or_else(|_| p.clone())).collect();
    let folders_queued = r.folders_queued.filter(|_| origin == Origin::Saved).unwrap_or_else(|| {
        // Of a saved job that does not say: what is away counts as a folder unless it is
        // named like a video.
        r.inputs.iter().chain(earlier).filter(|p| p.is_dir() || (!p.exists() && !is_video(p))).count()
    });
    let mut batch_jobs = Vec::new();
    for input in &inputs {
        let root = roots.iter().filter(|root| input.starts_with(root) && input != *root).max_by_key(|p| p.as_os_str().len());
        let out_dir = match root {
            Some(root) if input.is_file() => {
                let rel = input.parent().and_then(|p| p.strip_prefix(root).ok()).unwrap_or(Path::new(""));
                r.out_dir.join(rel)
            }
            // Folders queued together each keep their name, as on the command line.
            _ if input.is_dir() => batch::folder_out_among(folders_queued, input, &r.out_dir),
            _ => r.out_dir.clone(),
        };
        batch_jobs.extend(batch::expand(std::slice::from_ref(input), &out_dir, is_video));
    }
    if batch_jobs.is_empty() {
        bail!("no video files in the selection");
    }
    // The queue is kept as JSON, and a folder can hold names that cannot go there.
    let found = batch_jobs.len();
    batch_jobs.retain(|j| j.input.to_str().is_some() && std::fs::canonicalize(&j.input).map_or(true, |c| c.to_str().is_some()));
    let not_text = found - batch_jobs.len();
    if batch_jobs.is_empty() {
        bail!("no video in the selection can be encoded: {}", crate::tools::NOT_TEXT);
    }
    if !opts.remove.is_empty() && batch_jobs.len() > 1 {
        bail!("sections can only be removed when encoding a single video");
    }
    for j in &batch_jobs {
        if !j.out_dir.starts_with(&r.out_dir) {
            bail!("{} would be written outside the output folder", j.input.display());
        }
        crate::fsguard::ensure_writable_location(&j.out_dir, false)?;
    }
    // A folder asked for now is made now, and a name that cannot be used is said at once. After
    // this a missing folder means a drive that is away: see `out_dir_place`.
    let asked_now = origin == Origin::Asked && r.watch.is_none();
    let out_dir_made = if asked_now {
        std::fs::create_dir_all(&r.out_dir).with_context(|| format!("cannot create {}", r.out_dir.display()))?;
        true
    } else if origin == Origin::Saved && !r.out_dir_made {
        // Saved by a version that did not make the folder when a job was queued: nothing has
        // made it yet, so its absence says nothing about a drive. This happens once.
        let made = batch::make_out_dir_once(&r.out_dir);
        if let Err(e) = &made {
            eprintln!("warning: {e:#}");
        }
        made.is_ok()
    } else {
        true
    };
    let out_dir_place = r.out_dir_place.filter(|_| !asked_now).or_else(|| OutDirPlace::of(&r.out_dir));
    // Time estimates for files a scan has seen; the others get theirs when they are probed.
    let time = crate::estimate::TimeModel::fit(&known.history, opts.preset);
    // Estimates are for the way the files will be encoded. "Where uneven" is not known
    // before a file's samples are scored: each file gets the average of the two ways.
    let time = match opts.scenes {
        crate::encode::Scenes::Always => time.per_scene(),
        crate::encode::Scenes::Auto => time.auto(),
        crate::encode::Scenes::Off => time,
    };
    let abs_inputs: Vec<PathBuf> = batch_jobs.iter().map(|j| std::fs::canonicalize(&j.input).unwrap_or_else(|_| j.input.clone())).collect();
    let probed = known.probes.known(&abs_inputs);
    let files: Vec<FileState> = batch_jobs
        .iter()
        .zip(abs_inputs)
        .map(|(j, abs)| {
            let stem = j.input.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let output = j.out_dir.join(format!("{stem}.{}", opts.container.ext()));
            app.allow_video(&abs);
            FileState {
                report: output.with_extension("av1opt.json"),
                est_secs: probed.get(&abs).map(|i| time.secs(i)).filter(|s| *s > 0.0),
                input: abs,
                output,
                status: "pending".into(),
                input_bytes: std::fs::metadata(&j.input).map_or(0, |m| m.len()),
                ..Default::default()
            }
        })
        .collect();
    Ok(Job {
        id: 0,
        created: now(),
        inputs,
        roots,
        out_dir: r.out_dir,
        options,
        watch: r.watch,
        status: "queued".into(),
        error: None,
        current: None,
        files,
        time,
        batch: batch_jobs,
        hooks: None,
        cancel_requested: false,
        dropped,
        out_dir_place,
        out_dir_missing: false,
        out_dir_made,
        folders_queued,
        not_text,
    })
}

/// Puts a job in the queue. Saving and telling the page are left to the caller, who may be
/// adding many.
fn enqueue(app: &App, mut job: Job) -> Value {
    job.id = {
        let mut n = app.jobs.next_id.lock().unwrap();
        *n += 1;
        *n
    };
    let (id, count) = (job.id, job.files.len());
    let (out_dir, by_hand) = (job.out_dir.clone(), job.watch.is_none());
    app.jobs.jobs.lock().unwrap().push(job);
    let mut s = app.settings.lock().unwrap();
    // The folder offered for the next encode is the one last chosen, not a watch's.
    if by_hand {
        s.out_dir = Some(out_dir.clone());
    }
    if !s.history_dirs.contains(&out_dir) {
        s.history_dirs.push(out_dir);
    }
    json!({ "id": id, "files": count })
}

/// Version of `ui-queue.json`. Bump it when a saved job's layout changes meaning. The
/// first layout was a bare array of jobs; since versioning it is `{format, jobs}`.
const QUEUE_FORMAT: u64 = 1;

fn queue_path() -> Option<PathBuf> {
    super::library::settings_path().map(|p| p.with_file_name("ui-queue.json"))
}

/// A job as the saved queue holds it, and as it is queued again.
fn saved_form(j: &Job) -> Value {
    json!({
        "inputs": j.inputs, "dropped": j.dropped, "roots": j.roots, "out_dir": j.out_dir, "options": j.options, "watch": j.watch,
        "folders_queued": j.folders_queued, "out_dir_made": j.out_dir_made, "out_dir_place": j.out_dir_place,
    })
}

/// Inputs left out of a job outlive it: when the job leaves the list or has nothing more to do,
/// they become a saved entry of their own, with its settings, and come back at a later start.
fn keep_left_out(queue: &Queue, j: &mut Job) {
    if j.dropped.is_empty() {
        return;
    }
    let mut saved = saved_form(j);
    saved["inputs"] = json!(std::mem::take(&mut j.dropped));
    saved["dropped"] = json!([]);
    queue.unrestored.lock().unwrap().push((saved, "they were not there when the rest of their job ran; they are queued when they are back".into()));
}

/// Saves unfinished jobs so the next `av1opt ui` picks them up again: those in the queue, those
/// that could not be brought back and those still on their way. Whenever it is called, the
/// file holds every one of them.
fn persist(app: &App) {
    if let Some(path) = queue_path() {
        app.jobs.save(&path);
    }
}

impl Queue {
    fn save(&self, path: &Path) {
        let stopping = || self.shutting_down.load(Ordering::SeqCst);
        if stopping() {
            return;
        }
        self.saves.write("the queue", path, || {
            let pending: Vec<Value> = self
                .jobs
                .lock()
                .unwrap()
                .iter()
                .filter(|j| matches!(j.status.as_str(), "queued" | "running") || j.out_dir_missing)
                .map(saved_form)
                .chain(self.unrestored.lock().unwrap().iter().map(|(j, _)| j.clone()))
                .chain(self.to_restore.lock().unwrap().iter().cloned())
                .collect();
            // Looked at after the copy is taken. Shutting down sets this before it cancels the
            // running job, so a copy that lacks the job for that reason is never written.
            if stopping() {
                return None;
            }
            serde_json::to_vec_pretty(&json!({ "format": QUEUE_FORMAT, "jobs": pending })).ok()
        });
    }
}

/// The jobs in the saved queue. A file that cannot be understood is moved aside, or the next
/// save would replace it with an empty queue.
fn read_saved(app: &App, path: &Path) -> Vec<Value> {
    let unreadable = |why: String| {
        let told = super::set_aside(path, "the saved queue", &why, "the queue starts empty");
        app.notices.lock().unwrap().push(told);
        Vec::new()
    };
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => return unreadable(e.to_string()),
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Err(e) => unreadable(e.to_string()),
        Ok(Value::Array(jobs)) => jobs,
        Ok(Value::Object(mut o)) => {
            let format = o.get("format").and_then(Value::as_u64).unwrap_or(0);
            if format > QUEUE_FORMAT {
                // Written by a newer av1opt. Set it aside rather than overwrite it.
                let aside = path.with_file_name(format!("ui-queue.format{format}.json"));
                let _ = std::fs::rename(path, &aside);
                eprintln!("warning: the saved queue is from a newer av1opt; moved to {} and not restored", aside.display());
                return Vec::new();
            }
            match o.remove("jobs") {
                Some(Value::Array(jobs)) => jobs,
                _ => unreadable("it has no list of jobs".into()),
            }
        }
        Ok(_) => unreadable("it has no list of jobs".into()),
    }
}

/// Re-queues jobs left unfinished by the previous session. Finished files are recognised as
/// done and scene-mode chunks resume, as with a re-run of `av1opt encode`.
///
/// The file is read here and the jobs come back on a thread of their own: a long queue takes
/// a while, and the server has to answer meanwhile (the desktop app gives up on one that does
/// not). The page sees them arrive.
pub fn restore(app: &Arc<App>) {
    let Some(path) = queue_path() else { return };
    let saved = read_saved(app, &path);
    if saved.is_empty() {
        return;
    }
    *app.jobs.to_restore.lock().unwrap() = saved.into();
    let app = app.clone();
    std::thread::spawn(move || {
        {
            let _one = app.jobs.restoring.lock().unwrap_or_else(|e| e.into_inner());
            let (_, all_read) = bring_back(&app);
            finish_bringing_back(&app, all_read);
        }
        // Once the pass is over, so that what the page then reads says so.
        app.broadcast(json!({ "kind": "jobs" }));
    });
}

/// Moves the jobs of `to_restore` into the queue, or to the list of those that could not be
/// restored. Returns how many got into the queue, and whether the list was gone through.
fn bring_back(app: &App) -> (usize, bool) {
    let known = Known::load();
    let (mut added, mut told) = (0, Instant::now());
    loop {
        if shutting_down(app) {
            return (added, false);
        }
        let Some(saved) = app.jobs.to_restore.lock().unwrap().front().cloned() else { return (added, true) };
        let prepared = prepare(app, &saved, Origin::Saved, &known);
        {
            // Not while a save is on its way: it would find this job in both places, or neither.
            let _saving = app.jobs.saves.hold();
            match prepared {
                Ok(job) => {
                    // Not `add`: with broken tools the jobs wait in the queue rather than get dropped.
                    eprintln!("re-queued {} file(s) from the last session", job.files.len());
                    enqueue(app, job);
                    added += 1;
                }
                Err(e) => {
                    eprintln!("could not re-queue a job from the last session (kept for later): {e:#}");
                    app.jobs.unrestored.lock().unwrap().push((saved, format!("{e:#}")));
                }
            }
            app.jobs.to_restore.lock().unwrap().pop_front();
        }
        app.jobs.wake.notify_all();
        // Each message has the page fetch the whole queue.
        if told.elapsed() >= Duration::from_secs(1) {
            told = Instant::now();
            app.broadcast(json!({ "kind": "jobs" }));
        }
    }
}

/// Saves what a pass over `to_restore` led to, once. A pass cut short saves nothing: the file
/// still holds every job, and the next start goes through them again.
fn finish_bringing_back(app: &App, all_read: bool) {
    if all_read {
        app.save_settings();
        persist(app);
    }
}

/// Saved jobs that could not be restored, with the reason.
fn unrestored_list(app: &App) -> Vec<Value> {
    let list = app.jobs.unrestored.lock().unwrap();
    list.iter()
        .map(|(j, why)| {
            let mut entry = json!({ "out_dir": j["out_dir"], "error": why });
            // A saved file can hold anything, and this one could not be understood perhaps
            // for that reason: the page gets a list of names or none.
            if j["inputs"].as_array().is_some_and(|l| l.iter().all(Value::is_string)) {
                entry["inputs"] = j["inputs"].clone();
            }
            entry
        })
        .collect()
}

/// Tries the saved-but-missing jobs again (say, after mounting a drive), or forgets them.
pub fn unrestored_action(app: &App, retry: bool) -> Result<Value> {
    let one = match app.jobs.restoring.try_lock() {
        Ok(one) => one,
        Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => bail!("the saved queue is still being read; try again in a moment"),
    };
    {
        let _saving = app.jobs.saves.hold();
        let list = std::mem::take(&mut *app.jobs.unrestored.lock().unwrap());
        if retry {
            app.jobs.to_restore.lock().unwrap().extend(list.into_iter().map(|(job, _)| job));
        }
    }
    let (restored, all_read) = if retry { bring_back(app) } else { (0, true) };
    finish_bringing_back(app, all_read);
    drop(one);
    app.broadcast(json!({ "kind": "jobs" }));
    Ok(json!({ "restored": restored, "unrestored": unrestored_list(app) }))
}

pub fn cancel(app: &App, id: u64) -> Result<Value> {
    let mut jobs = app.jobs.jobs.lock().unwrap();
    let Some(j) = jobs.iter_mut().find(|j| j.id == id) else { bail!("not found: job {id}") };
    match j.status.as_str() {
        "queued" => {
            j.status = "cancelled".into();
            for f in &mut j.files {
                f.status = "cancelled".into();
            }
        }
        "running" => match &j.hooks {
            Some(h) => h.cancel(),
            None => j.cancel_requested = true,
        },
        _ => {}
    }
    drop(jobs);
    persist(app);
    app.broadcast(json!({ "kind": "jobs" }));
    Ok(json!({ "ok": true }))
}

pub fn remove(app: &App, id: u64) -> Result<Value> {
    let mut jobs = app.jobs.jobs.lock().unwrap();
    if jobs.iter().any(|j| j.id == id && j.status == "running") {
        bail!("cancel the job first");
    }
    if let Some(at) = jobs.iter().position(|j| j.id == id) {
        keep_left_out(&app.jobs, &mut jobs[at]);
        jobs.remove(at);
    }
    drop(jobs);
    persist(app);
    app.broadcast(json!({ "kind": "jobs" }));
    Ok(json!({ "ok": true }))
}

/// Moves a waiting job ahead of the other waiting ones, so it is the next to start. The
/// order is the saved one too.
pub fn first(app: &App, id: u64) -> Result<Value> {
    let mut jobs = app.jobs.jobs.lock().unwrap();
    let Some(from) = jobs.iter().position(|j| j.id == id) else { bail!("not found: job {id}") };
    if jobs[from].status != "queued" {
        bail!("only a waiting job can be moved");
    }
    let to = jobs.iter().position(|j| j.status == "queued").unwrap_or(from);
    let job = jobs.remove(from);
    jobs.insert(to, job);
    drop(jobs);
    persist(app);
    app.broadcast(json!({ "kind": "jobs" }));
    Ok(json!({ "ok": true }))
}

/// Queues the files of a cancelled or failed job once more, with the same options, and
/// drops the old entry. Files that were finished are recognised as done and scene chunks
/// resume, as with any re-run.
pub fn again(app: &App, id: u64) -> Result<Value> {
    let body = {
        let jobs = app.jobs.jobs.lock().unwrap();
        let j = find(&jobs, id)?;
        if !matches!(j.status.as_str(), "cancelled" | "failed") {
            bail!("only a cancelled or failed job can be queued again");
        }
        saved_form(j)
    };
    let r = add_checked(app, &body, Origin::Saved)?;
    // Only once the new job exists: a refused request leaves the old entry on screen.
    app.jobs.jobs.lock().unwrap().retain(|j| j.id != id);
    persist(app);
    app.broadcast(json!({ "kind": "jobs" }));
    Ok(r)
}

/// Takes the jobs that are over off the list. Those waiting for their output folder are not
/// over: they stay saved for when the drive is back, until they are removed one by one.
pub fn clear_finished(app: &App) -> Result<Value> {
    app.jobs.jobs.lock().unwrap().retain_mut(|j| {
        let stays = matches!(j.status.as_str(), "queued" | "running") || j.out_dir_missing;
        if !stays {
            keep_left_out(&app.jobs, j);
        }
        stays
    });
    persist(app);
    app.broadcast(json!({ "kind": "jobs" }));
    Ok(json!({ "ok": true }))
}

/// Folds one event into the job's state. Returns the file index it concerned.
fn apply(job: &mut Job, e: &Event) -> Option<usize> {
    if let Event::FileStart { index, .. } = e {
        job.current = Some(*index);
    }
    let idx = match e {
        Event::FileEnd { index, .. } => *index,
        _ => job.current?,
    };
    let f = job.files.get_mut(idx)?;
    let v = serde_json::to_value(e).unwrap_or(Value::Null);
    match e {
        Event::FileStart { output, .. } => {
            f.status = "running".into();
            f.output = output.clone();
            f.report = output.with_extension("av1opt.json");
            f.started = Some(now());
            f.paused_before = job.hooks.as_ref().map_or(0.0, |h| h.paused_time().as_secs_f64());
            f.phase = "probing".into();
        }
        Event::Source { width, height, fps, duration_s, .. } => {
            f.source = Some(v);
            if f.est_secs.is_none() {
                let info = crate::probe::MediaInfo { width: *width, height: *height, fps: *fps, duration_s: *duration_s, ..Default::default() };
                f.est_secs = Some(job.time.secs(&info)).filter(|s| *s > 0.0);
            }
        }
        Event::Phase { phase, detail } => {
            f.phase = phase.clone();
            f.phase_detail = detail.clone();
            f.progress = None;
        }
        Event::Trial { .. } => f.trials.push(v),
        Event::Chosen { .. } => f.chosen = Some(v),
        Event::Progress { label, frac, fps, eta_secs } => {
            f.progress = Some(Progress { label: label.clone(), frac: *frac, fps: *fps, eta_secs: *eta_secs })
        }
        Event::ChunkPlan { chunks, done } => {
            f.chunks_total = *chunks;
            f.chunks_resumed = *done;
        }
        Event::Chunk { frac, eta_secs, .. } => {
            f.chunks.push(v);
            f.progress = Some(Progress { label: "chunks".into(), frac: *frac, fps: 0.0, eta_secs: *eta_secs });
        }
        Event::SpotCheck { .. } => f.spot_checks.push(v),
        Event::Note { text } => f.notes.push(text.clone()),
        Event::FileEnd { status, detail, input_bytes, output_bytes, .. } => {
            f.status = status.clone();
            f.detail = detail.clone();
            f.input_bytes = *input_bytes;
            f.output_bytes = *output_bytes;
            f.ended = Some(now());
            f.progress = None;
            f.phase.clear();
            // Live details are in the report now; dropping them keeps long queues small.
            f.phase_detail.clear();
            f.source = None;
            f.trials = Vec::new();
            f.chunks = Vec::new();
            f.spot_checks = Vec::new();
        }
    }
    Some(idx)
}

/// Runs queued jobs one after another, forever.
pub fn start_worker(app: Arc<App>) {
    std::thread::spawn(move || {
        loop {
            let (id, opts, retry, out_dir, place, workers) = loop {
                // Checked before taking the lock: probing runs ffmpeg. With unusable tools,
                // queued jobs wait (the page shows why) instead of failing.
                let tools_ok = app.encode_problem().is_none();
                // A benchmark measures encode times: jobs wait until it is done (it wakes us).
                let benchmarking = super::bench::running(&app);
                // Paused, or outside the hours encodes may run in.
                let held = hold::current(&app).is_some();
                let mut jobs = app.jobs.jobs.lock().unwrap();
                if tools_ok
                    && !benchmarking
                    && !held
                    && !app.jobs.shutting_down.load(Ordering::SeqCst)
                    && let Some(j) = jobs.iter_mut().find(|j| j.status == "queued")
                {
                    j.status = "running".into();
                    break (j.id, j.options.to_options(), j.options.retry_skipped, j.out_dir.clone(), j.out_dir_place.clone(), j.options.workers);
                }
                // Woken by new jobs and tool changes; the timeout is only a safety net.
                drop(app.jobs.wake.wait_timeout(jobs, std::time::Duration::from_secs(30)).unwrap());
            };
            app.broadcast(json!({ "kind": "jobs" }));
            // A bug anywhere under a job fails that job; the queue goes on.
            let result = batch::guarded(|| run_job(&app, id, opts, retry, &out_dir, place, workers));
            let mut jobs = app.jobs.jobs.lock().unwrap();
            let mut done = None;
            if let Some(j) = jobs.iter_mut().find(|j| j.id == id) {
                j.hooks = None;
                j.current = None;
                match result {
                    Ok(s) if s.cancelled => {
                        j.status = "cancelled".into();
                        for f in j.files.iter_mut().filter(|f| f.status == "pending") {
                            f.status = "cancelled".into();
                        }
                    }
                    Ok(_) => {
                        j.status = "done".into();
                        keep_left_out(&app.jobs, j);
                    }
                    Err(e) => {
                        j.status = "failed".into();
                        j.error = Some(format!("{e:#}"));
                        j.out_dir_missing = batch::is_out_dir_problem(&e);
                        // A job that waits for its folder comes back whole.
                        if !j.out_dir_missing {
                            keep_left_out(&app.jobs, j);
                        }
                    }
                }
                done = Some(head(j));
            }
            drop(jobs);
            persist(&app);
            app.broadcast(json!({ "kind": "jobs" }));
            // For a desktop notification; not sent for jobs stopped by shutting down.
            if let Some(h) = done
                && !app.jobs.shutting_down.load(Ordering::SeqCst)
            {
                app.broadcast(json!({ "kind": "job_done", "job": h }));
            }
        }
    });
}

fn run_job(app: &Arc<App>, id: u64, opts: Result<Options>, retry: bool, out_dir: &Path, place: Option<OutDirPlace>, workers: u8) -> Result<batch::Summary> {
    let mut opts = opts?;
    // The folder was made when the job was queued, or will not be: made anew now it would land
    // on the disk under the mount point of a drive that is away.
    opts.out_dir_must_exist = true;
    crate::fsguard::ensure_writable_location(out_dir, false)?;
    if let Some(problem) = batch::out_dir_problem(out_dir, place.as_ref()) {
        return Err(problem);
    }
    let place = place.or_else(|| OutDirPlace::of(out_dir));
    let sink_app = app.clone();
    let hooks = Arc::new(Hooks::new(move |e: Event| {
        let file = {
            let mut jobs = sink_app.jobs.jobs.lock().unwrap();
            let job = jobs.iter_mut().find(|j| j.id == id);
            job.and_then(|j| {
                let i = apply(j, &e)?;
                // Finished outputs become playable in the comparison view.
                if let Event::FileEnd { status, .. } = &e
                    && (status == "ok" || status == "done_before")
                {
                    sink_app.allow_video(&j.files[i].output);
                    sink_app.allow_video(&j.files[i].input);
                }
                Some(i)
            })
        };
        sink_app.broadcast(json!({ "kind": "job", "id": id, "file": file, "event": e }));
    }));
    let mut tools = app.tools(if opts.scenes.possible() { workers } else { 1 });
    tools.hooks = Some(hooks.clone());
    // A hold that arrived since the worker looked. One that arrives after this is put into
    // effect through the job's hooks, which exist from here on.
    let held = hold::current(app).is_some();
    // Take the batch list out of the job so the lock isn't held while encoding.
    let batch_jobs = {
        let mut jobs = app.jobs.jobs.lock().unwrap();
        let Some(j) = jobs.iter_mut().find(|j| j.id == id) else { bail!("job vanished") };
        if j.cancel_requested {
            hooks.cancel();
        } else if held {
            hooks.pause();
        }
        j.hooks = Some(hooks);
        j.out_dir_place.clone_from(&place);
        std::mem::take(&mut j.batch)
    };
    let r = batch::run_placed(&tools, &batch_jobs, out_dir, place.as_ref(), &opts, retry);
    if let Some(j) = app.jobs.jobs.lock().unwrap().iter_mut().find(|j| j.id == id) {
        j.batch = batch_jobs;
    }
    let mut summary = r?;
    if summary.cancelled {
        return Ok(summary);
    }
    // Lost on the way, the folder failed the files after that, even if it is back by now. The
    // job waits for the drive like one that found it missing at the start, or those files
    // would be forgotten.
    match summary.out_dir_lost.take().or_else(|| batch::out_dir_problem(out_dir, place.as_ref())) {
        Some(problem) => Err(problem),
        None => Ok(summary),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_save_that_was_waiting_when_the_server_began_to_stop_writes_nothing() {
        let dir = std::env::temp_dir().join(format!("av1opt-queue-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ui-queue.json");
        let queue = Queue::default();
        queue.save(&path);
        assert!(path.exists(), "a save in normal times writes the file");
        std::fs::remove_file(&path).unwrap();
        std::thread::scope(|s| {
            // The save gets past its first check and waits for the one in progress.
            let held = queue.saves.hold();
            let saving = s.spawn(|| queue.save(&path));
            std::thread::sleep(Duration::from_millis(50));
            queue.shutting_down.store(true, Ordering::SeqCst);
            drop(held);
            saving.join().unwrap();
        });
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
