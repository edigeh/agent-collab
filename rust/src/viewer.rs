//! Loopback-only, read-only web viewer (port of `viewer.py`), with the web
//! assets compiled into the binary.

use crate::argparse::{parse_args, report, Spec};
use crate::board::{self, Board, BRIEF_VIEWS};
use crate::error::{Error, Result};
use crate::json::{self, Map, Value};
use crate::obj;
use crate::presence;
use crate::pyfmt::{self, Ts};
use crate::state::{self, T};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::Duration;

const INDEX: &[u8] = include_bytes!("../../collab_core/web/index.html");
const APP: &[u8] = include_bytes!("../../collab_core/web/app.js");
const STYLE: &[u8] = include_bytes!("../../collab_core/web/style.css");

const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; object-src 'none'; frame-ancestors 'none'; base-uri 'none'";

/// `viewer.snapshot`: the board for the browser, from one lock over the journal.
pub fn snapshot(board: &Board, view: &str) -> Result<Value> {
    if !BRIEF_VIEWS.contains(&view) {
        return Err(Error::Value("view must be compact or full".into()));
    }
    let (s, records) = board.read_history()?;
    let mut history = Map::new();
    for record in &records {
        let event = &record.payload;
        if !event.at("accepted").truthy() {
            continue;
        }
        let req = event.at("request");
        let data = req.at("data");
        let op = req.at("op").as_str().unwrap_or("");
        let mut targets: Vec<Value> = if op == "post.create" { vec![req.at("id").clone()] } else { Vec::new() };
        if op.starts_with("post.") && data.at("post").truthy() {
            targets.push(data.at("post").clone());
        }
        let outcomes: &[Value] = data.at("outcomes").as_arr().map(Vec::as_slice).unwrap_or(&[]);
        if op == "moderation.apply" {
            targets.extend(outcomes.iter().map(|o| o.at("post").clone()));
        }
        for target in targets {
            let detail = outcomes.iter().find(|o| o.at("post").py_eq(&target)).unwrap_or(data).clone();
            let entry = obj! {"operation" => req.at("op").clone(), "actor" => req.at("actor").clone(), "at" => event.at("recorded_at").clone(), "detail" => detail};
            let key = state::py_str(&target);
            match history.get_mut(&key) {
                Some(Value::Arr(list)) => list.push(entry),
                _ => history.insert(key, vec![entry]),
            }
        }
    }
    let sequence = records.last().map(|r| Value::from(r.seq)).unwrap_or(Value::Int(0));
    crate::discard(records);
    let agents = presence::everyone(&s, &Ts::now());
    if view == "compact" {
        let projects: Vec<Value> = s.values(T::Projects).map(|p| obj! {"id" => p.at("id").clone(), "name" => p.at("name").clone(), "revision" => p.at("revision").clone()}).collect();
        let sessions: Vec<Value> = s
            .values(T::Sessions)
            .map(|x| {
                obj! {
                    "id" => x.at("id").clone(), "name" => x.at("name").clone(), "harness" => x.at("harness").clone(),
                    "parent" => x.at("parent").clone(), "project" => x.at("project").clone(), "seen_at" => x.at("seen_at").clone(),
                }
            })
            .collect();
        let mut posts = Vec::new();
        for p in s.values(T::Posts) {
            posts.push(board::render_item("post", p, "compact", board::COMPACT_PREVIEW_LIMIT)?);
        }
        let mut tasks = Vec::new();
        for t in s.values(T::Tasks) {
            tasks.push(board::render_item("task", t, "compact", board::COMPACT_PREVIEW_LIMIT)?);
        }
        let mut counts = Map::new();
        for (k, v) in history.iter() {
            counts.push(k.clone(), v.as_arr().map(Vec::len).unwrap_or(0));
        }
        return Ok(obj! {
            "view" => "compact", "projects" => projects, "sessions" => sessions, "posts" => posts, "tasks" => tasks,
            "history_counts" => counts, "presence" => agents, "sequence" => sequence,
        });
    }
    let list = |t: T| s.values(t).cloned().collect::<Vec<_>>();
    Ok(obj! {
        "projects" => list(T::Projects), "sessions" => list(T::Sessions), "posts" => list(T::Posts), "tasks" => list(T::Tasks),
        "view" => "full", "history" => history, "presence" => agents, "sequence" => sequence,
    })
}

// ---------------------------------------------------------------- HTTP

struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

enum Parse {
    Ok(Request),
    Bad(u16, String),
    Closed,
}

fn read_request(stream: &mut BufReader<TcpStream>) -> Parse {
    let mut line = Vec::new();
    match stream.by_ref().take(65537).read_until(b'\n', &mut line) {
        Ok(0) | Err(_) => return Parse::Closed,
        Ok(_) => {}
    }
    if line.len() > 65536 {
        return Parse::Bad(414, "Request-URI Too Long".into());
    }
    let text = String::from_utf8_lossy(&line).trim_end_matches(['\r', '\n']).to_string();
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return Parse::Closed;
    }
    if words.len() != 3 || !words[2].starts_with("HTTP/") {
        return Parse::Bad(400, format!("Bad request syntax ({})", state::py_repr(&Value::from(text.as_str()))));
    }
    let mut headers = Vec::new();
    loop {
        let mut h = Vec::new();
        match stream.by_ref().take(65537).read_until(b'\n', &mut h) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if h.len() > 65536 {
            return Parse::Bad(431, "Line too long".into());
        }
        let h = String::from_utf8_lossy(&h).into_owned();
        let h = h.trim_end_matches(['\r', '\n']);
        if h.is_empty() {
            break;
        }
        if headers.len() >= 100 {
            return Parse::Bad(431, "Too many headers".into());
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Parse::Ok(Request { method: words[0].to_string(), target: words[1].to_string(), headers })
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        414 => "Request-URI Too Long",
        431 => "Request Header Fields Too Large",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

fn http_date() -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let days = now.div_euclid(86400);
    let secs = now.rem_euclid(86400);
    let (y, m, d) = pyfmt::civil_from_days(days);
    let weekday = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][days.rem_euclid(7) as usize];
    let month = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"][(m - 1) as usize];
    format!("{weekday}, {d:02} {month} {y} {:02}:{:02}:{:02} GMT", secs / 3600, secs / 60 % 60, secs % 60)
}

fn reply(stream: &mut TcpStream, status: u16, body: &[u8], mime: &str) {
    let head = format!(
        "HTTP/1.0 {status} {}\r\nServer: collab-viewer\r\nDate: {}\r\nContent-Type: {mime}; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: {CSP}\r\n\r\n",
        reason(status),
        http_date(),
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#x27;")
}

/// `BaseHTTPRequestHandler.send_error`.
fn send_error(stream: &mut TcpStream, status: u16, message: &str, explain: &str) {
    let body = format!(
        "<!DOCTYPE HTML>\n<html lang=\"en\">\n    <head>\n        <meta charset=\"utf-8\">\n        <title>Error response</title>\n    </head>\n    <body>\n        <h1>Error response</h1>\n        <p>Error code: {status}</p>\n        <p>Message: {}.</p>\n        <p>Error code explanation: {status} - {}.</p>\n    </body>\n</html>\n",
        html_escape(message),
        html_escape(explain)
    );
    let head = format!(
        "HTTP/1.0 {status} {message}\r\nServer: collab-viewer\r\nDate: {}\r\nConnection: close\r\nContent-Type: text/html;charset=utf-8\r\nContent-Length: {}\r\n\r\n",
        http_date(),
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

/// `urlsplit(target)` reduced to (path, query).
fn split_target(target: &str) -> (String, String) {
    let target = target.split('#').next().unwrap_or("");
    let (mut rest, query) = match target.split_once('?') {
        Some((a, b)) => (a, b),
        None => (target, ""),
    };
    if let Some(colon) = rest.find(':') {
        let scheme = &rest[..colon];
        if !scheme.is_empty()
            && scheme.as_bytes()[0].is_ascii_alphabetic()
            && scheme.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.'))
        {
            rest = &rest[colon + 1..];
        }
    }
    if let Some(after) = rest.strip_prefix("//") {
        rest = after.find('/').map(|i| &after[i..]).unwrap_or("");
    }
    (rest.to_string(), query.to_string())
}

fn unquote_plus(s: &str) -> String {
    let b = s.replace('+', " ").into_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| (c as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `parse_qs(query).get('view', ['full'])[0]`.
fn view_param(query: &str) -> String {
    for pair in query.split('&') {
        let Some((name, value)) = pair.split_once('=') else { continue };
        if value.is_empty() {
            continue;
        }
        if unquote_plus(name) == "view" {
            return unquote_plus(value);
        }
    }
    "full".into()
}

type Job = (String, mpsc::Sender<(u16, Vec<u8>)>);

fn handle(mut stream: TcpStream, port: u16, jobs: mpsc::Sender<Job>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
    let Ok(clone) = stream.try_clone() else { return };
    let mut reader = BufReader::new(clone);
    let request = match read_request(&mut reader) {
        Parse::Ok(r) => r,
        Parse::Bad(code, message) => return send_error(&mut stream, code, &message, ""),
        Parse::Closed => return,
    };
    if request.method != "GET" {
        let message = format!("Unsupported method ({})", state::py_repr(&Value::from(request.method.as_str())));
        return send_error(&mut stream, 501, &message, "Server does not support this operation");
    }
    let allowed = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    let host_ok = request.header("Host").is_some_and(|h| allowed.iter().any(|a| a == h));
    let origin_ok = match request.header("Origin") {
        Some(o) if !o.is_empty() => allowed.iter().any(|a| format!("http://{a}") == o),
        _ => true,
    };
    if !host_ok || !origin_ok {
        return reply(&mut stream, 403, b"Forbidden", "text/plain");
    }
    let (path, query) = split_target(&request.target);
    if path == "/api/board" {
        let view = view_param(&query);
        if !BRIEF_VIEWS.contains(&view.as_str()) {
            let body = json::dumps(&obj! {"error" => "view must be compact or full"}, json::DEFAULT);
            return reply(&mut stream, 400, body.as_bytes(), "application/json");
        }
        let (tx, rx) = mpsc::channel();
        if jobs.send((view, tx)).is_err() {
            return;
        }
        if let Ok((status, body)) = rx.recv() {
            reply(&mut stream, status, &body, "application/json");
        }
        return;
    }
    let asset = match path.as_str() {
        "/" => Some((INDEX, "text/html")),
        "/app.js" => Some((APP, "text/javascript")),
        "/style.css" => Some((STYLE, "text/css")),
        _ => None,
    };
    match asset {
        Some((body, mime)) => reply(&mut stream, 200, body, mime),
        None => reply(&mut stream, 404, b"Not found", "text/plain"),
    }
}

/// The board lives on one thread (it caches the replayed view between requests);
/// connection threads hand it snapshot requests.
fn board_worker(home: String, receipts: Option<String>, jobs: mpsc::Receiver<Job>) {
    let board = Board::new(&home, receipts.as_deref());
    for (view, reply_to) in jobs {
        let result = match snapshot(&board, &view) {
            Ok(v) => (200, json::dumps(&v, json::VIEWER).into_bytes()),
            Err(e @ (Error::Journal(_) | Error::Busy(_) | Error::Corrupt(_) | Error::Os(_) | Error::Value(_) | Error::Rejected(_) | Error::Cli(_))) => {
                (503, json::dumps(&obj! {"error" => format!("Board unavailable: {}", e.message())}, json::DEFAULT).into_bytes())
            }
            Err(Error::Crash(_)) => continue,
        };
        let _ = reply_to.send(result);
    }
}

fn spec() -> Spec {
    let mut p = Spec::new("@viewer");
    p.store("--home").default(&std::env::var("COLLAB_HOME").unwrap_or_else(|_| "~/.agent-collab".into()));
    p.store("--receipts").default_opt(std::env::var("COLLAB_RECEIPTS").ok());
    p.store("--port").int(Some(8765));
    p
}

pub fn main(argv: Vec<String>) -> i32 {
    let parsed = match parse_args(&spec(), &argv) {
        Ok(p) => p,
        Err(e) => return report(|_| "collab-viewer".into(), e),
    };
    let get = |k: &str| match parsed.ns.get(k) {
        Some(crate::argparse::Val::Str(s)) => Some(s.clone()),
        _ => None,
    };
    let port = match parsed.ns.get("port") {
        Some(crate::argparse::Val::Int(Value::Int(p))) if (0..=65535).contains(p) => *p as u16,
        _ => {
            eprintln!("OverflowError: bind(): port must be 0-65535.");
            return 1;
        }
    };
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("OSError: {}", pyfmt::os_error_text(&e));
            return 1;
        }
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    let (tx, rx) = mpsc::channel::<Job>();
    let home = get("home").unwrap_or_default();
    let receipts = get("receipts");
    std::thread::spawn(move || board_worker(home, receipts, rx));
    println!("Agent Collab viewer: http://127.0.0.1:{port}");
    let _ = std::io::stdout().flush();
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let jobs = tx.clone();
        std::thread::spawn(move || handle(stream, port, jobs));
    }
    0
}
