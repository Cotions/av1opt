//! Black-box tests of `av1opt ui`: the real binary, started the way the desktop app starts it,
//! talked to over HTTP. They need no ffmpeg: the tools are pointed at `false`, so jobs are
//! accepted into the queue but never encode.
#![cfg(feature = "ui")]

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const TOKEN: &str = "testtoken0123456789abcdef";

struct Server {
    child: Child,
    port: u16,
    dir: PathBuf,
}

struct Resp {
    status: u16,
    headers: String,
    body: String,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.body))
    }
}

impl Server {
    /// Starts a server with its own data and cache folders; `prepare` may put files there first.
    fn start(name: &str, prepare: impl FnOnce(&Path)) -> Server {
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
            .env("AV1OPT_FFMPEG", "false")
            .env("AV1OPT_FFPROBE", "false")
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

    fn data(&self, name: &str) -> PathBuf {
        self.dir.join("data/av1opt").join(name)
    }

    fn raw(&self, request: &str) -> Resp {
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

    fn request(&self, method: &str, path: &str, extra: &[(&str, &str)], body: Option<&str>) -> Resp {
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

    fn cookie(&self) -> String {
        format!("av1opt_{}={TOKEN}", self.port)
    }

    fn get(&self, path: &str) -> Resp {
        self.request("GET", path, &[("Cookie", &self.cookie())], None)
    }

    fn post(&self, path: &str, body: &Value) -> Resp {
        let b = body.to_string();
        self.request("POST", path, &[("Cookie", &self.cookie()), ("Content-Type", "application/json")], Some(&b))
    }

    /// SIGTERM, as the desktop app does when it quits; true if it exited within 10 s.
    fn terminate(&mut self) -> bool {
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

#[test]
fn sign_in_and_request_checks() {
    let s = Server::start("auth", |_| {});
    let p = s.port;

    // DNS rebinding: a foreign Host is refused before anything else.
    let r = s.raw(&format!("GET /api/state HTTP/1.1\r\nHost: evil.example:{p}\r\nCookie: {}\r\nConnection: close\r\n\r\n", s.cookie()));
    assert_eq!(r.status, 403);
    // A page on another origin cannot use the session either.
    let r = s.request("GET", "/api/state", &[("Cookie", &s.cookie()), ("Origin", "http://evil.example")], None);
    assert_eq!(r.status, 403);
    let r = s.request("GET", "/api/state", &[("Cookie", &s.cookie()), ("Origin", &format!("http://127.0.0.1:{p}"))], None);
    assert_eq!(r.status, 200);

    // No session.
    assert_eq!(s.request("GET", "/api/state", &[], None).status, 401);
    assert_eq!(s.request("GET", "/", &[("Cookie", &format!("av1opt_{p}=wrongtoken0123456789abc"))], None).status, 401);
    // Session cookie of another instance (other port) does not count.
    assert_eq!(s.request("GET", "/api/state", &[("Cookie", &format!("av1opt_1={TOKEN}"))], None).status, 401);

    // Sign-in link.
    assert_eq!(s.request("GET", "/?token=nottheright0123456789", &[], None).status, 403);
    let r = s.request("GET", &format!("/?token={TOKEN}"), &[], None);
    assert_eq!(r.status, 303);
    let h = r.headers.to_ascii_lowercase();
    assert!(h.contains(&format!("set-cookie: av1opt_{p}={}", TOKEN.to_ascii_lowercase())), "{}", r.headers);
    assert!(h.contains("httponly") && h.contains("samesite=strict"), "{}", r.headers);

    // Page and hardening headers.
    let r = s.get("/");
    assert_eq!(r.status, 200);
    assert!(r.body.contains("<!doctype html") || r.body.contains("<!DOCTYPE html"));
    let h = r.headers.to_ascii_lowercase();
    for want in ["x-content-type-options: nosniff", "x-frame-options: deny", "cache-control: no-store"] {
        assert!(h.contains(want), "missing {want}: {}", r.headers);
    }
    assert_eq!(s.get("/app.js").status, 200);
    assert_eq!(s.get("/style.css").status, 200);

    // POSTs must be JSON (a cross-site form cannot send that without a preflight).
    let r = s.request("POST", "/api/jobs/clear", &[("Cookie", &s.cookie()), ("Content-Type", "text/plain")], Some("{}"));
    assert_eq!(r.status, 415);
    let r = s.request("POST", "/api/jobs/clear", &[("Cookie", &s.cookie()), ("Content-Type", "application/json")], Some("{nope"));
    assert_eq!(r.status, 400);

    // Unknown routes.
    assert_eq!(s.get("/api/nothing").status, 404);
    assert_eq!(s.get("/nothing").status, 404);
}

#[test]
fn state_reports_versions_and_tools() {
    let s = Server::start("state", |_| {});
    let st = s.get("/api/state").json();
    assert_eq!(st["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(st["api"], 1, "API_VERSION changed: update desktop/main.js (API_VERSION) too");
    assert!(st["home"].is_string());
    // `false` is no ffmpeg; the page shows why encodes are off.
    assert!(st["tools"]["problem"].is_string(), "{}", st["tools"]);
}

#[test]
fn media_serves_only_picked_files() {
    let s = Server::start("media", |d| {
        std::fs::write(d.join("videos/a.mkv"), b"not really a video").unwrap();
        std::fs::write(d.join("videos/b.mkv"), b"x").unwrap();
        // A file in a job counts as picked (here a job saved by an earlier session).
        let saved = json!({ "format": 1, "jobs": [{ "inputs": [d.join("videos/a.mkv")], "roots": [], "out_dir": d.join("out"), "options": {} }] });
        std::fs::write(d.join("data/av1opt/ui-queue.json"), saved.to_string()).unwrap();
    });
    let a = s.dir.join("videos/a.mkv");
    let q = |p: &Path| format!("/media?path={}", p.display());
    assert_eq!(s.get("/media?path=/etc/passwd").status, 403);
    assert_eq!(s.get("/media").status, 400);
    let r = s.get(&q(&a));
    assert_eq!(r.status, 200);
    assert_eq!(r.body, "not really a video");
    let r = s.request("GET", &q(&a), &[("Cookie", &s.cookie()), ("Range", "bytes=4-9")], None);
    assert_eq!(r.status, 206);
    assert_eq!(r.body, "really");
    // The same file through `..` is still that file; a sibling that was never picked is not.
    assert_eq!(s.get(&q(&s.dir.join("videos/../videos/a.mkv"))).status, 200);
    assert_eq!(s.get(&q(&s.dir.join("videos/b.mkv"))).status, 403);
}

#[test]
fn jobs_wait_while_tools_are_missing_and_survive_a_restart() {
    let mut s = Server::start("jobs", |d| {
        std::fs::write(d.join("videos/a.mkv"), b"x").unwrap();
        std::fs::write(d.join("videos/b.mp4"), b"x").unwrap();
    });
    let out = s.dir.join("out");

    // Bad requests.
    let r = s.post("/api/jobs", &json!({ "inputs": [], "roots": [], "out_dir": out }));
    assert_eq!(r.status, 400);
    let r = s.post("/api/jobs", &json!({ "inputs": [s.dir.join("videos")], "roots": [], "out_dir": "relative/out" }));
    assert_eq!(r.status, 400);
    let r = s.post("/api/jobs", &json!({ "inputs": [s.dir.join("videos/none.mkv")], "roots": [], "out_dir": out }));
    assert_eq!(r.status, 400);

    // With no usable ffmpeg, `add` refuses (the page shows the tools banner instead).
    let r = s.post("/api/jobs", &json!({ "inputs": [s.dir.join("videos")], "roots": [], "out_dir": out }));
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.json()["error"].as_str().unwrap_or("").contains("Settings"), "{}", r.body);
    assert!(s.get("/api/jobs/heads").json()["jobs"].as_array().unwrap().is_empty());
    s.terminate();

    // A queue saved by an earlier session is restored even with broken tools, and held.
    let videos = s.dir.join("videos");
    let saved = json!({ "format": 1, "jobs": [{ "inputs": [videos], "roots": [], "out_dir": out, "options": {} }] });
    std::fs::write(s.data("ui-queue.json"), saved.to_string()).unwrap();
    let mut s2 = Server::start_in(&s.dir);
    let heads = s2.get("/api/jobs/heads").json();
    let jobs = heads["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 1, "{heads}");
    assert_eq!(jobs[0]["status"], "queued");
    assert_eq!(jobs[0]["total"], 2);
    let id = jobs[0]["id"].as_u64().unwrap();
    let full = s2.get(&format!("/api/jobs/{id}")).json();
    assert_eq!(full["files"].as_array().unwrap().len(), 2);
    let f = s2.get(&format!("/api/jobs/{id}/files/1")).json();
    assert_eq!(f["index"], 1);
    assert_eq!(s2.get(&format!("/api/jobs/{id}/files/9")).status, 404);
    assert_eq!(s2.get("/api/jobs/999").status, 404);

    // Still queued after a few seconds: the worker holds jobs while the tools are broken.
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(s2.get("/api/jobs/heads").json()["jobs"][0]["status"], "queued");

    // Stopping keeps the queue for the next start, in the versioned layout.
    assert!(s2.terminate(), "server did not stop on SIGTERM");
    let q: Value = serde_json::from_slice(&std::fs::read(s2.data("ui-queue.json")).unwrap()).unwrap();
    assert_eq!(q["format"], 1);
    assert_eq!(q["jobs"].as_array().unwrap().len(), 1);

    // Cancelling removes it from the saved queue.
    let s3 = Server::start_in(&s.dir);
    let id = s3.get("/api/jobs/heads").json()["jobs"][0]["id"].as_u64().unwrap();
    assert_eq!(s3.post(&format!("/api/jobs/{id}/cancel"), &json!({})).status, 200);
    assert_eq!(s3.get("/api/jobs/heads").json()["jobs"][0]["status"], "cancelled");
    let q: Value = serde_json::from_slice(&std::fs::read(s3.data("ui-queue.json")).unwrap()).unwrap();
    assert!(q["jobs"].as_array().unwrap().is_empty(), "{q}");
}

#[test]
fn saved_jobs_with_missing_files_are_kept() {
    let s = Server::start("missing", |d| {
        let saved = json!([{ "inputs": [d.join("videos/later.mkv")], "roots": [], "out_dir": d.join("out"), "options": {} }]);
        // The layout from before versioning: a bare array.
        std::fs::write(d.join("data/av1opt/ui-queue.json"), saved.to_string()).unwrap();
    });
    let list = s.get("/api/jobs").json();
    assert_eq!(list["unrestored"].as_array().unwrap().len(), 1, "{list}");
    let r = s.post("/api/jobs/unrestored/retry", &json!({})).json();
    assert_eq!(r["restored"], 0);
    assert_eq!(r["unrestored"].as_array().unwrap().len(), 1);
    // The file shows up (say, a drive was mounted): retry picks it up.
    std::fs::write(s.dir.join("videos/later.mkv"), b"x").unwrap();
    let r = s.post("/api/jobs/unrestored/retry", &json!({})).json();
    assert_eq!(r["restored"], 1, "{r}");
    assert!(r["unrestored"].as_array().unwrap().is_empty());
    assert_eq!(s.get("/api/jobs/heads").json()["jobs"].as_array().unwrap().len(), 1);
}

#[test]
fn files_from_a_newer_version_are_not_overwritten() {
    let s = Server::start("newer", |d| {
        std::fs::write(d.join("data/av1opt/ui.json"), r#"{"format":99,"history_dirs":[],"future_field":true}"#).unwrap();
        std::fs::write(d.join("data/av1opt/ui-queue.json"), r#"{"format":99,"jobs":[{"anything":1}]}"#).unwrap();
    });
    // Settings change in this session but the file stays as the newer version wrote it.
    assert_eq!(s.post("/api/settings/tools", &json!({ "ffmpeg": "/usr/bin/ffmpeg" })).status, 200);
    let ui = std::fs::read_to_string(s.data("ui.json")).unwrap();
    assert!(ui.contains("future_field") && !ui.contains("/usr/bin/ffmpeg"), "{ui}");
    // The queue is set aside, not restored and not overwritten.
    assert!(!s.data("ui-queue.json").exists() || std::fs::read_to_string(s.data("ui-queue.json")).unwrap().contains("\"format\": 1"));
    assert!(std::fs::read_to_string(s.data("ui-queue.format99.json")).unwrap().contains("anything"));
    assert!(s.get("/api/jobs/heads").json()["jobs"].as_array().unwrap().is_empty());
}

#[test]
fn settings_tools_validation() {
    let s = Server::start("tools", |_| {});
    assert_eq!(s.post("/api/settings/tools", &json!({ "ffmpeg": "relative/ffmpeg" })).status, 400);
    let r = s.post("/api/settings/tools", &json!({ "ffmpeg": "/nonexistent/ffmpeg", "ffprobe": "" }));
    assert_eq!(r.status, 200, "{}", r.body);
    let ui: Value = serde_json::from_slice(&std::fs::read(s.data("ui.json")).unwrap()).unwrap();
    assert_eq!(ui["format"], 1);
    assert_eq!(ui["ffmpeg"], "/nonexistent/ffmpeg");
    assert!(s.get("/api/state").json()["tools"]["problem"].is_string());
}

impl Server {
    /// Starts again on the same folders (a restart).
    fn start_in(dir: &Path) -> Server {
        let name = dir.file_name().unwrap().to_string_lossy().trim_start_matches("ui-").to_string();
        let keep = dir.with_extension("keep");
        let _ = std::fs::remove_dir_all(&keep);
        std::fs::rename(dir, &keep).unwrap();
        Server::start(&name, |d| {
            let _ = std::fs::remove_dir_all(d);
            std::fs::rename(&keep, d).unwrap();
        })
    }
}
