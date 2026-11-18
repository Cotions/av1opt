//! Refuses to write to ZFS unless explicitly allowed (protects the main library pool).

use anyhow::{Result, bail};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const ZFS_SUPER_MAGIC: i64 = 0x2fc12fc1;

fn fs_magic(path: &Path) -> Option<i64> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `st` is a properly sized out-parameter.
    (unsafe { libc::statfs(c.as_ptr(), &mut st) } == 0).then_some(st.f_type as i64)
}

/// `path` as an absolute one without `.` and `..`, each `..` taken the way the system will
/// take it once every folder on the way is made: out of a folder that exists, to its real
/// parent (which for a link is not the folder the link is in); out of one still to be made,
/// to the folder it will be made in.
///
/// Without this, `/home/me/new/../../../pool/x` with no `new` yet would be judged by
/// `/home/me`, its nearest ancestor that exists, and land on the pool once `new` is made.
fn plain(path: &Path) -> std::io::Result<PathBuf> {
    let mut out = PathBuf::new();
    for c in std::path::absolute(path)?.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if let Ok(real) = std::fs::canonicalize(&out) {
                    out = real;
                }
                out.pop();
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

/// Whether `path` (or, before it is made, the nearest folder above it that exists) is on ZFS.
pub fn on_zfs(path: &Path) -> bool {
    let Ok(abs) = plain(path) else { return false };
    abs.ancestors().find(|p| p.exists()).and_then(fs_magic) == Some(ZFS_SUPER_MAGIC)
}

/// Checks the nearest existing ancestor of `path`, so it works before the folder is created.
pub fn ensure_writable_location(path: &Path, allow_zfs: bool) -> Result<()> {
    let abs = plain(path)?;
    let existing = abs.ancestors().find(|p| p.exists()).unwrap_or(Path::new("/"));
    if !allow_zfs && fs_magic(existing) == Some(ZFS_SUPER_MAGIC) {
        bail!("{} is on ZFS; refusing to write there. Pick another output folder, or pass --allow-zfs.", abs.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_to_be_made_is_not_judged_through_dot_dot() {
        let dir = std::env::temp_dir().join(format!("av1opt-fsguard-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("there")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        assert!(ensure_writable_location(&dir.join("there/../there"), false).is_ok());
        assert!(ensure_writable_location(&dir.join("new/deeper"), false).is_ok());
        assert_eq!(plain(&dir.join("there/./../there/x")).unwrap(), dir.join("there/x"));
        // Through a folder that is not there yet, `..` leads to where that folder will be:
        // the path is judged by where it ends, not by the last folder that happens to exist.
        assert_eq!(plain(&dir.join("new/../../elsewhere/x")).unwrap(), dir.parent().unwrap().join("elsewhere/x"));
        assert_eq!(plain(&dir.join("new/more/../../x")).unwrap(), dir.join("x"));
        // Out of a link, `..` is the parent of what the link points to.
        std::fs::create_dir_all(dir.join("far/inner")).unwrap();
        std::os::unix::fs::symlink(dir.join("far/inner"), dir.join("there/link")).unwrap();
        assert_eq!(plain(&dir.join("there/link/../x")).unwrap(), dir.join("far/x"));
        assert_eq!(plain(Path::new("/../..")).unwrap(), Path::new("/"));
        // A relative folder next to the current one is no reason to refuse.
        assert!(ensure_writable_location(Path::new("../av1opt-not-made-yet"), true).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
