//! Benchmark of encoder settings for the page: runs in the background with progress events,
//! shows results while videos are still being measured, keeps the last finished result between
//! sessions, and saves the settings picked from it for `tuned` encodes.

use super::{App, library, now};
use crate::bench::{self, Config, Progress, Report, Video};
use crate::events::Hooks;
use crate::tuning::{Class, Tuning};
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub struct Run {
    id: u64,
    paths: Vec<PathBuf>,
    /// running, done, failed, cancelled
    status: String,
    error: Option<String>,
    /// probing, sampling, measuring
    phase: &'static str,
    started: u64,
    configs: Vec<Config>,
    target_vmaf: f64,
    /// Videos chosen, and settings finished over all of them (of videos x settings).
    videos_total: usize,
    cells_done: usize,
    /// What is being measured right now: video index and path, setting index, last trial.
    current: Value,
    /// Finished videos, for results while the run goes on.
    videos: Vec<Video>,
    report: Option<Arc<Report>>,
    hooks: Arc<Hooks>,
}

#[derive(Deserialize)]
struct Request {
    paths: Vec<PathBuf>,
    #[serde(default = "d_presets")]
    presets: Vec<u8>,
    #[serde(default)]
    param_sets: Vec<String>,
    #[serde(default = "d_vmaf")]
    target_vmaf: f64,
    #[serde(default = "d_videos")]
    max_videos: usize,
    #[serde(default = "d_samples")]
    samples: usize,
    #[serde(default = "d_secs")]
    sample_secs: f64,
    #[serde(default)]
    refresh: bool,
}

fn d_presets() -> Vec<u8> {
    bench::DEFAULT_PRESETS.to_vec()
}
fn d_vmaf() -> f64 {
    93.0
}
fn d_videos() -> usize {
    6
}
fn d_samples() -> usize {
    2
}
fn d_secs() -> f64 {
    6.0
}

fn head(r: &Run) -> Value {
    json!({
        "id": r.id, "paths": r.paths, "status": r.status, "error": r.error, "phase": r.phase, "started": r.started,
        "configs": r.configs, "target_vmaf": r.target_vmaf,
        "videos_total": r.videos_total, "videos_done": r.videos.len(), "cells_done": r.cells_done,
        "cells_total": r.videos_total * r.configs.len(), "current": r.current,
    })
}

/// True while a benchmark is measuring: encodes wait, since they would spoil its timings.
pub fn running(app: &App) -> bool {
    app.bench.lock().unwrap().as_ref().is_some_and(|r| r.status == "running")
}

fn last_path() -> Option<PathBuf> {
    Some(library::settings_path()?.with_file_name("bench-last.json"))
}

/// The last finished result of an earlier session.
fn saved_report() -> Option<Report> {
    let r: Report = serde_json::from_slice(&std::fs::read(last_path()?).ok()?).ok()?;
    (r.format <= bench::REPORT_FORMAT).then_some(r)
}

pub fn start(app: &Arc<App>, body: &Value) -> Result<Value> {
    let r: Request = serde_json::from_value(body.clone())?;
    if r.paths.is_empty() {
        bail!("add at least one folder or video");
    }
    if !(50.0..=99.5).contains(&r.target_vmaf) {
        bail!("target VMAF must be between 50 and 99.5");
    }
    if r.presets.is_empty() || r.presets.iter().any(|p| *p > 13) {
        bail!("pick presets between 0 and 13");
    }
    if !(1..=50).contains(&r.max_videos) || !(1..=8).contains(&r.samples) || !(2.0..=30.0).contains(&r.sample_secs) {
        bail!("videos must be 1-50, clips per video 1-8, clip length 2-30 s");
    }
    let mut param_sets: Vec<String> = Vec::new();
    for p in r.param_sets.iter().map(|p| p.trim()).filter(|p| !p.is_empty()) {
        param_sets.push(crate::search::parse_params(p)?);
    }
    if param_sets.is_empty() {
        param_sets = bench::DEFAULT_PARAM_SETS.map(String::from).to_vec();
    }
    let configs = bench::matrix(&r.presets, &param_sets);
    if configs.len() > 60 {
        bail!("{} settings is more than a benchmark can get through; pick fewer presets or parameter sets", configs.len());
    }
    if super::jobs::shutting_down(app) {
        bail!("av1opt is shutting down");
    }
    if let Some(p) = app.encode_problem() {
        bail!("{p} (see Settings)");
    }
    if super::jobs::encoding(app) {
        bail!("an encode is running; a benchmark at the same time would measure wrong encode times. Wait for it or stop it first");
    }
    let paths: Vec<PathBuf> = r
        .paths
        .iter()
        .map(|p| std::fs::canonicalize(p).with_context(|| format!("cannot open {}", p.display())).and_then(super::text_path))
        .collect::<Result<_>>()?;
    let hooks = Arc::new(Hooks::new(|_| {}));
    let id = {
        let mut run = app.bench.lock().unwrap();
        if run.as_ref().is_some_and(|x| x.status == "running") {
            bail!("a benchmark is already running");
        }
        let id = run.as_ref().map_or(0, |x| x.id) + 1;
        *run = Some(Run {
            id, paths: paths.clone(), status: "running".into(), error: None, phase: "probing", started: now(), configs: configs.clone(),
            target_vmaf: r.target_vmaf, videos_total: 0, cells_done: 0, current: Value::Null, videos: Vec::new(), report: None, hooks: hooks.clone(),
        });
        id
    };
    {
        let mut s = app.settings.lock().unwrap();
        s.bench_paths = paths.clone();
    }
    app.save_settings();

    let app = app.clone();
    std::thread::spawn(move || {
        let mut tools = app.tools(1);
        tools.hooks = Some(hooks);
        let last = Mutex::new(Instant::now());
        let progress = |p: Progress| {
            let mut guard = app.bench.lock().unwrap();
            let Some(run) = guard.as_mut().filter(|x| x.id == id) else { return };
            // Probing reports every file; the rest is seconds apart at best.
            let mut throttled = false;
            match p {
                Progress::Probing { done, total } => {
                    run.current = json!({ "done": done, "total": total });
                    throttled = done != total;
                }
                Progress::Plan { videos, .. } => {
                    run.videos_total = videos;
                    run.phase = "measuring";
                    run.current = Value::Null;
                }
                Progress::Sampling { video, path } => {
                    run.phase = "sampling";
                    run.current = json!({ "video": video, "path": path });
                }
                Progress::Trial { video, config, measure, cached } => {
                    run.phase = "measuring";
                    run.current = json!({ "video": video, "config": config, "crf": measure.crf, "vmaf": measure.vmaf, "cached": cached });
                    // Remembered results arrive by the hundred.
                    throttled = cached;
                }
                Progress::Cell { .. } => run.cells_done += 1,
                Progress::VideoDone { video, .. } => run.videos.push(video.clone()),
            }
            let msg = json!({ "kind": "bench", "run": head(run) });
            drop(guard);
            let mut l = last.lock().unwrap();
            if !throttled || l.elapsed().as_millis() > 250 {
                *l = Instant::now();
                app.broadcast(msg);
            }
        };
        let opts = bench::Options {
            configs, target_vmaf: r.target_vmaf, min_crf: 20, max_crf: 55, samples: r.samples, sample_secs: r.sample_secs,
            max_videos: r.max_videos, jobs: app.cfg.jobs.clamp(1, 64), refresh: r.refresh, work_dir: None,
        };
        // A crash must not leave the run "running": the queue would wait for it forever.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| bench::run(&paths, &tools, &opts, &progress)))
            .unwrap_or_else(|_| Err(anyhow!("the benchmark crashed; the terminal or log has the details")));
        {
            let mut guard = app.bench.lock().unwrap();
            if let Some(run) = guard.as_mut().filter(|x| x.id == id) {
                run.current = Value::Null;
                match result {
                    Ok(report) => {
                        if let Some(p) = last_path() {
                            let _ = p.parent().map(std::fs::create_dir_all);
                            let _ = std::fs::write(&p, serde_json::to_vec(&report).unwrap_or_default());
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
        app.broadcast(json!({ "kind": "bench_done", "id": id }));
        // Encodes that waited for the benchmark can go now.
        super::jobs::kick(&app);
    });
    Ok(json!({ "id": id }))
}

/// What a result looks like while videos are still being measured (or after a run was
/// stopped): the finished videos, compared like a full report.
fn partial(r: &Run) -> Value {
    json!({
        "partial": true,
        "configs": r.configs,
        "baseline": 0,
        "target_vmaf": r.target_vmaf,
        "videos": r.videos,
        "summaries": bench::summaries(&r.videos, r.configs.len(), 0),
        "skipped": [],
    })
}

/// The result the page shows: this session's finished run, else what a running or stopped run
/// has measured so far, else the last finished result of an earlier run.
enum Shown {
    Full(Arc<Report>),
    Partial(Value),
    /// A run is going and no video is finished yet.
    Nothing,
    Saved,
}

fn shown(app: &App) -> (Value, Shown) {
    let guard = app.bench.lock().unwrap();
    match guard.as_ref() {
        None => (Value::Null, Shown::Saved),
        Some(r) => {
            let what = match &r.report {
                Some(report) => Shown::Full(report.clone()),
                None if !r.videos.is_empty() => Shown::Partial(partial(r)),
                None if r.status == "running" => Shown::Nothing,
                None => Shown::Saved,
            };
            (head(r), what)
        }
    }
}

/// The current run (if any) with its results so far, else the last finished result, plus
/// the tuned settings and what a new run would use by default.
pub fn get(app: &App) -> Result<Value> {
    // Serialized and read from disk without the lock held.
    let (run, what) = shown(app);
    let report = match what {
        Shown::Full(r) => json!(&*r),
        Shown::Partial(p) => p,
        Shown::Nothing => Value::Null,
        Shown::Saved => json!(saved_report()),
    };
    Ok(json!({
        "run": run,
        "report": report,
        "tuning": tuning_json(&Tuning::load().unwrap_or_default()),
        "defaults": { "presets": bench::DEFAULT_PRESETS, "param_sets": bench::DEFAULT_PARAM_SETS, "baseline": Config::baseline() },
    }))
}

/// Tuned settings as a list in class order, with labels for the page.
pub fn tuning_json(t: &Tuning) -> Value {
    json!(Class::ALL.iter().filter_map(|c| t.classes.get(c).map(|x| json!({ "class": c, "label": c.label(), "choice": x }))).collect::<Vec<_>>())
}

pub fn cancel(app: &App) -> Result<Value> {
    if let Some(r) = app.bench.lock().unwrap().as_ref().filter(|r| r.status == "running") {
        r.hooks.cancel();
    }
    Ok(json!({ "ok": true }))
}

/// Stops a running benchmark so no ffmpeg outlives the server.
pub fn shutdown(app: &App) {
    if let Some(r) = app.bench.lock().unwrap().as_ref() {
        r.hooks.cancel();
    }
}

#[derive(Deserialize)]
struct TuneRequest {
    /// The classes to change; empty means every class the result has videos for.
    #[serde(default)]
    classes: Vec<Class>,
    /// Index of the setting in the last result to use; None forgets the classes' settings.
    #[serde(default)]
    config: Option<usize>,
}

/// Makes a setting of the last result the tuned one for some resolution classes (with the
/// numbers measured for each class), or forgets them.
pub fn tune(app: &App, body: &Value) -> Result<Value> {
    let r: TuneRequest = serde_json::from_value(body.clone())?;
    let mut t = Tuning::load()?;
    match r.config {
        None => {
            if r.classes.is_empty() {
                t.classes.clear();
            }
            for c in &r.classes {
                t.classes.remove(c);
            }
        }
        Some(config) => {
            // The setting is an index into the result on the page, so it must be that result.
            let report = match shown(app).1 {
                Shown::Full(r) => r,
                Shown::Saved => Arc::new(saved_report().ok_or_else(|| anyhow!("no finished benchmark to take settings from"))?),
                Shown::Partial(_) | Shown::Nothing => bail!("the benchmark hasn't measured every video; let one finish first (what was measured is remembered)"),
            };
            let mut set = 0;
            for s in report.summaries.iter() {
                let Some(class) = s.class else { continue };
                if !r.classes.is_empty() && !r.classes.contains(&class) {
                    continue;
                }
                let row = s.rows.iter().find(|x| x.config == config).ok_or_else(|| anyhow!("no such setting in the result"))?;
                if row.missing > 0 {
                    bail!("that setting has no result on some {} videos", class.label());
                }
                t.classes.insert(class, bench::choice(&report, s, config).ok_or_else(|| anyhow!("no such setting in the result"))?);
                set += 1;
            }
            if set == 0 {
                bail!("the result has no videos of that size to judge the setting by");
            }
        }
    }
    t.save()?;
    Ok(json!({ "tuning": tuning_json(&t) }))
}
