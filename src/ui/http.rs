//! Request handling: session check, routing, static files, the event stream and media ranges.

use super::{App, bench, dups, err_json, fields, hold, jobs, library, watch};
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

/// What the page is: its own script and style files, video from this server or from memory,
/// pictures from this server and the icon written into it. Nothing may frame it.
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; media-src 'self' blob:; img-src 'self' data:; style-src 'self'; script-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// Security headers sent with every response. Cross-Origin-Resource-Policy keeps a video or a
/// picture from being taken into a page of another port, which counts as the same site.
const SECURITY_HEADERS: [(&str, &str); 6] = [
    ("X-Content-Type-Options", "nosniff"),
    ("X-Frame-Options", "DENY"),
    ("Referrer-Policy", "no-referrer"),
    ("Cache-Control", "no-store"),
    ("Cross-Origin-Resource-Policy", "same-origin"),
    ("Content-Security-Policy", CONTENT_SECURITY_POLICY),
];

/// The largest request body. A list of paths is what gets big: this holds some tens of
/// thousands of them.
const MAX_BODY: usize = 8 << 20;

/// What to say about a request over [`MAX_BODY`], of which `start` was read: the paths in that
/// part are about as many as fit.
fn too_large(start: &[u8]) -> String {
    let items = start.windows(3).filter(|w| w == b"\",\"").count() + 1;
    let fits = if items > 1 { format!(", which is about {items} items of this length") } else { String::new() };
    format!("this selection is too large to send at once: a request can hold {} MiB{fits}. Select fewer and do the rest afterwards", MAX_BODY >> 20)
}

fn hardened<R: Read>(r: Response<R>) -> Response<R> {
    SECURITY_HEADERS.iter().fold(r, |r, (name, value)| r.with_header(header(name, value)))
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

/// Whether a browser, if this is one, says the request comes from this page or from the person
/// (an address typed, a link opened from another program). Cookies do not tell ports apart and
/// every port of 127.0.0.1 counts as the same site, so a page of any other local program would
/// otherwise have its requests sent signed in. Other clients do not send the header.
fn from_this_page_or_the_person(req: &Request) -> bool {
    get_header(req, "Sec-Fetch-Site").is_none_or(|site| site == "same-origin" || site == "none")
}

fn has_session(app: &App, req: &Request) -> bool {
    let want = format!("{}={}", cookie_name(app.port), app.token);
    get_header(req, "Cookie").is_some_and(|c| c.split(';').any(|kv| ct_eq(kv.trim().as_bytes(), want.as_bytes())))
}

/// The user whose program holds the other end of a connection, from the kernel's list of
/// TCP sockets (`tcp` is the text of `/proc/net/tcp`): the row whose own address is the
/// client's and whose far address is this server's. `None` where there is no such row.
fn socket_owner(tcp: &str, client: std::net::SocketAddrV4, port: u16) -> Option<u32> {
    socket_row(tcp, client, port)?[7].parse().ok()
}

/// The columns of that row.
fn socket_row(tcp: &str, client: std::net::SocketAddrV4, port: u16) -> Option<Vec<&str>> {
    // Addresses there are the four bytes as the machine holds them, in hex, then the port.
    let hex = |ip: &std::net::Ipv4Addr, port: u16| format!("{:08X}:{port:04X}", u32::from_ne_bytes(ip.octets()));
    let (own, far) = (hex(client.ip(), client.port()), hex(&std::net::Ipv4Addr::LOCALHOST, port));
    tcp.lines().map(|l| l.split_whitespace().collect::<Vec<&str>>()).find(|f| f.len() > 7 && f[1] == own && f[2] == far)
}

/// Whether the kernel's list shows the other end of a connection as user `me`'s or the
/// system's. A socket its program has closed stays listed for a while under user 0, and may
/// be gone altogether: neither says whose it was, so user 0 only counts on a connection
/// still established (state 01), and no row does not count.
fn may_connect(tcp: &str, client: std::net::SocketAddrV4, port: u16, me: u32) -> bool {
    match socket_owner(tcp, client, port) {
        Some(0) => socket_row(tcp, client, port).is_some_and(|row| row[3] == "01"),
        Some(uid) => uid == me,
        None => false,
    }
}

/// Whether the request comes from a program of the user who started this server (or the
/// system's). The cookie alone does not say so: a browser sends it to every port of this
/// host, and a server another user runs on one of them would learn the token from a page
/// opened there. Only where the kernel's list cannot be read at all does the cookie have to
/// do.
fn same_user(app: &App, req: &Request) -> bool {
    let Some(std::net::SocketAddr::V4(client)) = req.remote_addr().copied() else { return true };
    let Ok(tcp) = std::fs::read_to_string("/proc/net/tcp") else { return true };
    // SAFETY: getuid has no failure mode and touches no memory.
    let me = unsafe { libc::getuid() };
    may_connect(&tcp, client, app.port, me)
}

/// Constant-time comparison for the session token.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn handle(app: &Arc<App>, mut req: Request) {
    if !same_host(app, &req) {
        return reply(req, 403, "text/plain", "forbidden: av1opt only answers to http://127.0.0.1");
    }
    if !from_this_page_or_the_person(&req) {
        return reply(req, 403, "text/plain", "forbidden: av1opt does not answer requests from other pages; open its address directly");
    }
    if !same_user(app, &req) {
        return reply(req, 403, "text/plain", "forbidden: this page is for the user who started av1opt");
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
        let mut bytes = Vec::new();
        if req.as_reader().take(MAX_BODY as u64 + 1).read_to_end(&mut bytes).is_err() {
            return reply(req, 400, "text/plain", "the request could not be read");
        }
        if bytes.len() > MAX_BODY {
            return reply_json(req, 413, &json!({ "error": too_large(&bytes) }));
        }
        let Ok(s) = String::from_utf8(bytes) else {
            return reply(req, 400, "text/plain", "the request is not valid text");
        };
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
        (Method::Post, ["scans"]) => {
            fields::check(body, fields::SCAN)?;
            library::start_scan(app, body)
        }
        (Method::Get, ["scans", id]) => library::scan_result(app, id.parse()?),
        (Method::Post, ["scans", id, "pick"]) => library::scan_pick(app, id.parse()?, body),
        (Method::Get, ["dups"]) => Ok(dups::list(app)),
        (Method::Post, ["dups"]) => {
            fields::check(body, fields::DUPS)?;
            dups::start(app, body)
        }
        (Method::Post, ["dups", "pair"]) => dups::pair(app, body),
        (Method::Get, ["dups", id]) => dups::get(app, id.parse()?),
        (Method::Post, ["dups", id, "cancel"]) => dups::cancel(app, id.parse()?),
        (Method::Post, ["dups", id, "trash"]) => dups::trash(app, id.parse()?, body),
        (Method::Get, ["bench"]) => bench::get(app),
        (Method::Post, ["bench"]) => {
            fields::check(body, fields::BENCH)?;
            bench::start(app, body)
        }
        (Method::Post, ["bench", "cancel"]) => bench::cancel(app),
        (Method::Post, ["bench", "tuning"]) => bench::tune(app, body),
        (Method::Get, ["jobs"]) => Ok(jobs::list(app)),
        (Method::Get, ["jobs", "heads"]) => Ok(jobs::heads(app)),
        (Method::Post, ["jobs", "unrestored", "retry"]) => jobs::unrestored_action(app, true),
        (Method::Post, ["jobs", "unrestored", "forget"]) => jobs::unrestored_action(app, false),
        (Method::Get, ["jobs", id]) => jobs::get(app, id.parse()?),
        (Method::Get, ["jobs", id, "files", i]) => jobs::get_file(app, id.parse()?, i.parse()?),
        // Here and not in `jobs::add`: a watch queues through that with the options it saved.
        (Method::Post, ["jobs"]) => {
            fields::check(&body["options"], fields::JOB)?;
            jobs::add(app, body)
        }
        (Method::Post, ["jobs", id, "cancel"]) => jobs::cancel(app, id.parse()?),
        (Method::Post, ["jobs", id, "remove"]) => jobs::remove(app, id.parse()?),
        (Method::Post, ["jobs", id, "first"]) => jobs::first(app, id.parse()?),
        (Method::Post, ["jobs", id, "again"]) => jobs::again(app, id.parse()?),
        (Method::Post, ["jobs", "clear"]) => jobs::clear_finished(app),
        (Method::Post, ["queue", "pause"]) => hold::set_paused(app, body),
        (Method::Post, ["queue", "schedule"]) => hold::set_schedule(app, body),
        (Method::Post, ["queue", "run-now"]) => hold::run_now(app),
        (Method::Get, ["watch"]) => Ok(watch::list(app)),
        (Method::Post, ["watch"]) => {
            fields::check(&body["options"], fields::JOB)?;
            watch::add(app, body)
        }
        (Method::Post, ["watch", id, "remove"]) => watch::remove(app, id.parse()?),
        (Method::Post, ["watch", id, "enabled"]) => watch::set_enabled(app, id.parse()?, body),
        (Method::Post, ["watch", id, "look"]) => watch::look_now(app, id.parse()?),
        (Method::Get, ["history"]) => Ok(library::history(app)),
        (Method::Post, ["settings", "tools"]) => library::set_tools(app, body),
        (Method::Post, ["history", "dirs"]) => library::add_history_dir(app, body),
        (Method::Post, ["history", "dirs", "remove"]) => library::remove_history_dir(app, body),
        (Method::Post, ["replace"]) => library::replace_original(app, body),
        (Method::Post, ["replace", "undo"]) => library::restore_original(app, body),
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
    let security: String = SECURITY_HEADERS.iter().map(|(name, value)| format!("{name}: {value}\r\n")).collect();
    let head = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n{security}\r\n");
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
        return reply(req, 403, "text/plain", library::NOT_LISTED);
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
            // Two digits exactly: `from_str_radix` alone would also take a sign for one.
            b'%' if i + 2 < b.len() && b[i + 1].is_ascii_hexdigit() && b[i + 2].is_ascii_hexdigit() => {
                let digit = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
                out.push(digit(b[i + 1]) << 4 | digit(b[i + 2]));
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    // Bytes that are not text name nothing the page was ever given. Made text they would name
    // another file, so they get a character that no name holds, and so find nothing.
    match String::from_utf8(out) {
        Ok(text) => text,
        Err(e) => format!("{}\0", String::from_utf8_lossy(e.as_bytes())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_other_end_of_a_connection_has_an_owner() {
        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   \
            0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 24653 1 0000000000000000 100 0 0 10 0\n   \
            1: 0100007F:1F90 0100007F:C3A2 01 00000000:00000000 00:00000000 00000000  1000        0 31001 1 0000000000000000 20 4 30 10 -1\n   \
            2: 0100007F:C3A2 0100007F:1F90 01 00000000:00000000 00:00000000 00000000  1001        0 31000 1 0000000000000000 20 4 30 10 -1\n";
        let client = |port| std::net::SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, port);
        if cfg!(target_endian = "little") {
            // The client's own row, not the server's row of the same connection.
            assert_eq!(socket_owner(tcp, client(0xC3A2), 0x1F90), Some(1001));
        }
        assert_eq!(socket_owner(tcp, client(0xC3A3), 0x1F90), None);
        assert_eq!(socket_owner(tcp, client(0xC3A2), 0x1F91), None);
        assert_eq!(socket_owner("", client(0xC3A2), 0x1F90), None);
    }

    #[test]
    fn only_this_users_sockets_and_live_ones_of_the_system_may_connect() {
        if !cfg!(target_endian = "little") {
            return;
        }
        let row = |port: u16, state: &str, uid: u32| {
            format!("   0: 0100007F:{port:04X} 0100007F:1F90 {state} 00000000:00000000 00:00000000 00000000 {uid:5}        0 31000 1 0 20 4 30 10 -1\n")
        };
        let tcp = [row(1, "01", 1000), row(2, "08", 1000), row(3, "01", 1001), row(4, "04", 1001), row(5, "01", 0), row(6, "06", 0), row(7, "04", 0), row(8, "07", 0)].concat();
        let may = |port, me| may_connect(&tcp, std::net::SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, port), 0x1F90, me);
        // This user's, in any state: a program that has stopped sending keeps its socket.
        assert!(may(1, 1000) && may(2, 1000));
        // Another user's.
        assert!(!may(3, 1000) && !may(4, 1000));
        // The system's while connected; afterwards user 0 says nothing about whose it was,
        // for a server running as root too.
        assert!(may(5, 1000) && may(5, 0));
        assert!(!may(6, 1000) && !may(7, 1000) && !may(8, 1000));
        assert!(!may(6, 0) && !may(7, 0));
        // No row at all.
        assert!(!may(9, 1000));
    }

    #[test]
    fn a_request_too_large_is_told_how_much_fits() {
        assert!(too_large(br#"{"paths":["/a","/b","/c"#).contains("about 3 items"));
        // Without a list there is nothing to count.
        let one = too_large(br#"{"note":"xxxx"#);
        assert!(one.contains("8 MiB") && !one.contains("items"), "{one}");
    }

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
        assert_eq!(percent_decode("a%FFb"), "a\u{fffd}b\0");
    }

    #[test]
    fn a_range_of_one_byte_works_at_both_ends() {
        assert_eq!(parse_range("bytes=0-0", 1), Some((0, 0)));
        assert_eq!(parse_range("bytes=-1", 1), Some((0, 0)));
        assert_eq!(parse_range("bytes=0-", 1), Some((0, 0)));
        assert_eq!(parse_range("bytes=99-99", 100), Some((99, 99)));
        assert_eq!(parse_range("bytes=99-", 100), Some((99, 99)));
    }

    #[test]
    fn ranges_past_the_end_are_cut_or_refused() {
        assert_eq!(parse_range("bytes=0-99", 100), Some((0, 99)));
        assert_eq!(parse_range("bytes=0-100", 100), Some((0, 99)));
        assert_eq!(parse_range("bytes=-100", 100), Some((0, 99)));
        assert_eq!(parse_range("bytes=-101", 100), Some((0, 99)));
        assert_eq!(parse_range("bytes=100-100", 100), None);
        assert_eq!(parse_range("bytes=100-200", 100), None);
    }

    #[test]
    fn a_range_of_nothing_is_refused() {
        assert_eq!(parse_range("bytes=-0", 100), None);
        assert_eq!(parse_range("bytes=5-4", 100), None);
        for h in ["bytes=-1", "bytes=-0", "bytes=0-0", "bytes=0-5"] {
            assert_eq!(parse_range(h, 0), None, "{h}");
        }
    }

    #[test]
    fn a_range_that_is_not_one_is_refused() {
        for h in ["", "bytes", "bytes=", "bytes=-", "bytes=a-b", "bytes=1-b", "bytes=a-1", "bytes=1", "bytes=1-2-3", "bytes=--1", "bytes=-1-", "bytes=٠-٩", "0-5", "items=0-5"] {
            assert_eq!(parse_range(h, 100), None, "{h:?}");
        }
    }

    #[test]
    fn only_the_first_of_several_ranges_counts() {
        assert_eq!(parse_range("bytes=0-1,5-6", 100), Some((0, 1)));
        assert_eq!(parse_range("bytes= 2-3 , 5-6", 100), Some((2, 3)));
        assert_eq!(parse_range("bytes=200-300,0-1", 100), None);
    }

    #[test]
    fn the_largest_numbers_do_not_overflow_a_range() {
        let max = u64::MAX;
        assert_eq!(parse_range(&format!("bytes={max}-"), 100), None);
        assert_eq!(parse_range(&format!("bytes=0-{max}"), 100), Some((0, 99)));
        assert_eq!(parse_range(&format!("bytes=-{max}"), 100), Some((0, 99)));
        assert_eq!(parse_range(&format!("bytes={}-{max}", max - 1), max), Some((max - 1, max - 1)));
        assert_eq!(parse_range("bytes=0-99999999999999999999999", 100), None);
    }

    #[test]
    fn the_query_is_split_before_it_is_decoded() {
        let q = parse_query;
        let pair = |k: &str, v: &str| (k.to_string(), v.to_string());
        assert_eq!(q(""), vec![]);
        assert_eq!(q("&&"), vec![]);
        assert_eq!(q("a"), vec![pair("a", "")]);
        assert_eq!(q("a="), vec![pair("a", "")]);
        assert_eq!(q("=v"), vec![pair("", "v")]);
        assert_eq!(q("a=b=c"), vec![pair("a", "b=c")]);
        assert_eq!(q("path=%2Fx%26y%3Dz&t=1.5"), vec![pair("path", "/x&y=z"), pair("t", "1.5")]);
        assert_eq!(q("a%20b=c+d"), vec![pair("a b", "c d")]);
    }

    #[test]
    fn the_first_of_two_parameters_of_one_name_counts() {
        let q = parse_query("path=/a&path=/b&empty=");
        assert_eq!(param(&q, "path"), Some("/a"));
        assert_eq!(param(&q, "empty"), Some(""));
        assert_eq!(param(&q, "other"), None);
        assert_eq!(param(&q, "PATH"), None);
    }

    #[test]
    fn a_percent_sign_without_two_hex_digits_stays_itself() {
        assert_eq!(percent_decode(""), "");
        assert_eq!(percent_decode("%"), "%");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("%2"), "%2");
        assert_eq!(percent_decode("%é"), "%é");
        assert_eq!(percent_decode("%aé"), "%aé");
        assert_eq!(percent_decode("%%41"), "%A");
        assert_eq!(percent_decode("%2541"), "%41");
    }

    #[test]
    fn a_sign_is_not_a_hex_digit() {
        // The percent sign stays as it is, and a plus is a space here as everywhere in a query.
        assert_eq!(percent_decode("%+1"), "% 1");
        assert_eq!(percent_decode("%+f.mkv"), "% f.mkv");
        assert_eq!(percent_decode("%-1"), "%-1");
    }

    #[test]
    fn decoded_text_keeps_its_characters_and_broken_text_finds_no_file() {
        assert_eq!(percent_decode("%E5%AD%97%E5%B9%95.mkv"), "字幕.mkv");
        assert_eq!(percent_decode("été+字"), "été 字");
        assert_eq!(percent_decode("%2f%2F"), "//");
        // Half a character: never a name a page was given.
        assert!(percent_decode("%C3").ends_with('\0'));
        assert!(percent_decode("a%E5%AD").ends_with('\0'));
    }

    #[test]
    fn video_types_go_by_extension_in_any_case() {
        for (name, want) in [
            ("a.mp4", "video/mp4"),
            ("a.M4V", "video/mp4"),
            ("a.WebM", "video/webm"),
            ("a.b.mkv", "video/x-matroska"),
            ("a.mov", "video/quicktime"),
            ("a.ts", "video/mp2t"),
            ("a.m2ts", "video/mp2t"),
            ("a.avi", "video/x-msvideo"),
            ("a.txt", "application/octet-stream"),
            ("mkv", "application/octet-stream"),
            (".mkv", "application/octet-stream"),
            ("", "application/octet-stream"),
        ] {
            assert_eq!(content_type(std::path::Path::new(name)), want, "{name:?}");
        }
    }

    #[test]
    fn tokens_are_equal_only_when_every_byte_is() {
        assert!(ct_eq(b"", b""));
        assert!(ct_eq(b"token", b"token"));
        assert!(!ct_eq(b"token", b"tokeN"));
        assert!(!ct_eq(b"Token", b"token"));
        assert!(!ct_eq(b"token", b"token "));
        assert!(!ct_eq(b"", b"t"));
    }

    #[test]
    fn each_port_has_its_own_cookie() {
        assert_eq!(cookie_name(8080), "av1opt_8080");
        assert_ne!(cookie_name(8080), cookie_name(8081));
    }

    #[test]
    fn a_short_or_garbled_row_of_the_socket_list_names_no_owner() {
        if !cfg!(target_endian = "little") {
            return;
        }
        let client = std::net::SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, 0xC3A2);
        // Too few columns, and an owner that is not a number.
        assert_eq!(socket_owner("0: 0100007F:C3A2 0100007F:1F90 01 0 0 0\n", client, 0x1F90), None);
        let garbled = "0: 0100007F:C3A2 0100007F:1F90 01 0:0 0:0 0 nobody 0 1\n";
        assert_eq!(socket_owner(garbled, client, 0x1F90), None);
        assert!(!may_connect(garbled, client, 0x1F90, 1000));
        // The ports are written with four digits, so a low port still matches.
        let low = std::net::SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, 80);
        assert_eq!(socket_owner("0: 0100007F:0050 0100007F:0051 01 0:0 0:0 0 1000 0 1\n", low, 81), Some(1000));
    }
}
