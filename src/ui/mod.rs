//! `av1opt ui`: a local web app on 127.0.0.1 for scanning, finding duplicates, a live encode
//! queue, a before/after comparison player, the history of past encodes and a benchmark of
//! encoder settings.
//!
//! The page is static files embedded in the binary; it talks to this server only through a JSON
//! API and one Server-Sent-Events stream, so a desktop wrapper (Electron-style) can load the same
//! URL later. Every request must carry the session token (as a cookie set on first visit), a
//! localhost Host header and, if present, a localhost Origin. Media are only served for files the
//! user picked (scan and duplicate results, queued encodes, reports in history folders) and
//! previews.

mod bench;
mod dups;
mod hold;
mod http;
mod jobs;
mod library;
mod watch;

use crate::tools::{Capabilities, Tools};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

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
    pub preview_lock: Mutex<()>,
    /// Shared by preview transcodes so shutdown can stop them.
    pub preview_hooks: Arc<crate::events::Hooks>,
    /// Capabilities of the current ffmpeg/ffprobe pair, keyed by their paths.
    caps: Mutex<Option<(String, String, Capabilities)>>,
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
        if c.starts_with(&self.preview_dir) || self.allowed.lock().unwrap().contains(&c) { Some(c) } else { None }
    }

    pub fn tools(&self, workers: u8) -> Tools {
        Tools::new(self.ffmpeg(), self.ffprobe(), crate::memory_cap(&self.cfg.memory_limit, workers))
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
                note("settings not saved: ui.json is from a newer av1opt");
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
                note(&format!("settings not saved: cannot write {}: {e}", path.display()));
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
    let server = tiny_http::Server::http(("127.0.0.1", cfg.port)).map_err(|e| anyhow::anyhow!("cannot listen on 127.0.0.1:{}: {e}", cfg.port))?;
    let port = server.server_addr().to_ip().context("not an IP listener")?.port();
    let preview_dir = library::preview_dir()?;
    let mut settings: Settings = library::settings_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    // A program chosen on the page was safe to start when it was chosen; it has to be still.
    for (name, tool) in [("ffmpeg", &mut settings.ffmpeg), ("ffprobe", &mut settings.ffprobe)] {
        // What runs is the file found safe, not whatever the path leads to later.
        let Some(path) = tool.as_deref().map(PathBuf::from).filter(|p| p.is_absolute()) else { continue };
        match library::chosen_program(name, &path) {
            Ok(real) => *tool = Some(real.to_string_lossy().into_owned()),
            Err(why) => {
                note(&format!("{name}: {why}; using the default"));
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
        subscribers: Mutex::new(Vec::new()),
        jobs: jobs::Queue::default(),
        hold: hold::State::default(),
        watch: watch::State::default(),
        scans: Mutex::new(Vec::new()),
        dups: Mutex::new(Vec::new()),
        bench: Mutex::new(None),
        preview_dir,
        preview_lock: Mutex::new(()),
        preview_hooks: Arc::new(crate::events::Hooks::new(|_| {})),
        caps: Mutex::new(None),
    });
    if let Some(p) = app.encode_problem() {
        eprintln!("warning: {p}; encodes stay off until it is fixed (Settings in the page)");
    }
    stop_on_signal(app.clone());
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
            note("stopping");
            jobs::shutdown(&a);
            std::process::exit(0);
        });
    }
    if std::env::var_os("AV1OPT_UI_EXIT_WITH_STDIN").is_some_and(|v| v == "1") {
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
            note("parent went away, stopping");
            jobs::shutdown(&app);
            std::process::exit(0);
        });
    }
}

/// Like `eprintln!` but never panics: when a wrapper app dies, stderr is a broken pipe.
fn note(msg: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "{msg}");
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
