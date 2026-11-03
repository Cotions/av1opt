//! Encode queue: jobs run one at a time through the same `batch::run` as `av1opt encode`.
//! Progress events update a per-file state (for page reloads) and go out on the event stream.

use super::{App, now};
use crate::batch;
use crate::encode::{Container, Options};
use crate::events::{Event, Hooks};
use crate::scan::is_video;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

#[derive(Deserialize, Serialize, Clone)]
pub struct JobOptions {
    #[serde(default = "d_container")]
    pub container: String,
    #[serde(default = "d_vmaf")]
    pub target_vmaf: f64,
    #[serde(default = "d_preset")]
    pub preset: u8,
    #[serde(default)]
    pub scenes: bool,
    #[serde(default = "d_workers")]
    pub workers: u8,
    #[serde(default = "d_savings")]
    pub min_savings: f64,
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub retry_skipped: bool,
}

fn d_container() -> String {
    "mp4".into()
}
fn d_vmaf() -> f64 {
    93.0
}
fn d_preset() -> u8 {
    6
}
fn d_workers() -> u8 {
    1
}
fn d_savings() -> f64 {
    0.15
}

impl JobOptions {
    fn to_options(&self) -> Result<Options> {
        if !(50.0..=99.5).contains(&self.target_vmaf) {
            bail!("target VMAF must be between 50 and 99.5");
        }
        if self.preset > 13 {
            bail!("preset must be 0-13");
        }
        if !(1..=8).contains(&self.workers) {
            bail!("workers must be 1-8");
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
            min_crf: 20,
            max_crf: 55,
            samples: 4,
            sample_secs: 8.0,
            min_savings: self.min_savings.clamp(0.0, 0.95),
            force: self.force,
            overwrite: false,
            keep_temp: false,
            scenes: self.scenes,
            workers: self.workers as usize,
        })
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
    /// queued, running, done, cancelled, failed
    status: String,
    error: Option<String>,
    current: Option<usize>,
    files: Vec<FileState>,
    #[serde(skip)]
    batch: Vec<batch::Job>,
    #[serde(skip)]
    hooks: Option<Arc<Hooks>>,
    /// Cancel arrived before the job's hooks existed.
    #[serde(skip)]
    cancel_requested: bool,
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
    unrestored: Mutex<Vec<Value>>,
}

/// Stops running encodes and previews so no ffmpeg outlives the server. Interrupted jobs stay
/// in the saved queue and resume on the next start.
pub fn shutdown(app: &App) {
    app.jobs.shutting_down.store(true, Ordering::SeqCst);
    app.preview_hooks.cancel();
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
    json!({
        "id": j.id, "created": j.created, "out_dir": j.out_dir, "options": j.options,
        "status": j.status, "error": j.error, "current": j.current,
        "total": j.files.len(), "counts": counts, "saved": saved,
    })
}

fn full(j: &Job) -> Value {
    let mut v = head(j);
    v["files"] = serde_json::to_value(&j.files).unwrap_or_default();
    v
}

pub fn list(app: &App) -> Value {
    json!({ "jobs": app.jobs.jobs.lock().unwrap().iter().map(full).collect::<Vec<_>>() })
}

/// Headers only, for the queue badge.
pub fn heads(app: &App) -> Value {
    json!({ "jobs": app.jobs.jobs.lock().unwrap().iter().map(head).collect::<Vec<_>>() })
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
}

pub fn add(app: &App, body: &Value) -> Result<Value> {
    if app.jobs.shutting_down.load(Ordering::SeqCst) {
        bail!("av1opt is shutting down");
    }
    if let Some(p) = app.encode_problem() {
        bail!("{p} (see Settings)");
    }
    add_unchecked(app, body)
}

/// Wakes the worker, e.g. after the tools were fixed.
pub fn kick(app: &App) {
    // Under the lock, so the worker can't miss it between its check and its wait.
    let _jobs = app.jobs.jobs.lock().unwrap();
    app.jobs.wake.notify_all();
}

fn add_unchecked(app: &App, body: &Value) -> Result<Value> {
    let r: AddRequest = serde_json::from_value(body.clone())?;
    let options = r.options.unwrap_or_else(|| serde_json::from_value(json!({})).expect("defaults"));
    let opts = options.to_options()?;
    if r.inputs.is_empty() {
        bail!("nothing to encode");
    }
    if !r.out_dir.is_absolute() {
        bail!("pick an output folder");
    }
    // Same safety rule as the command line, without the override.
    crate::fsguard::ensure_writable_location(&r.out_dir, false)?;
    // Canonical paths: no `..` tricks when output folders are derived from scan roots.
    let canon = |p: &PathBuf| std::fs::canonicalize(p).with_context(|| format!("{} does not exist", p.display()));
    let inputs: Vec<PathBuf> = r.inputs.iter().map(canon).collect::<Result<_>>()?;
    let roots: Vec<PathBuf> = r.roots.iter().filter_map(|p| std::fs::canonicalize(p).ok()).collect();
    let mut batch_jobs = Vec::new();
    for input in &inputs {
        let root = roots.iter().filter(|root| input.starts_with(root) && input != *root).max_by_key(|p| p.as_os_str().len());
        let out_dir = match root {
            Some(root) if input.is_file() => {
                let rel = input.parent().and_then(|p| p.strip_prefix(root).ok()).unwrap_or(Path::new(""));
                r.out_dir.join(rel)
            }
            _ => r.out_dir.clone(),
        };
        batch_jobs.extend(batch::expand(std::slice::from_ref(input), &out_dir, is_video));
    }
    if batch_jobs.is_empty() {
        bail!("no video files in the selection");
    }
    for j in &batch_jobs {
        if !j.out_dir.starts_with(&r.out_dir) {
            bail!("{} would be written outside the output folder", j.input.display());
        }
        crate::fsguard::ensure_writable_location(&j.out_dir, false)?;
    }
    let files: Vec<FileState> = batch_jobs
        .iter()
        .map(|j| {
            let stem = j.input.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let output = j.out_dir.join(format!("{stem}.{}", opts.container.ext()));
            let abs = std::fs::canonicalize(&j.input).unwrap_or_else(|_| j.input.clone());
            app.allow(&abs);
            FileState {
                report: output.with_extension("av1opt.json"),
                input: abs,
                output,
                status: "pending".into(),
                input_bytes: std::fs::metadata(&j.input).map_or(0, |m| m.len()),
                ..Default::default()
            }
        })
        .collect();
    let id = {
        let mut n = app.jobs.next_id.lock().unwrap();
        *n += 1;
        *n
    };
    let count = files.len();
    app.jobs.jobs.lock().unwrap().push(Job {
        id,
        created: now(),
        inputs,
        roots,
        out_dir: r.out_dir.clone(),
        options,
        status: "queued".into(),
        error: None,
        current: None,
        files,
        batch: batch_jobs,
        hooks: None,
        cancel_requested: false,
    });
    {
        let mut s = app.settings.lock().unwrap();
        s.out_dir = Some(r.out_dir.clone());
        if !s.history_dirs.contains(&r.out_dir) {
            s.history_dirs.push(r.out_dir.clone());
        }
    }
    app.save_settings();
    persist(app);
    app.jobs.wake.notify_all();
    app.broadcast(json!({ "kind": "jobs" }));
    Ok(json!({ "id": id, "files": count }))
}

fn queue_path() -> Option<PathBuf> {
    super::library::settings_path().map(|p| p.with_file_name("ui-queue.json"))
}

/// Saves unfinished jobs so the next `av1opt ui` picks them up again.
fn persist(app: &App) {
    if app.jobs.shutting_down.load(Ordering::SeqCst) {
        return;
    }
    let pending: Vec<Value> = app
        .jobs
        .jobs
        .lock()
        .unwrap()
        .iter()
        .filter(|j| matches!(j.status.as_str(), "queued" | "running"))
        .map(|j| json!({ "inputs": j.inputs, "roots": j.roots, "out_dir": j.out_dir, "options": j.options }))
        .chain(app.jobs.unrestored.lock().unwrap().iter().cloned())
        .collect();
    if let Some(p) = queue_path() {
        let tmp = p.with_extension("json.tmp");
        if std::fs::write(&tmp, serde_json::to_vec_pretty(&pending).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(tmp, p);
        }
    }
}

/// Re-queues jobs left unfinished by the previous session. Finished files are recognised as
/// done and scene-mode chunks resume, as with a re-run of `av1opt encode`.
pub fn restore(app: &App) {
    let Some(saved) = queue_path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice::<Vec<Value>>(&b).ok()) else {
        return;
    };
    for j in saved {
        // Not `add`: with broken tools the jobs wait in the queue rather than get dropped.
        match add_unchecked(app, &j) {
            Ok(r) => eprintln!("re-queued {} file(s) from the last session", r["files"]),
            Err(e) => {
                eprintln!("could not re-queue a job from the last session (kept for later): {e:#}");
                app.jobs.unrestored.lock().unwrap().push(j);
            }
        }
    }
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
    jobs.retain(|j| j.id != id);
    drop(jobs);
    persist(app);
    app.broadcast(json!({ "kind": "jobs" }));
    Ok(json!({ "ok": true }))
}

pub fn clear_finished(app: &App) -> Result<Value> {
    app.jobs.jobs.lock().unwrap().retain(|j| matches!(j.status.as_str(), "queued" | "running"));
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
            f.phase = "probing".into();
        }
        Event::Source { .. } => f.source = Some(v),
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
            let (id, opts, retry, out_dir, workers) = loop {
                // Checked before taking the lock: probing runs ffmpeg. With unusable tools,
                // queued jobs wait (the page shows why) instead of failing.
                let tools_ok = app.encode_problem().is_none();
                let mut jobs = app.jobs.jobs.lock().unwrap();
                if tools_ok
                    && !app.jobs.shutting_down.load(Ordering::SeqCst)
                    && let Some(j) = jobs.iter_mut().find(|j| j.status == "queued")
                {
                    j.status = "running".into();
                    break (j.id, j.options.to_options(), j.options.retry_skipped, j.out_dir.clone(), j.options.workers);
                }
                // Woken by new jobs and tool changes; the timeout is only a safety net.
                drop(app.jobs.wake.wait_timeout(jobs, std::time::Duration::from_secs(30)).unwrap());
            };
            app.broadcast(json!({ "kind": "jobs" }));
            let result = run_job(&app, id, opts, retry, &out_dir, workers);
            let mut jobs = app.jobs.jobs.lock().unwrap();
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
                    Ok(_) => j.status = "done".into(),
                    Err(e) => {
                        j.status = "failed".into();
                        j.error = Some(format!("{e:#}"));
                    }
                }
            }
            drop(jobs);
            persist(&app);
            app.broadcast(json!({ "kind": "jobs" }));
        }
    });
}

fn run_job(app: &Arc<App>, id: u64, opts: Result<Options>, retry: bool, out_dir: &Path, workers: u8) -> Result<batch::Summary> {
    let opts = opts?;
    crate::fsguard::ensure_writable_location(out_dir, false)?;
    std::fs::create_dir_all(out_dir)?;
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
                    sink_app.allow(&j.files[i].output);
                    sink_app.allow(&j.files[i].input);
                }
                Some(i)
            })
        };
        sink_app.broadcast(json!({ "kind": "job", "id": id, "file": file, "event": e }));
    }));
    let mut tools = app.tools(if opts.scenes { workers } else { 1 });
    tools.hooks = Some(hooks.clone());
    // Take the batch list out of the job so the lock isn't held while encoding.
    let batch_jobs = {
        let mut jobs = app.jobs.jobs.lock().unwrap();
        let Some(j) = jobs.iter_mut().find(|j| j.id == id) else { bail!("job vanished") };
        if j.cancel_requested {
            hooks.cancel();
        }
        j.hooks = Some(hooks);
        std::mem::take(&mut j.batch)
    };
    let r = batch::run(&tools, &batch_jobs, out_dir, &opts, retry);
    if let Some(j) = app.jobs.jobs.lock().unwrap().iter_mut().find(|j| j.id == id) {
        j.batch = batch_jobs;
    }
    r
}
