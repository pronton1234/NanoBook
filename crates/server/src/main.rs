//! A small HTTP server that runs the real pipeline and streams real numbers.
//!
//! Hand-rolled over `std::net::TcpListener` rather than built on a framework.
//! The API is four fixed endpoints with bounded parameters, which does not
//! justify pulling in an async runtime and its dependency tree — and for a
//! systems project, the server being ~250 lines of standard library is more to
//! the point than the server being one line of someone else's crate.
//!
//! ## Why it warms up before answering
//!
//! Free-tier hosts sleep, and a container that has just woken has cold caches,
//! unfaulted pages, and possibly throttled CPU. The first benchmark run on such
//! a host measures the host waking up. This project has already published one
//! impossible number from exactly that mistake — a negative stage cost, caused
//! by the first pass paying every page fault for ~6,000 symbol entries.
//!
//! So the server warms on startup in a background thread and reports its state
//! through `/health`. The page shows "waking", then "warming", then results.
//! The delay is not hidden, because the reason for it is the most interesting
//! thing on the page.
//!
//! ## Why only one benchmark runs at a time
//!
//! Two concurrent runs contend for CPU and cache, and the resulting latencies
//! are meaningless. A single slot means a second visitor waits or sees the last
//! result, which is the honest trade: a queue is better than a wrong number.

mod bench;
mod verify;

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use bench::{Container, Corpus, Report, Request};

/// Segments generated per run. Enough to be a real measurement, small enough
/// that a visitor is not left waiting on a shared vCPU.
const SEGMENTS: usize = 60_000;

static WARM: AtomicBool = AtomicBool::new(false);
static WARMUP_MS: AtomicU64 = AtomicU64::new(0);
static RUNS_SERVED: AtomicU64 = AtomicU64::new(0);

/// One benchmark at a time. See the module docs.
fn slot() -> &'static Mutex<()> {
    static SLOT: OnceLock<Mutex<()>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(()))
}

fn main() {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);

    // Warm in the background so the socket is accepting immediately and the
    // page can show progress instead of hanging on connect.
    std::thread::spawn(|| {
        let start = Instant::now();
        let req = Request {
            container: Container::Sorted,
            duplicate_rate: 0.0,
            gap_rate: 0.0,
            load: 0.5,
        };
        let corpus = bench::build_corpus(req, SEGMENTS);
        // Two full passes: the first faults pages in, the second settles caches.
        for _ in 0..2 {
            let _ = bench::run(req, &corpus);
        }
        WARMUP_MS.store(start.elapsed().as_millis() as u64, Ordering::Release);
        WARM.store(true, Ordering::Release);
        eprintln!("warm after {} ms", WARMUP_MS.load(Ordering::Acquire));
    });

    let listener = TcpListener::bind(("0.0.0.0", port)).expect("bind");
    eprintln!("listening on 0.0.0.0:{port}");
    for stream in listener.incoming().flatten() {
        std::thread::spawn(move || {
            let _ = handle(stream);
        });
    }
}

fn handle(mut stream: TcpStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }
    // Drain headers; nothing here needs them.
    let mut header = String::new();
    while reader.read_line(&mut header)? > 2 {
        header.clear();
    }

    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");
    if method == "OPTIONS" {
        return respond(&mut stream, 204, "text/plain", "");
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));

    match path {
        "/health" => {
            let warm = WARM.load(Ordering::Acquire);
            let body = format!(
                r#"{{"warm":{},"warmup_ms":{},"runs_served":{},"arch":"{}"}}"#,
                warm,
                WARMUP_MS.load(Ordering::Acquire),
                RUNS_SERVED.load(Ordering::Acquire),
                std::env::consts::ARCH
            );
            respond(&mut stream, 200, "application/json", &body)
        }
        "/run" => run_endpoint(&mut stream, query),
        "/budget" => budget_endpoint(&mut stream),
        "/days" => {
            let rows: Vec<String> = verify::SAMPLES
                .iter()
                .map(|s| format!(r#"{{"date":"{}","bytes":{}}}"#, s.date, s.bytes.len()))
                .collect();
            respond(
                &mut stream,
                200,
                "application/json",
                &format!("[{}]", rows.join(",")),
            )
        }
        "/verify" => verify_endpoint(&mut stream, query),
        "/" | "/index.html" => respond(
            &mut stream,
            200,
            "text/html; charset=utf-8",
            include_str!("../static/index.html"),
        ),
        _ => respond(&mut stream, 404, "text/plain", "not found"),
    }
}

fn run_endpoint(stream: &mut TcpStream, query: &str) -> std::io::Result<()> {
    if !WARM.load(Ordering::Acquire) {
        return respond(
            stream,
            503,
            "application/json",
            r#"{"error":"warming","detail":"the host is still faulting pages and settling caches; a measurement taken now would report the host waking up"}"#,
        );
    }

    let req = Request {
        container: Container::parse(param(query, "container").as_deref().unwrap_or("sorted")),
        duplicate_rate: param_f64(query, "dup", 0.0),
        gap_rate: param_f64(query, "gap", 0.0),
        load: param_f64(query, "load", 0.5),
    }
    .clamped();

    // Queue rather than contend. A concurrent run would make both wrong.
    let _guard = slot().lock().unwrap_or_else(|e| e.into_inner());
    let corpus: Corpus = bench::build_corpus(req, SEGMENTS);
    let report = bench::run(req, &corpus);
    RUNS_SERVED.fetch_add(1, Ordering::Relaxed);
    respond(stream, 200, "application/json", &to_json(&report))
}

/// The per-stage latency budget. Slower than `/run` because it makes nine
/// round-robin passes over six prefixes, so it is a separate endpoint rather
/// than part of every request.
fn budget_endpoint(stream: &mut TcpStream) -> std::io::Result<()> {
    if !WARM.load(Ordering::Acquire) {
        return respond(stream, 503, "application/json", r#"{"error":"warming"}"#);
    }
    let _guard = slot().lock().unwrap_or_else(|e| e.into_inner());
    let req = Request {
        container: Container::Sorted,
        duplicate_rate: 0.0,
        gap_rate: 0.0,
        load: 0.5,
    };
    let corpus = bench::build_corpus(req, SEGMENTS);
    let stages = bench::stage_budget(&corpus);
    let rows: Vec<String> = stages
        .iter()
        .map(|s| {
            format!(
                r#"{{"name":"{}","cumulative":{:.2},"this_stage":{:.2},"spread":{:.2},"unresolved":{}}}"#,
                s.name, s.cumulative, s.this_stage, s.spread, s.unresolved
            )
        })
        .collect();
    let unresolved = stages.iter().filter(|s| s.unresolved).count();
    let body = format!(
        r#"{{"arch":"{}","unresolved":{},"of":{},"stages":[{}]}}"#,
        std::env::consts::ARCH,
        unresolved,
        stages.len(),
        rows.join(",")
    );
    respond(stream, 200, "application/json", &body)
}

/// Decode one real exchange session and report whether it was understood.
///
/// Not gated on warm: this is a correctness check, not a timing measurement, so
/// a cold host gives the same answer. It is still serialised through the same
/// slot, because a decode running alongside a benchmark would perturb the
/// benchmark even though it would not perturb itself.
fn verify_endpoint(stream: &mut TcpStream, query: &str) -> std::io::Result<()> {
    let date = param(query, "day").unwrap_or_default();
    let Some(sample) = verify::find(&date) else {
        return respond(
            stream,
            404,
            "application/json",
            r#"{"error":"unknown day","detail":"see /days for the sessions compiled into this build"}"#,
        );
    };

    let _guard = slot().lock().unwrap_or_else(|e| e.into_inner());
    let v = verify::verify(sample);

    let types: Vec<String> = v
        .types
        .iter()
        .map(|(name, n)| format!(r#"{{"name":"{name}","count":{n}}}"#))
        .collect();
    let quotes: Vec<String> = v
        .quotes
        .iter()
        .map(|q| {
            format!(
                r#"{{"symbol":"{}","bid":"{}","bid_size":{},"ask":"{}","ask_size":{}}}"#,
                q.symbol, q.bid, q.bid_size, q.ask, q.ask_size
            )
        })
        .collect();

    let body = format!(
        r#"{{"date":"{}","format":"{}","bytes":{},"packets":{},"messages":{},"errors":{},"unknown":{},"symbols":{},"price_updates":{},"trades":{},"gaps":{},"duplicates":{},"crossed":{},"low":"{}","high":"{}","nanos":{},"types":[{}],"quotes":[{}]}}"#,
        v.date,
        v.format,
        v.bytes,
        v.packets,
        v.messages,
        v.errors,
        v.unknown,
        v.symbols,
        v.price_updates,
        v.trades,
        v.gaps,
        v.duplicates,
        v.crossed,
        v.low,
        v.high,
        v.nanos,
        types.join(","),
        quotes.join(",")
    );
    respond(stream, 200, "application/json", &body)
}

fn param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| v.to_string())
    })
}

fn param_f64(query: &str, key: &str, default: f64) -> f64 {
    param(query, key)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn to_json(r: &Report) -> String {
    let top: Vec<String> = r
        .top
        .iter()
        .map(|t| {
            let fmt = |o: &Option<(String, u32)>| match o {
                Some((p, s)) => format!(r#"{{"price":"{p}","size":{s}}}"#),
                None => "null".to_string(),
            };
            format!(
                r#"{{"symbol":"{}","bid":{},"ask":{},"bid_levels":{},"ask_levels":{}}}"#,
                t.symbol,
                fmt(&t.bid),
                fmt(&t.ask),
                t.bid_levels,
                t.ask_levels
            )
        })
        .collect();
    format!(
        r#"{{"container":"{}","packets":{},"messages":{},"book_updates":{},"orders":{},"ns_per_packet":{:.2},"spread":{:.2},"duplicates_suppressed":{},"gaps_detected":{},"gaps_injected":{},"crossed_when_stable":{},"service_p50":{},"service_p99":{},"corrected_p50":{},"corrected_p99":{},"behind_schedule":{},"arch":"{}","top":[{}]}}"#,
        r.container,
        r.packets,
        r.messages,
        r.book_updates,
        r.orders,
        r.ns_per_packet,
        r.spread,
        r.duplicates_suppressed,
        r.gaps_detected,
        r.gaps_injected,
        r.crossed_when_stable,
        r.service_p50,
        r.service_p99,
        r.corrected_p50,
        r.corrected_p99,
        r.behind_schedule,
        std::env::consts::ARCH,
        top.join(",")
    )
}

fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        503 => "Service Unavailable",
        _ => "Not Found",
    };
    // The page is served from GitHub Pages and this from elsewhere, so the
    // browser needs permission to make the call at all.
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Methods: GET, OPTIONS\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}
