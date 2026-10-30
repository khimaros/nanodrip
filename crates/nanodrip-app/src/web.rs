//! tiny http server for the status page and its json api.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::feed::Shared;

const INDEX: &str = include_str!("index.html");
const READ_TIMEOUT: Duration = Duration::from_secs(2);
const WRITE_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_CONNECTIONS: usize = 4;
const PAGE_ROWS: usize = 16;
const DEFAULT_SUMMARY_HOURS: u32 = 24;
const MAX_SUMMARY_HOURS: usize = 168;
const SECONDS_PER_HOUR: u32 = 3600;
/// holds the request buffers and the rendered reading history.
const CONNECTION_STACK: usize = 6144;
/// certificate parts are posted as hex, so this has to fit a few kilobytes.
const MAX_REQUEST: usize = 16384;
/// browsers cannot add this header cross-origin without a preflight we never
/// answer, which keeps other sites from changing the pairing.
const GUARD_HEADER: &str = "x-nanodrip:";
const HEAD_END: &[u8] = b"\r\n\r\n";
const JSON: &str = "application/json";
const HTML: &str = "text/html; charset=utf-8";
const TEXT: &str = "text/plain";

enum Body {
    Text(String),
    Static(&'static str),
    /// written as it is read: a week of history does not fit in memory as json.
    Readings(usize),
}

struct Response {
    status: &'static str,
    content_type: &'static str,
    body: Body,
}

fn text(status: &'static str, body: &str) -> Response {
    Response {
        status,
        content_type: TEXT,
        body: Body::Text(body.to_string()),
    }
}

fn json(body: Body) -> Response {
    Response {
        status: "200 OK",
        content_type: JSON,
        body,
    }
}

fn number_param(query: &str, name: &str) -> Option<usize> {
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
        .and_then(|value| value.parse().ok())
}

fn summary_json(shared: &Shared, query: &str) -> String {
    let hours = number_param(query, "hours").map_or(DEFAULT_SUMMARY_HOURS, |hours| {
        hours.min(MAX_SUMMARY_HOURS) as u32
    });
    let exact = number_param(query, "from").zip(number_param(query, "to"));
    let (low, high) = shared.settings.lock().unwrap().target();
    let store = shared.store.lock().unwrap();
    let (start, end) = exact.map_or_else(
        || store.recent_range(hours * SECONDS_PER_HOUR),
        |(from, to)| (from as u32, to as u32),
    );
    let totals = store.range_totals(start, end, low, high);
    let mean = totals
        .mean()
        .map_or("null".to_string(), |mean| format!("{mean:.1}"));
    let hours = exact.map_or(hours.to_string(), |_| "null".to_string());
    format!(
        concat!(
            r#"{{"hours":{},"above_seconds":{},"below_seconds":{},"#,
            r#""in_range_seconds":{},"mean":{}}}"#
        ),
        hours, totals.above, totals.below, totals.within, mean,
    )
}

fn hex_digit(byte: u8) -> Option<u8> {
    (byte as char).to_digit(16).map(|digit| digit as u8)
}

/// undoes form encoding; a malformed escape is kept as written.
fn url_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let escape = (bytes[at] == b'%' && at + 2 < bytes.len())
            .then(|| Some(hex_digit(bytes[at + 1])? << 4 | hex_digit(bytes[at + 2])?))
            .flatten();
        match (escape, bytes[at]) {
            (Some(byte), _) => {
                out.push(byte);
                at += 3;
            }
            (None, b'+') => {
                out.push(b' ');
                at += 1;
            }
            (None, byte) => {
                out.push(byte);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn form_value(body: &str, name: &str) -> String {
    body.split('&')
        .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
        .map(url_decode)
        .unwrap_or_default()
}

fn json_string(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// reports pairing as flags only: the code, certificates and wifi password
/// never leave.
fn status_json(shared: &Shared) -> String {
    let store = shared.store.lock().unwrap();
    let settings = shared.settings.lock().unwrap();
    let latest = store.latest().map_or("null".to_string(), |e| e.to_json());
    let last_error = shared.last_error.lock().unwrap();
    let last_error = last_error.map_or("null".to_string(), json_string);
    let mqtt_broker = settings
        .mqtt
        .as_ref()
        .map_or("null".to_string(), |mqtt| json_string(&mqtt.address));
    let wifi_ssid = settings
        .wifi_ssid
        .as_deref()
        .map_or("null".to_string(), json_string);
    format!(
        concat!(
            r#"{{"device":{},"latest":{},"count":{},"mqtt_pending":{},"#,
            r#""paired":{},"certificates":{},"wifi_ssid":{},"last_error":{},"#,
            r#""target_low":{},"target_high":{},"mqtt_broker":{}}}"#
        ),
        json_string(&shared.device),
        latest,
        store.len(),
        shared.pending.load(Ordering::Relaxed),
        settings.paired(),
        settings.has_certificates(),
        wifi_ssid,
        last_error,
        settings.target().0,
        settings.target().1,
        mqtt_broker,
    )
}

fn applied(ok: bool) -> Response {
    if ok {
        text("200 OK", "ok\n")
    } else {
        text("400 Bad Request", "invalid value\n")
    }
}

fn route_post(path: &str, body: &str, shared: &Shared) -> Response {
    let mut settings = shared.settings.lock().unwrap();
    match path {
        "/api/pair" => applied(settings.pair(&form_value(body, "code"))),
        "/api/certificates" => applied(settings.set_certificates(
            &form_value(body, "p1"),
            &form_value(body, "p2"),
            &form_value(body, "p3"),
        )),
        "/api/wifi" => {
            applied(settings.set_wifi(&form_value(body, "ssid"), &form_value(body, "password")))
        }
        "/api/target" => {
            applied(settings.set_target(&form_value(body, "low"), &form_value(body, "high")))
        }
        "/api/mqtt" => applied(settings.set_mqtt(
            &form_value(body, "address"),
            &form_value(body, "user"),
            &form_value(body, "password"),
        )),
        "/api/forget" => {
            settings.forget();
            applied(true)
        }
        _ => text("405 Method Not Allowed", "method not allowed\n"),
    }
}

fn route(request: &Request, shared: &Shared) -> Response {
    let (path, query) = request
        .target
        .split_once('?')
        .unwrap_or((&request.target, ""));
    if request.method == "POST" && path.starts_with("/api/") {
        return if request.guarded {
            route_post(path, &request.body, shared)
        } else {
            text("403 Forbidden", "missing x-nanodrip header\n")
        };
    }
    if request.method != "GET" {
        return text("405 Method Not Allowed", "method not allowed\n");
    }
    match path {
        "/" => Response {
            status: "200 OK",
            content_type: HTML,
            body: Body::Static(INDEX),
        },
        "/api/status" => json(Body::Text(status_json(shared))),
        "/api/summary" => json(Body::Text(summary_json(shared, query))),
        "/api/readings" => json(Body::Readings(
            number_param(query, "limit").unwrap_or(usize::MAX),
        )),
        _ => text("404 Not Found", "not found\n"),
    }
}

struct Request {
    method: String,
    target: String,
    guarded: bool,
    body: String,
}

fn head_end(data: &[u8]) -> Option<usize> {
    data.windows(HEAD_END.len())
        .position(|window| window == HEAD_END)
        .map(|at| at + HEAD_END.len())
}

fn content_length(head: &str) -> usize {
    head.lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            let matches = name.eq_ignore_ascii_case("content-length");
            matches.then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0)
}

fn parse_request(data: &[u8], end: usize) -> Option<Request> {
    let head = std::str::from_utf8(&data[..end]).ok()?;
    let body = String::from_utf8(data[end..].to_vec()).ok()?;
    let mut parts = head.split_whitespace();
    let guarded = head
        .lines()
        .any(|line| line.to_ascii_lowercase().starts_with(GUARD_HEADER));
    Some(Request {
        method: parts.next()?.to_string(),
        target: parts.next()?.to_string(),
        guarded,
        body,
    })
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut data = Vec::new();
    let mut chunk = [0u8; 512];
    while data.len() < MAX_REQUEST {
        if let Some(end) = head_end(&data) {
            let head = std::str::from_utf8(&data[..end]).ok()?;
            if data.len() >= end + content_length(head) {
                return parse_request(&data, end);
            }
        }
        let size = stream.read(&mut chunk).ok()?;
        if size == 0 {
            return None;
        }
        data.extend_from_slice(&chunk[..size]);
    }
    None
}

fn handle(mut stream: TcpStream, shared: &Shared) -> std::io::Result<()> {
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let Some(request) = read_request(&mut stream) else {
        return Ok(());
    };
    let response = route(&request, shared);
    let (status, content_type) = (response.status, response.content_type);
    match response.body {
        Body::Text(body) => write_body(&mut stream, status, content_type, &body),
        Body::Static(body) => write_body(&mut stream, status, content_type, body),
        Body::Readings(limit) => {
            write!(
                stream,
                "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\nconnection: close\r\n\r\n"
            )?;
            stream_readings(&mut stream, shared, limit)
        }
    }
}

fn write_body(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n",
        body.len()
    )?;
    stream.write_all(b"connection: close\r\n\r\n")?;
    stream.write_all(body.as_bytes())
}

/// writes the newest `limit` readings as a json array, a page at a time and
/// without holding the store while a slow client catches up.
fn stream_readings(stream: &mut TcpStream, shared: &Shared, limit: usize) -> std::io::Result<()> {
    let mut clock = shared.store.lock().unwrap().tail_start(limit);
    let mut remaining = limit;
    let mut separator = "[";
    while remaining > 0 {
        let count = PAGE_ROWS.min(remaining);
        let page = shared.store.lock().unwrap().page(clock, count);
        let Some(last) = page.last() else { break };
        clock = last.clock + 1;
        remaining -= page.len();
        let mut chunk = String::new();
        for entry in &page {
            chunk.push_str(separator);
            chunk.push_str(&entry.to_json());
            separator = ",";
        }
        stream.write_all(chunk.as_bytes())?;
    }
    stream.write_all(if separator == "[" { b"[]" } else { b"]" })
}

/// answers requests forever; run this on its own thread. each connection gets
/// a thread so a silent one, like a browser's speculative socket, cannot hold
/// up the rest. past the limit, connections are dropped to protect the heap.
pub fn serve(listener: TcpListener, shared: Arc<Shared>) {
    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming().flatten() {
        if active.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
            continue;
        }
        active.fetch_add(1, Ordering::Relaxed);
        let (worker_active, worker_shared) = (Arc::clone(&active), Arc::clone(&shared));
        let spawned = thread::Builder::new()
            .stack_size(CONNECTION_STACK)
            .spawn(move || {
                let _ = handle(stream, &worker_shared);
                worker_active.fetch_sub(1, Ordering::Relaxed);
            });
        if spawned.is_err() {
            active.fetch_sub(1, Ordering::Relaxed);
        }
    }
}
