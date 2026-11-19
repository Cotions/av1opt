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
    Ok(p)
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
}

const CHANGED_WHILE_CHECKED: &str = "the original changed while it was being checked";

/// Looks at one report and the two files it names. Changes nothing. May read the whole original.
pub fn check(report: &Path) -> Check {
    check_judging(report, Judging::default())
}

/// [`check`] for a list of reports, which does not read whole originals: one that would have to
/// be read is listed as ready, and replacing it checks it in full.
#[cfg(any(feature = "ui", test))]
pub fn check_for_list(report: &Path) -> Check {
    check_judging(report, Judging { for_list: true, ..Judging::default() })
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
        let in_trash = if judging.for_list { trash::holds_for_list(&trash, &source, &original) } else { trash::holds(&trash, &source, &original) };
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
    trash::rename_new(from, to)?;
    if file.is_at(to) {
        return Ok(());
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
    let date = std::fs::metadata(report).and_then(|m| m.modified()).ok();
    let part = crate::files::save_name(report);
    let write = || -> Result<()> {
        // (Made anew: a link left at that name is not written through.)
        let _ = std::fs::remove_file(&part);
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&part)?;
        std::io::Write::write_all(&mut f, &serde_json::to_vec_pretty(r)?)?;
        if let Some(date) = date {
            let _ = f.set_modified(date);
        }
        f.sync_all()?;
        Ok(std::fs::rename(&part, report)?)
    };
    write().inspect_err(|_| drop(std::fs::remove_file(&part)))
}

/// Held while the files of a report are moved, so that two runs (two clicks, or the command
/// and the page at once) never work on the same pair. Gone when the returned lock is dropped.
fn lock(report: &Path) -> Result<Lock> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    // Without waiting: a pipe left under a report's name would never finish opening.
    let f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(report).with_context(|| format!("no report at {}", report.display()))?;
    // SAFETY: flock on a descriptor we own; no memory is involved.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0
        && std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock
    {
        bail!("this file is being replaced or put back right now");
    }
    // A filesystem without locks: carry on as a single run would.
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
    apply_judging(report, allow_zfs, Judging::default(), &trash::trash, accept)
}

/// [`check_for_list`] on behalf of someone who says that only the original's date changed, as
/// `--accept-changed-date` does: what the report cannot tell about counts as the same.
#[cfg(feature = "ui")]
pub fn check_for_list_accepting_changed_date(report: &Path) -> Check {
    check_judging(report, Judging { for_list: true, accept_changed_date: true, ..Judging::default() })
}

/// [`apply_if`] on behalf of someone who says that only the original's date changed. An original
/// known to differ is still refused.
#[cfg(feature = "ui")]
pub fn apply_if_date_accepted(report: &Path, allow_zfs: bool, accept: AcceptOutput) -> Result<Done> {
    apply_judging(report, allow_zfs, Judging { accept_changed_date: true, ..Judging::default() }, &trash::trash, accept)
}

#[cfg(test)]
fn apply_with(report: &Path, allow_zfs: bool, to_trash: &dyn Fn(&Path) -> Result<PathBuf>) -> Result<Done> {
    apply_judging(report, allow_zfs, Judging::default(), to_trash, &|_, _| Ok(()))
}

/// Has the last word on what a report comes to, or says why not.
type Accept<'a, T> = &'a dyn Fn(&T) -> Result<(), String>;

/// [`Accept`] for a replacement, with the output as it was opened to be moved.
type AcceptOutput<'a> = &'a dyn Fn(&Plan, &OpenFile) -> Result<(), String>;

fn apply_judging(report: &Path, allow_zfs: bool, judging: Judging, to_trash: &dyn Fn(&Path) -> Result<PathBuf>, accept: AcceptOutput) -> Result<Done> {
    let _lock = lock(report)?;
    let reads_of_this_one = WholeReads::default();
    let judging = Judging { acting: true, remember: judging.remember.or(Some(&reads_of_this_one)), ..judging };
    let p = match check_judging(report, judging) {
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
    }
    // Reading a large original takes minutes, and a write behind the place being read would not
    // have shown in what was read.
    if !std::fs::symlink_metadata(&p.source).is_ok_and(|m| Stamp::of(&m) == source_stamp) {
        if staged_first.is_some() {
            let _ = std::fs::remove_file(&staged);
        }
        bail!("{CHANGED_WHILE_CHECKED}, so nothing was changed");
    }
    let trashed = match to_trash(&p.source) {
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

    // From here on both files are where they belong; what follows is tidying.
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
        }
    }
    let source_mtime = std::fs::symlink_metadata(&trashed).ok().and_then(|m| crate::source::modified_seconds(&m));
    let went = Recorded { modified: source_mtime.or(p.original.modified), ..p.original.clone() };
    let move_noted = crate::ledger::note_replaced(&crate::ledger::Replacement { original: &p.source, trash: &trashed, output: &p.dest, from: &p.output, source_bytes: p.source_bytes, known: &went, output_bytes: p.output_bytes });
    if let Err(e) = move_noted {
        warnings.push(format!("av1opt's own record of this could not be written ({e:#}), so the page will not offer to put the original back; `av1opt replace --undo` in a terminal can"));
    }
    let noted = read_report(report).map_err(|e| anyhow!(e)).and_then(|mut r| {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        r["output"] = json!(p.dest);
        r["replaced"] = json!({ "time": now, "from": p.output, "trash": trashed, "source_mtime": source_mtime });
        write_report(report, &r)
    });
    if let Err(e) = noted {
        warnings.push(format!("the report could not be updated ({e:#}); the original is in the trash at {}", trashed.display()));
    }
    Ok(Done { dest: p.dest, trash: trashed, source_bytes: p.source_bytes, warnings })
}

/// Undoes `apply` while the original is still in the trash: the AV1 file goes back where it
/// was, the original back to its place, and the report forgets the replacement.
#[cfg(test)]
fn undo(report: &Path, allow_zfs: bool) -> Result<Replaced> {
    undo_if(report, allow_zfs, &|_| Ok(()))
}

/// [`undo`], if `accept` agrees to what the report comes to, asked as [`apply_if`] asks.
pub fn undo_if(report: &Path, allow_zfs: bool, accept: Accept<Replaced>) -> Result<Replaced> {
    undo_watched(report, allow_zfs, accept, &|| {})
}

/// [`undo_if`], with `before_moving` called when everything has been checked and nothing moved.
fn undo_watched(report: &Path, allow_zfs: bool, accept: Accept<Replaced>, before_moving: &dyn Fn()) -> Result<Replaced> {
    let _lock = lock(report)?;
    let d = match check(report) {
        Check::Replaced(d) => d,
        Check::Ready(_) => bail!("this original has not been replaced, so there is nothing to put back"),
        Check::No(why) => bail!("{why}"),
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
    // Opened before it is judged, so that the file judged can be told from one put there later.
    let original = OpenFile::open(&d.trash, d.source_bytes).context("the file in the trash is not the original")?;
    if !trash::is_file_recorded(&d.trash, &d.original) {
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
    let moved = if d.from == d.dest { None } else { Some(move_open(&av1, &d.dest, &d.from, d.output_bytes)?) };
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
    sync_dir(d.source.parent().unwrap_or(Path::new("/")));
    if let Err(e) = crate::ledger::note_undone(&d.source, &d.dest, &d.from) {
        eprintln!("warning: the original is back, but av1opt's own record of that could not be written: {e:#}");
    }
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
    write_report(report, &r).context("both files are back, but the report could not be updated")?;
    Ok(d)
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
    use crate::report::gb;
    // Only what is going to be replaced is worth keeping reads for.
    let reads = WholeReads::default();
    let judging = Judging { accept_changed_date, on_command_line: true, remember: go.then_some(&reads), ..Judging::default() };
    let reports = reports_for(paths);
    if reports.is_empty() {
        bail!("no av1opt reports (*.av1opt.json) found there");
    }
    let named = NamedFolders::of(paths, anywhere);
    let ledger = crate::ledger::load();
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
    let mut failed = 0;

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
            match apply_judging(&p.report, allow_zfs, judging, &trash::trash, &accept) {
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

fn found_for_adoption(report: &Path, judging: Judging, ledger: &crate::ledger::Ledger) -> Found {
    use crate::ledger::Standing;
    match std::fs::symlink_metadata(report) {
        Ok(m) if m.file_type().is_symlink() => return Found::Refused("the report is a link".into()),
        Ok(m) if !m.is_file() => return Found::Refused("the report is not a plain file".into()),
        _ => {}
    }
    match check_judging(report, judging) {
        Check::No(why) => Found::Refused(why),
        Check::Ready(p) => match ledger.adopting(&p) {
            Standing::NotYet => Found::Ready(p),
            Standing::OnRecord => Found::OnRecord,
            Standing::Refused(why) => Found::Refused(why),
        },
        Check::Replaced(d) => match ledger.adopting_replaced(&d) {
            Standing::NotYet => why_it_cannot_come_back(&d).map_or(Found::Replaced(d), Found::Refused),
            Standing::OnRecord => Found::OnRecord,
            Standing::Refused(why) => Found::Refused(why),
        },
    }
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
fn adopt_one(report: &Path, reads: &WholeReads) -> Result<bool> {
    use crate::ledger::{AdoptedReplacement, Adoption, note_adopted};
    let _lock = lock(report)?;
    let judging = Judging { remember: Some(reads), acting: true, ..Judging::default() };
    match found_for_adoption(report, judging, &crate::ledger::load()) {
        Found::Refused(why) => bail!("{why}"),
        Found::OnRecord => return Ok(false),
        Found::Ready(p) => {
            let output_sha256 = hash_of_output(&p.output, p.output_bytes)?;
            let stamp = p.source_stamp.context(CHANGED_WHILE_CHECKED)?;
            if !std::fs::symlink_metadata(&p.source).is_ok_and(|m| Stamp::of(&m) == stamp) {
                bail!("{CHANGED_WHILE_CHECKED}");
            }
            note_adopted(Adoption { output: &p.output, report, source: &p.source, source_bytes: p.source_bytes, original: &p.original, output_bytes: p.output_bytes, output_sha256, replaced: None })?;
        }
        Found::Replaced(d) => {
            let output_sha256 = hash_of_output(&d.dest, d.output_bytes)?;
            // The report says how the original was when it was encoded, which is what it has to
            // match if the replacement is undone and the output offered again.
            let encoded = Recorded::in_report(&read_report(report).map_err(|why| anyhow!(why))?);
            let replaced = Some(AdoptedReplacement { trash: &d.trash, from: &d.recorded_from, known: &d.original });
            note_adopted(Adoption { output: &d.dest, report, source: &d.source, source_bytes: d.source_bytes, original: &encoded, output_bytes: d.output_bytes, output_sha256, replaced })?;
        }
    }
    Ok(true)
}

/// The `adopt` command: lists which outputs under `paths` can go on av1opt's record, and with `go`
/// writes them there. Nothing but the record is ever written.
pub fn adopt(paths: &[PathBuf], go: bool) -> Result<()> {
    println!("Naming a folder here says that you trust the reports in it, as `av1opt replace` on it does: what they say goes on av1opt's private record, and the page then moves these files on the strength of it.");
    let reports = reports_found(paths, &|dirs| report_names(dirs, &|e| !e.file_type().is_dir()));
    if reports.is_empty() {
        bail!("no av1opt reports (*.av1opt.json) found there");
    }
    crate::ledger::make_ready().context("av1opt's record of its own encodes cannot be used, so nothing can be taken on it")?;
    let ledger = crate::ledger::load();
    if let Some(why) = ledger.problem() {
        bail!("av1opt's record of its own encodes cannot be used ({why}), so nothing can be taken on it");
    }
    let reads = WholeReads::default();
    let judging = Judging { remember: go.then_some(&reads), ..Judging::default() };
    let (mut ready, mut replaced, mut known, mut refused) = (Vec::new(), Vec::new(), 0, Vec::new());
    for report in &reports {
        match found_for_adoption(report, judging, &ledger) {
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
        match adopt_one(report, &reads) {
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
            let r = json!({ "format": 1, "input": s, "output": o, "source_bytes": source_len, "output_bytes": output_len, "crf": 30.0 });
            std::fs::write(&report, serde_json::to_vec_pretty(&r).unwrap()).unwrap();
            report
        }

        fn apply(&self, report: &Path) -> Result<Done> {
            let home = self.dir.join("Trash");
            apply_with(report, true, &|p| trash::trash_with(p, Some(&home), 1000, "2026-11-10T22:15:03".into()))
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
        let err = apply_judging(&report, true, Judging::default(), &trash::trash, &|_, opened| {
            *hash_seen.borrow_mut() = opened.sha256().unwrap();
            swap_for_another_file(&output);
            Ok(())
        })
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
        let err = apply_judging(&report, true, Judging::default(), &trash::trash, &|_, _| {
            std::fs::write(&output, vec![b'e'; 400]).unwrap();
            Ok(())
        })
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
        let err = apply_with(&report, true, &|p| {
            let t = trash::trash_with(p, Some(&home(&f)), 1000, "2026-11-10T22:15:03".into())?;
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
        let err = apply_with(&report, true, &|p| {
            let t = trash::trash_with(p, Some(&home(&f)), 1000, "2026-11-10T22:15:03".into())?;
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
            let err = undo_watched(&report, true, &|_| Ok(()), &|| swap(&f, &done.trash)).unwrap_err();
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
        let err = undo_watched(&report, true, &|_| Ok(()), &|| std::fs::write(f.lib("a.mp4"), b"newcomer").unwrap()).unwrap_err();
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
        let err = apply_judging(&report, true, Judging::default(), &trash::trash, &|_, _| {
            std::os::unix::fs::FileExt::write_all_at(&File::options().write(true).open(&source).unwrap(), b"x", 999).unwrap();
            Ok(())
        })
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
        let err = apply_with(&report, true, &|p| {
            std::os::unix::fs::FileExt::write_all_at(&File::options().write(true).open(p).unwrap(), b"x", 999).unwrap();
            trash::trash_with(p, Some(&home), 1000, "2026-11-10T22:15:03".into())
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
            apply_judging(report, true, judging, &|p| trash::trash_with(p, Some(&home(f)), 1000, "2026-11-10T22:15:03".into()), &|_, _| Ok(()))
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
        apply_judging(&report, true, judging, &|p| trash::trash_with(p, Some(&home(&f)), 1000, "2026-11-10T22:15:03".into()), &|_, _| Ok(())).unwrap();
        assert_eq!(reads.reads_made(), 2);
    }

    #[test]
    fn what_is_refused_at_the_last_moment_moves_nothing() {
        let f = Fix::new("refused");
        let report = f.encode("a.mp4", "a.mkv", 1000, 400);
        let err = apply_judging(&report, true, Judging::default(), &trash::trash, &|_, _| Err("not this one".into())).unwrap_err();
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
        // One that holds neither goes by the size, as it always did.
        rewrite_report(json!({}));
        assert!(matches!(check(&report), Check::Ready(_)));

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
        old["source_mtime"] = json!(1_600_000_060u64);
        write_report(&old);
        assert!(can_undo(), "the time on record is that of the file there");
        old["source_mtime"] = json!(1_600_000_000u64);
        old.as_object_mut().unwrap().remove("source_fingerprint");
        write_report(&old);
        assert!(!can_undo(), "another time and no fingerprint to settle it");

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
        let err = apply_with(&report, false, &|_| bail!("no trash on this drive")).unwrap_err();
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
        let err = apply_with(&report, false, &|p| {
            let t = trash::trash_with(p, Some(&home), 1000, "2026-11-10T22:15:03".into())?;
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
        // A process number no process can have, and this very process.
        let dead = u32::MAX;
        let ours = staging_name(&dest);
        let others = [".av1opt-film.mkv.part".to_string(), format!(".av1opt-film.mkv.{dead}.3.part"), format!(".av1opt-film.{dead}.3.part")];
        let keep = [format!(".av1opt-other.mkv.{dead}.3.part"), ".av1opt-film.mkv.notes".to_string(), "film.mkv.part".to_string()];
        std::fs::write(&ours, b"being written").unwrap();
        for name in others.iter().chain(&keep) {
            std::fs::write(f.lib(name), b"x").unwrap();
        }
        clear_left_behind(&dest);
        assert!(ours.exists(), "the copy of this run stays");
        assert!(others.iter().all(|n| !f.lib(n).exists()), "copies of runs that are gone");
        assert!(keep.iter().all(|n| f.lib(n).exists()), "files that are something else");
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
    /// a test can count on; where it isn't one, there is nothing to check.
    #[test]
    fn outputs_on_another_drive_are_copied_then_removed() {
        let f = Fix::new("copy");
        let shm = Path::new("/dev/shm");
        let (Ok(a), Ok(b)) = (std::fs::metadata(shm), std::fs::metadata(&f.dir)) else { return };
        let out = shm.join(format!("av1opt-replace-test-{}", std::process::id()));
        if a.dev() == b.dev() || std::fs::create_dir_all(&out).is_err() {
            return;
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
        assert!(apply_with(&report, false, &|_| bail!("no trash on this drive")).is_err());
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
}
