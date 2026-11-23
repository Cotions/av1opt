//! Puts finished AV1 outputs where their originals were. The original goes to the trash (see
//! `trash`), the output takes its name (with the output's extension) in the original's folder,
//! and the report records where both went, so `undo` can put everything back while the
//! original is still in the trash.
//!
//! Nothing here deletes an original, and no file is ever written over: every move refuses to
//! land on an existing file. Space is only freed when the trash is emptied.

use crate::encode::REPORT_FORMAT;
use crate::source::{Recorded, Stamp, Verdict, WholeReads};
use crate::trash;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const REPORT_SUFFIX: &str = ".av1opt.json";

/// The most a report may hold. The longest encodes make reports of some hundreds of kilobytes.
pub const REPORT_LIMIT: u64 = 8 << 20;

/// What replacing one original would do. Made by `check`; nothing has changed yet.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub report: PathBuf,
    pub source: PathBuf,
    pub output: PathBuf,
    /// Where the output ends up: the original's folder and name, the output's extension.
    pub dest: PathBuf,
    pub source_bytes: u64,
    pub output_bytes: u64,
    /// The output is on another filesystem than the original: it is copied over, checked,
    /// and only then removed from where it was.
    pub copy: bool,
    /// The original is on ZFS, which is only written to when asked for.
    pub zfs: bool,
    /// Sections of the original that the output leaves out.
    pub removed: usize,
    /// What the report says of the original, which it was found to match.
    pub original: Recorded,
    /// How the original stood when it was last checked, for what is about to act on that check.
    pub source_stamp: Option<Stamp>,
}

/// An original that was replaced earlier, as its report tells it.
#[derive(Debug, Clone, PartialEq)]
pub struct Replaced {
    pub report: PathBuf,
    pub source: PathBuf,
    /// Where the AV1 file is now.
    pub dest: PathBuf,
    /// Where the AV1 file goes back to.
    pub from: PathBuf,
    /// Where the report says the AV1 file was before. Another place than `from` when a newer
    /// encode has taken that one.
    pub recorded_from: PathBuf,
    /// Where the original is in the trash.
    pub trash: PathBuf,
    pub source_bytes: u64,
    pub output_bytes: u64,
    /// What the file in the trash has to be to be the original.
    pub original: Recorded,
    /// The original is still in the trash and the AV1 file where it was put, both unchanged.
    pub can_undo: bool,
    /// The original's folder is on ZFS: it goes back there only when asked.
    pub zfs: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Check {
    Ready(Plan),
    Replaced(Replaced),
    /// Why this output stays where it is.
    No(String),
}

/// A regular file's size; None for anything else (missing, a folder, a link).
fn file_size(p: &Path) -> Option<u64> {
    std::fs::symlink_metadata(p).ok().filter(|m| m.is_file()).map(|m| m.len())
}

fn read_report(report: &Path) -> Result<Value, String> {
    let bytes = crate::files::read_regular(report, REPORT_LIMIT).map_err(|e| format!("no report ({e})"))?;
    let r: Value = serde_json::from_slice(&bytes).map_err(|_| "unreadable report".to_string())?;
    if r["format"].as_u64().unwrap_or(1) > u64::from(REPORT_FORMAT) {
        return Err("report written by a newer av1opt".into());
    }
    Ok(r)
}

/// An absolute path from a report. Reports of early versions held paths relative to the
/// folder `encode` ran in; guessing what they meant is not good enough for moving files.
fn full_path(r: &Value, key: &str) -> Result<PathBuf, String> {
    let p = PathBuf::from(r[key].as_str().ok_or_else(|| format!("report has no {key}"))?);
    if !p.is_absolute() {
        return Err("report from an older av1opt, without full paths".into());
    }
    // `a/../b` names a file outside `a` that a check of the folder `a` would let through, and
    // `encode` never writes one.
    if p.components().any(|c| c == std::path::Component::ParentDir) {
        return Err(format!("the report names a path with `..` in it ({}), which av1opt never writes", p.display()));
    }
    // Rebuilt from its parts, `/a/./b` and `/a/b` are one path, as they are one file.
    Ok(p.components().collect())
}

/// Whether `report` is the report `encode` wrote for an output at `output`: the same folder,
/// the output's name with `.av1opt.json` for its extension. A report is only a file, and what
/// it names gets moved; one that turns up somewhere else (in a folder of downloads, say) and
/// points at files elsewhere is not followed.
fn beside(report: &Path, output: &Path) -> bool {
    report.file_name().is_some() && report.file_name() == output.with_extension("av1opt.json").file_name() && same_folder(report, output)
}

fn same_folder(a: &Path, b: &Path) -> bool {
    let folder = |p: &Path| std::path::absolute(p).ok().and_then(|p| std::fs::canonicalize(p.parent()?).ok());
    folder(a).is_some_and(|d| Some(d) == folder(b))
}

/// The name for the report of `output` once a new encode needs its place, `NAME.replaced-N.av1opt.json`.
fn aside_name(output: &Path, number: u64) -> PathBuf {
    with_ending(output, &format!(".replaced-{number}.av1opt.json"))
}

/// `path` with `ending` in place of its extension, its stem shortened if the two do not fit.
fn with_ending(path: &Path, ending: &str) -> PathBuf {
    let mut name = crate::files::leave_room(crate::files::folder_of(path), path.file_stem().unwrap_or_default(), ending.len());
    name.push(ending);
    path.with_file_name(name)
}

/// Moves the report of an output that was replaced out of the way of a new encode of that
/// name. It says where the original is, so it has to stay, and be found as a report.
pub fn set_aside(report: &Path, output: &Path, now: u64) -> std::io::Result<PathBuf> {
    let mut number = now;
    loop {
        let aside = aside_name(output, number);
        match trash::rename_new(report, &aside) {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => number += 1,
            done => return done.map(|()| aside),
        }
    }
}

/// The number in the name of a report set aside.
fn aside_number(report: &Path) -> Option<u64> {
    let name = report.file_name()?.to_string_lossy();
    name.strip_suffix(REPORT_SUFFIX)?.rsplit_once(".replaced-")?.1.parse().ok()
}

/// Where the AV1 file goes back to when a replacement is undone from a report set aside. The
/// place it came from, `from`, now holds the newer encode, so it goes beside the report under
/// the report's name. None unless `report` is what [`set_aside`] makes of the report of `from`.
fn place_beside_aside(report: &Path, from: &Path) -> Option<PathBuf> {
    let number = aside_number(report)?;
    if report.file_name() != aside_name(from, number).file_name() || !same_folder(report, from) {
        return None;
    }
    let extension = from.extension()?.to_string_lossy();
    Some(with_ending(from, &format!(".replaced-{number}.{extension}")))
}

/// How far to go in telling whether an original is the file that was encoded.
#[derive(Clone, Copy, Default)]
struct Judging<'a> {
    /// For a list of many reports: whole files are not read for it. See `source::compare_for_list`.
    for_list: bool,
    /// Asked for on the command line: an original the report cannot tell about counts as the same.
    accept_changed_date: bool,
    /// The refusal may mention what the command line can do about it.
    on_command_line: bool,
    /// Whole files read for this act, so that it reads none of them twice.
    remember: Option<&'a WholeReads>,
    /// The original is about to go to the trash on this answer, so its date settles nothing.
    acting: bool,
    /// Only listed: an original in the trash is looked at as for a list of many, and what puts
    /// it back reads it in full first.
    listing: bool,
    /// For a list: a report behind what this record and the files show is listed as it would
    /// be once set right (see [`stranded`]). What acts sets it right first, reading in full.
    ledger: Option<&'a crate::ledger::Ledger>,
}

const CHANGED_WHILE_CHECKED: &str = "the original changed while it was being checked";

/// Looks at one report and the two files it names. Changes nothing. May read the whole original.
pub fn check(report: &Path) -> Check {
    check_judging(report, Judging::default())
}

/// [`check`] for a list of reports, which does not read whole originals: one that would have to
/// be read is listed as ready, and replacing it checks it in full.
#[cfg(test)]
pub fn check_for_list(report: &Path) -> Check {
    check_judging(report, Judging { for_list: true, ..Judging::default() })
}

/// [`check_for_list`] of a report as it would be once set right by what `ledger` and the files
/// show (see [`stranded`]).
#[cfg(any(feature = "ui", test))]
pub fn check_for_list_on_record(report: &Path, ledger: &crate::ledger::Ledger) -> Check {
    check_judging(report, Judging { for_list: true, ledger: Some(ledger), ..Judging::default() })
}

/// A report's contents as they would be once set right by what `ledger` and the files show
/// (see [`stranded`]), looked at as for a list; the contents as they are otherwise.
#[cfg(feature = "ui")]
pub fn as_recorded(report: &Path, r: Value, ledger: &crate::ledger::Ledger) -> Value {
    stranded(report, &r, ledger, false).map_or(r, |(fixed, _)| fixed)
}

/// What happened to a pair that its report does not say yet.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Stranded {
    /// The original went to the trash and the AV1 file took its place, and the record says so.
    Replaced,
    /// Both files went back, and the record says so.
    Undone,
}

/// The report as it would be had it been rewritten after the last replacement or undo of its
/// pair, when av1opt's record and the files both show that one went through and the report was
/// left behind: the run was killed between the record and the report, or the report could not
/// be written. Only what the record says counts, never the report's word: the report's facts of
/// the original have to be the recorded ones, the file in the trash has to say it came from the
/// original's place and be the recorded original, and the AV1 file has to have the recorded
/// hash. `full` reads all of the files compared; without it this is the look of a list, by
/// sizes and samples.
fn stranded(report: &Path, r: &Value, ledger: &crate::ledger::Ledger, full: bool) -> Option<(Value, Stranded)> {
    let source = full_path(r, "input").ok()?;
    let holds_output = |path: &Path, bytes: u64, hash: &str| {
        if !full {
            return file_size(path) == Some(bytes);
        }
        OpenFile::open(path, bytes).ok().and_then(|f| f.sha256().ok()).is_some_and(|now| now == hash)
    };
    let mut fixed = r.clone();
    if let Some(done) = r.get("replaced").filter(|d| d.is_object()) {
        // Put back, with the report still saying replaced.
        let dest = full_path(r, "output").ok()?;
        let from = full_path(done, "from").ok()?;
        let trash_path = full_path(done, "trash").ok()?;
        if !beside(report, &from) {
            return None;
        }
        let entry = ledger.entry(&from, &source).filter(|e| e.replaced.is_none())?;
        let undone = entry.undone.as_ref().filter(|m| m.trash == trash_path && m.from == from)?;
        let hash = entry.made.as_ref()?.output_sha256.as_deref()?;
        let back = file_size(&source) == Some(undone.original.source_bytes()) && std::fs::symlink_metadata(&trash_path).is_err();
        let left = dest == source || dest == from || std::fs::symlink_metadata(&dest).is_err();
        if !back || !left || !holds_output(&from, undone.output_bytes, hash) {
            return None;
        }
        fixed["output"] = json!(from);
        fixed.as_object_mut()?.remove("replaced");
        return Some((fixed, Stranded::Undone));
    }
    // Replaced, with the report still saying not.
    let from = full_path(r, "output").ok()?;
    if !beside(report, &from) || r["superseded"].as_bool() == Some(true) {
        return None;
    }
    let dest = source.with_extension(from.extension()?);
    let entry = ledger.entry(&dest, &source)?;
    let moved = entry.replaced.as_ref().filter(|m| m.from == from)?;
    let hash = entry.made.as_ref()?.output_sha256.as_deref()?;
    if !moved.original.has_contents_of(&Recorded::in_report(r)) || r["output_bytes"].as_u64() != Some(moved.output_bytes) {
        return None;
    }
    if dest != source && std::fs::symlink_metadata(&source).is_ok() {
        return None;
    }
    let known = moved.original.as_recorded();
    let in_trash = if full { trash::holds(&moved.trash, &source, &known) } else { trash::holds_for_list(&moved.trash, &source, &known) };
    if !in_trash || !holds_output(&dest, moved.output_bytes, hash) {
        return None;
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    fixed["output"] = json!(dest);
    fixed["replaced"] = json!({ "time": now, "from": from, "trash": moved.trash, "source_mtime": moved.original.modified() });
    Some((fixed, Stranded::Replaced))
}

/// Rewrites a report that is behind what av1opt's record and the files show (see [`stranded`]),
/// and puts right the files of a replacement or an undo that was stopped between two of its
/// moves (see [`CutOff`]), reading in full what that rests on. Called with the report's lock
/// held; a report rewritten here is locked before it takes the report's name, and that lock
/// goes into `held`, so that the pair stays this run's while it goes on. Says how the pair
/// stands then, if anything was done. A pair stopped half-way whose files are not as that run
/// would have left them is an error that says so, and nothing of it is moved.
fn set_right(report: &Path, allow_zfs: bool, held: &mut Vec<Lock>) -> Result<Option<Stranded>> {
    let mut done = None;
    // A report set right can show a move that was cut off, and the other way round.
    for _ in 0..3 {
        let Ok(r) = read_report(report) else { break };
        let ledger = crate::ledger::load();
        if let Some((fixed, what)) = stranded(report, &r, &ledger, true) {
            write_report_held(report, &fixed, held).context("the report is behind what av1opt's record and the files show, and could not be set right")?;
            done = Some(what);
        } else if let Some(cut) = CutOff::of(report, &r, &ledger, true) {
            done = Some(cut.put_right(report, &r, allow_zfs, held)?);
        } else {
            break;
        }
    }
    Ok(done)
}

/// A pair whose replacement or undo was stopped between two of its moves (killed, or the power
/// went): the line av1opt writes before the first move of each is the last on record for the
/// original, and the report is the one that run worked from. What it was stopped at is told by
/// that line and the files, never by the report's word: the file in the trash has to say it came
/// from the original's place and be the recorded original, and each AV1 file the size and the
/// hash on that line. Without such a line nothing is under way, whatever the files look like:
/// an original thrown away by hand, say, with its AV1 file kept in the output folder.
#[derive(Debug)]
struct CutOff {
    source: PathBuf,
    /// The AV1 file's place in the output folder.
    from: PathBuf,
    /// Its place in the original's folder.
    dest: PathBuf,
    /// Where the original is in the trash, or was to go; None for a replacement stopped before
    /// that was known.
    trash: Option<PathBuf>,
    undoing: bool,
    source_bytes: u64,
    output_bytes: u64,
    output_sha256: String,
    /// The original as that run checked it.
    original: Recorded,
    left: Left,
}

/// Where a stopped run left the files.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Left {
    /// Nothing is moved: the original is in its place, or the AV1 file in the original's for an
    /// undo. Only the line on record is left over.
    Nothing,
    /// A replacement: the original in the trash and the AV1 file still in the output folder,
    /// with or without a copy of it already in the original's folder.
    Trashed { copy_placed: bool },
    /// A replacement: the original in the trash and the AV1 file in the original's folder.
    Placed,
    /// An undo: the AV1 file in the original's folder and a copy of it in the output folder,
    /// the original still in the trash.
    CopiedBack,
    /// An undo: the AV1 file back in the output folder, the original still in the trash.
    MovedBack,
    /// An undo: both files back.
    BothBack,
    /// Not as any step of that run leaves them: moved by hand since, or not the files recorded.
    Unknown,
}

impl CutOff {
    /// What `report` (its contents `r`) and `ledger` show of a move that was cut off, if one was.
    /// `full` reads the files compared in full, which acting on it needs; without it this is the
    /// look of a list, by sizes and samples.
    fn of(report: &Path, r: &Value, ledger: &crate::ledger::Ledger, full: bool) -> Option<CutOff> {
        let source = full_path(r, "input").ok()?;
        let u = ledger.under_way(&source)?;
        if r["output_bytes"].as_u64() != Some(u.output_bytes) {
            return None;
        }
        // The report has to be the one that run worked from, as it left it.
        match r.get("replaced").filter(|d| d.is_object()) {
            Some(done) if u.undoing => {
                let recorded = full_path(done, "from").ok()?;
                let from = if beside(report, &recorded) { Some(recorded) } else { place_beside_aside(report, &recorded) };
                if from.as_ref() != Some(&u.from) || full_path(r, "output").ok()? != u.dest || u.trash.as_ref() != Some(&full_path(done, "trash").ok()?) {
                    return None;
                }
            }
            None if !u.undoing => {
                let from = full_path(r, "output").ok()?;
                if from != u.from || !beside(report, &from) || r["superseded"].as_bool() == Some(true) || source.with_extension(from.extension()?) != u.dest {
                    return None;
                }
            }
            _ => return None,
        }
        let original = u.original.as_recorded();
        let (from, dest, bytes, hash) = (&u.from, &u.dest, u.output_bytes, u.output_sha256.as_str());
        let is_original = |p: &Path| if full { trash::is_file_recorded(p, &original) } else { file_size(p) == Some(u.original.source_bytes()) };
        let in_trash = u.trash.as_deref().is_some_and(|t| if full { trash::holds(t, &source, &original) } else { trash::holds_for_list(t, &source, &original) });
        let is_av1 = |p: &Path| if full { OpenFile::open(p, bytes).ok().and_then(|f| f.sha256().ok()).is_some_and(|now| now == hash) } else { file_size(p) == Some(bytes) };
        let taken = |p: &Path| std::fs::symlink_metadata(p).is_ok();
        // Two names of one file (a folder mounted twice) are not two copies of it.
        let at_dest = is_av1(dest);
        let at_from = from != dest && !one_file(from, dest) && is_av1(from);
        let left = if !u.undoing {
            let source_free = !taken(&source) || (*dest == source && at_dest);
            // The original is the first to move: with nothing at its name in the trash and a
            // file in its place that is not the AV1 file, nothing of this run's was moved (or
            // it was taken back since), whatever that file is now.
            if u.trash.as_deref().is_none_or(|t| !taken(t)) && !source_free {
                Left::Nothing
            } else if in_trash && source_free && at_from {
                if from == dest || !taken(dest) {
                    Left::Trashed { copy_placed: false }
                } else if at_dest {
                    Left::Trashed { copy_placed: true }
                } else {
                    Left::Unknown
                }
            } else if in_trash && source_free && at_dest && from != dest && !taken(from) {
                Left::Placed
            } else {
                Left::Unknown
            }
        } else {
            let back = u.trash.as_deref().is_some_and(|t| !taken(t)) && is_original(&source);
            // The AV1 file is the first to move: still in the original's place, with nothing
            // in the place it goes back to and the trash entry still there, it never left.
            if at_dest && u.trash.as_deref().is_some_and(taken) && (from == dest || !taken(from)) {
                Left::Nothing
            } else if in_trash && at_dest && at_from {
                Left::CopiedBack
            } else if in_trash && at_from && !taken(dest) {
                Left::MovedBack
            } else if back && (if from == dest { at_dest } else { at_from && (*dest == source || !taken(dest)) }) {
                Left::BothBack
            } else {
                Left::Unknown
            }
        };
        let (from, dest, trash) = (u.from.clone(), u.dest.clone(), u.trash.clone());
        let (source_bytes, output_sha256) = (u.original.source_bytes(), u.output_sha256.clone());
        Some(CutOff { source, from, dest, trash, undoing: u.undoing, source_bytes, output_bytes: bytes, output_sha256, original, left })
    }

    /// What a list says of it; nothing where nothing was moved.
    fn told(&self) -> Option<String> {
        let what = if self.undoing { "putting this original back" } else { "replacing this original" };
        let stopped = format!("av1opt was stopped half-way through {what} (killed, or the power went)");
        let trash = self.trash.as_deref().map_or_else(|| "the trash".to_string(), |t| t.display().to_string());
        let (source, from, dest) = (self.source.display(), self.from.display(), self.dest.display());
        Some(match self.left {
            Left::Nothing => return None,
            Left::Trashed { .. } | Left::Placed => format!("{stopped}: the original is in the trash at {trash}; putting it right takes it back to {source}, and the AV1 file back to {from}"),
            Left::CopiedBack => format!("{stopped}: the AV1 file is still at {dest}, and a copy of it at {from}; putting it right removes that copy"),
            Left::MovedBack => format!("{stopped}: the AV1 file is back at {from}, and the original still in the trash at {trash}; putting it right takes the original back to {source}"),
            Left::BothBack => format!("{stopped}: both files are back, at {source} and {from}; putting it right only writes that down"),
            Left::Unknown => format!(
                "{stopped}, and the files are not where that run would have left them (moved or changed since), so av1opt leaves them as they are: the original was at {source} on its way to {trash}, and the AV1 file at {} on its way to {}",
                if self.undoing { &dest } else { &from },
                if self.undoing { &from } else { &dest }
            ),
        })
    }

    /// What is said once it was put right; nothing where nothing was moved.
    fn done(&self) -> Option<String> {
        let what = if self.undoing { "putting this original back" } else { "replacing this original" };
        let (source, from, dest) = (self.source.display(), self.from.display(), self.dest.display());
        let now = match self.left {
            Left::Nothing | Left::Unknown => return None,
            Left::Trashed { .. } | Left::Placed => format!("the original is back at {source} and the AV1 file at {from}, as they were before"),
            Left::CopiedBack => format!("the copy at {from} was removed, and the AV1 file is at {dest}, as it was before"),
            Left::MovedBack | Left::BothBack => format!("the original is back at {source} and the AV1 file at {from}, as the undo was to leave them"),
        };
        Some(format!("av1opt had been stopped half-way through {what}; {now}"))
    }

    /// Whether putting it right writes to a folder on ZFS.
    fn on_zfs(&self) -> bool {
        self.folders_written().iter().any(|f| crate::fsguard::ensure_writable_location(f, false).is_err())
    }

    /// The folders putting it right writes to.
    fn folders_written(&self) -> Vec<PathBuf> {
        let folder = |p: &Path| p.parent().unwrap_or(Path::new("/")).to_path_buf();
        let trash_folder = || self.trash.as_deref().map(folder);
        let mut folders = match self.left {
            Left::Nothing if !self.undoing => trash_folder().into_iter().collect(),
            Left::Nothing | Left::BothBack | Left::Unknown => vec![],
            Left::Trashed { .. } => vec![folder(&self.source), folder(&self.dest)],
            Left::Placed => vec![folder(&self.source), folder(&self.dest), folder(&self.from)],
            Left::CopiedBack => vec![folder(&self.from)],
            Left::MovedBack => vec![folder(&self.source)],
        };
        folders.dedup();
        folders
    }

    /// Moves the files to the nearest state in which the report and the record tell their whole
    /// story: before the replacement, or, for an undo, the replacement it was undoing or the undo
    /// finished, whichever is one step away. No file is moved over another, the one AV1 file
    /// removed is a copy of the one that stays, and nothing is written in a folder on ZFS
    /// without `allow_zfs`. Says how the pair stands then.
    fn put_right(&self, report: &Path, r: &Value, allow_zfs: bool, held: &mut Vec<Lock>) -> Result<Stranded> {
        if self.left == Left::Unknown {
            bail!("{}", self.told().unwrap_or_default());
        }
        for folder in self.folders_written() {
            crate::fsguard::ensure_writable_location(&folder, allow_zfs).with_context(|| format!("{}, which is not done there without --allow-zfs", self.told().unwrap_or_default()))?;
        }
        let folder = |p: &Path| p.parent().unwrap_or(Path::new("/")).to_path_buf();
        let restore = || -> Result<()> {
            let trash = self.trash.as_deref().context("no trash entry on record")?;
            let original = OpenFile::open(trash, self.source_bytes).context("the file in the trash is not the original")?;
            if !trash::is_open_file_recorded(&original.file, &self.original) || !trash::says_came_from(trash, &self.source) {
                bail!("the file in the trash is not the original: it changed after the original went there");
            }
            trash::restore_if(trash, &self.source, &|m| original.is(m))?;
            sync_dir(&folder(&self.source));
            Ok(())
        };
        let move_av1 = |from: &Path, to: &Path| -> Result<()> {
            let file = OpenFile::open(from, self.output_bytes)?;
            if file.sha256().ok().as_deref() != Some(self.output_sha256.as_str()) {
                bail!("{} is not the AV1 file av1opt recorded, so it was left where it is", from.display());
            }
            clear_left_behind(to);
            move_open(&file, from, to, self.output_bytes)?;
            sync_dir(&folder(to));
            Ok(())
        };
        let settled = || crate::ledger::note_settled(&self.source).context("the files are as they were, but av1opt's own record of that could not be written");
        match self.left {
            Left::Nothing => {
                // A stop once the original's name in the trash was reserved leaves an info file
                // with no file: this run's own, and nobody else's to keep.
                if !self.undoing
                    && let Some(trash) = &self.trash
                {
                    trash::drop_info_left_alone(trash, &self.source);
                }
                settled()?;
                Ok(if self.undoing { Stranded::Replaced } else { Stranded::Undone })
            }
            Left::Trashed { copy_placed } => {
                if copy_placed {
                    remove_copy(&self.dest, &self.from, self.output_bytes, &self.output_sha256)?;
                    crate::signals::stop_point("recover:moved");
                }
                clear_left_behind(&self.dest);
                restore()?;
                crate::signals::stop_point("recover:restored");
                settled()?;
                Ok(Stranded::Undone)
            }
            Left::Placed => {
                move_av1(&self.dest, &self.from)?;
                crate::signals::stop_point("recover:moved");
                restore()?;
                crate::signals::stop_point("recover:restored");
                settled()?;
                Ok(Stranded::Undone)
            }
            Left::CopiedBack => {
                remove_copy(&self.from, &self.dest, self.output_bytes, &self.output_sha256)?;
                crate::signals::stop_point("recover:moved");
                settled()?;
                Ok(Stranded::Replaced)
            }
            Left::MovedBack | Left::BothBack => {
                if self.left == Left::MovedBack {
                    restore()?;
                    crate::signals::stop_point("recover:restored");
                }
                crate::ledger::note_undone(&self.source, &self.dest, &self.from).context("both files are back, but av1opt's own record of that could not be written")?;
                crate::signals::stop_point("recover:noted");
                let mut r = r.clone();
                if r["replaced"]["from"].as_str().is_some_and(|recorded| Path::new(recorded) != self.from) {
                    r["superseded"] = json!(true);
                }
                r["output"] = json!(self.from);
                if let Some(o) = r.as_object_mut() {
                    o.remove("replaced");
                }
                write_report_held(report, &r, held).context("both files are back, but the report could not be updated")?;
                Ok(Stranded::Undone)
            }
            Left::Unknown => unreachable!("refused above"),
        }
    }
}

/// Whether `a` and `b` are one file under two names, not following a link at the end of either.
fn one_file(a: &Path, b: &Path) -> bool {
    let id = |p: &Path| std::fs::symlink_metadata(p).ok().map(|m| (m.dev(), m.ino()));
    id(a).is_some_and(|x| Some(x) == id(b))
}

/// Removes `copy`, a second whole file of what `kept` holds, both with the full hash `hash`.
/// One file reached by two names (its folder mounted twice) is no copy of itself: then nothing
/// is removed. The copy first goes to a hidden name beside it, and only if `kept` is still there
/// under its name, unchanged, and still another file, does it go; otherwise it gets its name back.
fn remove_copy(copy: &Path, kept: &Path, bytes: u64, hash: &str) -> Result<()> {
    let file = OpenFile::open(copy, bytes)?;
    let other = OpenFile::open(kept, bytes)?;
    let not_a_copy = || anyhow!("{} is not a copy of {}, so it was left where it is", copy.display(), kept.display());
    if other.file.metadata().is_ok_and(|m| file.is(&m)) {
        bail!("{} and {} are one file under two names, so it was left where it is", copy.display(), kept.display());
    }
    if file.sha256().ok().as_deref() != Some(hash) || other.sha256().ok().as_deref() != Some(hash) {
        return Err(not_a_copy());
    }
    let hidden = staging_name(copy);
    trash::rename_new(copy, &hidden).with_context(|| format!("cannot remove {}, a copy of {}", copy.display(), kept.display()))?;
    crate::signals::stop_point("recover:hidden");
    if !(file.is_at(&hidden) && other.is_at(kept) && other.untouched() && !one_file(&hidden, kept)) {
        let back = trash::rename_new(&hidden, copy);
        return Err(match back {
            Ok(()) => not_a_copy(),
            Err(e) => anyhow!("{} changed while its copy was being removed, so the copy was kept, at {} ({e})", kept.display(), hidden.display()),
        });
    }
    std::fs::remove_file(&hidden).with_context(|| format!("cannot remove {}, a copy of {}", hidden.display(), kept.display()))?;
    sync_dir(crate::files::folder_of(copy));
    Ok(())
}

fn check_judging(report: &Path, judging: Judging) -> Check {
    match check_inner(report, judging) {
        Ok(c) => c,
        Err(why) => Check::No(why),
    }
}

/// What a report of a replacement knows of the original it sent to the trash, if it has its
/// size. The time is the one written down at the replacement, which is that of the file that
/// went; reports from before that have the time at the encode, and the fingerprint settles it
/// if the original was touched in between.
pub fn original_in_trash(r: &Value) -> Option<Recorded> {
    let mut known = Recorded::in_report(r);
    known.size?;
    known.modified = r["replaced"]["source_mtime"].as_u64().or(known.modified);
    Some(known)
}

fn check_inner(report: &Path, judging: Judging) -> Result<Check, String> {
    if std::fs::symlink_metadata(report).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err("the report is a link".into());
    }
    let r = read_report(report)?;
    let r = match judging.ledger {
        Some(ledger) => stranded(report, &r, ledger, !(judging.for_list || judging.listing)).map_or(r, |(fixed, _)| fixed),
        None => r,
    };
    let source = full_path(&r, "input")?;
    let output = full_path(&r, "output")?;
    let source_bytes = r["source_bytes"].as_u64().ok_or("report has no sizes")?;
    let output_bytes = r["output_bytes"].as_u64().ok_or("report has no sizes")?;
    if let Some(done) = r.get("replaced").filter(|d| d.is_object()) {
        let recorded = full_path(done, "from")?;
        let trash = full_path(done, "trash")?;
        let from = if beside(report, &recorded) { Some(recorded.clone()) } else { place_beside_aside(report, &recorded) };
        let Some(from) = from else {
            return Err("the report is not the one next to where the output was".into());
        };
        if !crate::scan::is_video(&source) || !crate::scan::is_video(&output) {
            return Err("the report names a file that is not a video".into());
        }
        // `apply` puts the AV1 file under the original's name, or leaves it where it was:
        // anywhere else is not a place a replacement can have left it.
        let placed = recorded.extension().map(|ext| source.with_extension(ext));
        if output != recorded && Some(&output) != placed.as_ref() {
            return Err("the report does not say where the AV1 file went in a way `replace` would have".into());
        }
        let original = original_in_trash(&r).ok_or("report has no sizes")?;
        let in_trash = if judging.for_list || judging.listing { trash::holds_for_list(&trash, &source, &original) } else { trash::holds(&trash, &source, &original) };
        let can_undo = file_size(&output) == Some(output_bytes) && in_trash;
        let zfs = source.parent().is_some_and(crate::fsguard::on_zfs);
        return Ok(Check::Replaced(Replaced { report: report.to_path_buf(), source, dest: output, from, recorded_from: recorded, trash, source_bytes, output_bytes, original, can_undo, zfs }));
    }
    if source == output {
        return Err("output and original are the same file".into());
    }
    if !beside(report, &output) {
        return Err("the report is not the one next to its output".into());
    }
    if r["superseded"].as_bool() == Some(true) {
        return Err("a newer encode took the place of this one".into());
    }
    if !crate::scan::is_video(&source) || !crate::scan::is_video(&output) {
        return Err("the report names a file that is not a video".into());
    }
    match std::fs::symlink_metadata(&source) {
        Err(_) => return Err("the original is gone".into()),
        Ok(m) if m.file_type().is_symlink() => return Err("the original is a link".into()),
        Ok(m) if !m.is_file() => return Err("the original is not a file".into()),
        Ok(m) if m.len() != source_bytes => return Err("the original changed since the encode".into()),
        Ok(_) => {}
    }
    match file_size(&output) {
        None => return Err("the output is gone".into()),
        Some(n) if n != output_bytes => return Err("the output changed since the encode".into()),
        Some(_) => {}
    }
    if output_bytes >= source_bytes {
        return Err("the output is not smaller than the original".into());
    }
    // The original goes to the trash on the strength of this: it has to be the file that was
    // encoded, not one that took over its name and size since.
    let then = crate::source::Recorded::in_report(&r);
    let mut source_stamp = None;
    let verdict = match judging.remember {
        _ if judging.for_list => crate::source::compare_for_list(&then, &source),
        Some(reads) if judging.acting => {
            let (verdict, stamp) = crate::source::compare_to_act(&then, &source, reads);
            source_stamp = stamp;
            verdict
        }
        Some(reads) => crate::source::compare_remembering(&then, &source, reads),
        None => crate::source::compare(&then, &source),
    };
    match verdict {
        Verdict::Same => {}
        Verdict::OtherSize { .. } | Verdict::Changed => return Err("the original changed since the encode".into()),
        Verdict::CannotTell if judging.accept_changed_date => {}
        Verdict::CannotTell => {
            let or_accept = if judging.on_command_line { "; or, if you know that only its date changed, add --accept-changed-date" } else { "" };
            return Err(format!(
                "the original was last changed at another time than when it was encoded, and {}, so av1opt cannot tell whether it is still the file that was encoded; to go on, delete {} and encode the original again{or_accept}",
                then.why_it_cannot_tell(),
                output.display()
            ));
        }
        Verdict::Unreadable(e) => return Err(format!("the original cannot be read to check that it is the file that was encoded: {e}")),
    }
    // Size and date are all such a report holds, and another file can have both: one written
    // into the original's place, its date set back.
    if then.fingerprint.is_none() && then.whole.is_none() && !judging.accept_changed_date {
        // The page asks for the person's word itself, with this as what cannot be checked.
        let or_accept = if judging.on_command_line { "; or, if you know that it is, add --accept-changed-date to take it on what its report holds" } else { "" };
        return Err(format!(
            "its report, from an earlier version, holds only the original's size{}, which another file can have too, so av1opt cannot tell whether it is still the file that was encoded; to go on, delete {} and encode the original again{or_accept}",
            if then.modified.is_some() { " and date" } else { "" },
            output.display()
        ));
    }
    if judging.acting && source_stamp.is_none() {
        return Err(CHANGED_WHILE_CHECKED.into());
    }
    let dir = source.parent().ok_or("the original has no folder")?;
    let dest = match output.extension() {
        Some(ext) => source.with_extension(ext),
        None => return Err("the output has no file extension".into()),
    };
    if dest != source && dest != output && std::fs::symlink_metadata(&dest).is_ok() {
        return Err(format!("{} already exists", dest.display()));
    }
    let dev = |p: &Path| std::fs::metadata(p).map(|m| m.dev()).map_err(|e| format!("cannot read {}: {e}", p.display()));
    let copy = dest != output && dev(dir)? != dev(&output)?;
    let zfs = crate::fsguard::ensure_writable_location(dir, false).is_err();
    let removed = r["removed"].as_array().map_or(0, Vec::len);
    Ok(Check::Ready(Plan { report: report.to_path_buf(), source, output, dest, source_bytes, output_bytes, copy, zfs, removed, original: then, source_stamp }))
}

/// A replacement that went through.
#[derive(Debug)]
pub struct Done {
    pub dest: PathBuf,
    #[cfg_attr(not(feature = "ui"), allow(dead_code))]
    pub trash: PathBuf,
    pub source_bytes: u64,
    /// Things that went wrong after the files were in place (none of them lose anything).
    pub warnings: Vec<String>,
    /// The report could not be rewritten: what to do about it. It still says that nothing was
    /// replaced, so the run that did this has not done all it should.
    pub report_behind: Option<String>,
}

/// Bytes a new file in `dir` may take.
fn free_space(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: `c` is a valid NUL-terminated path and `st` a properly sized out-parameter.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0).then(|| st.f_bavail as u64 * st.f_frsize as u64)
}

const STAGING_START: &str = ".av1opt-";
const STAGING_END: &str = "part";

/// The name a file has while it is on its way to `dest`: hidden, in the same folder, never
/// taken for a video by a scan or a batch, and this copy's alone. Two long names can be cut
/// to the same beginning, and two replaces at once must not write into one file.
fn staging_name(dest: &Path) -> PathBuf {
    let tail = crate::files::unique_tail(STAGING_END);
    let mut name = std::ffi::OsString::from(STAGING_START);
    name.push(crate::files::leave_room(crate::files::folder_of(dest), dest.file_name().unwrap_or_default(), STAGING_START.len() + tail.len()));
    name.push(tail);
    dest.with_file_name(name)
}

/// Whether the file `name` is a copy on its way to `dest` that nobody is writing any more: one
/// whose process is gone, or one from before copies had a process in their name.
fn staging_left_behind(name: &std::ffi::OsStr, dest: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Some(wanted) = dest.file_name() else { return false };
    let Some(rest) = name.as_bytes().strip_prefix(STAGING_START.as_bytes()) else { return false };
    let Some(rest) = rest.strip_suffix(format!(".{STAGING_END}").as_bytes()) else { return false };
    let room = STAGING_START.len() + STAGING_END.len() + 1;
    if rest == crate::files::leave_room(crate::files::folder_of(dest), wanted, room).as_bytes() {
        return true;
    }
    // "<name, cut to fit>.<process>.<count>"
    let mut parts = rest.rsplitn(3, |b| *b == b'.');
    let (Some(count), Some(process), Some(start)) = (parts.next(), parts.next(), parts.next()) else { return false };
    let number = |b: &[u8]| std::str::from_utf8(b).ok().and_then(|t| t.parse::<u32>().ok());
    let (true, Some(process)) = (number(count).is_some() && wanted.as_bytes().starts_with(start), number(process)) else { return false };
    process != std::process::id() && !Path::new(&format!("/proc/{process}")).exists()
}

/// Removes what runs that were killed half-way left on their way to `dest`.
fn clear_left_behind(dest: &Path) {
    let Ok(entries) = std::fs::read_dir(crate::files::folder_of(dest)) else { return };
    for entry in entries.flatten() {
        if staging_left_behind(&entry.file_name(), dest) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// A file opened once. Whoever can write to its folder can put another file under its name at
/// any moment, so what is checked is read through this, and what is moved is compared with it.
pub struct OpenFile {
    file: File,
    stamp: Stamp,
}

impl OpenFile {
    /// Opens the plain file at `path`, which has to hold `bytes` bytes. A link is not followed.
    pub(crate) fn open(path: &Path, bytes: u64) -> Result<OpenFile> {
        let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path).map_err(|e| match e.raw_os_error() {
            Some(libc::ELOOP) => anyhow!("{} is a link", path.display()),
            _ => anyhow!("cannot open {}: {e}", path.display()),
        })?;
        let meta = file.metadata().with_context(|| format!("cannot look at {}", path.display()))?;
        if !meta.is_file() {
            bail!("{} is not a plain file", path.display());
        }
        if meta.len() != bytes {
            bail!("{} is not the size it should be", path.display());
        }
        // A file with a second name is reached through that name too: giving it the original's
        // permissions, or moving it into the original's place, would act on a file somebody may
        // have linked there from anywhere on the drive.
        if meta.nlink() != 1 {
            bail!("{} has other names on the drive too (hard links), so av1opt does not move it or change it; copy it to a file of its own to go on", path.display());
        }
        Ok(OpenFile { stamp: Stamp::of(&meta), file })
    }

    /// The SHA-256 of what is in this file, whatever its name leads to by now.
    pub fn sha256(&self) -> std::io::Result<String> {
        crate::ledger::sha256_of_open(&self.file)
    }

    fn is(&self, meta: &std::fs::Metadata) -> bool {
        self.stamp.same_file(meta)
    }

    /// Whether `path` leads to this file, not following a link.
    fn is_at(&self, path: &Path) -> bool {
        std::fs::symlink_metadata(path).is_ok_and(|m| self.is(&m))
    }

    /// Whether nothing has been written to this file, or changed about it, since it was opened.
    fn untouched(&self) -> bool {
        self.file.metadata().is_ok_and(|m| Stamp::of(&m) == self.stamp)
    }
}

/// Renames `from` to `to`, where nothing is, and makes sure that what arrived is `file`. If it is
/// not, it goes back where it came from, or under a hidden name beside that if the name has been
/// taken again: it must not stay in a place it was never checked for.
fn land(from: &Path, to: &Path, file: &OpenFile) -> std::io::Result<()> {
    land_watched(from, to, file, &|| {})
}

/// [`land`], with `after_rename` called between the rename and the look at what arrived.
fn land_watched(from: &Path, to: &Path, file: &OpenFile, after_rename: &dyn Fn()) -> std::io::Result<()> {
    // What the rename is about to move, which is not `file` if the name was taken over: only
    // that is taken away again, and not a file somebody put at `to` once it had arrived.
    let moving = std::fs::symlink_metadata(from)?;
    trash::rename_new(from, to)?;
    after_rename();
    if file.is_at(to) {
        return Ok(());
    }
    if !std::fs::symlink_metadata(to).is_ok_and(|m| (m.dev(), m.ino()) == (moving.dev(), moving.ino())) {
        return Err(std::io::Error::other(format!(
            "{} was replaced by another file while it was being moved, and {} holds a file this move did not put there by now, so that file was left where it is",
            from.display(),
            to.display()
        )));
    }
    let taken_back = trash::rename_new(to, from).or_else(|_| trash::rename_new(to, &staging_name(from)));
    Err(std::io::Error::other(match taken_back {
        Ok(()) => format!("{} was replaced by another file while it was being moved", from.display()),
        Err(e) => format!("{} was replaced by another file while it was being moved, and that file could not be taken away from {}: {e}", from.display(), to.display()),
    }))
}

/// Copies the open file `from` to `to` (which must not exist), flushed to disk and with the same
/// date. Returns the copy, open.
fn copy_new(from: &OpenFile, to: &Path, bytes: u64) -> Result<OpenFile> {
    let dir = to.parent().unwrap_or(Path::new("/"));
    if let Some(free) = free_space(dir)
        && free < bytes.saturating_add(16 << 20)
    {
        bail!("not enough room in {} ({} MB free, {} MB needed)", dir.display(), free >> 20, bytes >> 20);
    }
    let run = || -> Result<OpenFile> {
        let mut src = from.file.try_clone()?;
        src.seek(SeekFrom::Start(0))?;
        let mut dst = File::options().read(true).write(true).create_new(true).open(to)?;
        let n = std::io::copy(&mut src, &mut dst)?;
        if n != bytes {
            bail!("copied {n} bytes of {bytes}");
        }
        if let Ok(t) = src.metadata().and_then(|m| m.modified()) {
            let _ = dst.set_modified(t);
        }
        dst.sync_all()?;
        Ok(OpenFile { stamp: Stamp::of(&dst.metadata()?), file: dst })
    };
    run().inspect_err(|_| drop(std::fs::remove_file(to))).with_context(|| format!("cannot copy {} to {}", to.file_name().unwrap_or_default().to_string_lossy(), dir.display()))
}

/// How a file got to its new place.
enum Moved {
    Renamed,
    /// Across filesystems: this is the copy, and the file it was made from is gone.
    Copied(OpenFile),
}

/// Moves the open file `file` from `from` to a path nothing is at; across filesystems by a checked
/// copy under a hidden name, after which the old name is removed if it still leads to that file.
fn move_open(file: &OpenFile, from: &Path, to: &Path, bytes: u64) -> Result<Moved> {
    if !file.is_at(from) {
        bail!("{} was replaced by another file, so nothing was moved", from.display());
    }
    match land(from, to, file) {
        Ok(()) => Ok(Moved::Renamed),
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
            let part = staging_name(to);
            clear_left_behind(to);
            let copy = copy_new(file, &part, bytes)?;
            land(&part, to, &copy).inspect_err(|_| drop(std::fs::remove_file(&part))).with_context(|| format!("cannot name the copy {}", to.display()))?;
            // The copy's name on the drive before the only other name goes.
            sync_dir(crate::files::folder_of(to));
            crate::signals::stop_point("move:copied");
            if !file.is_at(from) {
                eprintln!("warning: copied, but {} is another file by now, so it was left there", from.display());
            } else if let Err(e) = std::fs::remove_file(from) {
                eprintln!("warning: copied, but could not remove {}: {e}", from.display());
            }
            Ok(Moved::Copied(copy))
        }
        Err(e) => Err(e).with_context(|| format!("cannot move {} to {}", from.display(), to.display())),
    }
}

fn sync_dir(dir: &Path) {
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
}

/// Rewrites the report through a second file, so a crash leaves the old or the new one. The
/// report keeps its date: that is when the encode was made, and History sorts by it.
fn write_report(report: &Path, r: &Value) -> Result<()> {
    write_report_locking(report, r, None)
}

/// [`write_report`] while the pair's files are still to be moved: the new report is locked
/// before it takes the report's name, and the lock goes into `held`. The lock on the old one
/// holds a file that no longer has the name, and another run would find the new one free.
fn write_report_held(report: &Path, r: &Value, held: &mut Vec<Lock>) -> Result<()> {
    write_report_locking(report, r, Some(held))
}

fn write_report_locking(report: &Path, r: &Value, held: Option<&mut Vec<Lock>>) -> Result<()> {
    use std::os::fd::AsRawFd;
    let date = std::fs::metadata(report).and_then(|m| m.modified()).ok();
    let part = crate::files::save_name(report);
    let write = || -> Result<File> {
        // (Made anew: a link left at that name is not written through.)
        let _ = std::fs::remove_file(&part);
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&part)?;
        // SAFETY: flock on a descriptor we own; no memory is involved. Nobody else has this new
        // file open; a filesystem without locks is carried on with, as `lock` does.
        unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        std::io::Write::write_all(&mut f, &serde_json::to_vec_pretty(r)?)?;
        if let Some(date) = date {
            let _ = f.set_modified(date);
        }
        f.sync_all()?;
        std::fs::rename(&part, report)?;
        Ok(f)
    };
    let f = write().inspect_err(|_| drop(std::fs::remove_file(&part)))?;
    sync_dir(crate::files::folder_of(report));
    if let Some(held) = held {
        held.push(Lock(f));
    }
    Ok(())
}

/// How long [`lock`] waits for a report another run holds.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// Held while the files of a report are moved, so that two runs (two clicks, or the command
/// and the page at once) never work on the same pair. Gone when the returned lock is dropped.
fn lock(report: &Path) -> Result<Lock> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    // Without waiting: a pipe left under a report's name would never finish opening.
    let f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(report).with_context(|| format!("no report at {}", report.display()))?;
    // A lock just let go of is still held for a moment by a program another thread is starting
    // (the encoder, or a tool the page runs): its copy of the descriptor goes only once that
    // program has started. A busy report is tried again for that long before it is said to be.
    let until = std::time::Instant::now() + LOCK_WAIT;
    loop {
        // SAFETY: flock on a descriptor we own; no memory is involved.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            break;
        }
        // A filesystem without locks: carry on as a single run would.
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::WouldBlock {
            break;
        }
        if std::time::Instant::now() >= until {
            bail!("this file is being replaced or put back right now");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Ok(Lock(f))
}

struct Lock(std::fs::File);

impl Drop for Lock {
    /// Closing the file is not enough: a process another thread is starting at this moment
    /// (an ffmpeg of the queue) holds a copy of the descriptor until it execs, and the lock
    /// would outlive us by that long, refusing the next click.
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // SAFETY: flock on a descriptor we own; no memory is involved.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Replaces the original named in `report` by its output, after checking both again, if
/// `accept` agrees to what the report comes to. It is asked with the report locked and read for
/// the last time, so what it agreed to is what is done.
///
/// The original goes to the trash, then the output is renamed into its place; if that fails
/// the original is taken back out of the trash. An output on another filesystem is first
/// copied next to the original under a hidden name, so a copy that fails (no room, a read
/// error) leaves the original untouched; its old file is removed once the copy is in place.
/// The hidden file is only ever a copy: deleting one never loses anything.
///
/// The output is opened once, before `accept` is asked, and handed to it: what is read through
/// that is what ends up at the destination, or nothing does.
#[cfg(feature = "ui")]
pub fn apply_if(report: &Path, allow_zfs: bool, accept: AcceptOutput) -> Result<Done> {
    apply_judging(report, allow_zfs, Judging::default(), &trash::trash_noting, accept, &|| {})
}

/// [`check_for_list`] on behalf of someone who says that only the original's date changed, as
/// `--accept-changed-date` does: what the report cannot tell about counts as the same. The report
/// is looked at as it would be once set right by what `ledger` and the files show.
#[cfg(feature = "ui")]
pub fn check_for_list_accepting_changed_date(report: &Path, ledger: &crate::ledger::Ledger) -> Check {
    check_judging(report, Judging { for_list: true, accept_changed_date: true, ledger: Some(ledger), ..Judging::default() })
}

/// [`apply_if`] on behalf of someone who says that only the original's date changed. An original
/// known to differ is still refused.
#[cfg(feature = "ui")]
pub fn apply_if_date_accepted(report: &Path, allow_zfs: bool, accept: AcceptOutput) -> Result<Done> {
    apply_judging(report, allow_zfs, Judging { accept_changed_date: true, ..Judging::default() }, &trash::trash_noting, accept, &|| {})
}

#[cfg(test)]
fn apply_with(report: &Path, allow_zfs: bool, to_trash: ToTrash) -> Result<Done> {
    apply_judging(report, allow_zfs, Judging::default(), to_trash, &|_, _| Ok(()), &|| {})
}

/// Has the last word on what a report comes to, or says why not.
type Accept<'a, T> = &'a dyn Fn(&T) -> Result<(), String>;

/// What sends the original to the trash ([`trash::trash_noting`] but in tests), telling the
/// function it is given the name the original is about to have there.
type ToTrash<'a> = &'a dyn Fn(&Path, trash::BeforeMove) -> Result<PathBuf>;

/// [`Accept`] for a replacement, with the output as it was opened to be moved.
type AcceptOutput<'a> = &'a dyn Fn(&Plan, &OpenFile) -> Result<(), String>;

/// `before_report` is called once the files are in place and on record, before the report is
/// rewritten.
fn apply_judging(report: &Path, allow_zfs: bool, judging: Judging, to_trash: ToTrash, accept: AcceptOutput, before_report: &dyn Fn()) -> Result<Done> {
    let mut held = vec![lock(report)?];
    let reads_of_this_one = WholeReads::default();
    // The report as it is on disk: one that is behind the record is set right, and then read,
    // and so is a pair whose last move on record was cut off.
    let judging = Judging { acting: true, remember: judging.remember.or(Some(&reads_of_this_one)), ledger: None, ..judging };
    let mut checked = check_judging(report, judging);
    if !matches!(checked, Check::Ready(_)) || under_way(report) {
        match set_right(report, allow_zfs, &mut held)? {
            Some(Stranded::Undone) => checked = check_judging(report, judging),
            Some(Stranded::Replaced) => bail!("already replaced: av1opt's record and the files show that this original went to the trash, and its report now says so too"),
            None => {}
        }
    }
    let p = match checked {
        Check::Ready(p) => p,
        Check::Replaced(_) => bail!("already replaced"),
        Check::No(why) => bail!("{why}"),
    };
    let source_stamp = p.source_stamp.context(CHANGED_WHILE_CHECKED)?;
    let output = OpenFile::open(&p.output, p.output_bytes)?;
    accept(&p, &output).map_err(|why| anyhow!(why))?;
    if p.zfs && !allow_zfs {
        bail!("the original is on ZFS, so it was left where it is and nothing was changed; --allow-zfs replaces originals there");
    }
    let source_meta = std::fs::symlink_metadata(&p.source)?;
    if !output.is_at(&p.output) || !output.untouched() {
        bail!("the output was changed or replaced by another file while it was being checked, so nothing was changed");
    }
    // On record before the first move, with what tells this run's moves from anyone else's.
    let on_record = crate::ledger::load().entry(&p.output, &p.source).and_then(|e| e.made.clone()).filter(|m| m.output_bytes == p.output_bytes).and_then(|m| m.output_sha256);
    let output_sha256 = match on_record {
        Some(hash) => hash,
        None => output.sha256().with_context(|| format!("cannot read {}, so nothing was changed", p.output.display()))?,
    };
    let replacing = crate::ledger::Replacing { source: &p.source, from: &p.output, dest: &p.dest, source_bytes: p.source_bytes, known: &p.original, output_bytes: p.output_bytes, output_sha256: &output_sha256 };
    crate::ledger::note_replacing(&replacing).context("av1opt's own record could not be written, and a replacement stopped half-way could not be put right without it, so nothing was changed")?;
    crate::signals::stop_point("replace:begun");
    let in_place = p.dest == p.output;
    let staged = staging_name(&p.dest);
    let stage = || {
        // The output itself is still where it was.
        clear_left_behind(&p.dest);
        copy_new(&output, &staged, p.output_bytes)
    };
    let mut staged_first = None;
    if !in_place && p.copy {
        staged_first = Some(stage()?);
        crate::signals::stop_point("replace:staged");
    }
    // Reading a large original takes minutes, and a write behind the place being read would not
    // have shown in what was read.
    if !std::fs::symlink_metadata(&p.source).is_ok_and(|m| Stamp::of(&m) == source_stamp) {
        if staged_first.is_some() {
            let _ = std::fs::remove_file(&staged);
        }
        bail!("{CHANGED_WHILE_CHECKED}, so nothing was changed");
    }
    let trashed = match to_trash(&p.source, &|t| crate::ledger::note_trashing(&p.source, t)) {
        Ok(t) => t,
        Err(e) => {
            if staged_first.is_some() {
                let _ = std::fs::remove_file(&staged);
            }
            return Err(e.context("the original could not be moved to the trash; nothing was changed"));
        }
    };
    // What went is what was checked, or it comes back.
    if !std::fs::symlink_metadata(&trashed).is_ok_and(|m| source_stamp.same_after_move(&m)) {
        if staged_first.is_some() {
            let _ = std::fs::remove_file(&staged);
        }
        return Err(match trash::restore(&trashed, &p.source) {
            Ok(()) => anyhow!("{CHANGED_WHILE_CHECKED}; it is back in its place"),
            Err(b) => anyhow!("{CHANGED_WHILE_CHECKED}, and it is still in the trash at {} ({b:#})", trashed.display()),
        });
    }
    crate::signals::stop_point("replace:trashed");
    // Some(copy): the output was copied, and its old file is still in the output folder.
    let placed: Result<Option<OpenFile>> = if in_place {
        if output.is_at(&p.output) { Ok(None) } else { Err(anyhow!("{} was replaced by another file", p.output.display())) }
    } else if let Some(copy) = staged_first {
        land(&staged, &p.dest, &copy).map(|()| Some(copy)).map_err(Into::into)
    } else {
        match land(&p.output, &p.dest, &output) {
            Ok(()) => Ok(None),
            // Same device number but not one mount (a filesystem mounted twice): copy after all.
            Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => stage().and_then(|copy| {
                land(&staged, &p.dest, &copy)?;
                Ok(Some(copy))
            }),
            Err(e) => Err(e.into()),
        }
    };
    let copy = match placed {
        Ok(copy) => copy,
        Err(e) => {
            if !in_place {
                let _ = std::fs::remove_file(&staged);
            }
            return Err(match trash::restore(&trashed, &p.source) {
                Ok(()) => anyhow!("cannot put the AV1 file at {} ({e:#}); the original is back in its place", p.dest.display()),
                Err(b) => anyhow!("cannot put the AV1 file at {} ({e:#}), and the original is still in the trash at {} ({b:#})", p.dest.display(), trashed.display()),
            });
        }
    };

    crate::signals::stop_point("replace:placed");
    let mut warnings = Vec::new();
    // Through the open file: by name, this would reach whatever has been put there since.
    if copy.as_ref().unwrap_or(&output).file.set_permissions(source_meta.permissions()).is_err() {
        warnings.push("could not give the AV1 file the original's permissions".to_string());
    }
    sync_dir(p.dest.parent().unwrap_or(Path::new("/")));
    if copy.is_some() {
        if !output.is_at(&p.output) {
            warnings.push(format!("the copy is in place, but {} is another file by now, so it was left there", p.output.display()));
        } else if let Err(e) = std::fs::remove_file(&p.output) {
            warnings.push(format!("the copy is in place, but {} could not be removed: {e}", p.output.display()));
        } else {
            sync_dir(crate::files::folder_of(&p.output));
        }
        crate::signals::stop_point("replace:removed");
    }
    // From here on both files are where they belong, and the line on record says that the
    // replacement is over. If what follows does not get done, the record is what the report
    // can be set right by.
    let source_mtime = std::fs::symlink_metadata(&trashed).ok().and_then(|m| crate::source::modified_seconds(&m));
    let went = Recorded { modified: source_mtime.or(p.original.modified), ..p.original.clone() };
    let move_noted = crate::ledger::note_replaced(&crate::ledger::Replacement { original: &p.source, trash: &trashed, output: &p.dest, from: &p.output, source_bytes: p.source_bytes, known: &went, output_bytes: p.output_bytes });
    crate::signals::stop_point("replace:noted");
    if let Err(e) = &move_noted {
        warnings.push(format!("av1opt's own record of this could not be written ({e:#}), so the page will not offer to put the original back; `av1opt replace --undo` in a terminal can"));
    }
    crate::signals::stop_point("replace:report");
    before_report();
    let noted = read_report(report).map_err(|e| anyhow!(e)).and_then(|mut r| {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        r["output"] = json!(p.dest);
        r["replaced"] = json!({ "time": now, "from": p.output, "trash": trashed, "source_mtime": source_mtime });
        write_report(report, &r)
    });
    let report_behind = noted.err().map(|e| {
        let at = format!("the original is in the trash at {} and the AV1 file at {}", trashed.display(), p.dest.display());
        let folder = |p: &Path| p.parent().map_or_else(String::new, |d| d.display().to_string());
        if move_noted.is_ok() {
            format!("the report could not be updated ({e:#}), so it still says that nothing was replaced: {at}. `av1opt replace --undo --apply {} {}` puts them back by av1opt's record, and the page can too", folder(report), folder(&p.source))
        } else {
            format!("the report could not be updated ({e:#}), and neither could av1opt's record: {at}; to put them back, move {} to {}, then take the original out of the trash", p.dest.display(), p.output.display())
        }
    });
    Ok(Done { dest: p.dest, trash: trashed, source_bytes: p.source_bytes, warnings, report_behind })
}

/// Undoes `apply` while the original is still in the trash: the AV1 file goes back where it
/// was, the original back to its place, and the report forgets the replacement.
#[cfg(test)]
fn undo(report: &Path, allow_zfs: bool) -> Result<Replaced> {
    undo_if(report, allow_zfs, &|_| Ok(()))
}

/// [`undo`], if `accept` agrees to what the report comes to, asked as [`apply_if`] asks.
pub fn undo_if(report: &Path, allow_zfs: bool, accept: Accept<Replaced>) -> Result<Replaced> {
    undo_watched(report, allow_zfs, accept, &|| {}, &|| {})
}

/// [`undo_if`], with `before_moving` called when everything has been checked and nothing moved,
/// and `before_report` once both files are back and on record, before the report is rewritten.
fn undo_watched(report: &Path, allow_zfs: bool, accept: Accept<Replaced>, before_moving: &dyn Fn(), before_report: &dyn Fn()) -> Result<Replaced> {
    let mut held = vec![lock(report)?];
    let first = check(report);
    let d = match first {
        Check::Replaced(d) if d.can_undo && !under_way(report) => d,
        first => match set_right(report, allow_zfs, &mut held)? {
            // What was asked is done already, by an undo whose report was left behind: what
            // `accept` would compare with the record, the record has been compared with.
            Some(Stranded::Undone) => match first {
                Check::Replaced(d) => return Ok(d),
                _ => bail!("this original has not been replaced, so there is nothing to put back"),
            },
            Some(Stranded::Replaced) => match check(report) {
                Check::Replaced(d) => d,
                Check::Ready(_) => bail!("this original has not been replaced, so there is nothing to put back"),
                Check::No(why) => bail!("{why}"),
            },
            None => match first {
                Check::Replaced(d) => d,
                Check::Ready(_) => bail!("this original has not been replaced, so there is nothing to put back"),
                Check::No(why) => bail!("{why}"),
            },
        },
    };
    accept(&d).map_err(|why| anyhow!(why))?;
    match file_size(&d.trash) {
        None => bail!("the original is no longer in the trash"),
        Some(n) if n != d.source_bytes => bail!("the file in the trash is not the original"),
        Some(_) => {}
    }
    if !trash::says_came_from(&d.trash, &d.source) {
        bail!("the trash does not say that {} came from {}", d.trash.display(), d.source.display());
    }
    // Opened before it is judged, and judged through what was opened, so that the file judged
    // can be told from one put there later.
    let original = OpenFile::open(&d.trash, d.source_bytes).context("the file in the trash is not the original")?;
    if !trash::is_open_file_recorded(&original.file, &d.original) {
        bail!("the file in the trash is not the original: it changed after the original went there");
    }
    // The original goes back to its folder: the same rule as when it left.
    crate::fsguard::ensure_writable_location(d.source.parent().context("the original has no folder")?, allow_zfs)?;
    if file_size(&d.dest) != Some(d.output_bytes) {
        bail!("the AV1 file is gone or has changed");
    }
    let av1 = OpenFile::open(&d.dest, d.output_bytes).context("the AV1 file is gone or has changed")?;
    if d.from != d.dest {
        if std::fs::symlink_metadata(&d.from).is_ok() {
            bail!("{} already exists", d.from.display());
        }
        let out_dir = d.from.parent().context("no output folder")?;
        crate::fsguard::ensure_writable_location(out_dir, allow_zfs)?;
        std::fs::create_dir_all(out_dir).with_context(|| format!("cannot make {}", out_dir.display()))?;
    }
    // The original's place may be the very path the AV1 file has (same extension), so the
    // AV1 file leaves first.
    if d.source != d.dest && std::fs::symlink_metadata(&d.source).is_ok() {
        bail!("{} already exists", d.source.display());
    }
    before_moving();
    if !original.is_at(&d.trash) || !original.untouched() {
        bail!("the file in the trash was changed or replaced while it was being checked, so nothing was moved");
    }
    let on_record = crate::ledger::load().entry(&d.dest, &d.source).and_then(|e| e.made.clone()).filter(|m| m.output_bytes == d.output_bytes).and_then(|m| m.output_sha256);
    let output_sha256 = match on_record {
        Some(hash) => hash,
        None => av1.sha256().with_context(|| format!("cannot read {}, so nothing was moved", d.dest.display()))?,
    };
    let undoing = crate::ledger::Undoing { source: &d.source, dest: &d.dest, back_to: &d.from, trash: &d.trash, source_bytes: d.source_bytes, known: &d.original, output_bytes: d.output_bytes, output_sha256: &output_sha256 };
    crate::ledger::note_undoing(&undoing).context("av1opt's own record could not be written, and an undo stopped half-way could not be put right without it, so nothing was moved")?;
    crate::signals::stop_point("undo:begun");
    let moved = if d.from == d.dest { None } else { Some(move_open(&av1, &d.dest, &d.from, d.output_bytes)?) };
    crate::signals::stop_point("undo:moved");
    if let Err(e) = trash::restore_if(&d.trash, &d.source, &|m| original.is(m)) {
        // Both go back to where the report says they are, so that this can be tried again.
        let at_from = match &moved {
            Some(Moved::Copied(copy)) => copy,
            _ => &av1,
        };
        let returned = if moved.is_some() { move_open(at_from, &d.from, &d.dest, d.output_bytes).map(drop) } else { Ok(()) };
        return Err(match returned {
            Ok(()) => e.context(format!("the original is still in the trash at {} and the AV1 file is at {}", d.trash.display(), d.dest.display())),
            Err(back) => e.context(format!("the original is still in the trash at {}, and the AV1 file is at {} because it could not go back to {} ({back:#})", d.trash.display(), d.from.display(), d.dest.display())),
        });
    }
    crate::signals::stop_point("undo:restored");
    sync_dir(d.source.parent().unwrap_or(Path::new("/")));
    if let Err(e) = crate::ledger::note_undone(&d.source, &d.dest, &d.from) {
        eprintln!("warning: the original is back, but av1opt's own record of that could not be written: {e:#}");
    }
    crate::signals::stop_point("undo:noted");
    before_report();
    let mut r = read_report(report).map_err(|e| anyhow!(e))?;
    // Undone from a report set aside: the AV1 file did not go back to where it came from, as
    // a newer encode is there, and that encode's report is the one to go by.
    if r["replaced"]["from"].as_str().is_some_and(|recorded| Path::new(recorded) != d.from) {
        r["superseded"] = json!(true);
    }
    r["output"] = json!(d.from);
    if let Some(o) = r.as_object_mut() {
        o.remove("replaced");
    }
    write_report(report, &r).context("both files are back, but the report could not be updated, so it still says that the original was replaced; `av1opt replace` or the page sets it right by av1opt's record")?;
    Ok(d)
}

/// Whether av1opt's record has a replacement or an undo of the original of `report` under way:
/// its line was written before its first move, and nothing after it says it ended.
fn under_way(report: &Path) -> bool {
    let source = read_report(report).ok().and_then(|r| full_path(&r, "input").ok());
    source.is_some_and(|s| crate::ledger::load().under_way(&s).is_some())
}

/// The reports under some folders (never looking into av1opt's own work folders).
pub fn report_files(dirs: &[PathBuf]) -> Vec<PathBuf> {
    report_names(dirs, &|e| e.file_type().is_file())
}

/// What goes by the name of a report under `dirs`, of the kinds `wanted` lets through.
fn report_names(dirs: &[PathBuf], wanted: &dyn Fn(&walkdir::DirEntry) -> bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for d in dirs {
        let walk = walkdir::WalkDir::new(d)
            .max_depth(8)
            .into_iter()
            .filter_entry(|e| !e.file_name().to_string_lossy().starts_with(".av1opt-"))
            .flatten();
        for e in walk {
            if wanted(&e) && e.file_name().to_string_lossy().ends_with(REPORT_SUFFIX) {
                out.push(e.into_path());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// The reports meant by what was named on the command line: every report under a folder, a
/// report itself, or the report next to an output file.
pub fn reports_for(paths: &[PathBuf]) -> Vec<PathBuf> {
    reports_found(paths, &report_files)
}

fn reports_found(paths: &[PathBuf], under: &dyn Fn(&[PathBuf]) -> Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for p in paths {
        if p.is_dir() {
            out.extend(under(std::slice::from_ref(p)));
        } else if p.to_string_lossy().ends_with(REPORT_SUFFIX) {
            out.push(p.clone());
        } else {
            out.push(p.with_extension("av1opt.json"));
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    out
}

/// The folders a `replace` was pointed at. A report is only a file, and whoever could put one
/// in those folders chose what it names: it is not followed out of them unless that was asked for.
struct NamedFolders(Option<Vec<PathBuf>>);

impl NamedFolders {
    /// The folders named, and the folder of each file named. None of them with `anywhere`.
    fn of(paths: &[PathBuf], anywhere: bool) -> NamedFolders {
        let folder = |p: &PathBuf| std::fs::canonicalize(if p.is_dir() { p.as_path() } else { p.parent()? }).ok();
        NamedFolders((!anywhere).then(|| paths.iter().filter_map(folder).collect()))
    }

    /// Whether the original at `source` may be touched. It may be gone (it is in the trash), so
    /// it is its folder that is looked up.
    fn holds(&self, source: &Path) -> bool {
        let Some(folders) = &self.0 else { return true };
        let parent = source.parent().and_then(|p| std::fs::canonicalize(p).ok());
        parent.is_some_and(|p| folders.iter().any(|f| p.starts_with(f)))
    }
}

const THREE_WAYS: &str = "name its folder too, add --anywhere if you trust this report, or take it on record with `av1opt adopt` on the output's folder";

fn outside_named(source: &Path) -> String {
    format!("the original {} is outside the folders named here and av1opt has no record of this pair; {THREE_WAYS}", source.display())
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The `replace` command: lists what would happen, and does it with `go`. A report is followed to
/// an original inside the folders in `paths`, or anywhere with `anywhere`, or where av1opt's own
/// record says that this output and this original belong together: then it is the record that
/// vouches for the original's path, and not the report.
pub fn run(paths: &[PathBuf], go: bool, back: bool, allow_zfs: bool, accept_changed_date: bool, anywhere: bool) -> Result<()> {
    run_watched(paths, go, back, allow_zfs, accept_changed_date, anywhere, &|| {})
}

/// [`run`], with `before_report` called as [`apply_judging`] calls it.
fn run_watched(paths: &[PathBuf], go: bool, back: bool, allow_zfs: bool, accept_changed_date: bool, anywhere: bool, before_report: &dyn Fn()) -> Result<()> {
    use crate::report::gb;
    // Only what is going to be replaced is worth keeping reads for.
    let reads = WholeReads::default();
    let reports = reports_for(paths);
    if reports.is_empty() {
        bail!("no av1opt reports (*.av1opt.json) found there");
    }
    let named = NamedFolders::of(paths, anywhere);
    let mut ledger = crate::ledger::load();
    // A pair whose report was left behind, or whose last move was cut off half-way, is set right
    // first when files may be changed, so that what is listed and done goes by where the files
    // are; when they may not, the list says what would be done about it.
    let (mut set_right_any, mut not_set_right) = (false, 0);
    for report in &reports {
        let Ok(r) = read_report(report) else { continue };
        let behind = stranded(report, &r, &ledger, false).is_some();
        let cut = if behind { None } else { CutOff::of(report, &r, &ledger, false) };
        if !behind && cut.is_none() {
            continue;
        }
        if let Some(cut) = &cut
            && let Some(told) = cut.told()
        {
            if cut.left == Left::Unknown {
                println!("{}: {told}", report.display());
                continue;
            }
            if !allow_zfs && cut.on_zfs() {
                println!("{}: {told}; that is on ZFS, so it is left as it is (--allow-zfs puts it right there too)", report.display());
                continue;
            }
            if !go {
                println!("{}: {told}; --apply does that first", report.display());
                continue;
            }
        }
        if !go {
            continue;
        }
        let mut held = Vec::new();
        match lock(report).and_then(|l| {
            held.push(l);
            set_right(report, allow_zfs, &mut held)
        }) {
            Ok(Some(_)) => {
                set_right_any = true;
                match cut.as_ref().and_then(CutOff::done) {
                    Some(done) => println!("put right {}: {done}", report.display()),
                    None if behind => println!("put right {}: its report was behind what av1opt's record and the files show", report.display()),
                    None => {}
                }
            }
            Ok(None) => {}
            Err(e) => {
                not_set_right += 1;
                eprintln!("FAILED to put right {}: {e:#}", report.display());
            }
        }
    }
    if set_right_any {
        ledger = crate::ledger::load();
    }
    let judging = Judging { accept_changed_date, on_command_line: true, remember: go.then_some(&reads), listing: true, ledger: Some(&ledger), ..Judging::default() };
    let (mut ready, mut replaced, mut stay) = (Vec::new(), Vec::new(), Vec::new());
    for r in &reports {
        match check_judging(r, judging) {
            Check::Ready(p) if !named.holds(&p.source) && !ledger.vouches_for(&p) => stay.push((p.output.clone(), outside_named(&p.source))),
            Check::Ready(p) if p.zfs && !allow_zfs => stay.push((p.output.clone(), "the original is on ZFS (--allow-zfs replaces there too)".to_string())),
            Check::Ready(p) => ready.push(p),
            Check::Replaced(d) => replaced.push(d),
            Check::No(why) => stay.push((r.clone(), why)),
        }
    }
    // Replaced, with a report that could not be rewritten.
    let (mut failed, mut behind) = (0, 0);

    if back {
        let (can, cannot): (Vec<_>, Vec<_>) = replaced.into_iter().partition(|d| d.can_undo);
        let (can, outside): (Vec<_>, Vec<_>) = can.into_iter().partition(|d| named.holds(&d.source) || ledger.check_replaced(d).is_ok());
        let (can, on_zfs): (Vec<_>, Vec<_>) = can.into_iter().partition(|d| !d.zfs || allow_zfs);
        println!("{} can go back to their place", plural(can.len(), "original", "originals"));
        for d in &can {
            println!("  {}  (AV1 file back to {})", d.source.display(), d.from.display());
        }
        if !on_zfs.is_empty() {
            println!("{} on ZFS (--allow-zfs puts back there too)", plural(on_zfs.len(), "original was", "originals were"));
            for d in &on_zfs {
                println!("  {}", d.source.display());
            }
        }
        if !outside.is_empty() {
            println!("{} outside the folders named here, with no record of them (name their folders too, add --anywhere if you trust these reports, or run `av1opt adopt` on the output folders)", plural(outside.len(), "original is", "originals are"));
            for d in &outside {
                println!("  {}", d.source.display());
            }
        }
        if !cannot.is_empty() {
            println!("{} cannot: no longer in the trash, or the AV1 file changed", plural(cannot.len(), "original", "originals"));
            for d in &cannot {
                println!("  {}", d.source.display());
            }
        }
        if !go {
            if !can.is_empty() {
                println!("\nNothing was changed. Add --apply to put them back.");
            }
            return Ok(());
        }
        let mut done = 0;
        for d in &can {
            // Asked again here: the record may have changed since the list was made.
            let accept = |d: &Replaced| if named.holds(&d.source) { Ok(()) } else { crate::ledger::load().check_replaced(d).map_err(|why| format!("{} ({why})", outside_named(&d.source))) };
            match undo_if(&d.report, allow_zfs, &accept) {
                Ok(_) => done += 1,
                Err(e) => {
                    failed += 1;
                    eprintln!("FAILED {}: {e:#}", d.source.display());
                }
            }
        }
        println!("\n{} back in place", plural(done, "original", "originals"));
    } else {
        let (bytes_in, bytes_out) = ready.iter().fold((0u64, 0u64), |a, p| (a.0 + p.source_bytes, a.1 + p.output_bytes));
        println!("{} can take their original's place", plural(ready.len(), "output", "outputs"));
        for p in &ready {
            let name = p.dest.file_name().unwrap_or_default().to_string_lossy();
            let mut notes = Vec::new();
            if p.copy {
                notes.push("copied from another drive".to_string());
            }
            if p.removed > 0 {
                notes.push(format!("{} left out", plural(p.removed, "section", "sections")));
            }
            let notes = if notes.is_empty() { String::new() } else { format!("  ({})", notes.join(", ")) };
            println!("  {:>9} -> {:>9}  {} -> {name}{notes}", gb(p.source_bytes as f64), gb(p.output_bytes as f64), p.source.display());
        }
        if !ready.is_empty() {
            println!("  total: {} of originals to the trash, {} of AV1 in their place", gb(bytes_in as f64), gb(bytes_out as f64));
        }
        if !stay.is_empty() {
            println!("{} as they are", plural(stay.len(), "stays", "stay"));
            for (path, why) in &stay {
                println!("  {}: {why}", path.display());
            }
        }
        if !replaced.is_empty() {
            println!("{} replaced earlier (--undo lists what can go back)", plural(replaced.len(), "original was", "originals were"));
        }
        if !go {
            if !ready.is_empty() {
                println!("\nNothing was changed. Add --apply to replace {}.", if ready.len() == 1 { "it" } else { "them" });
                let to_read = ready.iter().filter(|p| p.original.whole.is_some()).count();
                if to_read > 0 {
                    println!("With --apply, {} read whole first, to be sure of what goes to the trash; on large files this can take minutes.", plural(to_read, "original is", "originals are"));
                }
            }
            return Ok(());
        }
        let (mut done, mut trashed) = (0, 0u64);
        for p in &ready {
            // Outside the folders named, all of the output has to be what the record says.
            let accept = |p: &Plan, output: &OpenFile| if named.holds(&p.source) { Ok(()) } else { crate::ledger::load().check_ready(p, Some(output)).map_err(|why| format!("{} ({why})", outside_named(&p.source))) };
            match apply_judging(&p.report, allow_zfs, judging, &trash::trash_noting, &accept, before_report) {
                Ok(Done { report_behind: Some(why), source_bytes, .. }) => {
                    (done, behind) = (done + 1, behind + 1);
                    trashed += source_bytes;
                    eprintln!("FAILED {}: {why}", p.source.display());
                }
                Ok(d) => {
                    done += 1;
                    trashed += d.source_bytes;
                    println!("replaced {} by {}", p.source.display(), d.dest.file_name().unwrap_or_default().to_string_lossy());
                    for w in &d.warnings {
                        eprintln!("  warning: {w}");
                    }
                }
                Err(e) => {
                    failed += 1;
                    eprintln!("FAILED {}: {e:#}", p.source.display());
                }
            }
        }
        println!("\n{} replaced; {} of originals are in the trash and take up space until it is emptied", plural(done, "original", "originals"), gb(trashed as f64));
    }
    if failed > 0 {
        let what = if back { "put back" } else { "replaced" };
        bail!("{} not be {what}; the lines starting with FAILED above say why and where each file is now", plural(failed, "original could", "originals could"));
    }
    if behind > 0 {
        bail!("{} not be updated, and still say that nothing was replaced; the lines starting with FAILED above say where each file is and what puts it back", plural(behind, "report could", "reports could"));
    }
    if not_set_right > 0 {
        bail!("{} stopped half-way could not be put right; the lines starting with FAILED above say which and why", plural(not_set_right, "pair", "pairs"));
    }
    Ok(())
}

/// What a report comes to for `av1opt adopt`.
enum Found {
    Ready(Plan),
    Replaced(Replaced),
    OnRecord,
    Refused(String),
}

/// Why an original that went to the trash could not be put back, if it could not.
fn why_it_cannot_come_back(d: &Replaced) -> Option<String> {
    if d.can_undo {
        return None;
    }
    if file_size(&d.dest) != Some(d.output_bytes) {
        return Some("the AV1 file is gone or has changed".into());
    }
    Some(match file_size(&d.trash) {
        None => "the original is no longer in the trash".into(),
        Some(n) if n != d.source_bytes => "the file in the trash is not the original".into(),
        Some(_) if !trash::says_came_from(&d.trash, &d.source) => format!("the trash does not say that {} came from {}", d.trash.display(), d.source.display()),
        Some(_) => "the file in the trash is not the original: it changed after the original went there".into(),
    })
}

fn found_for_adoption(report: &Path, judging: Judging, ledger: &crate::ledger::Ledger, named: &NamedFolders) -> Found {
    use crate::ledger::Standing;
    match std::fs::symlink_metadata(report) {
        Ok(m) if m.file_type().is_symlink() => return Found::Refused("the report is a link".into()),
        Ok(m) if !m.is_file() => return Found::Refused("the report is not a plain file".into()),
        _ => {}
    }
    if let Err(why) = holds_enough_to_adopt(report) {
        return Found::Refused(why);
    }
    match check_judging(report, judging) {
        Check::No(why) => Found::Refused(why),
        Check::Ready(p) if !named.holds(&p.source) => Found::Refused(outside_named_for_adoption(&p.source)),
        Check::Ready(p) => match ledger.adopting(&p) {
            Standing::NotYet => Found::Ready(p),
            Standing::OnRecord => Found::OnRecord,
            Standing::Refused(why) => Found::Refused(why),
        },
        Check::Replaced(d) => match ledger.adopting_replaced(&d) {
            Standing::NotYet => match why_not_adopted_replaced(&d, named) {
                Some(why) => Found::Refused(why),
                None => why_it_cannot_come_back(&d).map_or(Found::Replaced(d), Found::Refused),
            },
            Standing::OnRecord => Found::OnRecord,
            Standing::Refused(why) => Found::Refused(why),
        },
    }
}

/// Whether a report holds enough of its original to take it on record: its date, and a
/// fingerprint or a hash of its contents. Size and date alone another file can have too, and
/// what goes on record is what the page later moves files on the strength of.
fn holds_enough_to_adopt(report: &Path) -> Result<(), String> {
    let r = read_report(report)?;
    let known = Recorded::in_report(&r);
    let missing: Vec<&str> = [
        known.modified.is_none().then_some("the original's date"),
        (known.fingerprint.is_none() && known.whole.is_none()).then_some("a fingerprint or hash of the original's contents"),
    ]
    .into_iter()
    .flatten()
    .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "its report does not hold {}, which av1opt needs to take an output on record (reports written before 2026-11-18 lack them); `av1opt replace` on the output's and the original's folders, with --accept-changed-date, can still replace it",
        missing.join(" or ")
    ))
}

fn outside_named_for_adoption(source: &Path) -> String {
    format!("the original {} is outside the folders named here; name its folder too, or add --anywhere if you trust this report", source.display())
}

/// Why an output that took its original's place is not taken on record, if it is not: it has
/// to be where `replace` would have put it, and its original's place inside the folders named.
/// The report alone says where the original is in the trash and where the AV1 file goes back
/// to, and the page puts files there on the strength of the record.
fn why_not_adopted_replaced(d: &Replaced, named: &NamedFolders) -> Option<String> {
    if d.recorded_from.extension().map(|ext| d.source.with_extension(ext)).as_ref() != Some(&d.dest) {
        return Some(format!("the report says the AV1 file is at {}, which is not the original's place, where `replace` puts it", d.dest.display()));
    }
    if !named.holds(&d.source) {
        return Some(outside_named_for_adoption(&d.source));
    }
    None
}

/// The full check of the file in the trash, through one descriptor: its info says it came from
/// the original's place, and its size, date and contents are those the report holds.
fn trash_holds_the_original(d: &Replaced) -> Result<()> {
    let file = OpenFile::open(&d.trash, d.source_bytes)?;
    if !trash::says_came_from(&d.trash, &d.source) {
        bail!("the trash does not say that {} came from {}", d.trash.display(), d.source.display());
    }
    let then = d.original.modified.context("the report does not hold the original's date")?;
    let now = file.file.metadata().ok().as_ref().and_then(crate::source::modified_seconds);
    if !now.is_some_and(|now| crate::source::same_time(then, now)) {
        bail!("the file in the trash was last changed at another time than the report says of the original");
    }
    if !trash::is_open_file_recorded(&file.file, &d.original) {
        bail!("the file in the trash is not the original: its contents are not what the report holds of it");
    }
    if !file.is_at(&d.trash) || !file.untouched() {
        bail!("the file in the trash changed while it was being read");
    }
    Ok(())
}

/// Reads the output once, through the descriptor it stays compared with.
fn hash_of_output(path: &Path, bytes: u64) -> Result<String> {
    let output = OpenFile::open(path, bytes)?;
    let hash = output.sha256().with_context(|| format!("cannot read {}", path.display()))?;
    if !output.is_at(path) || !output.untouched() {
        bail!("{} changed while it was being read", path.display());
    }
    Ok(hash)
}

/// Looks at one report again, with nothing else acting on it, and takes it on record. False if
/// it turned out to be there already.
fn adopt_one(report: &Path, reads: &WholeReads, held: &mut crate::ledger::Held, named: &NamedFolders) -> Result<bool> {
    use crate::ledger::{AdoptedReplacement, Adoption};
    let _lock = lock(report)?;
    let judging = Judging { remember: Some(reads), acting: true, ..Judging::default() };
    match found_for_adoption(report, judging, held.current(), named) {
        Found::Refused(why) => bail!("{why}"),
        Found::OnRecord => return Ok(false),
        Found::Ready(p) => {
            let output_sha256 = hash_of_output(&p.output, p.output_bytes)?;
            let stamp = p.source_stamp.context(CHANGED_WHILE_CHECKED)?;
            if !std::fs::symlink_metadata(&p.source).is_ok_and(|m| Stamp::of(&m) == stamp) {
                bail!("{CHANGED_WHILE_CHECKED}");
            }
            held.note_adopted(Adoption { output: &p.output, report, source: &p.source, source_bytes: p.source_bytes, original: &p.original, output_bytes: p.output_bytes, output_sha256, replaced: None })?;
        }
        Found::Replaced(d) => {
            trash_holds_the_original(&d)?;
            let output_sha256 = hash_of_output(&d.dest, d.output_bytes)?;
            // The report says how the original was when it was encoded, which is what it has to
            // match if the replacement is undone and the output offered again.
            let encoded = Recorded::in_report(&read_report(report).map_err(|why| anyhow!(why))?);
            let replaced = Some(AdoptedReplacement { trash: &d.trash, from: &d.recorded_from, known: &d.original });
            held.note_adopted(Adoption { output: &d.dest, report, source: &d.source, source_bytes: d.source_bytes, original: &encoded, output_bytes: d.output_bytes, output_sha256, replaced })?;
        }
    }
    crate::signals::stop_point("adopt:noted");
    Ok(true)
}

/// The `adopt` command: lists which outputs under `paths` can go on av1opt's record, and with `go`
/// writes them there. Nothing but the record is ever written, and without `go` nothing at all.
/// A report is followed to an original inside the folders in `paths`, or anywhere with
/// `anywhere`.
pub fn adopt(paths: &[PathBuf], go: bool, anywhere: bool) -> Result<()> {
    println!(
        "Naming a folder here says that you trust the reports in it: what a report says of the output beside it and of its original goes on av1opt's private record, and the page then replaces that original, and puts it back, on the strength of it. A report is followed only to an original inside the folders named here{}, and is taken only if it holds the original's date and a fingerprint or hash of it that the original (or, once replaced, the file in the trash) still matches.",
        if anywhere { " (anywhere, as --anywhere asks)" } else { "" }
    );
    let reports = reports_found(paths, &|dirs| report_names(dirs, &|e| !e.file_type().is_dir()));
    if reports.is_empty() {
        bail!("no av1opt reports (*.av1opt.json) found there");
    }
    if go {
        crate::ledger::make_ready().context("av1opt's record of its own encodes cannot be used, so nothing can be taken on it")?;
    }
    let mut held = crate::ledger::Held::load();
    if let Some(why) = held.current().problem() {
        let hint = if go { "" } else { "; if its folder is only open to other users, --apply closes it first" };
        bail!("av1opt's record of its own encodes cannot be used ({why}), so nothing can be taken on it{hint}");
    }
    let named = NamedFolders::of(paths, anywhere);
    let reads = WholeReads::default();
    let judging = Judging { remember: go.then_some(&reads), listing: true, ..Judging::default() };
    let (mut ready, mut replaced, mut known, mut refused) = (Vec::new(), Vec::new(), 0, Vec::new());
    for report in &reports {
        match found_for_adoption(report, judging, held.current(), &named) {
            Found::Ready(p) => ready.push(p),
            Found::Replaced(d) => replaced.push(d),
            Found::OnRecord => known += 1,
            Found::Refused(why) => refused.push((report, why)),
        }
    }
    println!("{} can go on record", plural(ready.len(), "output", "outputs"));
    for p in &ready {
        println!("  {}  (made from {})", p.output.display(), p.source.display());
    }
    println!("{} that already took the original's place can go on record, so that the page can put the original back", plural(replaced.len(), "output", "outputs"));
    for d in &replaced {
        println!("  {}  (original {}, in the trash at {})", d.dest.display(), d.source.display(), d.trash.display());
    }
    println!("{} on record already", plural(known, "output is", "outputs are"));
    println!("{} refused", plural(refused.len(), "report", "reports"));
    for (report, why) in &refused {
        println!("  {}: {why}", report.display());
    }
    let wanted: Vec<&Path> = ready.iter().map(|p| p.report.as_path()).chain(replaced.iter().map(|d| d.report.as_path())).collect();
    if !go {
        if !wanted.is_empty() {
            println!("\nNothing was recorded. Add --apply to record {}; each output is then read whole, and so is each original whose report holds a hash, which can take minutes on large files.", if wanted.len() == 1 { "it" } else { "them" });
        }
        return Ok(());
    }
    let (mut done, mut failed) = (0, 0);
    for report in wanted {
        match adopt_one(report, &reads, &mut held, &named) {
            Ok(added) => {
                done += usize::from(added);
                if added {
                    println!("recorded {}", report.display());
                }
            }
            Err(e) => {
                failed += 1;
                eprintln!("FAILED {}: {e:#}", report.display());
            }
        }
    }
    println!("\n{} taken on record", plural(done, "output", "outputs"));
    if failed > 0 {
        bail!("{} not be taken on record; the lines starting with FAILED above say why", plural(failed, "output could", "outputs could"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A library folder, an output folder and a trash of its own under one scratch folder.
    struct Fix {
        dir: PathBuf,
    }

    impl Fix {
        fn new(name: &str) -> Fix {
            let dir = std::env::temp_dir().join(format!("av1opt-replace-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("lib")).unwrap();
            std::fs::create_dir_all(dir.join("out")).unwrap();
            trash::TEST_HOME_TRASH.with(|h| *h.borrow_mut() = Some(dir.join("Trash")));
            Fix { dir }
        }

        /// An original of `source_len` bytes, its output of `output_len` bytes and the report.
        fn encode(&self, source: &str, output: &str, source_len: usize, output_len: usize) -> PathBuf {
            let (s, o) = (self.dir.join("lib").join(source), self.dir.join("out").join(output));
            std::fs::write(&s, vec![b's'; source_len]).unwrap();
            std::fs::write(&o, vec![b'o'; output_len]).unwrap();
            let report = o.with_extension("av1opt.json");
            // What a report of today holds of its original but the whole hash, which takes a
            // test of its own to be about.
            let (_, known) = crate::source::Recorded::of(&s).unwrap();
            let r = json!({ "format": 1, "input": s, "output": o, "source_bytes": source_len, "source_mtime": known.modified, "source_fingerprint": known.fingerprint, "output_bytes": output_len, "crf": 30.0 });
            std::fs::write(&report, serde_json::to_vec_pretty(&r).unwrap()).unwrap();
            report
        }

        fn apply(&self, report: &Path) -> Result<Done> {
            let home = self.dir.join("Trash");
            apply_with(report, true, &|p, n| trash::trash_with_noting(p, Some(&home), 1000, "2026-11-10T22:15:03".into(), n))
        }

        fn lib(&self, name: &str) -> PathBuf {
            self.dir.join("lib").join(name)
        }

        fn out(&self, name: &str) -> PathBuf {
            self.dir.join("out").join(name)
        }
    }

    impl Drop for Fix {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn a_report_is_followed_only_into_the_folders_named() {
        let f = Fix::new("named");
        let report = f.encode("a.mp4", "a.mkv", 1000, 400);
        let source = f.lib("a.mp4");
        let out = f.dir.join("out");
        let lib = f.dir.join("lib");
        assert!(!NamedFolders::of(std::slice::from_ref(&out), false).holds(&source));
        // A file named stands for its folder.
        assert!(!NamedFolders::of(std::slice::from_ref(&report), false).holds(&source));
        assert!(NamedFolders::of(&[out.clone(), lib.clone()], false).holds(&source));
        assert!(NamedFolders::of(std::slice::from_ref(&f.dir), false).holds(&source), "a folder holds what is below it");
        assert!(NamedFolders::of(std::slice::from_ref(&out), true).holds(&source));
        // Not by a name that only starts the same, nor by a way through `..` or a link.
        let twin = f.dir.join("lib2");
        std::fs::create_dir(&twin).unwrap();
        assert!(!NamedFolders::of(std::slice::from_ref(&twin), false).holds(&source));
        assert!(!NamedFolders::of(std::slice::from_ref(&out), false).holds(&out.join("../lib/a.mp4")));
        std::os::unix::fs::symlink(&lib, out.join("way")).unwrap();
        assert!(!NamedFolders::of(std::slice::from_ref(&out), false).holds(&out.join("way/a.mp4")));
        // An original already in the trash is still judged by the folder it came from.
        std::fs::remove_file(&source).unwrap();
        assert!(NamedFolders::of(std::slice::from_ref(&lib), false).holds(&source));
    }

    /// A file of the output's size that is not the output.
    fn swap_for_another_file(path: &Path) -> PathBuf {
        let kept = path.with_extension("real");
        std::fs::rename(path, &kept).unwrap();
        std::fs::write(path, vec![b'e'; 400]).unwrap();
        kept
    }

    fn swap_for_a_link(path: &Path, to: &Path) -> PathBuf {
        let kept = path.with_extension("real");
        std::fs::rename(path, &kept).unwrap();
        std::os::unix::fs::symlink(to, path).unwrap();
        kept
    }

    fn mode(p: &Path) -> u32 {
        std::fs::symlink_metadata(p).unwrap().mode() & 0o7777
    }

    #[test]
    fn an_output_swapped_after_it_was_checked_is_not_moved() {
        // While `accept` reads it, and so before anything has moved.
        let f = Fix::new("swap-checked");
        let report = f.encode("a.mp4", "a.mkv", 1000, 400);
        let output = f.out("a.mkv");
        let hash_seen = std::cell::RefCell::new(String::new());
        let err = apply_judging(&report, true, Judging::default(), &trash::trash_noting, &|_, opened| {
            *hash_seen.borrow_mut() = opened.sha256().unwrap();
            swap_for_another_file(&output);
            Ok(())
        }, &|| {})
        .unwrap_err();
        assert!(err.to_string().contains("replaced by another file"), "{err}");
        assert_eq!(*hash_seen.borrow(), crate::ledger::sha256_of_file(&f.out("a.real")).unwrap(), "the hash is of the file opened");
        assert_eq!(std::fs::read(&output).unwrap(), vec![b'e'; 400]);
        assert_eq!(std::fs::read(f.lib("a.mp4")).unwrap(), vec![b's'; 1000]);
        assert!(!f.lib("a.mkv").exists() && !f.dir.join("Trash/files/a.mp4").exists());
    }

    #[test]
    fn an_output_rewritten_after_it_was_checked_is_not_moved() {
        let f = Fix::new("rewrite-checked");
        let report = f.encode("a.mp4", "a.mkv", 1000, 400);
        let output = f.out("a.mkv");
        // File times move with the kernel's clock tick: a write in the tick that made the file
        // would leave no mark to go by.
        std::thread::sleep(std::time::Duration::from_millis(30));
        let err = apply_judging(&report, true, Judging::default(), &trash::trash_noting, &|_, _| {
            std::fs::write(&output, vec![b'e'; 400]).unwrap();
            Ok(())
        }, &|| {})
        .unwrap_err();
        assert!(err.to_string().contains("changed or replaced"), "{err}");
        assert!(f.lib("a.mp4").exists() && !f.lib("a.mkv").exists());
    }

    #[test]
    fn what_was_put_in_the_outputs_place_at_the_last_moment_goes_back() {
        let home = |f: &Fix| f.dir.join("Trash");
        // Another file, between the last look and the move.
        let f = Fix::new("swap-file");
        let report = f.encode("a.mp4", "a.mkv", 1000, 400);
        let (output, dest) = (f.out("a.mkv"), f.lib("a.mkv"));
        let err = apply_with(&report, true, &|p, n| {
            let t = trash::trash_with_noting(p, Some(&home(&f)), 1000, "2026-11-10T22:15:03".into(), n)?;
            swap_for_another_file(&output);
            Ok(t)
        })
        .unwrap_err();
        assert!(err.to_string().contains("replaced by another file") && err.to_string().ends_with("the original is back in its place"), "{err}");
        assert_eq!(std::fs::read(f.lib("a.mp4")).unwrap(), vec![b's'; 1000]);
        assert_eq!(std::fs::read(&output).unwrap(), vec![b'e'; 400]);
        assert_eq!(std::fs::read(f.out("a.real")).unwrap(), vec![b'o'; 400]);
        assert!(!dest.exists());
        assert_eq!(std::fs::read_dir(home(&f).join("files")).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(home(&f).join("info")).unwrap().count(), 0);

        // A link, which would also have passed the original's permissions on to what it leads to.
        let f = Fix::new("swap-link");
        let report = f.encode("a.mp4", "a.mkv", 1000, 400);
        let (output, dest, victim) = (f.out("a.mkv"), f.lib("a.mkv"), f.dir.join("victim"));
        std::fs::write(&victim, b"private").unwrap();
        std::fs::set_permissions(&victim, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
        std::fs::set_permissions(f.lib("a.mp4"), std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
        let err = apply_with(&report, true, &|p, n| {
            let t = trash::trash_with_noting(p, Some(&home(&f)), 1000, "2026-11-10T22:15:03".into(), n)?;
            swap_for_a_link(&output, &victim);
            Ok(t)
        })
        .unwrap_err();
        assert!(err.to_string().ends_with("the original is back in its place"), "{err}");
        assert!(std::fs::symlink_metadata(&output).unwrap().file_type().is_symlink());
        assert!(f.lib("a.mp4").exists() && std::fs::symlink_metadata(&dest).is_err());
        assert_eq!((mode(&victim), std::fs::read(&victim).unwrap()), (0o600, b"private".to_vec()));
    }

    #[test]
    fn a_link_in_the_outputs_place_before_the_check_is_refused() {
        let f = Fix::new("link-first");
        let report = f.encode("a.mp4", "a.mkv", 1000, 400);
        let target = f.dir.join("target.mkv");
        std::fs::write(&target, vec![b't'; 400]).unwrap();
        std::fs::remove_file(f.out("a.mkv")).unwrap();
        std::os::unix::fs::symlink(&target, f.out("a.mkv")).unwrap();
        assert!(matches!(check(&report), Check::No(_)));
        assert!(OpenFile::open(&f.out("a.mkv"), 400).err().unwrap().to_string().ends_with("is a link"));
        assert!(f.apply(&report).is_err() && f.lib("a.mp4").exists());
    }

    #[test]
    fn landing_refuses_what_is_not_the_file_opened_and_leaves_it_where_it_was() {
        let f = Fix::new("land");
        let (from, to) = (f.out("a.mkv"), f.lib("a.mkv"));
        std::fs::write(&from, vec![b'o'; 400]).unwrap();
        let opened = OpenFile::open(&from, 400).unwrap();
        swap_for_another_file(&from);
        assert!(land(&from, &to, &opened).is_err());
        assert_eq!(std::fs::read(&from).unwrap(), vec![b'e'; 400]);
        assert!(!to.exists());
        // With the name taken again, it still leaves the place it had no business in.
        std::fs::remove_file(&from).unwrap();
        std::fs::rename(f.out("a.real"), &from).unwrap();
        let opened = OpenFile::open(&from, 400).unwrap();
        std::fs::rename(&from, f.dir.join("kept")).unwrap();
        std::fs::write(&to, b"not it").unwrap();
        std::fs::remove_file(&to).unwrap();
        std::fs::write(f.out("b.mkv"), b"not it").unwrap();
        trash::rename_new(&f.out("b.mkv"), &from).unwrap();
        assert!(land(&from, &to, &opened).is_err());
        assert!(!to.exists() && from.exists());
    }

    #[test]
    fn an_output_with_a_second_name_is_neither_moved_nor_changed() {
        let f = Fix::new("hard-link");
        let report = f.encode("a.mp4", "a.mkv", 1000, 400);
        // A file of somebody's, reached through the output's name.
        let elsewhere = f.dir.join("elsewhere.bin");
        std::fs::rename(f.out("a.mkv"), &elsewhere).unwrap();
        std::fs::set_permissions(&elsewhere, std::os::unix::fs::PermissionsExt::from_mode(0o604)).unwrap();
        std::fs::hard_link(&elsewhere, f.out("a.mkv")).unwrap();
        std::fs::set_permissions(f.lib("a.mp4"), std::os::unix::fs::PermissionsExt::from_mode(0o640)).unwrap();
        let err = f.apply(&report).unwrap_err();
        assert!(err.to_string().contains("other names on the drive too"), "{err}");
        assert_eq!(mode(&elsewhere), 0o604);
        assert!(f.lib("a.mp4").exists() && f.out("a.mkv").exists() && !f.lib("a.mkv").exists());
        assert!(!f.dir.join("Trash/files/a.mp4").exists());
    }

    #[test]
    fn landing_takes_back_only_what_it_moved() {
        let f = Fix::new("land-planted");
        let (from, to) = (f.out("a.mkv"), f.lib("a.mkv"));
        std::fs::write(&from, vec![b'o'; 400]).unwrap();
        let opened = OpenFile::open(&from, 400).unwrap();
        swap_for_another_file(&from);
        // Once the swapped file has arrived, somebody takes it on and puts a file of theirs there.
        let planted = f.dir.join("planted");
        let err = land_watched(&from, &to, &opened, &|| {
            std::fs::rename(&to, &planted).unwrap();
            std::fs::write(&to, b"theirs").unwrap();
        })
        .unwrap_err();
        assert!(err.to_string().contains("did not put there"), "{err}");
        assert_eq!(std::fs::read(&to).unwrap(), b"theirs");
        assert!(!from.exists(), "a file the move did not put there was moved");
        assert_eq!(std::fs::read(&planted).unwrap(), vec![b'e'; 400]);
    }

    #[test]
    fn the_permissions_go_to_the_file_that_was_moved() {
        let f = Fix::new("modes");
        let report = f.encode("a.mp4", "a.mkv", 1000, 400);
        std::fs::set_permissions(f.lib("a.mp4"), std::os::unix::fs::PermissionsExt::from_mode(0o640)).unwrap();
        let done = f.apply(&report).unwrap();
        assert_eq!(mode(&done.dest), 0o640);
    }

    #[test]
    fn what_is_swapped_during_an_undo_stays_out_and_both_files_stay_as_they_were() {
        let run = |name: &str, swap: &dyn Fn(&Fix, &Path)| {
            let f = Fix::new(name);
            let report = f.encode("a.mp4", "a.mkv", 1000, 400);
            let done = f.apply(&report).unwrap();
            let err = undo_watched(&report, true, &|_| Ok(()), &|| swap(&f, &done.trash), &|| {}).unwrap_err();
            (f, report, done, err.to_string())
        };
        // The AV1 file, for another of its size: nothing has moved.
        let (f, report, done, why) = run("undo-av1", &|f, _| {
            swap_for_another_file(&f.lib("a.mkv"));
        });
        assert!(why.contains("replaced by another file"), "{why}");
        assert_eq!(std::fs::read(f.lib("a.mkv")).unwrap(), vec![b'e'; 400]);
        assert!(!f.out("a.mkv").exists() && !f.lib("a.mp4").exists() && done.trash.exists());
        assert!(matches!(check(&report), Check::Replaced(_)));

        // For a link.
        let (f, _, done, why) = run("undo-link", &|f, _| {
            swap_for_a_link(&f.lib("a.mkv"), &f.dir.join("elsewhere"));
        });
        assert!(why.contains("replaced by another file"), "{why}");
        assert!(std::fs::symlink_metadata(f.lib("a.mkv")).unwrap().file_type().is_symlink());
        assert!(!f.out("a.mkv").exists() && !f.lib("a.mp4").exists() && done.trash.exists());

        // The file in the trash: the AV1 file has not moved either.
        let (f, report, done, why) = run("undo-trash", &|_, trashed| {
            let kept = trashed.with_extension("real");
            std::fs::rename(trashed, kept).unwrap();
            std::fs::write(trashed, vec![b'x'; 1000]).unwrap();
        });
        assert!(why.contains("the file in the trash was changed or replaced"), "{why}");
        assert_eq!(std::fs::read(f.lib("a.mkv")).unwrap(), vec![b'o'; 400]);
        assert_eq!(std::fs::read(&done.trash).unwrap(), vec![b'x'; 1000]);
        assert!(!f.lib("a.mp4").exists() && !f.out("a.mkv").exists());
        assert!(matches!(check(&report), Check::Replaced(_)));
    }

    #[test]
    fn an_original_that_cannot_come_back_leaves_the_av1_file_where_it_was() {
        let f = Fix::new("undo-back");
        let report = f.encode("a.mp4", "a.mkv", 1000, 400);
        let done = f.apply(&report).unwrap();
        // Something takes the original's place once everything has been looked at.
        let err = undo_watched(&report, true, &|_| Ok(()), &|| std::fs::write(f.lib("a.mp4"), b"newcomer").unwrap(), &|| {}).unwrap_err();
        assert!(format!("{err:#}").contains("still in the trash"), "{err:#}");
        assert_eq!(std::fs::read(f.lib("a.mkv")).unwrap(), vec![b'o'; 400]);
        assert!(!f.out("a.mkv").exists() && done.trash.exists());
        assert_eq!(std::fs::read(f.lib("a.mp4")).unwrap(), b"newcomer");
        // With the place free again, it simply works.
        std::fs::remove_file(f.lib("a.mp4")).unwrap();
        undo(&report, false).unwrap();
        assert!(f.lib("a.mp4").exists() && f.out("a.mkv").exists());
    }

    #[test]
    fn a_file_put_in_the_trash_entrys_place_is_not_restored() {
        let f = Fix::new("restore-if");
        let (bin, to) = (f.dir.join("Trash"), f.lib("a.mp4"));
        std::fs::write(&to, vec![b's'; 1000]).unwrap();
        let trashed = trash::trash_with(&to, Some(&bin), 1000, "2026-11-10T22:15:03".into()).unwrap();
        assert!(trash::restore_if(&trashed, &to, &|_| false).is_err());
        assert!(trashed.exists() && !to.exists());
        assert!(bin.join("info/a.mp4.trashinfo").exists(), "the entry is whole again");
        trash::restore_if(&trashed, &to, &|_| true).unwrap();
        assert!(to.exists() && !bin.join("info/a.mp4.trashinfo").exists());
    }

    /// Gives the report what an encode of today writes about the original as it is now.
    fn record_whole_hash(report: &Path, source: &Path) -> u64 {
        use std::sync::atomic::{AtomicBool, AtomicU64};
        let size = std::fs::metadata(source).unwrap().len();
        let hash = crate::files::whole_digest(source, size, &AtomicU64::new(0), &AtomicBool::new(false)).unwrap();
        let time = crate::source::modified_seconds(&std::fs::metadata(source).unwrap()).unwrap();
        let mut r: Value = serde_json::from_slice(&std::fs::read(report).unwrap()).unwrap();
        r["source_hash"] = json!(format!("{hash:016x}"));
        r["source_mtime"] = json!(time);
        std::fs::write(report, serde_json::to_vec_pretty(&r).unwrap()).unwrap();
        time
    }

    fn set_time(path: &Path, seconds: u64) {
        let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds);
        File::options().write(true).open(path).unwrap().set_modified(time).unwrap();
    }

    #[test]
    fn an_original_rewritten_with_its_date_set_back_does_not_go_to_the_trash() {
        let f = Fix::new("date-back");
        let report = f.encode("a.mp4", "a.mkv", 1000, 400);
        let source = f.lib("a.mp4");
        let time = record_whole_hash(&report, &source);
        std::fs::write(&source, vec![b'x'; 1000]).unwrap();
        set_time(&source, time);
        // A list and a dry run go by the date, as they may.
        assert!(matches!(check(&report), Check::Ready(_)));
        let err = f.apply(&report).unwrap_err();
        assert!(err.to_string().contains("the original changed since the encode"), "{err}");
        assert_eq!(std::fs::read(&source).unwrap(), vec![b'x'; 1000]);
        assert!(f.out("a.mkv").exists() && !f.lib("a.mkv").exists());
    }

    #[test]
    fn an_original_written_to_after_it_was_read_does_not_go_to_the_trash() {
        // After the read, with everything still to come.
        let f = Fix::new("written-early");
        let report = f.encode("a.mp4", "a.mkv", 1000, 400);
        let source = f.lib("a.mp4");
        record_whole_hash(&report, &source);
        std::thread::sleep(std::time::Duration::from_millis(30));
        let err = apply_judging(&report, true, Judging::default(), &trash::trash_noting, &|_, _| {
            std::os::unix::fs::FileExt::write_all_at(&File::options().write(true).open(&source).unwrap(), b"x", 999).unwrap();
            Ok(())
        }, &|| {})
        .unwrap_err();
        assert_eq!(err.to_string(), "the original changed while it was being checked, so nothing was changed");
        assert!(source.exists() && f.out("a.mkv").exists() && !f.lib("a.mkv").exists());

        // In the last moment before it moves: it is back, and so is everything else.
        let f = Fix::new("written-late");
        let report = f.encode("a.mp4", "a.mkv", 1000, 400);
        let source = f.lib("a.mp4");
        record_whole_hash(&report, &source);
        std::thread::sleep(std::time::Duration::from_millis(30));
        let home = f.dir.join("Trash");
        let err = apply_with(&report, true, &|p, n| {
            std::os::unix::fs::FileExt::write_all_at(&File::options().write(true).open(p).unwrap(), b"x", 999).unwrap();
            trash::trash_with_noting(p, Some(&home), 1000, "2026-11-10T22:15:03".into(), n)
        })
        .unwrap_err();
        assert_eq!(err.to_string(), "the original changed while it was being checked; it is back in its place");
        assert_eq!(std::fs::read(&source).unwrap().last(), Some(&b'x'));
        assert!(f.out("a.mkv").exists() && !f.lib("a.mkv").exists());
        assert_eq!(std::fs::read_dir(home.join("files")).unwrap().count(), 0);
    }

    #[test]
    fn one_apply_reads_each_original_once() {
        let home = |f: &Fix| f.dir.join("Trash");
        let run = |f: &Fix, report: &Path, reads: &WholeReads| {
            let judging = Judging { remember: Some(reads), ..Judging::default() };
            assert!(matches!(check_judging(report, judging), Check::Ready(_)), "listed first, as the command does");
            apply_judging(report, true, judging, &|p, n| trash::trash_with_noting(p, Some(&home(f)), 1000, "2026-11-10T22:15:03".into(), n), &|_, _| Ok(()), &|| {})
        };
        // Its date as recorded: the list leaves the reading to the replacement.
        let f = Fix::new("once-same-date");
        let report = f.encode("a.mp4", "a.mkv", 1000, 400);
        record_whole_hash(&report, &f.lib("a.mp4"));
        let reads = WholeReads::default();
        run(&f, &report, &reads).unwrap();
        assert_eq!(reads.reads_made(), 1);

        // Another date: the list has to read it, and the replacement goes by that.
        let f = Fix::new("once-other-date");
        let report = f.encode("a.mp4", "a.mkv", 1000, 400);
        let time = record_whole_hash(&report, &f.lib("a.mp4"));
        set_time(&f.lib("a.mp4"), time - 1000);
        let reads = WholeReads::default();
        run(&f, &report, &reads).unwrap();
        assert_eq!(reads.reads_made(), 1);

        // A file touched between the two is read again.
        let f = Fix::new("once-touched");
        let report = f.encode("a.mp4", "a.mkv", 1000, 400);
        let time = record_whole_hash(&report, &f.lib("a.mp4"));
        set_time(&f.lib("a.mp4"), time - 1000);
        let reads = WholeReads::default();
        let judging = Judging { remember: Some(&reads), ..Judging::default() };
        assert!(matches!(check_judging(&report, judging), Check::Ready(_)));
        set_time(&f.lib("a.mp4"), time - 2000);
        apply_judging(&report, true, judging, &|p, n| trash::trash_with_noting(p, Some(&home(&f)), 1000, "2026-11-10T22:15:03".into(), n), &|_, _| Ok(()), &|| {}).unwrap();
        assert_eq!(reads.reads_made(), 2);
    }

    #[test]
    fn what_is_refused_at_the_last_moment_moves_nothing() {
        let f = Fix::new("refused");
        let report = f.encode("a.mp4", "a.mkv", 1000, 400);
        let err = apply_judging(&report, true, Judging::default(), &trash::trash_noting, &|_, _| Err("not this one".into()), &|| {}).unwrap_err();
        assert_eq!(err.to_string(), "not this one");
        assert!(f.lib("a.mp4").exists() && f.out("a.mkv").exists());
        f.apply(&report).unwrap();
        let err = undo_if(&report, true, &|_| Err("not this one".into())).unwrap_err();
        assert_eq!(err.to_string(), "not this one");
        assert!(!f.lib("a.mp4").exists() && f.lib("a.mkv").exists());
    }

    #[test]
    fn a_report_that_is_a_pipe_or_too_large_is_refused_without_waiting() {
        let f = Fix::new("pipe");
        let pipe = f.out("a.av1opt.json");
        let name = std::ffi::CString::new(pipe.to_str().unwrap()).unwrap();
        // SAFETY: `name` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o644) }, 0);
        assert!(no(&pipe).starts_with("no report"), "{}", no(&pipe));
        let huge = f.out("b.av1opt.json");
        std::fs::File::create(&huge).unwrap().set_len(REPORT_LIMIT + 1).unwrap();
        assert!(no(&huge).contains("larger than"), "{}", no(&huge));
    }

    fn no(report: &Path) -> String {
        match check(report) {
            Check::No(why) => why,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn an_original_rewritten_at_its_size_is_not_replaced_and_one_only_touched_is() {
        let f = Fix::new("rewritten");
        let report = f.encode("a.mp4", "a.mkv", 1000, 400);
        let source = f.lib("a.mp4");
        let (_, then) = crate::source::Recorded::of(&source).unwrap();
        let hash = crate::source::WholeHash::start(&source, 1000).finish(&|| false).unwrap();
        let rewrite_report = |extra: Value| {
            let mut r: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
            for key in ["source_mtime", "source_fingerprint", "source_hash"] {
                r.as_object_mut().unwrap().remove(key);
                if let Some(value) = extra.get(key) {
                    r[key] = value.clone();
                }
            }
            std::fs::write(&report, r.to_string()).unwrap();
        };
        let set_time = |seconds: u64| std::fs::File::options().write(true).open(&source).unwrap().set_modified(UNIX_EPOCH + std::time::Duration::from_secs(seconds)).unwrap();
        let accepting = Judging { accept_changed_date: true, on_command_line: true, ..Judging::default() };
        let command_line = Judging { on_command_line: true, ..Judging::default() };
        rewrite_report(json!({ "source_mtime": then.modified, "source_fingerprint": then.fingerprint, "source_hash": hash }));
        assert!(matches!(check(&report), Check::Ready(_)));

        // Only the date moved, and the hash of the whole file shows it is the one encoded.
        set_time(1_500_000_000);
        assert!(matches!(check(&report), Check::Ready(_)));

        // Without that hash nothing shows it: the samples match, and what was written may lie
        // between them. Refused, with what can be done. The way out by a flag is for the
        // command line, which is the only place that has it.
        rewrite_report(json!({ "source_mtime": then.modified, "source_fingerprint": then.fingerprint }));
        let why = no(&report);
        assert!(why.contains("cannot tell") && why.contains("only samples") && why.contains(&format!("delete {}", f.out("a.mkv").display())), "{why}");
        assert!(!why.contains("--accept-changed-date"), "{why}");
        let Check::No(why) = check_judging(&report, command_line) else { panic!() };
        assert!(why.contains("--accept-changed-date"), "{why}");
        assert!(matches!(check_judging(&report, accepting), Check::Ready(_)));
        // The same for a report that holds the time alone, and says which it is.
        rewrite_report(json!({ "source_mtime": then.modified }));
        let why = no(&report);
        assert!(why.contains("cannot tell") && why.contains("from an earlier version"), "{why}");
        assert!(matches!(check_judging(&report, accepting), Check::Ready(_)));
        rewrite_report(json!({ "source_mtime": then.modified, "source_fingerprint": null }));
        assert!(no(&report).contains("could not be read when its encode began"));
        // Nor do the size and the date alone, though the date is the same: another file can
        // have both. Nor the size alone. Only the command line, asked to, takes them.
        set_time(then.modified.unwrap());
        for holds in [json!({ "source_mtime": then.modified }), json!({})] {
            rewrite_report(holds);
            let why = no(&report);
            assert!(why.contains("holds only the original's size") && why.contains("cannot tell") && !why.contains("--accept-changed-date"), "{why}");
            let Check::No(why) = check_judging(&report, command_line) else { panic!() };
            assert!(why.contains("add --accept-changed-date"), "{why}");
            assert!(matches!(check_judging(&report, accepting), Check::Ready(_)));
            assert!(f.apply(&report).is_err() && source.exists());
        }
        set_time(1_500_000_000);

        // The download finished into its allocated space: same size, other contents. No flag
        // gets past that.
        std::fs::write(&source, vec![b't'; 1000]).unwrap();
        set_time(1_500_000_500);
        rewrite_report(json!({ "source_mtime": then.modified, "source_fingerprint": then.fingerprint, "source_hash": hash }));
        assert_eq!(no(&report), "the original changed since the encode");
        assert!(matches!(check_judging(&report, accepting), Check::No(_)));
        // A list shows what that look found. It reads no whole file itself: at a time nobody has
        // looked at, the file stays ready until it comes to replacing.
        assert!(matches!(check_for_list(&report), Check::No(_)));
        set_time(1_500_000_600);
        assert!(matches!(check_for_list(&report), Check::Ready(_)));
        assert!(f.apply(&report).is_err());
        assert!(source.exists());
    }

    #[test]
    fn undo_goes_by_what_is_on_record_of_the_file_that_went_to_the_trash() {
        let f = Fix::new("trash-age");
        let report = f.encode("a.mp4", "a.mkv", 1000, 400);
        let at = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        let later = at + std::time::Duration::from_secs(60);
        let set_time = |p: &Path, t| std::fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
        set_time(&f.lib("a.mp4"), at);
        let (_, then) = crate::source::Recorded::of(&f.lib("a.mp4")).unwrap();
        // The original was touched after the encode, and is replaced all the same.
        let mut r: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
        r["source_mtime"] = json!(1_500_000_000u64);
        r["source_fingerprint"] = json!(then.fingerprint);
        r["source_hash"] = json!(crate::source::WholeHash::start(&f.lib("a.mp4"), 1000).finish(&|| false).unwrap());
        std::fs::write(&report, serde_json::to_vec(&r).unwrap()).unwrap();
        let done = f.apply(&report).unwrap();
        let r: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
        assert_eq!(r["replaced"]["source_mtime"], 1_600_000_000u64);
        let can_undo = || matches!(check(&report), Check::Replaced(d) if d.can_undo);
        let write_report = |r: &Value| std::fs::write(&report, serde_json::to_vec(r).unwrap()).unwrap();
        assert!(can_undo());

        // Another file of the same size has taken the name in the trash.
        std::fs::write(&done.trash, vec![b'v'; 1000]).unwrap();
        set_time(&done.trash, later);
        assert!(!can_undo());
        assert!(undo(&report, true).unwrap_err().to_string().starts_with("the file in the trash is not the original"));
        assert!(!f.lib("a.mp4").exists());

        // A report from before the time was written down goes by the one from the encode, and
        // by the fingerprint where that differs.
        let mut old = r.clone();
        old["replaced"].as_object_mut().unwrap().remove("source_mtime");
        write_report(&old);
        assert!(!can_undo());
        // The time on record being that of the file there settles nothing: its contents are
        // read where a hash of them is on record, and only its samples where those are.
        old["source_mtime"] = json!(1_600_000_060u64);
        write_report(&old);
        assert!(!can_undo(), "the same time, and another file");
        let mut samples_only = old.clone();
        samples_only.as_object_mut().unwrap().remove("source_hash");
        write_report(&samples_only);
        assert!(!can_undo(), "the same time, and other samples");
        // With neither on record, a date that differs is the one thing that can tell.
        samples_only.as_object_mut().unwrap().remove("source_fingerprint");
        write_report(&samples_only);
        assert!(can_undo(), "the same size and time, and nothing else to go by");
        samples_only["source_mtime"] = json!(1_600_000_000u64);
        write_report(&samples_only);
        assert!(!can_undo(), "another time and nothing to settle it");

        // The original's contents back under another time: still the original.
        std::fs::write(&done.trash, vec![b's'; 1000]).unwrap();
        set_time(&done.trash, later + std::time::Duration::from_secs(60));
        write_report(&r);
        assert!(can_undo());
        // A report of an earlier version has only the samples to go by, and they match.
        let mut samples_only = r.clone();
        samples_only.as_object_mut().unwrap().remove("source_hash");
        write_report(&samples_only);
        assert!(can_undo());
        undo(&report, true).unwrap();
        assert!(f.lib("a.mp4").exists());
    }

    #[test]
    fn a_file_in_the_trash_changed_at_its_size_and_date_does_not_come_back() {
        let f = Fix::new("trash-rot");
        let report = f.encode("a.mp4", "a.mkv", 1000, 400);
        let source = f.lib("a.mp4");
        let hash = crate::source::WholeHash::start(&source, 1000).finish(&|| false).unwrap();
        for with_hash in [true, false] {
            let mut r: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
            if with_hash {
                r["source_hash"] = json!(hash);
            }
            std::fs::write(&report, r.to_string()).unwrap();
            let done = f.apply(&report).unwrap();
            let date = std::fs::metadata(&done.trash).unwrap().modified().unwrap();
            // One byte changed, as a drive that rots or a tool that keeps dates would leave it.
            let mut bytes = std::fs::read(&done.trash).unwrap();
            bytes[300] = b'x';
            std::fs::write(&done.trash, &bytes).unwrap();
            std::fs::File::options().write(true).open(&done.trash).unwrap().set_modified(date).unwrap();
            let err = undo(&report, true).unwrap_err();
            assert!(err.to_string().starts_with("the file in the trash is not the original"), "{err}");
            assert!(!source.exists() && done.trash.exists() && f.lib("a.mkv").exists());
            // As it was, it comes back.
            bytes[300] = b's';
            std::fs::write(&done.trash, &bytes).unwrap();
            std::fs::File::options().write(true).open(&done.trash).unwrap().set_modified(date).unwrap();
            undo(&report, true).unwrap();
            assert!(source.exists() && f.out("a.mkv").exists());
        }
    }

    #[test]
    fn an_original_rewritten_with_its_time_set_back_stays_where_it_is() {
        let f = Fix::new("set-back");
        let report = f.encode("a.mp4", "a.mkv", 1000, 400);
        let source = f.lib("a.mp4");
        let set_time = |seconds: u64| std::fs::File::options().write(true).open(&source).unwrap().set_modified(UNIX_EPOCH + std::time::Duration::from_secs(seconds)).unwrap();
        set_time(1_500_000_000);
        let (_, then) = crate::source::Recorded::of(&source).unwrap();
        let hash = crate::source::WholeHash::start(&source, 1000).finish(&|| false).unwrap();
        let mut r: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
        (r["source_mtime"], r["source_fingerprint"], r["source_hash"]) = (json!(then.modified), json!(then.fingerprint), json!(hash));
        std::fs::write(&report, r.to_string()).unwrap();

        // Only touched, and found to be the same by a look that a server keeps in mind.
        set_time(1_500_000_900);
        assert!(matches!(check(&report), Check::Ready(_)));
        // Another file of that size, under the very time that look saw.
        std::fs::write(&source, vec![b't'; 1000]).unwrap();
        set_time(1_500_000_900);
        assert_eq!(no(&report), "the original changed since the encode");
        assert!(f.apply(&report).is_err());
        assert_eq!(std::fs::read(&source).unwrap(), vec![b't'; 1000]);
        assert!(!f.dir.join("Trash/files/a.mp4").exists());
    }

    #[test]
    fn the_output_takes_the_originals_place_and_undo_puts_both_back() {
        let f = Fix::new("basic");
        let report = f.encode("clip one.mp4", "clip one.mkv", 1000, 400);
        let Check::Ready(plan) = check(&report) else { panic!() };
        assert_eq!(plan.dest, f.lib("clip one.mkv"));
        assert!(!plan.copy && plan.removed == 0);

        let encoded = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        std::fs::File::options().write(true).open(&report).unwrap().set_modified(encoded).unwrap();
        let done = f.apply(&report).unwrap();
        assert!(done.warnings.is_empty(), "{:?}", done.warnings);
        assert_eq!(done.dest, f.lib("clip one.mkv"));
        assert_eq!(std::fs::read(&done.dest).unwrap(), vec![b'o'; 400]);
        assert_eq!(std::fs::read(&done.trash).unwrap(), vec![b's'; 1000]);
        assert!(!f.lib("clip one.mp4").exists() && !f.out("clip one.mkv").exists());
        assert_eq!(done.trash, f.dir.join("Trash/files/clip one.mp4"));
        // No staging file is left behind.
        assert_eq!(std::fs::read_dir(f.dir.join("lib")).unwrap().count(), 1);

        assert_eq!(std::fs::metadata(&report).unwrap().modified().unwrap(), encoded);
        // The report now tells where things are, and a second go is refused.
        let r: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
        assert_eq!(r["output"], json!(f.lib("clip one.mkv")));
        assert_eq!(r["input"], json!(f.lib("clip one.mp4")));
        assert_eq!(r["replaced"]["from"], json!(f.out("clip one.mkv")));
        assert_eq!(r["replaced"]["trash"], json!(done.trash));
        assert_eq!(r["crf"], json!(30.0));
        let Check::Replaced(d) = check(&report) else { panic!() };
        assert!(d.can_undo);
        assert_eq!(f.apply(&report).unwrap_err().to_string(), "already replaced");

        undo(&report, false).unwrap();
        assert_eq!(std::fs::read(f.lib("clip one.mp4")).unwrap(), vec![b's'; 1000]);
        assert_eq!(std::fs::read(f.out("clip one.mkv")).unwrap(), vec![b'o'; 400]);
        assert!(!f.lib("clip one.mkv").exists() && !done.trash.exists());
        let r: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
        assert!(r.get("replaced").is_none());
        assert_eq!(r["output"], json!(f.out("clip one.mkv")));
        assert!(matches!(check(&report), Check::Ready(_)));
        assert_eq!(undo(&report, false).unwrap_err().to_string(), "this original has not been replaced, so there is nothing to put back");
    }

    /// Every file under `dir` and what it holds.
    fn everything_in(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut found = Vec::new();
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            if e.file_type().unwrap().is_dir() {
                found.extend(everything_in(&e.path()));
            } else {
                found.push((e.path(), std::fs::read(e.path()).unwrap()));
            }
        }
        found.sort();
        found
    }

    #[test]
    fn nothing_on_zfs_is_replaced_or_put_back_unless_that_is_allowed() {
        let f = Fix::new("zfs");
        let report = f.encode("pool.mp4", "pool.mkv", 1000, 400);
        let home = f.dir.join("Trash");
        let to_trash = |p: &Path, n: trash::BeforeMove| trash::trash_with_noting(p, Some(&home), 1000, "2026-11-10T22:15:03".into(), n);
        let zfs = |dirs: &[&str]| crate::fsguard::ZFS_IN_TESTS.set(dirs.iter().map(|d| f.dir.join(d)).collect());

        // The original's folder on ZFS: neither replacing nor the command that lists and
        // replaces moves anything.
        zfs(&["lib"]);
        let before = everything_in(&f.dir);
        let e = apply_with(&report, false, &to_trash).unwrap_err().to_string();
        assert!(e.contains("the original is on ZFS"), "{e}");
        assert_eq!(everything_in(&f.dir), before);
        run_watched(&[f.dir.join("out"), f.dir.join("lib")], true, false, false, false, false, &|| {}).unwrap();
        assert_eq!(everything_in(&f.dir), before);
        apply_with(&report, true, &to_trash).unwrap();

        // Putting it back writes to the original's folder, and to the output's.
        let replaced = everything_in(&f.dir);
        for (on, refused) in [("lib", f.dir.join("lib")), ("out", f.dir.join("out"))] {
            zfs(&[on]);
            let e = undo(&report, false).unwrap_err().to_string();
            assert!(e.contains(&format!("{} is on ZFS", refused.display())), "{on}: {e}");
            assert_eq!(everything_in(&f.dir), replaced, "{on}");
        }
        undo(&report, true).unwrap();
        assert_eq!(std::fs::read(f.lib("pool.mp4")).unwrap(), vec![b's'; 1000]);
        zfs(&[]);
    }

    #[test]
    fn one_run_at_a_time_per_report() {
        use std::os::fd::AsRawFd;
        let f = Fix::new("lock");
        let report = f.encode("busy.mp4", "busy.mkv", 1000, 400);
        let other = std::fs::File::open(&report).unwrap();
        // SAFETY: flock on a descriptor the test owns.
        assert_eq!(unsafe { libc::flock(other.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
        assert_eq!(f.apply(&report).unwrap_err().to_string(), "this file is being replaced or put back right now");
        assert!(f.lib("busy.mp4").exists() && f.out("busy.mkv").exists());
        drop(other);
        f.apply(&report).unwrap();
        // The lock went with the run: undo gets its own.
        undo(&report, false).unwrap();
    }

    #[test]
    fn a_trash_that_refuses_changes_nothing() {
        let f = Fix::new("no-trash");
        let report = f.encode("keep.mp4", "keep.mkv", 1000, 400);
        let err = apply_with(&report, false, &|_, _| bail!("no trash on this drive")).unwrap_err();
        assert!(format!("{err:#}").contains("nothing was changed"), "{err:#}");
        assert_eq!(std::fs::read(f.lib("keep.mp4")).unwrap(), vec![b's'; 1000]);
        assert_eq!(std::fs::read(f.out("keep.mkv")).unwrap(), vec![b'o'; 400]);
        assert_eq!(std::fs::read_dir(f.dir.join("lib")).unwrap().count(), 1);
        assert!(matches!(check(&report), Check::Ready(_)));
    }

    #[test]
    fn a_place_taken_at_the_last_moment_brings_the_original_back() {
        let f = Fix::new("race");
        let report = f.encode("r.mp4", "r.mkv", 1000, 400);
        let (home, dest) = (f.dir.join("Trash"), f.lib("r.mkv"));
        // Something lands on the new name between the check and the rename.
        let err = apply_with(&report, false, &|p, n| {
            let t = trash::trash_with_noting(p, Some(&home), 1000, "2026-11-10T22:15:03".into(), n)?;
            std::fs::write(&dest, b"someone else's")?;
            Ok(t)
        })
        .unwrap_err();
        assert!(err.to_string().ends_with("the original is back in its place"), "{err}");
        assert_eq!(std::fs::read(f.lib("r.mp4")).unwrap(), vec![b's'; 1000]);
        assert_eq!(std::fs::read(f.out("r.mkv")).unwrap(), vec![b'o'; 400]);
        assert_eq!(std::fs::read(&dest).unwrap(), b"someone else's");
        assert_eq!(std::fs::read_dir(home.join("files")).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(home.join("info")).unwrap().count(), 0);
    }

    #[test]
    fn same_extension_reuses_the_originals_path() {
        let f = Fix::new("same-ext");
        let report = f.encode("film.mkv", "film.mkv", 900, 300);
        let Check::Ready(plan) = check(&report) else { panic!() };
        assert_eq!(plan.dest, plan.source);
        let done = f.apply(&report).unwrap();
        assert_eq!(std::fs::read(f.lib("film.mkv")).unwrap(), vec![b'o'; 300]);
        assert_eq!(std::fs::read(&done.trash).unwrap(), vec![b's'; 900]);
        // Undo: the AV1 file has to leave before the original can come back to the same path.
        undo(&report, false).unwrap();
        assert_eq!(std::fs::read(f.lib("film.mkv")).unwrap(), vec![b's'; 900]);
        assert_eq!(std::fs::read(f.out("film.mkv")).unwrap(), vec![b'o'; 300]);
    }

    #[test]
    fn an_output_already_next_to_its_original_only_loses_the_original() {
        let f = Fix::new("in-place");
        let report = f.encode("a.mp4", "a.mkv", 800, 200);
        // Encoded straight into the library folder.
        let (o, rep) = (f.lib("a.mkv"), f.lib("a.av1opt.json"));
        std::fs::rename(f.out("a.mkv"), &o).unwrap();
        let mut r: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
        r["output"] = json!(o);
        std::fs::write(&rep, serde_json::to_vec(&r).unwrap()).unwrap();
        let Check::Ready(plan) = check(&rep) else { panic!() };
        assert_eq!(plan.dest, o);
        let done = f.apply(&rep).unwrap();
        assert!(o.exists() && !f.lib("a.mp4").exists());
        undo(&rep, false).unwrap();
        assert!(o.exists() && f.lib("a.mp4").exists() && !done.trash.exists());
    }

    #[test]
    fn anything_that_does_not_match_the_report_is_left_alone() {
        let f = Fix::new("refuse");
        let state = |f: &Fix| {
            let mut names: Vec<String> = walkdir::WalkDir::new(&f.dir)
                .into_iter()
                .flatten()
                .filter(|e| e.file_type().is_file() && !e.file_name().to_string_lossy().ends_with(".json"))
                .map(|e| format!("{}:{}", e.path().display(), e.metadata().unwrap().len()))
                .collect();
            names.sort();
            names
        };

        let changed = f.encode("changed.mp4", "changed.mkv", 1000, 400);
        std::fs::write(f.lib("changed.mp4"), b"edited since").unwrap();
        assert_eq!(no(&changed), "the original changed since the encode");

        let gone = f.encode("gone.mp4", "gone.mkv", 1000, 400);
        std::fs::remove_file(f.lib("gone.mp4")).unwrap();
        assert_eq!(no(&gone), "the original is gone");

        let out_changed = f.encode("o.mp4", "o.mkv", 1000, 400);
        std::fs::write(f.out("o.mkv"), b"other").unwrap();
        assert_eq!(no(&out_changed), "the output changed since the encode");

        let out_gone = f.encode("og.mp4", "og.mkv", 1000, 400);
        std::fs::remove_file(f.out("og.mkv")).unwrap();
        assert_eq!(no(&out_gone), "the output is gone");

        let bigger = f.encode("big.mp4", "big.mkv", 400, 400);
        assert_eq!(no(&bigger), "the output is not smaller than the original");

        let taken = f.encode("taken.mp4", "taken.mkv", 1000, 400);
        std::fs::write(f.lib("taken.mkv"), b"someone else's").unwrap();
        assert!(no(&taken).ends_with("taken.mkv already exists"));

        let link = f.encode("target.mp4", "link.mkv", 1000, 400);
        std::os::unix::fs::symlink(f.lib("target.mp4"), f.lib("link.mp4")).unwrap();
        let mut r: Value = serde_json::from_slice(&std::fs::read(&link).unwrap()).unwrap();
        r["input"] = json!(f.lib("link.mp4"));
        std::fs::write(&link, serde_json::to_vec(&r).unwrap()).unwrap();
        assert_eq!(no(&link), "the original is a link");

        let newer = f.encode("newer.mp4", "newer.mkv", 1000, 400);
        let mut r: Value = serde_json::from_slice(&std::fs::read(&newer).unwrap()).unwrap();
        r["format"] = json!(REPORT_FORMAT + 1);
        std::fs::write(&newer, serde_json::to_vec(&r).unwrap()).unwrap();
        assert_eq!(no(&newer), "report written by a newer av1opt");

        let relative = f.encode("rel.mp4", "rel.mkv", 1000, 400);
        let mut r: Value = serde_json::from_slice(&std::fs::read(&relative).unwrap()).unwrap();
        r["input"] = json!("lib/rel.mp4");
        std::fs::write(&relative, serde_json::to_vec(&r).unwrap()).unwrap();
        assert_eq!(no(&relative), "report from an older av1opt, without full paths");

        assert!(no(&f.out("missing.av1opt.json")).starts_with("no report"));

        // And `apply` on any of them changes nothing.
        let before = state(&f);
        for r in [&changed, &gone, &out_changed, &out_gone, &bigger, &taken, &link, &newer, &relative] {
            assert!(f.apply(r).is_err());
            assert!(undo(r, true).is_err());
        }
        assert_eq!(state(&f), before);
    }

    /// A report is a file anybody can write. One that is not next to its output, names files
    /// that are no videos, or points at a trash file that never was the original moves nothing.
    #[test]
    fn a_report_is_only_followed_next_to_its_output_and_for_videos() {
        let f = Fix::new("forged");
        let state = |f: &Fix| {
            let mut names: Vec<String> = walkdir::WalkDir::new(&f.dir).into_iter().flatten().filter(|e| e.file_type().is_file() && e.path().extension().is_none_or(|x| x != "lock")).map(|e| e.path().display().to_string()).collect();
            names.sort();
            names
        };
        // The report of one output, copied to another folder (with an output of that size).
        let report = f.encode("a.mp4", "a.mkv", 1000, 400);
        std::fs::create_dir_all(f.dir.join("downloads")).unwrap();
        let stray = f.dir.join("downloads/a.av1opt.json");
        std::fs::copy(&report, &stray).unwrap();
        assert_eq!(no(&stray), "the report is not the one next to its output");
        // Next to the output, under another name.
        let renamed = f.out("other.av1opt.json");
        std::fs::copy(&report, &renamed).unwrap();
        assert_eq!(no(&renamed), "the report is not the one next to its output");
        // Files that are no videos.
        let doc = f.encode("letter.odt", "letter.mkv", 1000, 400);
        assert_eq!(no(&doc), "the report names a file that is not a video");
        let out_doc = f.encode("b.mp4", "b.txt", 1000, 400);
        assert_eq!(no(&out_doc), "the report names a file that is not a video");
        let before = state(&f);
        for r in [&stray, &renamed, &doc, &out_doc] {
            assert!(f.apply(r).is_err());
            assert!(undo(r, true).is_err());
        }
        assert_eq!(state(&f), before);

        // A replaced report whose trash file is some other file of the right size: no undo.
        let done = f.apply(&report).unwrap();
        let victim = f.dir.join("elsewhere.mp4");
        std::fs::write(&victim, vec![b'v'; 1000]).unwrap();
        let honest = std::fs::read(&report).unwrap();
        let mut r: Value = serde_json::from_slice(&honest).unwrap();
        r["replaced"]["trash"] = json!(victim);
        std::fs::write(&report, serde_json::to_vec(&r).unwrap()).unwrap();
        let Check::Replaced(d) = check(&report) else { panic!() };
        assert!(!d.can_undo);
        assert!(undo(&report, true).unwrap_err().to_string().starts_with("the trash does not say that"));
        assert!(victim.exists() && f.lib("a.mkv").exists());
        // A trash folder of somebody else's making, whose info file names the original by
        // its full path: only the user's own trash is believed about that.
        let fake = f.dir.join("downloads/.Trash-1000");
        std::fs::create_dir_all(fake.join("files")).unwrap();
        std::fs::create_dir_all(fake.join("info")).unwrap();
        std::fs::write(fake.join("files/a.mp4"), vec![b'v'; 1000]).unwrap();
        std::fs::write(fake.join("info/a.mp4.trashinfo"), format!("[Trash Info]\nPath={}\nDeletionDate=2026-11-10T22:15:03\n", f.lib("a.mp4").display())).unwrap();
        r["replaced"]["trash"] = json!(fake.join("files/a.mp4"));
        std::fs::write(&report, serde_json::to_vec(&r).unwrap()).unwrap();
        assert!(undo(&report, true).unwrap_err().to_string().starts_with("the trash does not say that"));
        // The AV1 file somewhere a replacement would never have put it: undo would carry
        // that file off to the report's folder.
        let mut r: Value = serde_json::from_slice(&honest).unwrap();
        r["output"] = json!(victim);
        std::fs::write(&report, serde_json::to_vec(&r).unwrap()).unwrap();
        assert_eq!(no(&report), "the report does not say where the AV1 file went in a way `replace` would have");
        assert!(undo(&report, true).is_err() && victim.exists());
        // A link to a report is not a report.
        std::fs::write(&report, &honest).unwrap();
        let aside = f.dir.join("downloads/real.json");
        std::fs::rename(&report, &aside).unwrap();
        std::os::unix::fs::symlink(&aside, &report).unwrap();
        assert_eq!(no(&report), "the report is a link");
        std::fs::remove_file(&report).unwrap();
        // The honest report still undoes.
        std::fs::write(&report, honest).unwrap();
        let Check::Replaced(d) = check(&report) else { panic!() };
        assert!(d.can_undo && d.trash == done.trash);
        undo(&report, false).unwrap();
        assert!(f.lib("a.mp4").exists() && f.out("a.mkv").exists());
    }

    #[test]
    fn a_name_near_the_limit_is_replaced_set_aside_and_given_back() {
        let f = Fix::new("long");
        // As long as a stem can be and still have a report.
        let most = crate::files::name_max(&f.dir) - REPORT_SUFFIX.len();
        let stem = format!("{}{}", "é".repeat(most / 2), "x".repeat(most % 2));
        let (source, output) = (format!("{stem}.mp4"), format!("{stem}.mkv"));
        let report = f.encode(&source, &output, 1000, 400);
        let done = f.apply(&report).unwrap();
        assert!(done.warnings.is_empty(), "{:?}", done.warnings);
        let Check::Replaced(d) = check(&report) else { panic!("{}", no(&report)) };
        assert!(d.can_undo, "the report must say where the original went");

        let aside = set_aside(&report, &f.out(&output), 1234).unwrap();
        let Check::Replaced(d) = check(&aside) else { panic!("{}", no(&aside)) };
        assert!(d.can_undo);
        f.encode("y.mp4", &output, 900, 300);
        undo(&aside, false).unwrap();
        assert_eq!(std::fs::read(f.lib(&source)).unwrap().len(), 1000);
        assert_eq!(std::fs::read(&d.from).unwrap().len(), 400);
        assert!(d.from.parent() == Some(&*f.dir.join("out")) && d.from != f.out(&output));
        let longest = crate::files::name_max(&f.dir);
        assert_eq!(staging_name(&f.lib(&"e".repeat(longest))).file_name().unwrap().len(), longest);
    }

    #[test]
    fn two_long_names_that_begin_alike_are_copied_under_names_of_their_own() {
        let f = Fix::new("staging");
        let longest = crate::files::name_max(&f.dir);
        let name = |end: &str| format!("{}{end}", "e".repeat(longest - end.len()));
        let (a, b) = (staging_name(&f.lib(&name("1.mkv"))), staging_name(&f.lib(&name("2.mkv"))));
        assert_ne!(a, b);
        assert_eq!(a.file_name().unwrap().len(), longest);
        // Nor do two copies to one place take each other's name.
        assert_ne!(staging_name(&f.lib("x.mkv")), staging_name(&f.lib("x.mkv")));
    }

    #[test]
    fn only_copies_nobody_is_writing_are_cleared_away() {
        let f = Fix::new("left-behind");
        let dest = f.lib("film.mkv");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        // A process number no process can have, this very process, and another one that is
        // running (the one that started the tests), as another replace at work would be.
        let (dead, alive) = (u32::MAX, std::os::unix::process::parent_id());
        let ours = staging_name(&dest);
        let others = [".av1opt-film.mkv.part".to_string(), format!(".av1opt-film.mkv.{dead}.3.part"), format!(".av1opt-film.{dead}.3.part")];
        let keep = [
            format!(".av1opt-film.mkv.{alive}.3.part"),
            format!(".av1opt-other.mkv.{dead}.3.part"),
            ".av1opt-film.mkv.notes".to_string(),
            "film.mkv.part".to_string(),
        ];
        std::fs::write(&ours, b"being written").unwrap();
        for name in others.iter().chain(&keep) {
            std::fs::write(f.lib(name), b"x").unwrap();
        }
        clear_left_behind(&dest);
        assert!(ours.exists(), "the copy of this run stays");
        assert!(others.iter().all(|n| !f.lib(n).exists()), "copies of runs that are gone");
        assert!(keep.iter().all(|n| f.lib(n).exists()), "copies of a run at work, and files that are something else");
    }

    #[test]
    fn a_report_set_aside_for_a_newer_encode_can_still_undo() {
        let f = Fix::new("aside");
        let report = f.encode("x.mp4", "x.mkv", 1000, 400);
        let done = f.apply(&report).unwrap();
        let aside = set_aside(&report, &f.out("x.mkv"), 1234).unwrap();
        assert_eq!(aside, f.out("x.replaced-1234.av1opt.json"));
        // A newer encode now has the place and the report's name.
        let newer = f.encode("y.mp4", "x.mkv", 900, 300);
        assert_eq!(newer, report);
        assert_eq!(report_files(&[f.dir.join("out")]), [report.clone(), aside.clone()]);
        assert!(matches!(check(&newer), Check::Ready(_)));

        // It is listed as replaced, with a free place to go back to, and replaces nothing.
        let Check::Replaced(d) = check(&aside) else { panic!("{}", no(&aside)) };
        assert!(d.can_undo && d.trash == done.trash);
        assert_eq!((d.from.clone(), d.dest.clone()), (f.out("x.replaced-1234.mkv"), f.lib("x.mkv")));
        assert!(f.apply(&aside).is_err());

        undo(&aside, false).unwrap();
        assert_eq!(std::fs::read(f.lib("x.mp4")).unwrap().len(), 1000);
        assert_eq!(std::fs::read(f.out("x.replaced-1234.mkv")).unwrap().len(), 400);
        assert_eq!(std::fs::read(f.out("x.mkv")).unwrap().len(), 300);
        assert!(!f.lib("x.mkv").exists());
        // The older encode is not offered in the newer one's stead, and that one is untouched.
        assert_eq!(no(&aside), "a newer encode took the place of this one");
        assert!(f.apply(&aside).is_err());
        assert!(matches!(check(&newer), Check::Ready(_)));
    }

    #[test]
    fn a_report_set_aside_keeps_its_name_and_only_that_name_counts() {
        let f = Fix::new("aside-names");
        let report = f.encode("x.mp4", "x.mkv", 1000, 400);
        f.apply(&report).unwrap();
        let honest = std::fs::read(&report).unwrap();
        // Two in the same second do not take each other's place.
        assert_eq!(set_aside(&report, &f.out("x.mkv"), 7).unwrap(), f.out("x.replaced-7.av1opt.json"));
        std::fs::write(&report, &honest).unwrap();
        assert_eq!(set_aside(&report, &f.out("x.mkv"), 7).unwrap(), f.out("x.replaced-8.av1opt.json"));
        assert!(matches!(check(&f.out("x.replaced-8.av1opt.json")), Check::Replaced(_)));
        std::fs::create_dir_all(f.dir.join("downloads")).unwrap();
        for forged in [f.out("x.replaced-1x.av1opt.json"), f.out("x.replaced-01.av1opt.json"), f.out("x.replaced-.av1opt.json"), f.out("y.replaced-1.av1opt.json"), f.dir.join("downloads/x.replaced-1.av1opt.json")] {
            std::fs::write(&forged, &honest).unwrap();
            assert_eq!(no(&forged), "the report is not the one next to where the output was", "{}", forged.display());
        }
        // A video that happens to be called like that is a video like any other.
        let plain = f.encode("v.replaced-5.mp4", "v.replaced-5.mkv", 1000, 400);
        assert!(matches!(check(&plain), Check::Ready(_)), "{}", no(&plain));
    }

    #[test]
    fn undo_needs_the_original_in_the_trash_and_a_free_place() {
        let f = Fix::new("undo");
        let report = f.encode("x.mp4", "x.mkv", 1000, 400);
        let done = f.apply(&report).unwrap();
        // Something new sits where the AV1 file came from.
        std::fs::write(f.out("x.mkv"), b"newer encode").unwrap();
        assert!(undo(&report, false).unwrap_err().to_string().ends_with("x.mkv already exists"));
        std::fs::remove_file(f.out("x.mkv")).unwrap();
        // Something new sits where the original was.
        std::fs::write(f.lib("x.mp4"), b"downloaded again").unwrap();
        assert!(undo(&report, false).unwrap_err().to_string().ends_with("x.mp4 already exists"));
        assert!(f.lib("x.mkv").exists() && done.trash.exists());
        std::fs::remove_file(f.lib("x.mp4")).unwrap();
        // The trash was emptied.
        let kept = f.dir.join("kept");
        std::fs::rename(&done.trash, &kept).unwrap();
        let Check::Replaced(d) = check(&report) else { panic!() };
        assert!(!d.can_undo);
        assert_eq!(undo(&report, false).unwrap_err().to_string(), "the original is no longer in the trash");
        assert!(f.lib("x.mkv").exists());
        std::fs::rename(&kept, &done.trash).unwrap();
        undo(&report, false).unwrap();
        assert!(f.lib("x.mp4").exists() && f.out("x.mkv").exists() && !f.lib("x.mkv").exists());
    }

    /// An output on another filesystem is copied over. `/dev/shm` is the one other filesystem
    /// a test can count on; where it isn't one, there is nothing to check, and that is said.
    #[test]
    fn outputs_on_another_drive_are_copied_then_removed() {
        let f = Fix::new("copy");
        let shm = Path::new("/dev/shm");
        let skip = |why: &str| crate::tools::skipped("outputs_on_another_drive_are_copied_then_removed", why);
        let (Ok(a), Ok(b)) = (std::fs::metadata(shm), std::fs::metadata(&f.dir)) else { return skip("/dev/shm is not there") };
        let out = shm.join(format!("av1opt-replace-test-{}", std::process::id()));
        if a.dev() == b.dev() {
            return skip("/dev/shm is on the drive of the temporary folder");
        }
        if let Err(e) = std::fs::create_dir_all(&out) {
            return skip(&format!("nothing can be made in /dev/shm: {e}"));
        }
        let report = f.encode("far.mp4", "far.mkv", 5000, 2000);
        let o = out.join("far.mkv");
        std::fs::copy(f.out("far.mkv"), &o).unwrap();
        std::fs::remove_file(f.out("far.mkv")).unwrap();
        let old = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_500_000_000);
        std::fs::File::options().write(true).open(&o).unwrap().set_modified(old).unwrap();
        let mut r: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
        r["output"] = json!(o);
        // The report lives next to its output, there too.
        std::fs::remove_file(&report).unwrap();
        let report = out.join("far.av1opt.json");
        std::fs::write(&report, serde_json::to_vec(&r).unwrap()).unwrap();

        let Check::Ready(plan) = check(&report) else { panic!() };
        assert!(plan.copy);
        // A hidden copy left by a killed run is thrown away, never taken for the output; and
        // when the trash refuses, the fresh copy goes too and both files stay.
        std::fs::write(f.lib(".av1opt-far.mkv.part"), b"half a copy").unwrap();
        assert!(apply_with(&report, false, &|_, _| bail!("no trash on this drive")).is_err());
        assert!(o.exists() && f.lib("far.mp4").exists());
        assert_eq!(std::fs::read_dir(f.dir.join("lib")).unwrap().count(), 1);
        std::fs::write(f.lib(".av1opt-far.mkv.part"), b"half a copy").unwrap();
        let done = f.apply(&report).unwrap();
        assert!(done.warnings.is_empty(), "{:?}", done.warnings);
        assert_eq!(std::fs::read(f.lib("far.mkv")).unwrap(), vec![b'o'; 2000]);
        assert_eq!(std::fs::metadata(f.lib("far.mkv")).unwrap().modified().unwrap(), old);
        assert!(!o.exists() && !f.lib("far.mp4").exists());
        assert_eq!(std::fs::read_dir(f.dir.join("lib")).unwrap().count(), 1);

        undo(&report, false).unwrap();
        assert_eq!(std::fs::read(&o).unwrap(), vec![b'o'; 2000]);
        assert_eq!(std::fs::read(f.lib("far.mp4")).unwrap(), vec![b's'; 5000]);
        assert!(!f.lib("far.mkv").exists());
        let videos = std::fs::read_dir(&out).unwrap().flatten().filter(|e| !e.file_name().to_string_lossy().contains(".av1opt.json")).count();
        assert_eq!(videos, 1);
        let _ = std::fs::remove_dir_all(&out);
    }

    /// A copy that cannot be made, or comes up short, leaves both files where they were and no
    /// part of a copy behind.
    #[test]
    fn a_copy_that_fails_changes_nothing() {
        let f = Fix::new("copy-fails");
        // One that comes up short: the file it is made from is shorter than it should be.
        let from = f.out("short.mkv");
        std::fs::write(&from, vec![b'o'; 1000]).unwrap();
        let to = f.lib(".av1opt-short.mkv.part");
        let e = copy_new(&OpenFile::open(&from, 1000).unwrap(), &to, 2000).err().unwrap();
        assert!(format!("{e:#}").contains("copied 1000 bytes of 2000"), "{e:#}");
        assert!(!to.exists());

        // One across drives into a folder that cannot be written to.
        let skip = |why: &str| crate::tools::skipped("a_copy_that_fails_changes_nothing", why);
        let shm = Path::new("/dev/shm");
        let (Ok(a), Ok(b)) = (std::fs::metadata(shm), std::fs::metadata(&f.dir)) else { return skip("/dev/shm is not there") };
        if a.dev() == b.dev() {
            return skip("/dev/shm is on the drive of the temporary folder");
        }
        let out = shm.join(format!("av1opt-replace-copy-fails-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        if let Err(e) = std::fs::create_dir_all(&out) {
            return skip(&format!("nothing can be made in /dev/shm: {e}"));
        }
        let report = f.encode("far.mp4", "far.mkv", 5000, 2000);
        let o = out.join("far.mkv");
        std::fs::rename(f.out("far.mkv"), f.dir.join("far.mkv")).unwrap();
        std::fs::copy(f.dir.join("far.mkv"), &o).unwrap();
        let mut r: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
        r["output"] = json!(o);
        std::fs::remove_file(&report).unwrap();
        let report = out.join("far.av1opt.json");
        std::fs::write(&report, serde_json::to_vec(&r).unwrap()).unwrap();
        let Check::Ready(plan) = check(&report) else { panic!() };
        assert!(plan.copy);
        let before = (everything_in(&f.dir), everything_in(&out));
        std::fs::set_permissions(f.dir.join("lib"), std::os::unix::fs::PermissionsExt::from_mode(0o555)).unwrap();
        let e = f.apply(&report);
        std::fs::set_permissions(f.dir.join("lib"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let after = (everything_in(&f.dir), everything_in(&out));
        let _ = std::fs::remove_dir_all(&out);
        assert!(format!("{:#}", e.err().unwrap()).contains("cannot copy"));
        assert_eq!(after, before);
    }

    #[test]
    fn reports_are_found_from_folders_reports_and_outputs() {
        let f = Fix::new("find");
        let a = f.encode("a.mp4", "a.mkv", 10, 5);
        let b = f.encode("b.mp4", "b.mkv", 10, 5);
        std::fs::create_dir_all(f.out(".av1opt-work")).unwrap();
        std::fs::write(f.out(".av1opt-work/c.av1opt.json"), b"{}").unwrap();
        assert_eq!(reports_for(&[f.dir.join("out")]), vec![a.clone(), b.clone()]);
        assert_eq!(reports_for(&[f.out("b.mkv"), a.clone(), f.dir.join("out")]), vec![b, a]);
    }

    /// The record of this test's own, under its scratch folder, for as long as it lives.
    fn record_in(f: &Fix) -> PathBuf {
        crate::ledger::TEST_FOLDER.with(|d| *d.borrow_mut() = Some(f.dir.join("data/av1opt")));
        f.dir.join("data/av1opt/ledger.jsonl")
    }

    #[test]
    fn adopt_reads_the_record_once_however_many_reports_it_takes() {
        let f = Fix::new("adopt-once");
        let record = record_in(&f);
        for i in 0..2000 {
            f.encode(&format!("v{i}.mp4"), &format!("v{i}.mkv"), 30, 10);
        }
        let loads = || crate::ledger::LOADS.with(std::cell::Cell::get);
        let (before, started) = (loads(), std::time::Instant::now());
        adopt(std::slice::from_ref(&f.dir), false, false).unwrap();
        assert_eq!(loads() - before, 1);
        assert!(!f.dir.join("data").exists(), "the listing wrote to the record");
        let before = loads();
        adopt(std::slice::from_ref(&f.dir), true, false).unwrap();
        assert_eq!(loads() - before, 1);
        let text = std::fs::read_to_string(&record).unwrap();
        assert_eq!(text.lines().filter(|l| l.contains("\"adopted\"")).count(), 2000);
        // Each report is read a handful of times, and each file once; reading the record again
        // for each would make this quadratic.
        assert!(started.elapsed() < std::time::Duration::from_secs(120), "{:?}", started.elapsed());
        // What another run writes meanwhile is seen: the record is read again once it changed.
        let mut held = crate::ledger::Held::load();
        let before = loads();
        held.current();
        assert_eq!(loads() - before, 0);
        crate::ledger::note_undone(&f.lib("v0.mp4"), &f.lib("v0.mkv"), &f.out("v0.mkv")).unwrap();
        held.current();
        assert_eq!(loads() - before, 1);
        crate::ledger::TEST_FOLDER.with(|d| *d.borrow_mut() = None);
    }

    #[test]
    fn an_original_in_the_trash_is_taken_on_record_only_if_all_of_it_is_what_the_report_holds() {
        let f = Fix::new("adopt-trash-whole");
        let record = record_in(&f);
        let size = 4 << 20;
        let report = f.encode("big.mp4", "big.mkv", size, 1000);
        let source = f.lib("big.mp4");
        // A byte between the samples of the fingerprint, which only the whole hash covers.
        let mut bytes = std::fs::read(&source).unwrap();
        bytes[(5 << 20) / 4] = b'x';
        std::fs::write(&source, &bytes).unwrap();
        let mut r = read_report(&report).unwrap();
        let (_, known) = crate::source::Recorded::of(&source).unwrap();
        r["source_mtime"] = json!(known.modified);
        r["source_fingerprint"] = json!(known.fingerprint);
        r["source_hash"] = json!(crate::source::whole_hash_of_open(&File::open(&source).unwrap()).unwrap());
        write_report(&report, &r).unwrap();
        f.apply(&report).unwrap();
        std::fs::remove_file(&record).unwrap();
        let trashed = f.dir.join("Trash/files/big.mp4");
        let change = |byte: u8| {
            let date = std::fs::metadata(&trashed).unwrap().modified().unwrap();
            let mut bytes = std::fs::read(&trashed).unwrap();
            bytes[(5 << 20) / 4] = byte;
            std::fs::write(&trashed, &bytes).unwrap();
            File::options().write(true).open(&trashed).unwrap().set_modified(date).unwrap();
        };
        let named = [f.dir.join("out"), f.dir.join("lib")];

        change(b's');
        let failed = adopt(&named, true, false).unwrap_err();
        assert!(failed.to_string().contains("could not be taken on record"), "{failed:#}");
        assert!(!record.exists() || !std::fs::read_to_string(&record).unwrap().contains("\"adopted\""));
        change(b'x');
        adopt(&named, true, false).unwrap();
        assert!(std::fs::read_to_string(&record).unwrap().contains("\"adopted\""));
        crate::ledger::TEST_FOLDER.with(|d| *d.borrow_mut() = None);
    }

    #[test]
    fn an_adoption_stopped_after_its_first_output_takes_the_others_on_the_next_run() {
        let f = Fix::new("adopt-stopped");
        let record = record_in(&f);
        let ready = f.encode("a.mp4", "a.mkv", 30, 10);
        let replaced = f.encode("b.mp4", "b.mkv", 30, 10);
        f.apply(&replaced).unwrap();
        std::fs::remove_file(&record).unwrap();
        let named = [f.dir.join("out"), f.dir.join("lib")];
        let adopted = || std::fs::read_to_string(&record).unwrap_or_default().lines().filter(|l| l.contains("\"adopted\"")).count();
        stopped_at("adopt:noted", || drop(adopt(&named, true, false)));
        assert_eq!(adopted(), 1);
        adopt(&named, true, false).unwrap();
        assert_eq!(adopted(), 2);
        // Each once: a third run finds both there already.
        adopt(&named, true, false).unwrap();
        assert_eq!(adopted(), 2);
        let ledger = crate::ledger::load();
        assert!(ledger.entry(&f.out("a.mkv"), &f.lib("a.mp4")).is_some_and(|e| e.made.is_some() && e.replaced.is_none()), "{ready:?}");
        assert!(ledger.entry(&f.lib("b.mkv"), &f.lib("b.mp4")).is_some_and(|e| e.replaced.is_some()));
        crate::ledger::TEST_FOLDER.with(|d| *d.borrow_mut() = None);
    }

    #[test]
    fn a_path_that_climbs_out_of_a_folder_is_refused_and_one_with_dots_is_the_same_path() {
        let r = json!({ "input": "/videos/a/../b.mp4", "output": "/videos/./a/b.mkv" });
        assert!(full_path(&r, "input").unwrap_err().contains("with `..` in it"));
        assert_eq!(full_path(&r, "output").unwrap(), PathBuf::from("/videos/a/b.mkv"));
    }

    /// `report`'s pair on record as an encode, with the hash of its output, as `encode` leaves it.
    fn on_record(report: &Path) {
        let r = read_report(report).unwrap();
        let (source, output) = (full_path(&r, "input").unwrap(), full_path(&r, "output").unwrap());
        let original = Recorded::in_report(&r);
        let output_sha256 = Some(crate::ledger::sha256_of_file(&output).unwrap());
        let (source_bytes, output_bytes) = (file_size(&source).unwrap(), file_size(&output).unwrap());
        crate::ledger::note_encoded(&crate::ledger::Encode { output: &output, report, source: &source, source_bytes, original: &original, output_bytes, output_sha256 }).unwrap();
    }

    /// Runs `act` as a run killed at the point where it calls `kill`.
    fn killed(act: impl FnOnce(&dyn Fn())) {
        let stopped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| act(&|| std::panic::panic_any("killed here"))));
        assert!(stopped.is_err(), "the run went past the point it was to be killed at");
    }

    fn says_replaced(report: &Path) -> bool {
        read_report(report).unwrap().get("replaced").is_some()
    }

    #[test]
    fn a_replacement_killed_before_its_report_was_rewritten_is_put_back_by_the_record() {
        let f = Fix::new("stranded-apply");
        let _record = record_in(&f);
        let report = f.encode("a.mp4", "a.mkv", 1000, 400);
        on_record(&report);
        let home = f.dir.join("Trash");
        let to_trash = |p: &Path, n: trash::BeforeMove| trash::trash_with_noting(p, Some(&home), 1000, "2026-11-10T22:15:03".into(), n);
        killed(|kill| drop(apply_judging(&report, true, Judging::default(), &to_trash, &|_, _| Ok(()), kill)));
        assert!(!f.lib("a.mp4").exists() && f.lib("a.mkv").exists() && !says_replaced(&report));

        // The report alone has the original gone; with the record, it is a replacement to undo.
        assert!(matches!(check(&report), Check::No(why) if why.contains("the original is gone")));
        let ledger = crate::ledger::load();
        assert!(matches!(check_for_list_on_record(&report, &ledger), Check::Replaced(d) if d.can_undo));
        // Nothing is put right on a word the files do not bear out.
        std::fs::write(f.lib("a.mkv"), vec![b'x'; 400]).unwrap();
        assert!(set_right(&report, true, &mut Vec::new()).unwrap().is_none() && !says_replaced(&report));
        std::fs::write(f.lib("a.mkv"), vec![b'o'; 400]).unwrap();

        undo(&report, true).unwrap();
        assert_eq!(std::fs::read(f.lib("a.mp4")).unwrap(), vec![b's'; 1000]);
        assert!(f.out("a.mkv").exists() && !f.lib("a.mkv").exists() && !says_replaced(&report));
        assert!(matches!(check(&report), Check::Ready(_)));
        crate::ledger::TEST_FOLDER.with(|d| *d.borrow_mut() = None);
    }

    #[test]
    fn an_undo_killed_before_its_report_was_rewritten_is_set_right_by_the_record() {
        let f = Fix::new("stranded-undo");
        let _record = record_in(&f);
        let report = f.encode("a.mp4", "a.mkv", 1000, 400);
        on_record(&report);
        f.apply(&report).unwrap();
        killed(|kill| drop(undo_watched(&report, true, &|_| Ok(()), &|| {}, kill)));
        assert!(f.lib("a.mp4").exists() && f.out("a.mkv").exists() && says_replaced(&report));
        assert!(matches!(check(&report), Check::Replaced(d) if !d.can_undo));
        assert!(matches!(check_for_list_on_record(&report, &crate::ledger::load()), Check::Ready(_)));

        // Replacing it again sets the report right first.
        f.apply(&report).unwrap();
        assert!(!f.lib("a.mp4").exists() && f.lib("a.mkv").exists() && says_replaced(&report));

        // And an undo asked of a report left behind by an undo is done already.
        killed(|kill| drop(undo_watched(&report, true, &|_| Ok(()), &|| {}, kill)));
        assert!(says_replaced(&report));
        let d = undo(&report, true).unwrap();
        assert_eq!(d.source, f.lib("a.mp4"));
        assert!(f.lib("a.mp4").exists() && f.out("a.mkv").exists() && !says_replaced(&report));
        crate::ledger::TEST_FOLDER.with(|d| *d.borrow_mut() = None);
    }

    #[test]
    fn a_replacement_whose_report_could_not_be_written_fails_and_says_what_puts_it_back() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fix::new("report-unwritten");
        let _record = record_in(&f);
        let report = f.encode("a.mp4", "a.mkv", 1000, 400);
        on_record(&report);
        let unreadable = || std::fs::set_permissions(&report, std::fs::Permissions::from_mode(0o000)).unwrap();
        let home = f.dir.join("Trash");
        let to_trash = |p: &Path, n: trash::BeforeMove| trash::trash_with_noting(p, Some(&home), 1000, "2026-11-10T22:15:03".into(), n);
        let done = apply_judging(&report, true, Judging::default(), &to_trash, &|_, _| Ok(()), &unreadable).unwrap();
        std::fs::set_permissions(&report, std::fs::Permissions::from_mode(0o644)).unwrap();
        let why = done.report_behind.unwrap();
        assert!(why.contains("still says that nothing was replaced") && why.contains("av1opt replace --undo --apply"), "{why}");
        // Where both files are, which the report no longer can say.
        assert_eq!((std::fs::read(&done.trash).unwrap(), std::fs::read(f.lib("a.mkv")).unwrap()), (vec![b's'; 1000], vec![b'o'; 400]));
        let at = format!("the original is in the trash at {} and the AV1 file at {}", done.trash.display(), f.lib("a.mkv").display());
        assert!(why.contains(&at), "{why}");
        undo(&report, true).unwrap();
        assert!(f.lib("a.mp4").exists() && f.out("a.mkv").exists() && !says_replaced(&report));

        // The command says so in its exit status.
        let failed = run_watched(std::slice::from_ref(&f.dir), true, false, true, false, false, &unreadable).unwrap_err();
        std::fs::set_permissions(&report, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(failed.to_string().contains("1 report could not be updated"), "{failed:#}");
        assert!(f.lib("a.mkv").exists() && !f.lib("a.mp4").exists());
        crate::ledger::TEST_FOLDER.with(|d| *d.borrow_mut() = None);
    }

    /// Runs `act` as a run killed at `step`: it stops there, and what it changed until then stays.
    fn stopped_at(step: &'static str, act: impl FnOnce()) {
        crate::signals::STOP_AT.set(Some(step));
        let stopped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(act));
        crate::signals::STOP_AT.set(None);
        let why = stopped.expect_err("the run went past the step it was to be stopped at");
        assert_eq!(why.downcast_ref::<String>().map(String::as_str), Some(format!("stopped at {step}").as_str()));
    }

    /// A pair as `encode` leaves it, on a record of the fixture's own: `a.mp4` in the library and
    /// the AV1 file `av1` (`a.mkv`, or `a.mp4` to take the original's very name) with its report
    /// in `out`, the fixture's output folder or, when `far`, one in `/dev/shm` (another drive).
    /// None where `/dev/shm` is no other drive.
    struct Pair {
        f: Fix,
        report: PathBuf,
        out: PathBuf,
        av1: &'static str,
    }

    impl Pair {
        fn new(name: &str, far: bool, av1: &'static str) -> Option<Pair> {
            let f = Fix::new(name);
            record_in(&f);
            let mut report = f.encode("a.mp4", av1, 1000, 400);
            let mut out = f.dir.join("out");
            if far {
                let shm = Path::new("/dev/shm");
                if std::fs::metadata(shm).ok()?.dev() == std::fs::metadata(&f.dir).ok()?.dev() {
                    return None;
                }
                out = shm.join(format!("av1opt-replace-{name}-{}", std::process::id()));
                let _ = std::fs::remove_dir_all(&out);
                std::fs::create_dir_all(&out).ok()?;
                std::fs::copy(f.out(av1), out.join(av1)).unwrap();
                std::fs::remove_file(f.out(av1)).unwrap();
                let mut r = read_report(&report).unwrap();
                r["output"] = json!(out.join(av1));
                std::fs::remove_file(&report).unwrap();
                report = out.join("a.av1opt.json");
                std::fs::write(&report, serde_json::to_vec(&r).unwrap()).unwrap();
            }
            on_record(&report);
            Some(Pair { f, report, out, av1 })
        }

        /// `replace --apply`, or with `back` `replace --undo --apply`, on the two folders.
        fn command(&self, back: bool) {
            run_watched(&[self.out.clone(), self.f.dir.join("lib")], true, back, true, false, false, &|| {}).unwrap();
        }

        fn in_trash(&self) -> Vec<Vec<u8>> {
            let files = std::fs::read_dir(self.f.dir.join("Trash/files")).map(|d| d.flatten().map(|e| std::fs::read(e.path()).unwrap()).collect());
            files.unwrap_or_default()
        }

        /// The hidden copies a stopped run left, named as those of a run that has ended: in
        /// these tests it was this process, which is still there.
        fn as_if_its_run_had_ended(&self) {
            let mine = format!(".{}.", std::process::id());
            for dir in [self.f.dir.join("lib"), self.out.clone()] {
                for e in std::fs::read_dir(&dir).unwrap().flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if name.starts_with(STAGING_START) && name.contains(&mine) {
                        std::fs::rename(e.path(), dir.join(name.replace(&mine, ".999999999."))).unwrap();
                    }
                }
            }
        }

        /// The original whole, in its place or in the trash, and the AV1 file whole somewhere.
        fn nothing_lost(&self) {
            let original = vec![b's'; 1000];
            assert!(std::fs::read(self.f.lib("a.mp4")).ok().as_ref() == Some(&original) || self.in_trash().contains(&original), "the original is nowhere");
            let av1 = Some(vec![b'o'; 400]);
            assert!(std::fs::read(self.f.lib(self.av1)).ok() == av1 || std::fs::read(self.out.join(self.av1)).ok() == av1, "the AV1 file is nowhere");
        }

        /// As the encode left it, with nothing in the trash. A hidden copy on its way to the
        /// original's place may be left beside it by a replacement stopped before the original
        /// went to the trash: it is only ever a copy, and the next replacement clears it away.
        fn as_before(&self) {
            assert_eq!(std::fs::read(self.f.lib("a.mp4")).unwrap(), vec![b's'; 1000]);
            assert_eq!(std::fs::read(self.out.join(self.av1)).unwrap(), vec![b'o'; 400]);
            assert!(self.av1 == "a.mp4" || !self.f.lib(self.av1).exists());
            assert!(!says_replaced(&self.report));
            assert!(self.in_trash().is_empty());
            let lib: Vec<String> = std::fs::read_dir(self.f.dir.join("lib")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
            assert!(lib.iter().all(|n| n == "a.mp4" || staging_left_behind(n.as_ref(), &self.f.lib(self.av1))), "{lib:?}");
            assert!(matches!(check(&self.report), Check::Ready(_)), "{:?}", check(&self.report));
        }

        /// Replaced, the report and the record both saying so, and able to come back.
        fn as_replaced(&self) {
            assert_eq!(std::fs::read(self.f.lib(self.av1)).unwrap(), vec![b'o'; 400]);
            assert!((self.av1 == "a.mp4" || !self.f.lib("a.mp4").exists()) && !self.out.join(self.av1).exists());
            assert_eq!(self.in_trash(), vec![vec![b's'; 1000]]);
            assert_eq!(std::fs::read_dir(self.f.dir.join("lib")).unwrap().count(), 1);
            let Check::Replaced(d) = check(&self.report) else { panic!("{:?}", check(&self.report)) };
            assert!(d.can_undo);
            assert_eq!(crate::ledger::load().check_replaced(&d), Ok(()));
        }
    }

    impl Drop for Pair {
        fn drop(&mut self) {
            if self.out.starts_with("/dev/shm") {
                let _ = std::fs::remove_dir_all(&self.out);
            }
        }
    }

    /// Stops a replacement (or, when `undoing`, the undo of one) at `step`, then runs the command
    /// again, one way on one pair and the other way on another pair stopped alike:
    /// `replace --apply` leaves the pair replaced and `--undo --apply` as it was, and then the
    /// other one turns it round again. The original is whole somewhere all along.
    fn goes_on_after(step: &'static str, undoing: bool, far: bool) {
        goes_on_after_as(step, undoing, far, "a.mkv");
    }

    /// As [`goes_on_after`], for an AV1 file called `av1`.
    fn goes_on_after_as(step: &'static str, undoing: bool, far: bool, av1: &'static str) {
        for back in [false, true] {
            let name = format!("{}-{back}-{far}-{av1}", step.replace(':', "-"));
            let Some(p) = Pair::new(&name, far, av1) else { return crate::tools::skipped(&name, "/dev/shm is not another drive here") };
            if undoing {
                p.f.apply(&p.report).unwrap();
                p.as_replaced();
            }
            stopped_at(step, || drop(if undoing { undo(&p.report, true).map(drop) } else { p.f.apply(&p.report).map(drop) }));
            p.nothing_lost();
            p.as_if_its_run_had_ended();
            p.command(back);
            if back { p.as_before() } else { p.as_replaced() }
            p.command(!back);
            if back { p.as_replaced() } else { p.as_before() }
        }
        crate::ledger::TEST_FOLDER.with(|d| *d.borrow_mut() = None);
    }

    #[test]
    fn a_replacement_stopped_before_the_original_went_to_the_trash_is_done_or_left_the_next_time() {
        goes_on_after("trash:info", false, false);
    }

    #[test]
    fn a_replacement_stopped_with_the_original_in_the_trash_is_done_or_taken_back_the_next_time() {
        goes_on_after("replace:trashed", false, false);
    }

    #[test]
    fn a_replacement_stopped_with_both_files_moved_but_not_on_record_is_done_or_taken_back_the_next_time() {
        goes_on_after("replace:placed", false, false);
    }

    #[test]
    fn a_replacement_stopped_once_on_record_is_done_or_taken_back_the_next_time() {
        goes_on_after("replace:noted", false, false);
    }

    #[test]
    fn a_replacement_stopped_before_its_report_is_done_or_taken_back_the_next_time() {
        goes_on_after("replace:report", false, false);
    }

    #[test]
    fn an_undo_stopped_with_the_av1_file_back_but_the_original_in_the_trash_is_done_or_taken_back_the_next_time() {
        goes_on_after("undo:moved", true, false);
    }

    #[test]
    fn an_undo_stopped_with_both_files_back_but_not_on_record_is_done_or_taken_back_the_next_time() {
        goes_on_after("undo:restored", true, false);
    }

    #[test]
    fn an_undo_stopped_before_its_report_is_done_or_taken_back_the_next_time() {
        goes_on_after("undo:noted", true, false);
    }

    #[test]
    fn a_replacement_from_another_drive_stopped_with_its_copy_hidden_beside_the_original_is_done_or_left_the_next_time() {
        goes_on_after("replace:staged", false, true);
    }

    #[test]
    fn a_replacement_from_another_drive_stopped_with_the_original_in_the_trash_is_done_or_taken_back_the_next_time() {
        goes_on_after("replace:trashed", false, true);
    }

    #[test]
    fn a_replacement_from_another_drive_stopped_with_the_copy_in_place_but_not_on_record_is_done_or_taken_back_the_next_time() {
        goes_on_after("replace:placed", false, true);
    }

    #[test]
    fn a_replacement_from_another_drive_stopped_before_the_output_it_copied_was_removed_is_done_or_taken_back_the_next_time() {
        goes_on_after("replace:noted", false, true);
    }

    #[test]
    fn an_undo_to_another_drive_stopped_before_the_av1_file_it_copied_back_was_removed_is_done_or_taken_back_the_next_time() {
        goes_on_after("move:copied", true, true);
    }

    #[test]
    fn an_undo_to_another_drive_stopped_with_the_original_in_the_trash_is_done_or_taken_back_the_next_time() {
        goes_on_after("undo:moved", true, true);
    }

    #[test]
    fn an_undo_to_another_drive_stopped_with_both_files_back_but_not_on_record_is_done_or_taken_back_the_next_time() {
        goes_on_after("undo:restored", true, true);
    }

    #[test]
    fn a_replacement_in_the_originals_own_name_stopped_at_each_step_is_done_or_taken_back_the_next_time() {
        for step in ["trash:info", "replace:trashed", "replace:placed", "replace:noted", "replace:report"] {
            goes_on_after_as(step, false, false, "a.mp4");
        }
    }

    #[test]
    fn an_undo_of_a_replacement_in_the_originals_own_name_stopped_at_each_step_is_done_or_taken_back_the_next_time() {
        for step in ["undo:moved", "undo:restored", "undo:noted"] {
            goes_on_after_as(step, true, false, "a.mp4");
        }
    }

    #[test]
    fn a_replacement_in_the_originals_own_name_from_another_drive_stopped_at_each_step_is_done_or_taken_back_the_next_time() {
        for step in ["replace:staged", "trash:info", "replace:trashed", "replace:placed", "replace:noted", "replace:report"] {
            goes_on_after_as(step, false, true, "a.mp4");
        }
        for step in ["move:copied", "undo:moved", "undo:restored", "undo:noted"] {
            goes_on_after_as(step, true, true, "a.mp4");
        }
    }

    fn hand_trash(f: &Fix, name: &str) -> PathBuf {
        trash::trash_with(&f.lib(name), Some(&f.dir.join("Trash")), 1000, "2026-11-20T10:00:00".into()).unwrap()
    }

    /// No run of av1opt was stopped: the person threw the original away with the file manager
    /// and kept the AV1 file in the output folder. Nothing on record says a replacement was
    /// under way, so a listing does not say av1opt was stopped half-way, and an `--apply` over
    /// these folders leaves the file in the trash.
    #[test]
    fn an_original_the_person_threw_away_is_not_taken_for_a_replacement_stopped_half_way() {
        let p = Pair::new("proof-hand-trashed", false, "a.mkv").unwrap();
        let trashed = hand_trash(&p.f, "a.mp4");
        let r = read_report(&p.report).unwrap();
        let seen = CutOff::of(&p.report, &r, &crate::ledger::load(), false);
        // `replace --undo --apply`, meant for other pairs under these folders.
        p.command(true);
        let back = p.f.lib("a.mp4").exists();
        crate::ledger::TEST_FOLDER.with(|d| *d.borrow_mut() = None);
        assert!(seen.is_none(), "a listing says: {:?}", seen.unwrap().told());
        assert!(!back && trashed.exists(), "the original the person threw away is back in the library");
    }

    /// The same, with plain `replace --apply`: the pair stays as it is ("the original is gone"),
    /// the AV1 file in the output folder.
    #[test]
    fn an_output_whose_original_was_thrown_away_stays_in_the_output_folder() {
        let p = Pair::new("proof-hand-trashed-apply", false, "a.mkv").unwrap();
        hand_trash(&p.f, "a.mp4");
        p.command(false);
        let (in_out, in_lib) = (p.out.join("a.mkv").exists(), p.f.lib("a.mkv").exists());
        crate::ledger::TEST_FOLDER.with(|d| *d.borrow_mut() = None);
        assert!(in_out && !in_lib, "the AV1 file left the output folder for the library: before this branch the pair stayed as it was (\"the original is gone\")");
    }

    /// Two encodes of one original, in two output folders and two containers. The first
    /// replaced the original, start to finish. The second's report, which says nothing was
    /// replaced, is not taken for a replacement stopped half-way, and the original stays in
    /// the first one's trash entry, from where the first replacement can be undone.
    #[test]
    fn a_second_output_of_an_original_leaves_the_replacement_by_the_first_alone() {
        let f = Fix::new("proof-two-outputs");
        record_in(&f);
        let first = f.encode("a.mp4", "a.mkv", 1000, 400);
        on_record(&first);
        let out2 = f.dir.join("out2");
        std::fs::create_dir_all(&out2).unwrap();
        std::fs::write(out2.join("a.mp4"), vec![b'p'; 300]).unwrap();
        let mut r = read_report(&first).unwrap();
        r["output"] = json!(out2.join("a.mp4"));
        r["output_bytes"] = json!(300);
        let second = out2.join("a.av1opt.json");
        std::fs::write(&second, serde_json::to_vec(&r).unwrap()).unwrap();
        on_record(&second);
        f.apply(&first).unwrap();
        assert!(matches!(check(&first), Check::Replaced(d) if d.can_undo && crate::ledger::load().check_replaced(&d).is_ok()));
        // `replace --undo --apply` on the second output's folder and the library.
        run_watched(&[out2.clone(), f.dir.join("lib")], true, true, true, false, false, &|| {}).unwrap();
        let first_now = check(&first);
        let original_back = f.lib("a.mp4").exists();
        crate::ledger::TEST_FOLDER.with(|d| *d.borrow_mut() = None);
        assert!(!original_back, "the original left the trash entry of a replacement that had gone through");
        assert!(matches!(first_now, Check::Replaced(ref d) if d.can_undo), "{first_now:?}");
    }

    /// A replacement on ZFS (made with --allow-zfs) was stopped. The next `replace --apply`
    /// without --allow-zfs leaves it as it is: putting it right writes to that folder.
    #[test]
    fn nothing_of_a_stopped_replacement_is_put_back_on_zfs_without_allow_zfs() {
        let p = Pair::new("proof-zfs", false, "a.mkv").unwrap();
        stopped_at("replace:trashed", || drop(p.f.apply(&p.report)));
        assert!(!p.f.lib("a.mp4").exists());
        crate::fsguard::ZFS_IN_TESTS.with_borrow_mut(|z| z.push(p.f.dir.join("lib")));
        let _ = run_watched(&[p.out.clone(), p.f.dir.join("lib")], true, false, false, false, false, &|| {});
        crate::fsguard::ZFS_IN_TESTS.with_borrow_mut(Vec::clear);
        let back = p.f.lib("a.mp4").exists();
        crate::ledger::TEST_FOLDER.with(|d| *d.borrow_mut() = None);
        assert!(!back, "a file was moved into a folder on ZFS by a run without --allow-zfs");
    }

    /// The same for an undo stopped half-way: the original is not put back into the folder on
    /// ZFS by a run without --allow-zfs.
    #[test]
    fn nothing_of_a_stopped_undo_is_moved_onto_zfs_without_allow_zfs() {
        let p = Pair::new("proof-zfs-undo", false, "a.mkv").unwrap();
        p.f.apply(&p.report).unwrap();
        stopped_at("undo:moved", || drop(undo(&p.report, true)));
        assert!(!p.f.lib("a.mkv").exists() && p.out.join("a.mkv").exists());
        crate::fsguard::ZFS_IN_TESTS.with_borrow_mut(|z| z.push(p.f.dir.join("lib")));
        let _ = run_watched(&[p.out.clone(), p.f.dir.join("lib")], true, true, false, false, false, &|| {});
        crate::fsguard::ZFS_IN_TESTS.with_borrow_mut(Vec::clear);
        let moved_in = p.f.lib("a.mkv").exists();
        crate::ledger::TEST_FOLDER.with(|d| *d.borrow_mut() = None);
        assert!(!moved_in, "the AV1 file was moved into a folder on ZFS by a run without --allow-zfs");
    }

    /// What the page calls for "Put the original back" is `undo_if` on that one report. An undo
    /// to another drive that was stopped once its copy was made is still listed as one that can
    /// be undone, and the page's undo puts that right first and then goes on.
    #[test]
    fn the_pages_undo_goes_on_after_an_undo_stopped_once_its_copy_was_made() {
        let Some(p) = Pair::new("proof-page-undo", true, "a.mkv") else { return crate::tools::skipped("proof-page-undo", "/dev/shm is not another drive here") };
        p.f.apply(&p.report).unwrap();
        stopped_at("move:copied", || drop(undo(&p.report, true)));
        p.as_if_its_run_had_ended();
        let ledger = crate::ledger::load();
        assert!(matches!(check_for_list_on_record(&p.report, &ledger), Check::Replaced(d) if d.can_undo), "the page offers the undo");
        let done = undo_if(&p.report, true, &|d| crate::ledger::load().check_replaced(d)).map(drop).map_err(|e| format!("{e:#}"));
        crate::ledger::TEST_FOLDER.with(|d| *d.borrow_mut() = None);
        assert_eq!(done, Ok(()));
    }

    /// What the page shows of the other half-finished states: it has a button only for `ready`
    /// and for `replaced` with `can_undo` (ui/app.js), so these pairs can be put right from the
    /// command line alone, and the page says of an original that is in the trash that it is gone.
    #[test]
    #[ignore = "review proof: fails on crash-points aa34f6e until the fault is fixed"]
    fn proof_the_page_has_a_way_to_put_right_what_was_stopped_half_way() {
        let mut stuck = Vec::new();
        for (step, undoing) in [("replace:trashed", false), ("replace:placed", false), ("undo:moved", true), ("undo:restored", true)] {
            let p = Pair::new(&format!("proof-page-{}", step.replace(':', "-")), false, "a.mkv").unwrap();
            if undoing {
                p.f.apply(&p.report).unwrap();
            }
            stopped_at(step, || drop(if undoing { undo(&p.report, true).map(drop) } else { p.f.apply(&p.report).map(drop) }));
            let ledger = crate::ledger::load();
            match check_for_list_on_record(&p.report, &ledger) {
                Check::Ready(_) => {}
                Check::Replaced(d) if d.can_undo => {}
                Check::Replaced(_) => stuck.push(format!("{step}: listed as replaced and not to be undone")),
                Check::No(why) => stuck.push(format!("{step}: listed as \"{why}\"")),
            }
        }
        crate::ledger::TEST_FOLDER.with(|d| *d.borrow_mut() = None);
        assert!(stuck.is_empty(), "{stuck:#?}");
    }

    /// The output folder is the original's own folder, reached by a second path (a folder
    /// mounted twice; here a link, on a record line written by hand, since `encode` resolves
    /// links), and the person threw the original away. `from` and `dest` are one file under two
    /// names, and the only AV1 file stays.
    #[test]
    fn an_av1_file_reached_by_a_second_path_is_not_taken_for_its_own_copy() {
        let f = Fix::new("proof-one-file-two-names");
        record_in(&f);
        let made = f.encode("a.mp4", "a.mkv", 1000, 400);
        std::fs::rename(f.out("a.mkv"), f.lib("a.mkv")).unwrap();
        std::os::unix::fs::symlink(f.dir.join("lib"), f.dir.join("way")).unwrap();
        let mut r = read_report(&made).unwrap();
        r["output"] = json!(f.dir.join("way/a.mkv"));
        std::fs::remove_file(&made).unwrap();
        let report = f.lib("a.av1opt.json");
        std::fs::write(&report, serde_json::to_vec(&r).unwrap()).unwrap();
        on_record(&report);
        hand_trash(&f, "a.mp4");
        let _ = run_watched(&[f.dir.join("lib")], true, true, true, false, false, &|| {});
        let av1 = std::fs::read(f.lib("a.mkv")).ok();
        crate::ledger::TEST_FOLDER.with(|d| *d.borrow_mut() = None);
        assert_eq!(av1, Some(vec![b'o'; 400]), "the AV1 file is gone: it was removed as a copy of itself");
    }

    /// `replace` on the command line takes pairs that are not on av1opt's record (reports from
    /// before the record, never adopted), and such a replacement can be stopped half-way like
    /// any other. Its line on record is written all the same, and it is put right.
    #[test]
    fn a_replacement_of_a_pair_not_on_record_stopped_half_way_is_put_right() {
        let f = Fix::new("proof-not-on-record");
        record_in(&f);
        let report = f.encode("a.mp4", "a.mkv", 1000, 400);
        stopped_at("replace:trashed", || drop(f.apply(&report)));
        assert!(!f.lib("a.mp4").exists());
        let named = [f.dir.join("out"), f.dir.join("lib")];
        run_watched(&named, true, true, true, false, false, &|| {}).unwrap();
        let (back, av1) = (std::fs::read(f.lib("a.mp4")).ok(), std::fs::read(f.out("a.mkv")).ok());
        let listed = check(&report);
        crate::ledger::TEST_FOLDER.with(|d| *d.borrow_mut() = None);
        assert_eq!((back, av1), (Some(vec![b's'; 1000]), Some(vec![b'o'; 400])));
        assert!(matches!(listed, Check::Ready(_)), "{listed:?}");
    }

    /// The recovery is itself stopped at each of its own moves, from each state it puts right,
    /// and run again, either way: nothing is lost, and the pair ends as asked.
    #[test]
    fn a_recovery_that_is_itself_stopped_at_each_of_its_moves_is_finished_by_the_next_run() {
        let cases: [(&str, bool, bool, &[&'static str]); 7] = [
            ("replace:trashed", false, false, &["recover:restored"]),
            ("replace:placed", false, false, &["recover:moved", "recover:restored"]),
            ("replace:placed", false, true, &["recover:hidden", "recover:moved", "recover:restored"]),
            ("replace:removed", false, true, &["move:copied", "recover:moved", "recover:restored"]),
            ("move:copied", true, true, &["recover:hidden", "recover:moved"]),
            ("undo:moved", true, false, &["recover:restored", "recover:noted"]),
            ("undo:restored", true, true, &["recover:noted"]),
        ];
        for (step, undoing, far, inside) in cases {
            for &stop in inside {
                for back in [false, true] {
                    let name = format!("recovery-{}-{far}-{}-{back}", step.replace(':', "-"), stop.replace(':', "-"));
                    let Some(p) = Pair::new(&name, far, "a.mkv") else { return crate::tools::skipped(&name, "/dev/shm is not another drive here") };
                    if undoing {
                        p.f.apply(&p.report).unwrap();
                    }
                    stopped_at(step, || drop(if undoing { undo(&p.report, true).map(drop) } else { p.f.apply(&p.report).map(drop) }));
                    p.as_if_its_run_had_ended();
                    stopped_at(stop, || p.command(back));
                    p.nothing_lost();
                    p.as_if_its_run_had_ended();
                    p.command(back);
                    if back { p.as_before() } else { p.as_replaced() }
                    p.command(!back);
                    if back { p.as_replaced() } else { p.as_before() }
                }
            }
        }
        crate::ledger::TEST_FOLDER.with(|d| *d.borrow_mut() = None);
    }

    #[test]
    fn a_replacement_stopped_right_after_its_line_on_record_has_moved_nothing_and_goes_on_the_next_time() {
        goes_on_after("replace:begun", false, false);
        goes_on_after("replace:begun", false, true);
    }

    #[test]
    fn an_undo_stopped_right_after_its_line_on_record_has_moved_nothing_and_goes_on_the_next_time() {
        goes_on_after("undo:begun", true, false);
        goes_on_after("undo:begun", true, true);
    }

    /// Stopped once the original's name in the trash was taken, before it went there: the info
    /// file of that name, with no file, is this run's, and goes; one of another name stays.
    #[test]
    fn an_info_file_a_stopped_replacement_left_in_the_trash_goes_and_no_other() {
        let p = Pair::new("orphan-info", false, "a.mkv").unwrap();
        stopped_at("trash:info", || drop(p.f.apply(&p.report)));
        let info = p.f.dir.join("Trash/info");
        assert!(info.join("a.mp4.trashinfo").exists());
        std::fs::write(info.join("b.mp4.trashinfo"), format!("[Trash Info]\nPath={}\nDeletionDate=2026-11-20T10:00:00\n", p.f.lib("b.mp4").display())).unwrap();
        p.command(true);
        let left: Vec<String> = std::fs::read_dir(&info).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        crate::ledger::TEST_FOLDER.with(|d| *d.borrow_mut() = None);
        assert_eq!(left, ["b.mp4.trashinfo"]);
        p.as_before();
    }

    /// The one file at both names (a folder mounted twice: here a link to the folder stands in,
    /// since a hard link is refused before this is reached) is not removed as a copy of itself.
    #[test]
    fn a_file_is_not_removed_as_the_copy_of_itself_under_another_name() {
        let f = Fix::new("one-file-two-names");
        std::fs::write(f.lib("a.mkv"), vec![b'o'; 400]).unwrap();
        std::os::unix::fs::symlink(f.dir.join("lib"), f.dir.join("way")).unwrap();
        let hash = crate::ledger::sha256_of_file(&f.lib("a.mkv")).unwrap();
        let e = remove_copy(&f.dir.join("way/a.mkv"), &f.lib("a.mkv"), 400, &hash).unwrap_err();
        assert!(format!("{e:#}").contains("one file under two names"), "{e:#}");
        assert_eq!(std::fs::read(f.lib("a.mkv")).unwrap(), vec![b'o'; 400]);
        // A hard link is no copy either.
        std::fs::hard_link(f.lib("a.mkv"), f.out("a.mkv")).unwrap();
        assert!(remove_copy(&f.out("a.mkv"), &f.lib("a.mkv"), 400, &hash).is_err());
        assert!(f.out("a.mkv").exists() && f.lib("a.mkv").exists());
        // A real copy goes, and only it.
        std::fs::remove_file(f.out("a.mkv")).unwrap();
        std::fs::write(f.out("a.mkv"), vec![b'o'; 400]).unwrap();
        remove_copy(&f.out("a.mkv"), &f.lib("a.mkv"), 400, &hash).unwrap();
        assert!(!f.out("a.mkv").exists() && f.lib("a.mkv").exists());
        assert_eq!(std::fs::read_dir(f.dir.join("out")).unwrap().count(), 0);
    }

    /// A pair stopped half-way that cannot be put right (here: the original's folder cannot be
    /// written to) is named, and the run ends in failure, with the files as they were.
    #[test]
    fn a_pair_that_could_not_be_put_right_is_named_and_fails_the_run() {
        use std::os::unix::fs::PermissionsExt;
        let p = Pair::new("not-put-right", false, "a.mkv").unwrap();
        stopped_at("replace:trashed", || drop(p.f.apply(&p.report)));
        let lib = p.f.dir.join("lib");
        std::fs::set_permissions(&lib, std::fs::Permissions::from_mode(0o555)).unwrap();
        let done = run_watched(&[p.out.clone(), lib.clone()], true, true, true, false, false, &|| {}).map_err(|e| format!("{e:#}"));
        std::fs::set_permissions(&lib, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.nothing_lost();
        assert!(done.as_ref().is_err_and(|e| e.contains("1 pair stopped half-way could not be put right")), "{done:?}");
        p.command(true);
        crate::ledger::TEST_FOLDER.with(|d| *d.borrow_mut() = None);
        p.as_before();
    }

    /// A report rewritten while the pair is being put right is a new file under the report's
    /// name: it is locked before it takes the name, so that no other run can take the pair then.
    #[test]
    fn a_report_set_right_stays_locked_while_the_pair_is_worked_on() {
        let p = Pair::new("locked-after", false, "a.mkv").unwrap();
        stopped_at("replace:noted", || drop(p.f.apply(&p.report)));
        let mut held = vec![lock(&p.report).unwrap()];
        assert_eq!(set_right(&p.report, true, &mut held).unwrap(), Some(Stranded::Replaced));
        let second = lock(&p.report).map(drop).map_err(|e| e.to_string());
        drop(held);
        crate::ledger::TEST_FOLDER.with(|d| *d.borrow_mut() = None);
        assert_eq!(second, Err("this file is being replaced or put back right now".to_string()));
    }

    /// A lock let go of is still held by a program another thread is starting until it has
    /// started (here another descriptor of the report, let go of a little later): the next run
    /// waits for it rather than say the pair is busy. With every test of this program in one
    /// process, starting tools as they go, runs one after the other on one pair were refused.
    #[test]
    fn a_lock_let_go_of_a_moment_ago_is_waited_for() {
        use std::os::fd::AsRawFd;
        let f = Fix::new("lock-let-go");
        let report = f.encode("a.mp4", "a.mkv", 1000, 400);
        let other = std::fs::File::open(&report).unwrap();
        // SAFETY: flock on a descriptor the test owns.
        assert_eq!(unsafe { libc::flock(other.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
        let let_go = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            drop(other);
        });
        let taken = lock(&report).map(drop).map_err(|e| e.to_string());
        let_go.join().unwrap();
        assert_eq!(taken, Ok(()));
    }
}
