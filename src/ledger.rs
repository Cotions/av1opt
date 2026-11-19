//! av1opt's own record of what it made and moved, in `$XDG_DATA_HOME/av1opt/ledger.jsonl`.
//!
//! A report lies beside its output, in a folder others may be able to write to, so what it says
//! is only a claim. The page moves files and opens them on the strength of this record instead:
//! one line for each encode that finished, each original sent to the trash and each one brought
//! back, in a folder and a file only this user can reach. Lines are only ever added, and one
//! that cannot be read costs that line alone.

use crate::source::Recorded;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use crate::replace::{OpenFile, Plan, Replaced};
use std::collections::{HashMap, HashSet};
#[cfg(any(feature = "ui", test))]
use std::sync::{Arc, Mutex};

/// Version of a line. Lines newer than this are ignored rather than misread.
const FORMAT: u32 = 1;

const FILE_NAME: &str = "ledger.jsonl";

/// What the page says of an output this record does not hold.
pub const NOT_RECORDED: &str = "av1opt has no record of making this output here (it was made elsewhere, moved, made before this record began, or its report was put there by someone else), so the page will not move files for it; if you trust its report, run `av1opt adopt` in a terminal on the output's folder, or let `av1opt replace` there do it";

/// What is said of a report that tells another story than the record.
const DISAGREES: &str = "the report beside this output does not say what av1opt recorded when it made and moved these files, so the record does not vouch for it; if you trust the report, `av1opt replace` in a terminal can do it, given the original's folder or --anywhere";

#[cfg(test)]
thread_local! {
    /// The data folder in the tests of this thread. Without one nothing is recorded: tests
    /// must never reach the user's own record.
    pub(crate) static TEST_FOLDER: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
thread_local! {
    /// How often this thread has read the record, for the tests that count it.
    pub(crate) static LOADS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn folder() -> Option<PathBuf> {
    TEST_FOLDER.with(|f| f.borrow().clone())
}

#[cfg(not(test))]
fn folder() -> Option<PathBuf> {
    let base = crate::files::xdg_folder("XDG_DATA_HOME", ".local/share")?;
    Some(base.join("av1opt"))
}

/// What tells an original from any other file, under the names a report gives the same facts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Original {
    source_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_mtime: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_hash: Option<String>,
}

impl Original {
    fn new(source_bytes: u64, known: &Recorded) -> Original {
        Original { source_bytes, source_mtime: known.modified, source_fingerprint: known.fingerprint.clone(), source_hash: known.whole.clone() }
    }

    /// Whether a report says the same of the original. `replace` compares the file with what the
    /// report says, so a report that says this has had the file compared with the record.
    fn is_what(&self, said: &Recorded) -> bool {
        said.size == Some(self.source_bytes) && said.modified == self.source_mtime && said.fingerprint == self.source_fingerprint && said.whole == self.source_hash
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum Event {
    Encoded {
        output: PathBuf,
        report: PathBuf,
        source: PathBuf,
        #[serde(flatten)]
        original: Original,
        output_bytes: u64,
        /// Of the whole output: whoever can write to its folder can change every part of it,
        /// and this is what it would be put in the original's place on the strength of.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_sha256: Option<String>,
    },
    Replaced {
        source: PathBuf,
        trash: PathBuf,
        /// Where the AV1 file is now.
        output: PathBuf,
        /// Where it was before.
        from: PathBuf,
        /// The original as it went to the trash.
        #[serde(flatten)]
        original: Original,
        output_bytes: u64,
    },
    Undone {
        source: PathBuf,
        /// Where the AV1 file was while it stood in for the original.
        output: PathBuf,
        /// Where it is now.
        back_to: PathBuf,
    },
    /// An output from before this record, or from elsewhere, taken in by `av1opt adopt`: the
    /// person named its folder, and so vouched for its report.
    Adopted {
        /// Where the AV1 file is now.
        output: PathBuf,
        report: PathBuf,
        source: PathBuf,
        /// The original as it was encoded.
        #[serde(flatten)]
        original: Original,
        output_bytes: u64,
        output_sha256: String,
        /// Where the original is, if the output has taken its place already.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replaced: Option<WentToTrash>,
    },
    /// The one look at the encode history, made the first time the page starts with this record.
    TakenOver { history_lines: usize, taken: usize },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WentToTrash {
    trash: PathBuf,
    /// Where the AV1 file was before.
    from: PathBuf,
    /// The original as it went.
    #[serde(flatten)]
    original: Original,
}

#[derive(Serialize, Deserialize)]
struct Line {
    format: u32,
    time: u64,
    #[serde(flatten)]
    event: Event,
}

/// What an encode left, as it was when it finished.
#[derive(Debug, Clone, PartialEq)]
pub struct Made {
    pub original: Original,
    pub output_bytes: u64,
    pub output_sha256: Option<String>,
}

/// An original in the trash, and where its AV1 file came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Moved {
    pub trash: PathBuf,
    pub from: PathBuf,
    pub original: Original,
    pub output_bytes: u64,
}

/// An output as this record knows it, under the path it has now.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub source: PathBuf,
    /// None for an output only known from a replacement made on the command line.
    pub made: Option<Made>,
    pub replaced: Option<Moved>,
}

/// How an output stands for being taken on record.
#[derive(Debug, PartialEq)]
pub enum Standing {
    NotYet,
    OnRecord,
    Refused(String),
}

/// What the record says as of its last line.
#[derive(Debug, Default)]
pub struct Ledger {
    outputs: HashMap<PathBuf, Entry>,
    #[cfg_attr(not(any(feature = "ui", test)), allow(dead_code))]
    files: HashSet<PathBuf>,
    #[cfg_attr(not(any(feature = "ui", test)), allow(dead_code))]
    taken_over: bool,
    /// Why the record on disk was not used.
    problem: Option<String>,
}

impl Ledger {
    fn from_lines(bytes: &[u8]) -> Ledger {
        let mut ledger = Ledger::default();
        let lines = bytes.split(|b| *b == b'\n').filter_map(|l| serde_json::from_slice::<Line>(l).ok()).filter(|l| l.format <= FORMAT);
        for line in lines {
            ledger.apply(line.event);
        }
        // The page asks with links resolved. Sources and outputs are recorded that way; the
        // trash is recorded as the trash names it, and its folder may lie behind a link.
        let beside = |e: &Entry| e.replaced.as_ref().map_or_else(|| e.source.clone(), |m| std::fs::canonicalize(&m.trash).unwrap_or_else(|_| m.trash.clone()));
        ledger.files = ledger.outputs.iter().flat_map(|(output, e)| [output.clone(), beside(e)]).collect();
        ledger
    }

    fn apply(&mut self, event: Event) {
        match event {
            Event::Encoded { output, report: _, source, original, output_bytes, output_sha256 } => {
                self.outputs.insert(output, Entry { source, made: Some(Made { original, output_bytes, output_sha256 }), replaced: None });
            }
            Event::Replaced { source, trash, output, from, original, output_bytes } => {
                // A replacement on the command line can start from a report naming anything: it
                // takes over what is known of `from` only if that is this same pair.
                let made = if self.outputs.get(&from).is_some_and(|e| e.source == source) { self.outputs.remove(&from).and_then(|e| e.made) } else { None };
                self.outputs.insert(output, Entry { source, made, replaced: Some(Moved { trash, from, original, output_bytes }) });
            }
            Event::Undone { source, output, back_to } => {
                if self.outputs.get(&output).is_some_and(|e| e.source == source)
                    && let Some(mut entry) = self.outputs.remove(&output)
                {
                    entry.replaced = None;
                    self.outputs.insert(back_to, entry);
                }
            }
            // What av1opt saw for itself is never written over by what a report says.
            Event::Adopted { output, report: _, source, original, output_bytes, output_sha256, replaced } => {
                if self.known_at(&output).is_none() {
                    let replaced = replaced.map(|m| Moved { trash: m.trash, from: m.from, original: m.original, output_bytes });
                    self.outputs.insert(output, Entry { source, made: Some(Made { original, output_bytes, output_sha256: Some(output_sha256) }), replaced });
                }
            }
            Event::TakenOver { .. } => self.taken_over = true,
        }
    }

    /// What is recorded of whatever output is at `output`.
    fn known_at(&self, output: &Path) -> Option<&Entry> {
        self.outputs.get(output).filter(|e| e.made.is_some() || e.replaced.is_some())
    }

    /// What is recorded of the output at `output`, if it was made from `source` or took its
    /// place. An output put back by the command line that av1opt never saw made is not one.
    pub fn entry(&self, output: &Path, source: &Path) -> Option<&Entry> {
        self.known_at(output).filter(|e| e.source == source)
    }

    /// Why the record on disk is not being used, if it is not.
    pub fn problem(&self) -> Option<&str> {
        self.problem.as_deref()
    }

    /// Whether the record speaks for this output and this original, so that the report's word
    /// is not needed for where the original is. All of the output is compared before it moves.
    pub fn vouches_for(&self, p: &Plan) -> bool {
        self.check_ready(p, None).is_ok() && self.entry(&p.output, &p.source).and_then(|e| e.made.as_ref()).is_some_and(|m| m.output_sha256.is_some())
    }

    /// What `av1opt adopt` can do about an output that can be replaced.
    pub fn adopting(&self, p: &Plan) -> Standing {
        self.standing(&p.output, &p.source, || self.check_ready(p, None))
    }

    /// What `av1opt adopt` can do about an output that has replaced its original.
    pub fn adopting_replaced(&self, d: &Replaced) -> Standing {
        self.standing(&d.dest, &d.source, || self.check_replaced(d))
    }

    fn standing(&self, output: &Path, source: &Path, agrees: impl Fn() -> Result<(), String>) -> Standing {
        match self.known_at(output) {
            None => Standing::NotYet,
            // A report must not be able to give a file on record another past.
            Some(e) if e.source != source => Standing::Refused(format!("av1opt's record says this output came from another original, {}", e.source.display())),
            Some(_) => agrees().map_or_else(Standing::Refused, |()| Standing::OnRecord),
        }
    }

    /// Whether `file`, a path with its links resolved, is an output, a source or an original in
    /// the trash on record, and a plain file now.
    #[cfg(any(feature = "ui", test))]
    pub fn covers(&self, file: &Path) -> bool {
        self.files.contains(file) && std::fs::symlink_metadata(file).is_ok_and(|m| m.is_file())
    }

    /// What to tell someone asking the page to act on an output that is not on record.
    pub fn refusal(&self) -> String {
        match &self.problem {
            Some(why) => format!("{NOT_RECORDED} (av1opt's record is not being used: {why})"),
            None => NOT_RECORDED.to_string(),
        }
    }

    /// Whether the page may put this output in its original's place. `replace` has compared the
    /// original with what the report says, so that has to be what is recorded. With the output
    /// open, for when something is about to be moved, all of it is read through that.
    pub fn check_ready(&self, p: &Plan, output: Option<&OpenFile>) -> Result<(), String> {
        let entry = self.entry(&p.output, &p.source).ok_or_else(|| self.refusal())?;
        if entry.replaced.is_some() {
            return Err("av1opt's record says this original has already been replaced".into());
        }
        let made = entry.made.as_ref().ok_or_else(|| self.refusal())?;
        if p.source_bytes != made.original.source_bytes || !made.original.is_what(&p.original) {
            return Err(DISAGREES.into());
        }
        if p.output_bytes != made.output_bytes {
            return Err("the output is not the file av1opt made: its size differs".into());
        }
        let Some(output) = output else { return Ok(()) };
        let wanted = made.output_sha256.as_deref().ok_or("av1opt could not record the contents of this output when it was made, so the page will not move it; `av1opt replace` in a terminal can")?;
        match output.sha256() {
            Ok(now) if now == wanted => Ok(()),
            Ok(_) => Err("the output is not the file av1opt made: its contents differ".into()),
            Err(e) => Err(format!("the output could not be read to check that it is the file av1opt made: {e}")),
        }
    }

    /// Whether the page may undo this replacement: the original, the trash entry and the place
    /// the AV1 file goes back to have to be the ones on record, whatever the report says.
    pub fn check_replaced(&self, d: &Replaced) -> Result<(), String> {
        let entry = self.entry(&d.dest, &d.source).ok_or_else(|| self.refusal())?;
        let moved = entry.replaced.as_ref().ok_or_else(|| self.refusal())?;
        let same = d.trash == moved.trash && d.recorded_from == moved.from && d.output_bytes == moved.output_bytes && d.source_bytes == moved.original.source_bytes && moved.original.is_what(&d.original);
        if same { Ok(()) } else { Err(DISAGREES.into()) }
    }
}

/// The record as of now, read again only when the file has changed: `/media` asks on every
/// request a player makes.
#[cfg(any(feature = "ui", test))]
#[derive(Default)]
pub struct Shared(Mutex<Option<(Option<Stamp>, Arc<Ledger>)>>);

type Stamp = [u64; 7];

#[cfg(any(feature = "ui", test))]
impl Shared {
    pub fn current(&self) -> Arc<Ledger> {
        let stamp = stamp();
        let mut held = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((seen, ledger)) = &*held
            && stamp.is_some()
            && *seen == stamp
        {
            return ledger.clone();
        }
        let ledger = Arc::new(load());
        *held = Some((stamp, ledger.clone()));
        ledger
    }
}

/// Lines are only added, so the same file at the same length holds the same lines. Who owns the
/// file and its folder and who may write to them count too: a change there is looked at anew.
fn stamp() -> Option<Stamp> {
    let dir = folder()?;
    let d = std::fs::metadata(&dir).ok()?;
    let f = std::fs::symlink_metadata(dir.join(FILE_NAME)).ok()?;
    Some([f.dev(), f.ino(), f.len(), u64::from(f.uid()), u64::from(f.mode()), u64::from(d.uid()), u64::from(d.mode())])
}

fn my_user() -> u32 {
    // SAFETY: geteuid takes nothing and cannot fail.
    unsafe { libc::geteuid() }
}

/// Why this folder cannot hold the record, if it cannot.
fn folder_problem(meta: &std::fs::Metadata, dir: &Path) -> Option<String> {
    if !meta.is_dir() {
        return Some(format!("{} is not a folder", dir.display()));
    }
    if meta.uid() != my_user() {
        return Some(format!("{} belongs to another user", dir.display()));
    }
    (meta.mode() & 0o022 != 0).then(|| format!("others can write to {}", dir.display()))
}

/// Why this file cannot be the record, if it cannot.
fn file_problem(meta: &std::fs::Metadata, path: &Path) -> Option<String> {
    if !meta.is_file() {
        return Some(format!("{} is not a plain file", path.display()));
    }
    if meta.uid() != my_user() {
        return Some(format!("{} belongs to another user", path.display()));
    }
    (meta.mode() & 0o077 != 0).then(|| format!("others can reach {}", path.display()))
}

/// Reads the record. One that is missing is empty; one others could have written to is not
/// believed, and the page then acts as if nothing were on record.
pub fn load() -> Ledger {
    #[cfg(test)]
    LOADS.with(|n| n.set(n.get() + 1));
    let Some(dir) = folder() else { return Ledger::default() };
    let unusable = |why: String| Ledger { problem: Some(why), ..Ledger::default() };
    let path = dir.join(FILE_NAME);
    match std::fs::metadata(&dir) {
        Ok(meta) => {
            if let Some(why) = folder_problem(&meta, &dir) {
                return unusable(why);
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ledger::default(),
        Err(e) => return unusable(format!("cannot look at {}: {e}", dir.display())),
    }
    let opened = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&path);
    let mut file = match opened {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ledger::default(),
        Err(e) => return unusable(format!("cannot open {}: {e}", path.display())),
    };
    match file.metadata() {
        Ok(meta) => {
            if let Some(why) = file_problem(&meta, &path) {
                return unusable(why);
            }
        }
        Err(e) => return unusable(format!("cannot look at {}: {e}", path.display())),
    }
    let mut bytes = Vec::new();
    match file.read_to_end(&mut bytes) {
        Ok(_) => Ledger::from_lines(&bytes),
        Err(e) => unusable(format!("cannot read {}: {e}", path.display())),
    }
}

/// Makes the data folder, or closes one an earlier version left open to others (the folder
/// follows the umask there), and says where the record is.
fn private_folder() -> Result<Option<PathBuf>> {
    let Some(dir) = folder() else { return Ok(None) };
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).with_context(|| format!("cannot make {}", dir.display()))?;
    let meta = std::fs::metadata(&dir).with_context(|| format!("cannot look at {}", dir.display()))?;
    if meta.is_dir() && meta.uid() == my_user() && meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).with_context(|| format!("cannot close {} to other users", dir.display()))?;
    }
    let meta = std::fs::metadata(&dir).with_context(|| format!("cannot look at {}", dir.display()))?;
    if let Some(why) = folder_problem(&meta, &dir) {
        bail!("{why}");
    }
    Ok(Some(dir.join(FILE_NAME)))
}

fn add(event: Event) -> Result<()> {
    let Some(path) = private_folder()? else { return Ok(()) };
    let file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    let mut meta = file.metadata()?;
    if meta.is_file() && meta.uid() == my_user() && meta.mode() & 0o077 != 0 {
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        meta = file.metadata()?;
    }
    if let Some(why) = file_problem(&meta, &path) {
        bail!("{why}");
    }
    let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let line = serde_json::to_string(&Line { format: FORMAT, time, event })?;
    crate::files::append_line(&path, &line, false).with_context(|| format!("cannot write to {}", path.display()))
}

/// What an encode leaves on record once its output and report are in place.
pub struct Encode<'a> {
    pub output: &'a Path,
    pub report: &'a Path,
    pub source: &'a Path,
    pub source_bytes: u64,
    /// The source as the report describes it.
    pub original: &'a Recorded,
    pub output_bytes: u64,
    /// Without it the output is on record but stays where it is: the page has nothing to tell
    /// it from a file put there later.
    pub output_sha256: Option<String>,
}

pub fn note_encoded(e: &Encode) -> Result<()> {
    add(Event::Encoded {
        output: e.output.to_path_buf(),
        report: e.report.to_path_buf(),
        source: e.source.to_path_buf(),
        original: Original::new(e.source_bytes, e.original),
        output_bytes: e.output_bytes,
        output_sha256: e.output_sha256.clone(),
    })
}

/// An original gone to the trash and the output that took its place.
pub struct Replacement<'a> {
    pub original: &'a Path,
    pub trash: &'a Path,
    pub output: &'a Path,
    pub from: &'a Path,
    pub source_bytes: u64,
    /// The original as it went, which is what the file in the trash has to stay.
    pub known: &'a Recorded,
    pub output_bytes: u64,
}

pub fn note_replaced(r: &Replacement) -> Result<()> {
    add(Event::Replaced {
        source: r.original.to_path_buf(),
        trash: r.trash.to_path_buf(),
        output: r.output.to_path_buf(),
        from: r.from.to_path_buf(),
        original: Original::new(r.source_bytes, r.known),
        output_bytes: r.output_bytes,
    })
}

pub fn note_undone(source: &Path, output: &Path, back_to: &Path) -> Result<()> {
    add(Event::Undone { source: source.to_path_buf(), output: output.to_path_buf(), back_to: back_to.to_path_buf() })
}

/// Gets the record's folder ready for `av1opt adopt`, as the page does when it starts: one an
/// earlier version left open to others is closed, and then the record in it can be used.
pub fn make_ready() -> Result<()> {
    private_folder().map(drop)
}

/// An output `av1opt adopt` takes on record.
pub struct Adoption<'a> {
    pub output: &'a Path,
    pub report: &'a Path,
    pub source: &'a Path,
    pub source_bytes: u64,
    /// The original as the report describes it, from when it was encoded.
    pub original: &'a Recorded,
    pub output_bytes: u64,
    pub output_sha256: String,
    pub replaced: Option<AdoptedReplacement<'a>>,
}

/// Where things are for an adopted output that has replaced its original.
pub struct AdoptedReplacement<'a> {
    pub trash: &'a Path,
    pub from: &'a Path,
    /// The original as it went to the trash.
    pub known: &'a Recorded,
}

/// The record as one long run holds it: read once, kept up to date with what this run adds,
/// and read again only when something else has written to it since.
pub struct Held {
    stamp: Option<Stamp>,
    ledger: Ledger,
}

impl Held {
    pub fn load() -> Held {
        // Taken first: a line added in between makes the next look read the file again.
        let stamp = stamp();
        Held { stamp, ledger: load() }
    }

    pub fn current(&mut self) -> &Ledger {
        // No file before and none now is still nothing on record.
        if stamp() != self.stamp {
            *self = Held::load();
        }
        &self.ledger
    }

    /// Takes an output on record, in the file and in what is held of it.
    pub fn note_adopted(&mut self, a: Adoption) -> Result<()> {
        if folder().is_none() {
            bail!("there is no folder to keep av1opt's record in (neither XDG_DATA_HOME nor HOME is an absolute path)");
        }
        let replaced = a.replaced.map(|m| WentToTrash { trash: m.trash.to_path_buf(), from: m.from.to_path_buf(), original: Original::new(a.source_bytes, m.known) });
        let event = Event::Adopted {
            output: a.output.to_path_buf(),
            report: a.report.to_path_buf(),
            source: a.source.to_path_buf(),
            original: Original::new(a.source_bytes, a.original),
            output_bytes: a.output_bytes,
            output_sha256: a.output_sha256,
            replaced,
        };
        let unchanged = stamp() == self.stamp;
        add(event.clone())?;
        if unchanged {
            self.ledger.apply(event);
            self.stamp = stamp();
        } else {
            *self = Held::load();
        }
        Ok(())
    }
}

/// Gets the record ready when the page starts, and the first time looks at the encode history
/// for encodes to take over. That history keeps numbers for the size model and no file names,
/// so there is nothing in it to take: a report that claims to match one of its lines would be
/// believed on its own word, and anybody who can read the history could write such a report.
/// Returns what the person should hear.
#[cfg(feature = "ui")]
pub fn start_page() -> Option<String> {
    start_page_with(&|| crate::history::load().len())
}

#[cfg(any(feature = "ui", test))]
fn start_page_with(history_lines: &dyn Fn() -> usize) -> Option<String> {
    if let Err(e) = private_folder() {
        return Some(format!("av1opt's record of its own encodes cannot be used ({e:#}), so the page will not replace originals or put them back; `av1opt replace` in a terminal still can"));
    }
    let ledger = load();
    if let Some(why) = &ledger.problem {
        return Some(format!("av1opt's record of its own encodes cannot be used ({why}), so the page will not replace originals or put them back; `av1opt replace` in a terminal still can"));
    }
    if ledger.taken_over {
        return None;
    }
    let history_lines = history_lines();
    if let Err(e) = add(Event::TakenOver { history_lines, taken: 0 }) {
        eprintln!("warning: could not write to av1opt's record of its own encodes: {e:#}");
    }
    (history_lines > 0).then(|| {
        format!(
            "av1opt now keeps a private record of the encodes it makes, and the page replaces originals only for outputs in it. Taken over from the encode history: 0 of {history_lines} past encodes, because that history holds no file names. Their outputs stay listed in History; `av1opt adopt` in a terminal, on their folder, takes them in, and `av1opt replace` there can still replace them."
        )
    })
}

/// The SHA-256 of a whole file, as hexadecimal text. A hash nobody can make a second file for
/// on purpose: the file it vouches for may lie where others can write. A drive that stops
/// answering costs this read and not whoever asked.
pub fn sha256_of_file(path: &Path) -> std::io::Result<String> {
    let path = path.to_path_buf();
    crate::source::read_watched(move |got| {
        let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&path)?;
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::other("not a plain file"));
        }
        sha256_reading(&file, got)
    })
}

/// [`sha256_of_file`] of a file already open, which is then the file the answer is about,
/// whatever has become of its name.
pub fn sha256_of_open(file: &std::fs::File) -> std::io::Result<String> {
    let file = file.try_clone()?;
    crate::source::read_watched(move |got| sha256_reading(&file, got))
}

/// Reads by position, leaving alone the place in the file that others sharing it go by.
fn sha256_reading(file: &std::fs::File, got: &std::sync::atomic::AtomicU64) -> std::io::Result<String> {
    let mut hash = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    let mut at = 0;
    loop {
        match file.read_at(&mut buffer, at) {
            Ok(0) => return Ok(hash.finish()),
            Ok(n) => {
                hash.add(&buffer[..n]);
                at += n as u64;
                got.fetch_add(n as u64, Ordering::Relaxed);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

const ROUND_CONSTANTS: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// Takes the hash on by whole blocks.
type Compression = fn(&mut [u32; 8], &[[u8; 64]]);

/// SHA-256 as FIPS 180-4 has it, written out because the program has no cryptography library.
/// Every encode and every replacement reads a whole video through this, so it works on as many
/// blocks at once as it is given, with the processor's own instructions where there are any.
struct Sha256 {
    state: [u32; 8],
    pending: Vec<u8>,
    length: u64,
    compress: Compression,
}

impl Sha256 {
    fn new() -> Sha256 {
        Sha256::using(fastest_compression())
    }

    fn using(compress: Compression) -> Sha256 {
        Sha256 { state: [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19], pending: Vec::with_capacity(64), length: 0, compress }
    }

    fn add(&mut self, mut bytes: &[u8]) {
        self.length = self.length.wrapping_add(bytes.len() as u64);
        if !self.pending.is_empty() {
            let taken = bytes.len().min(64 - self.pending.len());
            self.pending.extend_from_slice(&bytes[..taken]);
            bytes = &bytes[taken..];
            if self.pending.len() < 64 {
                return;
            }
            let block: [u8; 64] = self.pending.as_slice().try_into().expect("64 bytes are pending");
            (self.compress)(&mut self.state, &[block]);
            self.pending.clear();
        }
        let (blocks, rest) = bytes.as_chunks::<64>();
        (self.compress)(&mut self.state, blocks);
        self.pending.extend_from_slice(rest);
    }

    fn finish(mut self) -> String {
        let bits = self.length.wrapping_mul(8);
        let padding = 1 + (119 - (self.length % 64) as usize) % 64;
        let mut end = vec![0u8; padding];
        end[0] = 0x80;
        end.extend_from_slice(&bits.to_be_bytes());
        self.add(&end);
        self.state.iter().map(|word| format!("{word:08x}")).collect()
    }
}

#[cfg(target_arch = "x86_64")]
fn fastest_compression() -> Compression {
    if std::is_x86_feature_detected!("sha") && std::is_x86_feature_detected!("ssse3") && std::is_x86_feature_detected!("sse4.1") { compress_by_processor } else { compress_by_hand }
}

#[cfg(not(target_arch = "x86_64"))]
fn fastest_compression() -> Compression {
    compress_by_hand
}

/// Only ever chosen by [`fastest_compression`], once the processor has said it has these.
#[cfg(target_arch = "x86_64")]
fn compress_by_processor(state: &mut [u32; 8], blocks: &[[u8; 64]]) {
    // SAFETY: the instructions this needs were found to be there before this was chosen.
    unsafe { compress_with_sha_instructions(state, blocks) }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sha,ssse3,sse4.1")]
fn compress_with_sha_instructions(state: &mut [u32; 8], blocks: &[[u8; 64]]) {
    use std::arch::x86_64::*;
    let big_endian = _mm_set_epi64x(0x0c0d0e0f08090a0b, 0x0405060700010203);
    // SAFETY: each pointer is to 16 bytes inside the array or block it was taken from, and
    // these loads and stores need no alignment.
    let load = |bytes: *const u8| unsafe { _mm_loadu_si128(bytes.cast()) };
    // The instructions want the words as ABEF and CDGH.
    let first = _mm_shuffle_epi32(load(state.as_ptr().cast()), 0xB1);
    let second = _mm_shuffle_epi32(load(state[4..].as_ptr().cast()), 0x1B);
    let mut abef = _mm_alignr_epi8(first, second, 8);
    let mut cdgh = _mm_blend_epi16(second, first, 0xF0);
    for block in blocks {
        let (abef_before, cdgh_before) = (abef, cdgh);
        let mut schedule = [0, 16, 32, 48].map(|at| _mm_shuffle_epi8(load(block[at..].as_ptr()), big_endian));
        for (group, constants) in ROUND_CONSTANTS.as_chunks::<4>().0.iter().enumerate() {
            if group >= 4 {
                let at = |offset: usize| (group + offset) % 4;
                let mixed = _mm_add_epi32(_mm_sha256msg1_epu32(schedule[at(0)], schedule[at(1)]), _mm_alignr_epi8(schedule[at(3)], schedule[at(2)], 4));
                schedule[at(0)] = _mm_sha256msg2_epu32(mixed, schedule[at(3)]);
            }
            let words = _mm_add_epi32(schedule[group % 4], load(constants.as_ptr().cast()));
            cdgh = _mm_sha256rnds2_epu32(cdgh, abef, words);
            abef = _mm_sha256rnds2_epu32(abef, cdgh, _mm_shuffle_epi32(words, 0x0E));
        }
        abef = _mm_add_epi32(abef, abef_before);
        cdgh = _mm_add_epi32(cdgh, cdgh_before);
    }
    let fe_ba = _mm_shuffle_epi32(abef, 0x1B);
    let dc_hg = _mm_shuffle_epi32(cdgh, 0xB1);
    // SAFETY: the two stores together fill the eight words of `state`, which need no alignment.
    unsafe {
        _mm_storeu_si128(state.as_mut_ptr().cast(), _mm_blend_epi16(fe_ba, dc_hg, 0xF0));
        _mm_storeu_si128(state[4..].as_mut_ptr().cast(), _mm_alignr_epi8(dc_hg, fe_ba, 8));
    }
}

/// The same for processors without those instructions.
fn compress_by_hand(state: &mut [u32; 8], blocks: &[[u8; 64]]) {
    for block in blocks {
        let mut w = [0u32; 64];
        for (word, bytes) in w.iter_mut().zip(block.as_chunks::<4>().0) {
            *word = u32::from_be_bytes(*bytes);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
        for (constant, word) in ROUND_CONSTANTS.iter().zip(w) {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ (!e & g);
            let t1 = h.wrapping_add(s1).wrapping_add(choice).wrapping_add(*constant).wrapping_add(word);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(majority);
            (h, g, f, e, d, c, b, a) = (g, f, e, d.wrapping_add(t1), c, b, a, t1.wrapping_add(t2));
        }
        for (now, more) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *now = now.wrapping_add(more);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_adopted_line_adds_what_is_missing_and_changes_nothing_known() {
        let line = |output: &str, source: &str, event: &str, more: &str| format!(r#"{{"format":1,"time":1,"event":"{event}","output":"{output}","report":"/r","source":"{source}","source_bytes":3,"output_bytes":1,"output_sha256":"aa"{more}}}"#);
        let trash = r#","replaced":{"trash":"/t/a.mp4","from":"/o/a.mkv","source_bytes":3,"source_mtime":9}"#;
        let text = [
            line("/o/known.mkv", "/v/known.mp4", "encoded", ""),
            line("/o/known.mkv", "/v/other.mp4", "adopted", ""),
            line("/o/new.mkv", "/v/new.mp4", "adopted", ""),
            line("/v/a.mkv", "/v/a.mp4", "adopted", trash),
        ]
        .join("\n");
        let ledger = Ledger::from_lines(text.as_bytes());
        assert!(ledger.entry(Path::new("/o/known.mkv"), Path::new("/v/known.mp4")).is_some());
        assert!(ledger.entry(Path::new("/o/known.mkv"), Path::new("/v/other.mp4")).is_none());
        let new = ledger.entry(Path::new("/o/new.mkv"), Path::new("/v/new.mp4")).unwrap();
        assert_eq!(new.made.as_ref().unwrap().output_sha256.as_deref(), Some("aa"));
        let moved = ledger.entry(Path::new("/v/a.mkv"), Path::new("/v/a.mp4")).unwrap().replaced.as_ref().unwrap();
        assert_eq!((moved.trash.as_path(), moved.from.as_path(), moved.original.source_mtime), (Path::new("/t/a.mp4"), Path::new("/o/a.mkv"), Some(9)));
    }
    use std::io::Write;

    /// A data folder of this test's own, used by the record for as long as it lives.
    struct Scratch(PathBuf);

    /// What the reports in these tests say of their originals.
    fn known() -> Recorded {
        Recorded { size: Some(3000), modified: Some(1_700_000_000), fingerprint: Some("samples".into()), whole: Some("whole".into()), ..Recorded::default() }
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!("av1opt-ledger-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TEST_FOLDER.with(|f| *f.borrow_mut() = Some(dir.join("data/av1opt")));
            Scratch(dir)
        }

        fn data(&self) -> PathBuf {
            self.0.join("data/av1opt")
        }

        fn file(&self) -> PathBuf {
            self.data().join(FILE_NAME)
        }

        /// A source and its output, on record as an encode.
        fn encoded(&self, name: &str) -> (PathBuf, PathBuf) {
            let (source, output) = (self.0.join(format!("{name}.mp4")), self.0.join("out").join(format!("{name}.mkv")));
            std::fs::create_dir_all(self.0.join("out")).unwrap();
            std::fs::write(&source, vec![b's'; 3000]).unwrap();
            std::fs::write(&output, vec![b'o'; 1000]).unwrap();
            let output_sha256 = Some(sha256_of_file(&output).unwrap());
            let report = output.with_extension("av1opt.json");
            note_encoded(&Encode { output: &output, report: &report, source: &source, source_bytes: 3000, original: &known(), output_bytes: 1000, output_sha256 }).unwrap();
            (source, output)
        }

        fn plan(&self, source: &Path, output: &Path) -> Plan {
            Plan {
                report: output.with_extension("av1opt.json"),
                source: source.to_path_buf(),
                output: output.to_path_buf(),
                dest: source.with_extension("mkv"),
                source_bytes: 3000,
                output_bytes: 1000,
                copy: false,
                zfs: false,
                removed: 0,
                original: known(),
                source_stamp: None,
            }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            TEST_FOLDER.with(|f| *f.borrow_mut() = None);
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().mode() & 0o777
    }

    fn moved(source: &Path, trash: &Path, dest: &Path, from: &Path) -> Result<()> {
        note_replaced(&Replacement { original: source, trash, output: dest, from, source_bytes: 3000, known: &known(), output_bytes: 1000 })
    }

    fn replaced(source: &Path, dest: &Path, trash: &Path, from: &Path) -> Replaced {
        Replaced {
            report: from.with_extension("av1opt.json"),
            source: source.to_path_buf(),
            dest: dest.to_path_buf(),
            from: from.to_path_buf(),
            recorded_from: from.to_path_buf(),
            trash: trash.to_path_buf(),
            source_bytes: 3000,
            output_bytes: 1000,
            original: known(),
            can_undo: true,
            zfs: false,
        }
    }

    #[test]
    fn the_folder_and_the_file_are_this_users_alone() {
        let s = Scratch::new("private");
        s.encoded("a");
        assert_eq!((mode(&s.data()), mode(&s.file())), (0o700, 0o600));
        // A folder an earlier version left open, and a file opened up since, are closed again.
        std::fs::set_permissions(s.data(), std::fs::Permissions::from_mode(0o775)).unwrap();
        std::fs::set_permissions(s.file(), std::fs::Permissions::from_mode(0o664)).unwrap();
        s.encoded("b");
        assert_eq!((mode(&s.data()), mode(&s.file())), (0o700, 0o600));
    }

    #[test]
    fn a_record_others_could_have_written_is_not_believed() {
        let s = Scratch::new("open");
        let (source, output) = s.encoded("a");
        assert!(load().entry(&output, &source).is_some());
        std::fs::set_permissions(s.file(), std::fs::Permissions::from_mode(0o666)).unwrap();
        let ledger = load();
        assert!(ledger.entry(&output, &source).is_none());
        assert!(ledger.refusal().contains("others can reach"), "{}", ledger.refusal());
        std::fs::set_permissions(s.file(), std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(s.data(), std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(load().refusal().contains("others can write"), "{}", load().refusal());
        // Nor is a link at its name, wherever it leads.
        std::fs::set_permissions(s.data(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let elsewhere = s.0.join("elsewhere.jsonl");
        std::fs::rename(s.file(), &elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, s.file()).unwrap();
        assert!(load().entry(&output, &source).is_none());
        assert!(moved(&source, &s.0.join("t"), &output, &output).is_err(), "a link was written through");
    }

    #[test]
    fn the_record_vouches_only_for_a_pair_as_it_recorded_it() {
        let s = Scratch::new("vouches");
        let (source, output) = s.encoded("a");
        let ledger = load();
        assert!(ledger.vouches_for(&s.plan(&source, &output)));
        // The same pair, with a report that tells another story of the original or the output:
        // `replace` compared the files with the report, not with the record.
        let other_original = Plan { original: Recorded { fingerprint: Some("other samples".into()), ..known() }, ..s.plan(&source, &output) };
        assert!(!ledger.vouches_for(&other_original));
        let other_output = Plan { output_bytes: 999, ..s.plan(&source, &output) };
        assert!(!ledger.vouches_for(&other_output));
        let replaced = s.0.join("a.mkv");
        moved(&source, &s.0.join("Trash/files/a.mp4"), &replaced, &output).unwrap();
        assert!(!load().vouches_for(&s.plan(&source, &replaced)));
    }

    #[test]
    fn a_damaged_line_costs_only_that_line() {
        let s = Scratch::new("damaged");
        let (first, first_out) = s.encoded("a");
        let mut f = std::fs::OpenOptions::new().append(true).open(s.file()).unwrap();
        f.write_all(b"{\"format\":1,\"event\":\"enc\xff\"}\nnot json\n{\"format\":99,\"time\":1,\"event\":\"taken_over\",\"history_lines\":1,\"taken\":1}\n{\"cut off").unwrap();
        drop(f);
        // What follows a line left without its end is not lost with it.
        let (second, second_out) = s.encoded("b");
        let ledger = load();
        assert!(ledger.entry(&first_out, &first).is_some());
        assert!(ledger.entry(&second_out, &second).is_some());
        assert!(!ledger.taken_over, "a line of a newer version was read");
    }

    #[test]
    fn what_a_line_holds_of_the_source_comes_back() {
        let s = Scratch::new("facts");
        let (source, output) = s.encoded("a");
        let made = load().entry(&output, &source).unwrap().made.clone().unwrap();
        assert_eq!(made.original, Original::new(3000, &known()));
        assert_eq!(made.original.source_hash.as_deref(), Some("whole"));
        assert!(made.original.is_what(&known()));
    }

    #[test]
    fn an_output_is_followed_through_its_replacement_and_back() {
        let s = Scratch::new("moves");
        let (source, output) = s.encoded("a");
        let (dest, trash) = (source.with_extension("mkv"), s.0.join("Trash/files/a.mp4"));
        assert!(load().covers(&output) && load().covers(&source));
        moved(&source, &trash, &dest, &output).unwrap();
        let ledger = load();
        assert!(ledger.entry(&output, &source).is_none());
        let on_record = ledger.entry(&dest, &source).unwrap().replaced.clone().unwrap();
        assert_eq!((&on_record.trash, &on_record.from), (&trash, &output));
        let (mut d, other) = (replaced(&source, &dest, &trash, &output), s.0.join("stolen.mkv"));
        assert_eq!(ledger.check_replaced(&d), Ok(()));
        // The trash entry and the way back are the record's, whatever a report says.
        d.recorded_from = other.clone();
        assert_eq!(ledger.check_replaced(&d), Err(DISAGREES.to_string()));
        let mut d = replaced(&source, &dest, &other, &output);
        assert_eq!(ledger.check_replaced(&d), Err(DISAGREES.to_string()));
        d.trash = trash.clone();
        d.source = other;
        assert_eq!(ledger.check_replaced(&d), Err(NOT_RECORDED.to_string()));
        // So is what the file in the trash has to be.
        let mut d = replaced(&source, &dest, &trash, &output);
        d.original.whole = Some("of the file put there instead".into());
        assert_eq!(ledger.check_replaced(&d), Err(DISAGREES.to_string()));
        note_undone(&source, &dest, &output).unwrap();
        let ledger = load();
        assert_eq!(ledger.entry(&output, &source).unwrap().replaced, None);
        assert!(ledger.entry(&dest, &source).is_none());
        assert!(ledger.entry(&output, &source).unwrap().made.is_some(), "what was known of the encode is kept");
    }

    #[test]
    fn a_replacement_from_any_report_leaves_what_is_on_record_of_others_alone() {
        let s = Scratch::new("other-pair");
        let (source, output) = s.encoded("a");
        let (victim, placed) = (s.0.join("victim.mp4"), s.0.join("victim.mkv"));
        moved(&victim, &s.0.join("Trash/files/victim.mp4"), &placed, &output).unwrap();
        let ledger = load();
        assert!(ledger.entry(&output, &source).is_some_and(|e| e.made.is_some()));
        assert!(ledger.entry(&placed, &victim).unwrap().made.is_none());
        // Put back again, that pair was never seen made, so the page has nothing to go by.
        note_undone(&victim, &placed, &s.0.join("back.mkv")).unwrap();
        assert!(load().entry(&s.0.join("back.mkv"), &victim).is_none());
    }

    #[test]
    fn only_the_files_that_were_made_pass() {
        let s = Scratch::new("ready");
        let (source, output) = s.encoded("a");
        let plan = s.plan(&source, &output);
        let open = |p: &Path| OpenFile::open(p, 1000).unwrap();
        assert_eq!(load().check_ready(&plan, Some(&open(&output))), Ok(()));
        // Another original under the same output is not what was encoded.
        let victim = s.0.join("victim.mp4");
        std::fs::write(&victim, vec![b'v'; 3000]).unwrap();
        assert_eq!(load().check_ready(&s.plan(&victim, &output), Some(&open(&output))), Err(NOT_RECORDED.to_string()));
        // A report that describes the original otherwise has had it compared with that instead.
        for change in [
            (|k: &mut Recorded| k.whole = Some("of what is there now".into())) as fn(&mut Recorded),
            |k| k.whole = None,
            |k| k.fingerprint = None,
            |k| k.modified = Some(1),
        ] {
            let mut forged = s.plan(&source, &output);
            change(&mut forged.original);
            assert_eq!(load().check_ready(&forged, None), Err(DISAGREES.to_string()));
        }
        // An output of the right size with other contents is only caught by reading it.
        std::fs::write(&output, vec![b'x'; 1000]).unwrap();
        assert_eq!(load().check_ready(&plan, None), Ok(()));
        assert!(load().check_ready(&plan, Some(&open(&output))).unwrap_err().contains("contents differ"));
    }

    #[test]
    fn an_output_whose_contents_could_not_be_recorded_stays_where_it_is() {
        let s = Scratch::new("no-hash");
        let (source, output) = (s.0.join("a.mp4"), s.0.join("a.mkv"));
        std::fs::write(&output, vec![b'o'; 1000]).unwrap();
        note_encoded(&Encode { output: &output, report: &output.with_extension("av1opt.json"), source: &source, source_bytes: 3000, original: &known(), output_bytes: 1000, output_sha256: None }).unwrap();
        assert!(load().check_ready(&s.plan(&source, &output), Some(&OpenFile::open(&output, 1000).unwrap())).unwrap_err().contains("could not record"));
    }

    #[test]
    fn a_newer_encode_at_the_same_place_is_the_one_on_record() {
        let s = Scratch::new("newer");
        let (source, output) = s.encoded("a");
        let other = s.0.join("other.mp4");
        std::fs::write(&other, vec![b's'; 500]).unwrap();
        note_encoded(&Encode { output: &output, report: &output.with_extension("av1opt.json"), source: &other, source_bytes: 500, original: &Recorded::default(), output_bytes: 1000, output_sha256: None }).unwrap();
        assert!(load().entry(&output, &source).is_none());
        assert!(load().entry(&output, &other).is_some());
    }

    #[test]
    fn only_files_on_record_that_are_plain_files_are_covered() {
        let s = Scratch::new("covers");
        let (source, output) = s.encoded("a");
        assert!(load().covers(&output) && load().covers(&source));
        assert!(!load().covers(&s.0.join("other.mkv")));
        // Swapped for a link, the name on record no longer stands for a file to serve.
        std::fs::remove_file(&output).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", &output).unwrap();
        assert!(!load().covers(&output));
    }

    #[test]
    fn the_copy_held_for_the_page_follows_new_lines() {
        let s = Scratch::new("shared");
        let shared = Shared::default();
        let (source, output) = s.encoded("a");
        assert!(shared.current().entry(&output, &source).is_some());
        let (other, other_out) = s.encoded("b");
        assert!(shared.current().entry(&other_out, &other).is_some());
    }

    #[test]
    fn the_hash_is_the_standard_one_at_every_length() {
        // By hand, and by the processor where this one can.
        let ways: [Compression; 2] = [compress_by_hand, fastest_compression()];
        for way in ways {
            let of = |bytes: &[u8], step: usize| {
                let mut hash = Sha256::using(way);
                bytes.chunks(step).for_each(|c| hash.add(c));
                hash.finish()
            };
            assert_eq!(Sha256::using(way).finish(), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
            assert_eq!(of(b"abc", 1), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
            let long = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
            assert_eq!(of(long, 7), "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1");
            let million = vec![b'a'; 1_000_000];
            assert_eq!(of(&million, 1000), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
            assert_eq!(of(&million, 999_999), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
        }
    }

    #[test]
    fn the_history_is_looked_at_once_and_gives_nothing_to_trust() {
        let s = Scratch::new("takeover");
        let told = start_page_with(&|| 2).unwrap();
        assert!(told.contains("0 of 2 past encodes"), "{told}");
        assert!(load().taken_over);
        assert_eq!(load().outputs.len(), 0);
        assert_eq!(start_page_with(&|| 2), None, "said a second time");
        assert_eq!(mode(&s.data()), 0o700);
        // With no history there is nothing to say, the first time either.
        std::fs::remove_file(s.file()).unwrap();
        assert_eq!(start_page_with(&|| 0), None);
        assert!(load().taken_over);
    }
}
