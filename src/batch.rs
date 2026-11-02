//! Batch encoding: expands folders, mirrors their layout under the output folder, logs every
//! result to `av1opt-log.tsv`, and resumes by skipping finished or previously skipped files.

use crate::encode::{self, Options, Skip};
use crate::events::{self, Event};
use crate::tools::Tools;
use anyhow::Result;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const LOG_NAME: &str = "av1opt-log.tsv";

/// One input file and the folder its output goes to.
pub struct Job {
    pub input: PathBuf,
    pub out_dir: PathBuf,
}

/// Files are used as given; folders are walked and their structure is kept below `out_dir`.
pub fn expand(inputs: &[PathBuf], out_dir: &Path, is_video: impl Fn(&Path) -> bool) -> Vec<Job> {
    let out_abs = std::path::absolute(out_dir).unwrap_or_else(|_| out_dir.to_path_buf());
    let mut jobs = Vec::new();
    for input in inputs {
        if input.is_dir() {
            let mut files: Vec<PathBuf> = walkdir::WalkDir::new(input)
                .into_iter()
                .filter_entry(|e| {
                    // Never descend into our own output or temp folders.
                    let abs = std::path::absolute(e.path()).unwrap_or_else(|_| e.path().to_path_buf());
                    !abs.starts_with(&out_abs) && !e.file_name().to_string_lossy().starts_with(".av1opt-")
                })
                .flatten()
                .filter(|e| e.file_type().is_file() && is_video(e.path()))
                .map(|e| e.into_path())
                .collect();
            files.sort();
            for f in files {
                let rel_dir = f.parent().and_then(|p| p.strip_prefix(input).ok()).unwrap_or(Path::new(""));
                jobs.push(Job { out_dir: out_dir.join(rel_dir), input: f });
            }
        } else {
            jobs.push(Job { input: input.clone(), out_dir: out_dir.to_path_buf() });
        }
    }
    jobs
}

/// Previous skip decisions, keyed by absolute input path and size (a changed file is retried).
fn read_skips(log: &Path) -> HashMap<(PathBuf, u64), String> {
    let Ok(text) = std::fs::read_to_string(log) else { return HashMap::new() };
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            if f.len() < 5 || f[1] != "skip" {
                return None;
            }
            Some(((PathBuf::from(f[2]), f[3].parse().ok()?), f[4].to_string()))
        })
        .collect()
}

fn log_line(log: &Path, status: &str, input: &Path, size: u64, detail: &str) -> Result<()> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let clean = |s: &str| s.replace(['\t', '\n'], " ");
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    writeln!(f, "{now}\t{status}\t{}\t{size}\t{}", clean(&input.display().to_string()), clean(detail))?;
    Ok(())
}

/// Absolute input path recorded in an output's `.av1opt.json` report.
fn report_input(output: &Path) -> Option<PathBuf> {
    let r: serde_json::Value = serde_json::from_slice(&std::fs::read(output.with_extension("av1opt.json")).ok()?).ok()?;
    let p = PathBuf::from(r["input"].as_str()?);
    std::fs::canonicalize(&p).ok().or_else(|| std::path::absolute(&p).ok())
}

#[derive(Default)]
pub struct Summary {
    pub ok: usize,
    pub skipped: usize,
    pub failed: usize,
    pub already_done: usize,
    pub bytes_in: u64,
    pub bytes_out: u64,
    /// The user stopped the run; remaining files were not started.
    pub cancelled: bool,
}

pub fn run(tools: &Tools, jobs: &[Job], out_dir: &Path, o: &Options, retry_skipped: bool) -> Result<Summary> {
    let log = out_dir.join(LOG_NAME);
    let skips = if retry_skipped { HashMap::new() } else { read_skips(&log) };
    let mut sum = Summary::default();
    for (i, job) in jobs.iter().enumerate() {
        let abs = std::fs::canonicalize(&job.input).or_else(|_| std::path::absolute(&job.input))?;
        let size = std::fs::metadata(&job.input).map_or(0, |m| m.len());
        let stem = job.input.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let output = job.out_dir.join(format!("{stem}.{}", o.container.ext()));
        eprintln!("\n[{}/{}] {}", i + 1, jobs.len(), job.input.display());
        if tools.check_cancel().is_err() {
            sum.cancelled = true;
            break;
        }
        tools.emit(Event::FileStart { index: i, total: jobs.len(), input: abs.clone(), output: output.clone() });
        let end = |status: &str, detail: &str, out_bytes: u64| {
            tools.emit(Event::FileEnd { index: i, status: status.into(), detail: detail.into(), input_bytes: size, output_bytes: out_bytes });
        };

        if output.exists() && !o.overwrite {
            // Same file name from two input folders would map to one output; only count it as
            // done if its report says it came from this input.
            match report_input(&output) {
                Some(src) if src != abs => {
                    let msg = format!("output name collision: {} was made from {}", output.display(), src.display());
                    eprintln!("FAILED: {msg}");
                    end("error", &msg, 0);
                    sum.failed += 1;
                    log_line(&log, "error", &abs, size, &msg)?;
                }
                _ => {
                    eprintln!("already done: {}", output.display());
                    end("done_before", &output.display().to_string(), std::fs::metadata(&output).map_or(0, |m| m.len()));
                    sum.already_done += 1;
                }
            }
            continue;
        }
        if let Some(reason) = skips.get(&(abs.clone(), size)) {
            eprintln!("skipped earlier: {reason} (use --retry-skipped to try again)");
            end("skip", &format!("skipped earlier: {reason}"), 0);
            sum.skipped += 1;
            continue;
        }

        encode::clean_stale_temp(&job.out_dir);
        match encode::optimize(tools, &job.input, &job.out_dir, o) {
            Ok(out) => {
                let out_size = std::fs::metadata(&out).map_or(0, |m| m.len());
                sum.ok += 1;
                sum.bytes_in += size;
                sum.bytes_out += out_size;
                end("ok", &out.display().to_string(), out_size);
                log_line(&log, "ok", &abs, size, &out.display().to_string())?;
            }
            Err(e) if events::is_cancelled(&e) => {
                // Not logged: the next run picks the file up again (scene work resumes).
                eprintln!("cancelled");
                end("cancelled", "", 0);
                sum.cancelled = true;
                break;
            }
            Err(e) if e.downcast_ref::<Skip>().is_some() => {
                eprintln!("skipped: {e}");
                end("skip", &e.to_string(), 0);
                sum.skipped += 1;
                log_line(&log, "skip", &abs, size, &e.to_string())?;
            }
            Err(e) => {
                eprintln!("FAILED: {e:#}");
                end("error", &format!("{e:#}"), 0);
                sum.failed += 1;
                log_line(&log, "error", &abs, size, &format!("{e:#}"))?;
            }
        }
    }
    Ok(sum)
}
