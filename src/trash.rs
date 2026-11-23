//! Moves files to the desktop's trash (freedesktop.org Trash specification 1.0), where the
//! file manager shows them and can put them back. A file is only ever renamed within its own
//! filesystem, never copied: trashing 50 GB takes a moment and can't half-finish.
//! `restore` puts a trashed file back, as the file manager's "Restore" would.

use crate::source::{Recorded, Verdict};
use anyhow::{Context, Result, bail};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

#[cfg(test)]
thread_local! {
    /// Stands in for the user's trash in the tests of this thread.
    pub(crate) static TEST_HOME_TRASH: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// The user's own trash: `$XDG_DATA_HOME/Trash`.
fn home_trash() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(p) = TEST_HOME_TRASH.with(|h| h.borrow().clone()) {
        return Some(p);
    }
    let base = crate::files::xdg_folder("XDG_DATA_HOME", ".local/share")?;
    Some(base.join("Trash"))
}

/// The most an info file may hold: a path as long as any can be, every byte of it encoded.
const INFO_LIMIT: u64 = 1 << 16;

/// Whether a folder of this name is a drive's trash, or what holds one: `.Trash-<uid>` and
/// `.Trash`, which has a folder per user.
pub fn is_trash_name(name: &std::ffi::OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt;
    name.as_bytes().starts_with(b".Trash")
}

/// Whether the folder `dir` is the user's own trash, by whatever path it is reached.
pub fn is_home_trash(dir: &Path) -> bool {
    home_trash().is_some_and(|home| dir == home || std::fs::canonicalize(dir).is_ok_and(|real| std::fs::canonicalize(home).is_ok_and(|h| real == h)))
}

/// Whether `path` lies in a trash folder, of a drive or the user's own, seen through any links
/// on the way. What is there is on its way out, and no copy to rely on. A file that is gone
/// is judged by the nearest folder above it that still exists.
pub fn in_a_trash(path: &Path) -> bool {
    let named = |p: &Path| p.components().any(|c| is_trash_name(c.as_os_str()));
    let home = home_trash();
    if named(path) || home.as_ref().is_some_and(|h| path.starts_with(h)) {
        return true;
    }
    let Some(real) = path.ancestors().find_map(|p| std::fs::canonicalize(p).ok()) else { return false };
    named(&real) || home.and_then(|h| std::fs::canonicalize(h).ok()).is_some_and(|h| real.starts_with(h))
}

/// Moves `path` to the trash of the filesystem it is on. Returns where it is now.
pub fn trash(path: &Path) -> Result<PathBuf> {
    // SAFETY: getuid has no failure mode and touches no memory.
    let uid = unsafe { libc::getuid() };
    trash_with(path, home_trash().as_deref(), uid, now_local())
}

/// A trash folder and what `Path=` in its info files is relative to (None: absolute paths).
struct Bin {
    dir: PathBuf,
    relative_to: Option<PathBuf>,
}

fn make_private(dir: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

/// Whether `dir` can be trusted with what is thrown away: a real folder, never a link, that is
/// user `me`'s and that nobody else can write to. Anyone who could would be able to take the
/// files out, or put others in their place.
fn usable_bin(dir: &Path, me: u32) -> std::io::Result<()> {
    let m = std::fs::symlink_metadata(dir)?;
    let refused = |why: &str| Err(std::io::Error::other(format!("{} {why}", dir.display())));
    if !m.is_dir() {
        return refused("is not a folder of this user's");
    }
    if m.mode() & 0o022 != 0 {
        return refused("can be written to by other users; `chmod go-w` on it would make it usable");
    }
    if m.uid() != me && !crate::files::own_files_show_as(dir, m.uid()) {
        return refused("is not a folder of this user's");
    }
    Ok(())
}

/// Makes what is missing of the trash at `dir`, and checks all three folders it consists of.
fn ready_bin(dir: &Path, me: u32) -> std::io::Result<()> {
    for folder in [dir.to_path_buf(), dir.join("files"), dir.join("info")] {
        make_private(&folder)?;
        usable_bin(&folder, me)?;
    }
    Ok(())
}

/// The trash at the top of a drive, as the specification orders them: `.Trash/<uid>` when an
/// administrator set up a shared `.Trash`, else `.Trash-<uid>`.
fn bin_on_drive(top: &Path, uid: u32, me: u32) -> std::io::Result<Bin> {
    // A shared `.Trash` counts only as a real folder with the sticky bit, as the specification
    // asks: anything else could be another user's trap.
    let shared = top.join(".Trash");
    let shared_ok = std::fs::symlink_metadata(&shared).is_ok_and(|m| m.is_dir() && m.mode() & 0o1000 != 0);
    let mut last = None;
    for dir in [shared_ok.then(|| shared.join(uid.to_string())), Some(top.join(format!(".Trash-{uid}")))].into_iter().flatten() {
        // On a drive others write to, a folder of that name can be someone's link to anywhere,
        // or their own folder.
        match ready_bin(&dir, me) {
            Ok(()) => return Ok(Bin { dir, relative_to: Some(top.to_path_buf()) }),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("cannot make one")))
}

/// The trash for a file on device `dev`: the home trash when that is on the same filesystem,
/// else the one at the top of the file's filesystem.
fn bin_for(path: &Path, dev: u64, home: Option<&Path>, uid: u32) -> Result<Bin> {
    // SAFETY: getuid has no failure mode and touches no memory.
    let me = unsafe { libc::getuid() };
    if let Some(home) = home
        && make_private(home).is_ok()
        && std::fs::metadata(home).is_ok_and(|m| m.dev() == dev)
    {
        // The specification knows no other place for a file of this drive: better no trash
        // than one that others can reach.
        ready_bin(home, me).with_context(|| format!("cannot use the trash at {}", home.display()))?;
        return Ok(Bin { dir: home.to_path_buf(), relative_to: None });
    }
    // The top of the filesystem: the last parent still on the same device.
    let mut top: Option<&Path> = None;
    for dir in path.ancestors().skip(1) {
        match std::fs::metadata(dir) {
            Ok(m) if m.dev() == dev => top = Some(dir),
            _ => break,
        }
    }
    let top = top.with_context(|| format!("cannot tell which drive {} is on", path.display()))?;
    bin_on_drive(top, uid, me).map_err(|e| anyhow::anyhow!("no trash on the drive of {} ({}): {e}", path.display(), top.display()))
}

/// Percent-encoding of the info file's `Path=`: everything but unreserved characters and `/`.
fn encode_path(p: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::new();
    for &b in p.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Local time as the specification writes it: `2026-11-10T22:15:03`.
fn now_local() -> String {
    // SAFETY: time() with a null pointer only returns the time; localtime_r fills the tm we
    // own and is the thread-safe variant.
    let tm = unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
}

pub(crate) fn trash_with(path: &Path, home: Option<&Path>, uid: u32, when: String) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("{} is not an absolute path", path.display());
    }
    // The link itself, not what it points to.
    let meta = std::fs::symlink_metadata(path).with_context(|| format!("{} is not there", path.display()))?;
    let name = path.file_name().with_context(|| format!("{} has no name", path.display()))?;
    let bin = bin_for(path, meta.dev(), home, uid)?;
    let (files, info) = (bin.dir.join("files"), bin.dir.join("info"));
    let recorded = match &bin.relative_to {
        Some(top) => path.strip_prefix(top).unwrap_or(path),
        None => path,
    };
    let text = format!("[Trash Info]\nPath={}\nDeletionDate={when}\n", encode_path(recorded));

    // The info file is made first and exclusively: that is what reserves the name.
    for n in 1..10_000u32 {
        // The name in the trash need not be the file's (`Path=` says what that was), and with
        // the ending of the info file it has to fit.
        let number = if n > 1 { format!(".{n}") } else { String::new() };
        let mut unique = crate::files::leave_room(&info, name, ".trashinfo".len() + number.len());
        unique.push(number);
        let mut info_name = unique.clone();
        info_name.push(".trashinfo");
        let info_path = info.join(&info_name);
        let mut f = match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&info_path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("cannot write to the trash at {}", bin.dir.display())),
        };
        let dest = files.join(&unique);
        // A leftover file without its info file: don't replace it.
        // (In one step: a file put there between a look and the move would be replaced.)
        let moved = f.write_all(text.as_bytes()).and_then(|_| f.sync_all()).and_then(|_| {
            crate::signals::stop_point("trash:info");
            rename_new(path, &dest)
        });
        match moved {
            Ok(()) => return Ok(dest),
            Err(e) => {
                let _ = std::fs::remove_file(&info_path);
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    continue;
                }
                return Err(e).with_context(|| format!("cannot move {} to the trash at {}", path.display(), bin.dir.display()));
            }
        }
    }
    bail!("the trash at {} is full of files named like {}", bin.dir.display(), path.display())
}

/// Renames `from` to `to` unless something is already at `to` (then: `AlreadyExists`, and
/// nothing has moved). One step where the filesystem can do that, else a look followed by
/// the rename.
pub fn rename_new(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = |p: &Path| std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput));
    let (cf, ct) = (c(from)?, c(to)?);
    // SAFETY: both are valid NUL-terminated paths that outlive the call.
    if unsafe { libc::renameat2(libc::AT_FDCWD, cf.as_ptr(), libc::AT_FDCWD, ct.as_ptr(), libc::RENAME_NOREPLACE) } == 0 {
        return Ok(());
    }
    let e = std::io::Error::last_os_error();
    match e.raw_os_error() {
        // Old kernels and some filesystems (FUSE ones like ntfs-3g) don't know the flag.
        Some(libc::EINVAL | libc::ENOSYS | libc::ENOTSUP) => {
            if std::fs::symlink_metadata(to).is_ok() {
                return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
            }
            std::fs::rename(from, to)
        }
        _ => Err(e),
    }
}

/// The reverse of [`encode_path`]. None for text that is not such an encoding.
fn decode_path(s: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let (b, mut out, mut i) = (s.as_bytes(), Vec::new(), 0);
    while i < b.len() {
        if b[i] == b'%' {
            // Two digits exactly: `from_str_radix` alone would also take a sign for one.
            let digits = b.get(i + 1..i + 3).filter(|d| d.iter().all(u8::is_ascii_hexdigit))?;
            out.push(u8::from_str_radix(std::str::from_utf8(digits).ok()?, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

/// Whether the plain file at `path` is the one `known` describes: see [`is_open_file_recorded`].
/// A link is not followed.
pub fn is_file_recorded(path: &Path, known: &Recorded) -> bool {
    let opened = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path);
    opened.is_ok_and(|file| is_open_file_recorded(&file, known))
}

/// Whether the open file is the one `known` describes, for a file about to leave the trash on
/// that answer. Its size has to be the same, and then all of it is read where the hash of the
/// whole file is on record, or its samples where only those are: a date says nothing of a file
/// rewritten (or rotted) in place, and going into the trash changes none of its contents. With
/// neither on record, as in reports of before 2026-11-18, its date has to be the same, if one
/// was written down; nothing is lost by putting a file back where nothing stands.
pub fn is_open_file_recorded(file: &std::fs::File, known: &Recorded) -> bool {
    let Ok(meta) = file.metadata() else { return false };
    if !meta.is_file() || known.size.is_some_and(|size| size != meta.len()) {
        return false;
    }
    match (&known.whole, &known.fingerprint) {
        (Some(hash), _) => crate::source::whole_hash_of_open(file).is_ok_and(|now| now == *hash),
        (None, Some(print)) => crate::source::fingerprint_of_open(file).is_ok_and(|now| now == *print),
        (None, None) => known.modified.is_none_or(|then| crate::source::modified_seconds(&meta).is_some_and(|now| crate::source::same_time(then, now))),
    }
}

fn counts_as_recorded(verdict: Verdict, known: &Recorded) -> bool {
    match verdict {
        Verdict::Same => true,
        Verdict::CannotTell => known.fingerprint.is_some(),
        _ => false,
    }
}

/// Whether `trashed` is the file `known` describes, in a trash that says it came from
/// `original`, reading as much of it as [`is_open_file_recorded`] does. An info file alone is no proof: it can be stale, its name taken over by a later
/// file, or planted to pass another file off as the original, which [`restore`] would then
/// put in the original's place.
pub fn holds(trashed: &Path, original: &Path, known: &Recorded) -> bool {
    is_file_recorded(trashed, known) && says_came_from(trashed, original)
}

/// [`holds`] for a list of many reports, which reads no whole file: see
/// `source::compare_for_list`. Putting the file back asks [`is_file_recorded`] again.
pub fn holds_for_list(trashed: &Path, original: &Path, known: &Recorded) -> bool {
    std::fs::symlink_metadata(trashed).is_ok_and(|m| m.is_file()) && counts_as_recorded(crate::source::compare_for_list(known, trashed), known) && says_came_from(trashed, original)
}

/// Whether `trashed` is a file in a trash whose info file says it came from `original`.
///
/// What says so has to be worth believing: a report on disk names both paths, and
/// [`restore`] moves the one to the other. An absolute `Path=` counts in the user's own trash
/// only. In any other trash folder the path counts from the folder the trash is in, as the
/// specification has it, so a `.Trash-1000` somebody put into a folder of downloads can only
/// ever give files back to that folder.
pub fn says_came_from(trashed: &Path, original: &Path) -> bool {
    holds_with(trashed, original, &|top| {
        // The top of a filesystem: its parent is on another one (or it has none).
        let dev = |p: &Path| std::fs::metadata(p).map(|m| m.dev()).ok();
        top.parent().is_none_or(|up| dev(top).is_some() && dev(top) != dev(up))
    })
}

/// [`says_came_from`], with what counts as the top of a drive passed in (tests have no drive of their
/// own). A trash folder anywhere else is only a folder with that name: this module never
/// makes one there, and anybody can.
fn holds_with(trashed: &Path, original: &Path, is_top: &dyn Fn(&Path) -> bool) -> bool {
    let inner = || -> Option<bool> {
        let files = trashed.parent().filter(|d| d.file_name().is_some_and(|n| n == "files"))?;
        let bin = files.parent()?;
        let mut info_name = trashed.file_name()?.to_os_string();
        info_name.push(".trashinfo");
        // A pipe under this name would never finish opening.
        let info = String::from_utf8(crate::files::read_regular(&bin.join("info").join(info_name), INFO_LIMIT).ok()?).ok()?;
        let recorded = decode_path(info.lines().find_map(|l| l.strip_prefix("Path="))?)?;
        if recorded.is_absolute() {
            let home = std::fs::canonicalize(home_trash()?).ok()?;
            return Some(std::fs::canonicalize(bin).ok()? == home && recorded == original);
        }
        // `.Trash-<uid>` lies at the top itself, `.Trash/<uid>` one folder below it.
        let top = match bin.parent()? {
            shared if shared.file_name().is_some_and(|n| n == ".Trash") => shared.parent()?,
            top => top,
        };
        // An empty path has no parts to object to, and would name the top of the drive.
        let plain = recorded.components().next().is_some() && recorded.components().all(|c| matches!(c, std::path::Component::Normal(_)));
        Some(plain && is_top(top) && top.join(recorded) == original)
    };
    inner().unwrap_or(false)
}

/// The files in the trash that say they came from `original`: in the user's own trash and in
/// the trash folders at the top of the original's drive, where [`trash`] puts a file of that
/// place. The newest first. What each is has yet to be shown (see [`holds`]): an info file
/// alone is no proof.
pub fn entries_from(original: &Path) -> Vec<PathBuf> {
    // SAFETY: getuid has no failure mode and touches no memory.
    let uid = unsafe { libc::getuid() };
    let mut bins: Vec<PathBuf> = home_trash().into_iter().collect();
    let folder = original.parent().unwrap_or(Path::new("/"));
    if let Ok(dev) = std::fs::metadata(folder).map(|m| m.dev()) {
        let top = std::iter::once(folder).chain(folder.ancestors().skip(1)).take_while(|d| std::fs::metadata(d).is_ok_and(|m| m.dev() == dev)).last();
        if let Some(top) = top {
            bins.push(top.join(".Trash").join(uid.to_string()));
            bins.push(top.join(format!(".Trash-{uid}")));
        }
    }
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for bin in bins {
        for entry in std::fs::read_dir(bin.join("info")).into_iter().flatten().flatten() {
            use std::os::unix::ffi::OsStrExt;
            let name = entry.file_name();
            let Some(stem) = name.as_bytes().strip_suffix(b".trashinfo") else { continue };
            let trashed = bin.join("files").join(std::ffi::OsStr::from_bytes(stem));
            if says_came_from(&trashed, original) {
                found.push((entry.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH), trashed));
            }
        }
    }
    found.sort_by_key(|f| std::cmp::Reverse(f.0));
    found.into_iter().map(|(_, p)| p).collect()
}

/// Puts a file `trash` moved away back at `to` and drops its info file. Nothing at `to` is
/// replaced.
pub fn restore(trashed: &Path, to: &Path) -> Result<()> {
    restore_if(trashed, to, &|_| true)
}

/// [`restore`], for a file that has to be a particular one: `is_it` is asked of what arrived, and
/// anything else goes back into the trash under its name, with the info file still there.
pub fn restore_if(trashed: &Path, to: &Path, is_it: &dyn Fn(&std::fs::Metadata) -> bool) -> Result<()> {
    let files = trashed.parent().filter(|d| d.file_name().is_some_and(|n| n == "files"));
    let (Some(files), Some(name)) = (files, trashed.file_name()) else { bail!("{} is not in a trash folder", trashed.display()) };
    rename_new(trashed, to).with_context(|| format!("cannot move {} back to {}", trashed.display(), to.display()))?;
    if !std::fs::symlink_metadata(to).is_ok_and(|m| is_it(&m)) {
        return Err(match rename_new(to, trashed) {
            Ok(()) => anyhow::anyhow!("{} was replaced by another file while it was being put back, so it was left in the trash", trashed.display()),
            Err(e) => anyhow::anyhow!("{} was replaced by another file while it was being put back, and that file could not be taken away from {}: {e}", trashed.display(), to.display()),
        });
    }
    let mut info_name = name.to_os_string();
    info_name.push(".trashinfo");
    if let Some(bin) = files.parent() {
        let _ = std::fs::remove_file(bin.join("info").join(info_name));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("av1opt-trash-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_info_file_cannot_pass_another_file_off_as_the_original() {
        let dir = scratch("known");
        let home = dir.join("data/Trash");
        TEST_HOME_TRASH.with(|h| *h.borrow_mut() = Some(home.clone()));
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        let a = dir.join("lib/a.mkv");
        std::fs::write(&a, b"first").unwrap();
        let then = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        let set_age = |p: &Path, t| std::fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
        set_age(&a, then);
        let (_, known) = Recorded::of(&a).unwrap();
        assert_eq!(known.modified, Some(1_600_000_000));
        let by_size = Recorded { modified: None, fingerprint: None, ..known.clone() };
        let by_time = Recorded { fingerprint: None, ..known.clone() };
        let dest = trash_with(&a, Some(&home), 1000, "2026-11-10T22:15:03".into()).unwrap();
        assert!(holds(&dest, &a, &known));
        assert!(holds(&dest, &a, &by_size));

        // The info file stays true to its word, the file under its name is another one.
        std::fs::write(&dest, b"other").unwrap();
        set_age(&dest, then + std::time::Duration::from_secs(60));
        assert!(says_came_from(&dest, &a));
        assert!(!holds(&dest, &a, &known), "same size, another time and other contents");
        assert!(!holds(&dest, &a, &by_time), "same size, another time, nothing to tell them apart");
        // Without anything else on record the size is all there is to go by.
        assert!(holds(&dest, &a, &by_size));
        std::fs::write(&dest, b"longer file").unwrap();
        set_age(&dest, then);
        assert!(!holds(&dest, &a, &known), "same time, another size");
        assert!(!holds(&dest, &a, &by_size));

        // The right contents under another time are still the original: samples are all a
        // report of an earlier version has, and with the whole hash that hash decides.
        std::fs::write(&dest, b"first").unwrap();
        set_age(&dest, then + std::time::Duration::from_secs(120));
        assert!(holds(&dest, &a, &known));
        assert!(holds_for_list(&dest, &a, &known));
        let hash = crate::source::WholeHash::start(&dest, 5).finish(&|| false).unwrap();
        assert!(holds(&dest, &a, &Recorded { whole: Some(hash), ..known.clone() }));
        assert!(!holds(&dest, &a, &Recorded { whole: Some("0".into()), ..known.clone() }));

        // A link of the right kind to a file of the right kind is still no file in the trash.
        let elsewhere = dir.join("elsewhere.mkv");
        std::fs::write(&elsewhere, b"first").unwrap();
        set_age(&elsewhere, then);
        std::fs::remove_file(&dest).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &dest).unwrap();
        assert!(!holds(&dest, &a, &known));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_pipe_under_the_name_of_an_info_file_is_no_answer_and_no_wait() {
        let dir = scratch("info-pipe");
        let home = dir.join("data/Trash");
        std::fs::create_dir_all(home.join("files")).unwrap();
        std::fs::create_dir_all(home.join("info")).unwrap();
        let trashed = home.join("files/a.mkv");
        std::fs::write(&trashed, b"x").unwrap();
        let pipe = std::ffi::CString::new(home.join("info/a.mkv.trashinfo").to_str().unwrap()).unwrap();
        // SAFETY: `pipe` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(pipe.as_ptr(), 0o644) }, 0);
        let (tell, told) = std::sync::mpsc::channel();
        let original = dir.join("lib/a.mkv");
        std::thread::spawn(move || {
            TEST_HOME_TRASH.with(|h| *h.borrow_mut() = Some(home));
            let _ = tell.send(says_came_from(&trashed, &original));
        });
        assert_eq!(told.recv_timeout(std::time::Duration::from_secs(1)), Ok(false));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_trash_is_believed_only_about_what_it_can_know() {
        let dir = scratch("holds");
        let home = dir.join("data/Trash");
        TEST_HOME_TRASH.with(|h| *h.borrow_mut() = Some(home.clone()));
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        let a = dir.join("lib/vidéo #1.mkv");
        std::fs::write(&a, b"first").unwrap();
        let dest = trash_with(&a, Some(&home), 1000, "2026-11-10T22:15:03".into()).unwrap();
        assert!(says_came_from(&dest, &a));
        assert!(!says_came_from(&dest, &dir.join("lib/other.mkv")));
        assert!(!says_came_from(&a, &a));
        assert_eq!(decode_path(&encode_path(&a)), Some(a.clone()));
        assert_eq!(decode_path("a%2"), None);
        assert_eq!(decode_path("a%zz"), None);

        // The trash at the top of a drive: paths count from that top, and only plain ones.
        let top = dir.join("drive");
        let bin = top.join(".Trash-1000");
        std::fs::create_dir_all(bin.join("files")).unwrap();
        std::fs::create_dir_all(bin.join("info")).unwrap();
        let put = |name: &str, path: &str| {
            std::fs::write(bin.join("files").join(name), b"x").unwrap();
            std::fs::write(bin.join("info").join(format!("{name}.trashinfo")), format!("[Trash Info]\nPath={path}\nDeletionDate=2026-11-10T22:15:03\n")).unwrap();
            bin.join("files").join(name)
        };
        let at_top = |t: &Path, o: &Path| holds_with(t, o, &|p| p == top);
        assert!(at_top(&put("in.mkv", "films/in.mkv"), &top.join("films/in.mkv")));
        assert!(!at_top(&put("abs.mkv", "/etc/abs.mkv"), Path::new("/etc/abs.mkv")));
        assert!(!at_top(&put("up.mkv", "../lib/up.mkv"), &top.join("../lib/up.mkv")));
        assert!(!at_top(&put("up.mkv", "../lib/up.mkv"), &dir.join("lib/up.mkv")));
        // The same folder anywhere but at the top of a drive is anybody's folder.
        assert!(!says_came_from(&bin.join("files/in.mkv"), &top.join("films/in.mkv")));
        // Shared trash folders keep one folder per user.
        let shared = top.join(".Trash/1000");
        std::fs::create_dir_all(shared.join("files")).unwrap();
        std::fs::create_dir_all(shared.join("info")).unwrap();
        std::fs::write(shared.join("files/s.mkv"), b"x").unwrap();
        std::fs::write(shared.join("info/s.mkv.trashinfo"), "[Trash Info]\nPath=films/s.mkv\n").unwrap();
        assert!(at_top(&shared.join("files/s.mkv"), &top.join("films/s.mkv")));
        // No info file, no say.
        std::fs::write(bin.join("files/bare.mkv"), b"x").unwrap();
        assert!(!at_top(&bin.join("files/bare.mkv"), &top.join("bare.mkv")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn files_go_to_the_home_trash_with_an_info_file() {
        let dir = scratch("home");
        let home = dir.join("data/Trash");
        std::fs::create_dir_all(dir.join("lib/sub dir")).unwrap();
        let a = dir.join("lib/sub dir/vidéo #1.mkv");
        std::fs::write(&a, b"first").unwrap();
        let dest = trash_with(&a, Some(&home), 1000, "2026-11-10T22:15:03".into()).unwrap();
        assert!(!a.exists());
        assert_eq!(dest, home.join("files/vidéo #1.mkv"));
        assert_eq!(std::fs::read(&dest).unwrap(), b"first");
        let info = std::fs::read_to_string(home.join("info/vidéo #1.mkv.trashinfo")).unwrap();
        let want = format!("[Trash Info]\nPath={}\nDeletionDate=2026-11-10T22:15:03\n", encode_path(&a));
        assert_eq!(info, want);
        assert!(info.contains("/lib/sub%20dir/vid%C3%A9o%20%231.mkv"), "{info}");

        // The same name again gets its own slot; nothing in the trash is replaced.
        std::fs::write(&a, b"second").unwrap();
        let dest2 = trash_with(&a, Some(&home), 1000, "2026-11-10T22:15:04".into()).unwrap();
        assert_eq!(dest2, home.join("files/vidéo #1.mkv.2"));
        assert_eq!(std::fs::read(&dest).unwrap(), b"first");
        assert_eq!(std::fs::read(&dest2).unwrap(), b"second");
        assert!(home.join("info/vidéo #1.mkv.2.trashinfo").exists());

        // Missing files and relative paths are errors, and leave no info file behind.
        assert!(trash_with(&a, Some(&home), 1000, "x".into()).is_err());
        assert!(trash_with(Path::new("relative.mkv"), Some(&home), 1000, "x".into()).is_err());
        assert_eq!(std::fs::read_dir(home.join("info")).unwrap().count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_near_the_limit_goes_to_the_trash_and_comes_back() {
        let dir = scratch("long");
        let home = dir.join("data/Trash");
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        TEST_HOME_TRASH.with(|h| *h.borrow_mut() = Some(home.clone()));
        let a = dir.join("lib").join(format!("{}.mkv", "é".repeat((crate::files::name_max(&dir) - 4) / 2)));
        for turn in [&b"first"[..], b"second"] {
            std::fs::write(&a, turn).unwrap();
            let dest = trash_with(&a, Some(&home), 1000, "2026-11-10T22:15:03".into()).unwrap();
            // Under a shorter name, and still known as that file.
            assert!(!a.exists() && holds_with(&dest, &a, &|_| false), "{}", dest.display());
            assert_eq!(std::fs::read(&dest).unwrap(), turn);
        }
        let mut names: Vec<_> = std::fs::read_dir(home.join("files")).unwrap().map(|e| e.unwrap().path()).collect();
        names.sort();
        assert_eq!(names.len(), 2);
        restore(&names[0], &a).unwrap();
        assert!(a.exists());
        assert_eq!(std::fs::read_dir(home.join("info")).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_link_is_trashed_not_its_target() {
        let dir = scratch("link");
        let home = dir.join("Trash");
        let target = dir.join("real.mkv");
        std::fs::write(&target, b"keep me").unwrap();
        let link = dir.join("link.mkv");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let dest = trash_with(&link, Some(&home), 1000, "x".into()).unwrap();
        assert!(std::fs::symlink_metadata(&dest).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(&target).unwrap(), b"keep me");
        assert!(std::fs::symlink_metadata(&link).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn trash_folders_are_known_by_name_and_by_where_the_users_own_is() {
        let dir = std::fs::canonicalize(scratch("where")).unwrap();
        let home = dir.join("data/Trash");
        TEST_HOME_TRASH.with(|h| *h.borrow_mut() = Some(home.clone()));
        make_private(&home.join("files")).unwrap();
        std::os::unix::fs::symlink(dir.join("data"), dir.join("link")).unwrap();
        std::os::unix::fs::symlink(dir.join("drive/.Trash-1000"), dir.join("to-bin")).unwrap();
        std::fs::create_dir_all(dir.join("drive/.Trash-1000/files")).unwrap();
        std::fs::create_dir_all(dir.join("lib/Trash")).unwrap();
        for inside in ["data/Trash", "data/Trash/files/a.mkv", "link/Trash/files/a.mkv", "drive/.Trash-1000/files/a.mkv", "drive/.Trash/1000/files/a.mkv", "to-bin/files/a.mkv"] {
            assert!(in_a_trash(&dir.join(inside)), "{inside}");
        }
        for outside in ["lib/a.mkv", "lib/Trash/a.mkv", "data/a.mkv", "gone/a.mkv"] {
            assert!(!in_a_trash(&dir.join(outside)), "{outside}");
        }
        assert!(is_home_trash(&home) && is_home_trash(&dir.join("link/Trash")));
        assert!(!is_home_trash(&dir.join("lib/Trash")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn a_trash_on_a_drive_that_maps_owners_is_recognised_by_what_is_made_in_it() {
        let dir = scratch("owners");
        let bin = dir.join(".Trash-1000");
        make_private(&bin).unwrap();
        std::os::unix::fs::symlink(&bin, dir.join("link")).unwrap();
        std::fs::write(dir.join("file"), b"x").unwrap();
        let owner = std::fs::metadata(&bin).unwrap().uid();
        // A number this user does not go by on the drive, as with sshfs or NFS: what is made
        // in the folder shows up under the folder's owner, so it is this user's.
        let me = owner.wrapping_add(1);
        assert!(usable_bin(&bin, owner).is_ok());
        assert!(usable_bin(&bin, me).is_ok());
        assert_eq!(std::fs::read_dir(&bin).unwrap().count(), 0, "the proof is not left behind");
        // A folder this user cannot make anything in is somebody else's.
        // SAFETY: a plain query of this process's user.
        if unsafe { libc::geteuid() } != 0 {
            set_mode(&bin, 0o500);
            let refused = usable_bin(&bin, me).unwrap_err().to_string();
            set_mode(&bin, 0o700);
            assert!(refused.ends_with("is not a folder of this user's"), "{refused}");
        }
        // A link or a file is no trash, whoever it seems to belong to.
        assert!(usable_bin(&dir.join("link"), owner).is_err());
        assert!(usable_bin(&dir.join("file"), owner).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_trash_others_can_write_to_is_refused_and_nothing_moves() {
        let dir = scratch("open");
        let me = std::fs::metadata(&dir).unwrap().uid();
        for mode in [0o777, 0o775, 0o757, 0o1777] {
            let bin = dir.join(format!("bin{mode:o}"));
            make_private(&bin).unwrap();
            set_mode(&bin, mode);
            let refused = usable_bin(&bin, me).unwrap_err().to_string();
            assert!(refused.contains("can be written to by other users"), "{refused}");
        }
        for mode in [0o700, 0o755, 0o750] {
            let bin = dir.join(format!("fine{mode:o}"));
            make_private(&bin).unwrap();
            set_mode(&bin, mode);
            assert!(usable_bin(&bin, me).is_ok(), "{mode:o}");
        }
        // The same goes for the folders inside, in the user's own trash too.
        let home = dir.join("Trash");
        make_private(&home.join("info")).unwrap();
        set_mode(&home.join("info"), 0o777);
        let a = dir.join("a.mkv");
        std::fs::write(&a, b"x").unwrap();
        let e = format!("{:#}", trash_with(&a, Some(&home), 1000, "x".into()).unwrap_err());
        assert!(e.contains("cannot use the trash") && e.contains("can be written to by other users"), "{e}");
        assert!(a.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_link_inside_a_trash_leads_nothing_there() {
        let dir = scratch("inner-link");
        let home = dir.join("Trash");
        let away = dir.join("away");
        make_private(&home).unwrap();
        make_private(&away).unwrap();
        std::os::unix::fs::symlink(&away, home.join("files")).unwrap();
        let a = dir.join("a.mkv");
        std::fs::write(&a, b"x").unwrap();
        let e = format!("{:#}", trash_with(&a, Some(&home), 1000, "x".into()).unwrap_err());
        assert!(e.contains("is not a folder of this user's"), "{e}");
        assert!(a.exists() && std::fs::read_dir(&away).unwrap().count() == 0);
        assert!(!home.join("info").read_dir().is_ok_and(|mut d| d.next().is_some()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_drive_falls_back_from_the_shared_trash_as_the_specification_says() {
        let top = scratch("fallback");
        let me = std::fs::metadata(&top).unwrap().uid();
        let own = top.join(".Trash-77");
        let shared = top.join(".Trash");
        // Nothing set up: the user's own folder at the top, made private.
        assert_eq!(bin_on_drive(&top, 77, me).unwrap().dir, own);
        assert_eq!(std::fs::metadata(own.join("files")).unwrap().mode() & 0o777, 0o700);
        // A `.Trash` without the sticky bit is passed over.
        make_private(&shared).unwrap();
        assert_eq!(bin_on_drive(&top, 77, me).unwrap().dir, own);
        assert!(!shared.join("77").exists());
        // With it, the user's folder in there comes first.
        set_mode(&shared, 0o1777);
        assert_eq!(bin_on_drive(&top, 77, me).unwrap().dir, shared.join("77"));
        // One whose `files` leads elsewhere is passed over as well.
        std::fs::remove_dir(shared.join("77/files")).unwrap();
        std::os::unix::fs::symlink(top.join("elsewhere"), shared.join("77/files")).unwrap();
        assert_eq!(bin_on_drive(&top, 77, me).unwrap().dir, own);
        // A `.Trash` that is a link is no shared trash.
        std::fs::remove_file(shared.join("77/files")).unwrap();
        set_mode(&shared, 0o700);
        std::fs::remove_dir_all(&shared).unwrap();
        let real = top.join("real");
        make_private(&real).unwrap();
        set_mode(&real, 0o1777);
        std::os::unix::fs::symlink(&real, &shared).unwrap();
        assert_eq!(bin_on_drive(&top, 77, me).unwrap().dir, own);
        assert_eq!(std::fs::read_dir(&real).unwrap().count(), 0);
        // With neither usable there is no trash, and the message says what is wrong.
        set_mode(&own, 0o777);
        let refused = bin_on_drive(&top, 77, me).err().unwrap().to_string();
        assert!(refused.contains("can be written to by other users"), "{refused}");
        std::fs::remove_dir_all(&top).unwrap();
    }

    /// Files on another filesystem than the home trash stay on theirs. `/dev/shm` is the one
    /// other filesystem a test can count on; where it isn't one, there is nothing to check.

    #[test]
    fn other_drives_use_their_own_trash() {
        let home_dir = scratch("drive");
        let home = home_dir.join("Trash");
        let shm = Path::new("/dev/shm");
        // A made-up user number keeps the test's trash apart from a real one.
        let uid = 3_000_000_000 + std::process::id() % 1_000_000;
        let dir = shm.join(format!("av1opt-trash-test-{}", std::process::id()));
        let (Ok(a), Ok(b)) = (std::fs::metadata(shm), std::fs::metadata(&home_dir)) else {
            return crate::tools::skipped("other_drives_use_their_own_trash", "/dev/shm is not there");
        };
        if a.dev() == b.dev() {
            return crate::tools::skipped("other_drives_use_their_own_trash", "/dev/shm is on the drive of the temporary folder");
        }
        if let Err(e) = std::fs::create_dir_all(dir.join("sub")) {
            return crate::tools::skipped("other_drives_use_their_own_trash", &format!("nothing can be made in /dev/shm: {e}"));
        }
        let f = dir.join("sub/clip.mp4");
        std::fs::write(&f, b"x").unwrap();
        let dest = trash_with(&f, Some(&home), uid, "2026-11-10T22:15:03".into()).unwrap();
        let bin = shm.join(format!(".Trash-{uid}"));
        assert_eq!(dest, bin.join("files/clip.mp4"));
        assert!(!f.exists());
        // Recorded relative to the top of that drive, so it survives another mount point.
        let info = std::fs::read_to_string(bin.join("info/clip.mp4.trashinfo")).unwrap();
        assert!(info.contains(&format!("Path=av1opt-trash-test-{}/sub/clip.mp4\n", std::process::id())), "{info}");
        assert!(!home.join("files/clip.mp4").exists());
        // A trash of that name that is a link to somewhere else is someone's trap, not a trash.
        let (uid, f) = (uid + 1, dir.join("sub/other.mp4"));
        let trap = shm.join(format!(".Trash-{uid}"));
        std::fs::write(&f, b"x").unwrap();
        std::fs::create_dir_all(dir.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(dir.join("elsewhere"), &trap).unwrap();
        let e = trash_with(&f, Some(&home), uid, "x".into()).unwrap_err().to_string();
        assert!(e.contains("is not a folder of this user's"), "{e}");
        assert!(f.exists() && std::fs::read_dir(dir.join("elsewhere")).unwrap().count() == 0);
        let _ = std::fs::remove_file(&trap);
        for d in [&bin, &dir, &home_dir] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn restoring_puts_the_file_back_and_replaces_nothing() {
        let dir = scratch("restore");
        let home = dir.join("Trash");
        let a = dir.join("clip.mp4");
        std::fs::write(&a, b"original").unwrap();
        let dest = trash_with(&a, Some(&home), 1000, "x".into()).unwrap();
        // Something else took the place: the trashed file stays where it is.
        std::fs::write(&a, b"newcomer").unwrap();
        assert!(restore(&dest, &a).is_err());
        assert_eq!(std::fs::read(&dest).unwrap(), b"original");
        assert_eq!(std::fs::read(&a).unwrap(), b"newcomer");
        assert!(home.join("info/clip.mp4.trashinfo").exists());
        std::fs::remove_file(&a).unwrap();
        restore(&dest, &a).unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), b"original");
        assert!(!dest.exists() && !home.join("info/clip.mp4.trashinfo").exists());
        // Only files in a trash folder are "restored".
        assert!(restore(&a, &dir.join("b.mp4")).is_err());
        assert!(a.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_date_looks_like_the_specification() {
        let d = now_local();
        let b = d.as_bytes();
        assert_eq!(d.len(), 19, "{d}");
        assert!(b[4] == b'-' && b[7] == b'-' && b[10] == b'T' && b[13] == b':' && b[16] == b':', "{d}");
        assert!(d[..4].parse::<u32>().unwrap() >= 2026);
    }

    #[test]
    fn everything_but_plain_characters_is_percent_encoded() {
        assert_eq!(encode_path(Path::new("/az/AZ/09/-_.~")), "/az/AZ/09/-_.~");
        assert_eq!(encode_path(Path::new("/a b/é%#?=\n.mkv")), "/a%20b/%C3%A9%25%23%3F%3D%0A.mkv");
        assert_eq!(encode_path(Path::new("")), "");
    }

    #[test]
    fn every_byte_of_a_name_goes_through_encoding_and_back() {
        use std::os::unix::ffi::OsStrExt;
        let bytes: Vec<u8> = (0..=255).collect();
        let path = Path::new(std::ffi::OsStr::from_bytes(&bytes));
        let text = encode_path(path);
        // Nothing in it can break the info file's lines.
        assert!(text.is_ascii() && !text.contains(['\n', '\r', ' ']), "{text}");
        assert_eq!(decode_path(&text).unwrap().as_os_str().as_bytes(), &bytes[..]);
    }

    #[test]
    fn other_programs_spellings_of_a_path_are_understood() {
        assert_eq!(decode_path("").unwrap(), Path::new(""));
        assert_eq!(decode_path("/a%c3%a9").unwrap(), Path::new("/aé"));
        // Characters an encoder could have left alone.
        assert_eq!(decode_path("/vidéo #1.mkv").unwrap(), Path::new("/vidéo #1.mkv"));
    }

    #[test]
    fn a_percent_sign_without_two_digits_is_no_encoding() {
        for bad in ["%", "a%", "%4", "%%41", "%G0", "%0G", "%é", "é%é", "%4é"] {
            assert_eq!(decode_path(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_sign_is_not_a_digit_of_an_encoded_byte() {
        assert_eq!(decode_path("a%+1"), None);
        assert_eq!(decode_path("%+f"), None);
    }

    /// A trash at `<dir>/drive/.Trash-1000` holding `name`, with this info file.
    fn in_drive_trash(dir: &Path, name: &str, info: &str) -> PathBuf {
        let bin = dir.join("drive/.Trash-1000");
        std::fs::create_dir_all(bin.join("files")).unwrap();
        std::fs::create_dir_all(bin.join("info")).unwrap();
        std::fs::write(bin.join("files").join(name), b"x").unwrap();
        std::fs::write(bin.join("info").join(format!("{name}.trashinfo")), info).unwrap();
        bin.join("files").join(name)
    }

    #[test]
    fn only_the_line_that_is_the_path_says_where_a_file_came_from() {
        let dir = scratch("lines");
        let top = dir.join("drive");
        let on_drive = |t: &Path, o: &Path| holds_with(t, o, &|p| p == top);
        let f = in_drive_trash(&dir, "none.mkv", "[Trash Info]\nDeletionDate=2026-11-10T22:15:03\n");
        assert!(!on_drive(&f, &top.join("none.mkv")));
        let f = in_drive_trash(&dir, "other.mkv", "[Trash Info]\nOldPath=other.mkv\n Path=other.mkv\npath=other.mkv\n");
        assert!(!on_drive(&f, &top.join("other.mkv")));
        // Of two, the first counts.
        let f = in_drive_trash(&dir, "two.mkv", "[Trash Info]\nPath=a/two.mkv\nPath=b/two.mkv\n");
        assert!(on_drive(&f, &top.join("a/two.mkv")));
        assert!(!on_drive(&f, &top.join("b/two.mkv")));
        let f = in_drive_trash(&dir, "bad.mkv", "[Trash Info]\nPath=bad%zz.mkv\n");
        assert!(!on_drive(&f, &top.join("bad%zz.mkv")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_way_out_of_the_drive_is_not_hidden_by_encoding_it() {
        let dir = scratch("encoded");
        let top = dir.join("drive");
        let f = in_drive_trash(&dir, "up.mkv", "[Trash Info]\nPath=%2E%2E%2Flib%2Fup.mkv\n");
        assert!(!holds_with(&f, &top.join("../lib/up.mkv"), &|p| p == top));
        assert!(!holds_with(&f, &dir.join("lib/up.mkv"), &|p| p == top));
        let f = in_drive_trash(&dir, "abs.mkv", "[Trash Info]\nPath=%2Fetc%2Fabs.mkv\n");
        assert!(!holds_with(&f, Path::new("/etc/abs.mkv"), &|p| p == top));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_empty_path_names_no_file() {
        let dir = scratch("emptypath");
        let top = dir.join("drive");
        let f = in_drive_trash(&dir, "e.mkv", "[Trash Info]\nPath=\n");
        let held = holds_with(&f, &top, &|p| p == top);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(!held);
    }

    #[test]
    fn an_absolute_path_counts_in_the_users_own_trash_and_nowhere_else() {
        let dir = scratch("absolute");
        let home = dir.join("data/Trash");
        TEST_HOME_TRASH.with(|h| *h.borrow_mut() = Some(home.clone()));
        let wanted = dir.join("lib/a.mkv");
        let info = format!("[Trash Info]\nPath={}\n", encode_path(&wanted));
        // Before the user's trash exists nothing can be believed.
        let f = in_drive_trash(&dir, "a.mkv", &info);
        assert!(!holds_with(&f, &wanted, &|_| true));
        std::fs::create_dir_all(home.join("files")).unwrap();
        std::fs::create_dir_all(home.join("info")).unwrap();
        assert!(!holds_with(&f, &wanted, &|_| true));
        std::fs::write(home.join("files/a.mkv"), b"x").unwrap();
        std::fs::write(home.join("info/a.mkv.trashinfo"), &info).unwrap();
        assert!(holds_with(&home.join("files/a.mkv"), &wanted, &|_| false));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_outside_a_files_folder_is_held_by_no_trash() {
        let dir = scratch("outside");
        let top = dir.join("drive");
        let f = in_drive_trash(&dir, "a.mkv", "[Trash Info]\nPath=a.mkv\n");
        let moved = f.parent().unwrap().parent().unwrap().join("a.mkv");
        std::fs::rename(&f, &moved).unwrap();
        assert!(!holds_with(&moved, &top.join("a.mkv"), &|_| true));
        assert!(!holds_with(Path::new("/"), &top.join("a.mkv"), &|_| true));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_path_that_ends_without_a_name_is_refused_and_nothing_moves() {
        let dir = scratch("noname");
        let home = dir.join("Trash");
        std::fs::create_dir_all(dir.join("lib/sub")).unwrap();
        let e = trash_with(&dir.join("lib/sub/.."), Some(&home), 1000, "x".into()).unwrap_err().to_string();
        assert!(e.ends_with("has no name"), "{e}");
        assert!(dir.join("lib/sub").is_dir() && !home.exists());
        assert!(trash_with(Path::new("/"), Some(&home), 1000, "x".into()).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_left_in_the_trash_without_its_info_is_not_replaced() {
        let dir = scratch("leftover");
        let home = dir.join("Trash");
        make_private(&home.join("files")).unwrap();
        std::fs::write(home.join("files/clip.mp4"), b"left behind").unwrap();
        let a = dir.join("clip.mp4");
        std::fs::write(&a, b"new").unwrap();
        let dest = trash_with(&a, Some(&home), 1000, "x".into()).unwrap();
        assert_eq!(dest, home.join("files/clip.mp4.2"));
        assert_eq!(std::fs::read(home.join("files/clip.mp4")).unwrap(), b"left behind");
        // The name it could not have is not left reserved.
        assert!(!home.join("info/clip.mp4.trashinfo").exists());
        assert!(home.join("info/clip.mp4.2.trashinfo").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_folder_goes_to_the_trash_whole_and_a_line_break_in_a_name_stays_in_its_line() {
        let dir = scratch("whole");
        let home = dir.join("data/Trash");
        TEST_HOME_TRASH.with(|h| *h.borrow_mut() = Some(home.clone()));
        let folder = dir.join("two\nlines");
        std::fs::create_dir_all(folder.join("in")).unwrap();
        std::fs::write(folder.join("in/a"), b"x").unwrap();
        let dest = trash_with(&folder, Some(&home), 1000, "2026-11-10T22:15:03".into()).unwrap();
        assert!(!folder.exists() && dest.join("in/a").exists());
        let info = std::fs::read_to_string(home.join("info/two\nlines.trashinfo")).unwrap();
        assert_eq!(info.lines().count(), 3, "{info}");
        assert!(says_came_from(&dest, &folder));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_rename_never_replaces_and_says_why_it_did_not_happen() {
        let dir = scratch("rename");
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::write(&a, b"first").unwrap();
        std::fs::write(&b, b"second").unwrap();
        assert_eq!(rename_new(&a, &b).unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!((std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap()), (b"first".to_vec(), b"second".to_vec()));
        // A link that leads nowhere is still something at that name.
        std::os::unix::fs::symlink(dir.join("nowhere"), dir.join("link")).unwrap();
        assert_eq!(rename_new(&a, &dir.join("link")).unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(rename_new(&dir.join("gone"), &dir.join("c")).unwrap_err().kind(), std::io::ErrorKind::NotFound);
        assert_eq!(rename_new(Path::new("a\0b"), &dir.join("c")).unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(rename_new(&a, Path::new("a\0b")).unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
        rename_new(&a, &dir.join("c")).unwrap();
        assert!(!a.exists() && dir.join("c").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_restore_with_nowhere_to_go_leaves_the_trash_as_it_was() {
        let dir = scratch("nowhere");
        let home = dir.join("Trash");
        let a = dir.join("clip.mp4");
        std::fs::write(&a, b"x").unwrap();
        let dest = trash_with(&a, Some(&home), 1000, "x".into()).unwrap();
        assert!(restore(&dest, &dir.join("gone/clip.mp4")).is_err());
        assert!(dest.exists() && home.join("info/clip.mp4.trashinfo").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_trash_that_is_not_there_is_not_usable() {
        let dir = scratch("missing");
        assert_eq!(usable_bin(&dir.join("none"), 0).unwrap_err().kind(), std::io::ErrorKind::NotFound);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
