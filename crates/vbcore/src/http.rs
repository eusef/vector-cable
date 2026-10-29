//! A minimal HTTP/1.0 GET and POST client over `std::net`, enough for the
//! bridge's plain-http JSON endpoints and home page. No TLS, no dependencies, no
//! redirects. The parser is separate from the socket code so it can be tested.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpOutcome {
    Response {
        status: u16,
        body: String,
    },
    /// The server closed the connection before sending a status line.
    ConnectionDropped,
    /// Could not connect, or the socket failed.
    Unreachable(String),
}

/// Parses raw HTTP/1.x response bytes. `None` means no status line.
pub fn parse_response(raw: &[u8]) -> Option<(u16, String)> {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = match text.split_once("\r\n\r\n") {
        Some((h, b)) => (h, b.to_string()),
        None => match text.split_once("\n\n") {
            Some((h, b)) => (h, b.to_string()),
            None => (text.as_ref(), String::new()),
        },
    };
    let status_line = head.lines().next()?;
    let mut parts = status_line.split_whitespace();
    let proto = parts.next()?;
    if !proto.starts_with("HTTP/") {
        return None;
    }
    let status = parts.next().and_then(|s| s.parse::<u16>().ok())?;
    Some((status, body))
}

/// `GET path` from `host` (a name or address) on `port`.
pub fn get(host: &str, port: u16, path: &str, timeout: Duration) -> HttpOutcome {
    request(host, port, path, None, None, timeout)
}

/// `POST path` with a JSON body. One request per connection: the bridge
/// closes it after a 404 or a `wifi_open` refusal.
pub fn post_json(host: &str, port: u16, path: &str, body: &str, timeout: Duration) -> HttpOutcome {
    request(host, port, path, Some(body), None, timeout)
}

/// `post_json` with `Authorization: Bearer <token>` when a token is given (the
/// bridge's admin PIN session, DOCS/web-api.md).
pub fn post_json_auth(
    host: &str,
    port: u16,
    path: &str,
    body: &str,
    token: Option<&str>,
    timeout: Duration,
) -> HttpOutcome {
    request(host, port, path, Some(body), token, timeout)
}

fn request(
    host: &str,
    port: u16,
    path: &str,
    body: Option<&str>,
    token: Option<&str>,
    timeout: Duration,
) -> HttpOutcome {
    let addrs: Vec<SocketAddr> = match (host, port).to_socket_addrs() {
        Ok(a) => a.collect(),
        Err(e) => return HttpOutcome::Unreachable(format!("resolve {host}: {e}")),
    };
    let Some(addr) = addrs.first() else {
        return HttpOutcome::Unreachable(format!("resolve {host}: no address"));
    };
    let mut stream = match TcpStream::connect_timeout(addr, timeout) {
        Ok(s) => s,
        Err(e) => return HttpOutcome::Unreachable(format!("connect {addr}: {e}")),
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let request = match body {
        None => format!(
            "GET {path} HTTP/1.0\r\nHost: {host}\r\nUser-Agent: vectorcable\r\nConnection: close\r\n\r\n"
        ),
        Some(body) => format!(
            "POST {path} HTTP/1.0\r\nHost: {host}\r\nUser-Agent: vectorcable\r\nContent-Type: application/json\r\n{auth}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    };
    if let Err(e) = stream.write_all(request.as_bytes()) {
        return HttpOutcome::Unreachable(format!("send: {e}"));
    }
    let mut raw = Vec::new();
    if let Err(e) = stream.read_to_end(&mut raw) {
        if raw.is_empty() {
            return HttpOutcome::ConnectionDropped;
        }
        if e.kind() != std::io::ErrorKind::UnexpectedEof {
            return HttpOutcome::Unreachable(format!("read: {e}"));
        }
    }
    match parse_response(&raw) {
        Some((status, body)) => HttpOutcome::Response { status, body },
        None => HttpOutcome::ConnectionDropped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_response_reads_status_and_body() {
        let raw = b"HTTP/1.0 503 Service Unavailable\r\nContent-Type: application/json\r\n\r\n{\"ok\":false}";
        assert_eq!(
            parse_response(raw),
            Some((503, "{\"ok\":false}".to_string()))
        );
        assert_eq!(
            parse_response(b"HTTP/1.1 200 OK\r\n\r\n"),
            Some((200, String::new()))
        );
        assert_eq!(parse_response(b""), None);
        assert_eq!(parse_response(b"garbage\r\n\r\n"), None);
    }

    #[test]
    fn dropped_connection_is_reported_as_dropped() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            drop(stream); // close with no reply, as issue #26 describes
        });
        let outcome = get("127.0.0.1", port, "/api/ladder", Duration::from_secs(2));
        server.join().unwrap();
        assert_eq!(outcome, HttpOutcome::ConnectionDropped);
    }

    #[test]
    fn get_reads_a_small_reply() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            stream
                .write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 11\r\n\r\n{\"ok\":true}")
                .unwrap();
        });
        let outcome = get("127.0.0.1", port, "/api/rigctl", Duration::from_secs(2));
        server.join().unwrap();
        assert_eq!(
            outcome,
            HttpOutcome::Response {
                status: 200,
                body: "{\"ok\":true}".into()
            }
        );
    }
}
