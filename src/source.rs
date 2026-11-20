//! Whether a video is still the file an output was made from.
//!
//! The time a file was last changed says little alone. A file only touched, or a library copied
//! to another drive without its dates, has a new time and the same contents; a download written
//! into a file allocated ahead, or out of order, has the same size and other contents, and those
//! can lie anywhere in it. So a report keeps a hash of the whole file, read while the encode
//! works, and the rule is the same everywhere: another size is another file; the same size and
//! the same time is the same file, and nothing is read; the same size at another time is settled
//! by the hash. A report without that hash cannot settle it: the samples earlier reports hold
//! can only show that a file did change.

use serde_json::Value;
use std::collections::HashMap;
use std::io::{Error, ErrorKind};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Times this close count as the same: some filesystems keep them in steps of two seconds, and
/// a copy can round them.
const SAME_TIME_WITHIN: u64 = 2;

/// How much is read at the start, the middle and the end of a file.
const FINGERPRINT_SAMPLE: u64 = 1 << 20;

/// A read that gets nothing for this long is given up on: the drive has stalled.
const READ_STALL: Duration = Duration::from_secs(120);

#[cfg(test)]
thread_local! {
    /// [`READ_STALL`] for what this thread waits for, in the tests of a read that stalls.
    static STALL_IN_TESTS: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
}

fn read_stall() -> Duration {
    #[cfg(test)]
    if let Some(limit) = STALL_IN_TESTS.get() {
        return limit;
    }
    READ_STALL
}

/// What was known of a source when an encode of it began, as a report keeps it. Reports of
/// earlier versions lack the later fields.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Recorded {
    pub size: Option<u64>,
    /// When it was last changed, in seconds since 1970.
    pub modified: Option<u64>,
    /// That time as the file system gives it. Only of a file looked at now: reports hold seconds.
    pub modified_exactly: Option<SystemTime>,
    pub fingerprint: Option<String>,
    /// The hash of the whole file.
    pub whole: Option<String>,
    /// The encode tried to read the fingerprint and could not, as opposed to a report of a
    /// version that did not keep one.
    pub fingerprint_failed: bool,
}

/// Which file this is and how it stands: its place on the drive, its size, the exact time it was
/// last written, and the time anything about it last changed, which nobody can set back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Stamp {
    device: u64,
    inode: u64,
    size: u64,
    written: (i64, i64),
    changed: (i64, i64),
}

impl Stamp {
    pub fn of(meta: &std::fs::Metadata) -> Stamp {
        use std::os::unix::fs::MetadataExt;
        Stamp { device: meta.dev(), inode: meta.ino(), size: meta.len(), written: (meta.mtime(), meta.mtime_nsec()), changed: (meta.ctime(), meta.ctime_nsec()) }
    }

    /// Whether `meta` is this very file, wherever it is now.
    pub fn same_file(&self, meta: &std::fs::Metadata) -> bool {
        let now = Stamp::of(meta);
        meta.is_file() && (now.device, now.inode) == (self.device, self.inode)
    }

    /// Whether `meta` is this file as it was, now that it has been moved, which itself counts as
    /// a change to it: so only its size and the time it was written can tell.
    pub fn same_after_move(&self, meta: &std::fs::Metadata) -> bool {
        let now = Stamp::of(meta);
        self.same_file(meta) && (now.size, now.written) == (self.size, self.written)
    }
}

/// What whole files were found to hold during one act, like one `replace --apply`, so that none
/// is read twice for it. An answer counts only for a file that is exactly as it was when its read
/// began, and nothing is kept after the act.
#[derive(Default)]
pub struct WholeReads {
    found: Mutex<HashMap<Stamp, String>>,
    reads: AtomicUsize,
}

impl WholeReads {
    /// The hash of all of the plain file at `path`, read through one open file unless the file is
    /// still as it was when that was last done. A link is not followed.
    fn hash_of(&self, path: &Path, date_is_the_same: bool) -> std::io::Result<String> {
        let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
        let meta = file.metadata()?;
        must_be_regular(&meta)?;
        let stamp = Stamp::of(&meta);
        if let Some(found) = self.found.lock().unwrap().get(&stamp) {
            return Ok(found.clone());
        }
        if date_is_the_same {
            eprintln!("reading all of {} to make sure it is still the file that was encoded before it goes to the trash (on a large file this can take minutes)", path.display());
        } else {
            eprintln!("reading all of {} to see whether it is still the file that was encoded: its date changed (on a large file this can take minutes)", path.display());
        }
        self.reads.fetch_add(1, Ordering::Relaxed);
        let reader = file.try_clone()?;
        let size = meta.len();
        let hash = read_watched(move |got| Ok(format!("{:016x}", crate::files::whole_digest_of(reader, size, got, &AtomicBool::new(false))?)))?;
        // A write during the read may have landed behind it: such a hash is of no file.
        if file.metadata().is_ok_and(|now| Stamp::of(&now) == stamp) {
            self.found.lock().unwrap().insert(stamp, hash.clone());
            remember(&WHOLE_HASHES, path, &meta, &hash);
        }
        Ok(hash)
    }

    #[cfg(test)]
    pub fn reads_made(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }
}

/// The time a file was last changed, in the whole seconds a report keeps.
pub fn modified_seconds(meta: &std::fs::Metadata) -> Option<u64> {
    Some(meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs())
}

/// Only a regular file can be read to its end and read again the same: a named pipe would hold
/// whoever opens it for as long as nobody writes to it.
fn must_be_regular(meta: &std::fs::Metadata) -> std::io::Result<()> {
    if meta.is_file() { Ok(()) } else { Err(Error::new(ErrorKind::InvalidInput, "it is not a regular file")) }
}

/// Waits for what a reading thread sends for as long as `got` keeps growing. `give_up` is asked
/// now and then, for someone who no longer wants the answer.
fn wait_for_read<T>(answer: &Receiver<std::io::Result<T>>, got: &AtomicU64, give_up: &dyn Fn() -> bool) -> std::io::Result<T> {
    let limit = read_stall();
    let (mut seen, mut since) = (got.load(Ordering::Relaxed), Instant::now());
    loop {
        match answer.recv_timeout(limit.min(Duration::from_secs(1))) {
            Ok(result) => return result,
            Err(RecvTimeoutError::Disconnected) => return Err(Error::other("the read ended without an answer")),
            Err(RecvTimeoutError::Timeout) => {}
        }
        if give_up() {
            return Err(Error::new(ErrorKind::Interrupted, "no longer needed"));
        }
        let now = got.load(Ordering::Relaxed);
        if now != seen {
            (seen, since) = (now, Instant::now());
        } else if since.elapsed() >= limit {
            return Err(Error::new(ErrorKind::TimedOut, format!("nothing could be read from it for {} seconds; its drive may have stalled", limit.as_secs())));
        }
    }
}

/// Runs `read` on a thread of its own, so that a drive that stops answering costs this file and
/// not whoever asked. The thread of such a read stays behind until the drive answers; it holds
/// nothing but the file.
pub fn read_watched<T: Send + 'static>(read: impl FnOnce(&AtomicU64) -> std::io::Result<T> + Send + 'static) -> std::io::Result<T> {
    let (tell, answer) = channel();
    let got = Arc::new(AtomicU64::new(0));
    let counted = got.clone();
    std::thread::spawn(move || drop(tell.send(read(&counted))));
    wait_for_read(&answer, &got, &|| false)
}

fn fingerprint(path: &Path, size: u64) -> std::io::Result<String> {
    let path = path.to_path_buf();
    read_watched(move |got| Ok(format!("{:016x}", crate::files::sample_digest_counting(&path, size, FINGERPRINT_SAMPLE, got)?)))
}

fn whole_hash(path: &Path, size: u64) -> std::io::Result<String> {
    whole_hash_stopped_by(path, size, Arc::new(AtomicBool::new(false)))
}

/// Sets `stop` when it returns: if the read was given up on, it may wake with the drive, and
/// nobody wants what it would go on to read.
fn whole_hash_stopped_by(path: &Path, size: u64, stop: Arc<AtomicBool>) -> std::io::Result<String> {
    let (path, stopped) = (path.to_path_buf(), stop.clone());
    let answer = read_watched(move |got| Ok(format!("{:016x}", crate::files::whole_digest(&path, size, got, &stopped)?)));
    stop.store(true, Ordering::Relaxed);
    answer
}

/// The fingerprint of a file already open, read through it: of that file, whatever its name leads
/// to by now.
pub fn fingerprint_of_open(file: &std::fs::File) -> std::io::Result<String> {
    let (file, size) = (file.try_clone()?, file.metadata()?.len());
    read_watched(move |got| Ok(format!("{:016x}", crate::files::sample_digest_of(file, size, FINGERPRINT_SAMPLE, got)?)))
}

/// The hash of all of a file already open, read through it from its start.
pub fn whole_hash_of_open(file: &std::fs::File) -> std::io::Result<String> {
    use std::io::Seek;
    let (mut file, size) = (file.try_clone()?, file.metadata()?.len());
    file.seek(std::io::SeekFrom::Start(0))?;
    read_watched(move |got| Ok(format!("{:016x}", crate::files::whole_digest_of(file, size, got, &AtomicBool::new(false))?)))
}

/// Whether two times, in the seconds a report keeps, count as the same.
pub fn same_time(a: u64, b: u64) -> bool {
    a.abs_diff(b) <= SAME_TIME_WITHIN
}

type Known = HashMap<(PathBuf, u64, SystemTime), String>;

/// What was last found in files, by path, size and time, for the History page: it checks every
/// report each time it is opened, and a library that lost its dates would otherwise be read again
/// from the drive for each of them. Size and time say little, since a file can be rewritten and
/// its time set back. So only [`compare_for_list`] takes anything from here: what acts on the
/// answer, like `replace` before an original goes to the trash, reads the file.
static FINGERPRINTS: Mutex<Option<Known>> = Mutex::new(None);
static WHOLE_HASHES: Mutex<Option<Known>> = Mutex::new(None);

fn memory_key(path: &Path, meta: &std::fs::Metadata) -> Option<(PathBuf, u64, SystemTime)> {
    Some((path.to_path_buf(), meta.len(), meta.modified().ok()?))
}

fn recall(memory: &Mutex<Option<Known>>, path: &Path, meta: &std::fs::Metadata) -> Option<String> {
    memory.lock().unwrap().as_ref()?.get(&memory_key(path, meta)?).cloned()
}

fn remember(memory: &Mutex<Option<Known>>, path: &Path, meta: &std::fs::Metadata, found: &str) {
    let Some(key) = memory_key(path, meta) else { return };
    let mut memory = memory.lock().unwrap();
    let known = memory.get_or_insert_default();
    if known.len() >= 100_000 {
        known.clear();
    }
    known.insert(key, found.to_string());
}

/// The fingerprint of the file as it is now, read from it.
fn fresh_fingerprint(path: &Path, meta: &std::fs::Metadata) -> std::io::Result<String> {
    let print = fingerprint(path, meta.len())?;
    remember(&FINGERPRINTS, path, meta, &print);
    Ok(print)
}

/// The hash of the whole file as it is now, read from it. This can take minutes, so it is said.
fn fresh_whole_hash(path: &Path, meta: &std::fs::Metadata) -> std::io::Result<String> {
    eprintln!("reading all of {} to see whether it is still the file that was encoded: its date changed (on a large file this can take minutes)", path.display());
    let hash = whole_hash(path, meta.len())?;
    remember(&WHOLE_HASHES, path, meta, &hash);
    Ok(hash)
}

impl Recorded {
    /// The file at `path` as it stands, and its size. A fingerprint that cannot be read is left
    /// out: the file may well be readable for what comes next. Anything but a regular file is
    /// refused here, before it is opened.
    pub fn of(path: &Path) -> std::io::Result<(u64, Recorded)> {
        let meta = std::fs::metadata(path)?;
        must_be_regular(&meta)?;
        let size = meta.len();
        let fingerprint = fingerprint(path, size).ok();
        let fingerprint_failed = fingerprint.is_none();
        Ok((size, Recorded { size: Some(size), modified: modified_seconds(&meta), modified_exactly: meta.modified().ok(), fingerprint, fingerprint_failed, whole: None }))
    }

    pub fn in_report(report: &Value) -> Recorded {
        Recorded {
            size: report["source_bytes"].as_u64(),
            modified: report["source_mtime"].as_u64(),
            modified_exactly: None,
            fingerprint: report["source_fingerprint"].as_str().map(String::from),
            whole: report["source_hash"].as_str().map(String::from),
            // Written as null by the versions that keep one; not there at all before them.
            fingerprint_failed: report.get("source_fingerprint").is_some_and(Value::is_null),
        }
    }

    /// Why this record cannot settle whether a file at another time is the same one, for the
    /// message that says so.
    pub fn why_it_cannot_tell(&self) -> &'static str {
        match (&self.fingerprint, self.fingerprint_failed) {
            (Some(_), _) => "its report holds only samples of the file, which still match, and those do not show whether the rest of it changed",
            (None, true) => "the file could not be read when its encode began, so its report holds only its date",
            (None, false) => "its report, from an earlier version, holds only its date",
        }
    }
}

/// What became of a source since it was recorded.
#[derive(Debug)]
pub enum Verdict {
    Same,
    OtherSize { then: u64, now: u64 },
    /// The same size at another time, and its contents differ.
    Changed,
    /// The same size at another time, and nothing recorded to tell whether the contents changed.
    /// [`Recorded::why_it_cannot_tell`] says what is missing.
    CannotTell,
    /// The file could not be looked at, which says nothing about it.
    Unreadable(std::io::Error),
}

/// Compares the file at `path` with what was recorded of it. Whatever it needs is read from the
/// file now, which can mean all of it; nothing remembered counts.
pub fn compare(then: &Recorded, path: &Path) -> Verdict {
    judge(then, path, Look::Now)
}

/// [`compare`] for a list of many files, which may go by what earlier looks found. Where only
/// reading a whole file could tell, and no earlier look did, the file counts as the same for now.
/// So the answer is for showing: whatever acts on it has to ask [`compare`].
pub fn compare_for_list(then: &Recorded, path: &Path) -> Verdict {
    judge(then, path, Look::ForList)
}

/// [`compare`] on the way to acting, whose whole reads are kept in `reads` for the act itself.
pub fn compare_remembering(then: &Recorded, path: &Path, reads: &WholeReads) -> Verdict {
    judge(then, path, Look::Remembering(reads))
}

/// [`compare`] for what is about to act on the answer. A date can be set back, so where a hash was
/// recorded the file is read whatever its date says (once per act: see [`WholeReads`]). Comes
/// with how the file stood when the look began, for the caller to compare with just before it
/// acts; None if the file changed during the look, or could not be looked at.
pub fn compare_to_act(then: &Recorded, path: &Path, reads: &WholeReads) -> (Verdict, Option<Stamp>) {
    let stamp = || std::fs::symlink_metadata(path).ok().map(|m| Stamp::of(&m));
    let before = stamp();
    let verdict = judge(then, path, Look::ToAct(reads));
    (verdict, before.filter(|_| stamp() == before))
}

/// How far a look at a file goes.
#[derive(Clone, Copy)]
enum Look<'a> {
    Now,
    ForList,
    Remembering(&'a WholeReads),
    ToAct(&'a WholeReads),
}

fn judge(then: &Recorded, path: &Path, look: Look) -> Verdict {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) => return Verdict::Unreadable(e),
    };
    if let Err(e) = must_be_regular(&meta) {
        return Verdict::Unreadable(e);
    }
    if let Some(size) = then.size.filter(|size| *size != meta.len()) {
        return Verdict::OtherSize { then: size, now: meta.len() };
    }
    let date_is_the_same = matches!((then.modified, modified_seconds(&meta)), (Some(a), Some(b)) if a.abs_diff(b) <= SAME_TIME_WITHIN);
    if date_is_the_same && !(matches!(look, Look::ToAct(_)) && then.whole.is_some()) {
        return Verdict::Same;
    }
    let is = |same: bool, otherwise: Verdict| if same { Verdict::Same } else { otherwise };
    if let Some(hash) = &then.whole {
        let now = match look {
            Look::ForList => return recall(&WHOLE_HASHES, path, &meta).map_or(Verdict::Same, |now| is(now == *hash, Verdict::Changed)),
            Look::Now => fresh_whole_hash(path, &meta),
            Look::Remembering(reads) | Look::ToAct(reads) => reads.hash_of(path, date_is_the_same),
        };
        return match now {
            Ok(now) => is(now == *hash, Verdict::Changed),
            Err(e) => Verdict::Unreadable(e),
        };
    }
    let remembered = if matches!(look, Look::ForList) { recall(&FINGERPRINTS, path, &meta) } else { None };
    match &then.fingerprint {
        // Samples that differ show a change. Samples that match show nothing: what was written
        // may lie between them.
        Some(print) => match remembered.map_or_else(|| fresh_fingerprint(path, &meta), Ok) {
            Ok(now) if now != *print => Verdict::Changed,
            Ok(_) => Verdict::CannotTell,
            Err(e) => Verdict::Unreadable(e),
        },
        None if then.modified.is_some() => Verdict::CannotTell,
        // A report of before times were kept: its size is all there is to go by.
        None => Verdict::Same,
    }
}

/// Whether the file's samples are those recorded. None when that cannot be known: nothing was
/// recorded, or the file cannot be read now.
pub fn samples_match(then: &Recorded, path: &Path, meta: &std::fs::Metadata) -> Option<bool> {
    let print = then.fingerprint.as_ref()?;
    must_be_regular(meta).ok()?;
    fingerprint(path, meta.len()).ok().map(|now| now == *print)
}

/// The hash of a whole source, being read on a thread of its own while the encode works. Dropped,
/// it stops reading.
pub struct WholeHash {
    answer: Receiver<std::io::Result<String>>,
    size: u64,
    got: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
}

impl WholeHash {
    pub fn start(path: &Path, size: u64) -> WholeHash {
        let (tell, answer) = channel();
        let (got, stop) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicBool::new(false)));
        let (path, counted, stopped) = (path.to_path_buf(), got.clone(), stop.clone());
        std::thread::spawn(move || {
            let hash = crate::files::whole_digest(&path, size, &counted, &stopped).map(|h| format!("{h:016x}"));
            let _ = tell.send(hash);
        });
        WholeHash { answer, size, got, stop }
    }

    /// Whether the file has been read to its end, so that [`WholeHash::finish`] will not wait.
    pub fn all_read(&self) -> bool {
        self.got.load(Ordering::Relaxed) >= self.size
    }

    /// Waits for the hash for as long as the read gets anywhere and `give_up` says no.
    pub fn finish(self, give_up: &dyn Fn() -> bool) -> std::io::Result<String> {
        wait_for_read(&self.answer, &self.got, give_up)
    }
}

impl Drop for WholeHash {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom, Write};
    use std::time::Duration;

    #[test]
    fn a_whole_hash_given_up_on_stops_reading() {
        // A file with nothing in it takes no room, and reading it takes far longer than this test.
        let dir = std::env::temp_dir().join(format!("av1opt-source-dropped-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.mkv");
        let size = 1u64 << 38;
        std::fs::File::create(&path).unwrap().set_len(size).unwrap();
        let hash = WholeHash::start(&path, size);
        let got = hash.got.clone();
        let started = std::time::Instant::now();
        while got.load(Ordering::Relaxed) == 0 {
            assert!(started.elapsed() < Duration::from_secs(60), "the read did not begin");
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(hash);
        // What is under way ends; after that the count stands still.
        let settled = || {
            let before = got.load(Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(200));
            before == got.load(Ordering::Relaxed)
        };
        let waited = std::time::Instant::now();
        while !settled() {
            assert!(waited.elapsed() < Duration::from_secs(10), "it goes on reading: {} bytes", got.load(Ordering::Relaxed));
        }
        assert!(got.load(Ordering::Relaxed) < size);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_read_behind_a_whole_hash_is_told_to_stop_when_the_hash_is_over() {
        let sample = Sample::new("hash-stop", 10);
        let stop = Arc::new(AtomicBool::new(false));
        assert!(whole_hash_stopped_by(&sample.0, 10, stop.clone()).is_ok());
        assert!(stop.load(Ordering::Relaxed));
    }

    struct Sample(PathBuf);

    impl Sample {
        fn new(name: &str, size: usize) -> Sample {
            Sample::holding(name, vec![7u8; size])
        }

        fn holding(name: &str, bytes: Vec<u8>) -> Sample {
            let dir = std::env::temp_dir().join(format!("av1opt-source-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("a.mkv");
            std::fs::write(&path, bytes).unwrap();
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

        /// What a report of an encode of the file as it is now holds.
        fn encoded(&self) -> Recorded {
            let whole = WholeHash::start(&self.0, std::fs::metadata(&self.0).unwrap().len()).finish(&|| false).unwrap();
            Recorded { whole: Some(whole), ..self.recorded() }
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
        let then = f.encoded();
        assert_eq!(then.modified, Some(1_700_000_000));
        assert!(matches!(compare(&then, &f.0), Verdict::Same));
        f.set_time(1_700_000_900);
        assert!(matches!(compare(&then, &f.0), Verdict::Same));
    }

    #[test]
    fn new_contents_at_the_same_size_are_another_file_wherever_they_are() {
        const MEGABYTE: u64 = 1 << 20;
        let size = 8 * MEGABYTE;
        // In the three samples, and between them, where only the whole file shows it.
        for at in [0, MEGABYTE * 2, size / 2, MEGABYTE * 6, size - 1] {
            let f = Sample::new(&format!("written-{at}"), size as usize);
            let then = f.encoded();
            f.write_at(at, b"x");
            f.set_time(1_700_000_900);
            assert!(matches!(compare(&then, &f.0), Verdict::Changed), "at {at}");
        }
    }

    #[test]
    fn a_hole_filled_between_the_samples_is_seen_only_by_a_report_with_the_whole_hash() {
        const MEGABYTE: u64 = 1 << 20;
        let f = Sample::new("hole", 8 * MEGABYTE as usize);
        let with_hash = f.encoded();
        let samples_only = Recorded { whole: None, ..with_hash.clone() };
        f.write_at(MEGABYTE * 2, b"filled in");
        f.set_time(1_700_000_900);
        assert!(matches!(compare(&with_hash, &f.0), Verdict::Changed));
        // The samples are as they were: that proves nothing, so nothing is concluded from it.
        assert_eq!(samples_only.fingerprint, f.recorded().fingerprint);
        assert!(matches!(compare(&samples_only, &f.0), Verdict::CannotTell));
        assert!(samples_only.why_it_cannot_tell().contains("only samples"));
    }

    #[test]
    fn the_same_time_settles_it_without_reading_and_another_size_at_any_time_does_too() {
        let f = Sample::new("time", 5000);
        let then = f.encoded();
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
        let then = f.encoded();
        let (print, hash) = (then.fingerprint.unwrap(), then.whole.unwrap());
        let read = |r: Value| compare(&Recorded::in_report(&r), &f.0);
        f.set_time(1_700_000_900);
        let time = 1_700_000_000u64;
        // The hash of the whole file decides.
        assert!(matches!(read(serde_json::json!({ "source_bytes": 5000, "source_mtime": time, "source_fingerprint": print, "source_hash": hash })), Verdict::Same));
        assert!(matches!(read(serde_json::json!({ "source_bytes": 5000, "source_mtime": time, "source_hash": "0" })), Verdict::Changed));
        // Samples alone can show a change, and cannot show that there was none.
        assert!(matches!(read(serde_json::json!({ "source_bytes": 5000, "source_mtime": time, "source_fingerprint": print })), Verdict::CannotTell));
        assert!(matches!(read(serde_json::json!({ "source_bytes": 5000, "source_mtime": time, "source_fingerprint": "0" })), Verdict::Changed));
        assert!(matches!(read(serde_json::json!({ "source_bytes": 5000, "source_fingerprint": print })), Verdict::CannotTell));
        assert!(matches!(read(serde_json::json!({ "source_bytes": 5000, "source_fingerprint": "0" })), Verdict::Changed));
        // A time alone says something only when it is the file's.
        assert!(matches!(read(serde_json::json!({ "source_bytes": 5000, "source_mtime": time })), Verdict::CannotTell));
        assert!(matches!(read(serde_json::json!({ "source_bytes": 5000, "source_mtime": 1_700_000_900u64 })), Verdict::Same));
        assert!(matches!(read(serde_json::json!({ "source_bytes": 4000 })), Verdict::OtherSize { .. }));
    }

    #[test]
    fn a_report_with_neither_time_nor_fingerprint_goes_by_the_size_alone() {
        // Reports from before either was kept: nothing else can be asked of them, whatever has
        // become of the file's date. Null is what a report of a later version holds for a value
        // it could not get, and it counts the same.
        let f = Sample::new("oldest", 5000);
        f.set_time(1_600_000_000);
        for report in [serde_json::json!({ "source_bytes": 5000 }), serde_json::json!({ "source_bytes": 5000, "source_mtime": null, "source_fingerprint": null })] {
            assert!(matches!(compare(&Recorded::in_report(&report), &f.0), Verdict::Same), "{report}");
        }
        assert!(matches!(compare(&Recorded::in_report(&serde_json::json!({ "source_bytes": 5001 })), &f.0), Verdict::OtherSize { .. }));
    }

    #[test]
    fn what_a_report_lacks_is_said_as_it_is() {
        let of = |r: Value| Recorded::in_report(&r).why_it_cannot_tell();
        assert!(of(serde_json::json!({ "source_mtime": 1 })).contains("from an earlier version"));
        assert!(of(serde_json::json!({ "source_mtime": 1, "source_fingerprint": null })).contains("could not be read when its encode began"));
        assert!(of(serde_json::json!({ "source_mtime": 1, "source_fingerprint": "ab" })).contains("only samples"));
    }

    #[test]
    fn a_list_does_not_read_whole_files_and_what_acts_on_it_does() {
        let f = Sample::new("list", 5000);
        let then = Recorded { whole: Some("0".into()), ..f.recorded() };
        f.set_time(1_700_000_900);
        assert!(matches!(compare_for_list(&then, &f.0), Verdict::Same));
        assert!(matches!(compare(&then, &f.0), Verdict::Changed));
        // What that look found is shown from then on, without reading the file again.
        assert!(matches!(compare_for_list(&then, &f.0), Verdict::Changed));
        // The rest of the rule is the same for it.
        let cannot = Recorded { whole: None, ..then.clone() };
        assert!(matches!(compare_for_list(&cannot, &f.0), Verdict::CannotTell));
    }

    #[test]
    fn a_file_rewritten_with_its_time_set_back_is_changed_for_what_acts_on_the_answer() {
        let f = Sample::new("set-back", 5000);
        let then = f.encoded();
        let samples_only = Recorded { whole: None, ..then.clone() };
        // Touched, and looked at: size and time now stand for contents that were the right ones.
        f.set_time(1_700_000_900);
        assert!(matches!(compare(&then, &f.0), Verdict::Same));
        assert!(matches!(compare(&samples_only, &f.0), Verdict::CannotTell));
        // Another byte, the same size, and the time it had when it was looked at.
        f.write_at(0, b"x");
        f.set_time(1_700_000_900);
        // Only a list may go by what it remembers.
        assert!(matches!(compare_for_list(&then, &f.0), Verdict::Same));
        assert!(matches!(compare_for_list(&samples_only, &f.0), Verdict::CannotTell));
        assert!(matches!(compare(&then, &f.0), Verdict::Changed));
        assert!(matches!(compare(&samples_only, &f.0), Verdict::Changed));
    }

    /// Runs `work` where a hang shows as a failure and not as a test that never ends.
    fn soon<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
        let (tell, answer) = channel();
        std::thread::spawn(move || drop(tell.send(work())));
        answer.recv_timeout(Duration::from_secs(30)).expect("it should not hang")
    }

    #[test]
    fn a_named_pipe_is_refused_without_being_opened() {
        let f = Sample::new("pipe", 10);
        let then = f.recorded();
        std::fs::remove_file(&f.0).unwrap();
        let name = std::ffi::CString::new(f.0.to_str().unwrap()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let path = f.0.clone();
        let (of, seen) = soon(move || {
            let of = Recorded::of(&path).map(|_| ()).unwrap_err();
            (of, compare(&Recorded { modified: Some(1), ..then }, &path))
        });
        assert_eq!(of.kind(), ErrorKind::InvalidInput);
        assert!(of.to_string().contains("not a regular file"), "{of}");
        assert!(matches!(seen, Verdict::Unreadable(e) if e.kind() == ErrorKind::InvalidInput));
    }

    #[test]
    fn a_read_that_gets_nothing_is_given_up_and_one_that_goes_on_is_not() {
        let stalled = soon(|| {
            STALL_IN_TESTS.set(Some(Duration::from_millis(100)));
            read_watched(|_| -> std::io::Result<()> {
                std::thread::sleep(Duration::from_secs(600));
                Ok(())
            })
        });
        assert_eq!(stalled.unwrap_err().kind(), ErrorKind::TimedOut);
        // Longer than the limit in all, with bytes coming in all the while.
        let done = soon(|| {
            STALL_IN_TESTS.set(Some(Duration::from_millis(300)));
            read_watched(|got| {
                for _ in 0..12 {
                    std::thread::sleep(Duration::from_millis(50));
                    got.fetch_add(1, Ordering::Relaxed);
                }
                Ok(7)
            })
        });
        assert_eq!(done.unwrap(), 7);
        // Opening a pipe nobody writes to never ends, which is a stall like any other.
        let f = Sample::new("stall", 10);
        std::fs::remove_file(&f.0).unwrap();
        let name = std::ffi::CString::new(f.0.to_str().unwrap()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let path = f.0.clone();
        let e = soon(move || {
            STALL_IN_TESTS.set(Some(Duration::from_millis(100)));
            fingerprint(&path, 10).unwrap_err()
        });
        assert_eq!(e.kind(), ErrorKind::TimedOut);
        // So is a hash of the whole file, which also stops when nobody wants it any more.
        let path = f.0.clone();
        let e = soon(move || {
            STALL_IN_TESTS.set(Some(Duration::from_millis(100)));
            WholeHash::start(&path, 10).finish(&|| false).unwrap_err()
        });
        assert_eq!(e.kind(), ErrorKind::TimedOut);
        let path = f.0.clone();
        let e = soon(move || WholeHash::start(&path, 10).finish(&|| true).unwrap_err());
        assert_eq!(e.kind(), ErrorKind::Interrupted);
    }

    #[test]
    fn a_file_that_cannot_be_looked_at_is_not_called_changed() {
        use std::os::unix::fs::PermissionsExt;
        let f = Sample::new("gone", 10);
        let then = f.encoded();
        let samples_only = Recorded { whole: None, ..then.clone() };
        // There, at another time, and not to be read: only reading it could tell, and that fails.
        f.set_time(1_700_000_900);
        std::fs::set_permissions(&f.0, std::fs::Permissions::from_mode(0o000)).unwrap();
        let denied = |v: Verdict| matches!(v, Verdict::Unreadable(e) if e.kind() == ErrorKind::PermissionDenied);
        assert!(denied(compare(&then, &f.0)));
        assert!(denied(compare(&samples_only, &f.0)));
        assert!(denied(compare_to_act(&then, &f.0, &WholeReads::default()).0));
        std::fs::remove_file(&f.0).unwrap();
        assert!(matches!(compare(&then, &f.0), Verdict::Unreadable(e) if e.kind() == ErrorKind::NotFound));
    }

    #[test]
    fn the_fingerprint_and_the_hash_stay_what_reports_hold() {
        // Over three megabytes of bytes that differ, so that the size of the samples and where
        // they are taken both count. Written out: reports keep these values, and another result
        // would call every file changed, or every changed file the same.
        let bytes: Vec<u8> = (0..3_500_017u64).map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8).collect();
        let f = Sample::holding("stable", bytes);
        let then = f.encoded();
        assert_eq!(then.fingerprint.as_deref(), Some("a3fd411f1146f5f4"));
        assert_eq!(then.whole.as_deref(), Some("62e3f8906c02c13c"));
    }

    #[test]
    fn the_fingerprint_of_a_small_file_stays_what_earlier_versions_wrote() {
        let f = Sample::new("stable-small", 5000);
        assert_eq!(f.recorded().fingerprint.as_deref(), Some("d628ee27255a67f4"));
    }
}
