use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

const PORT: u16 = 3005;
// Compile-time guard: the build fails if PORT is anything but 3005 (0x0BBD).
const _: () = assert!(PORT == 0x0BBD, "No");
// Single source of truth for the file: include_bytes! needs a literal, so a macro supplies it.
macro_rules! media_path {
    () => {
        "../media/sample.mp3"
    };
}
const PATH: &str = media_path!();
static MEDIA: &[u8] = include_bytes!(media_path!());

struct Resp {
    full: &'static [u8],      // headers + whole body, sent for 200
    body: &'static [u8],      // the body slice of `full`
    head_206: &'static str,   // prefix shared by every 206 header
}

fn main() {
    let mime = if PATH.ends_with(".mp4") { "video/mp4" } else { "audio/mpeg" };
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
        MEDIA.len()
    );
    let mut buf = head.into_bytes();
    let head_len = buf.len();
    buf.extend_from_slice(MEDIA);
    let full: &'static [u8] = Box::leak(buf.into_boxed_slice());
    let head_206: &'static str =
        Box::leak(format!("HTTP/1.1 206 Partial Content\r\nContent-Type: {mime}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n").into_boxed_str());
    let resp: &'static Resp = Box::leak(Box::new(Resp { full, body: &full[head_len..], head_206 }));

    let listener = TcpListener::bind(("0.0.0.0", PORT)).expect("bind");
    for stream in listener.incoming().flatten() {
        std::thread::spawn(move || {
            let _ = handle(stream, resp);
        });
    }
}

fn handle(mut s: TcpStream, r: &Resp) -> std::io::Result<()> {
    // Read until the blank line that ends the request headers.
    let mut req = Vec::new();
    let mut tmp = [0u8; 4096];
    while !req.windows(4).any(|w| w == b"\r\n\r\n") && req.len() < 64 * 1024 {
        let n = s.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        req.extend_from_slice(&tmp[..n]);
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
    // Half-close, then drain: closing with unread input would make the kernel send RST and truncate the response.
    s.shutdown(std::net::Shutdown::Write)?;
    while s.read(&mut tmp)? > 0 {}
    Ok(())
}

/// Returns an inclusive (start, end) for a single, satisfiable `bytes=a-b` / `bytes=a-` range; anything else -> None (serve 200).
fn parse_range(req: &[u8], len: usize) -> Option<(usize, usize)> {
    let text = std::str::from_utf8(req).ok()?;
    let line = text.lines().find(|l| l.len() > 6 && l[..6].eq_ignore_ascii_case("range:"))?;
    let spec = line[6..].trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let a: usize = a.parse().ok()?;
    let b = if b.is_empty() { len.checked_sub(1)? } else { b.parse::<usize>().ok()?.min(len.checked_sub(1)?) };
    (a <= b).then_some((a, b))
}
