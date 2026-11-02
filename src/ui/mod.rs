//! `av1opt ui`: a local web app on 127.0.0.1 for scanning, a live encode queue, a before/after
//! comparison player and the history of past encodes.
//!
//! The page is static files embedded in the binary; it talks to this server only through a JSON
//! API and one Server-Sent-Events stream, so a desktop wrapper (Electron-style) can load the same
//! URL later. Every request must carry the session token (as a cookie set on first visit), a
//! localhost Host header and, if present, a localhost Origin. Media are only served for files the
//! user picked (scan results, queued encodes, reports in history folders) and previews.

mod http;
mod jobs;
mod library;

use crate::tools::Tools;
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
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Settings {
    #[serde(default)]
    pub history_dirs: Vec<PathBuf>,
    #[serde(default)]
    pub out_dir: Option<PathBuf>,
    #[serde(default)]
    pub scan_paths: Vec<PathBuf>,
}

pub struct App {
    pub cfg: Config,
    pub token: String,
    pub port: u16,
    pub settings: Mutex<Settings>,
    /// Canonical paths the media endpoint may serve.
    pub allowed: Mutex<HashSet<PathBuf>>,
    /// Open event streams; dead ones are dropped on the next send.
    subscribers: Mutex<Vec<Sender<String>>>,
    pub jobs: jobs::Queue,
    pub scans: Mutex<Vec<library::Scan>>,
    pub preview_dir: PathBuf,
    pub preview_lock: Mutex<()>,
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

    pub fn is_allowed(&self, p: &Path) -> Option<PathBuf> {
        let c = std::fs::canonicalize(p).ok()?;
        if c.starts_with(&self.preview_dir) || self.allowed.lock().unwrap().contains(&c) { Some(c) } else { None }
    }

    pub fn tools(&self, workers: u8) -> Tools {
        Tools::new(self.cfg.ffmpeg.clone(), self.cfg.ffprobe.clone(), crate::memory_cap(&self.cfg.memory_limit, workers))
    }

    pub fn save_settings(&self) {
        let s = self.settings.lock().unwrap().clone();
        if let Some(p) = library::settings_path() {
            let _ = p.parent().map(std::fs::create_dir_all);
            let _ = std::fs::write(&p, serde_json::to_vec_pretty(&s).unwrap_or_default());
        }
    }
}

pub fn run(cfg: Config) -> Result<()> {
    // A wrapper app (Electron-style) can choose the token and load `/?token=...` itself.
    let token = match std::env::var("AV1OPT_UI_TOKEN") {
        Ok(t) if t.len() >= 16 && t.bytes().all(|b| b.is_ascii_alphanumeric()) => t,
        Ok(_) => anyhow::bail!("AV1OPT_UI_TOKEN must be at least 16 letters or digits"),
        Err(_) => random_hex(16)?,
    };
    let server = tiny_http::Server::http(("127.0.0.1", cfg.port)).map_err(|e| anyhow::anyhow!("cannot listen on 127.0.0.1:{}: {e}", cfg.port))?;
    let port = server.server_addr().to_ip().context("not an IP listener")?.port();
    let preview_dir = library::preview_dir()?;
    let settings: Settings = library::settings_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let open = cfg.open;
    let app = Arc::new(App {
        cfg,
        token,
        port,
        settings: Mutex::new(settings),
        allowed: Mutex::new(HashSet::new()),
        subscribers: Mutex::new(Vec::new()),
        jobs: jobs::Queue::default(),
        scans: Mutex::new(Vec::new()),
        preview_dir,
        preview_lock: Mutex::new(()),
    });
    jobs::start_worker(app.clone());

    let url = format!("http://127.0.0.1:{port}/?token={}", app.token);
    eprintln!("av1opt ui on http://127.0.0.1:{port}/ (local only). Open this link to sign in:\n  {url}");
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
