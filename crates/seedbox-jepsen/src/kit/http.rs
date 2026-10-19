//! A minimal blocking HTTP/1.1 client: one request per connection, bodies by
//! `Content-Length`, chunked encoding or end of stream. Enough for etcd's JSON
//! gateway (including streamed watches), NATS monitoring and the Docker
//! engine API over a Unix socket. Timeouts apply to each socket operation, as
//! Python's `http.client` does.
use super::{Error, Result, Rng};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    sync::Mutex,
    time::{Duration, Instant},
};

/// How long clients avoid a host after it refused a connection, as real
/// clients back off: a stopped node refuses at once, so clients with no
/// think time would otherwise spin on it, filling the history with requests
/// never sent and taking the guest's CPU from the requests that are.
pub const CONNECT_BACKOFF: Duration = Duration::from_millis(100);

/// The hosts that recently refused a connection, so clients send to the
/// others instead of waiting.
#[derive(Default)]
pub struct Refusals(Mutex<BTreeMap<String, Instant>>);

impl Refusals {
    pub const fn new() -> Self {
        Self(Mutex::new(BTreeMap::new()))
    }

    /// Note that `host` refused a connection just now.
    pub fn refused(&self, host: &str) {
        self.0
            .lock()
            .unwrap()
            .insert(host.to_owned(), Instant::now());
    }

    /// A random host among those that have not refused a connection within
    /// [`CONNECT_BACKOFF`]; if every one has, wait that long and choose
    /// among them all.
    pub fn choose<'a>(&self, rng: &mut Rng, hosts: &'a [String]) -> &'a String {
        let up: Vec<&String> = {
            let refused = self.0.lock().unwrap();
            hosts
                .iter()
                .filter(|h| {
                    refused
                        .get(*h)
                        .is_none_or(|t| t.elapsed() >= CONNECT_BACKOFF)
                })
                .collect()
        };
        if up.is_empty() {
            std::thread::sleep(CONNECT_BACKOFF);
            return rng.choice(hosts);
        }
        let host: &&String = rng.choice(&up);
        host
    }
}

/// Connect to `host:port`, trying each resolved address within `timeout`.
pub fn connect(host: &str, port: u16, timeout: Duration) -> Result<TcpStream> {
    let addresses = (host, port)
        .to_socket_addrs()
        .map_err(|e| Error(format!("{host}: {e}")))?;
    let mut last = Error(format!("{host}: no address"));
    for address in addresses {
        match TcpStream::connect_timeout(&address, timeout) {
            Ok(stream) => {
                stream.set_read_timeout(Some(timeout))?;
                stream.set_write_timeout(Some(timeout))?;
                let _ = stream.set_nodelay(true);
                return Ok(stream);
            }
            Err(error) => last = Error(format!("{host}:{port}: {error}")),
        }
    }
    Err(last)
}

/// Write one request; `host` is the `Host` header value.
pub fn write_request(
    stream: &mut impl Write,
    method: &str,
    host: &str,
    path: &str,
    body: Option<(&str, &[u8])>,
) -> Result<()> {
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nAccept-Encoding: identity\r\nConnection: close\r\n");
    if let Some((content_type, data)) = body {
        head.push_str(&format!(
            "Content-Type: {content_type}\r\nContent-Length: {}\r\n",
            data.len()
        ));
    } else if method == "POST" {
        head.push_str("Content-Length: 0\r\n");
    }
    head.push_str("\r\n");
    let mut frame = head.into_bytes();
    if let Some((_, data)) = body {
        frame.extend_from_slice(data);
    }
    stream.write_all(&frame)?;
    stream.flush()?;
    Ok(())
}

/// A response's status line and headers (names lowercased).
#[derive(Debug, Clone)]
pub struct Head {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

impl Head {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

fn read_line(reader: &mut impl BufRead) -> Result<String> {
    let mut line = Vec::new();
    reader.by_ref().take(65_536).read_until(b'\n', &mut line)?;
    if line.is_empty() {
        return Err(Error("connection closed before the response".into()));
    }
    Ok(String::from_utf8_lossy(&line).trim_end().to_string())
}

pub fn read_head(reader: &mut impl BufRead) -> Result<Head> {
    loop {
        let status_line = read_line(reader)?;
        let status = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .ok_or_else(|| Error(format!("bad status line {status_line:?}")))?;
        let mut headers = Vec::new();
        loop {
            let line = read_line(reader)?;
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
            }
        }
        // Skip interim 1xx responses.
        if !(100..200).contains(&status) {
            return Ok(Head { status, headers });
        }
    }
}

enum Framing {
    Length(u64),
    Chunked { remaining: u64, done: bool },
    Eof,
}

/// A response body decoded from its framing.
pub struct Body<R> {
    inner: R,
    framing: Framing,
}

impl<R: BufRead> Body<R> {
    pub fn new(inner: R, head: &Head) -> Self {
        let framing = if head
            .header("transfer-encoding")
            .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
        {
            Framing::Chunked {
                remaining: 0,
                done: false,
            }
        } else if let Some(length) = head.header("content-length").and_then(|v| v.parse().ok()) {
            Framing::Length(length)
        } else {
            Framing::Eof
        };
        Self { inner, framing }
    }
}

impl<R: BufRead> Read for Body<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        match &mut self.framing {
            Framing::Length(0) => Ok(0),
            Framing::Length(left) => {
                let want = out.len().min(*left as usize);
                let n = self.inner.read(&mut out[..want])?;
                if n == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "body ended early",
                    ));
                }
                *left -= n as u64;
                Ok(n)
            }
            Framing::Eof => self.inner.read(out),
            Framing::Chunked { remaining, done } => {
                if *done {
                    return Ok(0);
                }
                if *remaining == 0 {
                    let line =
                        read_line(&mut self.inner).map_err(|e| std::io::Error::other(e.0))?;
                    let size = line.split(';').next().unwrap_or("").trim();
                    let size = u64::from_str_radix(size, 16).map_err(|_| {
                        std::io::Error::new(std::io::ErrorKind::InvalidData, "bad chunk size")
                    })?;
                    if size == 0 {
                        *done = true;
                        // Trailers end with an empty line; ignore them.
                        while let Ok(line) = read_line(&mut self.inner) {
                            if line.is_empty() {
                                break;
                            }
                        }
                        return Ok(0);
                    }
                    *remaining = size;
                }
                let want = out.len().min(*remaining as usize);
                let n = self.inner.read(&mut out[..want])?;
                if n == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "chunk ended early",
                    ));
                }
                *remaining -= n as u64;
                if *remaining == 0 {
                    let mut crlf = [0u8; 2];
                    self.inner.read_exact(&mut crlf)?;
                }
                Ok(n)
            }
        }
    }
}

/// One complete exchange on `stream`: status and whole body.
pub fn exchange<S: Read + Write>(
    mut stream: S,
    method: &str,
    host: &str,
    path: &str,
    body: Option<(&str, &[u8])>,
) -> Result<(u16, Vec<u8>)> {
    write_request(&mut stream, method, host, path, body)?;
    let mut reader = BufReader::new(stream);
    let head = read_head(&mut reader)?;
    let mut data = Vec::new();
    Body::new(&mut reader, &head).read_to_end(&mut data)?;
    Ok((head.status, data))
}

/// `GET http://host:port/path`.
pub fn get(host: &str, port: u16, path: &str, timeout: Duration) -> Result<(u16, Vec<u8>)> {
    let stream = connect(host, port, timeout)?;
    exchange(stream, "GET", &format!("{host}:{port}"), path, None)
}

/// POST a JSON document and parse the JSON reply; a status other than 200
/// is an error, as etcd's gateway reports failures.
pub fn post_json(
    host: &str,
    port: u16,
    path: &str,
    body: &Value,
    timeout: Duration,
) -> Result<Value> {
    let stream = connect(host, port, timeout)?;
    let data = body.to_string();
    let (status, reply) = exchange(
        stream,
        "POST",
        &format!("{host}:{port}"),
        path,
        Some(("application/json", data.as_bytes())),
    )?;
    if status != 200 {
        let text = String::from_utf8_lossy(&reply[..reply.len().min(200)]).to_string();
        return Err(Error(format!("{host}{path}: HTTP {status} {text:?}")));
    }
    Ok(serde_json::from_slice(&reply)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clients_avoid_a_host_that_refused_recently() {
        let hosts: Vec<String> = ["n1", "n2", "n3"].map(String::from).to_vec();
        let refusals = Refusals::new();
        let mut rng = Rng::new(1);
        refusals.refused("n2");
        for _ in 0..50 {
            assert_ne!(refusals.choose(&mut rng, &hosts), "n2");
        }
        refusals.refused("n1");
        refusals.refused("n3");
        // Every host refused: wait out the backoff, then choose among all.
        let start = Instant::now();
        refusals.choose(&mut rng, &hosts);
        assert!(start.elapsed() >= CONNECT_BACKOFF);
        let seen: std::collections::BTreeSet<_> =
            (0..50).map(|_| refusals.choose(&mut rng, &hosts)).collect();
        assert_eq!(seen.len(), 3);
    }

    #[test]
    fn chunked_and_length_bodies_decode() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n6\r\n{\"a\":1\r\n2\r\n}\n\r\n0\r\n\r\n";
        let mut reader = BufReader::new(&raw[..]);
        let head = read_head(&mut reader).unwrap();
        let mut body = BufReader::new(Body::new(&mut reader, &head));
        let mut line = String::new();
        body.read_line(&mut line).unwrap();
        assert_eq!(line, "{\"a\":1}\n");
        line.clear();
        assert_eq!(body.read_line(&mut line).unwrap(), 0);

        let raw = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.0 404 Not Found\r\ncontent-length: 3\r\n\r\nabcdef";
        let mut reader = BufReader::new(&raw[..]);
        let head = read_head(&mut reader).unwrap();
        assert_eq!(head.status, 404);
        let mut data = String::new();
        Body::new(&mut reader, &head)
            .read_to_string(&mut data)
            .unwrap();
        assert_eq!(data, "abc");
    }

    #[test]
    fn requests_carry_host_and_length() {
        let mut out = Vec::new();
        write_request(
            &mut out,
            "POST",
            "n1:2379",
            "/v3/kv/put",
            Some(("application/json", b"{}")),
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("POST /v3/kv/put HTTP/1.1\r\nHost: n1:2379\r\n"));
        assert!(text.ends_with("Content-Length: 2\r\n\r\n{}"));
    }
}
