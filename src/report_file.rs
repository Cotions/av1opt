//! The report files a person names on the command line (`--csv`, `--json`, `--list`).
//!
//! The name is typed by hand, next to the names of videos, so what stands there may be anything.
//! A report only ever replaces an earlier report of its own kind: a mistyped name costs a
//! message, and not the video it happened to name.

use anyhow::{Context, Result, anyhow};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// How much of an existing file is looked at to tell what it is.
const LOOK_AT: u64 = 64 * 1024;

/// What a report file holds.
#[derive(Clone, Copy)]
pub enum Kind {
    /// A table with these columns.
    Csv(&'static [&'static str]),
    Json,
    /// Absolute paths, one per line.
    List,
}

impl Kind {
    fn option(self) -> &'static str {
        match self {
            Kind::Csv(_) => "--csv",
            Kind::Json => "--json",
            Kind::List => "--list",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Csv(_) => "CSV report",
            Kind::Json => "JSON report",
            Kind::List => "list of paths",
        }
    }

    /// Whether a file starting with `start` is an earlier report of this kind. `whole` says
    /// that `start` is all of the file. Of a table only the first columns are compared: later
    /// versions add columns at the end, and their reports are still reports.
    fn fits(self, start: &[u8], whole: bool) -> bool {
        match self {
            Kind::Csv(columns) => {
                let first = columns.iter().take(3).copied().collect::<Vec<_>>().join(",") + ",";
                start.starts_with(first.as_bytes())
            }
            Kind::Json => start.iter().find(|b| !b.is_ascii_whitespace()).is_some_and(|b| matches!(b, b'{' | b'[')),
            Kind::List => {
                // The last line may have been cut where the look ended.
                let end = if whole { start.len() } else { start.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1) };
                end > 0 && std::str::from_utf8(&start[..end]).is_ok_and(|text| text.lines().all(|l| l.starts_with('/')))
            }
        }
    }
}

/// Why a report may not go to `path`, if it may not. `given` are the paths the command was given.
/// A video the command found on its way has a video's name, and the bytes of one if it has been
/// linked under another name, so this can be asked before any of the work is done.
fn problem(path: &Path, kind: Kind, given: &[PathBuf]) -> Option<String> {
    let shown = path.display();
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let folder = crate::files::folder_of(path);
            let there = folder.as_os_str().is_empty() || folder.is_dir();
            return (!there).then(|| format!("the folder of {shown} does not exist"));
        }
        Err(e) => return Some(format!("{shown} cannot be looked at ({e})")),
    };
    if !meta.is_file() {
        return Some(format!("{shown} is there and is not a regular file"));
    }
    let same_file = |p: &PathBuf| std::fs::metadata(p).is_ok_and(|m| m.is_file() && (m.dev(), m.ino()) == (meta.dev(), meta.ino()));
    if crate::scan::is_video(path) || given.iter().any(same_file) {
        return Some(format!("{shown} is a video"));
    }
    if meta.len() == 0 {
        return None;
    }
    let mut start = Vec::new();
    if let Err(e) = std::fs::File::open(path).and_then(|f| f.take(LOOK_AT).read_to_end(&mut start)) {
        return Some(format!("{shown} cannot be read ({e})"));
    }
    (!kind.fits(&start, start.len() as u64 == meta.len())).then(|| format!("{shown} is there and does not look like an earlier {} of av1opt", kind.name()))
}

fn refusal(why: &str, kind: Kind) -> anyhow::Error {
    anyhow!("{}: {why}, so nothing was written; name another file, or move that one away first", kind.option())
}

/// Says now what would stop the reports from being written at the end, before hours of work
/// depend on it. Two reports cannot share a file either.
pub fn check_all(reports: &[(&Path, Kind)], given: &[PathBuf]) -> Result<()> {
    let absolute = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    for (i, &(path, kind)) in reports.iter().enumerate() {
        if let Some(why) = problem(path, kind, given) {
            return Err(refusal(&why, kind));
        }
        if reports[..i].iter().any(|&(earlier, _)| absolute(earlier) == absolute(path)) {
            return Err(refusal(&format!("{} is named for two reports", path.display()), kind));
        }
    }
    Ok(())
}

/// Puts the report at `path`, by way of a file beside it that then takes the name: what was
/// there stays whole until the new report is. Asks again what [`check_all`] asked, since the
/// work in between can take hours.
pub fn write(path: &Path, kind: Kind, given: &[PathBuf], data: &[u8]) -> Result<()> {
    if let Some(why) = problem(path, kind, given) {
        return Err(refusal(&why, kind));
    }
    crate::files::replace_with(path, data).with_context(|| format!("{}: writing {}", kind.option(), path.display()))
}

/// [`write`] for a table that has been written to memory.
pub fn write_csv(table: csv::Writer<Vec<u8>>, path: &Path, kind: Kind, given: &[PathBuf]) -> Result<()> {
    let data = table.into_inner().map_err(|e| anyhow!("{e}"))?;
    write(path, kind, given, &data)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: Kind = Kind::Csv(&["path", "verdict", "size_bytes", "codec"]);

    struct Folder(PathBuf);

    impl Folder {
        fn new(name: &str) -> Folder {
            let dir = std::env::temp_dir().join(format!("av1opt-report-file-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Folder(dir)
        }

        fn file(&self, name: &str, data: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, data).unwrap();
            path
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_video_keeps_its_bytes_whatever_it_holds() {
        let f = Folder::new("video");
        // Even one whose first bytes would pass for a report.
        let video = f.file("a.mkv", b"{ not really json");
        for kind in [TABLE, Kind::Json, Kind::List] {
            let e = write(&video, kind, &[], b"new").unwrap_err().to_string();
            assert!(e.contains("is a video") && e.contains("nothing was written") && e.contains(video.to_str().unwrap()), "{e}");
        }
        assert_eq!(std::fs::read(&video).unwrap(), b"{ not really json");
        // Under a name that is no video's, it is known by being what the command was given.
        let named = f.file("clip.bin", b"[1]");
        let linked = f.0.join("report.json");
        std::fs::hard_link(&named, &linked).unwrap();
        assert!(write(&linked, Kind::Json, std::slice::from_ref(&named), b"new").is_err());
        assert_eq!(std::fs::read(&named).unwrap(), b"[1]");
        assert_eq!(std::fs::read_dir(&f.0).unwrap().count(), 3, "nothing is left beside them");
    }

    #[test]
    fn only_an_earlier_report_of_the_same_kind_is_replaced() {
        let f = Folder::new("kind");
        let csv = "path,verdict,size_bytes,an,older,layout\n/v/a.mkv,x,1\n".as_bytes();
        let list = b"/v/a.mkv\n/v/b c.mkv\n";
        for (kind, earlier, others) in [
            (TABLE, csv, [&b"{}"[..], list, b"notes, of mine\n"]),
            (Kind::Json, b" \n{\"a\":1}", [csv, list, b"notes\n"]),
            (Kind::Json, b"[1]", [csv, list, b"\x00\x01"]),
            (Kind::List, list, [csv, b"{}", b"/v/a.mkv\nnot a path\n"]),
        ] {
            let path = f.file("report", earlier);
            write(&path, kind, &[], b"new").unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), b"new");
            for other in others {
                std::fs::write(&path, other).unwrap();
                let e = write(&path, kind, &[], b"new").unwrap_err().to_string();
                assert!(e.contains("does not look like an earlier") && e.contains("nothing was written"), "{e}");
                assert_eq!(std::fs::read(&path).unwrap(), other);
            }
        }
        // A file made empty beforehand (`mktemp`), and no file at all, are free to take.
        let empty = f.file("empty", b"");
        write(&empty, Kind::List, &[], b"/v\n").unwrap();
        write(&f.0.join("new.csv"), TABLE, &[], b"x").unwrap();
    }

    #[test]
    fn a_long_list_is_judged_by_its_whole_lines() {
        let f = Folder::new("long");
        let line = format!("/videos/{}/é.mkv\n", "x".repeat(100));
        let path = f.file("list.txt", line.repeat(2000).as_bytes());
        assert!(path.metadata().unwrap().len() > LOOK_AT);
        write(&path, Kind::List, &[], b"/v\n").unwrap();
    }

    #[test]
    fn what_is_no_regular_file_is_left_alone() {
        let f = Folder::new("odd");
        let target = f.file("target.csv", b"path,verdict,size_bytes,codec\n");
        let link = f.0.join("link.csv");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        for path in [&link, &f.0, &PathBuf::from("/dev/null")] {
            let e = write(path, TABLE, &[], b"new").unwrap_err().to_string();
            assert!(e.contains("not a regular file"), "{e}");
        }
        assert_eq!(std::fs::read(&target).unwrap(), b"path,verdict,size_bytes,codec\n");
    }

    #[test]
    fn what_would_fail_at_the_end_is_said_at_the_start() {
        let f = Folder::new("start");
        let (csv, json) = (f.0.join("r.csv"), f.0.join("r.json"));
        check_all(&[(&csv, TABLE), (&json, Kind::Json)], &[]).unwrap();
        assert!(!csv.exists() && !json.exists(), "asking writes nothing");
        let e = check_all(&[(&csv, TABLE), (&f.0.join("./r.csv"), Kind::Json)], &[]).unwrap_err().to_string();
        assert!(e.contains("named for two reports"), "{e}");
        let e = check_all(&[(&f.0.join("no/such/r.csv"), TABLE)], &[]).unwrap_err().to_string();
        assert!(e.contains("does not exist"), "{e}");
        check_all(&[(Path::new("in-the-working-folder.csv"), TABLE)], &[]).unwrap();
    }
}
