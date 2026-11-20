//! Refuses to write to ZFS unless explicitly allowed (protects the main library pool).

use anyhow::{Result, bail};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const ZFS_SUPER_MAGIC: i64 = 0x2fc12fc1;

#[cfg(test)]
thread_local! {
    /// Folders this thread takes to be on ZFS, with all below them, for the tests of what is
    /// refused there: no test may write to a pool that is.
    pub(crate) static ZFS_IN_TESTS: std::cell::RefCell<Vec<PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn fs_magic(path: &Path) -> Option<i64> {
    #[cfg(test)]
    if ZFS_IN_TESTS.with_borrow(|z| z.iter().any(|d| path.starts_with(d))) {
        return Some(ZFS_SUPER_MAGIC);
    }
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

/// Where `path` leads as the system takes it, links included, whether or not all of it exists
/// yet. Followed one name at a time, because a link can point at a folder still to be made:
/// what is written through it will land there, not next to the link.
#[cfg(feature = "ui")]
pub fn resolved(path: &Path) -> std::io::Result<PathBuf> {
    /// As many links as the system itself follows before it gives up on a path.
    const MOST_LINKS: u32 = 40;
    let names = |p: &Path| p.components().rev().map(|c| c.as_os_str().to_os_string()).collect::<Vec<_>>();
    let (mut left, mut out, mut links) = (names(&plain(path)?), PathBuf::new(), 0);
    while let Some(name) = left.pop() {
        if name == ".." {
            out.pop();
        } else if name == "/" {
            out = PathBuf::from("/");
        } else if name != "." {
            let next = out.join(&name);
            match std::fs::read_link(&next) {
                Ok(target) if links == MOST_LINKS => return Err(std::io::Error::other(format!("too many links on the way, the last to {}", target.display()))),
                Ok(target) => {
                    links += 1;
                    left.extend(names(&target));
                }
                Err(_) => out = next,
            }
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

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("av1opt-fsguard-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    #[test]
    fn a_plain_path_comes_back_as_it_is() {
        assert_eq!(plain(Path::new("/")).unwrap(), Path::new("/"));
        assert_eq!(plain(Path::new("/no/such/folder")).unwrap(), Path::new("/no/such/folder"));
        assert_eq!(plain(Path::new("//no//such/./folder/")).unwrap(), Path::new("/no/such/folder"));
    }

    #[test]
    fn a_relative_path_counts_from_the_current_folder() {
        let here = std::env::current_dir().unwrap();
        assert_eq!(plain(Path::new(".")).unwrap(), here);
        assert_eq!(plain(Path::new("not/made/yet")).unwrap(), here.join("not/made/yet"));
        assert_eq!(plain(Path::new("not-made/../x")).unwrap(), here.join("x"));
    }

    #[test]
    fn names_that_are_not_text_survive() {
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::OsStr::from_bytes(b"caf\xe9");
        assert_eq!(plain(&Path::new("/no/such").join(name)).unwrap().file_name(), Some(name));
    }

    #[test]
    fn going_up_past_a_link_after_a_folder_to_be_made_leaves_from_where_the_link_leads() {
        let dir = scratch("link");
        std::fs::create_dir_all(dir.join("far/inner")).unwrap();
        std::os::unix::fs::symlink(dir.join("far/inner"), dir.join("link")).unwrap();
        // One up out of `new` is still the link's folder, a second one leaves what it points to.
        assert_eq!(plain(&dir.join("link/new/../x")).unwrap(), dir.join("link/x"));
        assert_eq!(plain(&dir.join("link/new/../../x")).unwrap(), dir.join("far/x"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(feature = "ui")]
    #[test]
    fn a_resolved_path_goes_through_links_and_keeps_what_is_still_to_be_made() {
        let dir = scratch("resolved");
        std::fs::create_dir_all(dir.join("far/inner")).unwrap();
        std::os::unix::fs::symlink(dir.join("far/inner"), dir.join("link")).unwrap();
        assert_eq!(resolved(&dir.join("link")).unwrap(), dir.join("far/inner"));
        assert_eq!(resolved(&dir.join("link/new/deeper")).unwrap(), dir.join("far/inner/new/deeper"));
        assert_eq!(resolved(&dir.join("new/../far")).unwrap(), dir.join("far"));
        assert_eq!(resolved(Path::new("/")).unwrap(), Path::new("/"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(feature = "ui")]
    #[test]
    fn a_link_to_a_folder_still_to_be_made_resolves_to_that_folder() {
        let dir = scratch("dangling");
        std::fs::create_dir_all(dir.join("far")).unwrap();
        std::os::unix::fs::symlink(dir.join("far/new"), dir.join("link")).unwrap();
        let got = resolved(&dir.join("link/x"));
        let want = dir.join("far/new/x");
        // A relative link counts from the folder it is in, and a circle of links leads nowhere.
        std::os::unix::fs::symlink("far/other", dir.join("near")).unwrap();
        let relative = resolved(&dir.join("near/x"));
        std::os::unix::fs::symlink("round", dir.join("round")).unwrap();
        let circle = resolved(&dir.join("round/x"));
        let far = dir.join("far/other/x");
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(got.unwrap(), want);
        assert_eq!(relative.unwrap(), far);
        assert!(circle.unwrap_err().to_string().starts_with("too many links"));
    }

    #[test]
    fn the_filesystem_is_read_from_the_nearest_folder_that_exists() {
        // /proc is on every Linux and never on ZFS.
        assert_eq!(fs_magic(Path::new("/proc")), Some(libc::PROC_SUPER_MAGIC));
        assert_eq!(fs_magic(Path::new("/proc/no-such-entry")), None);
        assert_eq!(fs_magic(Path::new("/pro\0c")), None);
        assert!(!on_zfs(Path::new("/proc/no-such-entry/deeper")));
        assert!(ensure_writable_location(Path::new("/proc/no-such-entry/deeper"), false).is_ok());
        assert!(ensure_writable_location(Path::new("/proc/no-such-entry/deeper"), true).is_ok());
    }

    #[test]
    fn a_path_the_system_could_not_take_is_judged_by_the_folder_above_it() {
        assert!(!on_zfs(Path::new("/proc/a\0b")));
        assert!(ensure_writable_location(Path::new("/proc/a\0b"), false).is_ok());
    }
}
