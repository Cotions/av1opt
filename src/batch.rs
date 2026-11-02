//! Batch encoding: expands folders, mirrors their layout under the output folder, logs every
//! result to `av1opt-log.tsv`, and resumes by skipping finished or previously skipped files.

use crate::encode::{self, Options, Skip};
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
                    !abs.starts_with(&out_abs) && !e.file_name().to_string_lossy().starts_with(".av1opt-tmp-")
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

#[derive(Default)]
pub struct Summary {
    pub ok: usize,
    pub skipped: usize,
    pub failed: usize,
    pub already_done: usize,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

pub fn run(tools: &Tools, jobs: &[Job], out_dir: &Path, o: &Options, retry_skipped: bool) -> Result<Summary> {
    let log = out_dir.join(LOG_NAME);
    let skips = if retry_skipped { HashMap::new() } else { read_skips(&log) };
    let mut sum = Summary::default();
    for (i, job) in jobs.iter().enumerate() {
        let abs = std::path::absolute(&job.input)?;
        let size = std::fs::metadata(&job.input).map_or(0, |m| m.len());
        let stem = job.input.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let output = job.out_dir.join(format!("{stem}.{}", o.container.ext()));
        eprintln!("\n[{}/{}] {}", i + 1, jobs.len(), job.input.display());

        if output.exists() && !o.overwrite {
            eprintln!("already done: {}", output.display());
            sum.already_done += 1;
            continue;
        }
        if let Some(reason) = skips.get(&(abs.clone(), size)) {
            eprintln!("skipped earlier: {reason} (use --retry-skipped to try again)");
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
                log_line(&log, "ok", &abs, size, &out.display().to_string())?;
            }
            Err(e) if e.downcast_ref::<Skip>().is_some() => {
                eprintln!("skipped: {e}");
                sum.skipped += 1;
                log_line(&log, "skip", &abs, size, &e.to_string())?;
            }
            Err(e) => {
                eprintln!("FAILED: {e:#}");
                sum.failed += 1;
                log_line(&log, "error", &abs, size, &format!("{e:#}"))?;
            }
        }
    }
    Ok(sum)
}
