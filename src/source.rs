//! Whether a video is still the file an output was made from.
//!
//! The time a file was last changed says little alone. A file only touched, or a library copied
//! to another drive without its dates, has a new time and the same contents; a download written
//! into a file allocated ahead has the same size and other contents. So the report keeps a
//! fingerprint of the file as well, and the rule is the same everywhere: another size is another
//! file; the same size and the same time is the same file; the same size at another time is
//! settled by the fingerprint.

use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Times this close count as the same: some filesystems keep them in steps of two seconds, and
/// a copy can round them.
const SAME_TIME_WITHIN: u64 = 2;

/// How much is read at the start, the middle and the end of a file.
const FINGERPRINT_SAMPLE: u64 = 1 << 20;

/// What was known of a source when an encode of it began, as a report keeps it. Reports of
/// earlier versions lack the later fields.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Recorded {
    pub size: Option<u64>,
    /// When it was last changed, in seconds since 1970.
    pub modified: Option<u64>,
    pub fingerprint: Option<String>,
}

/// The time a file was last changed, in the whole seconds a report keeps.
pub fn modified_seconds(meta: &std::fs::Metadata) -> Option<u64> {
    Some(meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs())
}

fn fingerprint(path: &Path, size: u64) -> std::io::Result<String> {
    Ok(format!("{:016x}", crate::files::sample_digest(path, size, FINGERPRINT_SAMPLE)?))
}

/// The fingerprint of the file as it is now. Remembered for as long as the file keeps its size
/// and time, because the History page checks every report each time it is opened, and a library
/// that lost its dates would otherwise be read again from the drive for each of them.
fn current_fingerprint(path: &Path, meta: &std::fs::Metadata) -> std::io::Result<String> {
    type Known = HashMap<(PathBuf, u64, SystemTime), String>;
    static KNOWN: Mutex<Option<Known>> = Mutex::new(None);
    let Ok(time) = meta.modified() else { return fingerprint(path, meta.len()) };
    let key = (path.to_path_buf(), meta.len(), time);
    if let Some(known) = KNOWN.lock().unwrap().as_ref().and_then(|k| k.get(&key)) {
        return Ok(known.clone());
    }
    let print = fingerprint(path, meta.len())?;
    let mut known = KNOWN.lock().unwrap();
    let known = known.get_or_insert_default();
    if known.len() >= 100_000 {
        known.clear();
    }
    known.insert(key, print.clone());
    Ok(print)
}

impl Recorded {
    /// The file at `path` as it stands, and its size. A fingerprint that cannot be read is left
    /// out: the file may well be readable for what comes next, and without one a later change
    /// of its time counts as a change of the file.
    pub fn of(path: &Path) -> std::io::Result<(u64, Recorded)> {
        let meta = std::fs::metadata(path)?;
        let size = meta.len();
        Ok((size, Recorded { size: Some(size), modified: modified_seconds(&meta), fingerprint: fingerprint(path, size).ok() }))
    }

    pub fn in_report(report: &Value) -> Recorded {
        Recorded {
            size: report["source_bytes"].as_u64(),
            modified: report["source_mtime"].as_u64(),
            fingerprint: report["source_fingerprint"].as_str().map(String::from),
        }
    }
}

/// What became of a source since it was recorded.
#[derive(Debug)]
pub enum Verdict {
    Same,
    OtherSize { then: u64, now: u64 },
    /// The same size at another time, and the fingerprint differs.
    Changed,
    /// The same size at another time, and nothing recorded to tell whether the contents changed.
    CannotTell,
    /// The file could not be looked at, which says nothing about it.
    Unreadable(std::io::Error),
}

/// Compares the file at `path` with what was recorded of it.
pub fn compare(then: &Recorded, path: &Path) -> Verdict {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) => return Verdict::Unreadable(e),
    };
    if let Some(size) = then.size.filter(|size| *size != meta.len()) {
        return Verdict::OtherSize { then: size, now: meta.len() };
    }
    let same_time = matches!((then.modified, modified_seconds(&meta)), (Some(a), Some(b)) if a.abs_diff(b) <= SAME_TIME_WITHIN);
    match &then.fingerprint {
        _ if same_time => Verdict::Same,
        Some(print) => match current_fingerprint(path, &meta) {
            Ok(now) if now == *print => Verdict::Same,
            Ok(_) => Verdict::Changed,
            Err(e) => Verdict::Unreadable(e),
        },
        None if then.modified.is_some() => Verdict::CannotTell,
        // A report of before times were kept: its size is all there is to go by.
        None => Verdict::Same,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom, Write};
    use std::time::Duration;

    struct Sample(PathBuf);

    impl Sample {
        fn new(name: &str, size: usize) -> Sample {
            let dir = std::env::temp_dir().join(format!("av1opt-source-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("a.mkv");
            std::fs::write(&path, vec![7u8; size]).unwrap();
            let sample = Sample(path);
            sample.set_time(1_700_000_000);
            sample
        }

        fn set_time(&self, seconds: u64) {
            let time = UNIX_EPOCH + Duration::from_secs(seconds);
            std::fs::File::options().write(true).open(&self.0).unwrap().set_modified(time).unwrap();
        }

        fn write_at(&self, at: u64, bytes: &[u8]) {
            let mut f = std::fs::File::options().write(true).open(&self.0).unwrap();
            f.seek(SeekFrom::Start(at)).unwrap();
            f.write_all(bytes).unwrap();
        }

        fn recorded(&self) -> Recorded {
            Recorded::of(&self.0).unwrap().1
        }
    }

    impl Drop for Sample {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.0.parent().unwrap());
        }
    }

    #[test]
    fn a_new_time_with_the_same_contents_is_the_same_file() {
        let f = Sample::new("touched", 5000);
        let then = f.recorded();
        assert_eq!(then.modified, Some(1_700_000_000));
        assert!(matches!(compare(&then, &f.0), Verdict::Same));
        f.set_time(1_700_000_900);
        assert!(matches!(compare(&then, &f.0), Verdict::Same));
    }

    #[test]
    fn new_contents_at_the_same_size_are_another_file_wherever_they_are_sampled() {
        const MEGABYTE: u64 = 1 << 20;
        let size = 8 * MEGABYTE;
        for at in [0, size / 2, size - 1] {
            let f = Sample::new(&format!("written-{at}"), size as usize);
            let then = f.recorded();
            f.write_at(at, b"x");
            f.set_time(1_700_000_900);
            assert!(matches!(compare(&then, &f.0), Verdict::Changed), "at {at}");
        }
    }

    #[test]
    fn the_same_time_settles_it_without_reading_and_another_size_at_any_time_does_too() {
        let f = Sample::new("time", 5000);
        let then = f.recorded();
        // Within the two seconds, whatever the bytes: nothing is read.
        f.write_at(0, b"x");
        f.set_time(1_700_000_002);
        assert!(matches!(compare(&then, &f.0), Verdict::Same));
        f.set_time(1_699_999_998);
        assert!(matches!(compare(&then, &f.0), Verdict::Same));
        f.set_time(1_700_000_003);
        assert!(matches!(compare(&then, &f.0), Verdict::Changed));
        std::fs::File::options().append(true).open(&f.0).unwrap().write_all(b"more").unwrap();
        f.set_time(1_700_000_000);
        assert!(matches!(compare(&then, &f.0), Verdict::OtherSize { then: 5000, now: 5004 }));
    }

    #[test]
    fn a_report_is_judged_by_what_it_holds() {
        let f = Sample::new("report", 5000);
        let print = f.recorded().fingerprint.unwrap();
        let read = |r: Value| compare(&Recorded::in_report(&r), &f.0);
        f.set_time(1_700_000_900);
        // Time and fingerprint, time alone, and neither.
        assert!(matches!(read(serde_json::json!({ "source_bytes": 5000, "source_mtime": 1_700_000_000u64, "source_fingerprint": print })), Verdict::Same));
        assert!(matches!(read(serde_json::json!({ "source_bytes": 5000, "source_mtime": 1_700_000_000u64 })), Verdict::CannotTell));
        assert!(matches!(read(serde_json::json!({ "source_bytes": 5000, "source_mtime": 1_700_000_900u64 })), Verdict::Same));
        assert!(matches!(read(serde_json::json!({ "source_bytes": 5000 })), Verdict::Same));
        assert!(matches!(read(serde_json::json!({ "source_bytes": 4000 })), Verdict::OtherSize { .. }));
        // A file whose time could not be kept is settled by its fingerprint.
        assert!(matches!(read(serde_json::json!({ "source_bytes": 5000, "source_fingerprint": print })), Verdict::Same));
        assert!(matches!(read(serde_json::json!({ "source_bytes": 5000, "source_fingerprint": "0" })), Verdict::Changed));
    }

    #[test]
    fn a_file_that_cannot_be_looked_at_is_not_called_changed() {
        let f = Sample::new("gone", 10);
        let then = f.recorded();
        std::fs::remove_file(&f.0).unwrap();
        assert!(matches!(compare(&then, &f.0), Verdict::Unreadable(_)));
    }

    #[test]
    fn the_fingerprint_stays_what_earlier_versions_wrote() {
        let f = Sample::new("stable", 5000);
        // Written out: reports keep this value, so another hash would call every file changed.
        assert_eq!(f.recorded().fingerprint.as_deref(), Some("d628ee27255a67f4"));
    }
}
