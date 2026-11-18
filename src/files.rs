//! Writing files where another program, or another user, may be writing too.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Writes `data` to a new file at `path`. What was at that name is removed first and the
/// file is made anew: a link left there (in an output folder others can write to) is
/// replaced, never written through.
pub fn write_new(path: &Path, data: &[u8]) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    f.write_all(data)
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
    use std::os::unix::fs::{FileExt, OpenOptionsExt};
    let mut o = std::fs::OpenOptions::new();
    o.read(true).create(true).append(true);
    if !follow {
        o.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = o.open(path)?;
    let size = file.metadata()?.len();
    let mut last = *b"\n";
    if size > 0 {
        file.read_exact_at(&mut last, size - 1)?;
    }
    let start = if last[0] == b'\n' { "" } else { "\n" };
    file.write_all(format!("{start}{line}\n").as_bytes())
}

/// A hash (FNV-1a, the same in every build) of a file's size and of `chunk` bytes at its start,
/// its middle and its end: enough to tell files of one size apart without reading gigabytes
/// from a slow drive.
pub fn sample_digest(path: &Path, size: u64, chunk: u64) -> std::io::Result<u64> {
    let add = |hash: u64, bytes: &[u8]| bytes.iter().fold(hash, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3));
    let mut file = std::fs::File::open(path)?;
    let mut hash = add(0xcbf29ce484222325, &size.to_le_bytes());
    let mut buffer = Vec::with_capacity(chunk as usize);
    for start in [0, (size / 2).saturating_sub(chunk / 2), size.saturating_sub(chunk)] {
        file.seek(SeekFrom::Start(start))?;
        buffer.clear();
        (&file).take(chunk).read_to_end(&mut buffer)?;
        hash = add(hash, &buffer);
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
mod tests {
    use super::*;

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
}
