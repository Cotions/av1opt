//! Batch encoding: expands folders, mirrors their layout under the output folder, logs every
//! result to `av1opt-log.tsv`, and resumes by skipping finished or previously skipped files.

use crate::encode::{self, Options, Skip};
use crate::events::{self, Event};
use crate::tools::Tools;
use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const LOG_NAME: &str = "av1opt-log.tsv";

/// One input file and the folder its output goes to.
pub struct Job {
    pub input: PathBuf,
    pub out_dir: PathBuf,
}

/// Where the files of the folder `input` go. Given alone, a folder's contents sit directly
/// below `out_dir`. Given with other folders, each gets a folder of its own name there:
/// two libraries encoded in one run stay apart, as they are where they came from.
pub fn folder_out(inputs: &[PathBuf], input: &Path, out_dir: &Path) -> PathBuf {
    let folders = inputs.iter().filter(|p| p.is_dir()).count();
    let abs = std::path::absolute(input).unwrap_or_else(|_| input.to_path_buf());
    match abs.file_name() {
        Some(name) if folders > 1 => out_dir.join(name),
        _ => out_dir.to_path_buf(),
    }
}

/// Files are used as given; folders are walked and their structure is kept below `out_dir`
/// (below a folder of their own name there when several are given, see [`folder_out`]).
pub fn expand(inputs: &[PathBuf], out_dir: &Path, is_video: impl Fn(&Path) -> bool) -> Vec<Job> {
    // Folders as the system resolves them: an output folder named through a link or `..`
    // is the same folder, and two strings would not say so.
    let real = |p: &Path| std::fs::canonicalize(p).or_else(|_| std::path::absolute(p)).unwrap_or_else(|_| p.to_path_buf());
    let out_abs = real(out_dir);
    let mut jobs = Vec::new();
    for input in inputs {
        if input.is_dir() {
            let below = folder_out(inputs, input, out_dir);
            // A folder that lies in the output folder (or is it) is still walked: only an
            // output folder below the input is left out.
            let inside = real(input).starts_with(&out_abs);
            let mut files: Vec<PathBuf> = walkdir::WalkDir::new(input)
                .into_iter()
                .filter_entry(|e| {
                    // Never descend into our own output or temp folders.
                    let ours = !inside && e.file_type().is_dir() && real(e.path()).starts_with(&out_abs);
                    !ours && !e.file_name().to_string_lossy().starts_with(".av1opt-")
                })
                .flatten()
                .filter(|e| e.file_type().is_file() && is_video(e.path()))
                .map(|e| e.into_path())
                .collect();
            files.sort();
            for f in files {
                let rel_dir = f.parent().and_then(|p| p.strip_prefix(input).ok()).unwrap_or(Path::new(""));
                jobs.push(Job { out_dir: below.join(rel_dir), input: f });
            }
        } else {
            jobs.push(Job { input: input.clone(), out_dir: out_dir.to_path_buf() });
        }
    }
    jobs
}

/// Which file of a batch is encoded first.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Order {
    /// As given: files in the order named, folders by path.
    #[default]
    Given,
    /// The file with the largest estimated saving first.
    Savings,
    /// The file with the largest estimated saving per hour of encoding first: stopping a
    /// long run early then leaves the most space saved for the time spent.
    PerHour,
}

/// Puts the jobs with the highest `worth` first. Jobs of equal worth (and the ones it knows
/// nothing about, worth 0) keep their order.
pub fn order_by(jobs: &mut [Job], worth: impl Fn(&Path) -> f64) {
    jobs.sort_by_cached_key(|j| std::cmp::Reverse(Sortable(worth(&j.input))));
}

/// A float that sorts, with NaN counted as nothing.
#[derive(PartialEq)]
struct Sortable(f64);
impl Eq for Sortable {}
impl PartialOrd for Sortable {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Sortable {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let clean = |x: f64| if x.is_nan() { 0.0 } else { x };
        clean(self.0).total_cmp(&clean(other.0))
    }
}

/// The settings a skip decision depends on. A skip made with other settings (say a faster
/// preset that can't reach the target) doesn't stop a run with these settings.
fn settings_key(o: &Options) -> String {
    let mut k = format!("vmaf={} preset={} crf={}-{} min_savings={} force={}", o.target_vmaf, o.preset, o.min_crf, o.max_crf, o.min_savings, o.force);
    if !o.remove.is_empty() {
        let r: Vec<String> = o.remove.iter().map(|(a, b)| format!("{a}-{b}")).collect();
        k.push_str(&format!(" remove={}", r.join(",")));
    }
    if o.svt_params != crate::search::DEFAULT_PARAMS {
        k.push_str(&format!(" svt={}", o.svt_params));
    }
    if let Some(t) = &o.tuning {
        let picks: Vec<String> = t.classes.iter().map(|(c, x)| format!("{}:{}:{}", c.id(), x.preset, x.params)).collect();
        k.push_str(&format!(" tuned={}", picks.join(",")));
    }
    // What else a skip can turn on: a CRF per scene holds a stretch one CRF cannot, lighter
    // sound makes a smaller file, other samples another search. (Named only when set, so
    // that the skips in the logs there are keep their key.)
    match o.scenes {
        encode::Scenes::Off => {}
        encode::Scenes::Auto => k.push_str(" scenes=auto"),
        encode::Scenes::Always => k.push_str(" scenes=always"),
    }
    if o.audio != encode::Audio::Keep {
        k.push_str(" audio=shrink");
    }
    if let Some(n) = o.samples {
        k.push_str(&format!(" samples={n}"));
    }
    if let Some(s) = o.sample_secs {
        k.push_str(&format!(" sample_secs={s}"));
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
    let line = format!("{now}\t{status}\t{}\t{size}\t{}\t{settings}", clean(&input.display().to_string()), clean(detail));
    Ok(crate::files::append_line(log, &line, false)?)
}

/// What an output's report says it was made from: the input, the sections left out, and
/// the size the input had.
type Made = (PathBuf, Vec<(f64, f64)>, Option<u64>);

/// Absolute input path recorded in an output's `.av1opt.json` report, the sections that
/// output left out, and the size the input had.
fn report_input(output: &Path) -> Option<Made> {
    let r: serde_json::Value = serde_json::from_slice(&std::fs::read(output.with_extension("av1opt.json")).ok()?).ok()?;
    let p = PathBuf::from(r["input"].as_str()?);
    let removed = serde_json::from_value(r["removed"].clone()).unwrap_or_default();
    Some((std::fs::canonicalize(&p).ok().or_else(|| std::path::absolute(&p).ok())?, removed, r["source_bytes"].as_u64()))
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
                Some((_, removed, _)) if !same_sections(&o.remove, &removed) => {
                    let msg = format!("{} exists but has other sections removed; move or delete it to encode again", output.display());
                    eprintln!("FAILED: {msg}");
                    end("error", &msg, 0);
                    sum.failed += 1;
                    log_line(&log, "error", &abs, size, &msg, &settings)?;
                }
                Some((src, _, _)) if src != abs => {
                    let msg = format!("output name collision: {} was made from {}", output.display(), src.display());
                    eprintln!("FAILED: {msg}");
                    end("error", &msg, 0);
                    sum.failed += 1;
                    log_line(&log, "error", &abs, size, &msg, &settings)?;
                }
                // The same name and another file: a download done again, an edit saved over it.
                Some((_, _, Some(then))) if then != size => {
                    let msg = format!(
                        "{} was made from this file when it was another size ({then} bytes, now {size}); move or delete it to encode again",
                        output.display()
                    );
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
        match guarded(|| encode::optimize(tools, &job.input, &job.out_dir, o)) {
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

/// Runs the work on one file so that a bug in av1opt itself (a panic) fails that file and
/// not the hundred queued behind it. What the encode had open is closed on the way out as
/// after any other error: temporary folders go, child processes are stopped.
pub(crate) fn guarded<T>(work: impl FnOnce() -> Result<T>) -> Result<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)) {
        Ok(r) => r,
        Err(p) => {
            let what = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "no message".into());
            Err(anyhow::anyhow!("av1opt itself went wrong here (a bug in the program, not in the file): {what}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn a_bug_fails_one_file_not_the_run() {
        let quiet = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let by_text = guarded(|| -> Result<()> { panic!("index {} out of range", 7) });
        let by_unwrap = guarded(|| -> Result<u8> { Ok([1u8][std::hint::black_box(4)]) });
        std::panic::set_hook(quiet);
        let e = by_text.unwrap_err().to_string();
        assert!(e.starts_with("av1opt itself went wrong here") && e.ends_with("index 7 out of range"), "{e}");
        assert!(by_unwrap.is_err());
        assert_eq!(guarded(|| Ok(3)).unwrap(), 3);
        assert_eq!(guarded(|| -> Result<()> { anyhow::bail!("plain") }).unwrap_err().to_string(), "plain");
    }

    fn options() -> Options {
        Options {
            container: encode::Container::Mp4, target_vmaf: 93.0, preset: 5, svt_params: crate::search::DEFAULT_PARAMS.into(), tuning: None,
            min_crf: 20, max_crf: 55, samples: None, sample_secs: None, min_savings: 0.15, force: false, overwrite: false, keep_temp: false,
            scenes: encode::Scenes::Off, workers: 1, meta: Default::default(), remove: Vec::new(), audio: encode::Audio::Keep, fresh_search: false,
        }
    }

    #[test]
    fn a_skip_is_keyed_by_what_it_turned_on() {
        // The key of the logs there are: a skip made with the defaults still counts.
        assert_eq!(settings_key(&options()), "vmaf=93 preset=5 crf=20-55 min_savings=0.15 force=false");
        let other = [
            Options { scenes: encode::Scenes::Auto, ..options() },
            Options { scenes: encode::Scenes::Always, ..options() },
            Options { audio: encode::Audio::Shrink, ..options() },
            Options { samples: Some(12), ..options() },
            Options { sample_secs: Some(8.0), ..options() },
            Options { remove: vec![(10.0, 20.0)], ..options() },
            Options { svt_params: "tune=0:enable-qm=0".into(), ..options() },
            Options { preset: 4, ..options() },
        ];
        let mut keys: Vec<String> = other.iter().map(settings_key).chain([settings_key(&options())]).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), other.len() + 1);
        // What does not change whether a file is worth it leaves the key alone.
        assert_eq!(settings_key(&Options { overwrite: true, keep_temp: true, workers: 4, fresh_search: true, ..options() }), settings_key(&options()));
    }

    #[test]
    fn the_output_folder_is_left_out_of_a_walk_however_it_is_named() {
        let dir = std::env::temp_dir().join(format!("av1opt-walktest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("in/out/deep")).unwrap();
        std::fs::create_dir_all(dir.join("in/.av1opt-tmp")).unwrap();
        for f in ["in/a.mkv", "in/out/a.mp4", "in/out/deep/b.mp4", "in/.av1opt-tmp/c.mkv"] {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        std::os::unix::fs::symlink(dir.join("in/out"), dir.join("link")).unwrap();
        let names = |inputs: &[PathBuf], out: &Path| -> Vec<String> {
            expand(inputs, out, |_| true).iter().map(|j| j.input.strip_prefix(&dir).unwrap().to_string_lossy().into_owned()).collect()
        };
        // Named plainly, through a link and with `..` in it: one folder.
        assert_eq!(names(&[dir.join("in")], &dir.join("in/out")), ["in/a.mkv"]);
        assert_eq!(names(&[dir.join("in")], &dir.join("link")), ["in/a.mkv"]);
        assert_eq!(names(&[dir.join("in")], &dir.join("in/out/deep/..")), ["in/a.mkv"]);
        // A folder inside the output folder is walked all the same, and so is the output
        // folder when it is the input.
        assert_eq!(names(&[dir.join("in/out/deep")], &dir.join("in/out")), ["in/out/deep/b.mp4"]);
        assert_eq!(names(&[dir.join("in/out")], &dir.join("in/out")), ["in/out/a.mp4", "in/out/deep/b.mp4"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn jobs_are_ordered_by_worth_and_ties_keep_their_place() {
        let job = |name: &str| Job { input: PathBuf::from(name), out_dir: PathBuf::from("/out") };
        let mut jobs: Vec<Job> = ["a", "b", "c", "d", "e"].map(job).into();
        order_by(&mut jobs, |p| match p.to_str().unwrap() {
            "b" => 5.0,
            "d" => 9.0,
            "e" => f64::NAN,
            _ => 0.0,
        });
        let names: Vec<&str> = jobs.iter().map(|j| j.input.to_str().unwrap()).collect();
        assert_eq!(names, ["d", "b", "a", "c", "e"]);
    }

    #[test]
    fn several_folders_each_keep_a_folder_of_their_own() {
        let dir = std::env::temp_dir().join(format!("av1opt-foldertest-{}", std::process::id()));
        for d in ["in/Kay/old", "in/Sofia", "out"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        for f in ["in/Kay/a.mp4", "in/Kay/old/b.mp4", "in/Sofia/a.mp4", "in/loose.mp4"] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        let out = dir.join("out");
        let outs = |inputs: &[&str]| -> Vec<PathBuf> {
            let inputs: Vec<PathBuf> = inputs.iter().map(|i| dir.join(i)).collect();
            expand(&inputs, &out, |_| true).into_iter().map(|j| j.out_dir.strip_prefix(&out).unwrap().to_path_buf()).collect()
        };
        let p = |names: &[&str]| -> Vec<PathBuf> { names.iter().map(PathBuf::from).collect() };
        // One folder: as it always was, its contents directly below the output folder.
        assert_eq!(outs(&["in/Kay"]), p(&["", "old"]));
        assert_eq!(outs(&["in/Kay", "in/loose.mp4"]), p(&["", "old", ""]));
        // Two: the same file name in both no longer lands on one output.
        assert_eq!(outs(&["in/Kay", "in/Sofia/", "in/loose.mp4"]), p(&["Kay", "Kay/old", "Sofia", ""]));
        std::fs::remove_dir_all(dir).unwrap();
    }

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
