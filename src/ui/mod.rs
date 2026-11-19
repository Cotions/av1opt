//! `av1opt ui`: a local web app on 127.0.0.1 for scanning, finding duplicates, a live encode
//! queue, a before/after comparison player, the history of past encodes and a benchmark of
//! encoder settings.
//!
//! The page is static files embedded in the binary; it talks to this server only through a JSON
//! API and one Server-Sent-Events stream, so a desktop wrapper (Electron-style) can load the same
//! URL later. Every request must carry the session token (as a cookie set on first visit), a
//! localhost Host header and, if present, a localhost Origin. Media are only served for files the
//! user picked (scan and duplicate results, queued encodes), files in av1opt's own record of what
//! it made (see `ledger`) and previews. A report found in a folder adds nothing to these.

mod bench;
mod connections;
mod dups;
mod fields;
mod hold;
mod http;
mod jobs;
mod library;
mod turns;
mod watch;

use crate::tools::{Capabilities, Tools};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard};

pub struct Config {
    pub port: u16,
    pub open: bool,
    pub ffmpeg: String,
    pub ffprobe: String,
    pub memory_limit: String,
    pub jobs: usize,
}

/// Remembered between sessions in `$XDG_DATA_HOME/av1opt/ui.json`.
/// Version of the HTTP API, reported as `api` in `/api/state`. Bump it when a route or a
/// response changes incompatibly, so a wrapper app bundled with another av1opt can tell.
pub const API_VERSION: u32 = 1;

/// Version of `ui.json`. Bump it when a field changes meaning or is removed (adding
/// fields is fine). A file from a newer av1opt is used as far as it is understood but never
/// overwritten, so going back to an older version loses nothing.
pub const SETTINGS_FORMAT: u32 = 1;

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Settings {
    #[serde(default)]
    pub format: u32,
    #[serde(default)]
    pub history_dirs: Vec<PathBuf>,
    #[serde(default)]
    pub out_dir: Option<PathBuf>,
    #[serde(default)]
    pub scan_paths: Vec<PathBuf>,
    /// Folders, preferred folders and avoided path texts of the last duplicate search.
    #[serde(default)]
    pub dup_paths: Vec<PathBuf>,
    #[serde(default)]
    pub dup_prefer: Vec<PathBuf>,
    #[serde(default)]
    pub dup_avoid: Vec<String>,
    /// Folders and videos of the last benchmark.
    #[serde(default)]
    pub bench_paths: Vec<PathBuf>,
    /// Chosen in the page; wins over --ffmpeg / AV1OPT_FFMPEG. A desktop launcher has no shell
    /// environment, so this is how the desktop app finds an ffmpeg with libvmaf.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ffmpeg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ffprobe: Option<String>,
    /// The queue's pause switch: a paused queue stays paused when the app is started again.
    #[serde(default)]
    pub queue_paused: bool,
    /// Hours of the day encodes may run in.
    #[serde(default)]
    pub schedule: hold::Schedule,
    /// Folders whose new videos are queued by themselves.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watches: Vec<watch::Watch>,
}

pub struct App {
    pub cfg: Config,
    pub token: String,
    pub port: u16,
    pub settings: Mutex<Settings>,
    /// Held from taking the settings to having written them, so that of two saves the later
    /// one is what stays on disk.
    settings_save: Mutex<()>,
    /// Canonical paths the media endpoint may serve.
    pub allowed: Mutex<HashSet<PathBuf>>,
    /// What av1opt itself recorded making and moving.
    pub ledger: crate::ledger::Shared,
    /// Open event streams; dead ones are dropped on the next send.
    subscribers: Mutex<Vec<Sender<String>>>,
    pub jobs: jobs::Queue,
    pub hold: hold::State,
    pub watch: watch::State,
    pub scans: Mutex<Vec<library::Scan>>,
    pub dups: Mutex<Vec<dups::Run>>,
    /// The benchmark of this session, running or finished.
    pub bench: Mutex<Option<bench::Run>>,
    pub preview_dir: PathBuf,
    /// Shared by preview transcodes so shutdown can stop them.
    pub preview_hooks: Arc<crate::events::Hooks>,
    /// Capabilities of the current ffmpeg/ffprobe pair, keyed by their paths.
    caps: Mutex<Option<(String, String, Capabilities)>>,
    bench_saves: Saves,
    /// What went wrong at start that the user should hear of, for the page.
    pub notices: Mutex<Vec<String>>,
    /// Held as long as the server runs: see [`one_server_per_user`].
    _only_server: Option<std::fs::File>,
}

/// `p`, if the page can be told it. What goes to the page is JSON, which holds text and has no
/// way to write other bytes: a request can only arrive at such a path through a link, and
/// answering it would end the thread that answers, with whatever lock it holds.
pub fn text_path(p: PathBuf) -> anyhow::Result<PathBuf> {
    match p.to_str() {
        Some(_) => Ok(p),
        None => anyhow::bail!("{}: {}", p.display(), crate::tools::NOT_TEXT),
    }
}

impl App {
    /// Sends one message to every open event stream.
    pub fn broadcast(&self, msg: Value) {
        let s = msg.to_string();
        self.subscribers.lock().unwrap().retain(|tx| tx.send(s.clone()).is_ok());
    }

    pub fn allow(&self, p: &Path) {
        if let Ok(c) = std::fs::canonicalize(p) {
            self.allowed.lock().unwrap().insert(c);
        }
    }

    /// As [`App::allow`], for a path that only says it is a video: the file it leads to in
    /// the end has to be one by its name too. (A job can name any file, and a link named
    /// like a video can lead to any; neither should make the file readable through the page.)
    pub fn allow_video(&self, p: &Path) {
        if let Ok(c) = std::fs::canonicalize(p)
            && c.is_file()
            && crate::scan::is_video(&c)
        {
            self.allowed.lock().unwrap().insert(c);
        }
    }

    pub fn is_allowed(&self, p: &Path) -> Option<PathBuf> {
        let c = std::fs::canonicalize(p).ok()?;
        let listed = c.starts_with(&self.preview_dir) || self.allowed.lock().unwrap().contains(&c);
        (listed || self.ledger.current().covers(&c)).then_some(c)
    }

    pub fn tools(&self, workers: u8) -> Tools {
        Tools::new(self.ffmpeg(), self.ffprobe(), crate::memory_cap(&self.cfg.memory_limit, workers))
    }

    /// The tools of an encode: `workers` is how many of its chunks may run at the same time.
    pub fn encode_tools(&self, workers: u8) -> Tools {
        self.tools(1).sharing_between_chunks(crate::memory_cap(&self.cfg.memory_limit, workers))
    }

    pub fn ffmpeg(&self) -> String {
        self.settings.lock().unwrap().ffmpeg.clone().unwrap_or_else(|| self.cfg.ffmpeg.clone())
    }

    pub fn ffprobe(&self) -> String {
        self.settings.lock().unwrap().ffprobe.clone().unwrap_or_else(|| self.cfg.ffprobe.clone())
    }

    pub fn capabilities(&self) -> Capabilities {
        let (ffmpeg, ffprobe) = (self.ffmpeg(), self.ffprobe());
        if let Some((a, b, caps)) = &*self.caps.lock().unwrap()
            && *a == ffmpeg
            && *b == ffprobe
        {
            return caps.clone();
        }
        // Probed without the lock held: it runs ffmpeg.
        let caps = Capabilities::probe(&ffmpeg, &ffprobe);
        *self.caps.lock().unwrap() = Some((ffmpeg, ffprobe, caps.clone()));
        caps
    }

    /// Forgets the probe result, e.g. after the user installed a new ffmpeg at the same path.
    pub fn recheck_tools(&self) {
        *self.caps.lock().unwrap() = None;
    }

    /// Why encodes cannot run with the current tools, if they cannot.
    pub fn encode_problem(&self) -> Option<String> {
        self.capabilities().problem(&self.ffmpeg(), &self.ffprobe())
    }

    pub fn save_settings(&self) {
        one_at_a_time(&self.settings_save, || self.settings.lock().unwrap().clone(), |mut s| {
            if s.format > SETTINGS_FORMAT {
                eprintln!("settings not saved: ui.json is from a newer av1opt");
                return;
            }
            s.format = SETTINGS_FORMAT;
            let Some(path) = library::settings_path() else { return };
            // A settings file kept elsewhere (a folder of dotfiles) stays a link to it.
            let path = behind_links(&path);
            let saved = path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| crate::files::replace_with(&path, &serde_json::to_vec_pretty(&s).unwrap_or_default()));
            if let Err(e) = saved {
                eprintln!("settings not saved: cannot write {}: {e}", path.display());
            }
        });
    }
}

/// Takes what is to be saved and writes it with `gate` held throughout: a save that took its
/// copy earlier cannot land after one that took a later copy, and undo it.
fn one_at_a_time<T>(gate: &Mutex<()>, take: impl FnOnce() -> T, write: impl FnOnce(T)) {
    let _saving = gate.lock().unwrap_or_else(|e| e.into_inner());
    write(take());
}

/// Saves of one file: one at a time, each by way of a file of its own, and a failure said once
/// rather than after every change while the disk stays full.
#[derive(Default)]
pub struct Saves {
    gate: Mutex<()>,
    failing: AtomicBool,
}

impl Saves {
    /// Writes what `take` returns to `path`, if it returns anything. `take` runs with the other
    /// saves held off, so that the last to take its copy is the last to write.
    pub fn write(&self, what: &str, path: &Path, take: impl FnOnce() -> Option<Vec<u8>>) {
        one_at_a_time(&self.gate, take, |data| {
            let Some(data) = data else { return };
            let saved = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| crate::files::replace_with(path, &data));
            if let Some(message) = self.failure_to_tell(what, path, saved) {
                eprintln!("{message}");
            }
        });
    }

    /// Holds saves off while the things they read are being changed together.
    pub fn hold(&self) -> MutexGuard<'_, ()> {
        self.gate.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn failure_to_tell(&self, what: &str, path: &Path, saved: std::io::Result<()>) -> Option<String> {
        match saved {
            Ok(()) => {
                self.failing.store(false, Ordering::SeqCst);
                None
            }
            Err(e) => (!self.failing.swap(true, Ordering::SeqCst)).then(|| format!("warning: {what} could not be saved to {}: {e}; it is kept in memory only until saving works again", path.display())),
        }
    }
}

/// Moves a file that cannot be understood out of the way, so that the next save does not
/// replace it, and says so. Returns what was said, for the page. The new name has the time in
/// it, and a file kept elsewhere through a link is moved there and the link left.
pub fn set_aside(path: &Path, what: &str, why: &str, consequence: &str) -> String {
    let real = behind_links(path);
    let told = match move_aside(&real) {
        Ok(kept) => format!("{what} could not be read ({why}); it was kept as {} and {consequence}", kept.display()),
        Err(e) => format!("{what} could not be read ({why}) and could not be moved aside ({e}); {consequence}, and the next save may replace it"),
    };
    eprintln!("warning: {told}");
    told
}

fn move_aside(path: &Path) -> std::io::Result<PathBuf> {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
    let extension = path.extension().map_or(String::new(), |e| format!(".{}", e.to_string_lossy()));
    let stamp = local_stamp();
    let mut result = Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
    for attempt in 0..100 {
        let number = if attempt == 0 { String::new() } else { format!("-{attempt}") };
        let aside = path.with_file_name(format!("{stem}.unreadable-{stamp}{number}{extension}"));
        result = crate::trash::rename_new(path, &aside).map(|()| aside);
        if !result.as_ref().is_err_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists) {
            break;
        }
    }
    result
}

/// The present local time as `20261118-213005`.
fn local_stamp() -> String {
    // SAFETY: `time` and `localtime_r` only write to the struct given to them.
    let tm = unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    };
    format!("{:04}{:02}{:02}-{:02}{:02}{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// Keeps a second server off this user's files: two would restore the same jobs, encode them
/// twice and overwrite each other's queue. The system lets go of the lock when the process
/// ends, however it ends.
fn one_server_per_user() -> Result<Option<std::fs::File>> {
    use std::io::{Read, Seek, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let Some(path) = library::settings_path().map(|p| p.with_file_name("ui.lock")) else { return Ok(None) };
    let opened = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| {
        std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(&path)
    });
    let mut file = match opened {
        Ok(f) => f,
        Err(e) => {
            eprintln!("warning: cannot open {}: {e}; nothing keeps a second av1opt page off the same files", path.display());
            return Ok(None);
        }
    };
    // SAFETY: flock(2) on a descriptor this function owns.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EWOULDBLOCK) {
            // Some network file systems have no such locks; that is no reason not to start.
            eprintln!("warning: cannot lock {}: {e}; nothing keeps a second av1opt page off the same files", path.display());
            return Ok(Some(file));
        }
        let mut holder = String::new();
        let _ = file.read_to_string(&mut holder);
        let holder = holder.trim().parse::<u32>().map_or(String::new(), |pid| format!(" (process {pid})"));
        anyhow::bail!("av1opt's page is already running for this user{holder}; use that one, or stop it before starting another");
    }
    // Only to be named in the message above: the lock is what counts.
    let _ = file.set_len(0).and_then(|()| file.rewind()).and_then(|()| write!(file, "{}", std::process::id()));
    Ok(Some(file))
}

/// The saved settings. A file that cannot be understood is moved aside and the queue starts
/// paused: its pause switch, hours and watched folders are gone with it, and nothing should
/// start running because of that.
fn read_settings(notices: &mut Vec<String>) -> Settings {
    let Some(path) = library::settings_path() else { return Settings::default() };
    let why = match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(settings) => return settings,
            Err(e) => e.to_string(),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Settings::default(),
        Err(e) => e.to_string(),
    };
    notices.push(set_aside(&path, "the settings file", &why, "settings and watched folders are back to their defaults; the queue starts paused, so resume it when you are ready"));
    Settings { queue_paused: true, ..Settings::default() }
}

/// The file a chain of links at `path` ends at, there or not. A chain that never ends leaves
/// `path` itself.
fn behind_links(path: &Path) -> PathBuf {
    let mut at = path.to_path_buf();
    for _ in 0..40 {
        match std::fs::read_link(&at) {
            Ok(next) => at = at.parent().unwrap_or(Path::new("")).join(next),
            Err(_) => return at,
        }
    }
    path.to_path_buf()
}

pub fn run(cfg: Config) -> Result<()> {
    // A wrapper app (Electron-style) can choose the token and load `/?token=...` itself.
    let (token, token_given) = match std::env::var("AV1OPT_UI_TOKEN") {
        Ok(t) if t.len() >= 16 && t.bytes().all(|b| b.is_ascii_alphanumeric()) => (t, true),
        Ok(_) => anyhow::bail!("AV1OPT_UI_TOKEN must be at least 16 letters or digits"),
        Err(_) => (random_hex(16)?, false),
    };
    // The programs started from here (ffmpeg, the browser) have no use for it.
    // SAFETY: the environment is changed before any other thread is started.
    unsafe { std::env::remove_var("AV1OPT_UI_TOKEN") };
    // Before anything is read, wiped or restored.
    library::close_private_folders();
    let only_server = one_server_per_user()?;
    let server = tiny_http::Server::http(("127.0.0.1", cfg.port)).map_err(|e| anyhow::anyhow!("cannot listen on 127.0.0.1:{}: {e}", cfg.port))?;
    let port = server.server_addr().to_ip().context("not an IP listener")?.port();
    let preview_dir = library::preview_dir()?;
    let mut notices = Vec::new();
    let mut settings = read_settings(&mut notices);
    let settings_were_lost = !notices.is_empty();
    if let Some(told) = crate::ledger::start_page() {
        eprintln!("{told}");
        notices.push(told);
    }
    // A program chosen on the page was safe to start when it was chosen; it has to be still.
    for (name, tool) in [("ffmpeg", &mut settings.ffmpeg), ("ffprobe", &mut settings.ffprobe)] {
        // What runs is the file found safe, not whatever the path leads to later.
        let Some(path) = tool.as_deref().map(PathBuf::from).filter(|p| p.is_absolute()) else { continue };
        match library::chosen_program(name, &path) {
            Ok(real) => *tool = Some(real.to_string_lossy().into_owned()),
            Err(why) => {
                eprintln!("{name}: {why}; using the default");
                *tool = None;
            }
        }
    }
    let open = cfg.open;
    let app = Arc::new(App {
        cfg,
        token,
        port,
        settings: Mutex::new(settings),
        settings_save: Mutex::new(()),
        allowed: Mutex::new(HashSet::new()),
        ledger: crate::ledger::Shared::default(),
        subscribers: Mutex::new(Vec::new()),
        jobs: jobs::Queue::default(),
        hold: hold::State::default(),
        watch: watch::State::default(),
        scans: Mutex::new(Vec::new()),
        dups: Mutex::new(Vec::new()),
        bench: Mutex::new(None),
        preview_dir,
        preview_hooks: Arc::new(crate::events::Hooks::new(|_| {})),
        caps: Mutex::new(None),
        bench_saves: Saves::default(),
        notices: Mutex::new(notices),
        _only_server: only_server,
    });
    // So that the pause they came up with is still there after a restart.
    if settings_were_lost {
        app.save_settings();
    }
    if let Some(p) = app.encode_problem() {
        eprintln!("warning: {p}; encodes stay off until it is fixed (Settings in the page)");
    }
    stop_on_signal(app.clone());
    // Goes on in the background: the listening line below must not wait for a long queue.
    jobs::restore(&app);
    jobs::start_worker(app.clone());
    hold::start_clock(app.clone());
    watch::start(app.clone());

    let url = format!("http://127.0.0.1:{port}/?token={}", app.token);
    // A caller that chose the token knows it; keep it out of logs (a desktop launcher's output
    // may end up in the journal).
    if token_given {
        eprintln!("av1opt ui on http://127.0.0.1:{port}/ (local only; sign in with the token you set)");
    } else {
        eprintln!("av1opt ui on http://127.0.0.1:{port}/ (local only). Open this link to sign in:\n  {url}");
    }
    eprintln!("press Ctrl-C to stop (a running encode resumes on the next run)");
    if open {
        open_browser(&url);
    }
    connections::start(port);
    for req in server.incoming_requests() {
        let app = app.clone();
        std::thread::spawn(move || http::handle(&app, req));
    }
    Ok(())
}

static SIGNAL_PIPE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

extern "C" fn on_signal(_: libc::c_int) {
    let fd = SIGNAL_PIPE.load(std::sync::atomic::Ordering::Relaxed);
    // SAFETY: write(2) is async-signal-safe; the pipe stays open for the life of the process.
    unsafe { libc::write(fd, b"x".as_ptr().cast(), 1) };
}

fn stop(app: &App) -> ! {
    jobs::shutdown(app);
    let _ = std::fs::remove_dir_all(&app.preview_dir);
    std::process::exit(0);
}

/// On SIGINT, SIGTERM or SIGHUP (and, for a wrapper app, when stdin closes with
/// `AV1OPT_UI_EXIT_WITH_STDIN=1`), stops the encodes cleanly and exits.
fn stop_on_signal(app: Arc<App>) {
    let mut fds = [0; 2];
    // SAFETY: plain pipe(2) and sigaction(2) calls; the handler only writes one byte.
    let ok = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } == 0;
    if ok {
        SIGNAL_PIPE.store(fds[1], std::sync::atomic::Ordering::Relaxed);
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            unsafe {
                // Keep signals the parent chose to ignore ignored (`nohup`, background jobs).
                let mut old: libc::sigaction = std::mem::zeroed();
                libc::sigaction(sig, std::ptr::null(), &mut old);
                if old.sa_sigaction == libc::SIG_IGN {
                    continue;
                }
                let mut sa: libc::sigaction = std::mem::zeroed();
                sa.sa_sigaction = on_signal as *const () as usize;
                sa.sa_flags = libc::SA_RESTART;
                libc::sigaction(sig, &sa, std::ptr::null_mut());
            }
        }
        let a = app.clone();
        std::thread::spawn(move || {
            let mut b = [0u8; 1];
            // SAFETY: read(2) on our own pipe.
            while unsafe { libc::read(fds[0], b.as_mut_ptr().cast(), 1) } < 0 {}
            eprintln!("stopping");
            stop(&a);
        });
    }
    if std::env::var_os("AV1OPT_UI_EXIT_WITH_STDIN").is_some_and(|v| v == "1") {
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
            eprintln!("parent went away, stopping");
            stop(&app);
        });
    }
}

/// Hands the sign-in link to the desktop's default browser through a private file, so the
/// token does not show up in other users' process lists.
fn open_browser(url: &str) {
    use std::os::unix::fs::OpenOptionsExt;
    let Some(dir) = library::cache_dir() else { return };
    let page = dir.join("ui-open.html");
    let html = format!("<!doctype html><meta http-equiv=\"refresh\" content=\"0;url={url}\"><title>av1opt</title>");
    let _ = std::fs::remove_file(&page);
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&page)
        .and_then(|mut f| std::io::Write::write_all(&mut f, html.as_bytes()));
    if written.is_err() {
        return;
    }
    let spawned = std::process::Command::new("xdg-open")
        .arg(&page)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if spawned.is_err() {
        eprintln!("could not start a browser; open the link above yourself");
    }
    // The browser reads the page right away; don't leave the token lying around.
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(20));
        let _ = std::fs::remove_file(page);
    });
}

fn random_hex(n: usize) -> Result<String> {
    let mut b = vec![0u8; n];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut b)?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Error reply for the API: `{ "error": "..." }`.
pub fn err_json(e: &anyhow::Error) -> Value {
    json!({ "error": format!("{e:#}") })
}

#[cfg(test)]
mod save_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_failing_save_is_said_once_until_saving_works_again() {
        let saves = Saves::default();
        let path = Path::new("/nowhere/ui-queue.json");
        let failure = || Err(std::io::Error::from(std::io::ErrorKind::StorageFull));
        let told = saves.failure_to_tell("the queue", path, failure()).unwrap();
        assert!(told.contains("the queue could not be saved to /nowhere/ui-queue.json"), "{told}");
        assert_eq!(saves.failure_to_tell("the queue", path, failure()), None);
        assert_eq!(saves.failure_to_tell("the queue", path, Ok(())), None);
        assert!(saves.failure_to_tell("the queue", path, failure()).is_some());
    }

    #[test]
    fn a_file_that_cannot_be_read_is_kept_under_a_name_with_the_time() {
        let dir = std::env::temp_dir().join(format!("av1opt-aside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ui.json");
        let mut kept = Vec::new();
        // Twice within a second: the second does not take the place of the first.
        for text in ["first", "second"] {
            std::fs::write(&path, text).unwrap();
            let told = set_aside(&path, "the settings file", "not JSON", "it starts over");
            assert!(told.contains("the settings file could not be read (not JSON)"), "{told}");
            assert!(!path.exists());
            kept.push(std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()).find(|p| std::fs::read_to_string(p).unwrap() == text).unwrap());
        }
        assert_ne!(kept[0], kept[1]);
        let name = kept[0].file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("ui.unreadable-20") && name.ends_with(".json"), "{name}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_slow_save_does_not_undo_a_later_one() {
        let (gate, current, on_disk) = (Mutex::new(()), Mutex::new(1), Mutex::new(0));
        std::thread::scope(|s| {
            s.spawn(|| {
                one_at_a_time(&gate, || *current.lock().unwrap(), |value| {
                    // The change that follows is made while this copy is on its way.
                    *current.lock().unwrap() = 2;
                    std::thread::sleep(Duration::from_millis(150));
                    *on_disk.lock().unwrap() = value;
                });
            });
            s.spawn(|| {
                while *current.lock().unwrap() != 2 {
                    std::thread::yield_now();
                }
                one_at_a_time(&gate, || *current.lock().unwrap(), |value| *on_disk.lock().unwrap() = value);
            });
        });
        assert_eq!(*on_disk.lock().unwrap(), 2);
    }
}
