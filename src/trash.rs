//! Moves files to the desktop's trash (freedesktop.org Trash specification 1.0), where the
//! file manager shows them and can put them back. A file is only ever renamed within its own
//! filesystem, never copied: trashing 50 GB takes a moment and can't half-finish.
//! `restore` puts a trashed file back, as the file manager's "Restore" would.

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
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(base.join("Trash"))
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

/// Whether `dir` will do as a trash on a drive whose top folder belongs to `top_owner`: a real
/// folder, never a link, that is user `me`'s. Drives that map owners (sshfs, NFS, CIFS) show
/// a user's own folders under another number, the same as the drive's top folder.
fn usable_bin(dir: &Path, me: u32, top_owner: Option<u32>) -> std::io::Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() && (m.uid() == me || Some(m.uid()) == top_owner) => Ok(()),
        Ok(_) => Err(std::io::Error::other(format!("{} is not a folder of this user's", dir.display()))),
        Err(e) => Err(e),
    }
}

/// The trash for a file on device `dev`: the home trash when that is on the same filesystem,
/// else the one at the top of the file's filesystem (`.Trash/<uid>` when an administrator set
/// one up, `.Trash-<uid>` otherwise).
fn bin_for(path: &Path, dev: u64, home: Option<&Path>, uid: u32) -> Result<Bin> {
    if let Some(home) = home
        && make_private(home).is_ok()
        && std::fs::metadata(home).is_ok_and(|m| m.dev() == dev)
    {
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
    // A shared `.Trash` counts only as a real folder with the sticky bit, as the specification
    // asks: anything else could be another user's trap.
    let shared = top.join(".Trash");
    let shared_ok = std::fs::symlink_metadata(&shared).is_ok_and(|m| m.is_dir() && m.mode() & 0o1000 != 0);
    let candidates = [shared_ok.then(|| shared.join(uid.to_string())), Some(top.join(format!(".Trash-{uid}")))];
    // SAFETY: getuid has no failure mode and touches no memory.
    let me = unsafe { libc::getuid() };
    let top_owner = std::fs::metadata(top).ok().map(|m| m.uid());
    let mut last = None;
    for dir in candidates.into_iter().flatten() {
        // On a drive others write to, a folder of that name can be someone's link to
        // anywhere, or their own folder.
        match make_private(&dir).and_then(|()| usable_bin(&dir, me, top_owner)) {
            Ok(()) => return Ok(Bin { dir, relative_to: Some(top.to_path_buf()) }),
            Err(e) => last = Some(e),
        }
    }
    bail!("no trash on the drive of {} ({}): {}", path.display(), top.display(), last.map_or("cannot make one".to_string(), |e| e.to_string()))
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
    make_private(&files).and_then(|_| make_private(&info)).with_context(|| format!("cannot use the trash at {}", bin.dir.display()))?;
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
        let moved = f.write_all(text.as_bytes()).and_then(|_| f.sync_all()).and_then(|_| rename_new(path, &dest));
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
            out.push(u8::from_str_radix(s.get(i + 1..i + 3)?, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

/// Whether `trashed` is a file in a trash whose info file says it came from `original`.
///
/// What says so has to be worth believing: a report on disk names both paths, and
/// [`restore`] moves the one to the other. An absolute `Path=` counts in the user's own trash
/// only. In any other trash folder the path counts from the folder the trash is in, as the
/// specification has it, so a `.Trash-1000` somebody put into a folder of downloads can only
/// ever give files back to that folder.
pub fn holds(trashed: &Path, original: &Path) -> bool {
    holds_with(trashed, original, &|top| {
        // The top of a filesystem: its parent is on another one (or it has none).
        let dev = |p: &Path| std::fs::metadata(p).map(|m| m.dev()).ok();
        top.parent().is_none_or(|up| dev(top).is_some() && dev(top) != dev(up))
    })
}

/// [`holds`], with what counts as the top of a drive passed in (tests have no drive of their
/// own). A trash folder anywhere else is only a folder with that name: this module never
/// makes one there, and anybody can.
fn holds_with(trashed: &Path, original: &Path, is_top: &dyn Fn(&Path) -> bool) -> bool {
    let inner = || -> Option<bool> {
        let files = trashed.parent().filter(|d| d.file_name().is_some_and(|n| n == "files"))?;
        let bin = files.parent()?;
        let mut info_name = trashed.file_name()?.to_os_string();
        info_name.push(".trashinfo");
        let info = std::fs::read_to_string(bin.join("info").join(info_name)).ok()?;
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
        let plain = recorded.components().all(|c| matches!(c, std::path::Component::Normal(_)));
        Some(plain && is_top(top) && top.join(recorded) == original)
    };
    inner().unwrap_or(false)
}

/// Puts a file `trash` moved away back at `to` and drops its info file. Nothing at `to` is
/// replaced.
pub fn restore(trashed: &Path, to: &Path) -> Result<()> {
    let files = trashed.parent().filter(|d| d.file_name().is_some_and(|n| n == "files"));
    let (Some(files), Some(name)) = (files, trashed.file_name()) else { bail!("{} is not in a trash folder", trashed.display()) };
    rename_new(trashed, to).with_context(|| format!("cannot move {} back to {}", trashed.display(), to.display()))?;
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
    fn a_trash_is_believed_only_about_what_it_can_know() {
        let dir = scratch("holds");
        let home = dir.join("data/Trash");
        TEST_HOME_TRASH.with(|h| *h.borrow_mut() = Some(home.clone()));
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        let a = dir.join("lib/vidéo #1.mkv");
        std::fs::write(&a, b"first").unwrap();
        let dest = trash_with(&a, Some(&home), 1000, "2026-11-10T22:15:03".into()).unwrap();
        assert!(holds(&dest, &a));
        assert!(!holds(&dest, &dir.join("lib/other.mkv")));
        assert!(!holds(&a, &a));
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
        assert!(!holds(&bin.join("files/in.mkv"), &top.join("films/in.mkv")));
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

    /// Files on another filesystem than the home trash stay on theirs. `/dev/shm` is the one
    /// other filesystem a test can count on; where it isn't one, there is nothing to check.
    #[test]
    fn a_trash_on_a_drive_that_maps_owners_is_recognised() {
        let dir = scratch("owners");
        let bin = dir.join(".Trash-1000");
        std::fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(&bin, dir.join("link")).unwrap();
        std::fs::write(dir.join("file"), b"x").unwrap();
        let owner = std::fs::metadata(&bin).unwrap().uid();
        // A number this user does not go by on the drive, as with sshfs or NFS.
        let me = owner.wrapping_add(1);
        assert!(usable_bin(&bin, owner, None).is_ok());
        assert!(usable_bin(&bin, me, Some(owner)).is_ok());
        let refused = usable_bin(&bin, me, Some(me)).unwrap_err().to_string();
        assert!(refused.ends_with("is not a folder of this user's"), "{refused}");
        assert!(usable_bin(&bin, me, None).is_err());
        // A link or a file is no trash, whoever it seems to belong to.
        assert!(usable_bin(&dir.join("link"), owner, Some(owner)).is_err());
        assert!(usable_bin(&dir.join("file"), owner, Some(owner)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn other_drives_use_their_own_trash() {
        let home_dir = scratch("drive");
        let home = home_dir.join("Trash");
        let shm = Path::new("/dev/shm");
        let (Ok(a), Ok(b)) = (std::fs::metadata(shm), std::fs::metadata(&home_dir)) else { return };
        if a.dev() == b.dev() {
            return;
        }
        // A made-up user number keeps the test's trash apart from a real one.
        let uid = 3_000_000_000 + std::process::id() % 1_000_000;
        let dir = shm.join(format!("av1opt-trash-test-{}", std::process::id()));
        if std::fs::create_dir_all(dir.join("sub")).is_err() {
            return;
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
}
