//! Writing files where another program, or another user, may be writing too.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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
    use std::os::unix::fs::OpenOptionsExt;
    let open = |read: bool| {
        let mut o = std::fs::OpenOptions::new();
        o.read(read).create(true).append(true);
        if !follow {
            o.custom_flags(libc::O_NOFOLLOW);
        }
        o.open(path)
    };
    // Reading is only for the last byte: a log that may only be written to still gets its line.
    let mut file = match open(true) {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => open(false)?,
        opened => opened?,
    };
    let start = if needs_line_end_first(&file) { "\n" } else { "" };
    file.write_all(format!("{start}{line}\n").as_bytes())
}

/// Whether the file ends in the middle of a line. When that cannot be found out it is taken to:
/// every reader of these files passes over a blank line, and none can part two joined ones.
fn needs_line_end_first(file: &std::fs::File) -> bool {
    use std::os::unix::fs::FileExt;
    let Ok(size) = file.metadata().map(|m| m.len()) else { return true };
    let mut last = [b'\n'];
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

/// A hash (FNV-1a, the same in every build) of a file's size and of `chunk` bytes at its start,
/// its middle and its end: enough to tell files of one size apart without reading gigabytes
/// from a slow drive.
pub fn sample_digest(path: &Path, size: u64, chunk: u64) -> std::io::Result<u64> {
    sample_digest_counting(path, size, chunk, &AtomicU64::new(0))
}

/// [`sample_digest`], adding the bytes read to `got` as they arrive, for whoever is watching
/// whether the drive still answers.
pub fn sample_digest_counting(path: &Path, size: u64, chunk: u64, got: &AtomicU64) -> std::io::Result<u64> {
    let add = |hash: u64, bytes: &[u8]| bytes.iter().fold(hash, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3));
    let mut file = std::fs::File::open(path)?;
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
    const BLOCK: usize = 1 << 20;
    let mut file = std::fs::File::open(path)?;
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
        let words = buffer[..room].chunks_exact(8);
        let end = words.remainder();
        hash = words.fold(hash, |h, w| mix_word(h, u64::from_le_bytes(w.try_into().expect("eight bytes"))));
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
