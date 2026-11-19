//! Black-box tests of `av1opt ui`: the real binary, started the way the desktop app starts it,
//! talked to over HTTP. They need no ffmpeg: the tools are pointed at `false`, so jobs are
//! accepted into the queue but never encode.
#![cfg(feature = "ui")]

mod common;

use common::{Server, TOKEN};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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
        // A job can name any file, and a link can be named like a video: neither is one.
        std::fs::write(d.join("videos/notes.txt"), b"private").unwrap();
        std::os::unix::fs::symlink(d.join("videos/notes.txt"), d.join("videos/link.mkv")).unwrap();
        let inputs = [d.join("videos/a.mkv"), d.join("videos/notes.txt"), d.join("videos/link.mkv")];
        let saved = json!({ "format": 1, "jobs": [{ "inputs": inputs, "roots": [], "out_dir": d.join("out"), "options": {} }] });
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
    assert_eq!(s.get(&q(&s.dir.join("videos/notes.txt"))).status, 403);
    assert_eq!(s.get(&q(&s.dir.join("videos/link.mkv"))).status, 403);
}

#[test]
fn names_that_are_not_text_stay_off_the_page_and_take_nothing_down() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let s = Server::start("nottext", |d| {
        let v = d.join("videos");
        for name in [&b"ok.mkv"[..], "w\u{fffd}.mkv".as_bytes(), b"w\xff.mkv"] {
            std::fs::write(v.join(OsStr::from_bytes(name)), b"x").unwrap();
        }
        let inside = v.join(OsStr::from_bytes(b"d\xff"));
        std::fs::create_dir(&inside).unwrap();
        std::fs::write(inside.join("in.mkv"), b"x").unwrap();
        // The one way a request can get to such a path. And the folder the broken name would be.
        std::os::unix::fs::symlink(&inside, v.join("link")).unwrap();
        std::fs::create_dir(v.join("d\u{fffd}")).unwrap();
    });
    let v = s.dir.join("videos");
    let esc = |t: &str| t.bytes().map(|b| if b.is_ascii_alphanumeric() || b"/._-".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect::<String>();
    let vs = esc(v.to_str().unwrap());

    // Listed: what has a name that can be shown and asked for again, and how much has not.
    let r = s.get(&format!("/api/browse?path={vs}"));
    assert_eq!(r.status, 200, "{}", r.body);
    let list = r.json();
    let names = |key: &str, field: Option<&str>| -> Vec<String> {
        let mut n: Vec<String> = list[key].as_array().unwrap().iter().map(|e| field.map_or(e, |f| &e[f]).as_str().unwrap().to_string()).collect();
        n.sort();
        n
    };
    assert_eq!(names("files", Some("name")), ["ok.mkv", "w\u{fffd}.mkv"]);
    assert_eq!(names("dirs", None), ["d\u{fffd}", "link"]);
    assert_eq!(list["not_listed"], 2);

    // Bytes that are not text do not become the file or folder they would look like.
    assert_eq!(s.get(&format!("/api/browse?path={vs}/d%FF")).status, 400);
    // A link to such a folder or file is refused, with a reason.
    let r = s.get(&format!("/api/browse?path={vs}/link"));
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.json()["error"].as_str().unwrap().contains("not valid UTF-8"), "{}", r.body);
    assert_eq!(s.post("/api/source", &json!({ "path": v.join("link/in.mkv") })).status, 400);
    assert_eq!(s.post("/api/scans", &json!({ "paths": [v.join("link")] })).status, 400);
    assert_eq!(s.post("/api/history/dirs", &json!({ "path": v.join("link") })).status, 400);

    // A scan of the folder finishes, twice over, and lists the rest.
    for _ in 0..2 {
        let id = s.post("/api/scans", &json!({ "paths": [v] })).json()["id"].as_u64().unwrap();
        let started = Instant::now();
        let done = loop {
            let r = s.get(&format!("/api/scans/{id}"));
            assert_eq!(r.status, 200, "{}", r.body);
            let scan = r.json();
            if scan["status"] != "running" {
                break scan;
            }
            assert!(started.elapsed() < Duration::from_secs(30), "the scan does not end");
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(done["status"], "done", "{done}");
        assert_eq!(done["not_listed"], 2, "{done}");
        let rows: Vec<&str> = done["rows"].as_array().unwrap().iter().map(|r| r["path"].as_str().unwrap()).collect();
        assert!(rows.iter().all(|p| Path::new(p).exists()), "{rows:?}");
        for name in ["ok.mkv", "w\u{fffd}.mkv"] {
            assert!(rows.contains(&v.join(name).to_str().unwrap()), "{name} is missing from {rows:?}");
        }
    }

    // The scan made the real file playable; the broken name still does not reach it.
    assert_eq!(s.get(&format!("/media?path={vs}/{}", esc("w\u{fffd}.mkv"))).status, 200);
    assert_eq!(s.get(&format!("/media?path={vs}/w%FF.mkv")).status, 403);
    assert_eq!(s.get("/api/state").status, 200);
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
    let mut s2 = Server::start_in(&s.dir, "false", "false");
    let heads = s2.get("/api/jobs/heads").json();
    let jobs = heads["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 1, "{heads}");
    assert_eq!(jobs[0]["status"], "queued");
    assert_eq!(jobs[0]["total"], 2);
    // Never scanned or probed: no time estimate yet, and the header says for how many files.
    assert_eq!((jobs[0]["left_secs"].as_f64(), jobs[0]["left_unknown"].as_u64()), (Some(0.0), Some(2)), "{heads}");
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
    let s3 = Server::start_in(&s.dir, "false", "false");
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
    assert_eq!(s.post("/api/settings/tools", &json!({ "ffmpeg": "ffmpeg9" })).status, 200);
    let ui = std::fs::read_to_string(s.data("ui.json")).unwrap();
    assert!(ui.contains("future_field") && !ui.contains("ffmpeg9"), "{ui}");
    // The queue is set aside, not restored and not overwritten.
    assert!(!s.data("ui-queue.json").exists() || std::fs::read_to_string(s.data("ui-queue.json")).unwrap().contains("\"format\": 1"));
    assert!(std::fs::read_to_string(s.data("ui-queue.format99.json")).unwrap().contains("anything"));
    assert!(s.get("/api/jobs/heads").json()["jobs"].as_array().unwrap().is_empty());
}

#[test]
fn a_link_is_followed_at_start_and_kept_only_to_a_program_of_that_name() {
    let s = Server::start("tool-links", |d| {
        let bin = d.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for f in ["ffmpeg-7.1", "launcher"] {
            std::fs::write(bin.join(f), b"not a program").unwrap();
        }
        std::os::unix::fs::symlink(bin.join("ffmpeg-7.1"), bin.join("ffmpeg")).unwrap();
        std::os::unix::fs::symlink(bin.join("launcher"), bin.join("ffprobe")).unwrap();
        let saved = json!({ "format": 1, "ffmpeg": bin.join("ffmpeg"), "ffprobe": bin.join("ffprobe") });
        std::fs::write(d.join("data/av1opt/ui.json"), saved.to_string()).unwrap();
    });
    let bin = std::fs::canonicalize(s.dir.join("bin")).unwrap();
    let tools = s.get("/api/state").json()["tools"].clone();
    // The saved link now names the file it led to when it was checked; the link to a program
    // of another name is dropped for the default.
    assert_eq!(tools["ffmpeg"], json!(bin.join("ffmpeg-7.1")), "{tools}");
    assert_eq!(tools["ffprobe"], "false", "{tools}");
    // Chosen on the page, it is refused and the reason names what it leads to.
    let r = s.post("/api/settings/tools", &json!({ "ffprobe": bin.join("ffprobe") }));
    assert!(r.status == 400 && r.body.contains("a program with another name"), "{}", r.body);
}

#[test]
fn saves_at_the_same_moment_lose_nothing() {
    let s = Server::start("saves", |d| {
        for i in 0..24 {
            std::fs::create_dir_all(d.join(format!("out{i}"))).unwrap();
        }
    });
    let real = std::fs::canonicalize(&s.dir).unwrap();
    let dirs: Vec<String> = (0..24).map(|i| real.join(format!("out{i}")).to_string_lossy().into_owned()).collect();
    std::thread::scope(|scope| {
        for dir in &dirs {
            let s = &s;
            scope.spawn(move || assert_eq!(s.post("/api/history/dirs", &json!({ "path": dir })).status, 200));
        }
    });
    let ui: Value = serde_json::from_slice(&std::fs::read(s.data("ui.json")).unwrap()).unwrap();
    let saved = ui["history_dirs"].as_array().unwrap();
    for dir in &dirs {
        assert!(saved.iter().any(|d| d == dir), "{dir} is missing from {saved:?}");
    }
    let left: Vec<_> = std::fs::read_dir(s.dir.join("data/av1opt")).unwrap().flatten().map(|e| e.file_name()).filter(|n| n.to_string_lossy().ends_with(".tmp")).collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn a_settings_file_kept_elsewhere_stays_a_link() {
    let s = Server::start("dotfiles", |d| {
        std::fs::create_dir_all(d.join("dotfiles")).unwrap();
        std::fs::write(d.join("dotfiles/ui.json"), "{}").unwrap();
        std::os::unix::fs::symlink("../../dotfiles/ui.json", d.join("data/av1opt/ui.json")).unwrap();
    });
    assert_eq!(s.post("/api/settings/tools", &json!({ "ffmpeg": "ffmpeg9" })).status, 200);
    assert!(std::fs::symlink_metadata(s.data("ui.json")).unwrap().file_type().is_symlink());
    let kept: Value = serde_json::from_slice(&std::fs::read(s.dir.join("dotfiles/ui.json")).unwrap()).unwrap();
    assert_eq!(kept["ffmpeg"], "ffmpeg9");
}

#[test]
fn a_program_that_has_stopped_sending_is_still_answered() {
    use std::io::{Read, Write};
    let s = Server::start("half-closed", |_| {});
    let mut c = std::net::TcpStream::connect(("127.0.0.1", s.port)).unwrap();
    c.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
    write!(c, "GET /api/state HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nCookie: {}\r\nConnection: close\r\n\r\n", s.port, s.cookie()).unwrap();
    c.shutdown(std::net::Shutdown::Write).unwrap();
    let mut answer = String::new();
    c.read_to_string(&mut answer).unwrap();
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
}

#[test]
fn settings_tools_validation() {
    let s = Server::start("tools", |_| {});
    assert_eq!(s.post("/api/settings/tools", &json!({ "ffmpeg": "relative/ffmpeg" })).status, 400);
    // The page starts these programs: only ones named like the tools they stand for.
    for bad in [json!({ "ffmpeg": "/bin/sh" }), json!({ "ffmpeg": "rm" }), json!({ "ffprobe": "/usr/bin/ffmpeg" }), json!({ "ffmpeg": "/usr/bin/ffprobe" })] {
        let r = s.post("/api/settings/tools", &bad);
        assert_eq!(r.status, 400, "{bad}: {}", r.body);
    }
    // A path has to lead to a file that is there: one that is not could be put there later
    // by anyone who can write to the folder.
    let r = s.post("/api/settings/tools", &json!({ "ffmpeg": "/nonexistent/ffmpeg", "ffprobe": "" }));
    assert!(r.status == 400 && r.body.contains("is not there"), "{}", r.body);
    use std::os::unix::fs::PermissionsExt;
    let bin = s.dir.join("bin");
    std::fs::create_dir_all(bin.join("open")).unwrap();
    for f in ["ffprobe-7.1", "open/ffmpeg"] {
        std::fs::write(bin.join(f), b"not a program").unwrap();
    }
    std::os::unix::fs::symlink(bin.join("ffprobe-7.1"), bin.join("ffprobe")).unwrap();
    // In a folder anyone can write to, the file can be swapped for another.
    std::fs::set_permissions(bin.join("open"), std::fs::Permissions::from_mode(0o777)).unwrap();
    let r = s.post("/api/settings/tools", &json!({ "ffmpeg": bin.join("open/ffmpeg") }));
    assert!(r.status == 400 && r.body.contains("can be changed by others"), "{}", r.body);
    std::fs::set_permissions(bin.join("open"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::create_dir_all(bin.join("ffmpeg.d")).unwrap();
    let r = s.post("/api/settings/tools", &json!({ "ffmpeg": bin.join("ffmpeg.d") }));
    assert!(r.status == 400 && r.body.contains("is not a program"), "{}", r.body);
    // What is kept is the file the path led to, not the link.
    let r = s.post("/api/settings/tools", &json!({ "ffmpeg": "ffmpeg7", "ffprobe": bin.join("ffprobe") }));
    assert_eq!(r.status, 200, "{}", r.body);
    let ui: Value = serde_json::from_slice(&std::fs::read(s.data("ui.json")).unwrap()).unwrap();
    assert_eq!(ui["format"], 1);
    assert_eq!(ui["ffmpeg"], "ffmpeg7");
    assert_eq!(ui["ffprobe"], json!(std::fs::canonicalize(bin.join("ffprobe-7.1")).unwrap()));
    assert!(s.get("/api/state").json()["tools"]["problem"].is_string());
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
    assert_eq!(b["defaults"]["baseline"], json!({ "preset": 5, "params": "tune=0" }));
    assert_eq!(b["defaults"]["presets"], json!([4, 5, 8]));
    assert!(b["defaults"]["param_sets"].as_array().unwrap().contains(&json!("tune=0")));
    let st = s.get("/api/state").json();
    assert_eq!(st["tuning"], json!([]));
    assert_eq!(st["default_svt_params"], "tune=0");

    // Bad requests are refused before anything runs.
    for (body, why) in [
        (json!({ "paths": [] }), "folder"),
        (json!({ "paths": [videos], "target_vmaf": 0 }), "target VMAF"),
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

#[test]
fn originals_are_replaced_and_put_back_through_reports_in_history() {
    let s = Server::start("replace", |d| {
        std::fs::write(d.join("videos/film.mp4"), vec![b's'; 3000]).unwrap();
        for dir in ["out", "elsewhere"] {
            std::fs::create_dir_all(d.join(dir)).unwrap();
            let r = json!({
                "format": 1, "input": d.join("videos/film.mp4"), "output": d.join("out/film.mkv"), "source_bytes": 3000, "output_bytes": 1000,
                "savings": 0.667, "mode": "whole", "crf": 30.0, "preset": 6, "target_vmaf": 93.0,
            });
            std::fs::write(d.join(dir).join("film.av1opt.json"), r.to_string()).unwrap();
        }
        std::fs::write(d.join("out/film.mkv"), vec![b'o'; 1000]).unwrap();
        // The encode as av1opt would have recorded it.
        write_record(d, &[encoded_line(&d.join("videos/film.mp4"), &d.join("out/film.mkv"))]);
    });
    let (original, output, new) = (s.dir.join("videos/film.mp4"), s.dir.join("out/film.mkv"), s.dir.join("videos/film.mkv"));
    let report = s.dir.join("out/film.av1opt.json");
    let sizes = || [&original, &output, &new].map(|p| std::fs::metadata(p).map(|m| m.len()).ok());
    let state = |s: &Server| s.get("/api/history").json()["reports"][0]["replace"].clone();

    // Only reports in a history folder can move files.
    let r = s.post("/api/replace", &json!({ "report": report }));
    assert_eq!(r.status, 400);
    assert!(r.body.contains("outside the history folders"), "{}", r.body);
    assert_eq!(s.post("/api/history/dirs", &json!({ "path": s.dir.join("out") })).status, 200);
    assert_eq!(s.post("/api/replace", &json!({ "report": s.dir.join("elsewhere/film.av1opt.json") })).status, 400);
    let r = s.post("/api/replace", &json!({ "report": original }));
    assert!(r.status == 400 && r.body.contains("not a report"), "{}", r.body);
    assert_eq!(s.post("/api/replace/undo", &json!({ "report": report })).status, 400);
    assert_eq!(sizes(), [Some(3000), Some(1000), None]);
    let st = state(&s);
    assert_eq!(st["state"], "ready");
    assert_eq!(st["dest"], json!(new));
    assert_eq!(st["copy"], false);

    // The output takes the original's place; the original is in the (scratch) trash.
    let r = s.post("/api/replace", &json!({ "report": report }));
    assert_eq!(r.status, 200, "{}", r.body);
    let trashed = s.dir.join("data/Trash/files/film.mp4");
    assert_eq!(r.json()["trash"], json!(trashed));
    assert_eq!(r.json()["dest"], json!(new));
    assert_eq!(sizes(), [None, None, Some(1000)]);
    assert_eq!(std::fs::metadata(&trashed).unwrap().len(), 3000);
    let r = s.post("/api/replace", &json!({ "report": report }));
    assert!(r.status == 400 && r.body.contains("already replaced"), "{}", r.body);
    let row = s.get("/api/history").json()["reports"][0].clone();
    assert_eq!(row["replace"]["state"], "replaced");
    assert_eq!(row["replace"]["can_undo"], true);
    assert_eq!(row["output"], json!(new));
    assert_eq!(row["input_exists"], true);
    // Compare still has a source to show: the copy in the trash.
    assert_eq!(s.get(&format!("/media?path={}", trashed.display())).status, 200);

    // And back.
    let r = s.post("/api/replace/undo", &json!({ "report": report }));
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(sizes(), [Some(3000), Some(1000), None]);
    assert!(!trashed.exists() && !s.dir.join("data/Trash/info/film.mp4.trashinfo").exists());
    assert_eq!(state(&s)["state"], "ready");

    // A file that no longer matches its report stays where it is.
    std::fs::write(&original, b"edited").unwrap();
    assert_eq!(state(&s)["why"], "the original changed since the encode");
    let r = s.post("/api/replace", &json!({ "report": report }));
    assert!(r.status == 400 && r.body.contains("changed since the encode"), "{}", r.body);
    assert_eq!(sizes(), [Some(6), Some(1000), None]);
}

/// Puts lines in av1opt's own record, private as av1opt keeps it.
fn write_record(d: &Path, lines: &[Value]) {
    use std::os::unix::fs::PermissionsExt;
    let path = d.join("data/av1opt/ledger.jsonl");
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn sha256(p: &Path) -> String {
    let o = std::process::Command::new("sha256sum").arg(p).output().unwrap();
    String::from_utf8(o.stdout).unwrap().split(' ').next().unwrap().to_string()
}

/// The line an encode of `source` to `output` leaves, both files being there.
fn encoded_line(source: &Path, output: &Path) -> Value {
    let size = |p: &Path| std::fs::metadata(p).unwrap().len();
    json!({
        "format": 1, "time": 1, "event": "encoded", "output": output, "report": output.with_extension("av1opt.json"), "source": source,
        "source_bytes": size(source), "output_bytes": size(output), "output_sha256": sha256(output),
    })
}

/// A file in the (scratch) trash that says it came from `original`, as a real entry does.
fn in_the_trash(d: &Path, original: &Path, bytes: usize) -> PathBuf {
    let name = original.file_name().unwrap().to_str().unwrap();
    std::fs::create_dir_all(d.join("data/Trash/files")).unwrap();
    std::fs::create_dir_all(d.join("data/Trash/info")).unwrap();
    let trashed = d.join("data/Trash/files").join(name);
    std::fs::write(&trashed, vec![b's'; bytes]).unwrap();
    let info = format!("[Trash Info]\nPath={}\nDeletionDate=2026-11-10T22:15:03\n", original.display());
    std::fs::write(d.join("data/Trash/info").join(format!("{name}.trashinfo")), info).unwrap();
    trashed
}

fn write_report(at: &Path, report: Value) {
    std::fs::create_dir_all(at.parent().unwrap()).unwrap();
    std::fs::write(at, report.to_string()).unwrap();
}

fn add_history_folder(s: &Server, name: &str) {
    assert_eq!(s.post("/api/history/dirs", &json!({ "path": s.dir.join(name) })).status, 200);
}

fn nothing_in_the_trash(s: &Server) -> bool {
    std::fs::read_dir(s.dir.join("data/Trash/files")).map_or(true, |mut d| d.next().is_none())
}

#[test]
fn a_planted_report_cannot_send_a_video_to_the_trash() {
    let s = Server::start("planted-replace", |d| {
        std::fs::write(d.join("videos/movie.mp4"), vec![b's'; 3000]).unwrap();
        std::fs::create_dir_all(d.join("drop")).unwrap();
        std::fs::write(d.join("drop/evil.mkv"), vec![b'e'; 500]).unwrap();
        let r = json!({ "format": 1, "input": d.join("videos/movie.mp4"), "output": d.join("drop/evil.mkv"), "source_bytes": 3000, "output_bytes": 500, "crf": 30.0 });
        write_report(&d.join("drop/evil.av1opt.json"), r);
    });
    add_history_folder(&s, "drop");
    let (victim, evil, report) = (s.dir.join("videos/movie.mp4"), s.dir.join("drop/evil.mkv"), s.dir.join("drop/evil.av1opt.json"));

    // Still listed, as what it is, with nothing offered and nothing looked up.
    let row = s.get("/api/history").json()["reports"][0].clone();
    assert_eq!((&row["on_record"], &row["replace"]["state"]), (&json!(false), &json!("not_recorded")), "{row}");
    assert!(row["replace"]["why"].as_str().unwrap().contains("`av1opt replace`"), "{row}");
    assert_eq!((&row["input_exists"], &row["output_exists"]), (&Value::Null, &Value::Null), "{row}");

    let r = s.post("/api/replace", &json!({ "report": report }));
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.body.contains("no record of making this output") && r.body.contains("`av1opt replace`"), "{}", r.body);
    assert_eq!(std::fs::read(&victim).unwrap(), vec![b's'; 3000]);
    assert_eq!(std::fs::read(&evil).unwrap(), vec![b'e'; 500]);
    assert!(!s.dir.join("videos/movie.mkv").exists() && nothing_in_the_trash(&s));
}

/// A report that holds a hash of the original, and its time, as an encode of today writes it.
/// The hash is of something else, as it is for a file rewritten since.
fn report_of_a_rewritten_original(original: &Path, output: &Path) -> Value {
    let time = std::fs::metadata(original).unwrap().modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    json!({
        "format": 1, "input": original, "output": output, "source_bytes": 3000, "output_bytes": 1000,
        "source_mtime": time, "source_hash": "0123456789abcdef",
    })
}

#[test]
fn an_original_rewritten_with_its_date_set_back_stays_where_it_is() {
    // On the page.
    let s = Server::start("date-set-back", |d| {
        std::fs::write(d.join("videos/movie.mp4"), vec![b's'; 3000]).unwrap();
        std::fs::create_dir_all(d.join("out")).unwrap();
        std::fs::write(d.join("out/movie.mkv"), vec![b'o'; 1000]).unwrap();
        write_record(d, &[encoded_line(&d.join("videos/movie.mp4"), &d.join("out/movie.mkv"))]);
        write_report(&d.join("out/movie.av1opt.json"), report_of_a_rewritten_original(&d.join("videos/movie.mp4"), &d.join("out/movie.mkv")));
    });
    add_history_folder(&s, "out");
    let r = s.post("/api/replace", &json!({ "report": s.dir.join("out/movie.av1opt.json") }));
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.body.contains("the original changed since the encode"), "{}", r.body);
    assert_eq!(std::fs::read(s.dir.join("videos/movie.mp4")).unwrap(), vec![b's'; 3000]);
    assert!(s.dir.join("out/movie.mkv").exists() && nothing_in_the_trash(&s));

    // On the command line, where the dry run says what --apply will do.
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ui-date-set-back");
    let _ = std::fs::remove_dir_all(&dir);
    for folder in ["videos", "out", "data"] {
        std::fs::create_dir_all(dir.join(folder)).unwrap();
    }
    let (original, output) = (dir.join("videos/film.mp4"), dir.join("out/film.mkv"));
    std::fs::write(&original, vec![b's'; 3000]).unwrap();
    std::fs::write(&output, vec![b'o'; 1000]).unwrap();
    write_report(&dir.join("out/film.av1opt.json"), report_of_a_rewritten_original(&original, &output));
    let run = replace_command(&dir, &[dir.as_os_str()]);
    let said = String::from_utf8_lossy(&run.stdout).into_owned();
    assert!(said.contains("1 output can take") && said.contains("With --apply, 1 original is read whole first"), "{said}");
    let run = replace_command(&dir, &[dir.as_os_str(), std::ffi::OsStr::new("--apply")]);
    let said = String::from_utf8_lossy(&run.stderr).into_owned();
    assert!(!run.status.success() && said.contains("the original changed since the encode"), "{said}");
    assert_eq!(said.matches("reading all of").count(), 1, "{said}");
    assert_eq!(std::fs::read(&original).unwrap(), vec![b's'; 3000]);
    assert!(output.exists() && !dir.join("videos/film.mkv").exists());
}

#[test]
fn a_recorded_output_swapped_for_another_file_does_not_take_the_originals_place() {
    let s = Server::start("planted-swap", |d| {
        std::fs::write(d.join("videos/movie.mp4"), vec![b's'; 3000]).unwrap();
        std::fs::write(d.join("videos/other.mp4"), vec![b't'; 3000]).unwrap();
        std::fs::create_dir_all(d.join("out")).unwrap();
        std::fs::write(d.join("out/movie.mkv"), vec![b'o'; 1000]).unwrap();
        write_record(d, &[encoded_line(&d.join("videos/movie.mp4"), &d.join("out/movie.mkv"))]);
        let r = json!({ "format": 1, "input": d.join("videos/movie.mp4"), "output": d.join("out/movie.mkv"), "source_bytes": 3000, "output_bytes": 1000 });
        write_report(&d.join("out/movie.av1opt.json"), r);
        // Someone who can write to the output folder changes the file, keeping its size.
        std::fs::write(d.join("out/movie.mkv"), vec![b'e'; 1000]).unwrap();
    });
    add_history_folder(&s, "out");
    let report = s.dir.join("out/movie.av1opt.json");
    let r = s.post("/api/replace", &json!({ "report": report }));
    assert!(r.status == 400 && r.body.contains("not the file av1opt made"), "{}", r.body);
    assert_eq!(std::fs::read(s.dir.join("videos/movie.mp4")).unwrap(), vec![b's'; 3000]);
    assert!(nothing_in_the_trash(&s));

    // The report rewritten to claim another original is no better.
    let forged = json!({ "format": 1, "input": s.dir.join("videos/other.mp4"), "output": s.dir.join("out/movie.mkv"), "source_bytes": 3000, "output_bytes": 1000 });
    write_report(&report, forged);
    assert_eq!(s.get("/api/history").json()["reports"][0]["replace"]["state"], "not_recorded");
    let r = s.post("/api/replace", &json!({ "report": report }));
    assert!(r.status == 400 && r.body.contains("no record of making this output"), "{}", r.body);
    assert_eq!(std::fs::read(s.dir.join("videos/other.mp4")).unwrap(), vec![b't'; 3000]);
    assert!(nothing_in_the_trash(&s));
}

#[test]
fn a_report_rewritten_to_fit_a_changed_original_is_not_believed() {
    let modified = |p: &Path| std::fs::metadata(p).unwrap().modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let s = Server::start("planted-facts", |d| {
        std::fs::write(d.join("videos/movie.mp4"), vec![b's'; 3000]).unwrap();
        std::fs::create_dir_all(d.join("out")).unwrap();
        std::fs::write(d.join("out/movie.mkv"), vec![b'o'; 1000]).unwrap();
        let mut line = encoded_line(&d.join("videos/movie.mp4"), &d.join("out/movie.mkv"));
        line["source_mtime"] = json!(1_000_000);
        write_record(d, &[line]);
        // The time the file has now, so that by the report alone it is the file encoded.
        let now = modified(&d.join("videos/movie.mp4"));
        let r = json!({ "format": 1, "input": d.join("videos/movie.mp4"), "output": d.join("out/movie.mkv"), "source_bytes": 3000, "output_bytes": 1000, "source_mtime": now });
        write_report(&d.join("out/movie.av1opt.json"), r);
    });
    add_history_folder(&s, "out");
    let row = s.get("/api/history").json()["reports"][0].clone();
    assert_eq!(row["replace"]["state"], "no", "{row}");
    let r = s.post("/api/replace", &json!({ "report": s.dir.join("out/movie.av1opt.json") }));
    assert!(r.status == 400 && r.body.contains("does not say what av1opt recorded"), "{}", r.body);
    assert!(s.dir.join("videos/movie.mp4").exists() && s.dir.join("out/movie.mkv").exists() && nothing_in_the_trash(&s));
}

#[test]
fn a_planted_report_cannot_pull_a_file_out_through_undo() {
    let plant = |d: &Path| {
        let (original, placed) = (d.join("videos/movie.mp4"), d.join("videos/movie.mkv"));
        std::fs::write(&placed, vec![b'a'; 1000]).unwrap();
        let trashed = in_the_trash(d, &original, 3000);
        let r = json!({
            "format": 1, "input": original, "output": placed, "source_bytes": 3000, "output_bytes": 1000,
            "replaced": { "time": 1, "from": d.join("drop/stolen.mkv"), "trash": trashed },
        });
        write_report(&d.join("drop/stolen.av1opt.json"), r);
        (original, placed, trashed)
    };
    let attempt = |s: &Server| {
        add_history_folder(s, "drop");
        let r = s.post("/api/replace/undo", &json!({ "report": s.dir.join("drop/stolen.av1opt.json") }));
        assert!(r.status == 400 && (r.body.contains("no record of making this output") || r.body.contains("does not say what av1opt recorded")), "{}", r.body);
        assert_eq!(std::fs::read(s.dir.join("videos/movie.mkv")).unwrap(), vec![b'a'; 1000]);
        assert!(!s.dir.join("drop/stolen.mkv").exists() && !s.dir.join("videos/movie.mp4").exists());
        assert_eq!(std::fs::metadata(s.dir.join("data/Trash/files/movie.mp4")).unwrap().len(), 3000);
    };
    // Nothing on record at all.
    let s = Server::start("planted-undo", |d| {
        plant(d);
    });
    attempt(&s);
    // The replacement is on record, and the report only lies about where the file goes back to.
    let s = Server::start("planted-undo-recorded", |d| {
        let (original, placed, trashed) = plant(d);
        let replaced = json!({ "format": 1, "time": 2, "event": "replaced", "source": original, "trash": trashed, "output": placed, "from": d.join("out/movie.mkv"), "source_bytes": 3000, "output_bytes": 1000 });
        write_record(d, &[replaced]);
    });
    attempt(&s);
}

#[test]
fn what_only_a_report_names_is_neither_served_nor_opened() {
    let probed = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("planted-read-probed");
    let _ = std::fs::remove_file(&probed);
    let (ffmpeg, ffprobe) = stand_in_tools("planted-read", &format!("touch {}\nexit 1", probed.display()));
    let s = Server::start_with_tools("planted-read", &ffmpeg, &ffprobe, |d| {
        std::fs::write(d.join("videos/secret.mkv"), b"private video").unwrap();
        std::fs::write(d.join("videos/notes.txt"), b"private notes").unwrap();
        let r = json!({ "format": 1, "input": d.join("videos/notes.txt"), "output": d.join("videos/secret.mkv"), "source_bytes": 13, "output_bytes": 13 });
        write_report(&d.join("drop/read.av1opt.json"), r);
    });
    add_history_folder(&s, "drop");
    let q = |p: &Path| format!("/media?path={}", p.display());
    let (notes, secret) = (s.dir.join("videos/notes.txt"), s.dir.join("videos/secret.mkv"));

    // Listing history, and opening the report, give access to nothing.
    assert_eq!(s.get("/api/history").json()["reports"].as_array().unwrap().len(), 1);
    let r = s.get(&format!("/api/report?path={}", s.dir.join("drop/read.av1opt.json").display()));
    assert_eq!(r.status, 200, "{}", r.body);
    let r = r.json();
    assert_eq!(r["replace"]["state"], "not_recorded", "{r}");
    for side in ["input_info", "output_info"] {
        assert!(r[side]["error"].as_str().unwrap().contains("was not opened"), "{r}");
        assert!(r[side].get("size").is_none(), "{r}");
    }
    assert_eq!(s.get(&q(&secret)).status, 403);
    assert_eq!(s.get(&q(&notes)).status, 403);
    assert!(!probed.exists(), "ffprobe was run on a file only a report names");

    // Picked by the person, the same video can be looked at.
    assert_eq!(s.post("/api/source", &json!({ "path": secret })).status, 200);
    assert_eq!(s.get(&q(&secret)).status, 200);
    assert_eq!(s.get(&q(&notes)).status, 403);
}

#[test]
fn a_report_has_to_be_a_plain_file_of_a_sensible_size() {
    let s = Server::start("planted-pipe", |d| {
        std::fs::create_dir_all(d.join("drop")).unwrap();
        let pipe = std::ffi::CString::new(d.join("drop/pipe.av1opt.json").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(pipe.as_ptr(), 0o644) }, 0);
        std::fs::write(d.join("drop/huge.av1opt.json"), vec![b' '; (8 << 20) + 1]).unwrap();
    });
    add_history_folder(&s, "drop");
    let (pipe, huge) = (s.dir.join("drop/pipe.av1opt.json"), s.dir.join("drop/huge.av1opt.json"));
    let started = Instant::now();
    let r = s.get(&format!("/api/report?path={}", pipe.display()));
    assert!(r.status == 400 && r.body.contains("not a plain file"), "{}", r.body);
    for route in ["/api/replace", "/api/replace/undo"] {
        let r = s.post(route, &json!({ "report": pipe }));
        assert!(r.status == 400 && r.body.contains("not a plain file"), "{}", r.body);
    }
    let r = s.get(&format!("/api/report?path={}", huge.display()));
    assert!(r.status == 400 && r.body.contains("larger than"), "{}", r.body);
    assert_eq!(s.get("/api/history").json()["reports"], json!([]));
    assert!(started.elapsed() < Duration::from_secs(5), "a request waited on the pipe");
}

/// A past encode as the history file holds it.
fn history_line(time: u64) -> Value {
    json!({
        "format": 1, "time": time, "codec": "h264", "width": 640, "height": 360, "fps": 24.0, "duration_s": 60.0, "source_bitrate": 2e6, "output_bitrate": 5e5,
        "target_vmaf": 93.0, "preset": 6, "mode": "whole", "crf": 30.0, "svt_params": "tune=0",
    })
}

#[test]
fn the_encode_history_is_looked_at_once_and_vouches_for_no_report() {
    use std::os::unix::fs::PermissionsExt;
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ui-takeover");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("data/av1opt")).unwrap();
    std::fs::create_dir_all(dir.join("cache")).unwrap();
    std::fs::create_dir_all(dir.join("videos")).unwrap();
    std::fs::write(dir.join("videos/old.mp4"), vec![b's'; 3000]).unwrap();
    std::fs::create_dir_all(dir.join("out")).unwrap();
    std::fs::write(dir.join("out/old.mkv"), vec![b'o'; 1000]).unwrap();
    // A report that agrees with a line of the history in every number the line has.
    let r = json!({ "format": 1, "input": dir.join("videos/old.mp4"), "output": dir.join("out/old.mkv"), "source_bytes": 3000, "output_bytes": 1000, "crf": 30.0, "preset": 6, "target_vmaf": 93.0, "mode": "whole", "svt_params": "tune=0" });
    write_report(&dir.join("out/old.av1opt.json"), r);
    std::fs::write(dir.join("data/av1opt/history.jsonl"), format!("{}\nnot json\n{}\n", history_line(100), history_line(200))).unwrap();
    std::fs::write(dir.join("data/av1opt/ui.json"), json!({ "format": 1, "history_dirs": [dir.join("out")] }).to_string()).unwrap();

    let first_words = |name: &str| -> String {
        let log = dir.join(name);
        let mut c = Server::command(&dir, "false", "false").stderr(std::fs::File::create(&log).unwrap()).spawn().unwrap();
        let started = Instant::now();
        while !std::fs::read_to_string(&log).unwrap().contains("av1opt ui on") {
            assert!(started.elapsed() < Duration::from_secs(20), "the page did not start");
            std::thread::sleep(Duration::from_millis(10));
        }
        c.kill().unwrap();
        c.wait().unwrap();
        std::fs::read_to_string(&log).unwrap()
    };
    let said = first_words("first.log");
    assert!(said.contains("0 of 2 past encodes") && said.contains("no file names") && said.contains("`av1opt replace`"), "{said}");
    let record = dir.join("data/av1opt/ledger.jsonl");
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!((mode(&record), mode(&dir.join("data/av1opt"))), (0o600, 0o700));
    let text = std::fs::read_to_string(&record).unwrap();
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(text.contains("\"taken_over\"") && text.contains("\"taken\":0"), "{text}");

    // Once only, and the report stays what it was.
    let said = first_words("second.log");
    assert!(!said.contains("past encodes"), "{said}");
    assert_eq!(std::fs::read_to_string(&record).unwrap(), text);
    let keep = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ui-takeover");
    let s = Server::start_in(&keep, "false", "false");
    assert_eq!(s.get("/api/state").json()["notices"], json!([]));
    let row = s.get("/api/history").json()["reports"][0].clone();
    assert_eq!(row["replace"]["state"], "not_recorded", "{row}");
    assert_eq!(s.post("/api/replace", &json!({ "report": s.dir.join("out/old.av1opt.json") })).status, 400);
    assert!(s.dir.join("videos/old.mp4").exists());
}

#[test]
fn the_page_says_it_when_past_encodes_could_not_be_taken_over() {
    let s = Server::start("takeover-notice", |d| {
        std::fs::write(d.join("data/av1opt/history.jsonl"), format!("{}\n", history_line(100))).unwrap();
    });
    let notices = s.get("/api/state").json()["notices"].to_string();
    assert!(notices.contains("0 of 1 past encodes"), "{notices}");
}

/// `av1opt replace` on the folders below `dir`, with its trash and record there.
fn replace_command(dir: &Path, args: &[&std::ffi::OsStr]) -> std::process::Output {
    av1opt_command(dir, "replace", args)
}

fn adopt_command(dir: &Path, args: &[&std::ffi::OsStr]) -> std::process::Output {
    av1opt_command(dir, "adopt", args)
}

fn out_of(run: &std::process::Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&run.stdout), String::from_utf8_lossy(&run.stderr))
}

fn av1opt_command(dir: &Path, command: &str, args: &[&std::ffi::OsStr]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_av1opt"))
        .arg(command)
        .args(args)
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("XDG_CACHE_HOME", dir.join("cache"))
        .env("AV1OPT_HISTORY", dir.join("data/av1opt/history.jsonl"))
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn the_command_follows_a_report_only_into_the_folders_it_was_given() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ui-replace-command");
    let _ = std::fs::remove_dir_all(&dir);
    for folder in ["videos", "out", "data"] {
        std::fs::create_dir_all(dir.join(folder)).unwrap();
    }
    let (original, output, placed) = (dir.join("videos/film.mp4"), dir.join("out/film.mkv"), dir.join("videos/film.mkv"));
    std::fs::write(&original, vec![b's'; 3000]).unwrap();
    std::fs::write(&output, vec![b'o'; 1000]).unwrap();
    write_report(&dir.join("out/film.av1opt.json"), json!({ "format": 1, "input": original, "output": output, "source_bytes": 3000, "output_bytes": 1000 }));
    let (out, videos) = (dir.join("out"), dir.join("videos"));
    let apply = std::ffi::OsStr::new("--apply");
    let undo = std::ffi::OsStr::new("--undo");
    let anywhere = std::ffi::OsStr::new("--anywhere");

    // The output folder alone does not reach the original.
    let run = replace_command(&dir, &[out.as_os_str(), apply]);
    let said = String::from_utf8_lossy(&run.stdout).into_owned();
    assert!(run.status.success() && said.contains("outside the folders named") && said.contains("name its folder too") && said.contains("--anywhere") && said.contains("av1opt adopt"), "{said}");
    assert!(original.exists() && output.exists() && !placed.exists());
    // A report named by itself does not either.
    let run = replace_command(&dir, &[dir.join("out/film.av1opt.json").as_os_str(), apply]);
    assert!(String::from_utf8_lossy(&run.stdout).contains("outside the folders named"));
    assert!(original.exists());

    // Named as well, it does, and what was done is on record.
    let run = replace_command(&dir, &[out.as_os_str(), videos.as_os_str(), apply]);
    assert!(run.status.success() && placed.exists() && !original.exists(), "{}", String::from_utf8_lossy(&run.stdout));
    let record = std::fs::read_to_string(dir.join("data/av1opt/ledger.jsonl")).unwrap();
    assert!(record.contains("\"event\":\"replaced\""), "{record}");

    // Without that record, putting back follows the same rule.
    std::fs::remove_file(dir.join("data/av1opt/ledger.jsonl")).unwrap();
    let run = replace_command(&dir, &[out.as_os_str(), undo, apply]);
    assert!(String::from_utf8_lossy(&run.stdout).contains("outside the folders named"), "{}", String::from_utf8_lossy(&run.stdout));
    assert!(placed.exists() && !original.exists());
    let run = replace_command(&dir, &[out.as_os_str(), undo, apply, anywhere]);
    assert!(run.status.success() && original.exists() && output.exists() && !placed.exists(), "{}", String::from_utf8_lossy(&run.stdout));

    // Asked for, a report is followed wherever it leads.
    let run = replace_command(&dir, &[out.as_os_str(), apply, anywhere]);
    assert!(run.status.success() && placed.exists() && !original.exists(), "{}", String::from_utf8_lossy(&run.stdout));
}

/// A folder for the commands, with an output and its report as an encode left them.
fn command_folder(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    for folder in ["videos", "out", "data/av1opt"] {
        std::fs::create_dir_all(dir.join(folder)).unwrap();
    }
    std::fs::set_permissions(dir.join("data/av1opt"), std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let (original, output) = (dir.join("videos/film.mp4"), dir.join("out/film.mkv"));
    std::fs::write(&original, vec![b's'; 3000]).unwrap();
    std::fs::write(&output, vec![b'o'; 1000]).unwrap();
    write_report(&dir.join("out/film.av1opt.json"), json!({ "format": 1, "input": original, "output": output, "source_bytes": 3000, "output_bytes": 1000 }));
    (dir, original, output)
}

#[test]
fn the_command_follows_a_pair_on_record_without_being_told_where_the_original_is() {
    let (dir, original, output) = command_folder("ui-replace-on-record");
    let placed = dir.join("videos/film.mkv");
    write_record(&dir, &[encoded_line(&original, &output)]);
    let (out, apply, undo) = (dir.join("out"), std::ffi::OsStr::new("--apply"), std::ffi::OsStr::new("--undo"));
    let run = replace_command(&dir, &[out.as_os_str(), apply]);
    assert!(run.status.success() && placed.exists() && !original.exists(), "{}", out_of(&run));
    let run = replace_command(&dir, &[out.as_os_str(), undo, apply]);
    assert!(run.status.success() && original.exists() && output.exists() && !placed.exists(), "{}", out_of(&run));

    // The output is compared whole before it moves: the record speaks for the file it saw.
    std::fs::write(&output, vec![b'e'; 1000]).unwrap();
    let run = replace_command(&dir, &[out.as_os_str(), apply]);
    let said = out_of(&run);
    assert!(!run.status.success() && said.contains("outside the folders named") && said.contains("contents differ"), "{said}");
    assert!(original.exists() && output.exists() && !placed.exists());
}

#[test]
fn outputs_from_before_the_record_are_adopted_and_then_work_on_the_page() {
    let s = Server::start("adopt", |d| {
        std::fs::write(d.join("videos/movie.mp4"), vec![b's'; 3000]).unwrap();
        std::fs::create_dir_all(d.join("out")).unwrap();
        std::fs::write(d.join("out/movie.mkv"), vec![b'o'; 1000]).unwrap();
        write_report(&d.join("out/movie.av1opt.json"), json!({ "format": 1, "input": d.join("videos/movie.mp4"), "output": d.join("out/movie.mkv"), "source_bytes": 3000, "output_bytes": 1000 }));
    });
    add_history_folder(&s, "out");
    let (original, output, placed, report) = (s.dir.join("videos/movie.mp4"), s.dir.join("out/movie.mkv"), s.dir.join("videos/movie.mkv"), s.dir.join("out/movie.av1opt.json"));
    let out = s.dir.join("out");
    let apply = std::ffi::OsStr::new("--apply");

    let row = s.get("/api/history").json()["reports"][0].clone();
    assert_eq!(row["replace"]["state"], json!("not_recorded"), "{row}");
    assert!(row["replace"]["why"].as_str().unwrap().contains("`av1opt adopt`"), "{row}");
    let r = s.post("/api/replace", &json!({ "report": report }));
    assert!(r.status == 400 && r.body.contains("no record of making this output"), "{}", r.body);

    // Listing writes nothing.
    let record = s.dir.join("data/av1opt/ledger.jsonl");
    let before = std::fs::read(&record).unwrap_or_default();
    let run = adopt_command(&s.dir, &[out.as_os_str()]);
    let said = out_of(&run);
    assert!(run.status.success() && said.contains("you trust the reports") && said.contains("1 output can go on record") && said.contains("movie.mkv") && said.contains("movie.mp4") && said.contains("--apply"), "{said}");
    assert_eq!(std::fs::read(&record).unwrap_or_default(), before);
    assert_eq!(s.post("/api/replace", &json!({ "report": report })).status, 400);

    let run = adopt_command(&s.dir, &[out.as_os_str(), apply]);
    assert!(run.status.success(), "{}", out_of(&run));
    assert!(std::fs::read_to_string(&record).unwrap().contains(&sha256(&output)));
    assert_eq!(s.get("/api/history").json()["reports"][0]["replace"]["state"], json!("ready"));
    let run = adopt_command(&s.dir, &[out.as_os_str(), apply]);
    assert!(out_of(&run).contains("1 output is on record already") && out_of(&run).contains("0 outputs taken on record"), "{}", out_of(&run));

    let r = s.post("/api/replace", &json!({ "report": report }));
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(placed.exists() && !original.exists() && !output.exists());
    let r = s.post("/api/replace/undo", &json!({ "report": report }));
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(std::fs::read(&original).unwrap(), vec![b's'; 3000]);
    assert_eq!(std::fs::read(&output).unwrap(), vec![b'o'; 1000]);
    assert!(!placed.exists());
}

#[test]
fn an_output_that_already_replaced_its_original_is_adopted_and_can_be_put_back() {
    let s = Server::start("adopt-replaced", |d| {
        std::fs::write(d.join("videos/movie.mp4"), vec![b's'; 3000]).unwrap();
        std::fs::create_dir_all(d.join("out")).unwrap();
        std::fs::write(d.join("out/movie.mkv"), vec![b'o'; 1000]).unwrap();
        write_report(&d.join("out/movie.av1opt.json"), json!({ "format": 1, "input": d.join("videos/movie.mp4"), "output": d.join("out/movie.mkv"), "source_bytes": 3000, "output_bytes": 1000 }));
        let run = replace_command(d, &[d.join("out").as_os_str(), d.join("videos").as_os_str(), std::ffi::OsStr::new("--apply")]);
        assert!(run.status.success(), "{}", out_of(&run));
        // As it was before the record began.
        std::fs::remove_file(d.join("data/av1opt/ledger.jsonl")).unwrap();
    });
    add_history_folder(&s, "out");
    let (original, output, placed, report) = (s.dir.join("videos/movie.mp4"), s.dir.join("out/movie.mkv"), s.dir.join("videos/movie.mkv"), s.dir.join("out/movie.av1opt.json"));
    let out = s.dir.join("out");
    assert!(placed.exists() && !original.exists());
    assert_eq!(s.post("/api/replace/undo", &json!({ "report": report })).status, 400);

    let run = adopt_command(&s.dir, &[out.as_os_str()]);
    assert!(out_of(&run).contains("1 output that already took the original's place"), "{}", out_of(&run));
    let run = adopt_command(&s.dir, &[out.as_os_str(), std::ffi::OsStr::new("--apply")]);
    assert!(run.status.success() && out_of(&run).contains("1 output taken on record"), "{}", out_of(&run));
    let r = s.post("/api/replace/undo", &json!({ "report": report }));
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(std::fs::read(&original).unwrap(), vec![b's'; 3000]);
    assert!(output.exists() && !placed.exists());

    // Back where it began, it can take the original's place again.
    let r = s.post("/api/replace", &json!({ "report": report }));
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(placed.exists() && !original.exists());
}

#[test]
fn adopting_closes_a_data_folder_an_earlier_version_left_open() {
    let (dir, _, _) = command_folder("ui-adopt-open-folder");
    let folder = dir.join("data/av1opt");
    std::fs::set_permissions(&folder, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let run = adopt_command(&dir, &[dir.join("out").as_os_str(), std::ffi::OsStr::new("--apply")]);
    assert!(run.status.success() && out_of(&run).contains("1 output taken on record"), "{}", out_of(&run));
    assert_eq!(std::os::unix::fs::MetadataExt::mode(&std::fs::metadata(&folder).unwrap()) & 0o777, 0o700);
}

#[test]
fn reports_that_do_not_check_out_are_not_adopted() {
    let (dir, original, output) = command_folder("ui-adopt-refused");
    let (out, apply) = (dir.join("out"), std::ffi::OsStr::new("--apply"));
    let record = dir.join("data/av1opt/ledger.jsonl");
    let adopted = || std::fs::read_to_string(&record).unwrap_or_default().contains("\"adopted\"");

    // An original that is not the size the report says.
    std::fs::write(&original, vec![b's'; 2000]).unwrap();
    let said = out_of(&adopt_command(&dir, &[out.as_os_str(), apply]));
    assert!(said.contains("1 report refused") && said.contains("the original changed since the encode"), "{said}");
    assert!(!adopted());

    // Of that size, but not the file its hash belongs to.
    std::fs::write(&original, vec![b's'; 3000]).unwrap();
    write_report(&dir.join("out/film.av1opt.json"), json!({ "format": 1, "input": original, "output": output, "source_bytes": 3000, "output_bytes": 1000, "source_mtime": 1000, "source_hash": "0123456789abcdef" }));
    let said = out_of(&adopt_command(&dir, &[out.as_os_str(), apply]));
    assert!(said.contains("the original changed since the encode") && !adopted(), "{said}");

    // Neither a link nor a pipe is a report.
    let report = dir.join("out/film.av1opt.json");
    let real = dir.join("real-report");
    std::fs::rename(&report, &real).unwrap();
    std::os::unix::fs::symlink(&real, &report).unwrap();
    let said = out_of(&adopt_command(&dir, &[out.as_os_str(), apply]));
    assert!(said.contains("the report is a link") && !adopted(), "{said}");
    std::fs::remove_file(&report).unwrap();
    let made = std::process::Command::new("mkfifo").arg(&report).status().unwrap();
    assert!(made.success());
    let said = out_of(&adopt_command(&dir, &[out.as_os_str(), apply]));
    assert!(said.contains("the report is not a plain file") && !adopted(), "{said}");
    std::fs::remove_file(&report).unwrap();

    // A file on record is not given another past by a report.
    write_report(&report, json!({ "format": 1, "input": original, "output": output, "source_bytes": 3000, "output_bytes": 1000 }));
    let other = dir.join("videos/other.mp4");
    std::fs::write(&other, vec![b't'; 3000]).unwrap();
    write_record(&dir, &[encoded_line(&other, &output)]);
    let said = out_of(&adopt_command(&dir, &[out.as_os_str(), apply]));
    assert!(said.contains("came from another original") && !adopted(), "{said}");
    std::fs::remove_file(&record).unwrap();

    // A replacement whose original has left the trash has nothing left to put back.
    let run = replace_command(&dir, &[dir.as_os_str(), apply]);
    assert!(run.status.success(), "{}", out_of(&run));
    std::fs::remove_file(&record).unwrap();
    std::fs::remove_file(dir.join("data/Trash/files/film.mp4")).unwrap();
    let said = out_of(&adopt_command(&dir, &[out.as_os_str(), apply]));
    assert!(said.contains("the original is no longer in the trash") && !adopted(), "{said}");
}

#[test]
fn queue_pause_and_hours_are_kept_and_checked() {
    let mut s = Server::start("hold", |_| {});
    let q = s.get("/api/jobs").json()["queue"].clone();
    assert_eq!(q, json!({ "hold": null, "paused": false, "schedule": { "on": false, "from": "23:00", "to": "07:00" } }));
    assert_eq!(s.get("/api/jobs/heads").json()["queue"], q);

    // The pause switch.
    assert_eq!(s.post("/api/queue/pause", &json!({})).status, 400);
    let r = s.post("/api/queue/pause", &json!({ "paused": true }));
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["hold"], json!({ "why": "paused" }));
    assert_eq!(s.get("/api/jobs/heads").json()["queue"]["paused"], true);

    // Hours: times of day only, and not an empty span.
    for bad in [json!({ "on": true, "from": "25:00", "to": "07:00" }), json!({ "on": true, "from": "10:00", "to": "10:00" }), json!({ "on": true, "from": "10:00" }), json!({ "from": "10:00", "to": "11:00" })] {
        let r = s.post("/api/queue/schedule", &bad);
        assert_eq!(r.status, 400, "{bad}: {}", r.body);
    }
    // Hours that leave out the present minute whenever the test runs: they begin in twelve hours.
    let minute = {
        let t = unsafe { libc::time(std::ptr::null_mut()) };
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe { libc::localtime_r(&t, &mut tm) };
        (tm.tm_hour * 60 + tm.tm_min) as u32
    };
    let clock = |m: u32| format!("{:02}:{:02}", m / 60 % 24, m % 60);
    let (from, to) = (clock(minute + 12 * 60), clock(minute + 13 * 60));
    let r = s.post("/api/queue/schedule", &json!({ "on": true, "from": from, "to": to }));
    assert_eq!(r.status, 200, "{}", r.body);
    // The pause still counts first.
    assert_eq!(r.json()["hold"]["why"], "paused");
    assert_eq!(r.json()["schedule"], json!({ "on": true, "from": from, "to": to }));

    // Both survive a restart.
    assert!(s.terminate());
    let s2 = Server::start_in(&s.dir, "false", "false");
    let q = s2.get("/api/jobs").json()["queue"].clone();
    assert_eq!((q["paused"].as_bool(), &q["schedule"]), (Some(true), &json!({ "on": true, "from": from, "to": to })), "{q}");

    // Resumed: the hours hold it now, until they begin.
    let r = s2.post("/api/queue/pause", &json!({ "paused": false })).json();
    assert_eq!(r["hold"]["why"], "hours", "{r}");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let wait = r["hold"]["opens"].as_u64().unwrap() - now;
    assert!((12 * 3600 - 120..=12 * 3600).contains(&wait), "{wait}");
    // "Run now" lifts it; saving the hours again brings it back; switching them off ends it.
    assert_eq!(s2.post("/api/queue/run-now", &json!({})).json()["hold"], Value::Null);
    assert_eq!(s2.get("/api/jobs/heads").json()["queue"]["hold"], Value::Null);
    assert_eq!(s2.post("/api/queue/schedule", &json!({ "on": true, "from": from, "to": to })).json()["hold"]["why"], "hours");
    assert_eq!(s2.post("/api/queue/schedule", &json!({ "on": false, "from": from, "to": to })).json()["hold"], Value::Null);
}

#[test]
fn waiting_jobs_can_be_moved_first_and_stopped_ones_queued_again() {
    // Three saved jobs; with no usable ffmpeg they all wait, so their order can be looked at.
    let s = Server::start("order", |d| {
        let job = |name: &str| {
            std::fs::write(d.join("videos").join(name), b"x").unwrap();
            json!({ "inputs": [d.join("videos").join(name)], "roots": [], "out_dir": d.join("out"), "options": {} })
        };
        let saved = json!({ "format": 1, "jobs": [job("a.mkv"), job("b.mkv"), job("c.mkv")] });
        std::fs::write(d.join("data/av1opt/ui-queue.json"), saved.to_string()).unwrap();
    });
    let ids = |s: &Server| -> Vec<u64> { s.get("/api/jobs/heads").json()["jobs"].as_array().unwrap().iter().map(|j| j["id"].as_u64().unwrap()).collect() };
    let order = ids(&s);
    assert_eq!(order.len(), 3);
    let (a, b, c) = (order[0], order[1], order[2]);

    assert_eq!(s.post(&format!("/api/jobs/{c}/first"), &json!({})).status, 200);
    assert_eq!(ids(&s), [c, a, b]);
    // The first one stays where it is, and a job that is not there is said so.
    assert_eq!(s.post(&format!("/api/jobs/{c}/first"), &json!({})).status, 200);
    assert_eq!(ids(&s), [c, a, b]);
    assert_eq!(s.post("/api/jobs/999/first", &json!({})).status, 404);

    // The saved queue has the new order.
    let q: Value = serde_json::from_slice(&std::fs::read(s.data("ui-queue.json")).unwrap()).unwrap();
    let names: Vec<String> = q["jobs"].as_array().unwrap().iter().map(|j| j["inputs"][0].as_str().unwrap().rsplit('/').next().unwrap().to_string()).collect();
    assert_eq!(names, ["c.mkv", "a.mkv", "b.mkv"]);

    // A stopped job is not moved; a waiting one is ahead of the stopped ones only by status.
    assert_eq!(s.post(&format!("/api/jobs/{a}/cancel"), &json!({})).status, 200);
    let r = s.post(&format!("/api/jobs/{a}/first"), &json!({}));
    assert_eq!(r.status, 400, "{}", r.body);
    assert_eq!(s.post(&format!("/api/jobs/{b}/first"), &json!({})).status, 200);
    assert_eq!(ids(&s), [b, c, a]);

    // Only stopped jobs are queued again, and a refusal (no ffmpeg here) keeps the entry.
    let r = s.post(&format!("/api/jobs/{b}/again"), &json!({}));
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.json()["error"].as_str().unwrap_or("").contains("cancelled or failed"), "{}", r.body);
    let r = s.post(&format!("/api/jobs/{a}/again"), &json!({}));
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.json()["error"].as_str().unwrap_or("").contains("Settings"), "{}", r.body);
    assert_eq!(ids(&s), [b, c, a]);
    assert_eq!(s.post("/api/jobs/999/again", &json!({})).status, 404);
}

/// An ffmpeg and ffprobe that pass the checks at start and can do nothing else, so that jobs
/// and watches get as far as what comes before an encode. `on_a_file` is the shell text ffprobe
/// runs when asked about a file.
fn stand_in_tools(name: &str, on_a_file: &str) -> (String, String) {
    stand_in_tools_running(name, on_a_file, "")
}

/// Like [`stand_in_tools`], with `on_a_run` as the shell text ffmpeg runs when asked to work.
fn stand_in_tools_running(name: &str, on_a_file: &str, on_a_run: &str) -> (String, String) {
    let bin = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("tools-{name}"));
    let _ = std::fs::remove_dir_all(&bin);
    std::fs::create_dir_all(&bin).unwrap();
    let ffmpeg = format!("#!/bin/sh\n[ \"$1\" = --ready ] && exit 0\ncase \"$2\" in\n-version) echo 'ffmpeg version 8.0';;\n-encoders) echo ' V....D libsvtav1 SVT-AV1';;\n-filters) echo ' ... libvmaf VV->V VMAF';;\n-nostdin) {on_a_run}\n;;\nesac\n");
    let ffprobe = format!("#!/bin/sh\n[ \"$1\" = --ready ] && exit 0\nif [ \"$2\" = -version ]; then echo 'ffprobe version 8.0'; exit 0; fi\n{on_a_file}\n");
    let write = |file: &str, text: &str| {
        use std::os::unix::fs::PermissionsExt;
        let path = bin.join(file);
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        // While it was open for writing, a program this process started at that moment may
        // have got a copy of it, and until that program is under way the system answers
        // `Text file busy`. Once it runs here it runs for the server too.
        loop {
            match std::process::Command::new(&path).arg("--ready").status() {
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => std::thread::sleep(Duration::from_millis(5)),
                done => {
                    done.unwrap();
                    break;
                }
            }
        }
        path.to_str().unwrap().to_string()
    };
    (write("ffmpeg", &ffmpeg), write("ffprobe", &ffprobe))
}

#[test]
fn requests_from_other_pages_are_refused_and_every_answer_is_hardened() {
    use std::io::{Read, Write};
    let (ffmpeg, ffprobe) = stand_in_tools("fetch-site", A_VIDEO);
    let s = Server::start_with_tools("fetch-site", &ffmpeg, &ffprobe, |d| std::fs::write(d.join("videos/a.mp4"), b"xx").unwrap());
    let cookie = s.cookie();
    let from = |site: &str, path: &str| s.request("GET", path, &[("Cookie", &cookie), ("Sec-Fetch-Site", site)], None);
    // Another port of this host counts as the same site, and the cookie goes there too.
    for site in ["cross-site", "same-site"] {
        for path in ["/api/state", "/", "/thumb?path=/x&t=1", "/media?path=/x", &format!("/?token={TOKEN}")] {
            let r = from(site, path);
            assert_eq!(r.status, 403, "{site} {path}: {}", r.body);
            assert!(!r.headers.to_ascii_lowercase().contains("set-cookie"), "{site} {path}");
        }
    }
    // The page itself, an address the person opened, and a program that says nothing.
    for site in ["same-origin", "none"] {
        assert_eq!(from(site, "/api/state").status, 200, "{site}");
    }
    assert_eq!(s.get("/api/state").status, 200);
    let signed_in = s.request("GET", &format!("/?token={TOKEN}"), &[("Sec-Fetch-Site", "none")], None);
    assert_eq!(signed_in.status, 303, "{}", signed_in.body);
    assert!(signed_in.headers.contains("Set-Cookie: av1opt_"), "{}", signed_in.headers);

    let check = |what: &str, headers: &str| {
        for wanted in [
            "Cross-Origin-Resource-Policy: same-origin",
            "X-Content-Type-Options: nosniff",
            "Content-Security-Policy: default-src 'self'; media-src 'self' blob:; img-src 'self' data:; style-src 'self'; script-src 'self'; connect-src 'self'",
            "frame-ancestors 'none'",
        ] {
            assert!(headers.contains(wanted), "{what}: no {wanted} in {headers}");
        }
    };
    check("sign-in", &signed_in.headers);
    check("refused", &from("cross-site", "/").headers);
    check("not signed in", &s.request("GET", "/", &[], None).headers);
    for path in ["/", "/app.js", "/style.css", "/api/state", "/no/such/page"] {
        check(path, &s.get(path).headers);
    }
    let clip = s.dir.join("videos/a.mp4");
    assert_eq!(s.post("/api/source", &json!({ "path": clip })).status, 200);
    let media = s.get(&format!("/media?path={}", clip.display()));
    assert_eq!(media.status, 200, "{}", media.body);
    check("media", &media.headers);
    // The event stream stays open, so only its head is read.
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", s.port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(stream, "GET /api/events HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nCookie: {cookie}\r\n\r\n", s.port).unwrap();
    let (mut head, mut byte) = (Vec::new(), [0u8; 1]);
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
    }
    check("events", &String::from_utf8_lossy(&head));
}

#[test]
fn what_the_server_keeps_is_closed_to_other_users() {
    use std::os::unix::fs::PermissionsExt;
    let open = |p: &Path, mode: u32| std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
    let mode = |p: &Path| std::fs::metadata(p).unwrap_or_else(|e| panic!("{}: {e}", p.display())).permissions().mode() & 0o777;
    let (ffmpeg, ffprobe) = stand_in_tools_running("private", A_VIDEO, HANGS_ON_ONE_FILE);
    // As an earlier version left things under a usual umask.
    let s = Server::launch("private", &ffmpeg, &ffprobe, |d| {
        let thumbs = d.join("cache/av1opt/thumbs");
        std::fs::create_dir_all(&thumbs).unwrap();
        std::fs::write(thumbs.join("old.jpg"), b"x").unwrap();
        std::fs::write(d.join("data/av1opt/ui.json"), b"{}").unwrap();
        std::fs::write(d.join("data/av1opt/history.jsonl"), b"").unwrap();
        std::fs::write(d.join("videos/clip.mp4"), b"xx").unwrap();
        open(&thumbs.join("old.jpg"), 0o644);
        open(&thumbs, 0o755);
        for file in ["ui.json", "history.jsonl"] {
            open(&d.join("data/av1opt").join(file), 0o644);
        }
        open(&d.join("data/av1opt"), 0o755);
        open(&d.join("cache/av1opt"), 0o755);
    });
    s.wait_for_restored_jobs();
    let (data, cache) = (s.dir.join("data/av1opt"), s.dir.join("cache/av1opt"));
    for folder in [&data, &cache, &cache.join("thumbs"), &cache.join("previews"), &cache.join(format!("previews/{}", s.child.id()))] {
        assert_eq!(mode(folder), 0o700, "{}", folder.display());
    }
    for file in [data.join("ui.json"), data.join("history.jsonl"), data.join("ui.lock"), cache.join("thumbs/old.jpg")] {
        assert_eq!(mode(&file), 0o600, "{}", file.display());
    }

    // What is written from now on: settings, the queue, a preview and a thumbnail.
    let clip = s.dir.join("videos/clip.mp4");
    assert_eq!(s.post("/api/source", &json!({ "path": clip })).status, 200);
    let r = s.post("/api/queue/pause", &json!({ "paused": true }));
    assert_eq!(r.status, 200, "{}", r.body);
    let r = s.post("/api/preview", &json!({ "path": clip, "kind": "remux" }));
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(s.get(&format!("/thumb?path={}&t=1", clip.display())).status, 200);
    let made: Vec<PathBuf> = [cache.join(format!("previews/{}", s.child.id())), cache.join("thumbs")].iter().flat_map(|d| std::fs::read_dir(d).unwrap().flatten().map(|e| e.path())).collect();
    assert!(made.len() >= 3, "{made:?}");
    for file in made.iter().chain([&data.join("ui.json")]) {
        assert_eq!(mode(file), 0o600, "{}", file.display());
    }
}

#[test]
fn connections_held_open_are_cut_to_the_limit_and_the_server_goes_on() {
    use std::io::{ErrorKind, Read};
    let s = Server::start("connections", |_| {});
    const OPENED: usize = 104;
    let mut idle: Vec<std::net::TcpStream> = (0..OPENED).map(|_| std::net::TcpStream::connect(("127.0.0.1", s.port)).unwrap()).collect();
    for c in &idle {
        c.set_read_timeout(Some(Duration::from_millis(1))).unwrap();
    }
    let started = Instant::now();
    let mut refusals = 0;
    while idle.len() > 64 {
        assert!(started.elapsed() < Duration::from_secs(20), "{} connections are still open", idle.len());
        idle.retain_mut(|c| {
            let mut said = [0u8; 200];
            match c.read(&mut said) {
                Ok(0) => false,
                Ok(n) => {
                    assert!(said[..n].starts_with(b"HTTP/1.1 503"), "{}", String::from_utf8_lossy(&said[..n]));
                    refusals += 1;
                    false
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => true,
                Err(e) => panic!("{e}"),
            }
        });
    }
    assert_eq!(refusals, OPENED - 64);
    // Those within the limit are left alone, and new requests are answered beside them.
    assert_eq!(s.get("/api/state").status, 200);
    std::thread::sleep(Duration::from_millis(1200));
    let still_open = idle
        .iter()
        .filter(|c| {
            let mut reader: &std::net::TcpStream = c;
            matches!(reader.read(&mut [0u8; 1]), Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut))
        })
        .count();
    assert_eq!(still_open, 64);
    assert_eq!(s.get("/api/state").status, 200);
}

/// A stand-in ffmpeg that writes its output, except for a file with `hang` in its name: there
/// it notes its process number beside itself and never ends.
const HANGS_ON_ONE_FILE: &str = "case \"$*\" in\n*hang*) echo $$ > \"$(dirname \"$0\")/hung\"; exec sleep 600;;\nesac\nfor last; do :; done\nprintf x > \"$last\"";

const A_VIDEO: &str = r#"echo '{"streams":[{"index":0,"codec_type":"video","codec_name":"h264","width":64,"height":64,"pix_fmt":"yuv420p","r_frame_rate":"25/1","avg_frame_rate":"25/1","duration":"9"}],"format":{"format_name":"mov","duration":"9","size":"2"}}'"#;

/// A server whose ffmpeg hangs on `hang.mp4`, given `limit` seconds, with both clips listed.
fn server_with_a_clip_that_hangs(name: &str, limit: &str) -> (Server, PathBuf, PathBuf, PathBuf) {
    let (ffmpeg, ffprobe) = stand_in_tools_running(name, A_VIDEO, HANGS_ON_ONE_FILE);
    let hung = Path::new(&ffmpeg).with_file_name("hung");
    let s = Server::launch_with(name, &ffmpeg, &ffprobe, &[("AV1OPT_PREVIEW_LIMIT_SECS", limit)], |d| {
        for clip in ["hang.mp4", "fine.mp4", "other.mp4"] {
            std::fs::write(d.join("videos").join(clip), b"xx").unwrap();
        }
    });
    s.wait_for_restored_jobs();
    let (hangs, fine) = (s.dir.join("videos/hang.mp4"), s.dir.join("videos/fine.mp4"));
    for clip in [&hangs, &fine, &s.dir.join("videos/other.mp4")] {
        let r = s.post("/api/source", &json!({ "path": clip }));
        assert_eq!(r.status, 200, "{}", r.body);
    }
    (s, hangs, fine, hung)
}

/// Waits for the stand-in to be hanging, and returns its process number.
fn hanging_process(hung: &Path) -> String {
    let started = Instant::now();
    loop {
        if let Some(pid) = std::fs::read_to_string(hung).ok().filter(|p| p.ends_with('\n')) {
            return pid.trim().to_string();
        }
        assert!(started.elapsed() < Duration::from_secs(20), "ffmpeg was not started");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_preview_that_hangs_is_killed_and_holds_up_no_other() {
    let (s, hangs, fine, hung) = server_with_a_clip_that_hangs("hung-preview", "4");
    let preview = |clip: &Path| s.post("/api/preview", &json!({ "path": clip, "kind": "remux" }));
    std::thread::scope(|scope| {
        let hanging = scope.spawn(|| preview(&hangs));
        let pid = hanging_process(&hung);
        // While it hangs another clip is made, as the hung one holds only its own lock.
        let r = preview(&fine);
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(!hanging.is_finished(), "the other clip waited for the hung one to be stopped");
        let r = hanging.join().unwrap();
        assert!(r.status >= 400 && r.body.contains("no answer within 4 s"), "{} {}", r.status, r.body);
        assert!(!Path::new("/proc").join(&pid).exists(), "ffmpeg {pid} is still there");
    });
    // Its turn was given back, and nothing of it is left to be taken for a preview.
    let r = preview(&s.dir.join("videos/other.mp4"));
    assert_eq!(r.status, 200, "{}", r.body);
    let left: Vec<String> = std::fs::read_dir(s.dir.join("cache/av1opt/previews")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.contains(".part.")).collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn a_thumbnail_that_hangs_is_killed_and_holds_up_no_other() {
    let (s, hangs, fine, hung) = server_with_a_clip_that_hangs("hung-thumb", "4");
    let thumb = |clip: &Path| s.get(&format!("/thumb?path={}&t=1", clip.display()));
    std::thread::scope(|scope| {
        let hanging = scope.spawn(|| thumb(&hangs));
        let pid = hanging_process(&hung);
        let r = thumb(&fine);
        assert_eq!(r.status, 200, "{}", r.body);
        assert!(!hanging.is_finished());
        assert_ne!(hanging.join().unwrap().status, 200);
        assert!(!Path::new("/proc").join(&pid).exists(), "ffmpeg {pid} is still there");
    });
}

fn saved_queue(s: &Server) -> Value {
    serde_json::from_slice(&std::fs::read(s.data("ui-queue.json")).unwrap()).unwrap()
}

fn saved_jobs(s: &Server) -> usize {
    saved_queue(s)["jobs"].as_array().unwrap().len()
}

fn heads(s: &Server) -> Vec<Value> {
    s.get("/api/jobs/heads").json()["jobs"].as_array().unwrap().clone()
}

/// Waits for the first job to have ended, and returns it.
fn first_job_ended(s: &Server) -> Value {
    let started = Instant::now();
    loop {
        let job = heads(s)[0].clone();
        if !matches!(job["status"].as_str(), Some("queued" | "running")) {
            return job;
        }
        assert!(started.elapsed() < Duration::from_secs(20), "still going: {job}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_long_saved_queue_comes_back_behind_a_server_that_already_answers() {
    const JOBS: usize = 30_000;
    let write_queue = |d: &Path| {
        std::fs::write(d.join("videos/a.mkv"), b"x").unwrap();
        let job = json!({ "inputs": [d.join("videos/a.mkv")], "roots": [], "out_dir": d.join("out"), "options": {} });
        let saved = json!({ "format": 1, "jobs": vec![job; JOBS] });
        std::fs::write(d.join("data/av1opt/ui-queue.json"), saved.to_string()).unwrap();
    };
    let mut s = Server::launch("restoring", "false", "false", write_queue);

    // Answering at once, with the page told what is still to come.
    let list = s.get("/api/jobs/heads").json();
    assert!(list["restoring"].as_u64().unwrap() > 0, "the queue was back before the first answer: make it longer");
    // A change made meanwhile is saved with all that has not come back yet.
    let started = Instant::now();
    while heads(&s).is_empty() {
        assert!(started.elapsed() < Duration::from_secs(20));
    }
    let id = heads(&s)[0]["id"].as_u64().unwrap();
    assert_eq!(s.post(&format!("/api/jobs/{id}/remove"), &json!({})).status, 200);
    assert_eq!(saved_jobs(&s), JOBS - 1);

    // Stopped half way, as the desktop app stops a server it has given up on: nothing is lost.
    assert!(s.get("/api/jobs/heads").json()["restoring"].as_u64().unwrap() > 0, "restored too soon to tell");
    assert!(s.terminate());
    assert_eq!(saved_jobs(&s), JOBS - 1);
    let dir = s.dir.clone();
    drop(s);

    // Killed half way: the same.
    let keep = dir.with_extension("again");
    let _ = std::fs::remove_dir_all(&keep);
    std::fs::rename(&dir, &keep).unwrap();
    let s = Server::launch("restoring", "false", "false", |d| {
        std::fs::remove_dir_all(d).unwrap();
        std::fs::rename(&keep, d).unwrap();
    });
    drop(s);
    let s = Server::start_in(&dir, "false", "false");
    assert_eq!(heads(&s).len(), JOBS - 1);
    assert_eq!(s.get("/api/jobs/heads").json()["restoring"], 0);
    assert_eq!(saved_jobs(&s), JOBS - 1);
}

#[test]
fn inputs_that_are_gone_are_left_out_of_a_saved_job_and_named() {
    let (ffmpeg, ffprobe) = stand_in_tools("gone", "exit 1");
    let mut s = Server::start_with_tools("gone", &ffmpeg, &ffprobe, |d| {
        std::fs::write(d.join("videos/a.mkv"), b"x").unwrap();
        std::fs::create_dir_all(d.join("out")).unwrap();
        std::fs::write(d.join("data/av1opt/ui.json"), json!({ "queue_paused": true }).to_string()).unwrap();
        let jobs = json!([
            { "inputs": [d.join("videos/a.mkv"), d.join("videos/b.mkv")], "roots": [], "out_dir": d.join("out"), "options": {} },
            { "inputs": [d.join("videos/c.mkv"), d.join("videos/e.mkv")], "roots": [], "out_dir": d.join("out"), "options": {} },
        ]);
        std::fs::write(d.join("data/av1opt/ui-queue.json"), json!({ "format": 1, "jobs": jobs }).to_string()).unwrap();
    });
    let videos = s.dir.join("videos");
    let job = heads(&s)[0].clone();
    assert_eq!(job["total"], 1, "{job}");
    let note = job["note"].as_str().unwrap_or_default();
    assert!(note.contains("b.mkv") && !note.contains("a.mkv"), "{note}");
    // All of its inputs gone: kept aside as before, and one back is enough to retry it.
    assert_eq!(s.get("/api/jobs").json()["unrestored"].as_array().unwrap().len(), 1);
    std::fs::write(videos.join("c.mkv"), b"x").unwrap();
    let r = s.post("/api/jobs/unrestored/retry", &json!({})).json();
    assert_eq!(r["restored"], 1, "{r}");
    let retried = heads(&s)[1].clone();
    assert_eq!(retried["total"], 1, "{retried}");
    assert!(retried["note"].as_str().unwrap_or_default().contains("e.mkv"), "{retried}");
    assert_eq!(s.post(&format!("/api/jobs/{}/remove", retried["id"]), &json!({})).status, 200);

    // Queued again while the missing one is back: whole, with nothing to say.
    let again = |s: &Server| {
        let id = heads(s)[0]["id"].as_u64().unwrap();
        assert_eq!(s.post(&format!("/api/jobs/{id}/cancel"), &json!({})).status, 200);
        let r = s.post(&format!("/api/jobs/{id}/again"), &json!({}));
        (r.status, r.body, heads(s))
    };
    std::fs::write(videos.join("b.mkv"), b"x").unwrap();
    let (status, body, jobs) = again(&s);
    assert_eq!(status, 200, "{body}");
    assert_eq!((jobs.len(), jobs[0]["total"].as_u64(), jobs[0]["note"].is_null()), (1, Some(2), true), "{jobs:?}");

    // Queued again with one of them gone: the rest goes, and the saved job remembers it.
    std::fs::remove_file(videos.join("b.mkv")).unwrap();
    let (status, body, jobs) = again(&s);
    assert_eq!(status, 200, "{body}");
    assert_eq!(jobs[0]["total"], 1, "{jobs:?}");
    assert!(jobs[0]["note"].as_str().unwrap_or_default().contains("b.mkv"), "{jobs:?}");
    assert!(saved_queue(&s)["jobs"][0]["dropped"][0].as_str().unwrap().ends_with("b.mkv"));

    // Back by the next start, it is in the job again.
    assert!(s.terminate());
    std::fs::write(videos.join("b.mkv"), b"x").unwrap();
    let s = Server::start_in(&s.dir, &ffmpeg, &ffprobe);
    let jobs = heads(&s);
    assert_eq!((jobs[0]["total"].as_u64(), jobs[0]["note"].is_null()), (Some(2), true), "{jobs:?}");

    // With nothing left of it the job is refused, and its entry stays.
    std::fs::remove_file(videos.join("a.mkv")).unwrap();
    std::fs::remove_file(videos.join("b.mkv")).unwrap();
    let (status, body, jobs) = again(&s);
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("does not exist"), "{body}");
    assert_eq!(jobs.len(), 1);
}

#[test]
fn a_missing_output_folder_is_not_made_for_a_job_nobody_just_asked_for() {
    let (ffmpeg, ffprobe) = stand_in_tools("drive", "exit 1");
    let mut s = Server::start_with_tools("drive", &ffmpeg, &ffprobe, |d| {
        std::fs::write(d.join("videos/a.mkv"), b"x").unwrap();
        let job = json!({ "inputs": [d.join("videos/a.mkv")], "roots": [], "out_dir": d.join("drive/out"), "options": {}, "out_dir_made": true });
        std::fs::write(d.join("data/av1opt/ui-queue.json"), json!({ "format": 1, "jobs": [job] }).to_string()).unwrap();
    });
    let (input, away) = (s.dir.join("videos/a.mkv"), s.dir.join("drive/out"));
    let job = first_job_ended(&s);
    assert_eq!(job["status"], "failed", "{job}");
    let error = job["error"].as_str().unwrap();
    assert!(error.contains("is not there") && error.contains("may not be mounted"), "{error}");
    assert!(!s.dir.join("drive").exists());
    // The job waits in the saved queue for the drive.
    assert_eq!(saved_jobs(&s), 1);
    assert!(s.terminate());
    let s = Server::start_in(&s.dir, &ffmpeg, &ffprobe);
    assert_eq!(first_job_ended(&s)["status"], "failed");
    assert_eq!(saved_jobs(&s), 1);
    // Clearing what is finished leaves it: it is waiting, not over.
    assert_eq!(s.post("/api/jobs/clear", &json!({})).status, 200);
    assert_eq!((heads(&s).len(), saved_jobs(&s)), (1, 1));
    // Taken off the list, it is gone for good.
    let id = heads(&s)[0]["id"].as_u64().unwrap();
    assert_eq!(s.post(&format!("/api/jobs/{id}/remove"), &json!({})).status, 200);
    assert_eq!(saved_jobs(&s), 0);

    // A watch's job is held to the same.
    let r = s.post("/api/jobs", &json!({ "inputs": [input], "roots": [], "out_dir": away, "watch": s.dir.join("videos") }));
    assert_eq!(r.status, 200, "{}", r.body);
    let job = first_job_ended(&s);
    assert!(job["error"].as_str().unwrap_or_default().contains("is not there"), "{job}");
    assert!(!s.dir.join("drive").exists());
    assert_eq!(s.post(&format!("/api/jobs/{}/remove", job["id"]), &json!({})).status, 200);

    // A folder asked for by hand is made, as it always was.
    let r = s.post("/api/jobs", &json!({ "inputs": [input], "roots": [], "out_dir": away }));
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(away.is_dir());
}

#[test]
fn a_second_server_on_the_same_folders_is_refused() {
    let s = Server::start("only-one", |d| {
        std::fs::write(d.join("videos/a.mkv"), b"x").unwrap();
        let job = json!({ "inputs": [d.join("videos/a.mkv")], "roots": [], "out_dir": d.join("out"), "options": {} });
        std::fs::write(d.join("data/av1opt/ui-queue.json"), json!({ "format": 1, "jobs": [job] }).to_string()).unwrap();
    });
    let preview = s.dir.join("cache/av1opt/previews/playing.mp4");
    std::fs::write(&preview, b"x").unwrap();
    let queue = std::fs::read(s.data("ui-queue.json")).unwrap();

    let mut second = Server::command(&s.dir, "false", "false").stderr(std::process::Stdio::piped()).spawn().unwrap();
    let started = Instant::now();
    while second.try_wait().unwrap().is_none() {
        if started.elapsed() > Duration::from_secs(10) {
            second.kill().unwrap();
            second.wait().unwrap();
            panic!("a second server started on the folders of the first");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let second = second.wait_with_output().unwrap();
    let said = String::from_utf8_lossy(&second.stderr);
    assert!(!second.status.success(), "{said}");
    assert!(said.contains(&format!("already running for this user (process {})", s.child.id())), "{said}");
    assert!(preview.exists(), "the second start wiped the previews of the first");
    assert_eq!(std::fs::read(s.data("ui-queue.json")).unwrap(), queue);
    assert_eq!(heads(&s).len(), 1);

    // However the first one ended, the place is free again.
    let dir = s.dir.clone();
    drop(s);
    let s = Server::start_in(&dir, "false", "false");
    assert_eq!(heads(&s).len(), 1);
}

#[test]
fn files_that_cannot_be_read_are_kept_and_said_and_the_queue_starts_paused() {
    let aside = |s: &Server, start: &str| -> Vec<String> {
        let names = std::fs::read_dir(s.data("")).unwrap().flatten();
        names.map(|e| e.path()).filter(|p| p.file_name().unwrap().to_string_lossy().starts_with(start)).map(|p| std::fs::read_to_string(p).unwrap()).collect()
    };
    let mut s = Server::start("unreadable", |d| {
        std::fs::write(d.join("data/av1opt/ui.json"), "{ the settings").unwrap();
        std::fs::write(d.join("data/av1opt/ui-queue.json"), "{ the queue").unwrap();
    });
    let notices = s.get("/api/state").json()["notices"].clone();
    let text = notices.to_string();
    assert_eq!(notices.as_array().unwrap().len(), 2, "{notices}");
    assert!(text.contains("the settings file could not be read") && text.contains("the saved queue could not be read"), "{text}");
    assert!(text.contains("ui.unreadable-") && text.contains("ui-queue.unreadable-"), "{text}");
    assert_eq!(aside(&s, "ui.unreadable-"), ["{ the settings"]);
    assert_eq!(aside(&s, "ui-queue.unreadable-"), ["{ the queue"]);
    assert_eq!(s.get("/api/jobs/heads").json()["queue"]["paused"], true);

    // The pause is saved like any other, and there is nothing more to say next time.
    assert!(s.terminate());
    let mut s = Server::start_in(&s.dir, "false", "false");
    assert_eq!(s.get("/api/jobs/heads").json()["queue"]["paused"], true);
    assert_eq!(s.get("/api/state").json()["notices"], json!([]));

    // A queue of the wrong shape is no queue either, and says nothing of the pause.
    assert_eq!(s.post("/api/queue/pause", &json!({ "paused": false })).status, 200);
    assert!(s.terminate());
    std::fs::write(s.data("ui-queue.json"), r#"{"format":1}"#).unwrap();
    let s = Server::start_in(&s.dir, "false", "false");
    assert_eq!(s.get("/api/state").json()["notices"].as_array().unwrap().len(), 1);
    assert_eq!(aside(&s, "ui-queue.unreadable-").len(), 2);
    assert_eq!(s.get("/api/jobs/heads").json()["queue"]["paused"], false);
}

#[test]
fn the_queue_is_saved_whatever_lies_where_a_temporary_file_once_went() {
    let s = Server::start("saving", |d| {
        // The fixed name saves used to go through.
        std::fs::create_dir_all(d.join("data/av1opt/ui-queue.json.tmp")).unwrap();
        let job = |name: &str| {
            std::fs::write(d.join("videos").join(name), b"x").unwrap();
            json!({ "inputs": [d.join("videos").join(name)], "roots": [], "out_dir": d.join("out"), "options": {} })
        };
        std::fs::write(d.join("data/av1opt/ui-queue.json"), json!({ "format": 1, "jobs": [job("a.mkv"), job("b.mkv")] }).to_string()).unwrap();
    });
    let last = heads(&s)[1]["id"].as_u64().unwrap();
    assert_eq!(s.post(&format!("/api/jobs/{last}/first"), &json!({})).status, 200);
    assert!(saved_queue(&s)["jobs"][0]["inputs"][0].as_str().unwrap().ends_with("b.mkv"));
    // No temporary file of a save stays behind.
    let names: Vec<String> = std::fs::read_dir(s.data("")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with("ui-queue") || n.starts_with(".ui-queue")).collect();
    assert_eq!(names.len(), 2, "{names:?}");
}

#[test]
fn a_probe_that_was_cut_short_is_tried_again_and_a_rejected_file_is_not() {
    let (ffmpeg, ffprobe) = stand_in_tools("probes", "case \"$*\" in\n*rejected*) echo 'Invalid data found when processing input' >&2; exit 1;;\n*) kill -9 $$;;\nesac");
    let s = Server::start_with_tools("probes", &ffmpeg, &ffprobe, |d| std::fs::create_dir_all(d.join("videos/in")).unwrap());
    let dir = s.dir.join("videos/in");
    let r = s.post("/api/watch", &json!({ "path": dir, "out_dir": s.dir.join("out"), "options": {} }));
    assert_eq!(r.status, 200, "{}", r.body);
    let id = r.json()["id"].as_u64().unwrap();
    let watch = || s.post(&format!("/api/watch/{id}/look"), &json!({})).json()["watches"][0].clone();
    let look = || {
        let w = watch();
        (w["fresh"].as_u64().unwrap(), w["left"].as_u64().unwrap(), w["unread"].as_u64().unwrap())
    };
    look();
    for name in ["rejected.mp4", "cut-short.mp4"] {
        std::fs::write(dir.join(name), b"xx").unwrap();
        age(&dir.join(name), 600);
    }
    assert_eq!(look(), (2, 0, 0), "seen, and not yet quiet for two looks");
    assert_eq!(look(), (0, 1, 1), "the rejected one is left at once, the other tried again");
    assert_eq!(look(), (0, 1, 1));
    assert_eq!(look(), (0, 1, 1), "three tries, and it is still not given up on");
    // The page is told what is true of it, and looking by hand tries it at once.
    let w = watch();
    let unread = &w["unread_files"][0];
    assert!(w["error"].is_null(), "{w}");
    assert!(unread["path"].as_str().unwrap().ends_with("cut-short.mp4"), "{w}");
    assert_eq!(unread["tries"], 4, "{w}");
    assert!(!unread["why"].as_str().unwrap_or_default().is_empty(), "{w}");
    assert!(unread["next_secs"].as_u64().unwrap() >= 1000, "the wait grows: {w}");
}

/// Waits for something the server does just after what a test saw.
fn soon(what: &str, done: impl Fn() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < Duration::from_secs(20), "not so after twenty seconds: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn paused(d: &Path) {
    std::fs::write(d.join("data/av1opt/ui.json"), json!({ "queue_paused": true }).to_string()).unwrap();
}

fn write_saved_queue(d: &Path, jobs: Value) {
    std::fs::write(d.join("data/av1opt/ui-queue.json"), json!({ "format": 1, "jobs": jobs }).to_string()).unwrap();
}

/// Where the first job's files go.
fn outputs(s: &Server) -> Vec<PathBuf> {
    let list = s.get("/api/jobs").json();
    list["jobs"][0]["files"].as_array().unwrap().iter().map(|f| PathBuf::from(f["output"].as_str().unwrap())).collect()
}

#[test]
fn inputs_left_out_of_a_job_outlive_it() {
    let (ffmpeg, ffprobe) = stand_in_tools("outlive", "exit 1");
    let mut s = Server::start_with_tools("outlive", &ffmpeg, &ffprobe, |d| {
        std::fs::write(d.join("videos/a.mkv"), b"x").unwrap();
        std::fs::create_dir_all(d.join("out")).unwrap();
        write_saved_queue(d, json!([{ "inputs": [d.join("videos/a.mkv"), d.join("videos/b.mkv")], "roots": [], "out_dir": d.join("out"), "options": { "preset": 9 }, "out_dir_made": true }]));
    });
    let job = first_job_ended(&s);
    assert_eq!(job["status"], "done", "{job}");
    assert!(job["note"].is_null(), "they are not this job's any more: {job}");
    // Saved on their own, with the settings of the job, and shown with the way to forget them.
    soon("the left-out input is saved", || saved_jobs(&s) == 1);
    let saved = saved_queue(&s)["jobs"][0].clone();
    assert_eq!(saved["inputs"], json!([s.dir.join("videos/b.mkv")]), "{saved}");
    assert_eq!(saved["options"]["preset"], 9, "{saved}");
    let unrestored = s.get("/api/jobs").json()["unrestored"].clone();
    assert_eq!(unrestored[0]["inputs"], json!([s.dir.join("videos/b.mkv")]), "{unrestored}");

    // Still away at the next start they stay saved, and once back they are queued.
    assert!(s.terminate());
    let mut s = Server::start_in(&s.dir, &ffmpeg, &ffprobe);
    assert_eq!(s.get("/api/jobs").json()["unrestored"].as_array().unwrap().len(), 1);
    assert!(s.terminate());
    std::fs::write(s.dir.join("videos/b.mkv"), b"x").unwrap();
    let s = Server::start_in(&s.dir, &ffmpeg, &ffprobe);
    assert_eq!(heads(&s)[0]["total"], 1);
    assert!(s.get("/api/jobs").json()["unrestored"].as_array().unwrap().is_empty());

    // Cleared from the list once it has run its course, a job's left-out inputs still come back
    // by themselves.
    let mut s = Server::start_with_tools("outlive-done", &ffmpeg, &ffprobe, |d| {
        std::fs::write(d.join("videos/a.mkv"), b"x").unwrap();
        std::fs::create_dir_all(d.join("out")).unwrap();
        write_saved_queue(d, json!([{ "inputs": [d.join("videos/a.mkv"), d.join("videos/b.mkv")], "roots": [], "out_dir": d.join("out"), "options": {}, "out_dir_made": true }]));
    });
    assert_eq!(first_job_ended(&s)["status"], "done");
    assert_eq!(s.post("/api/jobs/clear", &json!({})).status, 200);
    assert!(s.terminate());
    std::fs::write(s.dir.join("videos/b.mkv"), b"x").unwrap();
    let s = Server::start_in(&s.dir, &ffmpeg, &ffprobe);
    assert_eq!(heads(&s).len(), 1);
    drop(s);

    // Those of a job someone stopped are kept apart, however it was stopped, and start only
    // when that is asked for.
    for (name, stop) in [("cancelled", &["cancel"][..]), ("cancelled-cleared", &["cancel", "clear"][..]), ("removed", &["remove"][..])] {
        let name = format!("outlive-{name}");
        let mut s = Server::start_with_tools(&name, &ffmpeg, &ffprobe, |d| {
            paused(d);
            std::fs::write(d.join("videos/a.mkv"), b"x").unwrap();
            std::fs::create_dir_all(d.join("out")).unwrap();
            write_saved_queue(d, json!([{ "inputs": [d.join("videos/a.mkv"), d.join("videos/b.mkv"), d.join("videos/e.mkv")], "roots": [], "out_dir": d.join("out"), "options": {}, "out_dir_made": true }]));
        });
        let id = heads(&s)[0]["id"].as_u64().unwrap();
        for action in stop {
            let path = if *action == "clear" { "/api/jobs/clear".to_string() } else { format!("/api/jobs/{id}/{action}") };
            assert_eq!(s.post(&path, &json!({})).status, 200, "{name}: {action}");
        }
        let left_out = json!([s.dir.join("videos/b.mkv"), s.dir.join("videos/e.mkv")]);
        assert_eq!(saved_queue(&s)["jobs"][0]["inputs"], left_out, "{name}");
        assert_eq!(saved_queue(&s)["jobs"][0]["only_by_hand"], true, "{name}");
        assert_eq!(s.get("/api/jobs").json()["unrestored"][0]["inputs"], left_out, "{name}");

        // Back at the next start, they are shown and do not start.
        assert!(s.terminate());
        std::fs::write(s.dir.join("videos/b.mkv"), b"x").unwrap();
        let s = Server::start_in(&s.dir, &ffmpeg, &ffprobe);
        soon("the saved queue is read", || s.get("/api/jobs").json()["restoring"] == 0);
        let list = s.get("/api/jobs").json();
        assert!(heads(&s).iter().all(|j| j["status"] == "cancelled"), "{name}: {list}");
        assert_eq!(list["unrestored"][0]["inputs"], left_out, "{name}");
        assert!(list["unrestored"][0]["error"].as_str().unwrap().contains("Try again"), "{name}: {list}");

        // Try again queues the one that is back. The other goes on waiting to be asked for.
        let r = s.post("/api/jobs/unrestored/retry", &json!({})).json();
        assert_eq!(r["restored"], 1, "{name}: {r}");
        assert_eq!(heads(&s).last().unwrap()["total"], 1, "{name}");
        assert_eq!(r["unrestored"][0]["inputs"], json!([s.dir.join("videos/e.mkv")]), "{name}: {r}");
        assert!(r["unrestored"][0]["error"].as_str().unwrap().contains("Try again"), "{name}: {r}");
    }

    // Set apart by cancelling, they go back with the job when it is queued again.
    let s = Server::start_with_tools("outlive-again", &ffmpeg, &ffprobe, |d| {
        paused(d);
        std::fs::write(d.join("videos/a.mkv"), b"x").unwrap();
        std::fs::create_dir_all(d.join("out")).unwrap();
        write_saved_queue(d, json!([{ "inputs": [d.join("videos/a.mkv"), d.join("videos/b.mkv")], "roots": [], "out_dir": d.join("out"), "options": {}, "out_dir_made": true }]));
    });
    let id = heads(&s)[0]["id"].as_u64().unwrap();
    assert_eq!(s.post(&format!("/api/jobs/{id}/cancel"), &json!({})).status, 200);
    std::fs::write(s.dir.join("videos/b.mkv"), b"x").unwrap();
    assert_eq!(s.post(&format!("/api/jobs/{id}/again"), &json!({})).status, 200);
    assert_eq!(heads(&s)[0]["total"], 2);
    assert!(s.get("/api/jobs").json()["unrestored"].as_array().unwrap().is_empty());
    drop(s);

    // A job stopped while it runs counts the same.
    let (slow_ffmpeg, slow_ffprobe) = stand_in_tools("outlive-slow", "sleep 1\nexit 1");
    let s = Server::start_with_tools("outlive-running", &slow_ffmpeg, &slow_ffprobe, |d| {
        for name in ["a.mkv", "c.mkv"] {
            std::fs::write(d.join("videos").join(name), b"x").unwrap();
        }
        std::fs::create_dir_all(d.join("out")).unwrap();
        write_saved_queue(d, json!([{ "inputs": [d.join("videos/a.mkv"), d.join("videos/b.mkv"), d.join("videos/c.mkv")], "roots": [], "out_dir": d.join("out"), "options": {}, "out_dir_made": true }]));
    });
    soon("the job runs", || heads(&s).first().is_some_and(|j| j["status"] == "running"));
    let id = heads(&s)[0]["id"].as_u64().unwrap();
    assert_eq!(s.post(&format!("/api/jobs/{id}/cancel"), &json!({})).status, 200);
    soon("the job has stopped", || heads(&s)[0]["status"] == "cancelled");
    soon("what it left out is saved", || saved_queue(&s)["jobs"][0]["only_by_hand"] == true);
    assert_eq!(saved_queue(&s)["jobs"][0]["inputs"], json!([s.dir.join("videos/b.mkv")]));
}

#[test]
fn a_job_that_comes_back_in_part_writes_where_it_would_have_written() {
    let (ffmpeg, ffprobe) = stand_in_tools("layout", "exit 1");
    let mut s = Server::start_with_tools("layout", &ffmpeg, &ffprobe, |d| {
        paused(d);
        for name in ["A/a.mkv", "B/b.mkv"] {
            std::fs::create_dir_all(d.join("videos").join(name).parent().unwrap()).unwrap();
            std::fs::write(d.join("videos").join(name), b"x").unwrap();
        }
    });
    let (videos, out) = (s.dir.join("videos"), s.dir.join("out"));
    let r = s.post("/api/jobs", &json!({ "inputs": [videos.join("A"), videos.join("B")], "roots": [], "out_dir": out }));
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(outputs(&s), [out.join("A/a.mp4"), out.join("B/b.mp4")]);
    // One folder away at a restart: the other's outputs stay where they are.
    assert!(s.terminate());
    std::fs::rename(videos.join("B"), videos.join("B.away")).unwrap();
    let mut s = Server::start_in(&s.dir, &ffmpeg, &ffprobe);
    assert_eq!(outputs(&s), [out.join("A/a.mp4")]);
    // Back, it goes where it would have gone.
    assert!(s.terminate());
    std::fs::rename(videos.join("B.away"), videos.join("B")).unwrap();
    let s = Server::start_in(&s.dir, &ffmpeg, &ffprobe);
    assert_eq!(outputs(&s), [out.join("A/a.mp4"), out.join("B/b.mp4")]);
    drop(s);

    // Saved by an earlier version, which did not write down how many folders there were.
    let s = Server::start_with_tools("layout-old", &ffmpeg, &ffprobe, |d| {
        paused(d);
        std::fs::create_dir_all(d.join("videos/A")).unwrap();
        std::fs::write(d.join("videos/A/a.mkv"), b"x").unwrap();
        write_saved_queue(d, json!([{ "inputs": [d.join("videos/A")], "dropped": [d.join("videos/B")], "roots": [], "out_dir": d.join("out"), "options": {}, "out_dir_made": true }]));
    });
    assert_eq!(outputs(&s), [s.dir.join("out/A/a.mp4")]);
    drop(s);

    // The folder a selection was made in, away with one of the files: nothing is forgotten.
    let mut s = Server::start_with_tools("layout-root", &ffmpeg, &ffprobe, |d| {
        paused(d);
        std::fs::write(d.join("videos/y.mkv"), b"x").unwrap();
        let root = d.join("videos/library");
        write_saved_queue(d, json!([{ "inputs": [root.join("sub/x.mkv"), d.join("videos/y.mkv")], "roots": [root], "out_dir": d.join("out"), "options": {}, "out_dir_made": true }]));
    });
    let root = s.dir.join("videos/library");
    soon("the job is saved again", || saved_queue(&s)["jobs"][0]["dropped"][0].is_string());
    assert_eq!(saved_queue(&s)["jobs"][0]["roots"], json!([root]));
    assert!(s.terminate());
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/x.mkv"), b"x").unwrap();
    let s = Server::start_in(&s.dir, &ffmpeg, &ffprobe);
    assert!(outputs(&s).contains(&s.dir.join("out/sub/x.mp4")), "{:?}", outputs(&s));
}

#[test]
fn an_output_folder_nothing_has_made_yet_is_made_once() {
    let (ffmpeg, ffprobe) = stand_in_tools("once", "exit 1");
    let mut s = Server::start_with_tools("once", &ffmpeg, &ffprobe, |d| {
        paused(d);
        std::fs::write(d.join("videos/a.mkv"), b"x").unwrap();
        std::fs::create_dir_all(d.join("first")).unwrap();
        std::fs::write(d.join("first/something"), b"").unwrap();
        write_saved_queue(d, json!([{ "inputs": [d.join("videos/a.mkv")], "roots": [], "out_dir": d.join("first/out"), "options": {} }]));
    });
    let out = s.dir.join("first/out");
    assert!(out.is_dir());
    soon("the job is saved with its mark", || saved_queue(&s)["jobs"][0]["out_dir_made"] == true);
    assert!(saved_queue(&s)["jobs"][0]["out_dir_place"]["device"].is_u64(), "where the folder is is saved with it");
    // From then on its absence means a drive that is away.
    assert!(s.terminate());
    std::fs::remove_dir_all(s.dir.join("first")).unwrap();
    let s = Server::start_in(&s.dir, &ffmpeg, &ffprobe);
    assert_eq!(heads(&s).len(), 1);
    assert!(!s.dir.join("first").exists());
    drop(s);

    // A watch saved by an earlier version, and one that cannot work: the output folder holds
    // the watched folder. The second is said to be wrong and is not walked.
    let s = Server::start_with_tools("once-watch", &ffmpeg, &ffprobe, |d| {
        std::fs::create_dir_all(d.join("videos/in")).unwrap();
        std::fs::create_dir_all(d.join("videos/inside")).unwrap();
        std::fs::write(d.join("videos/inside/a.mkv"), b"x").unwrap();
        std::fs::create_dir_all(d.join("second")).unwrap();
        std::fs::write(d.join("second/something"), b"").unwrap();
        let settings = json!({ "watches": [
            { "id": 1, "path": d.join("videos/in"), "out_dir": d.join("second/out"), "options": {}, "enabled": false },
            { "id": 2, "path": d.join("videos/inside"), "out_dir": d.join("videos"), "options": {}, "enabled": false, "out_dir_made": true },
        ] });
        std::fs::write(d.join("data/av1opt/ui.json"), settings.to_string()).unwrap();
    });
    let out = s.dir.join("second/out");
    let look = |id: u64| s.post(&format!("/api/watch/{id}/look"), &json!({})).json()["watches"].as_array().unwrap().iter().find(|w| w["id"] == id).unwrap().clone();
    let w = look(1);
    assert!(w["error"].is_null(), "{w}");
    assert!(out.is_dir());
    let settings: Value = serde_json::from_slice(&std::fs::read(s.data("ui.json")).unwrap()).unwrap();
    assert_eq!(settings["watches"][0]["out_dir_made"], true, "{settings}");
    assert!(settings["watches"][0]["out_dir_place"]["device"].is_u64(), "{settings}");
    std::fs::remove_dir_all(s.dir.join("second")).unwrap();
    look(1);
    assert!(!s.dir.join("second").exists());
    let w = look(2);
    assert!(w["error"].as_str().unwrap_or_default().contains("contains the watched folder"), "{w}");
    assert_eq!(w["videos"], 0, "{w}");
}

#[test]
fn what_was_saved_before_folders_were_made_does_not_land_under_a_mount_point() {
    // The first start after the upgrade, with the drive away: its mount point is an empty folder,
    // or is not there at all. Saved jobs and watches have no mark yet.
    let (ffmpeg, ffprobe) = stand_in_tools("upgrade", "exit 1");
    let mut s = Server::start_with_tools("upgrade", &ffmpeg, &ffprobe, |d| {
        std::fs::write(d.join("videos/a.mkv"), b"x").unwrap();
        std::fs::create_dir_all(d.join("videos/in")).unwrap();
        std::fs::create_dir_all(d.join("mnt/usb")).unwrap();
        write_saved_queue(d, json!([{ "inputs": [d.join("videos/a.mkv")], "roots": [], "out_dir": d.join("mnt/usb/out"), "options": {} }]));
        let settings = json!({ "watches": [
            { "id": 1, "path": d.join("videos/in"), "out_dir": d.join("mnt/usb/watched"), "options": {}, "enabled": false },
            { "id": 2, "path": d.join("videos/in"), "out_dir": d.join("mnt/none/deep/watched"), "options": {}, "enabled": false },
        ] });
        std::fs::write(d.join("data/av1opt/ui.json"), settings.to_string()).unwrap();
    });
    let job = first_job_ended(&s);
    assert_eq!(job["status"], "failed", "{job}");
    assert!(job["error"].as_str().unwrap_or_default().contains("may not be mounted"), "{job}");
    soon("the job stays saved", || saved_jobs(&s) == 1);
    assert_eq!(saved_queue(&s)["jobs"][0]["out_dir_made"], false, "it has not been made yet");
    let look = |id: u64| s.post(&format!("/api/watch/{id}/look"), &json!({})).json()["watches"].as_array().unwrap().iter().find(|w| w["id"] == id).unwrap().clone();
    for id in [1, 2] {
        let w = look(id);
        assert!(w["error"].as_str().unwrap_or_default().contains("may not be mounted"), "{w}");
    }
    assert_eq!(std::fs::read_dir(s.dir.join("mnt/usb")).unwrap().count(), 0, "nothing under the mount point");
    assert!(!s.dir.join("mnt/none").exists());

    // With the drive back, what has to be made is made, once.
    assert!(s.terminate());
    std::fs::write(s.dir.join("mnt/usb/on the drive"), b"").unwrap();
    let s = Server::start_in(&s.dir, &ffmpeg, &ffprobe);
    let job = first_job_ended(&s);
    assert_eq!(job["status"], "done", "{job}");
    assert!(s.dir.join("mnt/usb/out").is_dir());
    let w = s.post("/api/watch/1/look", &json!({})).json()["watches"][0].clone();
    assert!(w["error"].is_null(), "{w}");
    assert!(s.dir.join("mnt/usb/watched").is_dir());
}

#[test]
fn an_output_folder_now_on_another_drive_keeps_its_job_waiting() {
    let (ffmpeg, ffprobe) = stand_in_tools("elsewhere", "exit 1");
    let s = Server::start_with_tools("elsewhere", &ffmpeg, &ffprobe, |d| {
        std::fs::write(d.join("videos/a.mkv"), b"x").unwrap();
        std::fs::create_dir_all(d.join("videos/in")).unwrap();
        std::fs::write(d.join("videos/in/b.mkv"), b"x").unwrap();
        age(&d.join("videos/in/b.mkv"), 1000);
        let mtime = std::fs::metadata(d.join("videos/in/b.mkv")).unwrap().modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        std::fs::create_dir_all(d.join("mnt/usb")).unwrap();
        // Queued while the drive was there; what stands at the path now is its mount point.
        let place = json!({ "device": 1, "mount": d.join("mnt/usb/not mounted") });
        write_saved_queue(d, json!([{ "inputs": [d.join("videos/a.mkv")], "roots": [], "out_dir": d.join("mnt/usb"), "options": {}, "out_dir_made": true, "out_dir_place": place }]));
        let settings = json!({ "watches": [{ "id": 1, "path": d.join("videos/in"), "out_dir": d.join("mnt/usb"), "options": {}, "enabled": false, "out_dir_made": true, "out_dir_place": place }] });
        std::fs::write(d.join("data/av1opt/ui.json"), settings.to_string()).unwrap();
        let files = json!({ d.join("videos/in/b.mkv").to_str().unwrap(): { "size": 1, "mtime": mtime, "mark": "fresh", "seen": 3 } });
        std::fs::write(d.join("data/av1opt/ui-watch.json"), json!({ "format": 1, "folders": { "1": { "files": files } } }).to_string()).unwrap();
    });
    let job = first_job_ended(&s);
    assert_eq!(job["status"], "failed", "{job}");
    assert!(job["error"].as_str().unwrap_or_default().contains("on another drive than when it was queued"), "{job}");
    soon("the job stays saved", || saved_jobs(&s) == 1);
    assert_eq!(s.post("/api/jobs/clear", &json!({})).status, 200);
    assert_eq!(saved_jobs(&s), 1, "it is waiting, not over");
    assert_eq!(std::fs::read_dir(s.dir.join("mnt/usb")).unwrap().count(), 0, "nothing is written there");
    // The watch queues nothing that could only fail, and says why.
    let w = s.post("/api/watch/1/look", &json!({})).json()["watches"][0].clone();
    assert!(w["error"].as_str().unwrap_or_default().contains("on another drive"), "{w}");
    assert_eq!(heads(&s).len(), 1, "{w}");
}

#[test]
fn a_video_an_earlier_version_gave_up_on_is_tried_again() {
    let s = Server::start("given-up", |d| {
        std::fs::create_dir_all(d.join("videos/in")).unwrap();
        let settings = json!({ "watches": [{ "id": 1, "path": d.join("videos/in"), "out_dir": d.join("out"), "options": {}, "enabled": false, "out_dir_made": true }] });
        std::fs::write(d.join("data/av1opt/ui.json"), settings.to_string()).unwrap();
        let video = d.join("videos/in/a.mkv");
        let files = json!({ video.to_str().unwrap(): { "size": 1, "mtime": 2, "mark": "left", "seen": 3, "tries": 3 } });
        std::fs::write(d.join("data/av1opt/ui-watch.json"), json!({ "format": 1, "folders": { "1": { "files": files } } }).to_string()).unwrap();
    });
    let w = s.get("/api/watch").json()["watches"][0].clone();
    assert_eq!((w["unread"].as_u64(), w["left"].as_u64()), (Some(1), Some(0)), "{w}");
}

#[test]
fn a_drive_lost_during_a_job_is_not_written_under_its_mount_point() {
    let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ui-lost/drive/out");
    // Reading the first video is when the drive goes.
    let (ffmpeg, ffprobe) = stand_in_tools("lost", &format!("rm -rf '{}'\nexit 1", out.parent().unwrap().display()));
    let s = Server::start_with_tools("lost", &ffmpeg, &ffprobe, |d| {
        for name in ["a.mkv", "b.mkv"] {
            std::fs::write(d.join("videos").join(name), b"x").unwrap();
        }
        std::fs::create_dir_all(d.join("drive/out")).unwrap();
        write_saved_queue(d, json!([{ "inputs": [d.join("videos/a.mkv"), d.join("videos/b.mkv")], "roots": [], "out_dir": d.join("drive/out"), "options": {}, "out_dir_made": true }]));
    });
    let job = first_job_ended(&s);
    assert_eq!(job["status"], "failed", "{job}");
    assert!(job["error"].as_str().unwrap_or_default().contains("is not there"), "{job}");
    assert!(!s.dir.join("drive").exists());
    let files = s.get("/api/jobs").json()["jobs"][0]["files"].clone();
    assert!(files[1]["detail"].as_str().unwrap_or_default().contains("is not there"), "{files}");
    // It waits for the drive with all its files.
    soon("the job stays saved", || saved_jobs(&s) == 1);
}

#[test]
fn a_saved_job_with_a_broken_list_of_inputs_reaches_the_page_without_it() {
    let s = Server::start("broken-list", |d| {
        write_saved_queue(d, json!([
            { "inputs": "not a list", "out_dir": d.join("out") },
            { "inputs": [1, 2], "out_dir": d.join("out") },
            { "inputs": [d.join("videos/gone.mkv")], "roots": [], "out_dir": d.join("out"), "options": {} },
        ]));
    });
    let list = s.get("/api/jobs").json()["unrestored"].as_array().unwrap().clone();
    assert_eq!(list.len(), 3, "{list:?}");
    assert!(list[0].get("inputs").is_none() && list[1].get("inputs").is_none(), "{list:?}");
    assert!(list[0]["error"].is_string());
    assert!(list[2]["inputs"].is_array(), "{list:?}");
}

#[test]
fn videos_left_out_for_their_names_are_said_on_the_page() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let (ffmpeg, ffprobe) = stand_in_tools("names", "exit 1");
    let s = Server::start_with_tools("names", &ffmpeg, &ffprobe, |d| {
        paused(d);
        std::fs::create_dir_all(d.join("videos/in")).unwrap();
        std::fs::write(d.join("videos/in/a.mkv"), b"x").unwrap();
        std::fs::write(d.join("videos/in").join(OsStr::from_bytes(b"b\xff.mkv")), b"x").unwrap();
    });
    let dir = s.dir.join("videos/in");
    let r = s.post("/api/jobs", &json!({ "inputs": [dir], "roots": [], "out_dir": s.dir.join("out") }));
    assert_eq!(r.status, 200, "{}", r.body);
    let job = heads(&s)[0].clone();
    assert_eq!(job["total"], 1, "{job}");
    let note = job["note"].as_str().unwrap_or_default();
    assert!(note.contains("1 video(s) left out") && note.contains("UTF-8"), "{job}");

    // A benchmark says it too, however it ends.
    let id = job["id"].as_u64().unwrap();
    assert_eq!(s.post(&format!("/api/jobs/{id}/remove"), &json!({})).status, 200);
    let r = s.post("/api/bench", &json!({ "paths": [dir], "presets": [8], "param_sets": ["tune=0"] }));
    assert_eq!(r.status, 200, "{}", r.body);
    soon("the benchmark ends", || s.get("/api/bench").json()["run"]["status"] != "running");
    let run = s.get("/api/bench").json()["run"].clone();
    let note = run["note"].as_str().unwrap_or_default();
    assert!(note.contains("1 video(s) left out") && note.contains("UTF-8"), "{run}");
}

/// `av1opt scan` of an empty folder, with a scratch environment of its own.
fn scan_command(name: &str) -> std::process::Command {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("ui-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("videos")).unwrap();
    let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_av1opt"));
    c.arg("scan").arg(dir.join("videos")).env("XDG_DATA_HOME", dir.join("data")).env("XDG_CACHE_HOME", dir.join("cache")).env("AV1OPT_HISTORY", dir.join("history.jsonl"));
    c.stdin(std::process::Stdio::null());
    c
}

#[test]
fn a_closed_stderr_is_no_reason_to_stop() {
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let out = scan_command("no-stderr").stdout(std::process::Stdio::null()).stderr(writer).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
}

#[test]
fn a_closed_stdout_ends_a_command_as_a_closed_pipe_does() {
    use std::os::unix::process::ExitStatusExt;
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let out = scan_command("no-stdout").stdout(writer).stderr(std::process::Stdio::piped()).output().unwrap();
    assert_eq!(out.status.signal(), Some(libc::SIGPIPE), "{out:?}");
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(!said.contains("panicked"), "{said}");
}

#[test]
fn what_a_command_was_asked_to_write_is_written_though_nobody_reads_its_output() {
    use std::os::unix::process::ExitStatusExt;
    let (ffmpeg, ffprobe) = stand_in_tools(
        "unread",
        r#"echo '{"format":{"duration":"60","size":"2","bit_rate":"8000000"},"streams":[{"index":0,"codec_type":"video","codec_name":"h264","width":640,"height":360,"avg_frame_rate":"24/1","r_frame_rate":"24/1","pix_fmt":"yuv420p"}]}'"#,
    );
    let closed = || {
        let (reader, writer) = std::io::pipe().unwrap();
        drop(reader);
        writer
    };
    let command = |what: &str, name: &str, args: &[&str]| {
        let c = scan_command(name);
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("ui-{name}"));
        std::fs::write(dir.join("videos/a.mkv"), b"xx").unwrap();
        let video_args: Vec<_> = c.get_args().skip(1).map(|a| a.to_owned()).collect();
        let mut command = std::process::Command::new(c.get_program());
        command.envs(c.get_envs().filter_map(|(k, v)| Some((k.to_owned(), v?.to_owned())))).env("XDG_CONFIG_HOME", dir.join("config"));
        command.arg(what).args(video_args).args(["--ffprobe", &ffprobe]);
        if what != "scan" {
            command.args(["--ffmpeg", &ffmpeg]);
        }
        command.args(args).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::piped());
        (command, dir)
    };
    let run = |what: &str, name: &str, args: &[&str], files: &[&str]| {
        let (mut command, dir) = command(what, name, args);
        for f in files {
            command.arg(dir.join(f));
        }
        let out = command.stdout(closed()).output().unwrap();
        assert_eq!(out.status.signal(), Some(libc::SIGPIPE), "{what}: {out:?}");
        for f in files {
            assert!(dir.join(f).is_file(), "{what}: {f} was not written: {}", String::from_utf8_lossy(&out.stderr));
        }
    };
    run("scan", "unread-scan", &["--csv"], &["list.csv"]);
    run("scan", "unread-scan-list", &["--hours", "1", "--list"], &["picked.txt"]);
    let dups = |name, flag, file| run("dups", name, &[flag], &[file]);
    dups("unread-dups-csv", "--csv", "groups.csv");
    dups("unread-dups-json", "--json", "groups.json");
    const ONE_SETTING: [&str; 4] = ["--presets", "8", "--params", "tune=0"];
    let bench = |name, flag, file| run("bench", name, &[&ONE_SETTING[..], &[flag]].concat(), &[file]);
    bench("unread-bench-csv", "--csv", "results.csv");
    bench("unread-bench-json", "--json", "results.json");

    // Nothing the stand-in tools do can be measured, so nothing is saved, and a run asked to
    // save goes as far as the others with its output closed.
    let (mut saving, dir) = command("bench", "unread-bench-save", &[&ONE_SETTING[..], &["--save"]].concat());
    let out = saving.stdout(closed()).output().unwrap();
    assert_eq!(out.status.signal(), Some(libc::SIGPIPE), "{out:?}");
    assert!(!dir.join("data/av1opt/tuning.json").exists());
}

/// Sets a file's modification time `secs` in the past.
fn age(path: &Path, secs: u64) {
    let f = std::fs::File::options().write(true).open(path).unwrap();
    f.set_modified(std::time::SystemTime::now() - Duration::from_secs(secs)).unwrap();
}

#[test]
fn watched_folders_are_kept_looked_at_and_never_queue_what_was_there() {
    let mut s = Server::start("watch", |d| {
        std::fs::create_dir_all(d.join("videos/in/sub")).unwrap();
        std::fs::write(d.join("videos/in/old.mkv"), b"x").unwrap();
        std::fs::write(d.join("videos/in/notes.txt"), b"x").unwrap();
    });
    let dir = s.dir.join("videos/in");
    let add = |s: &Server, path: &Path, out: &Path| s.post("/api/watch", &json!({ "path": path, "out_dir": out, "options": { "preset": 8 } }));
    let watches = |s: &Server| s.get("/api/watch").json()["watches"].as_array().unwrap().clone();

    // What cannot be watched is said so, and nothing is kept of it.
    for (path, out, why) in [
        (s.dir.join("videos/nowhere"), s.dir.join("out"), "does not exist"),
        (dir.join("old.mkv"), s.dir.join("out"), "not a folder"),
        (dir.clone(), PathBuf::from("out"), "output folder"),
        (dir.clone(), dir.clone(), "cannot be the watched folder"),
        (dir.clone(), dir.join("sub/.."), "cannot be the watched folder"),
        (dir.clone(), s.dir.join("videos"), "contains the watched folder"),
    ] {
        let r = add(&s, &path, &out);
        assert_eq!(r.status, 400, "{}", r.body);
        assert!(r.json()["error"].as_str().unwrap_or("").contains(why), "{why}: {}", r.body);
    }
    let r = s.post("/api/watch", &json!({ "path": dir, "out_dir": s.dir.join("out"), "options": { "remove": [[0.0, 1.0]] } }));
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(watches(&s).is_empty());

    let r = add(&s, &dir, &s.dir.join("out"));
    assert_eq!(r.status, 200, "{}", r.body);
    let id = r.json()["id"].as_u64().unwrap();
    let r = add(&s, &dir, &s.dir.join("elsewhere"));
    assert!(r.status == 400 && r.body.contains("watched already"), "{}", r.body);
    let look = |s: &Server| {
        let r = s.post(&format!("/api/watch/{id}/look"), &json!({}));
        assert_eq!(r.status, 200, "{}", r.body);
        r.json()["watches"][0].clone()
    };

    // The video that was there is noted and left; the text file is not a video.
    let w = look(&s);
    assert_eq!((w["videos"].as_u64(), w["before"].as_u64(), w["fresh"].as_u64(), w["enabled"].as_bool()), (Some(1), Some(1), Some(0), Some(true)), "{w}");
    assert_eq!(w["options"]["preset"], 8);

    // A new one, in a sub-folder, is seen at the next look and due at the one after: with no
    // usable ffmpeg here it cannot be judged, which is said, and it stays to be tried again.
    std::fs::write(dir.join("sub/new.mp4"), b"xx").unwrap();
    age(&dir.join("sub/new.mp4"), 600);
    // Hidden folders and the output folder are not looked into.
    std::fs::create_dir_all(dir.join(".av1opt-work-1")).unwrap();
    std::fs::write(dir.join(".av1opt-work-1/chunk.mkv"), b"x").unwrap();
    let w = look(&s);
    assert_eq!((w["videos"].as_u64(), w["fresh"].as_u64(), w["error"].as_str()), (Some(2), Some(1), None), "{w}");
    let w = look(&s);
    assert_eq!((w["fresh"].as_u64(), w["queued"].as_u64(), w["left"].as_u64()), (Some(1), Some(0), Some(0)), "{w}");
    assert!(w["error"].as_str().unwrap_or("").contains("Settings"), "{w}");
    assert!(s.get("/api/jobs/heads").json()["jobs"].as_array().unwrap().is_empty());

    // Switched off, and all of it still there after a restart.
    let r = s.post(&format!("/api/watch/{id}/enabled"), &json!({ "enabled": false }));
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(s.post(&format!("/api/watch/{id}/enabled"), &json!({})).status, 400);
    assert!(s.terminate());
    let mut s = Server::start_in(&s.dir, "false", "false");
    let w = watches(&s);
    assert_eq!(w.len(), 1);
    assert_eq!((w[0]["id"].as_u64(), w[0]["enabled"].as_bool(), w[0]["before"].as_u64(), w[0]["fresh"].as_u64()), (Some(id), Some(false), Some(1), Some(1)), "{}", w[0]);
    assert_eq!(w[0]["path"].as_str(), std::fs::canonicalize(&dir).unwrap().to_str());

    // What was seen is lost, or written by a newer av1opt: the videos in the folder are not
    // so many new ones to encode, and the newer file is kept under another name.
    assert!(s.terminate());
    std::fs::write(s.data("ui-watch.json"), br#"{"format":99,"folders":{"1":{"files":{}}}}"#).unwrap();
    let s = Server::start_in(&s.dir, "false", "false");
    let w = look(&s);
    assert_eq!((w["videos"].as_u64(), w["before"].as_u64(), w["fresh"].as_u64()), (Some(2), Some(2), Some(0)), "{w}");
    assert!(s.data("ui-watch.format99.json").exists());

    // Removed: gone from the settings and from what is remembered of its videos.
    assert_eq!(s.post("/api/watch/99/remove", &json!({})).status, 404);
    assert_eq!(s.post(&format!("/api/watch/{id}/remove"), &json!({})).status, 200);
    assert!(watches(&s).is_empty());
    let saved: Value = serde_json::from_slice(&std::fs::read(s.data("ui.json")).unwrap()).unwrap();
    assert!(saved.get("watches").is_none(), "{saved}");
    let state: Value = serde_json::from_slice(&std::fs::read(s.data("ui-watch.json")).unwrap()).unwrap();
    assert_eq!(state["folders"], json!({}));
}

/// What only the page decides is checked by tests/page_checks.js, on a stand-in for the browser.
#[test]
fn the_page_checks_pass() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let run = std::process::Command::new("node").arg(root.join("tests/page_checks.js")).arg(root.join("ui/app.js")).output();
    let output = match run {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return eprintln!("the_page_checks_pass: skipped, node is not installed"),
        Err(e) => panic!("cannot run node: {e}"),
    };
    let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "{text}");
    assert!(text.contains("ok: "), "{text}");
}

#[test]
fn numbers_out_of_range_are_refused_by_the_name_of_their_setting() {
    let s = Server::start("number-ranges", |d| {
        std::fs::create_dir_all(d.join("videos")).unwrap();
    });
    let videos = s.dir.join("videos");
    let out = s.dir.join("out");
    let job = |options: Value| json!({ "inputs": [videos], "out_dir": out, "options": options });
    let watch = |options: Value| json!({ "path": videos, "out_dir": out, "options": options });
    let scan = |extra: Value| {
        let mut body = json!({ "paths": [videos] });
        body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        body
    };
    for (path, body, named) in [
        ("/api/jobs", job(json!({ "target_vmaf": 0 })), "target VMAF"),
        ("/api/jobs", job(json!({ "target_vmaf": null })), "target VMAF"),
        ("/api/jobs", job(json!({ "preset": 14 })), "preset"),
        ("/api/jobs", job(json!({ "workers": 0 })), "workers"),
        ("/api/jobs", job(json!({ "min_savings": 2 })), "minimum savings"),
        ("/api/watch", watch(json!({ "target_vmaf": 0 })), "target VMAF"),
        ("/api/watch", watch(json!({ "workers": 9 })), "workers"),
        ("/api/scans", scan(json!({ "target_vmaf": 0 })), "target VMAF"),
        ("/api/scans", scan(json!({ "min_savings": -1 })), "minimum savings"),
        ("/api/scans", scan(json!({ "preset": null })), "preset"),
        ("/api/bench", scan(json!({ "sample_secs": 0 })), "clip length"),
        ("/api/bench", scan(json!({ "presets": [1.5] })), "presets"),
        ("/api/dups", scan(json!({ "tolerance": 21 })), "tolerance"),
    ] {
        let r = s.post(path, &body);
        assert_eq!(r.status, 400, "{path} {body}: {}", r.body);
        assert!(r.body.contains(named) && r.body.contains("must be"), "{path} {body}: {}", r.body);
    }
    // Nothing of it got anywhere.
    assert!(s.get("/api/jobs").json()["jobs"].as_array().unwrap().is_empty());
    assert_eq!(s.get("/api/watch").json()["watches"], json!([]));
}

#[test]
fn a_watch_record_that_cannot_be_read_is_kept_aside_and_nothing_gets_queued() {
    let s = Server::start("watch-unreadable", |d| {
        let folder = d.join("videos/in");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("old.mkv"), b"x").unwrap();
        let settings = json!({ "watches": [{ "id": 1, "path": folder, "out_dir": d.join("out"), "options": {} }] });
        std::fs::write(d.join("data/av1opt/ui.json"), settings.to_string()).unwrap();
        std::fs::write(d.join("data/av1opt/ui-watch.json"), b"{ not what was written").unwrap();
    });
    let notices = s.get("/api/state").json()["notices"].clone();
    let told = notices[0].as_str().unwrap_or_default();
    assert!(told.contains("watched folders") && told.contains("seen before"), "{notices}");
    assert!(!s.data("ui-watch.json").exists() || std::fs::read(s.data("ui-watch.json")).unwrap() != b"{ not what was written");
    let kept: Vec<_> = std::fs::read_dir(s.data("")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with("ui-watch.unreadable-")).collect();
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(std::fs::read(s.data(&kept[0])).unwrap(), b"{ not what was written");
    assert!(told.contains(&kept[0]), "{told}");

    // The video in the folder counts as there before, and stays out of the queue.
    let r = s.post("/api/watch/1/look", &json!({}));
    assert_eq!(r.status, 200, "{}", r.body);
    let w = r.json()["watches"][0].clone();
    assert_eq!((w["videos"].as_u64(), w["before"].as_u64(), w["fresh"].as_u64()), (Some(1), Some(1), Some(0)), "{w}");
    assert_eq!(s.get("/api/jobs").json()["jobs"], json!([]));
}

#[test]
fn servers_sharing_a_cache_keep_their_previews_apart() {
    let shared = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ui-shared-cache");
    let _ = std::fs::remove_dir_all(&shared);
    std::fs::create_dir_all(shared.join("av1opt/previews/2147483646")).unwrap();
    std::fs::create_dir_all(shared.join("av1opt/previews/not-a-process")).unwrap();
    std::fs::write(shared.join("av1opt/previews/2147483646/left.mp4"), b"x").unwrap();
    std::fs::write(shared.join("av1opt/previews/old-version.mp4"), b"x").unwrap();
    let use_shared = |d: &Path| {
        std::fs::remove_dir(d.join("cache")).unwrap();
        std::os::unix::fs::symlink(&shared, d.join("cache")).unwrap();
    };
    let previews = shared.join("av1opt/previews");
    let listed = || {
        let mut names: Vec<String> = std::fs::read_dir(&previews).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    };
    let mut first = Server::start("previews-first", use_shared);
    let own = previews.join(first.child.id().to_string());
    assert_eq!(listed(), [first.child.id().to_string()], "what servers that are gone left is cleared");
    std::fs::write(own.join("playing.mp4"), b"x").unwrap();

    let mut second = Server::start("previews-second", use_shared);
    assert!(own.join("playing.mp4").exists(), "the second server wiped the first one's previews");
    assert!(previews.join(second.child.id().to_string()).is_dir());

    assert!(first.terminate());
    assert!(!own.exists(), "a server takes its previews along when it stops");
    assert_eq!(listed(), [second.child.id().to_string()]);
    assert!(second.terminate());
    assert_eq!(listed(), Vec::<String>::new());
}

#[test]
fn a_scan_says_which_folders_it_could_not_read() {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: a plain query of this process's user.
    if unsafe { libc::geteuid() } == 0 {
        return eprintln!("skipped: root can read every folder");
    }
    let s = Server::start("scan-unreadable", |d| {
        for i in 0..7 {
            std::fs::create_dir_all(d.join(format!("videos/closed{i}"))).unwrap();
        }
    });
    let videos = s.dir.join("videos");
    let closed = |i: usize| videos.join(format!("closed{i}"));
    for i in 0..7 {
        std::fs::set_permissions(closed(i), std::fs::Permissions::from_mode(0o000)).unwrap();
    }
    let id = s.post("/api/scans", &json!({ "paths": [videos] })).json()["id"].as_u64().unwrap();
    let t = Instant::now();
    let r = loop {
        let r = s.get(&format!("/api/scans/{id}")).json();
        if r["status"] != "running" {
            break r;
        }
        assert!(t.elapsed() < Duration::from_secs(20), "{r}");
        std::thread::sleep(Duration::from_millis(50));
    };
    for i in 0..7 {
        std::fs::set_permissions(closed(i), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    assert_eq!(r["status"], "done", "{r}");
    assert_eq!(r["unreadable_folders"], 7, "{r}");
    let paths = r["unreadable_paths"].as_array().unwrap();
    assert_eq!(paths.len(), 5, "{r}");
    assert!(paths.iter().all(|p| p.as_str().unwrap().starts_with(videos.to_str().unwrap())), "{r}");
}
