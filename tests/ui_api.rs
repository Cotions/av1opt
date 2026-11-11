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

#[test]
fn metadata_presets_and_options() {
    let s = Server::start("meta", |d| {
        std::fs::write(d.join("videos/a.mkv"), b"x").unwrap();
        std::fs::write(d.join("videos/notes.txt"), b"x").unwrap();
        let options = json!({ "keep_metadata": false, "keep_dates": true, "meta_preset": "youtube", "meta_tags": [["title", "{title|file}"]] });
        let saved = json!({ "format": 1, "jobs": [{ "inputs": [d.join("videos/a.mkv")], "roots": [], "out_dir": d.join("out"), "options": options }] });
        std::fs::write(d.join("data/av1opt/ui-queue.json"), saved.to_string()).unwrap();
    });
    let st = s.get("/api/state").json();
    let presets = st["meta_presets"].as_array().unwrap();
    let yt = presets.iter().find(|p| p["id"] == "youtube").expect("youtube preset");
    assert!(yt["tags"].as_array().unwrap().iter().any(|t| t[0] == "purl" && t[1] == "{webpage_url}"), "{yt}");

    // Saved metadata options come back with the job.
    let o = &s.get("/api/jobs/heads").json()["jobs"][0]["options"];
    assert_eq!(o["keep_metadata"], false, "{o}");
    assert_eq!(o["keep_dates"], true);
    assert_eq!(o["meta_tags"][0][1], "{title|file}");

    // Preview checks tag names before touching the file, and only looks at videos.
    let a = s.dir.join("videos/a.mkv");
    let r = s.post("/api/meta/preview", &json!({ "input": a, "options": { "meta_tags": [["bad key", "x"]] } }));
    assert_eq!(r.status, 400);
    assert!(r.body.contains("bad key"), "{}", r.body);
    let r = s.post("/api/meta/preview", &json!({ "input": s.dir.join("videos/notes.txt") }));
    assert_eq!(r.status, 400);
    assert!(r.body.contains("not a video"), "{}", r.body);
}

#[test]
fn duplicate_search_runs_and_guards_its_files() {
    let s = Server::start("dups", |d| {
        std::fs::write(d.join("videos/a.mkv"), b"not really a video").unwrap();
        std::fs::write(d.join("videos/b.mp4"), b"not really a video").unwrap();
        std::fs::write(d.join("videos/notes.txt"), b"x").unwrap();
    });
    let videos = s.dir.join("videos");
    assert!(s.get("/api/dups").json()["runs"].as_array().unwrap().is_empty());

    // Bad requests.
    assert_eq!(s.post("/api/dups", &json!({ "paths": [] })).status, 400);
    assert_eq!(s.post("/api/dups", &json!({ "paths": [videos], "tolerance": 21 })).status, 400);
    assert_eq!(s.post("/api/dups", &json!({ "paths": [s.dir.join("nowhere")] })).status, 400);
    assert_eq!(s.get("/api/dups/1").status, 404);

    let r = s.post("/api/dups", &json!({ "paths": [videos], "prefer": [videos], "avoid": [" old ", ""] }));
    assert_eq!(r.status, 200, "{}", r.body);
    let id = r.json()["id"].as_u64().unwrap();
    let t = Instant::now();
    let run = loop {
        let run = s.get(&format!("/api/dups/{id}")).json();
        if run["status"] != "running" {
            break run;
        }
        assert!(t.elapsed() < Duration::from_secs(20), "still running: {run}");
        std::thread::sleep(Duration::from_millis(50));
    };
    // `false` is no ffprobe: both videos are listed as unreadable, the text file is not a video.
    assert_eq!(run["status"], "done", "{run}");
    assert_eq!(run["report"]["unreadable"].as_array().unwrap().len(), 2, "{run}");
    assert_eq!(run["report"]["stats"]["videos"], 2, "{run}");
    assert_eq!(run["report"]["stats"]["compared"], 0, "{run}");
    assert!(run["report"]["groups"].as_array().unwrap().is_empty());
    let runs = s.get("/api/dups").json();
    assert_eq!(runs["runs"][0]["id"], id);
    assert!(runs["runs"][0].get("report").is_none(), "the list carries no results");
    assert_eq!(s.post(&format!("/api/dups/{id}/cancel"), &json!({})).status, 200);

    // The folders are remembered for the next visit, with blank avoid entries dropped.
    let st: Value = serde_json::from_slice(&std::fs::read(s.data("ui.json")).unwrap()).unwrap();
    assert_eq!(st["dup_paths"][0], json!(videos));
    assert_eq!(st["dup_avoid"], json!(["old"]));

    // Thumbnails and pairs are only for files of a result; these two weren't in any group.
    let a = s.dir.join("videos/a.mkv");
    assert_eq!(s.get("/thumb?path=/etc/passwd&t=1").status, 404);
    assert_eq!(s.get(&format!("/thumb?path={}&t=1", a.display())).status, 404);
    assert_eq!(s.get("/thumb").status, 400);
    assert_eq!(s.post("/api/dups/pair", &json!({ "left": a, "right": "/etc/passwd" })).status, 400);

    // Only files that are in a group of the result can go to the trash.
    assert_eq!(run["trashed"], json!([]));
    assert_eq!(s.post("/api/dups/99/trash", &json!({ "paths": [a] })).status, 404);
    assert_eq!(s.post(&format!("/api/dups/{id}/trash"), &json!({ "paths": [] })).status, 400);
    for p in [a.clone(), PathBuf::from("/etc/passwd")] {
        let r = s.post(&format!("/api/dups/{id}/trash"), &json!({ "paths": [p] }));
        assert_eq!(r.status, 400, "{}", r.body);
        assert!(r.body.contains("not a file of this result"), "{}", r.body);
    }
    assert!(a.exists());
    assert!(!s.dir.join("data/Trash/files/a.mkv").exists());
}

#[test]
fn benchmark_requests_and_tuned_settings() {
    // A finished benchmark of an earlier session: three settings, the third without a result
    // on one 1080p video.
    let row = |config: usize, size: f64, time: f64, missing: usize| {
        json!({ "config": config, "size": size, "time": time, "size_worst": size, "size_best": size, "crf": 33.0, "speed": 1.0, "missing": missing, "pareto": missing == 0 })
    };
    let summary = |class: Value, videos: usize| {
        json!({ "class": class, "videos": videos, "left_out": 0, "rows": [row(0, 1.0, 1.0, 0), row(1, 0.9, 1.5, 0), row(2, 1.3, 0.5, 1)],
            "picks": { "smallest": 1, "balanced": 0, "fastest": 0 } })
    };
    let report = json!({
        "format": 1, "made": 1_700_000_000u64, "encoder": "ffmpeg version test", "target_vmaf": 93.0, "min_crf": 20, "max_crf": 55, "samples": 2, "sample_secs": 6.0,
        "configs": [{ "preset": 6, "params": "tune=0" }, { "preset": 4, "params": "tune=0:enable-qm=1" }, { "preset": 10, "params": "tune=0" }],
        "baseline": 0, "videos": [], "summaries": [summary(Value::Null, 2), summary(json!("hd"), 2)], "skipped": [],
        "elapsed_secs": 60.0, "fresh": 10, "cached": 0,
    });
    let s = Server::start("bench", |d| {
        std::fs::write(d.join("videos/a.mkv"), b"x").unwrap();
        std::fs::write(d.join("data/av1opt/bench-last.json"), report.to_string()).unwrap();
        let saved = json!({ "format": 1, "jobs": [{ "inputs": [d.join("videos/a.mkv")], "roots": [], "out_dir": d.join("out"), "options": { "tuned": true, "svt_params": "tune=2", "shrink_audio": true } }] });
        std::fs::write(d.join("data/av1opt/ui-queue.json"), saved.to_string()).unwrap();
    });
    let videos = s.dir.join("videos");

    // What the page starts from: no run, the saved result, nothing tuned, the defaults.
    let b = s.get("/api/bench").json();
    assert!(b["run"].is_null(), "{b}");
    assert_eq!(b["report"]["configs"][1]["preset"], 4, "{b}");
    assert_eq!(b["tuning"], json!([]));
    assert_eq!(b["defaults"]["baseline"], json!({ "preset": 6, "params": "tune=0" }));
    assert_eq!(b["defaults"]["presets"], json!([4, 6, 8]));
    assert!(b["defaults"]["param_sets"].as_array().unwrap().contains(&json!("tune=0")));
    let st = s.get("/api/state").json();
    assert_eq!(st["tuning"], json!([]));
    assert_eq!(st["default_svt_params"], "tune=0");

    // Bad requests are refused before anything runs.
    for (body, why) in [
        (json!({ "paths": [] }), "folder"),
        (json!({ "paths": [videos], "target_vmaf": 30 }), "VMAF"),
        (json!({ "paths": [videos], "presets": [14] }), "presets"),
        (json!({ "paths": [videos], "presets": [] }), "presets"),
        (json!({ "paths": [videos], "max_videos": 0 }), "videos"),
        (json!({ "paths": [videos], "samples": 9 }), "clips"),
        (json!({ "paths": [videos], "param_sets": ["crf=20"] }), "crf"),
        (json!({ "paths": [videos], "param_sets": ["tune"] }), "tune"),
        (json!({ "paths": [videos], "presets": [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13] }), "settings"),
    ] {
        let r = s.post("/api/bench", &body);
        assert_eq!(r.status, 400, "{body}: {}", r.body);
        assert!(r.body.contains(why), "{body}: {}", r.body);
    }
    // `false` is no ffmpeg: a good request says where to fix that, and nothing starts.
    let r = s.post("/api/bench", &json!({ "paths": [videos] }));
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.body.contains("Settings"), "{}", r.body);
    assert!(s.get("/api/bench").json()["run"].is_null());
    assert_eq!(s.post("/api/bench/cancel", &json!({})).status, 200);

    // Settings come from the result on the page, with that size's own numbers.
    let r = s.post("/api/bench/tuning", &json!({ "classes": ["hd"], "config": 1 }));
    assert_eq!(r.status, 200, "{}", r.body);
    let t = r.json();
    assert_eq!(t["tuning"][0]["class"], "hd");
    assert_eq!(t["tuning"][0]["label"], "1080p");
    assert_eq!(t["tuning"][0]["choice"]["preset"], 4);
    assert_eq!(t["tuning"][0]["choice"]["params"], "tune=0:enable-qm=1");
    assert_eq!(t["tuning"][0]["choice"]["videos"], 2);
    let file: Value = serde_json::from_slice(&std::fs::read(s.data("tuning.json")).unwrap()).unwrap();
    assert_eq!(file["format"], 1);
    assert_eq!(file["classes"]["hd"]["size"], 0.9);
    assert_eq!(s.get("/api/state").json()["tuning"][0]["choice"]["preset"], 4);
    assert_eq!(s.get("/api/bench").json()["tuning"][0]["class"], "hd");

    // Not a setting without a result somewhere, one that isn't there, a size nothing was
    // measured for, or a size that doesn't exist.
    for (body, why) in [
        (json!({ "classes": ["hd"], "config": 2 }), "no result"),
        (json!({ "classes": ["hd"], "config": 7 }), "no such setting"),
        (json!({ "classes": ["uhd"], "config": 1 }), "no videos of that size"),
        (json!({ "classes": ["4k"], "config": 1 }), "4k"),
    ] {
        let r = s.post("/api/bench/tuning", &body);
        assert_eq!(r.status, 400, "{body}: {}", r.body);
        assert!(r.body.contains(why), "{body}: {}", r.body);
    }
    assert_eq!(s.get("/api/bench").json()["tuning"].as_array().unwrap().len(), 1, "refused requests change nothing");

    // "All sizes" sets every size the result has, and forgetting takes them away again.
    let r = s.post("/api/bench/tuning", &json!({ "config": 0 }));
    assert_eq!(r.json()["tuning"][0]["choice"]["preset"], 6, "{}", r.body);
    let r = s.post("/api/bench/tuning", &json!({ "classes": ["hd"], "config": null }));
    assert_eq!(r.json()["tuning"], json!([]), "{}", r.body);

    // A saved job keeps its encoder options.
    let o = &s.get("/api/jobs/heads").json()["jobs"][0]["options"];
    assert_eq!(o["tuned"], true, "{o}");
    assert_eq!(o["shrink_audio"], true, "{o}");
    assert_eq!(o["svt_params"], "tune=2");
}
