//! Refuses to write to ZFS unless explicitly allowed (protects the main library pool).

use anyhow::{Result, bail};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

const ZFS_SUPER_MAGIC: i64 = 0x2fc12fc1;

fn fs_magic(path: &Path) -> Option<i64> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `st` is a properly sized out-parameter.
    (unsafe { libc::statfs(c.as_ptr(), &mut st) } == 0).then_some(st.f_type as i64)
}

/// Whether `path` (or, before it is made, the nearest folder above it that exists) is on ZFS.
pub fn on_zfs(path: &Path) -> bool {
    let Ok(abs) = std::path::absolute(path) else { return false };
    abs.ancestors().find(|p| p.exists()).and_then(fs_magic) == Some(ZFS_SUPER_MAGIC)
}

/// Checks the nearest existing ancestor of `path`, so it works before the folder is created.
pub fn ensure_writable_location(path: &Path, allow_zfs: bool) -> Result<()> {
    let abs = std::path::absolute(path)?;
    let existing = abs.ancestors().find(|p| p.exists()).unwrap_or(Path::new("/"));
    // `/home/me/new/../../../pool/x` with no `new` yet would be judged by `/home/me`, and
    // land on the pool once `new` is made. What exists is resolved by the system itself.
    if existing != abs && abs.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        bail!("{} does not exist yet and has `..` in it: name the folder by its plain path.", abs.display());
    }
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
        // What exists is judged where it really is, `..` or not; what does not, by its
        // nearest folder that does.
        assert!(ensure_writable_location(&dir.join("there/../there"), false).is_ok());
        assert!(ensure_writable_location(&dir.join("new/deeper"), false).is_ok());
        // Through a folder that is not there yet, `..` leads anywhere once it is made.
        let e = ensure_writable_location(&dir.join("new/../../elsewhere"), false).unwrap_err().to_string();
        assert!(e.contains("has `..` in it"), "{e}");
        assert!(ensure_writable_location(&dir.join("new/../../elsewhere"), true).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
