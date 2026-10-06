//! Small dependency-free Redis workload. Readiness retries, SET acknowledgements,
//! and exact GET values are checked; a planted bad expectation exercises FAIL.
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    time::{Duration, Instant},
};

#[derive(Debug, PartialEq, Eq)]
enum Reply {
    Simple(Vec<u8>),
    Bulk(Option<Vec<u8>>),
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn line(reader: &mut impl BufRead) -> io::Result<Vec<u8>> {
    let mut data = Vec::new();
    reader.take(8193).read_until(b'\n', &mut data)?;
    if data.len() > 8192 || !data.ends_with(b"\r\n") {
        return Err(invalid("invalid Redis response line"));
    }
    data.truncate(data.len() - 2);
    Ok(data)
}

fn reply(reader: &mut impl BufRead) -> io::Result<Reply> {
    let header = line(reader)?;
    match header.first() {
        Some(b'+') => Ok(Reply::Simple(header[1..].to_vec())),
        Some(b'$') => {
            if &header[1..] == b"-1" {
                return Ok(Reply::Bulk(None));
            }
            let size = std::str::from_utf8(&header[1..])
                .ok()
                .and_then(|text| text.parse::<usize>().ok())
                .filter(|size| *size <= 1024 * 1024)
                .ok_or_else(|| invalid("invalid Redis bulk length"))?;
            let mut data = vec![0; size];
            reader.read_exact(&mut data)?;
            let mut end = [0; 2];
            reader.read_exact(&mut end)?;
            if end != *b"\r\n" {
                return Err(invalid("invalid Redis bulk terminator"));
            }
            Ok(Reply::Bulk(Some(data)))
        }
        Some(b'-') => Err(invalid(&format!(
            "Redis error: {}",
            String::from_utf8_lossy(&header[1..])
        ))),
        _ => Err(invalid("unexpected Redis response kind")),
    }
}

fn encode(parts: &[&[u8]]) -> Vec<u8> {
    let mut bytes = format!("*{}\r\n", parts.len()).into_bytes();
    for part in parts {
        bytes.extend_from_slice(format!("${}\r\n", part.len()).as_bytes());
        bytes.extend_from_slice(part);
        bytes.extend_from_slice(b"\r\n");
    }
    bytes
}

fn command(connection: &mut BufReader<TcpStream>, parts: &[&[u8]]) -> io::Result<Reply> {
    connection.get_mut().write_all(&encode(parts))?;
    reply(connection)
}

fn connect(address: &str) -> io::Result<BufReader<TcpStream>> {
    let address = address
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| invalid("Redis address did not resolve"))?;
    let stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut stream = BufReader::new(stream);
    if command(&mut stream, &[b"PING"])? != Reply::Simple(b"PONG".to_vec()) {
        return Err(invalid("Redis PING did not return PONG"));
    }
    Ok(stream)
}

fn run() -> io::Result<()> {
    let address = std::env::var("REDIS_ADDRESS").unwrap_or_else(|_| "redis:6379".into());
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut connection = loop {
        match connect(&address) {
            Ok(connection) => break connection,
            Err(error) if Instant::now() >= deadline => return Err(error),
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    for i in 0..32 {
        let key = format!("seedbox/key/{i}");
        let value = format!("value-{i}");
        if command(&mut connection, &[b"SET", key.as_bytes(), value.as_bytes()])?
            != Reply::Simple(b"OK".to_vec())
        {
            return Err(invalid("Redis SET not acknowledged"));
        }
        if command(&mut connection, &[b"GET", key.as_bytes()])?
            != Reply::Bulk(Some(value.into_bytes()))
        {
            return Err(invalid("acknowledged Redis value did not round-trip"));
        }
    }
    if std::env::var("SEEDBOX_PLANT_FAILURE").as_deref() == Ok("1")
        && command(&mut connection, &[b"GET", b"seedbox/never-written"])?
            != Reply::Bulk(Some(b"planted-value".to_vec()))
    {
        return Err(invalid("planted expectation failed as intended"));
    }
    println!("RESULT pass acknowledged_and_verified=32");
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("RESULT fail {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bulk_values_keep_binary_contents_and_nil_is_distinct() {
        assert_eq!(
            reply(&mut &b"$5\r\na\0b\r\n\r\n"[..]).unwrap(),
            Reply::Bulk(Some(b"a\0b\r\n".to_vec()))
        );
        assert_eq!(reply(&mut &b"$-1\r\n"[..]).unwrap(), Reply::Bulk(None));
        assert_eq!(
            reply(&mut &b"$0\r\n\r\n"[..]).unwrap(),
            Reply::Bulk(Some(vec![]))
        );
    }
    #[test]
    fn invalid_and_error_responses_fail() {
        for mut bytes in [
            b"-ERR unavailable\r\n".as_slice(),
            b"$999999999\r\n",
            b"$2\r\na\r\n",
            b"+OK\n",
            b"?OK\r\n",
        ] {
            assert!(reply(&mut bytes).is_err());
        }
    }
    #[test]
    fn request_encoding_preserves_argument_boundaries() {
        assert_eq!(
            encode(&[b"SET", b"key a", b"a\0b"]),
            b"*3\r\n$3\r\nSET\r\n$5\r\nkey a\r\n$3\r\na\0b\r\n"
        );
    }
}
