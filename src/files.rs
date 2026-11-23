//! Writing files where another program, or another user, may be writing too.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Writes `data` to a new file at `path`. What was at that name is removed first and the
/// file is made anew: a link left there (in an output folder others can write to) is
/// replaced, never written through.
pub fn write_new(path: &Path, data: &[u8]) -> std::io::Result<()> {
    create_new(path)?.write_all(data)
}

/// A new, empty file at `path`, made as [`write_new`] makes one.
pub fn create_new(path: &Path) -> std::io::Result<std::fs::File> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    std::fs::OpenOptions::new().write(true).create_new(true).custom_flags(libc::O_NOFOLLOW).mode(new_file_mode(path)).open(path)
}

/// The mode for a file about to be made at `path`. In a folder only this user can enter it is
/// for this user alone too; elsewhere, as beside an output, the umask decides as usual.
fn new_file_mode(path: &Path) -> u32 {
    if std::fs::metadata(folder_of(path)).is_ok_and(|m| m.mode() & 0o077 == 0) { 0o600 } else { 0o666 }
}

/// A base folder of the XDG specification: the variable's value, or `below_home` in the home
/// folder. The specification has anything but an absolute path ignored; an empty or relative
/// value would put av1opt's files in whatever folder it was started from.
pub fn xdg_folder(variable: &str, below_home: &str) -> Option<PathBuf> {
    xdg_folder_from(std::env::var_os(variable), std::env::var_os("HOME"), below_home)
}

fn xdg_folder_from(value: Option<OsString>, home: Option<OsString>, below_home: &str) -> Option<PathBuf> {
    let absolute = |value: Option<OsString>| value.map(PathBuf::from).filter(|p| p.is_absolute());
    absolute(value).or_else(|| Some(absolute(home)?.join(below_home)))
}

/// Makes `path` and what is missing above it, and leaves `path` open to this user alone
/// whatever it was before. For the folders that hold what someone keeps, watches and encodes.
pub fn private_folder(path: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    if std::fs::metadata(path)?.mode() & 0o077 != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Takes away what others may do with the file at `path`, one another program has made.
#[cfg(any(feature = "ui", test))]
pub fn make_private(path: &Path) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// Takes away what others may do with everything below `folder`, for what versions before
/// this one left there. Links are not followed. What cannot be changed stays as it is.
#[cfg(any(feature = "ui", test))]
pub fn tighten_below(folder: &Path) {
    for entry in std::fs::read_dir(folder).into_iter().flatten().flatten() {
        let path = entry.path();
        let Ok(found) = std::fs::symlink_metadata(&path) else { continue };
        if found.file_type().is_symlink() {
            continue;
        }
        if found.mode() & 0o077 != 0 {
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(found.mode() & 0o700));
        }
        if found.is_dir() {
            tighten_below(&path);
        }
    }
}

/// `count` random bytes as text, for names nobody can work out ahead of time.
pub fn random_text(count: usize) -> std::io::Result<String> {
    let mut bytes = vec![0u8; count];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// When process `pid` started, in the system's clock ticks since boot. None if there is no
/// such process. A process number alone can come back as another program's.
pub fn process_start(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The program's name, in brackets, comes second and can hold anything: the fields are
    // counted from the last bracket, the first after it being number three.
    stat.rsplit_once(')')?.1.split_whitespace().nth(19)?.parse().ok()
}

fn my_user() -> u32 {
    // SAFETY: geteuid has no failure mode and touches no memory.
    unsafe { libc::geteuid() }
}

/// Makes a folder for this run in `parent`, which others may be able to write to, and returns
/// it. Only this user can enter it, and it is new: the name holds random bytes and the folder
/// is made by a `mkdir` that fails on anything already there, so nothing can have been laid
/// out in it beforehand, and what ffmpeg then writes by name in it cannot go through a link.
/// The name also says which process it is for: see [`remove_abandoned_folders`].
pub fn make_run_folder(parent: &Path, prefix: &str) -> std::io::Result<PathBuf> {
    let pid = std::process::id();
    let start = process_start(pid).unwrap_or(0);
    let mut result = Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
    for _ in 0..16 {
        let path = parent.join(format!("{prefix}{pid}-{start}-{}", random_text(8)?));
        result = std::fs::DirBuilder::new().mode(0o700).create(&path).map(|()| path);
        if !result.as_ref().is_err_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists) {
            break;
        }
    }
    result
}

/// Whether the process a run folder's name (what follows its prefix) speaks of is still
/// there. None for a name that is not one of a run folder: `<process>-<start>-<random>`, or
/// the bare process number older versions used.
fn run_folder_in_use(name: &str) -> Option<bool> {
    let mut parts = name.split('-');
    let pid: u32 = parts.next()?.parse().ok()?;
    match (parts.next(), parts.next(), parts.next()) {
        (None, ..) => Some(Path::new(&format!("/proc/{pid}")).exists()),
        (Some(start), Some(random), None) if !random.is_empty() && random.bytes().all(|b| b.is_ascii_hexdigit()) => {
            let start: u64 = start.parse().ok()?;
            Some(process_start(pid) == Some(start))
        }
        _ => None,
    }
}

/// Removes the run folders in `parent` left by runs that ended without clearing up.
pub fn remove_abandoned_folders(parent: &Path, prefix: &str) {
    remove_abandoned_folders_of(parent, prefix, my_user());
}

/// As [`remove_abandoned_folders`], for the folders of user `me`. The name says nothing of
/// who made a folder, as anyone who can write to `parent` can choose it: only a real folder
/// of this user's is ever removed. The removal does not follow links, neither one in the
/// folder's place nor ones inside it: each is taken away as a link.
fn remove_abandoned_folders_of(parent: &Path, prefix: &str, me: u32) {
    // Without the system's list of processes every run would look gone.
    if process_start(std::process::id()).is_none() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(parent) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(in_use) = name.to_str().and_then(|n| n.strip_prefix(prefix)).and_then(run_folder_in_use) else { continue };
        let path = entry.path();
        let mine = std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir() && m.uid() == me);
        if mine && !in_use {
            eprintln!("removing stale temp folder {}", path.display());
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// The work folder at `path`, kept from an earlier run so that it can go on from it, held open
/// if it can be believed; None if there is none yet, and otherwise why not. Its name can be
/// worked out by anyone, and what is in it decides what ends up in the output.
///
/// `tools_reach` says whether ffmpeg and ffprobe can go through a path of the descriptor (see
/// [`HeldFolder`]); where they cannot, the folder is held by its name, as where there is no
/// `/proc`.
pub fn check_kept_folder(path: &Path, tools_reach: bool) -> Result<Option<HeldFolder>, String> {
    check_kept_folder_of(path, my_user(), tools_reach, &|| shown_in(folder_of(path)))
}

fn check_kept_folder_of(path: &Path, me: u32, tools_reach: bool, shown: &dyn Fn() -> std::io::Result<Shown>) -> Result<Option<HeldFolder>, String> {
    let folder = match open_folder(path) {
        Ok(folder) => folder,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Err("it is a link".into()),
        Err(e) if e.raw_os_error() == Some(libc::ENOTDIR) => return Err("it is not a folder".into()),
        Err(e) => return Err(format!("it cannot be looked at ({e})")),
    };
    // Folders made by earlier versions follow the umask. One only its owner can write to is
    // as good as a private one, and may hold days of encoding. It is closed by way of the
    // descriptor: by name, a link put there meanwhile would be followed.
    let found = folder.metadata().map_err(|e| format!("it cannot be looked at ({e})"))?;
    let held = HeldFolder::hold(folder, path, me, 0o022, &|_| tools_reach, shown).map_err(|e| e.to_string())?;
    let entries = std::fs::read_dir(held.path()).map_err(|e| format!("it cannot be read ({e})"))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("it cannot be read ({e})"))?;
        let file = std::fs::symlink_metadata(entry.path()).map_err(|e| format!("it cannot be read ({e})"))?;
        // Of the owner the folder was found to have: where the filesystem shows another than
        // this user for everything, it shows that one for these files too.
        if !file.is_file() || file.uid() != found.uid() {
            return Err(format!("{} in it is not a plain file of this user's", entry.file_name().to_string_lossy()));
        }
    }
    if held.keeps_modes && found.mode() & 0o777 != 0o700 {
        held.folder.set_permissions(std::fs::Permissions::from_mode(0o700)).map_err(|e| format!("it cannot be made private ({e})"))?;
    }
    Ok(Some(held))
}

/// Makes the folder `path` for work kept between runs, private and new, and holds it (see
/// [`HeldFolder::made`]). Fails if something is there already.
pub fn make_kept_folder(path: &Path, tools_reach: &dyn Fn(&Path) -> bool) -> std::io::Result<HeldFolder> {
    std::fs::DirBuilder::new().mode(0o700).create(path)?;
    HeldFolder::made(path, tools_reach)
}

/// What a filesystem shows of a file av1opt has just made there with mode 0600: its owner, and
/// whether it keeps that mode.
#[derive(Clone, Copy, Debug)]
struct Shown {
    owner: u32,
    keeps_modes: bool,
}

/// What `folder`'s filesystem shows of a file made there now. FAT, exFAT and NTFS drives show
/// every file and folder with the owner and modes they were mounted with, and network drives
/// may show another owner for all (root, or the user everyone is mapped to). A file made here
/// with `O_EXCL` and judged on its own descriptor is the one proof of that nobody else can fake:
/// a folder can be swapped for another between its `mkdir` and its open, this cannot.
fn shown_in(folder: &Path) -> std::io::Result<Shown> {
    let probe = folder.join(format!(".av1opt-owner-check-{}-{}", std::process::id(), random_text(4)?));
    let file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&probe)?;
    let meta = file.metadata();
    let _ = std::fs::remove_file(&probe);
    let meta = meta?;
    Ok(Shown { owner: meta.uid(), keeps_modes: meta.mode() & 0o077 == 0 })
}

/// Whether what this user creates in `dir` shows up as belonging to `owner`. Drives that map
/// owners (sshfs, NFS, CIFS) show a user's own folders under another number, and a small file
/// is the one proof of that: in another user's closed folder it cannot even be made.
pub fn own_files_show_as(dir: &Path, owner: u32) -> bool {
    shown_in(dir).is_ok_and(|shown| shown.owner == owner)
}

/// Says, once for each folder, that the folders av1opt makes in it cannot be private.
fn say_no_modes_in(parent: &Path) {
    static SAID: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<PathBuf>>> = std::sync::LazyLock::new(Default::default);
    if SAID.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(parent.to_path_buf()) {
        eprintln!(
            "warning: the drive of {} does not keep who owns a file or who may open it (FAT, exFAT and NTFS drives and some network drives do not), so the temporary and work folders av1opt makes there cannot be made private; they are used by their names",
            parent.display()
        );
    }
}

/// Whether the folder `folder` holds is empty.
fn is_empty(folder: &std::fs::File) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;
    // SAFETY: openat on a descriptor we own, with a valid NUL-terminated name, gives a
    // descriptor of its own (and a reading position of its own), which fdopendir takes over
    // and closedir closes; readdir's entries are only read before the next call.
    unsafe {
        let fd = libc::openat(folder.as_raw_fd(), c".".as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let dir = libc::fdopendir(fd);
        if dir.is_null() {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }
        let mut empty = true;
        loop {
            let entry = libc::readdir(dir);
            if entry.is_null() {
                break;
            }
            let name = std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()).to_bytes();
            if name != b"." && name != b".." {
                empty = false;
                break;
            }
        }
        libc::closedir(dir);
        Ok(empty)
    }
}

/// Whether users other than this one can rename what is in `folder`: it is open to their
/// writing and lacks the sticky bit, which would keep each to their own names. Writing by the
/// folder's group counts only when someone else may be in that group: with the umask of 0002
/// many systems give, every folder can be written by its group, and that group is its user's
/// own. A folder with an access list may let anyone write, whatever its mode shows.
fn others_may_rename_in(folder: &Path) -> bool {
    let Ok(m) = std::fs::metadata(folder) else { return false };
    if m.mode() & 0o1000 != 0 {
        return false;
    }
    m.mode() & 0o002 != 0 || (m.mode() & 0o020 != 0 && (has_access_list(folder) || !group_is_mine_alone(m.gid())))
}

/// Whether `folder` has an access list beyond its mode.
fn has_access_list(folder: &Path) -> bool {
    let Ok(name) = std::ffi::CString::new(folder.as_os_str().as_bytes()) else { return true };
    // SAFETY: valid NUL-terminated strings; with no buffer, getxattr only gives the size.
    let size = unsafe { libc::getxattr(name.as_ptr(), c"system.posix_acl_access".as_ptr(), std::ptr::null_mut(), 0) };
    size > 0
}

/// Whether the open file or folder `file` has an access list beyond its mode.
fn has_access_list_open(file: &std::fs::File) -> bool {
    use std::os::fd::AsRawFd;
    // SAFETY: a descriptor we own and a valid NUL-terminated name; with no buffer, fgetxattr
    // only gives the size.
    let size = unsafe { libc::fgetxattr(file.as_raw_fd(), c"system.posix_acl_access".as_ptr(), std::ptr::null_mut(), 0) };
    size > 0
}

/// Whether users other than this one may write to the open file or folder `file`, of mode
/// `mode` and group `gid`: as [`others_may_rename_in`] judges a folder's group, sticky bit
/// aside.
fn others_may_write(file: &std::fs::File, mode: u32, gid: u32) -> bool {
    mode & 0o002 != 0 || (mode & 0o020 != 0 && (has_access_list_open(file) || !group_is_mine_alone(gid)))
}

/// Why a user other than this one may have put the plain file `file`, opened from `path`,
/// where it is or written what is in it, if one may: it belongs to another user, has other
/// names, others may write to it, or others may rename things in its folder (which has no
/// sticky bit to keep each to their own names), or the folder is another user's. Judged on the
/// open file and its folder, opened now and found to hold this same file under its name.
///
/// A drive that keeps no owners or modes of its own (FAT, exFAT, NTFS, and network drives that
/// show one owner for everything) shows what it was mounted with, and that is what is judged:
/// files shown as another user's, or as open to everyone, cannot be told to be this user's.
pub fn others_may_have_written(path: &Path, file: &std::fs::File) -> Option<String> {
    others_may_have_written_of(path, file, my_user())
}

fn others_may_have_written_of(path: &Path, file: &std::fs::File, me: u32) -> Option<String> {
    use std::os::fd::AsRawFd;
    let meta = match file.metadata() {
        Ok(meta) => meta,
        Err(e) => return Some(format!("it cannot be looked at ({e})")),
    };
    if !meta.is_file() {
        return Some(format!("it is {}, not a plain file", kind_of(&meta)));
    }
    if meta.uid() != me {
        return Some("it belongs to another user".into());
    }
    if meta.nlink() != 1 {
        return Some("it has other names on the drive too (hard links)".into());
    }
    if others_may_write(file, meta.mode(), meta.gid()) {
        return Some("other users can write to it".into());
    }
    let dir = folder_of(path);
    let folder = match open_folder(dir) {
        Ok(folder) => folder,
        Err(e) => return Some(format!("its folder {} cannot be looked at ({e})", dir.display())),
    };
    let Some(name) = path.file_name().and_then(|n| std::ffi::CString::new(n.as_bytes()).ok()) else {
        return Some("it has no name of its own".into());
    };
    // SAFETY: fstatat on a descriptor we own, with a valid NUL-terminated name, fills the
    // struct it is given and nothing else.
    let there = unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        (libc::fstatat(folder.as_raw_fd(), name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) == 0).then_some((st.st_dev, st.st_ino))
    };
    if there != Some((meta.dev(), meta.ino())) {
        return Some("it was moved or replaced while it was being looked at".into());
    }
    let found = match folder.metadata() {
        Ok(found) => found,
        Err(e) => return Some(format!("its folder {} cannot be looked at ({e})", dir.display())),
    };
    // The owner of a folder can rename anything in it, sticky bit or not.
    if found.uid() != me && found.uid() != 0 {
        return Some(format!("its folder {} belongs to another user", dir.display()));
    }
    if found.mode() & 0o1000 == 0 && others_may_write(&folder, found.mode(), found.gid()) {
        return Some(format!("other users can write to its folder {}, which has no sticky bit to keep them from renaming files of yours there", dir.display()));
    }
    None
}

/// Whether the group `gid` is this user's own and holds nobody else, as far as can be told
/// cheaply from the system's user and group lists (by way of libc, so that lists kept
/// elsewhere, as in LDAP, count too): it is this user's own group, named as this user is, and
/// lists no member but this user. A group some other user has as their own without being listed
/// in it can only be found by going through every user, which is not done: any other group
/// counts as shared.
fn group_is_mine_alone(gid: u32) -> bool {
    let mut buffer = vec![0 as libc::c_char; 1 << 16];
    // SAFETY: the structs are filled by the calls from the buffer given, which outlives every
    // use of the pointers in them; a result of null or an error means nothing was found.
    unsafe {
        let mut user: libc::passwd = std::mem::zeroed();
        let mut found = std::ptr::null_mut();
        if libc::getpwuid_r(my_user(), &mut user, buffer.as_mut_ptr(), buffer.len(), &mut found) != 0 || found.is_null() || user.pw_gid != gid {
            return false;
        }
        let me = std::ffi::CStr::from_ptr(user.pw_name).to_bytes().to_vec();
        let mut group_buffer = vec![0 as libc::c_char; 1 << 16];
        let mut group: libc::group = std::mem::zeroed();
        let mut found = std::ptr::null_mut();
        if libc::getgrgid_r(gid, &mut group, group_buffer.as_mut_ptr(), group_buffer.len(), &mut found) != 0 || found.is_null() {
            return false;
        }
        if std::ffi::CStr::from_ptr(group.gr_name).to_bytes() != me {
            return false;
        }
        let mut member = group.gr_mem;
        while !member.is_null() && !(*member).is_null() {
            if std::ffi::CStr::from_ptr(*member).to_bytes() != me {
                return false;
            }
            member = member.add(1);
        }
        true
    }
}

/// The folder at `path`, opened. A link at that name is refused (`ELOOP`) and so is anything
/// but a folder (`ENOTDIR`), without waiting on a pipe.
fn open_folder(path: &Path) -> std::io::Result<std::fs::File> {
    match std::fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC).open(path) {
        // Linux answers a link with ENOTDIR when a folder is asked for; it is told apart here so
        // that the message says what is there.
        Err(e) if e.raw_os_error() == Some(libc::ENOTDIR) && std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) => {
            Err(std::io::Error::from_raw_os_error(libc::ELOOP))
        }
        opened => opened,
    }
}

/// A folder of this user's held open, so that everything done in it goes by way of its
/// descriptor and not by its name. In a folder others can write to and that lacks the sticky
/// bit (a shared output folder made with mode 0777, say), anyone may rename a folder of ours
/// and put a link to one of theirs at its name: whatever went by the name after that would be
/// written into, read from and removed in their folder, and what av1opt then records as its
/// own encode could be their file.
///
/// [`HeldFolder::path`] is `/proc/<this process>/fd/<descriptor>`, which leads to the folder
/// itself wherever it has gone. That path is given to ffmpeg and ffprobe as well: they run as
/// this user, and the system lets a process of the same user go through another's descriptors,
/// one started by `systemd-run --scope` under the memory cap included. Under `/proc/self` the
/// path would only work for a program that inherited the descriptor, and none does: every
/// descriptor is closed for the programs av1opt starts.
///
/// Where that path does not work (no `/proc`, or ffmpeg run where it does not see this
/// process), or where the filesystem keeps no modes or owners and so nothing can be private,
/// the folder is held by its name instead, as before av1opt held folders: the first only in an
/// output folder where nobody else can rename things.
#[derive(Debug)]
pub struct HeldFolder {
    folder: std::fs::File,
    inside: PathBuf,
    /// Whether `inside` is the folder's name rather than a path of its descriptor.
    by_name: bool,
    /// Whether the filesystem shows the folder's own mode, so that making it private means
    /// something.
    keeps_modes: bool,
}

impl HeldFolder {
    /// Opens the folder at `path`, which av1opt has just made with mode 0700, to hold it: a
    /// real folder (a link there is refused), empty, of this user's and that nobody else can
    /// enter. Judged on the open descriptor, so that what is judged is what is then used.
    /// `tools_reach` is asked, with the path of the descriptor, whether ffmpeg and ffprobe can
    /// go through it.
    ///
    /// When the folder does not show what it was made with, either the filesystem keeps no
    /// modes or owners of its own, and the folder is held by its name with a warning, or the
    /// folder is not the one made: someone put another at its name in the moment between.
    /// Which of the two is told by a file made there now. A folder that is refused for want of
    /// a way through its descriptor is removed again, if it is still the one made.
    pub fn made(path: &Path, tools_reach: &dyn Fn(&Path) -> bool) -> std::io::Result<HeldFolder> {
        Self::made_of(path, my_user(), tools_reach, &|| shown_in(folder_of(path)))
    }

    fn made_of(path: &Path, me: u32, tools_reach: &dyn Fn(&Path) -> bool, shown: &dyn Fn() -> std::io::Result<Shown>) -> std::io::Result<HeldFolder> {
        let folder = open_folder(path)?;
        let meta = folder.metadata()?;
        // A folder av1opt has just made holds nothing: one that does is somebody else's, put at
        // its name, a kept work folder of another video, say.
        if !is_empty(&folder)? {
            return Err(std::io::Error::other("it is not the folder av1opt had just made: something was in it at once, so another folder was put at its name"));
        }
        let held = Self::hold(folder, path, me, 0o077, tools_reach, shown);
        if held.as_ref().is_err_and(|e| e.kind() == std::io::ErrorKind::Unsupported)
            && std::fs::symlink_metadata(path).is_ok_and(|now| now.is_dir() && (now.dev(), now.ino()) == (meta.dev(), meta.ino()))
        {
            let _ = std::fs::remove_dir(path);
        }
        held
    }

    /// `folder`, found at `path`, held: if it is a folder of user `me` without any of the bits
    /// in `refused`, or as near to that as its filesystem can show (see [`shown_in`], asked by
    /// way of `shown` only when the folder shows otherwise). `reach` is asked whether a path of
    /// the descriptor can be given to ffmpeg and ffprobe.
    fn hold(folder: std::fs::File, path: &Path, me: u32, refused: u32, reach: &dyn Fn(&Path) -> bool, shown: &dyn Fn() -> std::io::Result<Shown>) -> std::io::Result<HeldFolder> {
        use std::os::fd::AsRawFd;
        let meta = folder.metadata()?;
        if !meta.is_dir() {
            return Err(std::io::Error::other("it is not a folder"));
        }
        let mode_refused = || std::io::Error::other(if meta.mode() & refused & 0o022 != 0 { "other users can write to it" } else { "other users can enter it" });
        if meta.uid() != me || meta.mode() & refused != 0 {
            // Where the filesystem keeps owners and modes, a new file shows this user and its
            // mode, and the folder is what it looks like.
            let shown = shown()?;
            if meta.uid() != shown.owner {
                return Err(std::io::Error::other("it belongs to another user"));
            }
            if shown.keeps_modes && meta.mode() & refused != 0 {
                return Err(mode_refused());
            }
            // What is shown is what the drive shows of everything on it, and says nothing:
            // nothing there can be private, so a name is as good as a descriptor.
            say_no_modes_in(folder_of(path));
            return Ok(HeldFolder { folder, inside: path.to_path_buf(), by_name: true, keeps_modes: shown.keeps_modes });
        }
        let inside = PathBuf::from(format!("/proc/{}/fd/{}", std::process::id(), folder.as_raw_fd()));
        // Without the system's list of processes (a container without /proc) the descriptor
        // cannot be gone through by path, nor by tools that do not see this process (run in a
        // sandbox of their own). The name is then gone by, but only where nobody else can
        // rename the folder.
        let leads = std::fs::metadata(&inside).is_ok_and(|m| (m.dev(), m.ino()) == (meta.dev(), meta.ino()));
        if leads && reach(&inside) {
            HELD_NAMES.lock().unwrap_or_else(|e| e.into_inner()).push((inside.clone(), path.to_path_buf()));
            return Ok(HeldFolder { folder, inside, by_name: false, keeps_modes: true });
        }
        if !others_may_rename_in(folder_of(path)) {
            return Ok(HeldFolder { folder, inside: path.to_path_buf(), by_name: true, keeps_modes: true });
        }
        let why = if leads {
            "ffmpeg or ffprobe cannot open files through av1opt's open folders in /proc (are they run in a sandbox of their own?)"
        } else {
            "av1opt's open folders cannot be reached through /proc (is /proc there?)"
        };
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, why))
    }

    /// The path that leads into this folder, whatever has become of its name; its name where it
    /// is held by name.
    pub fn path(&self) -> &Path {
        &self.inside
    }

    /// Whether the folder is gone into by its name.
    pub fn by_name(&self) -> bool {
        self.by_name
    }

    /// The device and the number of the folder.
    fn identity(&self) -> std::io::Result<(u64, u64)> {
        self.folder.metadata().map(|m| (m.dev(), m.ino()))
    }

    /// Where the folder is now, if it is still there under some name.
    fn location(&self) -> Option<PathBuf> {
        use std::os::fd::AsRawFd;
        let now = if self.by_name { self.inside.clone() } else { std::fs::read_link(format!("/proc/self/fd/{}", self.folder.as_raw_fd())).ok()? };
        let identity = self.identity().ok()?;
        std::fs::symlink_metadata(&now).is_ok_and(|m| m.is_dir() && (m.dev(), m.ino()) == identity).then_some(now)
    }

    /// The plain file `name` in this folder, opened to be read; a link or anything else at
    /// that name is refused.
    pub fn open_file(&self, name: &OsStr) -> std::io::Result<std::fs::File> {
        use std::os::fd::{AsRawFd, FromRawFd};
        let c = std::ffi::CString::new(name.as_bytes()).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        // SAFETY: a descriptor we own and a valid NUL-terminated name; the new descriptor is
        // ours alone and is handed to a File, which closes it.
        let file = unsafe {
            let fd = libc::openat(self.folder.as_raw_fd(), c.as_ptr(), libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            std::fs::File::from_raw_fd(fd)
        };
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(std::io::Error::other(format!("not a plain file: it is {}", kind_of(&meta))));
        }
        Ok(file)
    }

    /// Moves `name`, in this folder, to `to`. Unless `replace`, fails with `AlreadyExists` when
    /// something has that name. The move starts from the descriptor, so what is moved is what
    /// was made in this folder.
    pub fn move_out(&self, name: &OsStr, to: &Path, replace: bool) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        let c = |b: &[u8]| std::ffi::CString::new(b).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput));
        let (from, to_c) = (c(name.as_bytes())?, c(to.as_os_str().as_bytes())?);
        let fd = self.folder.as_raw_fd();
        // SAFETY: a descriptor we own and valid NUL-terminated paths that outlive each call.
        let plain = || if unsafe { libc::renameat(fd, from.as_ptr(), libc::AT_FDCWD, to_c.as_ptr()) } == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) };
        if replace {
            return plain();
        }
        // SAFETY: as above.
        if unsafe { libc::renameat2(fd, from.as_ptr(), libc::AT_FDCWD, to_c.as_ptr(), libc::RENAME_NOREPLACE) } == 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        match e.raw_os_error() {
            // Old kernels and some filesystems (FUSE ones like ntfs-3g) don't know the flag.
            Some(libc::EINVAL | libc::ENOSYS | libc::ENOTSUP) => {
                if std::fs::symlink_metadata(to).is_ok() {
                    return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
                }
                plain()
            }
            _ => Err(e),
        }
    }

    /// Removes the folder and all it holds. What it holds is reached by way of the descriptor;
    /// the folder itself goes from wherever it is now, if that is still this folder.
    pub fn remove_all(&self) -> std::io::Result<()> {
        for entry in std::fs::read_dir(&self.inside)? {
            let entry = entry?;
            // Removing does not follow links, neither one at the entry's name nor ones below it.
            if entry.file_type()?.is_dir() {
                std::fs::remove_dir_all(entry.path())?;
            } else {
                std::fs::remove_file(entry.path())?;
            }
        }
        match self.location() {
            Some(now) => std::fs::remove_dir(now),
            None => Ok(()),
        }
    }
}

impl Drop for HeldFolder {
    fn drop(&mut self) {
        // The descriptor's number is free for another file once this one is closed, and its
        // path no longer means this folder.
        if !self.by_name {
            HELD_NAMES.lock().unwrap_or_else(|e| e.into_inner()).retain(|(inside, _)| *inside != self.inside);
        }
    }
}

/// The paths of the folders held through their descriptors, each with the folder's name. The
/// tools are given the first, but a `/proc/<pid>/fd/<n>` path in what is shown or kept means
/// nothing to the person, and leads nowhere once the run has ended.
static HELD_NAMES: std::sync::Mutex<Vec<(PathBuf, PathBuf)>> = std::sync::Mutex::new(Vec::new());

/// `text` with the path of each held folder's descriptor in it put back as the folder's name.
/// Tool output and av1opt's own messages go through this before they are shown or kept.
pub fn without_held(text: &str) -> std::borrow::Cow<'_, str> {
    let held = HELD_NAMES.lock().unwrap_or_else(|e| e.into_inner());
    let mut text = std::borrow::Cow::Borrowed(text);
    for (inside, name) in held.iter() {
        let (inside, name) = (inside.to_string_lossy(), name.to_string_lossy());
        if !text.contains(&*inside) {
            continue;
        }
        let mut put = String::with_capacity(text.len());
        let mut rest = &*text;
        while let Some(at) = rest.find(&*inside) {
            let after = &rest[at + inside.len()..];
            put.push_str(&rest[..at]);
            // `/proc/1/fd/6` is not the start of `/proc/1/fd/60`.
            put.push_str(if after.starts_with(|c: char| c.is_ascii_digit()) { &inside } else { &name });
            rest = after;
        }
        put.push_str(rest);
        text = std::borrow::Cow::Owned(put);
    }
    text
}

/// A path shown by the name of the held folder it is in rather than by its descriptor: see
/// [`without_held`].
pub struct Named<'a>(pub &'a Path);

impl std::fmt::Display for Named<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&without_held(&self.0.to_string_lossy()))
    }
}

/// Puts `data` at `path` by way of a file beside it that then takes its name: a reader
/// finds the old file or the new one, whole. The file beside it carries this process's
/// number and a count of its saves, so two programs or two threads saving at once do not
/// take each other's away.
pub fn replace_with(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = save_name(path);
    write_new(&tmp, data).and_then(|()| std::fs::rename(&tmp, path)).inspect_err(|_| drop(std::fs::remove_file(&tmp)))
}

/// The most bytes a file name can have in `folder`: 255 on most filesystems, 143 in an
/// encrypted home folder.
pub fn name_max(folder: &Path) -> usize {
    let folder = if folder.as_os_str().is_empty() { Path::new(".") } else { folder };
    let Ok(c) = std::ffi::CString::new(folder.as_os_str().as_bytes()) else { return 255 };
    // SAFETY: `c` is a valid NUL-terminated path.
    usize::try_from(unsafe { libc::pathconf(c.as_ptr(), libc::_PC_NAME_MAX) }).ok().filter(|n| *n > 0).unwrap_or(255)
}

/// `name` in at most `room` bytes, ended where a character ends: what is cut off must not
/// leave a name that is no longer text.
fn cut_name(name: &OsStr, room: usize) -> OsString {
    match name.to_str() {
        _ if name.len() <= room => name.to_owned(),
        Some(text) => text[..text.floor_char_boundary(room)].into(),
        None => OsStr::from_bytes(&name.as_bytes()[..room]).to_owned(),
    }
}

/// `name`, shortened if it must be for `added` more bytes to fit in a name in `folder`.
pub fn leave_room(folder: &Path, name: &OsStr, added: usize) -> OsString {
    cut_name(name, name_max(folder).saturating_sub(added))
}

/// The folder a path is in, for [`leave_room`].
pub fn folder_of(path: &Path) -> &Path {
    path.parent().unwrap_or(Path::new("."))
}

/// `path` as av1opt's record holds the files it names: its folder with every link on the way
/// resolved, and its own name as it is. A report can name one file by many paths (by way of
/// `/proc/self/root`, a link to a folder, `/a/./b`), and a path compared as text with what the
/// record holds would find nothing of a file the record knows well. A path whose folder is not
/// there is given back as it is.
pub fn in_real_folder(path: &Path) -> PathBuf {
    let real = path.parent().and_then(|d| std::fs::canonicalize(d).ok()).zip(path.file_name());
    real.map_or_else(|| path.to_path_buf(), |(d, name)| d.join(name))
}

/// Whether `a` and `b` name the same file: one path once their folders are resolved (see
/// [`in_real_folder`]), or, where both are there, one file on the drive. A second mount of a
/// folder, a drive that does not tell letter case apart, or a second (hard) link gives one
/// file other paths, which only the file's own number shows. A link at either name is taken
/// as itself, not as what it leads to.
pub fn same_file(a: &Path, b: &Path) -> bool {
    if a == b || in_real_folder(a) == in_real_folder(b) {
        return true;
    }
    match (std::fs::symlink_metadata(a), std::fs::symlink_metadata(b)) {
        (Ok(a), Ok(b)) => (a.dev(), a.ino()) == (b.dev(), b.ino()),
        _ => false,
    }
}

/// Whether two names could name the same file on some drive: the same once letter case is set
/// aside. What is compared by [`same_file`] where comparing everything would be slow.
pub fn may_be_same_name(a: &Path, b: &Path) -> bool {
    let lower = |p: &Path| p.file_name().map(|n| n.to_string_lossy().to_lowercase());
    lower(a).is_some() && lower(a) == lower(b)
}

/// The end of a name no other save has: this process's number and a count of its saves, and
/// then `ending`. Two names cut to fit can begin alike, so it is this that tells them apart.
pub fn unique_tail(ending: &str) -> String {
    static SAVES: AtomicU64 = AtomicU64::new(0);
    format!(".{}.{}.{ending}", std::process::id(), SAVES.fetch_add(1, Ordering::Relaxed))
}

/// A name beside `path` for what is about to take its place, unique to this save. As much of
/// the name as there is room for: one near the limit must still have a file beside it.
pub fn save_name(path: &Path) -> PathBuf {
    let tail = unique_tail("tmp");
    let mut name = leave_room(folder_of(path), path.file_name().unwrap_or_default(), tail.len());
    name.push(tail);
    path.with_file_name(name)
}

/// Adds `line` and a line end to the file at `path` in one write, so that two programs
/// adding at once do not cut into each other's lines. A last line left without its line end
/// (a program stopped while writing it) gets one first: the new line would otherwise join it,
/// and readers that skip damaged lines would lose both. `follow` says whether a link at that
/// name is gone through (a file of the user's own, which they may keep elsewhere) or
/// refused (a file in a folder others can write to).
pub fn append_line(path: &Path, line: &str, follow: bool) -> std::io::Result<()> {
    let open = |read: bool| {
        let mut o = std::fs::OpenOptions::new();
        o.read(read).create(true).append(true).mode(new_file_mode(path));
        if !follow {
            // Nor waited on: a pipe at that name would hold the run until someone read it.
            o.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        o.open(path)
    };
    // Reading is only for the last byte: a log that may only be written to still gets its line.
    let mut file = match open(true) {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => open(false)?,
        Err(e) if !follow && e.raw_os_error() == Some(libc::ELOOP) => return Err(std::io::Error::other("not a plain file: it is a link")),
        opened => opened?,
    };
    let meta = file.metadata()?;
    if !follow && !meta.is_file() {
        return Err(std::io::Error::other(format!("not a plain file: it is {}", kind_of(&meta))));
    }
    let start = if needs_line_end_first(&file) { "\n" } else { "" };
    file.write_all(format!("{start}{line}\n").as_bytes())
}

/// Whether the file ends in the middle of a line. When that cannot be found out it is taken to:
/// every reader of these files passes over a blank line, and none can part two joined ones.
fn needs_line_end_first(file: &std::fs::File) -> bool {
    use std::os::unix::fs::FileExt;
    let Ok(size) = file.metadata().map(|m| m.len()) else { return true };
    let mut last = *b"\n";
    size > 0 && file.read_exact_at(&mut last, size - 1).map_or(true, |()| last[0] != b'\n')
}

/// Reads `buffer` full from `file`, or as far as the file goes, adding what arrives to `got` as
/// it arrives. Returns how much there was.
fn read_counting(file: &mut std::fs::File, buffer: &mut [u8], got: &AtomicU64) -> std::io::Result<usize> {
    let mut have = 0;
    while have < buffer.len() {
        match file.read(&mut buffer[have..]) {
            Ok(0) => break,
            Ok(n) => {
                have += n;
                got.fetch_add(n as u64, Ordering::Relaxed);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(have)
}

/// What stands at a name, in words, for a message about something that is not a plain file.
pub fn kind_of(meta: &std::fs::Metadata) -> &'static str {
    use std::os::unix::fs::FileTypeExt;
    let kind = meta.file_type();
    if kind.is_symlink() {
        "a link"
    } else if kind.is_dir() {
        "a folder"
    } else if kind.is_fifo() {
        "a pipe"
    } else if kind.is_char_device() || kind.is_block_device() {
        "a device"
    } else if kind.is_socket() {
        "a socket"
    } else if kind.is_file() {
        "a plain file"
    } else {
        "something other than a file"
    }
}

/// The contents of the plain file at `path`, of at most `limit` bytes. What lies in a folder
/// others can write to may be a pipe, which would never finish opening, or a link (to
/// `/dev/zero`, say), or endless: each is refused with a message that says what is there, and
/// nothing is read from it. The open does not wait and does not follow a link, and the kind of
/// file is judged on the descriptor, so what is judged is what is read.
pub fn read_regular(path: &Path, limit: u64) -> std::io::Result<Vec<u8>> {
    use std::os::unix::fs::OpenOptionsExt;
    let too_large = || std::io::Error::other(format!("not a plain file of a sensible size: it is larger than the {} allowed", size_words(limit)));
    let file = match std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC).open(path) {
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Err(std::io::Error::other("not a plain file: it is a link")),
        opened => opened?,
    };
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(std::io::Error::other(format!("not a plain file: it is {}", kind_of(&meta))));
    }
    if meta.len() > limit {
        return Err(too_large());
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    // A file that grows while it is read is cut off one byte past the limit, and refused.
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(too_large());
    }
    Ok(bytes)
}

/// A size limit in words: whole MiB where it is one, bytes otherwise.
fn size_words(limit: u64) -> String {
    if limit >= 1 << 20 && limit.is_multiple_of(1 << 20) { format!("{} MiB", limit >> 20) } else { format!("{limit} bytes") }
}

/// The right to one piece of work, held by a lock on the file at its path. The system lets go of
/// the lock when the holder ends in any way, so a run that was killed stands in nobody's way;
/// the file it leaves means nothing.
pub struct Claim {
    file: std::fs::File,
    path: PathBuf,
}

impl Claim {
    /// `None` when another run, or another thread of this one, holds it. Something at the name
    /// that is not a plain file of this user's is refused (with [`ClaimRefused`] inside the
    /// error): in a folder others can write to, a pipe or a folder put there would otherwise
    /// let the run go on without the lock, and a file of theirs would let them hold it.
    pub fn take(path: &Path) -> std::io::Result<Option<Claim>> {
        Self::take_of(path, my_user(), &|| shown_in(folder_of(path)))
    }

    /// As [`Claim::take`] for user `me`, with `shown` asked what a file just made beside the
    /// lock shows, when one that was there already shows another owner.
    fn take_of(path: &Path, me: u32, shown: &dyn Fn() -> std::io::Result<Shown>) -> std::io::Result<Option<Claim>> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let refused = |why: String| std::io::Error::other(ClaimRefused(why));
        loop {
            // Not through a link: this is a folder others may be able to write to. Nor waiting
            // on a pipe. Made new if it can be, so that it is known to be this run's own.
            let open = |create: bool| std::fs::OpenOptions::new().read(true).write(true).create_new(create).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC).open(path);
            let (file, made) = match open(true) {
                Ok(file) => (file, true),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => match open(false) {
                    // Let go of and removed by its holder in the meantime.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Err(refused("it is a link".into())),
                    Err(e) if e.raw_os_error() == Some(libc::EISDIR) => return Err(refused("it is a folder".into())),
                    opened => (opened?, false),
                },
                Err(e) => return Err(e),
            };
            let found = file.metadata()?;
            if !found.is_file() {
                return Err(refused(format!("it is {}", kind_of(&found))));
            }
            // One made by this open is this run's whatever owner the drive shows for it; one
            // that was there has to show the owner a file made by this user there now shows
            // (another than this user on drives that map owners or keep none).
            if !made && found.uid() != me && shown()?.owner != found.uid() {
                return Err(refused("it belongs to another user".into()));
            }
            // SAFETY: flock on a descriptor we own; no memory is involved.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                let e = std::io::Error::last_os_error();
                return if e.kind() == std::io::ErrorKind::WouldBlock { Ok(None) } else { Err(e) };
            }
            // The holder before us removes the name as it lets go. If that happened after we
            // opened the file, we hold a lock on a file nobody else will find.
            let held = file.metadata()?;
            if std::fs::symlink_metadata(path).is_ok_and(|now| (now.dev(), now.ino()) == (held.dev(), held.ino())) {
                return Ok(Some(Claim { file, path: path.to_path_buf() }));
            }
        }
    }
}

/// Why the name of a [`Claim`] is not one av1opt can lock: something else is there.
#[derive(Debug)]
pub struct ClaimRefused(pub String);

impl std::fmt::Display for ClaimRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ClaimRefused {}

/// Whether a lock could not be taken because of what is at its name, rather than because the
/// filesystem gave none (some network drives have none, or fail with `EIO` or `ESTALE`).
pub fn claim_refused(e: &std::io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<ClaimRefused>())
}

impl Drop for Claim {
    /// The name goes first, while the lock still keeps others from believing they hold it. The
    /// lock is let go by hand: a program another thread is starting at this moment holds a copy
    /// of the descriptor until it has started, and closing alone would leave the lock with it.
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        let _ = std::fs::remove_file(&self.path);
        // SAFETY: flock on a descriptor we own; no memory is involved.
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// A hash (FNV-1a, the same in every build) of a file's size and of `chunk` bytes at its start,
/// its middle and its end: enough to tell files of one size apart without reading gigabytes
/// from a slow drive.
pub fn sample_digest(path: &Path, size: u64, chunk: u64) -> std::io::Result<u64> {
    sample_digest_counting(path, size, chunk, &AtomicU64::new(0))
}

/// [`sample_digest`], adding the bytes read to `got` as they arrive, for whoever is watching
/// whether the drive still answers.
pub fn sample_digest_counting(path: &Path, size: u64, chunk: u64, got: &AtomicU64) -> std::io::Result<u64> {
    sample_digest_of(std::fs::File::open(path)?, size, chunk, got)
}

/// [`sample_digest_counting`] of a file already open.
pub fn sample_digest_of(mut file: std::fs::File, size: u64, chunk: u64, got: &AtomicU64) -> std::io::Result<u64> {
    let add = |hash: u64, bytes: &[u8]| bytes.iter().fold(hash, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3));
    let mut hash = add(0xcbf29ce484222325, &size.to_le_bytes());
    let mut buffer = vec![0; (1 << 16).min(chunk as usize)];
    for start in [0, (size / 2).saturating_sub(chunk / 2), size.saturating_sub(chunk)] {
        file.seek(SeekFrom::Start(start))?;
        let mut left = chunk;
        while left > 0 {
            let room = buffer.len().min(usize::try_from(left).unwrap_or(usize::MAX));
            let have = read_counting(&mut file, &mut buffer[..room], got)?;
            hash = add(hash, &buffer[..have]);
            if have < room {
                break;
            }
            left -= have as u64;
        }
    }
    Ok(hash)
}

/// One step of [`whole_digest`]. FNV-1a's step taken a 64-bit word at a time, which is what makes
/// it fast, with a multiplier that spreads over the whole word and a turn that brings the top
/// bits back down: multiplying alone never carries a change in them anywhere else, and two such
/// changes would cancel out.
fn mix_word(hash: u64, word: u64) -> u64 {
    (hash ^ word).wrapping_mul(0x9e3779b97f4a7c15).rotate_left(23)
}

/// A hash of the first `size` bytes of the file at `path` and of `size`, for telling whether a
/// file is still the one it was, wherever it was written to. Not one against someone who wants
/// two files to match. Adds the bytes read to `got`; `stop` ends it early with an error.
pub fn whole_digest(path: &Path, size: u64, got: &AtomicU64, stop: &AtomicBool) -> std::io::Result<u64> {
    whole_digest_of(std::fs::File::open(path)?, size, got, stop)
}

/// [`whole_digest`] of a file already open, read from where it stands.
pub fn whole_digest_of(mut file: std::fs::File, size: u64, got: &AtomicU64, stop: &AtomicBool) -> std::io::Result<u64> {
    const BLOCK: usize = 1 << 20;
    let mut hash = mix_word(0xcbf29ce484222325, size);
    let mut buffer = vec![0; BLOCK];
    let mut left = size;
    while left > 0 {
        if stop.load(Ordering::Relaxed) {
            return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "no longer needed"));
        }
        let room = BLOCK.min(usize::try_from(left).unwrap_or(usize::MAX));
        if read_counting(&mut file, &mut buffer[..room], got)? < room {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the file is shorter than it was"));
        }
        // A block is a whole number of words, so only the last one can end inside a word.
        let (words, end) = buffer[..room].as_chunks::<8>();
        hash = words.iter().fold(hash, |h, word| mix_word(h, u64::from_le_bytes(*word)));
        if !end.is_empty() {
            let mut last = [0; 8];
            last[..end.len()].copy_from_slice(end);
            hash = mix_word(hash, u64::from_le_bytes(last));
        }
        left -= room as u64;
    }
    Ok(hash)
}

/// Writes a shell script for a test to run, and returns once it can be run. While the file
/// was open for writing, another thread of the tests may have started a program, which holds
/// a copy of everything open until it has started: until then the system refuses to run the
/// script (`Text file busy`). The script answers `--ready` by doing nothing, for this.
#[cfg(test)]
pub fn write_script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, format!("#!/bin/sh\n[ \"$1\" = --ready ] && exit 0\n{body}")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    loop {
        match std::process::Command::new(path).arg("--ready").status() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => std::thread::sleep(std::time::Duration::from_millis(5)),
            done => {
                done.unwrap();
                return;
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn only_absolute_paths_count_as_base_folders() {
        let folder = |value: Option<&str>, home: Option<&str>| xdg_folder_from(value.map(OsString::from), home.map(OsString::from), ".cache");
        assert_eq!(folder(Some("/x"), Some("/home/a")), Some(PathBuf::from("/x")));
        for ignored in [None, Some(""), Some("relative"), Some("./here")] {
            assert_eq!(folder(ignored, Some("/home/a")), Some(PathBuf::from("/home/a/.cache")), "{ignored:?}");
        }
        for no_home in [None, Some(""), Some("home")] {
            assert_eq!(folder(Some(""), no_home), None, "{no_home:?}");
        }
    }

    fn room(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("av1opt-files-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_file_counts_as_written_by_this_user_alone_only_if_nobody_else_could_have() {
        let dir = room("own-file");
        let path = dir.join("a.av1opt.json");
        std::fs::write(&path, b"{}").unwrap();
        let set = |p: &Path, mode: u32| std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
        set(&dir, 0o755);
        set(&path, 0o644);
        let judged = |me: u32| others_may_have_written_of(&path, &std::fs::File::open(&path).unwrap(), me);
        assert_eq!(judged(my_user()), None);
        // A file and a folder that look like another user's.
        assert_eq!(judged(my_user() + 1).as_deref(), Some("it belongs to another user"));
        set(&path, 0o666);
        assert_eq!(judged(my_user()).as_deref(), Some("other users can write to it"));
        set(&path, 0o644);
        set(&dir, 0o777);
        assert!(judged(my_user()).is_some_and(|why| why.starts_with("other users can write to its folder")));
        // The sticky bit keeps others to their own names.
        set(&dir, 0o1777);
        assert_eq!(judged(my_user()), None);
        set(&dir, 0o755);
        // A second name, which could be anywhere on the drive.
        std::fs::hard_link(&path, dir.join("other")).unwrap();
        assert!(judged(my_user()).is_some_and(|why| why.contains("hard links")));
        std::fs::remove_file(dir.join("other")).unwrap();
        // The file opened has to be the one at its name in the folder judged.
        let opened = std::fs::File::open(&path).unwrap();
        std::fs::rename(&path, dir.join("moved")).unwrap();
        std::fs::write(&path, b"{}").unwrap();
        assert!(others_may_have_written(&path, &opened).is_some_and(|why| why.contains("moved or replaced")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The number of a process that has ended.
    fn gone_process() -> u32 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    #[test]
    fn what_is_kept_for_this_user_is_for_this_user_alone() {
        use std::os::unix::fs::symlink;
        let dir = room("private");
        let mode = |p: &Path| std::fs::symlink_metadata(p).unwrap().mode() & 0o777;
        // A new folder and one left open by an earlier version both end up closed.
        let (new, old) = (dir.join("a/new"), dir.join("old"));
        private_folder(&new).unwrap();
        std::fs::create_dir(&old).unwrap();
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o755)).unwrap();
        private_folder(&old).unwrap();
        assert_eq!((mode(&new), mode(&old)), (0o700, 0o700));

        // Files made in a closed folder are closed, however they are made.
        write_new(&old.join("written"), b"x").unwrap();
        replace_with(&old.join("replaced"), b"x").unwrap();
        append_line(&old.join("added"), "x", true).unwrap();
        for name in ["written", "replaced", "added"] {
            assert_eq!(mode(&old.join(name)), 0o600, "{name}");
        }
        // Elsewhere the umask decides: a report beside an output stays readable as before.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        write_new(&dir.join("beside"), b"x").unwrap();
        assert_ne!(mode(&dir.join("beside")) & 0o600, 0);
        assert_eq!(new_file_mode(&dir.join("beside")), 0o666);

        // What is there already is closed too, and a link leads nothing astray.
        let outside = dir.join("outside");
        std::fs::write(&outside, b"x").unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o644)).unwrap();
        symlink(&outside, old.join("link")).unwrap();
        let (inner, file) = (old.join("inner"), old.join("inner/file"));
        std::fs::create_dir(&inner).unwrap();
        std::fs::write(&file, b"x").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o664)).unwrap();
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o775)).unwrap();
        tighten_below(&old);
        assert_eq!((mode(&inner), mode(&file), mode(&outside)), (0o700, 0o600, 0o644));
        make_private(&outside).unwrap();
        assert_eq!(mode(&outside), 0o600);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_run_folder_is_new_private_and_says_whose_it_is() {
        let dir = room("run-folder");
        let a = make_run_folder(&dir, ".t-").unwrap();
        let b = make_run_folder(&dir, ".t-").unwrap();
        assert_ne!(a, b);
        let name = a.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with(&format!(".t-{}-{}-", std::process::id(), process_start(std::process::id()).unwrap())), "{name}");
        // Nobody can have guessed it: 16 hexadecimal digits of chance.
        assert_eq!(name.rsplit('-').next().unwrap().len(), 16);
        let m = std::fs::symlink_metadata(&a).unwrap();
        assert!(m.is_dir() && m.mode() & 0o777 == 0o700, "{:o}", m.mode());
        assert_eq!(run_folder_in_use(name.strip_prefix(".t-").unwrap()), Some(true));
        // A folder that is there is not taken over, whatever it holds.
        assert_eq!(make_kept_folder(&a, &|_| true).unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_new_file_never_goes_through_a_link() {
        let dir = room("no-link");
        std::fs::write(dir.join("victim"), b"precious").unwrap();
        std::os::unix::fs::symlink(dir.join("victim"), dir.join("trap")).unwrap();
        write_new(&dir.join("trap"), b"new").unwrap();
        assert_eq!(std::fs::read(dir.join("victim")).unwrap(), b"precious");
        assert!(!std::fs::symlink_metadata(dir.join("trap")).unwrap().file_type().is_symlink());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_this_users_folders_of_ended_runs_are_cleared_and_never_through_a_link() {
        let dir = room("abandoned");
        let (me, gone) = (my_user(), gone_process());
        let keep = dir.join("keep");
        std::fs::create_dir(&keep).unwrap();
        std::fs::write(keep.join("precious"), b"x").unwrap();
        let make = |name: &str| {
            let d = dir.join(name);
            std::fs::create_dir(&d).unwrap();
            d
        };
        // Of a run that ended, in both ways of naming; one holds a link leading out.
        let ended = make(&format!(".t-{gone}-1-00ff"));
        std::os::unix::fs::symlink(&keep, ended.join("out")).unwrap();
        let older = make(&format!(".t-{gone}"));
        // Of this run; of a process with this number that started at another time.
        let running = make_run_folder(&dir, ".t-").unwrap();
        let reused = make(&format!(".t-{}-1-00ff", std::process::id()));
        // A link with such a name, names that are not a run's, and another prefix.
        let link = dir.join(format!(".t-{gone}-2-00ff"));
        std::os::unix::fs::symlink(&keep, &link).unwrap();
        let others = [make(&format!(".t-{gone}-1")), make(&format!(".t-{gone}-1-zz")), make(".t-x-1-00ff"), make(&format!(".u-{gone}-1-00ff"))];

        // Another user's look the same by name: the owner decides.
        remove_abandoned_folders_of(&dir, ".t-", me + 1);
        assert!(ended.exists() && older.exists() && reused.exists());

        remove_abandoned_folders(&dir, ".t-");
        assert!(!ended.exists() && !older.exists() && !reused.exists());
        assert!(running.exists());
        assert!(std::fs::symlink_metadata(&link).is_ok(), "a link is no folder of a run");
        assert!(others.iter().all(|d| d.exists()), "{others:?}");
        assert_eq!(std::fs::read(keep.join("precious")).unwrap(), b"x");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// What a filesystem that keeps modes and owners shows of user `me`'s new file, or one that
    /// keeps neither, as a FAT drive mounted for `me` does.
    fn shows(owner: u32, keeps_modes: bool) -> impl Fn() -> std::io::Result<Shown> {
        move || Ok(Shown { owner, keeps_modes })
    }

    #[test]
    fn a_kept_folder_of_another_user_is_not_believed() {
        let dir = room("kept");
        let kept = dir.join("work");
        make_kept_folder(&kept, &|_| true).unwrap();
        assert_eq!(std::fs::metadata(&kept).unwrap().mode() & 0o777, 0o700);
        std::fs::write(kept.join("plan.json"), b"{}").unwrap();
        assert_eq!(std::fs::read(check_kept_folder(&kept, true).unwrap().unwrap().path().join("plan.json")).unwrap(), b"{}");
        assert!(check_kept_folder(&dir.join("none"), true).unwrap().is_none());
        assert!(check_kept_folder_of(&kept, my_user() + 1, true, &shows(my_user() + 1, true)).unwrap_err().contains("another user"));
        // One an earlier version left open to reading is closed, by way of its descriptor.
        std::fs::set_permissions(&kept, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(check_kept_folder(&kept, true).unwrap().is_some());
        assert_eq!(std::fs::metadata(&kept).unwrap().mode() & 0o777, 0o700);
        std::fs::set_permissions(&kept, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert!(check_kept_folder(&kept, true).unwrap_err().contains("can write to it"));
        std::fs::set_permissions(&kept, std::fs::Permissions::from_mode(0o700)).unwrap();
        // A link or something other than a folder at the name is never gone through, a pipe
        // included, which would never finish opening.
        std::os::unix::fs::symlink(&kept, dir.join("link")).unwrap();
        assert_eq!(check_kept_folder(&dir.join("link"), true).unwrap_err(), "it is a link");
        make_pipe(&dir.join("pipe"));
        assert_eq!(check_kept_folder(&dir.join("pipe"), true).unwrap_err(), "it is not a folder");
        // A file nobody made here counts as much as a link.
        std::fs::create_dir(kept.join("sub")).unwrap();
        assert!(check_kept_folder(&kept, true).unwrap_err().contains("sub in it"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_kept_folder_on_a_drive_without_modes_or_owners_is_gone_on_from_by_its_name() {
        let dir = room("kept-no-modes");
        let kept = dir.join("work");
        make_kept_folder(&kept, &|_| true).unwrap();
        std::fs::write(kept.join("plan.json"), b"{}").unwrap();
        let me = my_user();
        // An NTFS drive shows every folder as 0777: nothing can be private there, and nothing
        // is tried, which such a drive would refuse.
        std::fs::set_permissions(&kept, std::fs::Permissions::from_mode(0o777)).unwrap();
        let held = check_kept_folder_of(&kept, me, true, &shows(me, false)).unwrap().unwrap();
        assert!(held.by_name() && held.path() == kept);
        assert_eq!(std::fs::metadata(&kept).unwrap().mode() & 0o777, 0o777);
        // A drive mounted for root shows root for every file this user makes, and for the
        // folder: as this test cannot make a folder of root's, "this user" is another number.
        std::fs::set_permissions(&kept, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(check_kept_folder_of(&kept, me + 1, true, &shows(me, false)).unwrap().unwrap().by_name());
        // Where the drive shows another owner for a new file than for the folder, the folder is
        // somebody else's after all; and where it keeps modes, an open folder is refused.
        assert!(check_kept_folder_of(&kept, me + 1, true, &shows(me + 2, false)).unwrap_err().contains("another user"));
        std::fs::set_permissions(&kept, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(check_kept_folder_of(&kept, me, true, &shows(me, true)).unwrap_err().contains("can write to it"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_name_near_the_limit_can_still_be_saved_over() {
        let dir = std::env::temp_dir().join(format!("av1opt-files-long-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("é".repeat(name_max(&dir) / 2));
        std::fs::write(&path, b"old").unwrap();
        replace_with(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(&dir).unwrap();

        // Cut where a character ends, and byte by byte where there are none.
        assert_eq!(cut_name(OsStr::new("aé"), 2), "a");
        assert_eq!(cut_name(OsStr::new("aé"), 3), "aé");
        assert_eq!(cut_name(OsStr::from_bytes(b"a\xff\xfe"), 2).as_bytes(), b"a\xff");
    }

    #[test]
    fn saves_at_the_same_moment_all_succeed() {
        let dir = std::env::temp_dir().join(format!("av1opt-files-saves-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::thread::scope(|s| {
            for thread in 0..8 {
                let path = &path;
                s.spawn(move || {
                    for turn in 0..200 {
                        replace_with(path, format!("{thread} {turn}").as_bytes()).unwrap();
                    }
                });
            }
        });
        // One whole file, and nothing left beside it.
        assert!(std::fs::read_to_string(&path).unwrap().ends_with(" 199"));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_link_at_the_name_is_replaced_not_written_through() {
        let dir = std::env::temp_dir().join(format!("av1opt-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (target, name) = (dir.join("target"), dir.join("report.json"));
        std::fs::write(&target, b"keep me").unwrap();
        std::os::unix::fs::symlink(&target, &name).unwrap();
        write_new(&name, b"new").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"keep me");
        assert_eq!(std::fs::read(&name).unwrap(), b"new");
        assert!(!std::fs::symlink_metadata(&name).unwrap().file_type().is_symlink());

        // The same by way of the file beside it, which is not left behind.
        std::fs::remove_file(&name).unwrap();
        std::os::unix::fs::symlink(&target, &name).unwrap();
        replace_with(&name, b"newer").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"keep me");
        assert_eq!(std::fs::read(&name).unwrap(), b"newer");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);

        // A line is added to a file there is, or to a new one; through a link only when asked.
        let log = dir.join("log.tsv");
        append_line(&log, "one", false).unwrap();
        append_line(&log, "two", false).unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "one\ntwo\n");
        let link = dir.join("link.tsv");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(append_line(&link, "x", false).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"keep me");
        append_line(&link, "x", true).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep me\nx\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_line_cut_short_does_not_take_the_next_one_with_it() {
        let dir = std::env::temp_dir().join(format!("av1opt-files-torn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("log.jsonl");
        std::fs::write(&log, b"{\"whole\":1}\n{\"cut sh").unwrap();
        append_line(&log, "{\"next\":2}", true).unwrap();
        append_line(&log, "{\"last\":3}", true).unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "{\"whole\":1}\n{\"cut sh\n{\"next\":2}\n{\"last\":3}\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_log_that_cannot_be_read_still_gets_its_line() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("av1opt-files-writeonly-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("log.jsonl");
        std::fs::write(&log, b"{\"cut sh").unwrap();
        std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o200)).unwrap();
        append_line(&log, "{\"next\":2}", true).unwrap();
        append_line(&log, "{\"last\":3}", true).unwrap();
        std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o600)).unwrap();
        // Whatever the last byte was, the new line stands alone. (A blank line is what it costs
        // where the file did end in a line end, and for someone allowed to read it there is none.)
        let text = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines, ["{\"cut sh", "{\"next\":2}", "{\"last\":3}"], "{text:?}");

        // A last byte that cannot be read counts as a line left open.
        let write_only = std::fs::File::options().append(true).open(&log).unwrap();
        assert!(needs_line_end_first(&write_only));
        let readable = std::fs::File::open(&log).unwrap();
        assert!(!needs_line_end_first(&readable));
        std::fs::write(&log, b"").unwrap();
        assert!(!needs_line_end_first(&std::fs::File::options().append(true).open(&log).unwrap()), "an empty file has no line to end");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_hash_of_a_whole_file_sees_every_byte_and_stays_what_reports_hold() {
        let dir = std::env::temp_dir().join(format!("av1opt-files-whole-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.bin");
        let digest = |size| whole_digest(&file, size, &AtomicU64::new(0), &AtomicBool::new(false));
        // Longer than a block, and not a whole number of words.
        let size = (1 << 20) + 13;
        let bytes: Vec<u8> = (0..size).map(|i: u64| (i.wrapping_mul(31) >> 3) as u8).collect();
        std::fs::write(&file, &bytes).unwrap();
        let hash = digest(size).unwrap();
        // Written out: reports keep this value, so another hash would call every file changed.
        assert_eq!(format!("{hash:016x}"), "31aae172d094ef06");
        for (at, bit) in [(0, 1), (7, 0x80), (1 << 19, 0x80), ((1 << 20) - 1, 0x80), ((1 << 20) + 12, 1)] {
            let mut other = bytes.clone();
            other[at] ^= bit;
            std::fs::write(&file, &other).unwrap();
            assert_ne!(digest(size).unwrap(), hash, "byte {at}");
        }
        // Top bits changed in two words do not cancel each other out.
        let mut other = bytes.clone();
        other[7] ^= 0x80;
        other[15] ^= 0x80;
        std::fs::write(&file, &other).unwrap();
        assert_ne!(digest(size).unwrap(), hash);
        // Nothing is read past the size asked for, and a shorter file is an error.
        std::fs::write(&file, &bytes).unwrap();
        assert_eq!(digest(size).unwrap(), hash);
        assert!(digest(size + 1).is_err());
        let stopped = whole_digest(&file, size, &AtomicU64::new(0), &AtomicBool::new(true)).unwrap_err();
        assert_eq!(stopped.kind(), std::io::ErrorKind::Interrupted);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_held_folder_is_reached_through_its_descriptor_whatever_is_done_to_its_name() {
        use std::os::unix::fs::symlink;
        let dir = room("held");
        let (made, moved, theirs) = (dir.join("made"), dir.join("moved"), dir.join("theirs"));
        let held = make_kept_folder(&made, &|_| true).unwrap();
        std::fs::create_dir(&theirs).unwrap();
        std::fs::write(theirs.join("partial.mp4"), b"theirs").unwrap();
        // Someone renames the folder and puts a link to one of theirs at its name.
        std::fs::rename(&made, &moved).unwrap();
        symlink(&theirs, &made).unwrap();
        write_new(&held.path().join("partial.mp4"), b"ours").unwrap();
        std::fs::create_dir(held.path().join("inner")).unwrap();
        std::fs::write(held.path().join("inner/x"), b"x").unwrap();
        assert_eq!(std::fs::read(moved.join("partial.mp4")).unwrap(), b"ours");
        assert_eq!(std::fs::read(theirs.join("partial.mp4")).unwrap(), b"theirs");
        // What is read and what is moved out is what was made in it.
        let opened = held.open_file(OsStr::new("partial.mp4")).unwrap();
        let mut read = String::new();
        (&opened).read_to_string(&mut read).unwrap();
        assert_eq!(read, "ours");
        assert!(held.open_file(OsStr::new("inner")).unwrap_err().to_string().contains("it is a folder"));
        symlink(theirs.join("partial.mp4"), moved.join("link")).unwrap();
        assert_eq!(held.open_file(OsStr::new("link")).unwrap_err().raw_os_error(), Some(libc::ELOOP));
        std::fs::write(dir.join("taken"), b"taken").unwrap();
        assert_eq!(held.move_out(OsStr::new("partial.mp4"), &dir.join("taken"), false).unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
        held.move_out(OsStr::new("partial.mp4"), &dir.join("out.mp4"), false).unwrap();
        assert_eq!(std::fs::read(dir.join("out.mp4")).unwrap(), b"ours");
        assert_eq!(std::fs::read(dir.join("taken")).unwrap(), b"taken");
        let (landed, kept) = (std::fs::metadata(dir.join("out.mp4")).unwrap(), opened.metadata().unwrap());
        assert_eq!((landed.dev(), landed.ino()), (kept.dev(), kept.ino()));
        // Removing it removes the real folder where it now is, and nothing of theirs.
        held.remove_all().unwrap();
        assert!(!moved.exists());
        assert_eq!(std::fs::read(theirs.join("partial.mp4")).unwrap(), b"theirs");
        assert!(std::fs::symlink_metadata(&made).unwrap().file_type().is_symlink());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_a_private_folder_of_this_users_is_held() {
        let dir = room("hold-refused");
        let open = dir.join("open");
        let me = my_user();
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(HeldFolder::made(&open, &|_| true).unwrap_err().to_string().contains("enter"));
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert!(HeldFolder::made(&open, &|_| true).unwrap_err().to_string().contains("write"));
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(HeldFolder::made_of(&open, me + 1, &|_| true, &shows(me + 1, true)).unwrap_err().to_string().contains("another user"));
        let held = HeldFolder::made(&open, &|_| true).unwrap();
        assert!(!held.by_name() && held.path().starts_with("/proc"));
        std::os::unix::fs::symlink(&open, dir.join("link")).unwrap();
        assert_eq!(HeldFolder::made(&dir.join("link"), &|_| true).err().and_then(|e| e.raw_os_error()), Some(libc::ELOOP));
        std::fs::write(dir.join("file"), b"").unwrap();
        assert_eq!(HeldFolder::made(&dir.join("file"), &|_| true).err().and_then(|e| e.raw_os_error()), Some(libc::ENOTDIR));
        // A folder av1opt has just made is empty: one that is not was put at its name.
        std::fs::write(open.join("chunk00000.obu"), b"of another video").unwrap();
        assert!(HeldFolder::made(&open, &|_| true).unwrap_err().to_string().contains("was put at its name"));
        assert!(open.join("chunk00000.obu").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn what_is_said_of_a_held_folder_names_it_and_not_its_descriptor() {
        let dir = room("held-names");
        let made = dir.join("made");
        std::fs::DirBuilder::new().mode(0o700).create(&made).unwrap();
        let held = HeldFolder::made(&made, &|_| true).unwrap();
        let inside = held.path().to_string_lossy().into_owned();
        assert!(inside.starts_with("/proc/"), "{inside}");
        let said = format!("Error opening output {inside}/nodir/partial.mp4: No such file or directory\nat {inside}");
        let named = made.to_string_lossy();
        assert_eq!(without_held(&said), format!("Error opening output {named}/nodir/partial.mp4: No such file or directory\nat {named}"));
        assert_eq!(Named(&held.path().join("partial.mp4")).to_string(), format!("{named}/partial.mp4"));
        // Another descriptor whose number starts with the same digits is not this folder.
        assert_eq!(without_held(&format!("{inside}0/x")), format!("{inside}0/x"));
        drop(held);
        assert!(!without_held(&said).contains(&*named), "{said}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_folder_just_made_on_a_drive_without_modes_or_owners_is_used_by_its_name() {
        let dir = room("made-no-modes");
        let made = dir.join("made");
        let me = my_user();
        // FAT and exFAT drives mounted for this user show 0755, NTFS ones 0777, whatever the
        // folder was made with; a file just made shows the same lack.
        for mode in [0o755, 0o777] {
            std::fs::create_dir(&made).unwrap();
            std::fs::set_permissions(&made, std::fs::Permissions::from_mode(mode)).unwrap();
            let held = HeldFolder::made_of(&made, me, &|_| panic!("the tools were asked"), &shows(me, false)).unwrap();
            assert!(held.by_name() && held.path() == made);
            write_new(&held.path().join("x"), b"x").unwrap();
            held.remove_all().unwrap();
            assert!(!made.exists());
        }
        // A drive that shows root, or the user all are mapped to, for every file.
        std::fs::create_dir(&made).unwrap();
        assert!(HeldFolder::made_of(&made, me + 1, &|_| true, &shows(me, false)).unwrap().by_name());
        // Where the drive keeps modes and owners, a folder that does not show what it was made
        // with is not the one made.
        std::fs::set_permissions(&made, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(HeldFolder::made_of(&made, me, &|_| true, &shows(me, true)).unwrap_err().to_string().contains("can write"));
        assert!(HeldFolder::made_of(&made, me + 1, &|_| true, &shows(me + 1, true)).unwrap_err().to_string().contains("another user"));
        // And where a new file there shows another owner than the folder, the folder is not ours.
        assert!(HeldFolder::made_of(&made, me + 1, &|_| true, &shows(me + 2, false)).unwrap_err().to_string().contains("another user"));
        // The probe itself: this test's own folder keeps modes and owners, and leaves nothing.
        let shown = shown_in(&dir).unwrap();
        assert!(shown.owner == me && shown.keeps_modes, "{shown:?}");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn without_a_way_through_its_descriptor_a_folder_is_used_by_name_only_where_nobody_else_can_rename_it() {
        let dir = room("no-way-through");
        let made = dir.join("made");
        let me = my_user();
        let cannot = |_: &Path| false;
        let real = shows(me, true);
        let make = || std::fs::DirBuilder::new().mode(0o700).create(&made).unwrap();
        // Only this user, or the sticky bit: its name is safe.
        for mode in [0o700, 0o755, 0o1777] {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).unwrap();
            make();
            let held = HeldFolder::made_of(&made, me, &cannot, &real).unwrap();
            assert!(held.by_name() && held.path() == made, "{mode:o}");
            held.remove_all().unwrap();
        }
        // Anyone can rename it: refused, and the empty folder goes again.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        make();
        let e = HeldFolder::made_of(&made, me, &cannot, &real).unwrap_err();
        assert!(e.kind() == std::io::ErrorKind::Unsupported && e.to_string().contains("cannot open files through av1opt's open folders"), "{e}");
        assert!(!made.exists(), "the refused folder was left behind");
        // Written by its group only, the group being this user's own and nobody else's (the
        // umask of 0002 makes every folder so): as good as this user's alone.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o775)).unwrap();
        // SAFETY: getegid has no failure mode.
        let own_group = unsafe { libc::getegid() };
        std::os::unix::fs::chown(&dir, None, Some(own_group)).unwrap();
        assert_eq!(others_may_rename_in(&dir), !group_is_mine_alone(own_group));
        // Another group this user is in may hold others.
        let mut groups = vec![0 as libc::gid_t; 256];
        // SAFETY: getgroups writes at most as many numbers as it is told there is room for.
        let count = unsafe { libc::getgroups(groups.len() as libc::c_int, groups.as_mut_ptr()) };
        if let Some(other) = groups[..count.max(0) as usize].iter().copied().find(|g| *g != own_group) {
            assert!(!group_is_mine_alone(other));
            std::os::unix::fs::chown(&dir, None, Some(other)).unwrap();
            assert!(others_may_rename_in(&dir));
            make();
            assert_eq!(HeldFolder::made_of(&made, me, &cannot, &real).unwrap_err().kind(), std::io::ErrorKind::Unsupported);
            std::os::unix::fs::chown(&dir, None, Some(own_group)).unwrap();
        }
        assert!(!group_is_mine_alone(0) || me == 0);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_lock_made_by_this_run_is_its_own_and_one_found_there_has_to_show_the_owner_new_files_show() {
        let dir = room("claim-owner");
        let me = my_user();
        let (made, found) = (dir.join("made"), dir.join("found"));
        // A drive mounted for root shows the lock just made as root's: it is this run's all the same.
        let claim = Claim::take_of(&made, me + 1, &|| panic!("a new lock was judged by its owner")).unwrap();
        assert!(claim.is_some());
        drop(claim);
        // One left there by somebody else is refused; one that shows the owner every new file
        // there shows is this user's.
        std::fs::write(&found, b"").unwrap();
        let e = Claim::take_of(&found, me + 1, &shows(me + 1, true)).err().expect("another user's lock was taken");
        assert!(claim_refused(&e) && e.to_string() == "it belongs to another user", "{e}");
        assert!(found.exists());
        assert!(Claim::take_of(&found, me + 1, &shows(me, false)).unwrap().is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn something_planted_at_a_claims_name_is_refused_and_left_there() {
        let dir = room("claim-planted");
        let (pipe, folder, link, target) = (dir.join("pipe"), dir.join("folder"), dir.join("link"), dir.join("target"));
        make_pipe(&pipe);
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(&target, b"keep").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        for (path, why) in [(&pipe, "it is a pipe"), (&folder, "it is a folder"), (&link, "it is a link")] {
            let e = Claim::take(path).err().unwrap_or_else(|| panic!("{} was taken", path.display()));
            assert_eq!(e.to_string(), why);
            assert!(claim_refused(&e));
            assert!(std::fs::symlink_metadata(path).is_ok(), "{} was removed", path.display());
        }
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_claim_has_one_holder_and_leaves_nothing_behind() {
        let dir = std::env::temp_dir().join(format!("av1opt-files-claim-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lock");
        let first = Claim::take(&path).unwrap().unwrap();
        assert!(Claim::take(&path).unwrap().is_none(), "held, even within one process");
        drop(first);
        assert!(!path.exists(), "the holder takes its file along");

        // What a killed run leaves is no claim.
        std::fs::write(&path, b"").unwrap();
        drop(Claim::take(&path).unwrap().unwrap());

        // Taken and given back as fast as threads can: never two holders at a time, also
        // when a holder removes the file that the next one has just opened.
        let holders = std::sync::atomic::AtomicU32::new(0);
        let held = std::sync::atomic::AtomicU32::new(0);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    for _ in 0..2000 {
                        if let Some(claim) = Claim::take(&path).unwrap() {
                            assert_eq!(holders.fetch_add(1, Ordering::SeqCst), 0, "two holders");
                            held.fetch_add(1, Ordering::SeqCst);
                            holders.fetch_sub(1, Ordering::SeqCst);
                            drop(claim);
                        }
                    }
                });
            }
        });
        assert!(held.load(Ordering::SeqCst) > 0);
        assert!(!path.exists());

        // A link at that name is not followed to whatever it points at.
        let target = dir.join("target");
        std::fs::write(&target, b"keep").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(Claim::take(&path).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_script_is_ready_once_nobody_holds_it_open_for_writing() {
        let dir = std::env::temp_dir().join(format!("av1opt-files-script-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("tool");
        // Held open by someone else for a moment, as by a program being started.
        let held = std::fs::File::create(&script).unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            drop(held);
        });
        write_script(&script, "exit 3\n");
        release.join().unwrap();
        assert_eq!(std::process::Command::new(&script).status().unwrap().code(), Some(3));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A pipe at `path`, made as anyone who can write to a folder can make one.
    pub(crate) fn make_pipe(path: &Path) {
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o644) }, 0);
    }

    #[test]
    fn only_a_plain_file_within_its_limit_is_read_and_nothing_else_is_waited_on() {
        let dir = room("read-regular");
        let (report, pipe, link, long, folder) = (dir.join("r.json"), dir.join("pipe"), dir.join("link"), dir.join("long"), dir.join("folder"));
        std::fs::write(&report, b"{}").unwrap();
        assert_eq!(read_regular(&report, 2).unwrap(), b"{}");
        make_pipe(&pipe);
        std::os::unix::fs::symlink("/dev/zero", &link).unwrap();
        std::fs::write(&long, vec![b' '; 3]).unwrap();
        std::fs::create_dir(&folder).unwrap();
        // Each in a thread of its own, so that a wait shows as a failed test, not a hung one.
        let said = |path: PathBuf, limit: u64| {
            let (tell, told) = std::sync::mpsc::channel();
            std::thread::spawn(move || drop(tell.send(read_regular(&path, limit).map_err(|e| e.to_string()))));
            told.recv_timeout(std::time::Duration::from_secs(2)).expect("the read waited").unwrap_err()
        };
        assert_eq!(said(pipe, 1 << 20), "not a plain file: it is a pipe");
        assert_eq!(said(link, 1 << 20), "not a plain file: it is a link");
        assert_eq!(said(PathBuf::from("/dev/zero"), 1 << 20), "not a plain file: it is a device");
        assert_eq!(said(folder, 1 << 20), "not a plain file: it is a folder");
        assert_eq!(said(long, 2), "not a plain file of a sensible size: it is larger than the 2 bytes allowed");
        assert_eq!(size_words(8 << 20), "8 MiB");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
