//! Request handling: session check, routing, static files, the event stream and media ranges.

use super::{App, bench, dups, err_json, jobs, library};
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::Duration;
use tiny_http::{Header, Method, Request, Response, StatusCode};

const INDEX: &str = include_str!("../../ui/index.html");
const APP_JS: &str = include_str!("../../ui/app.js");
const STYLE: &str = include_str!("../../ui/style.css");

/// Embedded page file; with `AV1OPT_UI_DEV=<repo>/ui` set, read from disk instead (edit and reload).
fn asset(name: &str, embedded: &str) -> String {
    std::env::var_os("AV1OPT_UI_DEV")
        .and_then(|d| std::fs::read_to_string(PathBuf::from(d).join(name)).ok())
        .unwrap_or_else(|| embedded.to_string())
}

fn header(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes()).expect("valid header")
}

fn get_header<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers().iter().find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name)).map(|h| h.value.as_str())
}

/// Security headers sent with every response.
fn hardened<R: Read>(r: Response<R>) -> Response<R> {
    r.with_header(header("X-Content-Type-Options", "nosniff"))
        .with_header(header("X-Frame-Options", "DENY"))
        .with_header(header("Referrer-Policy", "no-referrer"))
        .with_header(header("Cache-Control", "no-store"))
}

fn reply(req: Request, status: u16, ctype: &str, body: impl Into<Vec<u8>>) {
    let r = Response::from_data(body.into()).with_status_code(status).with_header(header("Content-Type", ctype));
    let _ = req.respond(hardened(r));
}

fn reply_json(req: Request, status: u16, v: &Value) {
    reply(req, status, "application/json", v.to_string());
}

fn cookie_name(port: u16) -> String {
    // Cookies are shared between ports of one host; keep sessions of several instances apart.
    format!("av1opt_{port}")
}

/// Host must be this server (blocks DNS rebinding); Origin, when sent, must be this server too.
fn same_host(app: &App, req: &Request) -> bool {
    let ok = |v: &str| v == format!("127.0.0.1:{}", app.port) || v == format!("localhost:{}", app.port);
    let host_ok = get_header(req, "Host").is_some_and(ok);
    let origin_ok = match get_header(req, "Origin") {
        None => true,
        Some(o) => o.strip_prefix("http://").is_some_and(ok),
    };
    host_ok && origin_ok
}

fn has_session(app: &App, req: &Request) -> bool {
    let want = format!("{}={}", cookie_name(app.port), app.token);
    get_header(req, "Cookie").is_some_and(|c| c.split(';').any(|kv| ct_eq(kv.trim().as_bytes(), want.as_bytes())))
}

/// Constant-time comparison for the session token.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn handle(app: &Arc<App>, mut req: Request) {
    if !same_host(app, &req) {
        return reply(req, 403, "text/plain", "forbidden: av1opt only answers to http://127.0.0.1");
    }
    let url = req.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let path = path.to_string();
    let q = parse_query(query);

    // Sign-in: the link printed at start carries the token once; it becomes a cookie.
    if path == "/" && let Some(t) = q.iter().find(|(k, _)| k == "token").map(|(_, v)| v) {
        if !ct_eq(t.as_bytes(), app.token.as_bytes()) {
            return reply(req, 403, "text/plain", "wrong token; use the link av1opt printed");
        }
        let cookie = format!("{}={}; Path=/; HttpOnly; SameSite=Strict", cookie_name(app.port), app.token);
        let r = Response::empty(303).with_header(header("Location", "/")).with_header(header("Set-Cookie", &cookie));
        let _ = req.respond(hardened(r));
        return;
    }
    if !has_session(app, &req) {
        return reply(req, 401, "text/html", "<!doctype html><p>Not signed in. Open the link that <code>av1opt ui</code> printed in the terminal.</p>");
    }

    let method = req.method().clone();
    if method == Method::Post {
        // JSON-only POSTs can't be sent cross-origin without a preflight we never grant.
        if !get_header(&req, "Content-Type").is_some_and(|c| c.starts_with("application/json")) {
            return reply(req, 415, "text/plain", "expected application/json");
        }
    }
    let body: Value = if method == Method::Post {
        let mut s = String::new();
        if req.as_reader().take(1 << 20).read_to_string(&mut s).is_err() {
            return reply(req, 400, "text/plain", "bad body");
        }
        match serde_json::from_str(&s) {
            Ok(v) => v,
            Err(e) => return reply_json(req, 400, &json!({ "error": format!("bad JSON: {e}") })),
        }
    } else {
        Value::Null
    };

    match (method, path.as_str()) {
        (Method::Get, "/") => reply(req, 200, "text/html; charset=utf-8", asset("index.html", INDEX)),
        (Method::Get, "/app.js") => reply(req, 200, "text/javascript; charset=utf-8", asset("app.js", APP_JS)),
        (Method::Get, "/style.css") => reply(req, 200, "text/css; charset=utf-8", asset("style.css", STYLE)),
        (Method::Get, "/api/events") => events(app, req),
        (Method::Get, "/media") => match q.iter().find(|(k, _)| k == "path") {
            Some((_, p)) => media(app, req, PathBuf::from(p)),
            None => reply(req, 400, "text/plain", "missing path"),
        },
        (Method::Get, "/thumb") => {
            let t = param(&q, "t").and_then(|t| t.parse().ok()).unwrap_or(0.0);
            match param(&q, "path").map(|p| dups::thumb(app, std::path::Path::new(p), t)) {
                Some(Ok(jpeg)) => reply(req, 200, "image/jpeg", jpeg),
                Some(Err(e)) => reply(req, 404, "text/plain", format!("{e:#}")),
                None => reply(req, 400, "text/plain", "missing path"),
            }
        }
        (m, p) if p.starts_with("/api/") => {
            let r = api(app, &m, p, &q, &body);
            match r {
                Ok(v) => reply_json(req, 200, &v),
                Err(e) => {
                    let status = if e.to_string().starts_with("not found") { 404 } else { 400 };
                    reply_json(req, status, &err_json(&e))
                }
            }
        }
        _ => reply(req, 404, "text/plain", "not found"),
    }
}

fn param<'a>(q: &'a [(String, String)], k: &str) -> Option<&'a str> {
    q.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str())
}

fn api(app: &Arc<App>, m: &Method, path: &str, q: &[(String, String)], body: &Value) -> Result<Value> {
    let seg: Vec<&str> = path.trim_start_matches("/api/").split('/').collect();
    match (m, seg.as_slice()) {
        (Method::Get, ["state"]) => Ok(library::state(app)),
        (Method::Get, ["browse"]) => library::browse(param(q, "path"), param(q, "hidden") == Some("1")),
        (Method::Post, ["scans"]) => library::start_scan(app, body),
        (Method::Get, ["scans", id]) => library::scan_result(app, id.parse()?),
        (Method::Get, ["dups"]) => Ok(dups::list(app)),
        (Method::Post, ["dups"]) => dups::start(app, body),
        (Method::Post, ["dups", "pair"]) => dups::pair(app, body),
        (Method::Get, ["dups", id]) => dups::get(app, id.parse()?),
        (Method::Post, ["dups", id, "cancel"]) => dups::cancel(app, id.parse()?),
        (Method::Post, ["dups", id, "trash"]) => dups::trash(app, id.parse()?, body),
        (Method::Get, ["bench"]) => bench::get(app),
        (Method::Post, ["bench"]) => bench::start(app, body),
        (Method::Post, ["bench", "cancel"]) => bench::cancel(app),
        (Method::Post, ["bench", "tuning"]) => bench::tune(app, body),
        (Method::Get, ["jobs"]) => Ok(jobs::list(app)),
        (Method::Get, ["jobs", "heads"]) => Ok(jobs::heads(app)),
        (Method::Post, ["jobs", "unrestored", "retry"]) => jobs::unrestored_action(app, true),
        (Method::Post, ["jobs", "unrestored", "forget"]) => jobs::unrestored_action(app, false),
        (Method::Get, ["jobs", id]) => jobs::get(app, id.parse()?),
        (Method::Get, ["jobs", id, "files", i]) => jobs::get_file(app, id.parse()?, i.parse()?),
        (Method::Post, ["jobs"]) => jobs::add(app, body),
        (Method::Post, ["jobs", id, "cancel"]) => jobs::cancel(app, id.parse()?),
        (Method::Post, ["jobs", id, "remove"]) => jobs::remove(app, id.parse()?),
        (Method::Post, ["jobs", "clear"]) => jobs::clear_finished(app),
        (Method::Get, ["history"]) => Ok(library::history(app)),
        (Method::Post, ["settings", "tools"]) => library::set_tools(app, body),
        (Method::Post, ["history", "dirs"]) => library::add_history_dir(app, body),
        (Method::Post, ["history", "dirs", "remove"]) => library::remove_history_dir(app, body),
        (Method::Get, ["report"]) => library::report(app, param(q, "path").ok_or_else(|| anyhow!("missing path"))?),
        (Method::Post, ["preview"]) => library::preview(app, body),
        (Method::Post, ["meta", "preview"]) => library::meta_preview(app, body),
        (Method::Post, ["source"]) => library::source(app, body),
        _ => bail!("not found: {path}"),
    }
}

/// Server-Sent Events: one JSON message per line, keep-alive comments every 15 s.
fn events(app: &App, req: Request) {
    let (tx, rx) = mpsc::channel::<String>();
    app.subscribers.lock().unwrap().push(tx);
    let mut w = req.into_writer();
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\n\r\n";
    if w.write_all(head.as_bytes()).and_then(|_| w.write_all(b"retry: 2000\n\n")).and_then(|_| w.flush()).is_err() {
        return;
    }
    loop {
        let chunk = match rx.recv_timeout(Duration::from_secs(15)) {
            Ok(msg) => format!("data: {msg}\n\n"),
            Err(mpsc::RecvTimeoutError::Timeout) => ":\n\n".to_string(),
            Err(_) => return,
        };
        if w.write_all(chunk.as_bytes()).and_then(|_| w.flush()).is_err() {
            return;
        }
    }
}

fn content_type(p: &std::path::Path) -> &'static str {
    match p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("mp4" | "m4v") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mkv") => "video/x-matroska",
        Some("mov") => "video/quicktime",
        Some("ts" | "m2ts") => "video/mp2t",
        Some("avi") => "video/x-msvideo",
        _ => "application/octet-stream",
    }
}

/// Serves an allowed file with HTTP range support (the video element seeks with ranges).
fn media(app: &App, req: Request, path: PathBuf) {
    let Some(path) = app.is_allowed(&path) else {
        return reply(req, 403, "text/plain", "not a file you picked");
    };
    let Ok(mut f) = std::fs::File::open(&path) else { return reply(req, 404, "text/plain", "cannot open") };
    let len = f.metadata().map_or(0, |m| m.len());
    let range = get_header(&req, "Range").and_then(|r| parse_range(r, len));
    let ctype = content_type(&path);
    let r = match range {
        Some((start, end)) => {
            if f.seek(SeekFrom::Start(start)).is_err() {
                return reply(req, 500, "text/plain", "seek failed");
            }
            let n = end - start + 1;
            Response::new(StatusCode(206), vec![], Box::new(f.take(n)) as Box<dyn Read + Send>, Some(n as usize), None)
                .with_header(header("Content-Range", &format!("bytes {start}-{end}/{len}")))
        }
        None if get_header(&req, "Range").is_some() => {
            let r = Response::empty(416).with_header(header("Content-Range", &format!("bytes */{len}")));
            let _ = req.respond(hardened(r));
            return;
        }
        None => Response::new(StatusCode(200), vec![], Box::new(f) as Box<dyn Read + Send>, Some(len as usize), None),
    };
    let r = r.with_header(header("Content-Type", ctype)).with_header(header("Accept-Ranges", "bytes"));
    let _ = req.respond(hardened(r));
}

/// `bytes=a-b`, `bytes=a-` or `bytes=-n` (one range only) to inclusive offsets.
fn parse_range(h: &str, len: u64) -> Option<(u64, u64)> {
    let spec = h.strip_prefix("bytes=")?.split(',').next()?.trim();
    let (a, b) = spec.split_once('-')?;
    let (start, end) = match (a.trim(), b.trim()) {
        ("", n) => {
            let n: u64 = n.parse().ok()?;
            (len.saturating_sub(n), len.checked_sub(1)?)
        }
        (s, "") => (s.parse().ok()?, len.checked_sub(1)?),
        (s, e) => (s.parse().ok()?, e.parse::<u64>().ok()?.min(len.checked_sub(1)?)),
    };
    (start <= end && start < len).then_some((start, end))
}

fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|s| !s.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-", 100), Some((0, 99)));
        assert_eq!(parse_range("bytes=10-19", 100), Some((10, 19)));
        assert_eq!(parse_range("bytes=90-200", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=-10", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=100-", 100), None);
        assert_eq!(parse_range("bytes=0-", 0), None);
    }

    #[test]
    fn decoding() {
        assert_eq!(percent_decode("a%20b+c%2Fd"), "a b c/d");
        assert_eq!(percent_decode("%C3%A9%"), "é%");
        assert_eq!(percent_decode("%4"), "%4");
    }
}
