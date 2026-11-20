//! A running `av1opt ui` for tests: the real binary with its own data and cache folders,
//! talked to over HTTP.
#![allow(dead_code)]

use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const TOKEN: &str = "testtoken0123456789abcdef";

pub struct Server {
    pub child: Child,
    pub port: u16,
    pub dir: PathBuf,
}

pub struct Resp {
    pub status: u16,
    pub headers: String,
    pub body: String,
}

impl Resp {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.body))
    }
}

impl Server {
    /// Starts a server with its own data and cache folders; `prepare` may put files there first.
    pub fn start(name: &str, prepare: impl FnOnce(&Path)) -> Server {
        Server::start_with_tools(name, "false", "false", prepare)
    }

    /// As [`Server::start`], with the ffmpeg and ffprobe to use (`false` for none). Returns
    /// once a saved queue has come back, which goes on after the server starts answering.
    pub fn start_with_tools(name: &str, ffmpeg: &str, ffprobe: &str, prepare: impl FnOnce(&Path)) -> Server {
        let s = Server::launch(name, ffmpeg, ffprobe, prepare);
        s.wait_for_restored_jobs();
        s
    }

    /// Waits until no saved job is still on its way back into the queue. A long queue on a busy
    /// machine takes what it takes: only a minute without one more job back is a failure.
    pub fn wait_for_restored_jobs(&self) {
        let (mut left, mut moved, mut pause) = (u64::MAX, Instant::now(), Duration::from_millis(10));
        loop {
            let now = self.get("/api/jobs/heads").json()["restoring"].as_u64().unwrap_or(u64::MAX);
            if now == 0 {
                return;
            }
            if now < left {
                (left, moved) = (now, Instant::now());
            }
            assert!(moved.elapsed() < Duration::from_secs(60), "the saved queue has stopped coming back with {left} jobs to go");
            // Each look has the server list every job back so far.
            std::thread::sleep(pause);
            pause = (pause * 2).min(Duration::from_millis(500));
        }
    }

    /// The command that starts a server on the folders below `dir`.
    pub fn command(dir: &Path, ffmpeg: &str, ffprobe: &str) -> Command {
        // A home of its own: what the server offers by default (an output folder in the home
        // folder, the folders to browse) then lies in the test's folder, not the user's.
        std::fs::create_dir_all(dir.join("home")).unwrap();
        let mut c = Command::new(env!("CARGO_BIN_EXE_av1opt"));
        c.args(["ui", "--port", "0", "--no-open"])
            .env("HOME", dir.join("home"))
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("XDG_DATA_HOME", dir.join("data"))
            .env("XDG_CACHE_HOME", dir.join("cache"))
            .env("AV1OPT_HISTORY", dir.join("data/av1opt/history.jsonl"))
            .env("AV1OPT_FFMPEG", ffmpeg)
            .env("AV1OPT_FFPROBE", ffprobe)
            .env("AV1OPT_UI_TOKEN", TOKEN)
            .env_remove("AV1OPT_UI_DEV")
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        c
    }

    /// Starts a server and returns as soon as it says it is listening.
    pub fn launch(name: &str, ffmpeg: &str, ffprobe: &str, prepare: impl FnOnce(&Path)) -> Server {
        Server::launch_with(name, ffmpeg, ffprobe, &[], prepare)
    }

    /// Like [`Server::launch`], with `environment` set for the server.
    pub fn launch_with(name: &str, ffmpeg: &str, ffprobe: &str, environment: &[(&str, &str)], prepare: impl FnOnce(&Path)) -> Server {
        Server::launch_adjusted(name, ffmpeg, ffprobe, environment, |_| {}, prepare)
    }

    /// Like [`Server::launch_with`], with `adjust` given the command before it is started.
    pub fn launch_adjusted(name: &str, ffmpeg: &str, ffprobe: &str, environment: &[(&str, &str)], adjust: impl FnOnce(&mut Command), prepare: impl FnOnce(&Path)) -> Server {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("ui-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("data/av1opt")).unwrap();
        std::fs::create_dir_all(dir.join("cache")).unwrap();
        std::fs::create_dir_all(dir.join("videos")).unwrap();
        prepare(&dir);
        let mut command = Server::command(&dir, ffmpeg, ffprobe);
        command.envs(environment.iter().copied()).stderr(Stdio::piped());
        adjust(&mut command);
        let mut child = command.spawn().expect("start av1opt ui");
        let mut err = BufReader::new(child.stderr.take().unwrap());
        let mut port = None;
        let mut seen = String::new();
        let mut line = String::new();
        while port.is_none() && err.read_line(&mut line).unwrap_or(0) > 0 {
            assert!(!line.contains(TOKEN), "the token must not be printed when the caller set it: {line}");
            if let Some(rest) = line.strip_prefix("av1opt ui on http://127.0.0.1:") {
                port = rest.split('/').next().and_then(|p| p.parse().ok());
            }
            seen.push_str(&line);
            line.clear();
        }
        let port = port.unwrap_or_else(|| panic!("no listening line; output was:\n{seen}"));
        // Keep reading so the server never blocks on a full stderr pipe.
        std::thread::spawn(move || {
            let mut sink = String::new();
            let _ = err.read_to_string(&mut sink);
        });
        Server { child, port, dir }
    }

    /// Starts again on the same folders (a restart).
    pub fn start_in(dir: &Path, ffmpeg: &str, ffprobe: &str) -> Server {
        let name = dir.file_name().unwrap().to_string_lossy().trim_start_matches("ui-").to_string();
        let keep = dir.with_extension("keep");
        let _ = std::fs::remove_dir_all(&keep);
        std::fs::rename(dir, &keep).unwrap();
        Server::start_with_tools(&name, ffmpeg, ffprobe, |d| {
            let _ = std::fs::remove_dir_all(d);
            std::fs::rename(&keep, d).unwrap();
        })
    }

    pub fn data(&self, name: &str) -> PathBuf {
        self.dir.join("data/av1opt").join(name)
    }

    pub fn raw(&self, request: &str) -> Resp {
        self.try_raw(request).unwrap_or_else(|e| panic!("no answer from the server: {e}"))
    }

    /// [`Server::raw`] for a server that may be gone by the time it answers.
    pub fn try_raw(&self, request: &str) -> std::io::Result<Resp> {
        let mut s = TcpStream::connect(("127.0.0.1", self.port))?;
        // For a server that hangs: a busy machine can take a while over a long answer.
        s.set_read_timeout(Some(Duration::from_secs(120)))?;
        s.write_all(request.as_bytes())?;
        let mut buf = Vec::new();
        s.read_to_end(&mut buf)?;
        let end = buf.windows(4).position(|w| w == b"\r\n\r\n").map_or(buf.len(), |i| i + 4);
        let (head, body) = buf.split_at(end);
        let head = String::from_utf8_lossy(head).trim_end().to_string();
        let status = head.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        // A long answer comes in pieces.
        let chunked = head.to_ascii_lowercase().contains("transfer-encoding: chunked");
        let body = if chunked { unchunked(body) } else { body.to_vec() };
        Ok(Resp { status, headers: head, body: String::from_utf8_lossy(&body).into_owned() })
    }

    pub fn request(&self, method: &str, path: &str, extra: &[(&str, &str)], body: Option<&str>) -> Resp {
        let mut r = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n", self.port);
        for (k, v) in extra {
            r.push_str(&format!("{k}: {v}\r\n"));
        }
        if let Some(b) = body {
            r.push_str(&format!("Content-Length: {}\r\n\r\n{b}", b.len()));
        } else {
            r.push_str("\r\n");
        }
        self.raw(&r)
    }

    pub fn cookie(&self) -> String {
        format!("av1opt_{}={TOKEN}", self.port)
    }

    pub fn get(&self, path: &str) -> Resp {
        self.request("GET", path, &[("Cookie", &self.cookie())], None)
    }

    pub fn post(&self, path: &str, body: &Value) -> Resp {
        let b = body.to_string();
        self.request("POST", path, &[("Cookie", &self.cookie()), ("Content-Type", "application/json")], Some(&b))
    }

    /// SIGTERM, as the desktop app does when it quits; true if it exited within a minute, which
    /// only a server that hangs does not.
    pub fn terminate(&mut self) -> bool {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(60) {
            if self.child.try_wait().unwrap().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }
}

/// The body of an answer sent in chunks: each is its length in hexadecimal, a line end, the
/// bytes and another line end, and one of length zero ends them.
fn unchunked(mut rest: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    while let Some(line) = rest.windows(2).position(|w| w == b"\r\n") {
        let length = std::str::from_utf8(&rest[..line]).ok().and_then(|l| usize::from_str_radix(l.trim(), 16).ok()).unwrap_or(0);
        if length == 0 {
            break;
        }
        let start = line + 2;
        body.extend_from_slice(&rest[start..start + length]);
        rest = &rest[start + length + 2..];
    }
    body
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
