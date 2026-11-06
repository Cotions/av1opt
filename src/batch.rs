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

/// The settings a skip decision depends on. A skip made with other settings (say a faster
/// preset that can't reach the target) doesn't stop a run with these settings.
fn settings_key(o: &Options) -> String {
    let mut k = format!("vmaf={} preset={} crf={}-{} min_savings={} force={}", o.target_vmaf, o.preset, o.min_crf, o.max_crf, o.min_savings, o.force);
    if !o.remove.is_empty() {
        let r: Vec<String> = o.remove.iter().map(|(a, b)| format!("{a}-{b}")).collect();
        k.push_str(&format!(" remove={}", r.join(",")));
    }
    k
}

/// Previous skip decisions, keyed by absolute input path and size (a changed file is retried).
/// Lines from older versions carry no settings and count for any settings.
fn read_skips(log: &Path, settings: &str) -> HashMap<(PathBuf, u64), String> {
    let Ok(text) = std::fs::read_to_string(log) else { return HashMap::new() };
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            if f.len() < 5 || f[1] != "skip" || f.get(5).is_some_and(|s| *s != settings) {
                return None;
            }
            Some(((PathBuf::from(f[2]), f[3].parse().ok()?), f[4].to_string()))
        })
        .collect()
}

fn log_line(log: &Path, status: &str, input: &Path, size: u64, detail: &str, settings: &str) -> Result<()> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let clean = |s: &str| s.replace(['\t', '\n'], " ");
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    writeln!(f, "{now}\t{status}\t{}\t{size}\t{}\t{settings}", clean(&input.display().to_string()), clean(detail))?;
    Ok(())
}

/// Absolute input path recorded in an output's `.av1opt.json` report, and the sections
/// that output left out.
fn report_input(output: &Path) -> Option<(PathBuf, Vec<(f64, f64)>)> {
    let r: serde_json::Value = serde_json::from_slice(&std::fs::read(output.with_extension("av1opt.json")).ok()?).ok()?;
    let p = PathBuf::from(r["input"].as_str()?);
    let removed = serde_json::from_value(r["removed"].clone()).unwrap_or_default();
    Some((std::fs::canonicalize(&p).ok().or_else(|| std::path::absolute(&p).ok())?, removed))
}

/// Whether an earlier output left out the sections asked for now. The report holds them
/// sorted, merged and clipped to the video's length, so compare in that form.
fn same_sections(asked: &[(f64, f64)], done: &[(f64, f64)]) -> bool {
    let mut a = asked.to_vec();
    a.sort_by(|x, y| x.0.total_cmp(&y.0));
    let mut merged: Vec<(f64, f64)> = Vec::new();
    for (s, e) in a {
        match merged.last_mut() {
            Some(l) if s <= l.1 => l.1 = l.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    merged.len() == done.len()
        && merged.iter().zip(done).all(|(x, y)| (x.0 - y.0).abs() < 0.002 && ((x.1 - y.1).abs() < 0.002 || (x.1 > y.1 && x == merged.last().unwrap())))
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
    let settings = settings_key(o);
    let skips = if retry_skipped { HashMap::new() } else { read_skips(&log, &settings) };
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
                Some((_, removed)) if !same_sections(&o.remove, &removed) => {
                    let msg = format!("{} exists but has other sections removed; move or delete it to encode again", output.display());
                    eprintln!("FAILED: {msg}");
                    end("error", &msg, 0);
                    sum.failed += 1;
                    log_line(&log, "error", &abs, size, &msg, &settings)?;
                }
                Some((src, _)) if src != abs => {
                    let msg = format!("output name collision: {} was made from {}", output.display(), src.display());
                    eprintln!("FAILED: {msg}");
                    end("error", &msg, 0);
                    sum.failed += 1;
                    log_line(&log, "error", &abs, size, &msg, &settings)?;
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
                log_line(&log, "ok", &abs, size, &out.display().to_string(), &settings)?;
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
                log_line(&log, "skip", &abs, size, &e.to_string(), &settings)?;
            }
            Err(e) => {
                eprintln!("FAILED: {e:#}");
                end("error", &format!("{e:#}"), 0);
                sum.failed += 1;
                log_line(&log, "error", &abs, size, &format!("{e:#}"), &settings)?;
            }
        }
    }
    Ok(sum)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_only_count_for_the_same_settings() {
        let dir = std::env::temp_dir().join(format!("av1opt-skiptest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join(LOG_NAME);
        let p = Path::new("/videos/a.mp4");
        log_line(&log, "skip", p, 10, "too lean", "vmaf=93 preset=10").unwrap();
        // Older format without settings.
        std::fs::OpenOptions::new().append(true).open(&log).map(|mut f| writeln!(f, "1\tskip\t/videos/b.mp4\t20\told")).unwrap().unwrap();
        let same = read_skips(&log, "vmaf=93 preset=10");
        let other = read_skips(&log, "vmaf=93 preset=6");
        assert!(same.contains_key(&(p.to_path_buf(), 10)));
        assert!(!other.contains_key(&(p.to_path_buf(), 10)));
        assert!(other.contains_key(&(PathBuf::from("/videos/b.mp4"), 20)));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
