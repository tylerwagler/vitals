//! The HTTP API: request/reply JSON, and a Server-Sent Events stream.
//!
//! The stream follows pulsar-server's /metrics/stream: one connection per
//! subscriber, `: keepalive` comments while there is nothing to say, a cap on
//! subscribers that answers 503 + Retry-After instead of queueing, and no
//! backpressure: each subscriber is sent the latest sample when it is due, so
//! a slow reader skips samples instead of holding anything up. Unlike pulsar's,
//! the event carries the sample itself: here the sample is the whole point.
//!
//! HTTP/1.1 by hand on std's TcpListener, a thread per connection: the API is
//! GETs, keep-alive and SSE, and a framework's runtime would cost more than
//! the parsing it saves.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use flate2::write::GzEncoder;
use flate2::Compression;
use serde_json::{json, Value};

use crate::collect::processes::SORTS;
use crate::hub::{pick, Hub, LIVE_SECTIONS, STATIC_ENVELOPE, STATIC_SECTIONS};
use crate::legacy::Exporter;
use crate::os;
use crate::VERSION;

const RETRY_AFTER_S: u32 = 5;
const MIN_INTERVAL_S: f64 = 0.25;
const MAX_INTERVAL_S: f64 = 60.0;
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30); // a kept-alive connection with nothing to ask
const GZIP_OVER: usize = 2048;
const MAX_HEADER_BYTES: usize = 16 * 1024;

const ENDPOINTS: &[(&str, &str)] = &[
    ("/", "this index"),
    ("/health", "liveness, version, NVML and stream state"),
    ("/v1/static[/<section>]", "what the machine is and has; ?refresh=1 re-collects"),
    ("/v1/live[/<section>]", "one live sample; ?sections=a,b"),
    ("/v1/processes", "the process table; ?top=25&sort=cpu|memory|gpu"),
    ("/v1/stream", "Server-Sent Events: hello, sample, static; ?interval=1&sections=a,b&static=1"),
    ("/metrics.json", "pulsar-gui's cluster snapshot (reference sparkview-exporter 0.6.0 shape)"),
];

fn endpoints() -> Value {
    Value::Object(ENDPOINTS.iter().map(|(k, v)| (k.to_string(), json!(v))).collect())
}

pub struct Server {
    pub hub: Arc<Hub>,
    pub legacy: Exporter,
    pub max_streams: usize,
    pub keepalive_s: f64,
    pub verbose: bool,
    streams: AtomicUsize,
}

struct Request {
    method: String,
    path: String,
    query: HashMap<String, Vec<String>>,
    headers: HashMap<String, String>,
    keep_alive: bool,
}

enum Outcome {
    KeepAlive,
    Close,
}

struct BadRequest(String);

type Query = HashMap<String, Vec<String>>;

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(&s[i + 1..i + 3], 16) {
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

/// urllib.parse.parse_qs(keep_blank_values=True).
fn parse_query(q: &str) -> Query {
    let mut out: Query = HashMap::new();
    for part in q.split(['&', ';']).filter(|p| !p.is_empty()) {
        let (k, v) = part.split_once('=').unwrap_or((part, ""));
        out.entry(percent_decode(k)).or_default().push(percent_decode(v));
    }
    out
}

fn sections_param(q: &Query, allowed: &[&str], path_section: Option<&str>) -> Result<Option<Vec<String>>, BadRequest> {
    let mut names: Vec<String> = path_section.map(|s| vec![s.to_string()]).unwrap_or_default();
    for v in q.get("sections").into_iter().flatten() {
        names.extend(v.split(',').map(|n| n.trim().to_string()).filter(|n| !n.is_empty()));
    }
    if names.is_empty() {
        return Ok(None);
    }
    let unknown: Vec<&str> = names.iter().filter(|n| !allowed.contains(&n.as_str())).map(|s| s.as_str()).collect();
    if !unknown.is_empty() {
        return Err(BadRequest(format!("unknown section {}; sections are {}", unknown.join(", "), allowed.join(", "))));
    }
    let mut seen = std::collections::HashSet::new();
    Ok(Some(names.into_iter().filter(|n| seen.insert(n.clone())).collect()))
}

fn number(q: &Query, name: &str, default: f64, lo: f64, hi: f64) -> Result<f64, BadRequest> {
    let Some(v) = q.get(name).and_then(|v| v.last()) else { return Ok(default) };
    let v: f64 = v.trim().parse().map_err(|_| BadRequest(format!("{name} must be a number")))?;
    if v.is_nan() {
        return Err(BadRequest(format!("{name} must be a number")));
    }
    Ok(v.clamp(lo, hi))
}

fn flag(q: &Query, name: &str) -> bool {
    q.get(name).and_then(|v| v.last()).is_some_and(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | ""))
}

/// RFC 7231 date, as http.server sends it.
fn http_date() -> String {
    let t = os::now() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: tm is valid for writing.
    unsafe { libc::gmtime_r(&t, &mut tm) };
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!("{}, {:02} {} {} {:02}:{:02}:{:02} GMT", DAYS[tm.tm_wday as usize % 7], tm.tm_mday,
            MONTHS[tm.tm_mon as usize % 12], tm.tm_year + 1900, tm.tm_hour, tm.tm_min, tm.tm_sec)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK", 204 => "No Content", 400 => "Bad Request", 404 => "Not Found",
        500 => "Internal Server Error", 503 => "Service Unavailable", _ => "",
    }
}

fn read_request(reader: &mut BufReader<TcpStream>) -> io::Result<Option<Request>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let (method, target, version) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"), parts.next().unwrap_or("HTTP/1.0"));
    let mut headers = HashMap::new();
    let mut total = line.len();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 {
            break;
        }
        total += h.len();
        if total > MAX_HEADER_BYTES {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "headers too large"));
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    // A body on a GET is allowed and ignored; read it so the next request parses.
    if let Some(n) = headers.get("content-length").and_then(|n| n.parse::<u64>().ok()) {
        io::copy(&mut reader.by_ref().take(n.min(1 << 20)), &mut io::sink())?;
    }
    let connection = headers.get("connection").map(|c| c.to_ascii_lowercase()).unwrap_or_default();
    let keep_alive = if version.eq_ignore_ascii_case("HTTP/1.1") { connection != "close" } else { connection == "keep-alive" };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    Ok(Some(Request { method: method.to_string(), path: percent_decode(path), query: parse_query(query), headers, keep_alive }))
}

struct Conn<'a> {
    stream: &'a mut TcpStream,
    req: &'a Request,
}

impl Conn<'_> {
    fn head(&mut self, status: u16, extra: &[(&str, String)], close: bool) -> io::Result<()> {
        let mut h = format!("HTTP/1.1 {status} {}\r\nServer: vitals/{VERSION}\r\nDate: {}\r\n", reason(status), http_date());
        for (k, v) in extra {
            h.push_str(&format!("{k}: {v}\r\n"));
        }
        h.push_str("Access-Control-Allow-Origin: *\r\nCache-Control: no-store\r\n");
        if close {
            h.push_str("Connection: close\r\n");
        }
        h.push_str("\r\n");
        self.stream.write_all(h.as_bytes())
    }

    fn json_bytes(&mut self, status: u16, body: &[u8], pretty: bool, extra: Vec<(&str, String)>) -> io::Result<()> {
        let pretty_body;
        let mut body: &[u8] = body;
        if pretty {
            if let Ok(v) = serde_json::from_slice::<Value>(body) {
                pretty_body = serde_json::to_vec_pretty(&v).unwrap_or_default();
                body = &pretty_body;
            }
        }
        let gz = body.len() > GZIP_OVER && self.req.headers.get("accept-encoding").is_some_and(|e| e.contains("gzip"));
        let compressed;
        if gz {
            let mut enc = GzEncoder::new(Vec::new(), Compression::new(5));
            enc.write_all(body)?;
            compressed = enc.finish()?;
            body = &compressed;
        }
        let mut headers = vec![("Content-Type", "application/json".to_string()), ("Content-Length", body.len().to_string()),
                               ("Vary", "Accept-Encoding".to_string())];
        if gz {
            headers.push(("Content-Encoding", "gzip".to_string()));
        }
        headers.extend(extra);
        self.head(status, &headers, !self.req.keep_alive)?;
        if self.req.method != "HEAD" {
            self.stream.write_all(body)?;
        }
        self.stream.flush()
    }

    fn json(&mut self, status: u16, v: &Value, pretty: bool, extra: Vec<(&str, String)>) -> io::Result<()> {
        self.json_bytes(status, &serde_json::to_vec(v).unwrap_or_default(), pretty, extra)
    }
}

impl Server {
    pub fn new(hub: Arc<Hub>, max_streams: usize, keepalive_s: f64, verbose: bool) -> Arc<Self> {
        let legacy = Exporter::new(hub.clone());
        Arc::new(Server { hub, legacy, max_streams, keepalive_s, verbose, streams: AtomicUsize::new(0) })
    }

    pub fn serve(self: &Arc<Self>, listener: TcpListener) {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let server = self.clone();
            let spawned = std::thread::Builder::new().name("vitals-conn".into()).spawn(move || server.connection(stream));
            if let Err(e) = spawned {
                eprintln!("WARNING cannot start a connection thread: {e}");
            }
        }
    }

    fn connection(&self, mut stream: TcpStream) {
        let _ = stream.set_nodelay(true);
        let _ = stream.set_read_timeout(Some(IDLE_TIMEOUT));
        let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
        let Ok(read_half) = stream.try_clone() else { return };
        let mut reader = BufReader::new(read_half);
        let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
        loop {
            let req = match read_request(&mut reader) {
                Ok(Some(r)) => r,
                _ => return,
            };
            if self.verbose {
                eprintln!("DEBUG {peer} {} {}", req.method, req.path);
            }
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.handle(&mut stream, &req)));
            match outcome {
                Ok(Ok(Outcome::KeepAlive)) if req.keep_alive => continue,
                Ok(Ok(_)) | Ok(Err(_)) => return,
                Err(_) => {
                    // one bad request must not kill the server
                    let mut c = Conn { stream: &mut stream, req: &req };
                    let _ = c.json(500, &json!({"error": "internal error"}), false, vec![]);
                    return;
                }
            }
        }
    }

    fn handle(&self, stream: &mut TcpStream, req: &Request) -> io::Result<Outcome> {
        let mut c = Conn { stream, req };
        if req.method == "OPTIONS" {
            c.head(204, &[("Access-Control-Allow-Methods", "GET, HEAD, OPTIONS".into()),
                          ("Access-Control-Allow-Headers", "*".into()), ("Content-Length", "0".into())], !req.keep_alive)?;
            return Ok(Outcome::KeepAlive);
        }
        let path = req.path.trim_end_matches('/');
        let path = if path.is_empty() { "/" } else { path };
        let q = &req.query;
        let pretty = flag(q, "pretty");
        let result = match path {
            "/" => self.index(&mut c, pretty),
            "/health" | "/healthz" => self.health(&mut c, pretty),
            "/metrics.json" => c.json_bytes(200, &self.legacy.body(), pretty, vec![]).map(|_| Ok(())),
            p if p == "/v1/static" || p.starts_with("/v1/static/") => self.static_(&mut c, p, q, pretty),
            p if p == "/v1/live" || p.starts_with("/v1/live/") => self.live(&mut c, p, q, pretty),
            "/v1/processes" => self.processes(&mut c, q, pretty),
            "/v1/stream" => return self.stream(&mut c, q),
            p => c.json(404, &json!({"error": format!("no such endpoint {p}"), "endpoints": endpoints()}), pretty, vec![]).map(|_| Ok(())),
        };
        match result? {
            Ok(()) => Ok(Outcome::KeepAlive),
            Err(BadRequest(msg)) => {
                c.json(400, &json!({"error": msg}), false, vec![])?;
                Ok(Outcome::KeepAlive)
            }
        }
    }

    fn index(&self, c: &mut Conn, pretty: bool) -> io::Result<Result<(), BadRequest>> {
        c.json(200, &json!({
            "service": "vitals", "version": VERSION, "hostname": os::hostname(),
            "endpoints": endpoints(), "live_sections": LIVE_SECTIONS, "static_sections": STATIC_SECTIONS,
        }), pretty, vec![])?;
        Ok(Ok(()))
    }

    fn health(&self, c: &mut Conn, pretty: bool) -> io::Result<Result<(), BadRequest>> {
        let hub = &self.hub;
        let nvml = hub.nv.ready();
        c.json(200, &json!({
            "status": "ok",
            "service": "vitals",
            "version": VERSION,
            "implementation": "rust",
            "hostname": os::hostname(),
            "uptime_seconds": crate::sysfs::round(os::now() - hub.started_at, 1),
            "nvml": nvml,
            "nvml_error": hub.nv.error(),
            "streams": self.streams.load(Ordering::Acquire),
            "max_streams": self.max_streams,
            "sampling": hub.sampling(),
            "seq": hub.seq(),
            "static_generation": hub.static_generation(),
        }), pretty, vec![])?;
        Ok(Ok(()))
    }

    fn static_(&self, c: &mut Conn, path: &str, q: &Query, pretty: bool) -> io::Result<Result<(), BadRequest>> {
        let sub = path.strip_prefix("/v1/static/");
        let sections = match sections_param(q, STATIC_SECTIONS, sub) {
            Ok(s) => s,
            Err(e) => return Ok(Err(e)),
        };
        let doc = self.hub.static_doc(flag(q, "refresh"));
        let etag = format!("\"{}\"", doc["generation"].as_str().unwrap_or(""));
        c.json(200, &pick(&doc, sections.as_deref(), STATIC_ENVELOPE), pretty, vec![("ETag", etag)])?;
        Ok(Ok(()))
    }

    fn live(&self, c: &mut Conn, path: &str, q: &Query, pretty: bool) -> io::Result<Result<(), BadRequest>> {
        let sub = path.strip_prefix("/v1/live/");
        let sections = match sections_param(q, LIVE_SECTIONS, sub) {
            Ok(s) => s,
            Err(e) => return Ok(Err(e)),
        };
        let doc = self.hub.live();
        let body = self.hub.encoded(&doc, sections.as_deref());
        c.json_bytes(200, &body, pretty, vec![])?;
        Ok(Ok(()))
    }

    fn processes(&self, c: &mut Conn, q: &Query, pretty: bool) -> io::Result<Result<(), BadRequest>> {
        let top = match number(q, "top", 25.0, 1.0, 10000.0) {
            Ok(t) => t as usize,
            Err(e) => return Ok(Err(e)),
        };
        let sort = q.get("sort").and_then(|v| v.last()).map(|s| s.as_str()).unwrap_or("cpu");
        if !SORTS.contains(&sort) {
            return Ok(Err(BadRequest(format!("sort is one of {}", SORTS.join(", ")))));
        }
        c.json(200, &self.hub.process_table(top, sort), pretty, vec![])?;
        Ok(Ok(()))
    }

    // -- the stream ---------------------------------------------------------

    fn event(stream: &mut TcpStream, name: Option<&str>, data: &[u8], id: Option<u64>) -> io::Result<()> {
        let mut frame = Vec::with_capacity(data.len() + 64);
        match name {
            None => frame.extend_from_slice(b": keepalive\n\n"),
            Some(name) => {
                if let Some(id) = id {
                    frame.extend_from_slice(format!("id: {id}\n").as_bytes());
                }
                frame.extend_from_slice(format!("event: {name}\ndata: ").as_bytes());
                frame.extend_from_slice(data);
                frame.extend_from_slice(b"\n\n");
            }
        }
        stream.write_all(&frame)?;
        stream.flush()
    }

    fn stream(&self, c: &mut Conn, q: &Query) -> io::Result<Outcome> {
        let sections = match sections_param(q, LIVE_SECTIONS, None) {
            Ok(s) => s,
            Err(BadRequest(msg)) => {
                c.json(400, &json!({"error": msg}), false, vec![])?;
                return Ok(Outcome::KeepAlive);
            }
        };
        let interval = match number(q, "interval", 1.0, MIN_INTERVAL_S, MAX_INTERVAL_S) {
            Ok(i) => i,
            Err(BadRequest(msg)) => {
                c.json(400, &json!({"error": msg}), false, vec![])?;
                return Ok(Outcome::KeepAlive);
            }
        };
        let taken = self.streams.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < self.max_streams).then_some(n + 1));
        if taken.is_err() {
            // Refused rather than queued: a client that cannot stream should
            // poll /v1/live, which costs the same sample.
            c.json(503, &json!({"error": "too many streams"}), false, vec![("Retry-After", RETRY_AFTER_S.to_string())])?;
            return Ok(Outcome::KeepAlive);
        }
        let mut sid = None;
        let result = (|| -> io::Result<()> {
            let static_doc = self.hub.static_doc(false);
            c.head(200, &[("Content-Type", "text/event-stream".into()), ("X-Accel-Buffering", "no".into())], true)?;
            c.stream.write_all(b"retry: 3000\n\n")?;
            let generation = static_doc["generation"].as_str().unwrap_or("").to_string();
            let mut hello = json!({
                "service": "vitals", "version": VERSION, "hostname": static_doc["hostname"],
                "interval_s": interval, "keepalive_s": self.keepalive_s,
                "sections": sections.clone().unwrap_or_else(|| LIVE_SECTIONS.iter().map(|s| s.to_string()).collect()),
                "static_generation": generation,
            });
            if flag(q, "static") {
                hello["static"] = (*static_doc).clone();
            }
            Self::event(c.stream, Some("hello"), &serde_json::to_vec(&hello).unwrap_or_default(), None)?;
            sid = Some(self.hub.subscribe(interval));
            self.pump(c.stream, interval, sections.as_deref(), generation)
        })();
        if let Some(id) = sid {
            self.hub.unsubscribe(id);
        }
        self.streams.fetch_sub(1, Ordering::AcqRel);
        let _ = result; // the subscriber went away, or the server is stopping
        Ok(Outcome::Close)
    }

    fn pump(&self, stream: &mut TcpStream, interval: f64, sections: Option<&[String]>, mut generation: String) -> io::Result<()> {
        let hub = &self.hub;
        let (mut last_seq, mut next_due, mut last_write) = (0u64, 0.0f64, os::mono());
        loop {
            let deadline = last_write + self.keepalive_s;
            let view = hub.wait(deadline, |v| {
                let fresh = v.seq != last_seq && os::mono() >= next_due;
                if fresh || v.static_generation.as_deref().is_some_and(|g| g != generation) {
                    None
                } else if v.seq != last_seq {
                    Some(next_due)
                } else {
                    Some(f64::INFINITY)
                }
            });
            if view.stopping {
                return Ok(());
            }
            let now = os::mono();
            let mut sent = false;
            if let Some(g) = view.static_generation.filter(|g| *g != generation) {
                Self::event(stream, Some("static"), &serde_json::to_vec(&json!({"generation": g})).unwrap_or_default(), None)?;
                generation = g;
                sent = true;
            }
            if let Some(doc) = view.latest.as_ref().filter(|_| view.seq != last_seq && now >= next_due) {
                Self::event(stream, Some("sample"), &hub.encoded(doc, sections), Some(view.seq))?;
                last_seq = view.seq;
                // Due again one interval on, with slack for the sampler's own
                // jitter, so a 5 s subscriber on a 1 s sampler gets every 5th.
                next_due = now + interval * 0.9;
                sent = true;
            }
            if !sent && now - last_write >= self.keepalive_s {
                Self::event(stream, None, b"", None)?;
                sent = true;
            }
            if sent {
                last_write = now;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_parsing() {
        let q = parse_query("sections=cpu%2Cmemory&pretty&x=a+b");
        assert_eq!(q["sections"], vec!["cpu,memory"]);
        assert_eq!(q["pretty"], vec![""]);
        assert_eq!(q["x"], vec!["a b"]);
        assert!(flag(&q, "pretty"));
        assert!(sections_param(&q, LIVE_SECTIONS, None).ok().flatten().unwrap() == vec!["cpu", "memory"]);
        assert!(sections_param(&parse_query("sections=nope"), LIVE_SECTIONS, None).is_err());
        assert_eq!(number(&parse_query("interval=0.01"), "interval", 1.0, 0.25, 60.0).ok(), Some(0.25));
    }
}
