//! A small blocking HTTP/1.1 client for the box's own services: Jellyfin, Plex and cloudflared on
//! loopback TCP, tailscaled's LocalAPI on its unix socket. Nothing else: a TCP target that is not
//! a loopback address is refused, so no caller can make the service talk to the network.
//! Requests that carry a secret go to [`Target::TcpOwnedBy`]: their ports are not privileged, so
//! while the real server is down any local process could listen there; the server's uid is
//! checked on the connection before a byte is sent.
//!
//! One request per connection (`Connection: close`), a deadline for the whole exchange, bodies
//! with `Content-Length`, chunked or up to EOF, at most [`MAX_RESPONSE`] bytes.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

/// Largest response read (headers and body).
pub const MAX_RESPONSE: usize = 4 << 20;

/// Where a request goes.
#[derive(Debug, Clone, Copy)]
pub enum Target<'a> {
    /// A loopback address (anything else is refused).
    Tcp(SocketAddr),
    /// A loopback address whose server must run as this uid (else nothing is sent).
    TcpOwnedBy(SocketAddr, u32),
    /// A unix socket, with the `Host` header to send.
    Unix(&'a Path, &'a str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// A request body and its content type.
pub struct Body<'a> {
    pub content_type: &'a str,
    pub bytes: &'a [u8],
}

trait Stream: Read + Write {
    fn set_timeouts(&self, t: Duration) -> io::Result<()>;
}

impl Stream for TcpStream {
    fn set_timeouts(&self, t: Duration) -> io::Result<()> {
        self.set_read_timeout(Some(t))?;
        self.set_write_timeout(Some(t))
    }
}

impl Stream for UnixStream {
    fn set_timeouts(&self, t: Duration) -> io::Result<()> {
        self.set_read_timeout(Some(t))?;
        self.set_write_timeout(Some(t))
    }
}

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "no answer in time")
}

/// Where the kernel lists TCP sockets (the service's own network namespace).
const PROC_NET_TCP: [&str; 2] = ["/proc/net/tcp", "/proc/net/tcp6"];

/// The uid that owns the server end of the loopback connection `s`: the accepted socket (or the
/// pending one, before `accept`) carries its listener's uid in `/proc/net/tcp`. Looked up a few
/// times: on loopback the server's side can show up just after `connect` returns.
fn server_uid(s: &TcpStream) -> io::Result<u32> {
    let (local, peer) = (s.local_addr()?, s.peer_addr()?);
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(10));
        }
        for file in PROC_NET_TCP {
            if let Ok(text) = std::fs::read_to_string(file)
                && let Some(uid) = uid_in(&text, peer, local)
            {
                return Ok(uid);
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("no server socket for {peer}"),
    ))
}

/// In the text of `/proc/net/tcp{,6}`: the uid of the socket at `local` connected to `remote`.
fn uid_in(text: &str, local: SocketAddr, remote: SocketAddr) -> Option<u32> {
    let (local, remote) = (unmapped(local), unmapped(remote));
    text.lines().skip(1).find_map(|line| {
        // sl local rem st tx:rx tr:when retrnsmt uid ...
        let f: Vec<&str> = line.split_whitespace().collect();
        let (l, r, uid) = (f.get(1)?, f.get(2)?, f.get(7)?);
        (proc_addr(l)? == local && proc_addr(r)? == remote)
            .then(|| uid.parse().ok())
            .flatten()
    })
}

fn unmapped(a: SocketAddr) -> SocketAddr {
    match a.ip() {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(IpAddr::V4(v4), a.port()),
            None => a,
        },
        IpAddr::V4(_) => a,
    }
}

/// A `/proc/net/tcp{,6}` address, `IP:PORT` in hex: the IP as the kernel's 32-bit words, each
/// printed in host byte order. An IPv4-mapped IPv6 address comes back as IPv4.
pub fn proc_addr(s: &str) -> Option<SocketAddr> {
    let (ip, port) = s.split_once(':')?;
    if !ip.is_ascii() {
        return None;
    }
    let port = u16::from_str_radix(port, 16).ok()?;
    let word = |i: usize| -> Option<[u8; 4]> {
        Some(
            u32::from_str_radix(ip.get(i * 8..i * 8 + 8)?, 16)
                .ok()?
                .to_ne_bytes(),
        )
    };
    let ip = match ip.len() {
        8 => IpAddr::V4(Ipv4Addr::from(word(0)?)),
        32 => {
            let mut b = [0u8; 16];
            for i in 0..4 {
                b[i * 4..i * 4 + 4].copy_from_slice(&word(i)?);
            }
            IpAddr::V6(Ipv6Addr::from(b))
        }
        _ => return None,
    };
    Some(unmapped(SocketAddr::new(ip, port)))
}

/// Send one request and read the answer, all within `timeout`.
pub fn request(
    target: Target<'_>,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<Body<'_>>,
    timeout: Duration,
) -> io::Result<Response> {
    let deadline = Instant::now() + timeout;
    let (mut stream, host): (Box<dyn Stream>, String) = match target {
        Target::Tcp(addr) | Target::TcpOwnedBy(addr, _) => {
            if !addr.ip().is_loopback() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{addr} is not a loopback address"),
                ));
            }
            let stream = TcpStream::connect_timeout(&addr, timeout)?;
            if let Target::TcpOwnedBy(_, owner) = target {
                let uid = server_uid(&stream)?;
                if uid != owner {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!("{addr} is served by uid {uid}, not {owner}"),
                    ));
                }
            }
            (Box::new(stream), addr.to_string())
        }
        Target::Unix(path, host) => (Box::new(UnixStream::connect(path)?), host.to_owned()),
    };
    let remaining = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(timed_out)
    };
    stream.set_timeouts(remaining()?)?;
    for (k, v) in headers {
        if [k, v]
            .iter()
            .any(|s| s.bytes().any(|b| b == b'\r' || b == b'\n'))
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad header"));
        }
    }
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    match &body {
        Some(b) => head.push_str(&format!(
            "Content-Type: {}\r\nContent-Length: {}\r\n",
            b.content_type,
            b.bytes.len()
        )),
        None if matches!(method, "POST" | "PUT") => head.push_str("Content-Length: 0\r\n"),
        None => {}
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    if let Some(b) = &body {
        stream.write_all(b.bytes)?;
    }
    stream.flush()?;

    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        if let Some(done) = parse(&buf, false) {
            return done;
        }
        stream.set_timeouts(remaining()?)?;
        let n = match stream.read(&mut chunk) {
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Err(timed_out());
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if n == 0 {
            return parse(&buf, true)
                .unwrap_or_else(|| Err(io::Error::new(io::ErrorKind::UnexpectedEof, "cut short")));
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_RESPONSE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "answer too large",
            ));
        }
    }
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_owned())
}

/// The response in `buf`, once it is complete (`eof`: the peer closed the connection).
/// `None`: more bytes are needed.
pub fn parse(buf: &[u8], eof: bool) -> Option<io::Result<Response>> {
    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return eof.then(|| Err(bad("no complete header")));
    };
    let head = match std::str::from_utf8(&buf[..end]) {
        Ok(h) => h,
        Err(_) => return Some(Err(bad("header is not UTF-8"))),
    };
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.splitn(3, ' ');
    let (Some(proto), Some(code)) = (parts.next(), parts.next()) else {
        return Some(Err(bad("bad status line")));
    };
    let Some(status) = proto
        .starts_with("HTTP/1.")
        .then(|| code.parse::<u16>().ok())
        .flatten()
        .filter(|c| (100..600).contains(c))
    else {
        return Some(Err(bad("bad status line")));
    };
    let mut length: Option<usize> = None;
    let mut chunked = false;
    for line in lines {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        if k == "content-length" {
            match v.parse() {
                Ok(n) => length = Some(n),
                Err(_) => return Some(Err(bad("bad Content-Length"))),
            }
        } else if k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
            chunked = true;
        }
    }
    let rest = &buf[end + 4..];
    // No body: 1xx (not expected), 204, 304.
    if status == 204 || status == 304 {
        return Some(Ok(Response {
            status,
            body: Vec::new(),
        }));
    }
    if chunked {
        return match dechunk(rest) {
            Ok(Some(body)) => Some(Ok(Response { status, body })),
            Ok(None) if eof => Some(Err(bad("chunked body cut short"))),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        };
    }
    match length {
        Some(n) if rest.len() >= n => Some(Ok(Response {
            status,
            body: rest[..n].to_vec(),
        })),
        Some(_) if eof => Some(Err(bad("body cut short"))),
        Some(_) => None,
        None if eof => Some(Ok(Response {
            status,
            body: rest.to_vec(),
        })),
        None => None,
    }
}

/// A chunked body: `Ok(None)` until the last chunk is in. The data is copied only then: called
/// after every read, a pass costs one step per chunk, not per byte.
fn dechunk(buf: &[u8]) -> io::Result<Option<Vec<u8>>> {
    let mut chunks = Vec::new();
    let mut at = 0;
    loop {
        let rest = &buf[at..];
        let Some(eol) = rest.windows(2).position(|w| w == b"\r\n") else {
            return Ok(None);
        };
        let size_text = std::str::from_utf8(&rest[..eol]).map_err(|_| bad("bad chunk size"))?;
        let size_text = size_text.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_text, 16)
            .ok()
            .filter(|s| *s <= MAX_RESPONSE)
            .ok_or_else(|| bad("bad chunk size"))?;
        at += eol + 2;
        if size == 0 {
            let mut body = Vec::with_capacity(chunks.iter().map(|(_, n)| n).sum());
            for (start, n) in chunks {
                body.extend_from_slice(&buf[start..start + n]);
            }
            return Ok(Some(body));
        }
        if buf.len() - at < size + 2 {
            return Ok(None);
        }
        if &buf[at + size..at + size + 2] != b"\r\n" {
            return Err(bad("bad chunk end"));
        }
        chunks.push((at, size));
        at += size + 2;
    }
}

/// Percent-encode a query value (everything but RFC 3986's unreserved characters).
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lengths_chunks_and_eof() {
        let r = parse(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi", false)
            .unwrap()
            .unwrap();
        assert_eq!((r.status, r.body.as_slice()), (200, &b"hi"[..]));
        assert!(parse(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nhi", false).is_none());
        assert!(
            parse(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nhi", true)
                .unwrap()
                .is_err()
        );
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWiki\r\n5;x=y\r\npedia\r\n0\r\n\r\n";
        let r = parse(chunked, false).unwrap().unwrap();
        assert_eq!(r.body, b"Wikipedia");
        assert!(parse(&chunked[..chunked.len() - 5], false).is_none());
        // A chunk size near usize::MAX is refused, not wrapped.
        for size in ["ffffffffffffffff", "fffffffffffffffe", "400001"] {
            let huge = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{size}\r\nx");
            assert!(parse(huge.as_bytes(), false).unwrap().is_err(), "{size}");
        }
        let r = parse(b"HTTP/1.0 503 No\r\n\r\nbody", true)
            .unwrap()
            .unwrap();
        assert_eq!((r.status, r.body.as_slice()), (503, &b"body"[..]));
        assert!(
            parse(b"HTTP/1.1 204 No Content\r\n\r\n", false)
                .unwrap()
                .unwrap()
                .ok()
        );
        for bad in [
            &b"SSH-2.0\r\n\r\n"[..],
            b"HTTP/1.1 abc\r\n\r\n",
            b"HTTP/1.1 999 x\r\n\r\n",
        ] {
            assert!(parse(bad, true).unwrap().is_err());
        }
    }

    #[test]
    fn refuses_other_hosts_and_header_injection() {
        let e = request(
            Target::Tcp("192.0.2.1:80".parse().unwrap()),
            "GET",
            "/",
            &[],
            None,
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let e = request(
            Target::Tcp(l.local_addr().unwrap()),
            "GET",
            "/",
            &[("X", "a\r\nInjected: 1")],
            None,
            Duration::from_millis(500),
        )
        .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn round_trip_and_timeout() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let t = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut req = Vec::new();
            let mut buf = [0u8; 1024];
            while !req.ends_with(b"{}") {
                let n = s.read(&mut buf).unwrap();
                assert!(n > 0);
                req.extend_from_slice(&buf[..n]);
            }
            let req = String::from_utf8_lossy(&req).into_owned();
            s.write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n",
            )
            .unwrap();
            // A silent second connection: the client must give up in time.
            let (_s2, _) = l.accept().unwrap();
            std::thread::sleep(Duration::from_millis(600));
            req
        });
        let r = request(
            Target::Tcp(addr),
            "POST",
            "/x?a=1",
            &[("Authorization", "MediaBrowser Token=\"k\"")],
            Some(Body {
                content_type: "application/json",
                bytes: b"{}",
            }),
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(r.body, b"{}");
        let start = Instant::now();
        let e = request(
            Target::Tcp(addr),
            "GET",
            "/",
            &[],
            None,
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_millis(550));
        let req = t.join().unwrap();
        assert!(req.starts_with("POST /x?a=1 HTTP/1.1\r\n"), "{req}");
        assert!(req.contains("Content-Length: 2\r\n") && req.ends_with("\r\n\r\n{}"));
        assert!(req.contains("Connection: close\r\n"));
    }

    #[test]
    fn proc_net_tcp_addresses_and_owners() {
        let v4 = |s: &str| -> SocketAddr { s.parse().unwrap() };
        // As little-endian x86_64 prints them.
        assert_eq!(proc_addr("0100007F:1FA0"), Some(v4("127.0.0.1:8096")));
        assert_eq!(
            proc_addr("0000000000000000FFFF00000100007F:1FA0"),
            Some(v4("127.0.0.1:8096"))
        );
        assert_eq!(
            proc_addr("00000000000000000000000001000000:0016"),
            Some(v4("[::1]:22"))
        );
        for bad in [
            "",
            "0100007F",
            "0100007:1FA0",
            "0100007F:x",
            "é100007F:1FA0",
        ] {
            assert_eq!(proc_addr(bad), None, "{bad}");
        }
        let text = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
            0: 0100007F:1FA0 00000000:0000 0A 00000000:00000000 00:00000000 00000000   990        0 1 1 0\n\
            1: 0100007F:1FA0 0100007F:A000 01 00000000:00000000 00:00000000 00000000   990        0 2 1 0\n\
            2: 0100007F:A000 0100007F:1FA0 01 00000000:00000000 00:00000000 00000000     0        0 3 1 0\n";
        let (server, client) = (v4("127.0.0.1:8096"), v4("127.0.0.1:40960"));
        assert_eq!(uid_in(text, server, client), Some(990));
        assert_eq!(uid_in(text, client, server), Some(0));
        assert_eq!(uid_in(text, server, v4("127.0.0.1:1")), None);
    }

    #[test]
    fn secrets_go_only_to_the_owner() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let me = rustix::process::getuid().as_raw();
        let t = std::thread::spawn(move || {
            let mut got = Vec::new();
            for _ in 0..2 {
                let (mut s, _) = l.accept().unwrap();
                let mut req = Vec::new();
                s.read_to_end(&mut req).unwrap();
                got.push(req);
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            }
            got
        });
        // Another owner: nothing is sent, the connection just closes.
        let e = request(
            Target::TcpOwnedBy(addr, me.wrapping_add(1)),
            "GET",
            "/",
            &[("Authorization", "secret")],
            None,
            Duration::from_secs(2),
        )
        .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{e}");
        // Ours: sent (the reader waits for EOF, so only the write half is checked here).
        let mut s = TcpStream::connect(addr).unwrap();
        assert_eq!(server_uid(&s).unwrap(), me);
        s.write_all(b"x").unwrap();
        s.shutdown(std::net::Shutdown::Write).unwrap();
        let got = t.join().unwrap();
        assert_eq!(got, [b"".to_vec(), b"x".to_vec()]);
    }

    #[test]
    fn query_encoding() {
        assert_eq!(encode("Kids & Family/2"), "Kids%20%26%20Family%2F2");
        assert_eq!(encode("é"), "%C3%A9");
        assert_eq!(encode("a-b_c.d~"), "a-b_c.d~");
    }
}
