//! Dependency-free TCP fault/recovery fixture. The client records only actual
//! PUT acknowledgments and later checks every value through independent GETs.
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
fn line(reader: &mut impl BufRead) -> io::Result<String> {
    let mut bytes = Vec::new();
    reader.take(8193).read_until(b'\n', &mut bytes)?;
    if bytes.len() > 8192 || bytes.last() != Some(&b'\n') {
        return Err(invalid("missing or oversized protocol line"));
    }
    bytes.pop();
    String::from_utf8(bytes).map_err(|_| invalid("non-UTF8 protocol line"))
}
fn argument(name: &str, default: &str) -> String {
    let args: Vec<_> = std::env::args().collect();
    args.iter().position(|v| v == name).and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| default.into())
}
fn number(name: &str, default: u64) -> io::Result<u64> {
    argument(name, &default.to_string()).parse().map_err(|_| invalid(format!("invalid {name}")))
}
fn token(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_alphanumeric() || b"/-_".contains(&b))
}

struct Store {
    values: BTreeMap<String, String>,
    log: Option<File>,
    generation: u64,
}
impl Store {
    fn open(path: &Path, broken: bool) -> io::Result<Self> {
        fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
        // Diagnostic epoch metadata persists in both versions. Only the healthy
        // version saves the user values being checked by the independent client.
        let epoch_path = path.with_extension("epoch");
        let previous = match fs::read_to_string(&epoch_path) {
            Ok(value) => value.trim().parse::<u64>().map_err(|_| invalid("invalid startup epoch"))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e),
        };
        let generation = previous.checked_add(1).ok_or_else(|| invalid("epoch overflow"))?;
        let temporary = epoch_path.with_extension("epoch-next");
        let mut epoch = File::create(&temporary)?;
        writeln!(epoch, "{generation}")?;
        epoch.sync_all()?;
        fs::rename(&temporary, &epoch_path)?;
        let mut values = BTreeMap::new();
        let log = if broken {
            None
        } else {
            let bytes = match fs::read(path) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
                Err(e) => return Err(e),
            };
            // A killed, unacknowledged append may leave an incomplete final
            // record. Retain every complete record and truncate only that tail.
            let complete = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
            for record in bytes[..complete].split(|b| *b == b'\n').filter(|r| !r.is_empty()) {
                let text = std::str::from_utf8(record).map_err(|_| invalid("invalid saved record"))?;
                let mut parts = text.split(' ');
                let key = parts.next().ok_or_else(|| invalid("missing saved key"))?;
                let value = parts.next().ok_or_else(|| invalid("missing saved value"))?;
                if parts.next().is_some() || !token(key) || !token(value) { return Err(invalid("invalid saved key/value")); }
                values.insert(key.into(), value.into());
            }
            let log = OpenOptions::new().create(true).append(true).open(path)?;
            log.set_len(complete as u64)?;
            Some(log)
        };
        Ok(Self { values, log, generation })
    }
    fn put(&mut self, key: &str, value: &str) -> io::Result<()> {
        if !token(key) || !token(value) { return Err(invalid("invalid PUT token")); }
        if let Some(log) = &mut self.log {
            writeln!(log, "{key} {value}")?;
            log.sync_all()?; // No ACK is sent before the value is saved.
        }
        self.values.insert(key.into(), value.into());
        Ok(())
    }
    fn reply(&mut self, request: &str) -> io::Result<String> {
        let parts: Vec<_> = request.split(' ').collect();
        match parts.as_slice() {
            ["PING"] => Ok(format!("PONG {}", self.generation)),
            ["PUT", key, value] => { self.put(key, value)?; Ok(format!("OK {}", self.generation)) },
            ["GET", key] if token(key) => Ok(match self.values.get(*key) {
                Some(value) => format!("VALUE {} {value}", self.generation),
                None => format!("MISSING {}", self.generation),
            }),
            _ => Err(invalid("invalid request")),
        }
    }
}
fn server() -> io::Result<u8> {
    let address = argument("--listen", "0.0.0.0:7000");
    let path = PathBuf::from(argument("--state", "/data/kv.log"));
    let broken = std::env::args().any(|v| v == "--broken");
    let mut store = Store::open(&path, broken)?;
    let listener = TcpListener::bind(&address)?;
    println!("SERVER_READY generation={} recovered={} broken={broken}", store.generation, store.values.len());
    for stream in listener.incoming() {
        let stream = stream?;
        stream.set_read_timeout(Some(Duration::from_millis(250)))?;
        stream.set_write_timeout(Some(Duration::from_millis(250)))?;
        let mut reader = BufReader::new(stream);
        // A client may be killed or disconnected before completing a request.
        // Close that connection without changing any saved state.
        let Ok(request) = line(&mut reader) else { continue };
        let response = store.reply(&request)?;
        let _ = writeln!(reader.get_mut(), "{response}");
    }
    Ok(0)
}
fn request(address: &str, command: &str) -> io::Result<String> {
    let address = address.to_socket_addrs()?.next().ok_or_else(|| invalid("address did not resolve"))?;
    let stream = TcpStream::connect_timeout(&address, Duration::from_millis(200))?;
    stream.set_read_timeout(Some(Duration::from_millis(200)))?;
    stream.set_write_timeout(Some(Duration::from_millis(200)))?;
    stream.set_nodelay(true)?;
    let mut reader = BufReader::new(stream);
    writeln!(reader.get_mut(), "{command}")?;
    line(&mut reader)
}
fn generation(response: &str, prefix: &str) -> io::Result<u64> {
    response.strip_prefix(prefix).ok_or_else(|| invalid(format!("unexpected reply {response}")))?.parse().map_err(|_| invalid("invalid reply generation"))
}
#[derive(Debug)]
struct Ack { key: String, value: String, generation: u64 }
fn client() -> io::Result<u8> {
    let address = argument("--address", "server:7000");
    let duration = number("--duration-ms", 15_000)?;
    let required_generation = number("--required-generation", 3)?;
    let verify_timeout = number("--verify-timeout-ms", 10_000)?;
    if duration == 0 || required_generation == 0 || verify_timeout == 0 { return Err(invalid("workload limits must be positive")); }
    let start = Instant::now();
    let mut acks = Vec::new();
    let mut phases = BTreeMap::<u64, usize>::new();
    let mut failed = 0;
    let mut sequence = 0;
    while start.elapsed() < Duration::from_millis(duration) {
        let key = format!("k/{sequence:06}");
        let value = format!("v{sequence:06}");
        match request(&address, &format!("PUT {key} {value}")) {
            Ok(response) => {
                let epoch = generation(&response, "OK ")?;
                let phase = phases.entry(epoch).or_default();
                *phase += 1;
                if *phase == 1 { println!("ACK_PHASE generation={epoch} key={key}"); }
                acks.push(Ack {key,value,generation:epoch});
            },
            Err(_) => failed += 1,
        }
        sequence += 1;
        std::thread::sleep(Duration::from_millis(20));
    }
    println!("LOAD acked={} failed={failed} generations={phases:?}", acks.len());
    let deadline = Instant::now() + Duration::from_millis(verify_timeout);
    let current = loop {
        match request(&address, "PING") {
            Ok(response) => break generation(&response, "PONG ")?,
            Err(error) if Instant::now() >= deadline => return Err(error),
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let pre_restart = acks.iter().filter(|a| a.generation == 1).count();
    let post_restart = acks.iter().filter(|a| a.generation == current).count();
    if acks.is_empty() || pre_restart == 0 || post_restart == 0 || current < required_generation {
        println!("RESULT incomplete acked={} pre_restart={pre_restart} post_restart={post_restart} generation={current} required_generation={required_generation}",acks.len());
        return Ok(3);
    }
    let mut missing = 0;
    for ack in &acks {
        let response = loop {
            match request(&address, &format!("GET {}", ack.key)) {
                Ok(response) => break response,
                Err(error) if Instant::now() >= deadline => return Err(error),
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        };
        if response != format!("VALUE {current} {}", ack.value) {
            if response != format!("MISSING {current}") { return Err(invalid(format!("unexpected verification reply {response}"))); }
            missing += 1;
            println!("MISSING_ACK key={} expected={} acknowledged_generation={}",ack.key,ack.value,ack.generation);
        }
    }
    println!("RESULT {} acked={} pre_restart={pre_restart} post_restart={post_restart} generation={current} missing={missing}",if missing == 0 {"pass"} else {"fail"},acks.len());
    Ok(u8::from(missing > 0))
}
fn main() {
    let result = match std::env::args().nth(1).as_deref() {
        Some("server") => server(),
        Some("client") => client(),
        _ => Err(invalid("expected server or client mode")),
    };
    match result {
        Ok(code) => std::process::exit(code.into()),
        Err(error) => { eprintln!("RESULT error {error}"); std::process::exit(2); },
    }
}
