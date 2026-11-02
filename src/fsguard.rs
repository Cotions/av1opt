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

/// Checks the nearest existing ancestor of `path`, so it works before the folder is created.
pub fn ensure_writable_location(path: &Path, allow_zfs: bool) -> Result<()> {
    let abs = std::path::absolute(path)?;
    let existing = abs.ancestors().find(|p| p.exists()).unwrap_or(Path::new("/"));
    if !allow_zfs && fs_magic(existing) == Some(ZFS_SUPER_MAGIC) {
        bail!("{} is on ZFS; refusing to write there. Pick another output folder, or pass --allow-zfs.", abs.display());
    }
    Ok(())
}
