//! Test-only HTTP primitives shared by the mock backends.
//!
//! Every mock backend in the gate needs the same three things: read the
//! request line, choose the status reason, write a response. They live here so
//! the harnesses differ only in what they assert (modes, counters, port
//! lifecycle), not in how they speak HTTP. The read timeout bounds failure and
//! is never the success condition; a client that connects and sends nothing
//! cannot hold a mock open.
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// The request path from the request line (`GET /metrics HTTP/1.1`), read up to
/// the end of the headers. `None` when the peer sends nothing or closes early.
pub(crate) fn read_request_path(stream: &mut TcpStream) -> Option<String> {
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    text.lines()
        .next()?
        .split_whitespace()
        .nth(1)
        .map(str::to_string)
}

/// The reason text for a status line.
pub(crate) fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        404 => "Not Found",
        501 => "Not Implemented",
        _ => "Error",
    }
}

/// One complete HTTP response.
pub(crate) fn write_response(stream: &mut TcpStream, status: u16, content_type: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
        reason = status_reason(status)
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}
