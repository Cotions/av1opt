//! Puts finished AV1 outputs where their originals were. The original goes to the trash (see
//! `trash`), the output takes its name (with the output's extension) in the original's folder,
//! and the report records where both went, so `undo` can put everything back while the
//! original is still in the trash.
//!
//! Nothing here deletes an original, and no file is ever written over: every move refuses to
//! land on an existing file. Space is only freed when the trash is emptied.

use crate::encode::REPORT_FORMAT;
use crate::trash;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const REPORT_SUFFIX: &str = ".av1opt.json";

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
}

/// An original that was replaced earlier, as its report tells it.
#[derive(Debug, Clone, PartialEq)]
pub struct Replaced {
    pub report: PathBuf,
    pub source: PathBuf,
    /// Where the AV1 file is now.
    pub dest: PathBuf,
    /// Where the AV1 file was before.
    pub from: PathBuf,
    /// Where the original is in the trash.
    pub trash: PathBuf,
    pub source_bytes: u64,
    pub output_bytes: u64,
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
    let bytes = std::fs::read(report).map_err(|e| format!("no report ({e})"))?;
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
    let folder = |p: &Path| std::path::absolute(p).ok().and_then(|p| std::fs::canonicalize(p.parent()?).ok());
    report.file_name().is_some()
        && report.file_name() == output.with_extension("av1opt.json").file_name()
        && folder(report).is_some_and(|d| Some(d) == folder(output))
}

/// Looks at one report and the two files it names. Changes nothing.
pub fn check(report: &Path) -> Check {
    match check_inner(report) {
        Ok(c) => c,
        Err(why) => Check::No(why),
    }
}

fn check_inner(report: &Path) -> Result<Check, String> {
    let r = read_report(report)?;
    let source = full_path(&r, "input")?;
    let output = full_path(&r, "output")?;
    let source_bytes = r["source_bytes"].as_u64().ok_or("report has no sizes")?;
    let output_bytes = r["output_bytes"].as_u64().ok_or("report has no sizes")?;
    if let Some(done) = r.get("replaced").filter(|d| d.is_object()) {
        let from = full_path(done, "from")?;
        let trash = full_path(done, "trash")?;
        if !beside(report, &from) {
            return Err("the report is not the one next to where the output was".into());
        }
        if !crate::scan::is_video(&source) || !crate::scan::is_video(&output) {
            return Err("the report names a file that is not a video".into());
        }
        let can_undo = file_size(&trash) == Some(source_bytes) && file_size(&output) == Some(output_bytes) && trash::holds(&trash, &source);
        let zfs = source.parent().is_some_and(crate::fsguard::on_zfs);
        return Ok(Check::Replaced(Replaced { report: report.to_path_buf(), source, dest: output, from, trash, source_bytes, output_bytes, can_undo, zfs }));
    }
    if source == output {
        return Err("output and original are the same file".into());
    }
    if !beside(report, &output) {
        return Err("the report is not the one next to its output".into());
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
    Ok(Check::Ready(Plan { report: report.to_path_buf(), source, output, dest, source_bytes, output_bytes, copy, zfs, removed }))
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

/// The name a file has while it is on its way to `dest`: hidden, in the same folder, and
/// never taken for a video by a scan or a batch.
fn staging_name(dest: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(".av1opt-");
    name.push(dest.file_name().unwrap_or_default());
    name.push(".part");
    dest.with_file_name(name)
}

/// Copies `from` to `to` (which must not exist), flushed to disk and with the same date.
fn copy_new(from: &Path, to: &Path, bytes: u64) -> Result<()> {
    let dir = to.parent().unwrap_or(Path::new("/"));
    if let Some(free) = free_space(dir)
        && free < bytes.saturating_add(16 << 20)
    {
        bail!("not enough room in {} ({} MB free, {} MB needed)", dir.display(), free >> 20, bytes >> 20);
    }
    let run = || -> Result<()> {
        let mut src = std::fs::File::open(from)?;
        let mut dst = std::fs::File::options().write(true).create_new(true).open(to)?;
        let n = std::io::copy(&mut src, &mut dst)?;
        if n != bytes {
            bail!("copied {n} bytes of {bytes}");
        }
        if let Ok(t) = src.metadata().and_then(|m| m.modified()) {
            let _ = dst.set_modified(t);
        }
        dst.sync_all()?;
        Ok(())
    };
    run().inspect_err(|_| drop(std::fs::remove_file(to))).with_context(|| format!("cannot copy {} to {}", from.display(), dir.display()))
}

/// Moves a file to a path nothing is at; across filesystems by a checked copy under a hidden
/// name, after which the file is removed from where it was.
fn move_new(from: &Path, to: &Path, bytes: u64) -> Result<()> {
    match trash::rename_new(from, to) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
            let part = staging_name(to);
            // Ours by name: left by a run that was killed half-way.
            let _ = std::fs::remove_file(&part);
            copy_new(from, &part, bytes)?;
            trash::rename_new(&part, to).inspect_err(|_| drop(std::fs::remove_file(&part))).with_context(|| format!("cannot name the copy {}", to.display()))?;
            if let Err(e) = std::fs::remove_file(from) {
                eprintln!("warning: copied, but could not remove {}: {e}", from.display());
            }
            Ok(())
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
    let part = report.with_extension("json.part");
    let write = || -> Result<()> {
        let mut f = std::fs::File::create(&part)?;
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
    let f = std::fs::File::open(report).with_context(|| format!("no report at {}", report.display()))?;
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

/// Replaces the original named in `report` by its output, after checking both again.
///
/// The original goes to the trash, then the output is renamed into its place; if that fails
/// the original is taken back out of the trash. An output on another filesystem is first
/// copied next to the original under a hidden name, so a copy that fails (no room, a read
/// error) leaves the original untouched; its old file is removed once the copy is in place.
/// The hidden file is only ever a copy: deleting one never loses anything.
pub fn apply(report: &Path, allow_zfs: bool) -> Result<Done> {
    apply_with(report, allow_zfs, &trash::trash)
}

fn apply_with(report: &Path, allow_zfs: bool, to_trash: &dyn Fn(&Path) -> Result<PathBuf>) -> Result<Done> {
    let _lock = lock(report)?;
    let p = match check(report) {
        Check::Ready(p) => p,
        Check::Replaced(_) => bail!("already replaced"),
        Check::No(why) => bail!("{why}"),
    };
    if p.zfs && !allow_zfs {
        bail!("the original is on ZFS, which is only replaced when asked for");
    }
    let source_meta = std::fs::symlink_metadata(&p.source)?;
    let in_place = p.dest == p.output;
    let staged = staging_name(&p.dest);
    let stage = || {
        // Left by a run that was killed half-way; the output itself is still where it was.
        let _ = std::fs::remove_file(&staged);
        copy_new(&p.output, &staged, p.output_bytes)
    };
    let staged_first = !in_place && p.copy;
    if staged_first {
        stage()?;
    }
    let trashed = match to_trash(&p.source) {
        Ok(t) => t,
        Err(e) => {
            if staged_first {
                let _ = std::fs::remove_file(&staged);
            }
            return Err(e.context("the original could not be moved to the trash; nothing was changed"));
        }
    };
    // Ok(true): the output was copied, and its old file is still in the output folder.
    let placed: Result<bool> = if in_place {
        Ok(false)
    } else if staged_first {
        trash::rename_new(&staged, &p.dest).map(|_| true).map_err(Into::into)
    } else {
        match trash::rename_new(&p.output, &p.dest) {
            Ok(()) => Ok(false),
            // Same device number but not one mount (a filesystem mounted twice): copy after all.
            Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => stage().and_then(|_| Ok(trash::rename_new(&staged, &p.dest)?)).map(|_| true),
            Err(e) => Err(e.into()),
        }
    };
    let copied = match placed {
        Ok(copied) => copied,
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
    if std::fs::set_permissions(&p.dest, source_meta.permissions()).is_err() {
        warnings.push("could not give the AV1 file the original's permissions".to_string());
    }
    sync_dir(p.dest.parent().unwrap_or(Path::new("/")));
    if copied && let Err(e) = std::fs::remove_file(&p.output) {
        warnings.push(format!("the copy is in place, but {} could not be removed: {e}", p.output.display()));
    }
    let noted = read_report(report).map_err(|e| anyhow!(e)).and_then(|mut r| {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        r["output"] = json!(p.dest);
        r["replaced"] = json!({ "time": now, "from": p.output, "trash": trashed });
        write_report(report, &r)
    });
    if let Err(e) = noted {
        warnings.push(format!("the report could not be updated ({e:#}); the original is in the trash at {}", trashed.display()));
    }
    Ok(Done { dest: p.dest, trash: trashed, source_bytes: p.source_bytes, warnings })
}

/// Undoes `apply` while the original is still in the trash: the AV1 file goes back where it
/// was, the original back to its place, and the report forgets the replacement.
pub fn undo(report: &Path, allow_zfs: bool) -> Result<Replaced> {
    let _lock = lock(report)?;
    let d = match check(report) {
        Check::Replaced(d) => d,
        Check::Ready(_) => bail!("not replaced"),
        Check::No(why) => bail!("{why}"),
    };
    match file_size(&d.trash) {
        None => bail!("the original is no longer in the trash"),
        Some(n) if n != d.source_bytes => bail!("the file in the trash is not the original"),
        Some(_) => {}
    }
    if !trash::holds(&d.trash, &d.source) {
        bail!("the trash does not say that {} came from {}", d.trash.display(), d.source.display());
    }
    // The original goes back to its folder: the same rule as when it left.
    crate::fsguard::ensure_writable_location(d.source.parent().context("the original has no folder")?, allow_zfs)?;
    if file_size(&d.dest) != Some(d.output_bytes) {
        bail!("the AV1 file is gone or has changed");
    }
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
    if d.from != d.dest {
        move_new(&d.dest, &d.from, d.output_bytes)?;
    }
    trash::restore(&d.trash, &d.source)
        .with_context(|| format!("the AV1 file is back at {}, but the original is still in the trash at {}", d.from.display(), d.trash.display()))?;
    sync_dir(d.source.parent().unwrap_or(Path::new("/")));
    let mut r = read_report(report).map_err(|e| anyhow!(e))?;
    r["output"] = json!(d.from);
    if let Some(o) = r.as_object_mut() {
        o.remove("replaced");
    }
    write_report(report, &r).context("both files are back, but the report could not be updated")?;
    Ok(d)
}

/// The reports under some folders (never looking into av1opt's own work folders).
pub fn report_files(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for d in dirs {
        let walk = walkdir::WalkDir::new(d)
            .max_depth(8)
            .into_iter()
            .filter_entry(|e| !e.file_name().to_string_lossy().starts_with(".av1opt-"))
            .flatten();
        for e in walk {
            if e.file_type().is_file() && e.file_name().to_string_lossy().ends_with(REPORT_SUFFIX) {
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
    let mut out = Vec::new();
    for p in paths {
        if p.is_dir() {
            out.extend(report_files(std::slice::from_ref(p)));
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

/// The `replace` command: lists what would happen, and does it with `go`.
pub fn run(paths: &[PathBuf], go: bool, back: bool, allow_zfs: bool) -> Result<()> {
    use crate::report::gb;
    let reports = reports_for(paths);
    if reports.is_empty() {
        bail!("no av1opt reports (*.av1opt.json) found there");
    }
    let (mut ready, mut replaced, mut stay) = (Vec::new(), Vec::new(), Vec::new());
    for r in &reports {
        match check(r) {
            Check::Ready(p) if p.zfs && !allow_zfs => stay.push((p.output.clone(), "the original is on ZFS (--allow-zfs replaces there too)".to_string())),
            Check::Ready(p) => ready.push(p),
            Check::Replaced(d) => replaced.push(d),
            Check::No(why) => stay.push((r.clone(), why)),
        }
    }
    let plural = |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let mut failed = 0;

    if back {
        let (can, cannot): (Vec<_>, Vec<_>) = replaced.into_iter().partition(|d| d.can_undo);
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
            match undo(&d.report, allow_zfs) {
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
            }
            return Ok(());
        }
        let (mut done, mut trashed) = (0, 0u64);
        for p in &ready {
            match apply(&p.report, allow_zfs) {
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
        bail!("{failed} failed");
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

    fn no(report: &Path) -> String {
        match check(report) {
            Check::No(why) => why,
            other => panic!("expected a refusal, got {other:?}"),
        }
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
        assert_eq!(undo(&report, false).unwrap_err().to_string(), "not replaced");
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
        // The honest report still undoes.
        std::fs::write(&report, honest).unwrap();
        let Check::Replaced(d) = check(&report) else { panic!() };
        assert!(d.can_undo && d.trash == done.trash);
        undo(&report, false).unwrap();
        assert!(f.lib("a.mp4").exists() && f.out("a.mkv").exists());
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
