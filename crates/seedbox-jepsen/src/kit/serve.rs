//! A single-threaded TCP server an oracle serves whenever it waits. The
//! oracle runs on one thread: fault RPCs and checks run between waits, and
//! every request is handled in arrival order, so the service's log is one
//! total order of the clients' reports. A request that arrives during an RPC
//! is served late, never lost.
use super::{Error, Result};
use std::{
    collections::BTreeMap,
    io::{ErrorKind, Read, Write},
    net::{TcpListener, TcpStream},
    os::fd::AsRawFd,
    time::{Duration, Instant},
};

/// What a service does with a connection's buffered bytes.
pub enum Outcome {
    /// Keep the connection and wait for more bytes.
    Wait,
    /// Send this response, then close the connection.
    Reply(Vec<u8>),
}

/// A protocol served by [`Loop`]. `receive` may consume a prefix of `buffer`.
pub trait Service {
    fn receive(&mut self, buffer: &mut Vec<u8>) -> Outcome;
}

struct Connection {
    stream: TcpStream,
    buffer: Vec<u8>,
}

pub struct Loop {
    listener: TcpListener,
    connections: BTreeMap<u64, Connection>,
    next: u64,
}

impl Loop {
    pub fn bind(port: u16) -> Result<Self> {
        let listener = TcpListener::bind(("0.0.0.0", port))
            .map_err(|e| Error(format!("bind port {port}: {e}")))?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            connections: BTreeMap::new(),
            next: 0,
        })
    }

    /// Serve until `until` returns a value or `timeout` seconds pass.
    pub fn wait_for<S: Service, T>(
        &mut self,
        service: &mut S,
        timeout: f64,
        mut until: impl FnMut(&S) -> Option<T>,
    ) -> Option<T> {
        let deadline = Instant::now() + Duration::from_secs_f64(timeout.max(0.0));
        loop {
            if let Some(value) = until(service) {
                return Some(value);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            self.poll(service, (deadline - now).min(Duration::from_millis(500)));
        }
    }

    /// Serve for `timeout` seconds.
    pub fn serve<S: Service>(&mut self, service: &mut S, timeout: f64) {
        self.wait_for(service, timeout, |_| None::<()>);
    }

    /// Wait until `until` holds or `timeout` passes; true if it held.
    pub fn wait_until<S: Service>(
        &mut self,
        service: &mut S,
        timeout: f64,
        mut until: impl FnMut(&S) -> bool,
    ) -> bool {
        self.wait_for(service, timeout, |s| until(s).then_some(()))
            .is_some()
    }

    /// Send a line to every open connection (blocking, best effort).
    pub fn broadcast(&mut self, line: &[u8]) {
        for connection in self.connections.values_mut() {
            let _ = connection.stream.set_nonblocking(false);
            let _ = connection.stream.write_all(line);
            let _ = connection.stream.set_nonblocking(true);
        }
    }

    fn poll<S: Service>(&mut self, service: &mut S, timeout: Duration) {
        let ids: Vec<u64> = self.connections.keys().copied().collect();
        let mut fds = vec![libc::pollfd {
            fd: self.listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        for id in &ids {
            fds.push(libc::pollfd {
                fd: self.connections[id].stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        let millis = timeout.as_millis().clamp(1, i32::MAX as u128) as i32;
        // SAFETY: `fds` is a valid array of `fds.len()` pollfd structures.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, millis) };
        if ready <= 0 {
            return;
        }
        if fds[0].revents != 0 {
            self.accept();
        }
        for (index, id) in ids.iter().enumerate() {
            if fds[index + 1].revents != 0 {
                self.receive(*id, service);
            }
        }
    }

    fn accept(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nonblocking(true);
                    let _ = stream.set_nodelay(true);
                    self.connections.insert(
                        self.next,
                        Connection {
                            stream,
                            buffer: Vec::new(),
                        },
                    );
                    self.next += 1;
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(_) => return,
            }
        }
    }

    fn receive<S: Service>(&mut self, id: u64, service: &mut S) {
        let Some(connection) = self.connections.get_mut(&id) else {
            return;
        };
        let mut chunk = vec![0u8; 1 << 20];
        let closed = match connection.stream.read(&mut chunk) {
            Ok(0) => true,
            Ok(n) => {
                connection.buffer.extend_from_slice(&chunk[..n]);
                false
            }
            Err(error)
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) =>
            {
                return
            }
            Err(_) => true,
        };
        // Bytes already buffered are still served when the peer closes.
        if !connection.buffer.is_empty() {
            if let Outcome::Reply(response) = service.receive(&mut connection.buffer) {
                let _ = connection.stream.set_nonblocking(false);
                let _ = connection
                    .stream
                    .set_write_timeout(Some(Duration::from_secs(2)));
                let _ = connection.stream.write_all(&response);
                self.connections.remove(&id);
                return;
            }
        }
        if closed {
            self.connections.remove(&id);
        }
    }
}

/// An HTTP/1.0 request/response service: a complete request (by
/// `Content-Length`) is handed to `handle`, which returns a status and JSON.
pub fn http_request(buffer: &[u8]) -> Option<(String, String, Vec<u8>)> {
    let end = buffer.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&buffer[..end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let body = buffer.get(end + 4..end + 4 + length)?;
    Some((method, path, body.to_vec()))
}

/// An HTTP/1.0 response with a JSON body.
pub fn http_response(status: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.0 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

/// Split complete newline-terminated lines off the front of `buffer`.
pub fn take_lines(buffer: &mut Vec<u8>) -> Vec<Vec<u8>> {
    let mut lines = Vec::new();
    while let Some(end) = buffer.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = buffer.drain(..=end).collect();
        lines.push(line[..line.len() - 1].to_vec());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo(Vec<String>);
    impl Service for Echo {
        fn receive(&mut self, buffer: &mut Vec<u8>) -> Outcome {
            match http_request(buffer) {
                Some((_, path, body)) => {
                    self.0
                        .push(format!("{path} {}", String::from_utf8_lossy(&body)));
                    Outcome::Reply(http_response("200 OK", b"{}"))
                }
                None => Outcome::Wait,
            }
        }
    }

    #[test]
    fn serves_requests_in_arrival_order_while_waiting() {
        let mut server = Loop::bind(0).unwrap();
        let port = server.listener.local_addr().unwrap().port();
        let client = std::thread::spawn(move || {
            for n in 0..3 {
                let body = format!("{{\"n\":{n}}}");
                let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
                let (status, reply) = super::super::http::exchange(
                    stream,
                    "POST",
                    "oracle",
                    "/event",
                    Some(("application/json", body.as_bytes())),
                )
                .unwrap();
                assert_eq!((status, reply.as_slice()), (200, &b"{}"[..]));
            }
        });
        let mut echo = Echo(Vec::new());
        assert!(server.wait_until(&mut echo, 10.0, |e| e.0.len() == 3));
        client.join().unwrap();
        assert_eq!(
            echo.0,
            ["/event {\"n\":0}", "/event {\"n\":1}", "/event {\"n\":2}"]
        );
    }

    #[test]
    fn lines_split_at_newlines() {
        let mut buffer = b"a\nbc\npartial".to_vec();
        assert_eq!(take_lines(&mut buffer), [b"a".to_vec(), b"bc".to_vec()]);
        assert_eq!(buffer, b"partial");
    }
}
