//! Batch encoding: expands folders, mirrors their layout under the output folder, logs every
//! result to `av1opt-log.tsv`, and resumes by skipping finished or previously skipped files.

use crate::encode::{self, Options, Skip};
use crate::events::{self, Event};
use crate::source::Verdict;
use crate::tools::Tools;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const LOG_NAME: &str = "av1opt-log.tsv";

/// A job's output folder is not there, and the job may not make it.
#[derive(Debug)]
pub struct OutDirMissing(pub PathBuf);

impl std::fmt::Display for OutDirMissing {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "the output folder {} is not there; its drive may not be mounted", self.0.display())
    }
}

impl std::error::Error for OutDirMissing {}

/// A job's output folder is there, but not on the drive it was on: what stands at that path
/// now is most likely the empty mount point of a drive that is away.
#[derive(Debug)]
pub struct OutDirElsewhere(pub PathBuf);

impl std::fmt::Display for OutDirElsewhere {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "the output folder {} is now on another drive than when it was queued; its drive may be unmounted, or mounted elsewhere", self.0.display())
    }
}

impl std::error::Error for OutDirElsewhere {}

/// Where a job's output folder was when it was made or first seen. Saved with the job.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutDirPlace {
    /// The device id of the file system holding it.
    pub device: u64,
    /// The top folder of that file system.
    pub mount: PathBuf,
}

impl OutDirPlace {
    /// None for a folder that cannot be looked at.
    pub fn of(dir: &Path) -> Option<OutDirPlace> {
        let dir = std::fs::canonicalize(dir).ok()?;
        let device = std::fs::metadata(&dir).ok()?.dev();
        let mut mount = dir.as_path();
        while let Some(parent) = mount.parent().filter(|p| std::fs::metadata(p).is_ok_and(|m| m.dev() == device)) {
            mount = parent;
        }
        Some(OutDirPlace { device, mount: mount.to_path_buf() })
    }

    /// Whether a folder found at `now` is still on this drive. The device id alone would not
    /// do: many file systems get a new one at each mount, and a drive back after a restart
    /// would count as another. So a file system at or below the same top folder counts too.
    fn holds(&self, now: &OutDirPlace) -> bool {
        now.device == self.device || now.mount.starts_with(&self.mount)
    }
}

/// What stops a job from writing to its output folder now, if anything.
pub fn out_dir_problem(out_dir: &Path, place: Option<&OutDirPlace>) -> Option<anyhow::Error> {
    if !out_dir.is_dir() {
        return Some(OutDirMissing(out_dir.to_path_buf()).into());
    }
    let now = OutDirPlace::of(out_dir)?;
    place.filter(|then| !then.holds(&now)).map(|_| OutDirElsewhere(out_dir.to_path_buf()).into())
}

/// Whether `error` says the output folder is away, which ends once the drive is back.
pub fn is_out_dir_problem(error: &anyhow::Error) -> bool {
    error.chain().any(|e| e.is::<OutDirMissing>() || e.is::<OutDirElsewhere>())
}

/// Makes the output folder of a job or watch saved by a version that did not make it when it
/// was queued. Nothing says yet whether its absence means a drive that is away, so only the
/// last part of the path is made: with its parent missing the drive is taken to be away, and
/// the same with a parent that is empty and on the same drive as the folder above it, which is
/// what the mount point of an unmounted drive looks like.
#[cfg(any(feature = "ui", test))]
pub fn make_out_dir_once(out_dir: &Path) -> Result<()> {
    use anyhow::Context;
    if out_dir.is_dir() {
        return Ok(());
    }
    let away = || anyhow::Error::from(OutDirMissing(out_dir.to_path_buf()));
    let parent = out_dir.parent().ok_or_else(away)?;
    if could_be_empty_mount_point(parent) {
        return Err(away());
    }
    match std::fs::create_dir(out_dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(away()),
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => Err(e).with_context(|| format!("cannot create {}", out_dir.display())),
        _ => Ok(()),
    }
}

#[cfg(any(feature = "ui", test))]
fn could_be_empty_mount_point(dir: &Path) -> bool {
    let device = |p: &Path| std::fs::metadata(p).ok().map(|m| m.dev());
    let empty = std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_none());
    empty && dir.parent().is_some_and(|above| device(above).is_some() && device(above) == device(dir))
}

/// One input file and the folder its output goes to.
pub struct Job {
    pub input: PathBuf,
    pub out_dir: PathBuf,
    /// Where a run that was given this file's folder with more or fewer other folders put its
    /// output (see [`folder_out_among`]), if that is another place.
    pub also_at: Option<PathBuf>,
}

/// Where the files of the folder `input` go, given with `folders` folders in all, counting
/// itself. Given alone, a folder's contents sit directly below `out_dir`. Given with other
/// folders, each gets a folder of its own name there: two libraries encoded in one run stay
/// apart, as they are where they came from. A job kept for later goes by how many there were
/// when it was queued, whichever are there now.
pub fn folder_out_among(folders: usize, input: &Path, out_dir: &Path) -> PathBuf {
    let abs = std::path::absolute(input).unwrap_or_else(|_| input.to_path_buf());
    match abs.file_name() {
        Some(name) if folders > 1 => out_dir.join(name),
        _ => out_dir.to_path_buf(),
    }
}

/// Files are used as given; folders are walked and their structure is kept below `out_dir`
/// (below a folder of their own name there when several are given, see [`folder_out_among`]). A file
/// reached twice is one job, and the run says so once.
pub fn expand(inputs: &[PathBuf], out_dir: &Path, is_video: impl Fn(&Path) -> bool) -> Vec<Job> {
    let (jobs, repeated) = expand_once(inputs, out_dir, is_video);
    if let Some(first) = repeated.first() {
        let more = if repeated.len() > 1 { format!(" and {} more", repeated.len() - 1) } else { String::new() };
        eprintln!("note: left out, as the same file was already reached by way of an input before it: {}{more}", first.display());
    }
    jobs
}

/// The jobs, and the paths left out for leading to a file that already has one. Files are told
/// apart by device and inode, as in a scan: one folder given inside another, or a hard link,
/// would otherwise encode one video twice. The first input to reach a file decides its place.
fn expand_once(inputs: &[PathBuf], out_dir: &Path, is_video: impl Fn(&Path) -> bool) -> (Vec<Job>, Vec<PathBuf>) {
    // Folders as the system resolves them: an output folder named through a link or `..`
    // is the same folder, and two strings would not say so.
    let real = |p: &Path| std::fs::canonicalize(p).or_else(|_| std::path::absolute(p)).unwrap_or_else(|_| p.to_path_buf());
    let out_abs = real(out_dir);
    let folders = inputs.iter().filter(|p| p.is_dir()).count();
    let mut seen = HashSet::new();
    let (mut jobs, mut repeated) = (Vec::new(), Vec::new());
    for input in inputs {
        if input.is_dir() {
            let below = folder_out_among(folders, input, out_dir);
            let otherwise = folder_out_among(if folders > 1 { 1 } else { 2 }, input, out_dir);
            // A folder that lies in the output folder (or is it) is still walked: only an
            // output folder below the input is left out.
            let inside = real(input).starts_with(&out_abs);
            let mut files: Vec<(PathBuf, Option<(u64, u64)>)> = walkdir::WalkDir::new(input)
                .into_iter()
                .filter_entry(|e| {
                    // Never descend into our own output or temp folders.
                    let ours = !inside && e.file_type().is_dir() && real(e.path()).starts_with(&out_abs);
                    !ours && !crate::scan::not_for_videos(e) && !e.file_name().to_string_lossy().starts_with(".av1opt-")
                })
                .flatten()
                .filter(|e| e.file_type().is_file() && is_video(e.path()))
                .map(|e| {
                    let inode = e.metadata().ok().map(|m| (m.dev(), m.ino()));
                    (e.into_path(), inode)
                })
                .collect();
            files.sort();
            for (f, inode) in files {
                if inode.is_some_and(|i| !seen.insert(i)) {
                    repeated.push(f);
                    continue;
                }
                let rel_dir = f.parent().and_then(|p| p.strip_prefix(input).ok()).unwrap_or(Path::new("")).to_path_buf();
                jobs.push(Job { out_dir: below.join(&rel_dir), also_at: (otherwise != below).then(|| otherwise.join(&rel_dir)), input: f });
            }
        } else {
            // One that cannot be read is kept: its job says what is wrong with it.
            if std::fs::metadata(input).is_ok_and(|m| !seen.insert((m.dev(), m.ino()))) {
                repeated.push(input.clone());
                continue;
            }
            jobs.push(Job { input: input.clone(), out_dir: out_dir.to_path_buf(), also_at: None });
        }
    }
    (jobs, repeated)
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
/// Lines from older versions carry no settings and count for any settings. Read as bytes and
/// line by line: one damaged line costs that line, not what the rest of the log remembers.
fn read_skips(log: &Path, settings: &str) -> HashMap<(PathBuf, u64), String> {
    let Ok(bytes) = std::fs::read(log) else { return HashMap::new() };
    bytes
        .split(|b| *b == b'\n')
        .filter_map(|l| {
            let l = std::str::from_utf8(l).ok()?;
            let f: Vec<&str> = l.trim_end_matches('\r').split('\t').collect();
            if f.len() < 5 || f[1] != "skip" || f.get(5).is_some_and(|s| *s != settings) {
                return None;
            }
            Some(((PathBuf::from(unescape_name(f[2])), f[3].parse().ok()?), f[4].to_string()))
        })
        .collect()
}

/// A path as a field of the log, which is split at tabs and line ends. Written out rather than
/// replaced by spaces: `a<tab>b.mkv` would otherwise read as `a b.mkv`, and a skip made for
/// the one would hold for the other.
fn escape_name(name: &str) -> String {
    name.replace('\\', "\\\\").replace('\t', "\\t").replace('\n', "\\n").replace('\r', "\\r")
}

/// The name [`escape_name`] wrote. A backslash before anything else is one of its own: the
/// logs of earlier versions did not escape it.
fn unescape_name(field: &str) -> String {
    let mut name = String::with_capacity(field.len());
    let mut chars = field.chars().peekable();
    while let Some(c) = chars.next() {
        let meant = match (c, chars.peek()) {
            ('\\', Some('\\')) => Some('\\'),
            ('\\', Some('t')) => Some('\t'),
            ('\\', Some('n')) => Some('\n'),
            ('\\', Some('r')) => Some('\r'),
            _ => None,
        };
        if meant.is_some() {
            chars.next();
        }
        name.push(meant.unwrap_or(c));
    }
    name
}

fn log_line(log: &Path, status: &str, input: &Path, size: u64, detail: &str, settings: &str) -> Result<()> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let clean = detail.replace(['\t', '\n'], " ");
    let line = format!("{now}\t{status}\t{}\t{size}\t{clean}\t{settings}", escape_name(&input.display().to_string()));
    Ok(crate::files::append_line(log, &line, false)?)
}

/// What an output's report says it was made from.
struct Made {
    /// The input, as an absolute path.
    source: PathBuf,
    /// The sections left out of the output.
    removed: Vec<(f64, f64)>,
    /// The input as it was then.
    then: crate::source::Recorded,
}

fn report_input(output: &Path) -> Option<Made> {
    let r: serde_json::Value = serde_json::from_slice(&std::fs::read(output.with_extension("av1opt.json")).ok()?).ok()?;
    let p = PathBuf::from(r["input"].as_str()?);
    let removed = serde_json::from_value(r["removed"].clone()).unwrap_or_default();
    Some(Made {
        source: std::fs::canonicalize(&p).ok().or_else(|| std::path::absolute(&p).ok())?,
        removed,
        then: crate::source::Recorded::in_report(&r),
    })
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

/// Where a job's output goes.
fn output_of(job: &Job, o: &Options) -> PathBuf {
    let stem = job.input.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    job.out_dir.join(format!("{stem}.{}", o.container.ext()))
}

/// A file as the system resolves it, whatever way it was named. A file that is not there yet
/// is its real folder and its name.
struct Identity {
    path: PathBuf,
    inode: Option<(u64, u64)>,
}

impl Identity {
    fn of(path: &Path) -> Identity {
        let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        let in_real_folder = || Some(std::fs::canonicalize(absolute.parent()?).ok()?.join(absolute.file_name()?));
        let path = std::fs::canonicalize(path).ok().or_else(in_real_folder).unwrap_or_else(|| absolute.clone());
        let inode = std::fs::metadata(&path).ok().map(|m| (m.dev(), m.ino()));
        Identity { path, inode }
    }

    fn is(&self, other: &Identity) -> bool {
        self.path == other.path || (self.inode.is_some() && self.inode == other.inode)
    }
}

/// An input of this batch standing where a job's output would go.
#[derive(Debug, PartialEq)]
enum Clash {
    /// The job's own input.
    Own,
    /// The input of another job.
    Input(PathBuf),
    /// No input, but the output of an earlier job too (`a.mkv` and `a.avi` in one folder):
    /// named here is that job's input.
    Twin(PathBuf),
}

/// For each job, the input its output would land on. Worked out before the first encode: a
/// folder that is walked into itself holds both `a.mkv` and `a.mp4`, and the output of the
/// first is the second.
fn clashes(jobs: &[Job], o: &Options) -> Vec<Option<Clash>> {
    let inputs: Vec<Identity> = jobs.iter().map(|j| Identity::of(&j.input)).collect();
    // A job refused for its name writes nothing, and what its name becomes as text may be the
    // name of a real file: it stands in nobody's way.
    let refused = |i: usize| jobs[i].input.to_str().is_none() || inputs[i].path.to_str().is_none();
    let mut by_path = HashMap::new();
    let mut by_inode = HashMap::new();
    for (i, input) in inputs.iter().enumerate().filter(|(i, _)| !refused(*i)) {
        by_path.entry(input.path.as_path()).or_insert(i);
        if let Some(inode) = input.inode {
            by_inode.entry(inode).or_insert(i);
        }
    }
    let mut writers: HashMap<PathBuf, usize> = HashMap::new();
    jobs.iter()
        .zip(&inputs)
        .enumerate()
        .map(|(i, (job, input))| {
            if refused(i) {
                return None;
            }
            let output = Identity::of(&output_of(job, o));
            if output.is(input) {
                return Some(Clash::Own);
            }
            if let Some(other) = by_path.get(output.path.as_path()).or_else(|| by_inode.get(&output.inode?)) {
                return Some(Clash::Input(jobs[*other].input.clone()));
            }
            let first = *writers.entry(output.path).or_insert(i);
            (first != i).then(|| Clash::Twin(jobs[first].input.clone()))
        })
        .collect()
}

/// Why `--overwrite` may not replace the file at `output` with an encode of `input`, an
/// absolute path. It is for encoding a file again: only the report beside an output says what
/// that output is, and without one the file may be anybody's. (av1opt leaves no file of its own
/// under an output's name without a report, except when a run dies in the moment between the
/// two, and nothing tells that one from a stranger.)
pub(crate) fn overwrite_refused(output: &Path, input: &Path) -> Option<String> {
    let shown = output.display();
    if !output.exists() {
        return None;
    }
    match report_input(output) {
        Some(made) if made.source == input => None,
        Some(made) => Some(format!("--overwrite replaces only an encode of the same file, and {shown} was made from {}; move or delete it to encode this file there", made.source.display())),
        None => Some(format!("--overwrite replaces only an encode of the same file, and {shown} has no report saying what it was made from; move or delete it if it may go")),
    }
}

/// The finished encode of this input that a run given its folder differently left at the job's
/// other place: the same name, a report naming this file as it is now, and the same sections.
fn done_elsewhere(job: &Job, input: &Path, o: &Options) -> Option<PathBuf> {
    if o.overwrite {
        return None;
    }
    let place = job.also_at.as_ref()?.join(output_of(job, o).file_name()?);
    let made = report_input(&place).filter(|_| place.is_file())?;
    let same = made.source == input && same_sections(&o.remove, &made.removed) && matches!(crate::source::compare(&made.then, input), Verdict::Same);
    same.then_some(place)
}

/// What to do with a job before any work on it.
#[derive(Debug, PartialEq)]
enum Before {
    Encode,
    /// The output there is this input's encode.
    Done,
    /// Nothing to do and nothing wrong: the input is itself the result of an earlier run.
    Leave(String),
    Refuse(String),
}

fn before_encoding(output: &Path, clash: Option<&Clash>, input: &Path, o: &Options) -> Before {
    let exists = output.exists();
    let made = if exists { report_input(output) } else { None };
    match clash {
        Some(Clash::Own) => {
            return match made {
                Some(m) if m.source != input => Before::Leave(format!("{} is the encode of {} from an earlier run", output.display(), m.source.display())),
                _ => Before::Refuse("the output would be written over this same video, so nothing was written; choose another output folder".into()),
            };
        }
        // Only an encode of this very input may stand there, and it is then not replaced.
        Some(Clash::Input(other)) if o.overwrite || made.as_ref().is_none_or(|m| m.source != input) => {
            return Before::Refuse(format!("its output would take the place of another input, {}", other.display()));
        }
        // Without --overwrite the first one's report settles it below: the second is a name
        // collision once the first is made. With it, the second would replace the first.
        Some(Clash::Twin(other)) if o.overwrite => {
            return Before::Refuse(format!("its output is also that of another input, {}; give one of them another output folder", other.display()));
        }
        _ => {}
    }
    if !exists || o.overwrite {
        if let Some(why) = encode::report_of_another_output(output) {
            return Before::Refuse(why);
        }
        return overwrite_refused(output, input).map_or(Before::Encode, Before::Refuse);
    }
    // Only a report says what an output was made from: the same file name can come from two
    // input folders, or be a file that has nothing to do with this run.
    let Some(made) = made else {
        // No report beside it: reports get tidied away, and a run that died between the file
        // and its report left one such. It is nobody's input (see above), so it is left alone
        // and counted as done, as it always was.
        return Before::Done;
    };
    if !same_sections(&o.remove, &made.removed) {
        return Before::Refuse(format!("{} exists but has other sections removed; move or delete it to encode again", output.display()));
    }
    if made.source != input {
        return Before::Refuse(format!("output name collision: {} was made from {}", output.display(), made.source.display()));
    }
    // The same name and another file: a download done again, an edit saved over it, or a
    // download that wrote into a file allocated ahead.
    let shown = output.display();
    let why = made.then.why_it_cannot_tell();
    match crate::source::compare(&made.then, input) {
        Verdict::Same => Before::Done,
        Verdict::OtherSize { then, now } => Before::Refuse(format!("{shown} was made from this file when it was another size ({then} bytes, now {now}); move or delete it to encode again")),
        Verdict::Changed => Before::Refuse(format!("{shown} was made from this file before it changed: it is the same size but holds something else now; move or delete {shown} to encode it again")),
        Verdict::CannotTell => Before::Refuse(format!(
            "{shown} was made from this file when it was last changed at another time, and {why}, so av1opt cannot tell whether only the date differs; this file counts as failed until then: if the file did change, or to be sure, delete {shown} to encode it again"
        )),
        Verdict::Unreadable(e) => Before::Refuse(format!("cannot tell whether {shown} is the encode of this file as it is now, because the file cannot be read: {e}")),
    }
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
    /// Files failed because the output folder was away. Whatever it is doing by the end of the
    /// run, those files are still to be done.
    pub out_dir_lost: Option<anyhow::Error>,
    /// What a stopped run left on purpose in the folder it was working in.
    pub kept: Vec<PathBuf>,
}

/// The folders a stopped run leaves in `out_dir`: per-scene work, which the next run resumes from,
/// and its temporary folder if it was told to keep that.
fn kept_when_stopped(out_dir: &Path, o: &Options) -> Vec<PathBuf> {
    // This run's own: the name goes on with when the process began and random text.
    let temp = format!(".av1opt-tmp-{}-", std::process::id());
    let mut kept: Vec<PathBuf> = std::fs::read_dir(out_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with(encode::WORK_PREFIX) || (o.keep_temp && name.starts_with(&temp))
        })
        .map(|e| e.path())
        .collect();
    kept.sort();
    kept
}

pub fn run(tools: &Tools, jobs: &[Job], out_dir: &Path, o: &Options, retry_skipped: bool) -> Result<Summary> {
    run_placed(tools, jobs, out_dir, None, o, retry_skipped)
}

/// [`run`] for an output folder known to have been at `place`.
pub fn run_placed(tools: &Tools, jobs: &[Job], out_dir: &Path, place: Option<&OutDirPlace>, o: &Options, retry_skipped: bool) -> Result<Summary> {
    let folder_problem = || if o.out_dir_must_exist { out_dir_problem(out_dir, place) } else { None };
    let may_write = || folder_problem().map_or(Ok(()), Err);
    let log = out_dir.join(LOG_NAME);
    let settings = settings_key(o);
    let skips = if retry_skipped { HashMap::new() } else { read_skips(&log, &settings) };
    let clashes = clashes(jobs, o);
    // The log serves the next run; a log that cannot be written is no reason to stop this one.
    let log_warned = Cell::new(false);
    let record = |status: &str, input: &Path, size: u64, detail: &str| {
        if let Err(e) = log_line(&log, status, input, size, detail, &settings)
            && !log_warned.replace(true)
        {
            eprintln!("warning: cannot write to {}: {e}; going on without it", log.display());
        }
    };
    let mut sum = Summary::default();
    for (i, job) in jobs.iter().enumerate() {
        let abs = std::fs::canonicalize(&job.input).or_else(|_| std::path::absolute(&job.input))?;
        let size = std::fs::metadata(&job.input).map_or(0, |m| m.len());
        let output = output_of(job, o);
        eprintln!("\n[{}/{}] {}", i + 1, jobs.len(), job.input.display());
        if tools.check_cancel().is_err() {
            sum.cancelled = true;
            break;
        }
        tools.emit(Event::FileStart { index: i, total: jobs.len(), input: abs.clone(), output: output.clone() });
        let end = |status: &str, detail: &str, out_bytes: u64| {
            tools.emit(Event::FileEnd { index: i, status: status.into(), detail: detail.into(), input_bytes: size, output_bytes: out_bytes });
        };

        // A drive lost during the job: encoding on would fill the disk under its mount point.
        // Not logged, since the log lives in the folder that is gone.
        if let Some(problem) = folder_problem() {
            let msg = problem.to_string();
            eprintln!("FAILED: {msg}");
            end("error", &msg, 0);
            sum.failed += 1;
            sum.out_dir_lost.get_or_insert(problem);
            continue;
        }

        // Checked before anything else: what is known about such a file goes by a name that
        // may be another file's.
        let named = [job.input.as_path(), &abs, &output].iter().all(|p| p.to_str().is_some());
        let before = if named { before_encoding(&output, clashes[i].as_ref(), &abs, o) } else { Before::Refuse(crate::tools::NOT_TEXT.into()) };
        match before {
            Before::Encode => {}
            Before::Done => {
                eprintln!("already done: {}", output.display());
                end("done_before", &output.display().to_string(), std::fs::metadata(&output).map_or(0, |m| m.len()));
                sum.already_done += 1;
                continue;
            }
            Before::Leave(why) => {
                eprintln!("skipped: {why}");
                end("skip", &why, 0);
                sum.skipped += 1;
                continue;
            }
            Before::Refuse(msg) => {
                eprintln!("FAILED: {msg}");
                end("error", &msg, 0);
                sum.failed += 1;
                record("error", &abs, size, &msg);
                continue;
            }
        }
        if let Some(place) = done_elsewhere(job, &abs, o) {
            eprintln!("already done: {} (where a run given this folder with more or fewer others put it)", place.display());
            end("done_before", &place.display().to_string(), std::fs::metadata(&place).map_or(0, |m| m.len()));
            sum.already_done += 1;
            continue;
        }
        if let Some(reason) = skips.get(&(abs.clone(), size)) {
            eprintln!("skipped earlier: {reason} (use --retry-skipped to try again)");
            end("skip", &format!("skipped earlier: {reason}"), 0);
            sum.skipped += 1;
            continue;
        }

        encode::clean_stale_temp(&job.out_dir);
        match guarded(|| encode::optimize(tools, &job.input, &job.out_dir, &may_write, o)) {
            Ok(out) => {
                let out_size = std::fs::metadata(&out).map_or(0, |m| m.len());
                sum.ok += 1;
                sum.bytes_in += size;
                sum.bytes_out += out_size;
                end("ok", &out.display().to_string(), out_size);
                record("ok", &abs, size, &out.display().to_string());
            }
            // Whatever else went wrong once the run was stopped comes of the stopping: Ctrl-C
            // reaches an ffprobe under way directly, and it fails like a damaged file.
            Err(e) if events::is_cancelled(&e) || tools.check_cancel().is_err() => {
                // Not logged: the next run picks the file up again (scene work resumes).
                eprintln!("cancelled");
                end("cancelled", "", 0);
                sum.cancelled = true;
                sum.kept = kept_when_stopped(&job.out_dir, o);
                break;
            }
            Err(e) if e.downcast_ref::<encode::InUse>().is_some() => {
                // Not logged: the file is still to be looked at by a later run.
                eprintln!("skipped: {e}");
                end("skip", &e.to_string(), 0);
                sum.skipped += 1;
            }
            Err(e) if e.downcast_ref::<Skip>().is_some() => {
                eprintln!("skipped: {e}");
                end("skip", &e.to_string(), 0);
                sum.skipped += 1;
                record("skip", &abs, size, &e.to_string());
            }
            Err(e) => {
                let detail = format!("{e:#}");
                eprintln!("FAILED: {detail}");
                end("error", &detail, 0);
                sum.failed += 1;
                // A file whose drive went away during its encode failed for that, whatever
                // the error was, and it may be back before the next file looks.
                match if is_out_dir_problem(&e) { Some(e) } else { folder_problem() } {
                    Some(problem) => {
                        sum.out_dir_lost.get_or_insert(problem);
                    }
                    None => record("error", &abs, size, &detail),
                }
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
            out_dir_must_exist: false,
        }
    }

    /// A folder for one test, and what a batch made of `names` in it comes to. The tools are
    /// `false`, so that a job let through to the encode fails at the first probe.
    struct Folder(PathBuf);

    impl Folder {
        fn new(name: &str, files: &[&str]) -> Folder {
            let dir = std::env::temp_dir().join(format!("av1opt-batch-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            for f in files {
                std::fs::write(dir.join(f), b"video").unwrap();
            }
            Folder(dir)
        }

        /// A report beside `output` saying it was made from `input`.
        fn report(&self, output: &str, input: &str, bytes: u64) {
            let at = self.0.join(output).with_extension("av1opt.json");
            std::fs::write(at, serde_json::json!({"input": self.0.join(input), "source_bytes": bytes}).to_string()).unwrap();
        }

        /// The folder walked into `out`: per input name, the status and detail it ended with.
        fn walk(&self, out: &Path, o: &Options) -> (Summary, HashMap<String, (String, String)>) {
            run_all(&expand(std::slice::from_ref(&self.0), out, a_video), out, o)
        }
    }

    fn a_video(p: &Path) -> bool {
        p.extension().is_some_and(|e| e != "json" && e != "tsv")
    }

    /// What a batch of `jobs` comes to: per input name, the status and detail it ended with.
    fn run_all(jobs: &[Job], out: &Path, o: &Options) -> (Summary, HashMap<String, (String, String)>) {
        {
            let names: Vec<String> = jobs.iter().map(|j| j.input.file_name().unwrap().to_string_lossy().into_owned()).collect();
            let seen = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
            let mut tools = Tools::new("false".into(), "false".into(), None);
            let sink = seen.clone();
            tools.hooks = Some(std::sync::Arc::new(events::Hooks::new(move |e| {
                if let Event::FileEnd { index, status, detail, .. } = e {
                    sink.lock().unwrap().insert(names[index].clone(), (status, detail));
                }
            })));
            let sum = run(&tools, jobs, out, o, false).unwrap();
            let ends = seen.lock().unwrap().clone();
            (sum, ends)
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// An encode that went through to the tools, which here cannot probe anything.
    fn reached_the_encode(end: &(String, String)) -> bool {
        end.0 == "error" && !end.1.contains("output") && !end.1.contains("exists")
    }

    #[test]
    fn an_output_is_never_written_over_another_input() {
        let f = Folder::new("overwrite", &["a.mkv", "a.mp4", "b.mkv"]);
        let o = Options { overwrite: true, ..options() };
        let (sum, ends) = f.walk(&f.0, &o);
        let mkv = &ends["a.mkv"];
        assert_eq!(mkv.0, "error");
        assert_eq!(mkv.1, format!("its output would take the place of another input, {}", f.0.join("a.mp4").display()));
        assert!(ends["a.mp4"].1.contains("the output would be written over this same video"), "{:?}", ends["a.mp4"]);
        // The rest of the batch goes on, and nothing was touched on the way.
        assert!(reached_the_encode(&ends["b.mkv"]), "{:?}", ends["b.mkv"]);
        assert_eq!(sum.failed, 3);
        assert_eq!(std::fs::read(f.0.join("a.mp4")).unwrap(), b"video");
    }

    #[test]
    fn an_input_counts_however_the_output_folder_is_named() {
        let f = Folder::new("named", &["a.mkv", "a.mp4"]);
        let link = std::env::temp_dir().join(format!("av1opt-batch-link-{}", std::process::id()));
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&f.0, &link).unwrap();
        for out in [link.clone(), f.0.join("../").join(f.0.file_name().unwrap())] {
            let (_, ends) = f.walk(&out, &Options { overwrite: true, ..options() });
            assert!(ends["a.mkv"].1.starts_with("its output would take the place of another input"), "{:?}", ends["a.mkv"]);
        }
        std::fs::remove_file(&link).unwrap();

        // Another name for the same file: the output is that input, a hard link away.
        let g = Folder::new("hardlink", &["c.mkv", "d.mkv"]);
        std::fs::hard_link(g.0.join("c.mkv"), g.0.join("d.mp4")).unwrap();
        let jobs: Vec<Job> = ["c.mkv", "d.mkv"].map(|n| Job { input: g.0.join(n), out_dir: g.0.clone(), also_at: None }).into();
        assert_eq!(clashes(&jobs, &options()), [None, Some(Clash::Input(g.0.join("c.mkv")))]);

        // Two inputs with one output name: with --overwrite the second would replace what
        // the first has just made.
        let t = Folder::new("twin", &["e.mkv", "e.avi"]);
        let (sum, ends) = t.walk(&Folder::new("twin-out", &[]).0, &Options { overwrite: true, ..options() });
        assert_eq!(sum.failed, 2);
        assert!(reached_the_encode(&ends["e.avi"]), "{:?}", ends["e.avi"]);
        assert_eq!(ends["e.mkv"].1, format!("its output is also that of another input, {}; give one of them another output folder", t.0.join("e.avi").display()));
        // And when that input is the job's own, it is its own.
        std::fs::rename(g.0.join("d.mp4"), g.0.join("c.mp4")).unwrap();
        let own = [Job { input: g.0.join("c.mkv"), out_dir: g.0.clone(), also_at: None }];
        assert_eq!(clashes(&own, &options()), [Some(Clash::Own)]);
    }

    #[test]
    fn an_output_without_a_report_is_not_done() {
        let f = Folder::new("noreport", &["a.mkv", "a.mp4"]);
        let (sum, ends) = f.walk(&f.0, &options());
        assert_eq!((sum.already_done, sum.failed), (0, 2));
        assert!(ends["a.mkv"].1.starts_with("its output would take the place of another input"), "{:?}", ends["a.mkv"]);
        assert!(ends["a.mp4"].1.contains("the output would be written over this same video"), "{:?}", ends["a.mp4"]);

        // A file of that name in another folder is nobody's input: whoever tidied its report
        // away does not get it encoded again, or an error for every file of a run.
        let source = Folder::new("noreport-in", &["c.mkv"]);
        let out = Folder::new("noreport-out", &["c.mp4"]);
        let (sum, ends) = source.walk(&out.0, &options());
        assert_eq!((sum.already_done, sum.failed), (1, 0));
        assert_eq!(ends["c.mkv"].0, "done_before");
    }

    #[test]
    fn overwrite_replaces_only_an_encode_of_the_same_file() {
        let source = Folder::new("replace-in", &["c.mkv", "d.mkv"]);
        let out = Folder::new("replace-out", &["c.mp4", "d.mp4"]);
        let o = Options { overwrite: true, ..options() };
        let report = |stem: &str, input: &Path| std::fs::write(out.0.join(format!("{stem}.av1opt.json")), serde_json::json!({"input": input, "output": out.0.join(format!("{stem}.mp4")), "source_bytes": 5}).to_string()).unwrap();
        // Its own encode goes through to the encoder.
        report("c", &source.0.join("c.mkv"));
        report("d", Path::new("/elsewhere/d.mkv"));
        let (sum, ends) = source.walk(&out.0, &o);
        assert!(reached_the_encode(&ends["c.mkv"]), "{:?}", ends["c.mkv"]);
        // The encode of another file is not this run's to replace.
        let (status, detail) = &ends["d.mkv"];
        assert_eq!(status, "error");
        assert!(detail.contains("was made from /elsewhere/d.mkv") && detail.contains("d.mp4"), "{detail}");
        assert_eq!(sum.failed, 2);
        // Nor is a file nothing is known about.
        std::fs::remove_file(out.0.join("d.av1opt.json")).unwrap();
        let (_, ends) = source.walk(&out.0, &o);
        assert!(ends["d.mkv"].1.contains("has no report"), "{:?}", ends["d.mkv"]);
        std::fs::write(out.0.join("d.av1opt.json"), b"not json").unwrap();
        let (_, ends) = source.walk(&out.0, &o);
        assert!(ends["d.mkv"].1.contains("has no report"), "{:?}", ends["d.mkv"]);
        assert_eq!(std::fs::read(out.0.join("d.mp4")).unwrap(), b"video");
    }

    #[test]
    fn a_report_belonging_to_another_container_is_left_to_it() {
        let source = Folder::new("container-in", &["x.mkv"]);
        let out = Folder::new("container-out", &["x.mp4"]);
        let report = out.0.join("x.av1opt.json");
        let fields = serde_json::json!({"input": source.0.join("x.mkv"), "output": out.0.join("x.mp4"), "source_bytes": 5});
        std::fs::write(&report, fields.to_string()).unwrap();
        let mkv = || Options { container: encode::Container::Mkv, ..options() };
        for o in [Options { overwrite: true, ..mkv() }, mkv()] {
            let (sum, ends) = source.walk(&out.0, &o);
            assert_eq!(sum.failed, 1);
            let detail = &ends["x.mkv"].1;
            assert!(detail.contains("x.av1opt.json") && detail.contains("x.mp4") && detail.contains("another output folder"), "{detail}");
        }
        assert_eq!(std::fs::read_to_string(&report).unwrap(), fields.to_string());
        // Under its own name the output goes on as ever.
        assert_eq!(source.walk(&out.0, &options()).1["x.mkv"].0, "done_before");
        // A report whose output has gone, or that says where an original went, stands in
        // nobody's way: the first is stale, and the second is set aside when the time comes.
        std::fs::remove_file(out.0.join("x.mp4")).unwrap();
        assert!(reached_the_encode(&source.walk(&out.0, &mkv()).1["x.mkv"]));
        std::fs::write(out.0.join("x.mp4"), b"video").unwrap();
        let replaced = serde_json::json!({"input": source.0.join("x.mkv"), "output": out.0.join("x.mp4"), "source_bytes": 5, "replaced": {"at": 1}});
        std::fs::write(&report, replaced.to_string()).unwrap();
        assert!(reached_the_encode(&source.walk(&out.0, &mkv()).1["x.mkv"]));
    }

    #[test]
    fn an_earlier_encode_in_the_walked_folder_is_recognised() {
        let f = Folder::new("earlier", &["x.mkv", "x.mp4"]);
        f.report("x.mp4", "x.mkv", 5);
        let (sum, ends) = f.walk(&f.0, &options());
        assert_eq!(ends["x.mkv"].0, "done_before");
        assert_eq!(ends["x.mp4"].0, "skip", "{:?}", ends["x.mp4"]);
        assert!(ends["x.mp4"].1.contains("from an earlier run"), "{:?}", ends["x.mp4"]);
        assert_eq!((sum.already_done, sum.skipped, sum.failed), (1, 1, 0));

        // The source has changed since: it is not done, and the encode stays where it is.
        f.report("x.mp4", "x.mkv", 4);
        let (sum, ends) = f.walk(&f.0, &options());
        assert!(ends["x.mkv"].1.contains("when it was another size"), "{:?}", ends["x.mkv"]);
        assert_eq!(sum.failed, 1);

        // A report that names a third file does not make this one done.
        f.report("x.mp4", "elsewhere.mkv", 5);
        let (_, ends) = f.walk(&f.0, &options());
        assert!(ends["x.mkv"].1.starts_with("its output would take the place of another input"), "{:?}", ends["x.mkv"]);

        // Asked to overwrite, the earlier encode is still an input of this batch.
        f.report("x.mp4", "x.mkv", 5);
        let (_, ends) = f.walk(&f.0, &Options { overwrite: true, ..options() });
        assert!(ends["x.mkv"].1.starts_with("its output would take the place of another input"), "{:?}", ends["x.mkv"]);
    }

    #[test]
    fn a_job_that_may_not_make_its_output_folder_stops_writing_when_it_is_gone() {
        let source = Folder::new("away-in", &["a.mkv", "b.mkv"]);
        let out = Folder::new("away-out", &[]);
        std::fs::remove_dir_all(&out.0).unwrap();
        let mut o = options();
        o.out_dir_must_exist = true;
        let (summary, ends) = source.walk(&out.0, &o);
        assert_eq!(summary.failed, 2);
        for (status, detail) in ends.values() {
            assert_eq!(status, "error");
            assert_eq!(*detail, OutDirMissing(out.0.clone()).to_string());
        }
        assert!(!out.0.exists());
    }

    #[test]
    fn an_output_folder_on_another_drive_than_it_was_stops_the_job() {
        let source = Folder::new("place-in", &["c.mkv"]);
        let out = Folder::new("place-out", &[]);
        let here = OutDirPlace::of(&out.0).unwrap();
        assert!(out_dir_problem(&out.0, Some(&here)).is_none());
        assert!(out.0.starts_with(&here.mount), "{here:?}");

        // Its drive has gone and something else stands at the path.
        let away = OutDirPlace { device: here.device + 1, mount: out.0.join("gone") };
        let e = out_dir_problem(&out.0, Some(&away)).unwrap();
        assert!(e.is::<OutDirElsewhere>() && is_out_dir_problem(&e), "{e}");
        assert!(e.to_string().contains("on another drive than when it was queued"), "{e}");
        // Back after a restart with a new device id, at the same place: the same drive.
        assert!(out_dir_problem(&out.0, Some(&OutDirPlace { device: here.device + 1, mount: here.mount.clone() })).is_none());
        assert!(is_out_dir_problem(&out_dir_problem(&out.0.join("nowhere"), None).unwrap()));

        // Every file fails without anything being written, and the run says why.
        let mut o = options();
        o.out_dir_must_exist = true;
        let jobs = vec![Job { input: source.0.join("c.mkv"), out_dir: out.0.clone(), also_at: None }];
        let tools = Tools::new("false".into(), "false".into(), None);
        let sum = run_placed(&tools, &jobs, &out.0, Some(&away), &o, false).unwrap();
        assert_eq!(sum.failed, 1);
        assert!(sum.out_dir_lost.is_some_and(|e| e.is::<OutDirElsewhere>()));
        assert_eq!(std::fs::read_dir(&out.0).unwrap().count(), 0, "nothing is written there, not even the log");
    }

    #[test]
    fn files_failed_for_a_folder_that_was_away_are_remembered_when_it_is_back() {
        let source = Folder::new("back-in", &["a.mkv", "b.mkv"]);
        let out = Folder::new("back-out", &[]);
        let gone = out.0.join("drive");
        let mut o = options();
        o.out_dir_must_exist = true;
        let jobs: Vec<Job> = ["a.mkv", "b.mkv"].iter().map(|n| Job { input: source.0.join(n), out_dir: gone.clone(), also_at: None }).collect();
        // The drive is back as soon as the first file has failed for its absence.
        let mut tools = Tools::new("false".into(), "false".into(), None);
        let back = gone.clone();
        tools.hooks = Some(std::sync::Arc::new(events::Hooks::new(move |e| {
            if matches!(e, Event::FileEnd { .. }) {
                let _ = std::fs::create_dir(&back);
            }
        })));
        let sum = run_placed(&tools, &jobs, &gone, None, &o, false).unwrap();
        assert!(gone.is_dir());
        assert_eq!(sum.failed, 2);
        assert!(sum.out_dir_lost.is_some_and(|e| e.is::<OutDirMissing>()), "the first file is still to be done");
    }

    #[test]
    fn a_folder_nothing_has_made_yet_is_made_only_where_its_drive_can_be_there() {
        let root = Folder::new("once", &[]);
        // Everything saved before folders were made has no mark. With the drive away at the
        // first start after the upgrade, nothing may appear under its mount point.
        let gone = root.0.join("gone/drive/out");
        assert!(make_out_dir_once(&gone).unwrap_err().is::<OutDirMissing>());
        assert!(!root.0.join("gone").exists());
        let mount_point = root.0.join("mount");
        std::fs::create_dir(&mount_point).unwrap();
        assert!(make_out_dir_once(&mount_point.join("out")).unwrap_err().is::<OutDirMissing>());
        assert_eq!(std::fs::read_dir(&mount_point).unwrap().count(), 0);
        // With something in the parent, the drive is there.
        std::fs::write(mount_point.join("a file"), b"").unwrap();
        make_out_dir_once(&mount_point.join("out")).unwrap();
        assert!(mount_point.join("out").is_dir());
        // One that exists is left as it is.
        make_out_dir_once(&mount_point.join("out")).unwrap();
    }

    #[test]
    fn an_output_counts_for_a_source_only_touched_and_not_for_one_rewritten() {
        let source = Folder::new("changed-in", &["c.mkv"]);
        let out = Folder::new("changed-out", &["c.mp4"]);
        let input = source.0.join("c.mkv");
        let meta = std::fs::metadata(&input).unwrap();
        let now = crate::source::modified_seconds(&meta).unwrap();
        let then = crate::source::Recorded::of(&input).unwrap().1;
        let print = then.fingerprint.unwrap();
        let hash = crate::source::WholeHash::start(&input, 5).finish(&|| false).unwrap();
        let report = |fields: serde_json::Value| std::fs::write(out.0.join("c.av1opt.json"), fields.to_string()).unwrap();
        report(serde_json::json!({"input": input, "source_bytes": 5, "source_mtime": now}));
        assert_eq!(source.walk(&out.0, &options()).1["c.mkv"].0, "done_before");
        // Another time and the same contents: touched, or copied without its dates.
        report(serde_json::json!({"input": input, "source_bytes": 5, "source_mtime": now - 60, "source_fingerprint": print, "source_hash": hash}));
        assert_eq!(source.walk(&out.0, &options()).1["c.mkv"].0, "done_before");
        // Another time and other contents, as for a file allocated ahead and filled in later.
        for other in [serde_json::json!({"source_hash": "0000000000000000"}), serde_json::json!({"source_fingerprint": "0000000000000000"})] {
            let mut fields = serde_json::json!({"input": input, "source_bytes": 5, "source_mtime": now - 60});
            fields.as_object_mut().unwrap().extend(other.as_object().unwrap().clone());
            report(fields);
            let (status, detail) = source.walk(&out.0, &options()).1["c.mkv"].clone();
            assert_eq!(status, "error");
            assert!(detail.contains("before it changed"), "{detail}");
        }
        // A report without the whole hash cannot tell, says why, and says what can be done.
        for (fields, why) in [
            (serde_json::json!({"input": input, "source_bytes": 5, "source_mtime": now - 60}), "from an earlier version"),
            (serde_json::json!({"input": input, "source_bytes": 5, "source_mtime": now - 60, "source_fingerprint": null}), "could not be read when its encode began"),
            (serde_json::json!({"input": input, "source_bytes": 5, "source_mtime": now - 60, "source_fingerprint": print}), "only samples"),
        ] {
            report(fields);
            let (status, detail) = source.walk(&out.0, &options()).1["c.mkv"].clone();
            assert_eq!(status, "error");
            assert!(detail.contains("when it was last changed at another time") && detail.contains(why) && detail.contains(&format!("delete {}", out.0.join("c.mp4").display())), "{detail}");
        }
        // A report from before the time was kept judges by the size alone.
        report(serde_json::json!({"input": input, "source_bytes": 5}));
        assert_eq!(source.walk(&out.0, &options()).1["c.mkv"].0, "done_before");
    }

    #[test]
    fn a_finished_output_elsewhere_still_counts_for_its_own_input_only() {
        let source = Folder::new("elsewhere-in", &["c.mkv"]);
        let out = Folder::new("elsewhere-out", &["c.mp4"]);
        std::fs::write(out.0.join("c.av1opt.json"), serde_json::json!({"input": source.0.join("c.mkv"), "source_bytes": 5}).to_string()).unwrap();
        assert_eq!(source.walk(&out.0, &options()).1["c.mkv"].0, "done_before");
        std::fs::write(out.0.join("c.av1opt.json"), serde_json::json!({"input": "/elsewhere/c.mkv", "source_bytes": 5}).to_string()).unwrap();
        assert!(source.walk(&out.0, &options()).1["c.mkv"].1.starts_with("output name collision"));
    }

    #[test]
    fn a_log_that_cannot_be_written_does_not_stop_the_batch() {
        let source = Folder::new("log-in", &["a.mkv", "b.mkv"]);
        let out = Folder::new("log-out", &[]);
        std::fs::write(out.0.join("elsewhere"), b"keep").unwrap();
        std::os::unix::fs::symlink(out.0.join("elsewhere"), out.0.join(LOG_NAME)).unwrap();
        let (sum, ends) = source.walk(&out.0, &options());
        assert_eq!((sum.failed, ends.len()), (2, 2));
        assert_eq!(std::fs::read(out.0.join("elsewhere")).unwrap(), b"keep");
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
    fn a_walk_stays_out_of_what_a_scan_stays_out_of() {
        let f = Folder::new("trash-walk", &[]);
        for d in [".Trash-1000/files", ".Trash/files", "lost+found", "$RECYCLE.BIN", "System Volume Information", "shows"] {
            std::fs::create_dir_all(f.0.join(d)).unwrap();
            std::fs::write(f.0.join(d).join("old.mkv"), d).unwrap();
        }
        // A file of such a name is a file like any other.
        std::fs::write(f.0.join(".Trash.mkv"), b"x").unwrap();
        let out = Folder::new("trash-walk-out", &[]);
        let jobs = expand(std::slice::from_ref(&f.0), &out.0, a_video);
        let names: Vec<String> = jobs.iter().map(|j| j.input.strip_prefix(&f.0).unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, [".Trash.mkv", "shows/old.mkv"]);
    }

    #[test]
    fn a_file_reached_twice_is_one_job_placed_by_the_first_input() {
        let f = Folder::new("reached-twice", &["top.mkv"]);
        let sub = f.0.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("x.mkv"), b"video").unwrap();
        std::fs::hard_link(sub.join("x.mkv"), f.0.join("linked.mkv")).unwrap();
        let out = PathBuf::from("/out");
        let inputs = |names: &[PathBuf]| expand_once(names, &out, a_video);
        let from = |jobs: &[Job]| jobs.iter().map(|j| (j.input.clone(), j.out_dir.clone())).collect::<Vec<_>>();
        let lib = f.0.file_name().unwrap();

        // The inner folder given as well, after the outer one and before it.
        let (jobs, left) = inputs(&[f.0.clone(), sub.clone()]);
        assert_eq!(from(&jobs), [(f.0.join("linked.mkv"), out.join(lib)), (f.0.join("top.mkv"), out.join(lib))]);
        assert_eq!(left, [sub.join("x.mkv"), sub.join("x.mkv")], "by its link's folder, then as given");
        let (jobs, left) = inputs(&[sub.clone(), f.0.clone()]);
        assert_eq!(from(&jobs), [(sub.join("x.mkv"), out.join("sub")), (f.0.join("top.mkv"), out.join(lib))]);
        assert_eq!(left, [f.0.join("linked.mkv"), sub.join("x.mkv")]);

        // The same file named twice, once through a link to it; one that is not there stays,
        // for its job to say so.
        let link = f.0.join("link-to-top");
        std::os::unix::fs::symlink(f.0.join("top.mkv"), &link).unwrap();
        let gone = f.0.join("gone.mkv");
        let (jobs, left) = inputs(&[f.0.join("top.mkv"), link.clone(), f.0.join("top.mkv"), gone.clone(), gone.clone()]);
        assert_eq!(jobs.iter().map(|j| j.input.clone()).collect::<Vec<_>>(), [f.0.join("top.mkv"), gone.clone(), gone]);
        assert_eq!(left, [link, f.0.join("top.mkv")]);
    }

    #[test]
    fn an_encode_made_when_a_folder_was_given_differently_counts_as_done() {
        let f = Folder::new("placed", &[]);
        let (kay, sofia) = (f.0.join("Kay"), f.0.join("Sofia"));
        for d in [&kay, &sofia] {
            std::fs::create_dir_all(d.join("old")).unwrap();
        }
        for file in ["Kay/a.mkv", "Kay/old/b.mkv", "Sofia/c.mkv"] {
            std::fs::write(f.0.join(file), b"video").unwrap();
        }
        let out = Folder::new("placed-out", &[]);
        let finished = |place: &Path, stem: &str, source: &Path, bytes: u64| {
            std::fs::create_dir_all(place).unwrap();
            std::fs::write(place.join(format!("{stem}.mp4")), b"av1").unwrap();
            let fields = serde_json::json!({"input": source, "output": place.join(format!("{stem}.mp4")), "source_bytes": bytes});
            std::fs::write(place.join(format!("{stem}.av1opt.json")), fields.to_string()).unwrap();
        };
        let both = [kay.clone(), sofia.clone()];

        // Made by `encode Kay -o out`, looked for by `encode Kay Sofia -o out`.
        finished(&out.0, "a", &kay.join("a.mkv"), 5);
        finished(&out.0.join("old"), "b", &kay.join("old/b.mkv"), 5);
        let (sum, ends) = run_all(&expand(&both, &out.0, a_video), &out.0, &options());
        assert_eq!(ends["a.mkv"], ("done_before".to_string(), out.0.join("a.mp4").display().to_string()));
        assert_eq!(ends["b.mkv"], ("done_before".to_string(), out.0.join("old/b.mp4").display().to_string()));
        assert!(reached_the_encode(&ends["c.mkv"]), "{:?}", ends["c.mkv"]);
        assert_eq!(sum.already_done, 2);
        assert!(!out.0.join("Kay").exists(), "nothing is made at the other place");

        // Not when it is no longer this file's encode, or when it is to be made again.
        finished(&out.0, "a", &kay.join("a.mkv"), 4);
        finished(&out.0.join("old"), "b", &sofia.join("c.mkv"), 5);
        let (_, ends) = run_all(&expand(&both, &out.0, a_video), &out.0, &options());
        assert!(reached_the_encode(&ends["a.mkv"]) && reached_the_encode(&ends["b.mkv"]), "{ends:?}");
        finished(&out.0, "a", &kay.join("a.mkv"), 5);
        let (_, ends) = run_all(&expand(&both, &out.0, a_video), &out.0, &Options { overwrite: true, ..options() });
        assert!(reached_the_encode(&ends["a.mkv"]), "{:?}", ends["a.mkv"]);

        // And the other way round: made with both folders, looked for with one.
        let other = Folder::new("placed-back", &[]);
        finished(&other.0.join("Kay"), "a", &kay.join("a.mkv"), 5);
        let (_, ends) = run_all(&expand(std::slice::from_ref(&kay), &other.0, a_video), &other.0, &options());
        assert_eq!(ends["a.mkv"], ("done_before".to_string(), other.0.join("Kay/a.mp4").display().to_string()));
    }

    #[test]
    fn jobs_are_ordered_by_worth_and_ties_keep_their_place() {
        let job = |name: &str| Job { input: PathBuf::from(name), out_dir: PathBuf::from("/out"), also_at: None };
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
    fn a_name_with_a_tab_or_line_break_stays_its_own_in_the_log() {
        let dir = std::env::temp_dir().join(format!("av1opt-oddlog-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join(LOG_NAME);
        let names = ["/v/a\tb.mkv", "/v/c\nd.mkv", "/v/e\rf.mkv", "/v/back\\slash.mkv", "/v/lit\\tab.mkv"];
        for n in names {
            log_line(&log, "skip", Path::new(n), 10, "lean", "s").unwrap();
        }
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), names.len());
        let skips = read_skips(&log, "s");
        for n in names {
            assert!(skips.contains_key(&(PathBuf::from(n), 10)), "{n:?}");
        }
        // The file with spaces where the other has a tab is another file.
        assert!(!skips.contains_key(&(PathBuf::from("/v/a b.mkv"), 10)));
        assert!(!skips.contains_key(&(PathBuf::from("/v/c d.mkv"), 10)));
        // A backslash in the log of an earlier version stays what it was.
        assert_eq!(unescape_name("/v/back\\slash.mkv"), "/v/back\\slash.mkv");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_name_that_is_not_text_is_refused_before_anything_is_looked_up() {
        use std::os::unix::ffi::OsStrExt;
        let f = Folder::new("nottext", &[]);
        let out = Folder::new("nottext-out", &[]);
        let odd = f.0.join(std::ffi::OsStr::from_bytes(b"a\xff.mkv"));
        std::fs::write(&odd, b"video").unwrap();
        // What the name would be as text: an encode of another file, which must not count.
        std::fs::write(out.0.join("a\u{fffd}.mp4"), b"other").unwrap();
        let (sum, ends) = f.walk(&out.0, &options());
        assert_eq!((sum.failed, sum.already_done), (1, 0));
        assert_eq!(ends["a\u{fffd}.mkv"], ("error".to_string(), crate::tools::NOT_TEXT.to_string()));
    }

    #[test]
    fn a_name_that_is_not_text_stands_in_no_real_files_way() {
        use std::os::unix::ffi::OsStrExt;
        let f = Folder::new("nottext-clash", &["a\u{fffd}.mkv"]);
        std::fs::write(f.0.join(std::ffi::OsStr::from_bytes(b"a\x80.mkv")), b"video").unwrap();
        let out = Folder::new("nottext-clash-out", &[]);
        let o = Options { overwrite: true, ..options() };
        let jobs = expand(std::slice::from_ref(&f.0), &out.0, |p| p.extension().is_some());
        assert_eq!(jobs.len(), 2);
        assert!(clashes(&jobs, &o).iter().all(Option::is_none));
        // And the run says so: both fail, neither for sharing an output with the other.
        let (sum, ends) = f.walk(&out.0, &o);
        assert_eq!(sum.failed, 2);
        assert!(ends.values().all(|e| !e.1.contains("another input")), "{ends:?}");
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

        // A byte that is not text costs its own line and no other.
        std::fs::OpenOptions::new().append(true).open(&log).map(|mut f| f.write_all(b"1\tskip\t/videos/\xff.mp4\t30\told\n")).unwrap().unwrap();
        log_line(&log, "skip", Path::new("/videos/c.mp4"), 40, "too lean", "s").unwrap();
        let after = read_skips(&log, "s");
        assert!(after.contains_key(&(PathBuf::from("/videos/b.mp4"), 20)) && after.contains_key(&(PathBuf::from("/videos/c.mp4"), 40)));
        assert_eq!(after.len(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
