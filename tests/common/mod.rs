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

    /// As [`Server::start`], with the ffmpeg and ffprobe to use (`false` for none).
    pub fn start_with_tools(name: &str, ffmpeg: &str, ffprobe: &str, prepare: impl FnOnce(&Path)) -> Server {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("ui-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("data/av1opt")).unwrap();
        std::fs::create_dir_all(dir.join("cache")).unwrap();
        std::fs::create_dir_all(dir.join("videos")).unwrap();
        prepare(&dir);
        let mut child = Command::new(env!("CARGO_BIN_EXE_av1opt"))
            .args(["ui", "--port", "0", "--no-open"])
            .env("XDG_DATA_HOME", dir.join("data"))
            .env("XDG_CACHE_HOME", dir.join("cache"))
            .env("AV1OPT_HISTORY", dir.join("data/av1opt/history.jsonl"))
            .env("AV1OPT_FFMPEG", ffmpeg)
            .env("AV1OPT_FFPROBE", ffprobe)
            .env("AV1OPT_UI_TOKEN", TOKEN)
            .env_remove("AV1OPT_UI_DEV")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start av1opt ui");
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
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s.write_all(request.as_bytes()).unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).unwrap();
        let text = String::from_utf8_lossy(&buf).into_owned();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let status = head.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        Resp { status, headers: head.to_string(), body: body.to_string() }
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

    /// SIGTERM, as the desktop app does when it quits; true if it exited within 10 s.
    pub fn terminate(&mut self) -> bool {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(10) {
            if self.child.try_wait().unwrap().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
