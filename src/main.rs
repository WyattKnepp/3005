#![forbid(unsafe_code)]
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Duration;

const PORT: u16 = 3005;
// Compile-time guard: the build fails if PORT is anything but 3005 (0x0BBD).
const _: () = assert!(PORT == 0x0BBD, "PORT must be 3005 (0x0BBD)");
// Single source of truth for the file: include_bytes! needs a literal, so a macro supplies it.
macro_rules! media_path {
    () => {
        "../media/3005.bbd"
    };
}
const PATH: &str = media_path!();
static MEDIA: &[u8] = include_bytes!(media_path!());

const MAX_CONNS: usize = 256; // hard cap on live threads
const MAX_REQ: usize = 16 * 1024; // request header cap; also the post-response drain cap
const TIMEOUT: Duration = Duration::from_secs(10); // per read/write call
static LIVE: AtomicUsize = AtomicUsize::new(0);

struct Resp {
    full: &'static [u8],    // headers + whole body, sent for 200
    body: &'static [u8],    // the body slice of `full`
    head_206: &'static str, // prefix shared by every 206 header
}

// Decrements the live-connection count when the thread ends, however it ends.
struct Slot;
impl Drop for Slot {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Relaxed);
    }
}

fn main() {
    if running_as_root() {
        eprintln!("refusing to run as root; use an unprivileged user");
        std::process::exit(1);
    }
    let mime = if PATH.ends_with(".mp4") { "video/mp4" } else { "audio/mpeg" };
    // nosniff stops browsers from treating the bytes as HTML/JS; sandbox CSP neuters anything that slips through.
    let common = format!(
        "Content-Type: {mime}\r\nAccept-Ranges: bytes\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: sandbox\r\nConnection: close\r\n"
    );
    let mut buf = format!("HTTP/1.1 200 OK\r\n{common}Content-Length: {}\r\n\r\n", MEDIA.len()).into_bytes();
    let head_len = buf.len();
    buf.extend_from_slice(MEDIA);
    let full: &'static [u8] = Box::leak(buf.into_boxed_slice());
    let head_206: &'static str = Box::leak(format!("HTTP/1.1 206 Partial Content\r\n{common}").into_boxed_str());
    let resp: &'static Resp = Box::leak(Box::new(Resp { full, body: &full[head_len..], head_206 }));

    let listener = TcpListener::bind(("0.0.0.0", PORT)).expect("bind");
    for stream in listener.incoming().flatten() {
        if LIVE.fetch_add(1, Relaxed) >= MAX_CONNS {
            LIVE.fetch_sub(1, Relaxed);
            continue; // over the cap: drop (close) immediately
        }
        // Builder::spawn returns Err on failure; thread::spawn would panic, and panic=abort kills the server.
        let spawned = std::thread::Builder::new().stack_size(64 * 1024).spawn(move || {
            let _slot = Slot;
            let _ = handle(stream, resp);
        });
        if spawned.is_err() {
            LIVE.fetch_sub(1, Relaxed);
        }
    }
}

/// Effective uid 0? Read from /proc so no libc/unsafe is needed. Unknown -> assume not root.
fn running_as_root() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("Uid:").map(|v| v.split_whitespace().nth(1) == Some("0"))))
        .unwrap_or(false)
}

fn handle(mut s: TcpStream, r: &Resp) -> std::io::Result<()> {
    s.set_read_timeout(Some(TIMEOUT))?;
    s.set_write_timeout(Some(TIMEOUT))?;
    // Read until the blank line that ends the request headers.
    let mut req = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = s.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        // Only rescan the new bytes plus 3 of overlap, so slow-drip input can't cost quadratic CPU.
        let from = req.len().saturating_sub(3);
        req.extend_from_slice(&tmp[..n]);
        if req[from..].windows(4).any(|w| w == b"\r\n\r\n") || req.len() >= MAX_REQ {
            break;
        }
    }
    match parse_range(&req, r.body.len()) {
        Some((a, b)) => {
            let hdr = format!(
                "{}Content-Range: bytes {a}-{b}/{}\r\nContent-Length: {}\r\n\r\n",
                r.head_206,
                r.body.len(),
                b - a + 1
            );
            // Two writes: the range header is per-request; the 200 path stays a single write.
            s.write_all(hdr.as_bytes())?;
            s.write_all(&r.body[a..=b])?;
        }
        None => s.write_all(r.full)?,
    }
    // Half-close, then drain (bounded): closing with unread input would make the kernel send RST and truncate the response.
    s.shutdown(Shutdown::Write)?;
    let mut drained = 0;
    while drained < MAX_REQ {
        match s.read(&mut tmp)? {
            0 => break,
            n => drained += n,
        }
    }
    Ok(())
}

/// Returns an inclusive (start, end) for a single, satisfiable `bytes=a-b` / `bytes=a-` range; anything else -> None (serve 200).
/// Works on bytes, not &str: slicing a str at a non-char boundary would panic (and panic=abort kills the process).
fn parse_range(req: &[u8], len: usize) -> Option<(usize, usize)> {
    let last = len.checked_sub(1)?;
    let line = req.split(|&c| c == b'\n').find(|l| l.len() > 6 && l[..6].eq_ignore_ascii_case(b"range:"))?;
    let spec = std::str::from_utf8(&line[6..]).ok()?.trim().strip_prefix("bytes=")?;
    let (a, b) = spec.split_once('-')?; // a "," in the spec makes the number parse fail -> None
    let a: usize = a.parse().ok()?;
    let b = if b.is_empty() { last } else { b.parse::<usize>().ok()?.min(last) };
    (a <= b).then_some((a, b))
}
