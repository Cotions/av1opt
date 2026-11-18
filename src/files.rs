//! Writing files where another program, or another user, may be writing too.

use std::io::Write;
use std::path::{Path, PathBuf};

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
/// number, so two programs saving at once do not write into each other's.
pub fn replace_with(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    let tmp: PathBuf = path.with_file_name(name);
    write_new(&tmp, data).and_then(|()| std::fs::rename(&tmp, path)).inspect_err(|_| drop(std::fs::remove_file(&tmp)))
}

/// Adds `line` and a line end to the file at `path` in one write, so that two programs
/// adding at once do not cut into each other's lines. `follow` says whether a link at that
/// name is gone through (a file of the user's own, which they may keep elsewhere) or
/// refused (a file in a folder others can write to).
pub fn append_line(path: &Path, line: &str, follow: bool) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut o = std::fs::OpenOptions::new();
    o.create(true).append(true);
    if !follow {
        o.custom_flags(libc::O_NOFOLLOW);
    }
    o.open(path)?.write_all(format!("{line}\n").as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep mex\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
