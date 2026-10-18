//! `READ`, `WRITE v` and `CAS expected new`, one request per line and one
//! thread per connection. With CASREG_BUG=1 (the default) a CAS reads under
//! the lock, does its bookkeeping, then takes the lock again to write.
use seedbox_eval_casreg::PORT;
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

struct Register {
    value: Mutex<Option<i64>>,
    buggy: bool,
    /// Reproducer: pause between comparing and setting, so the race is
    /// near certain when buggy and harmless when fixed (CASREG_YIELD=1).
    yield_inside: bool,
    /// Audit rounds per update; more widens the race window (CASREG_WORK).
    work: u64,
}

/// Per-update bookkeeping (an audit checksum): `work` rounds of hashing.
fn audit(expected: i64, new: i64, work: u64) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64 ^ (expected as u64) ^ ((new as u64) << 32);
    for i in 0..work {
        hash = (hash ^ i).wrapping_mul(0x0100_0000_01b3);
    }
    std::hint::black_box(hash)
}

fn cas(register: &Register, expected: i64, new: i64) -> bool {
    if register.buggy {
        let current = *register.value.lock().unwrap();
        if current != Some(expected) {
            return false;
        }
        audit(expected, new, register.work);
        if register.yield_inside {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        *register.value.lock().unwrap() = Some(new);
        true
    } else {
        let mut value = register.value.lock().unwrap();
        if *value != Some(expected) {
            return false;
        }
        audit(expected, new, register.work);
        if register.yield_inside {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        *value = Some(new);
        true
    }
}

fn serve(register: Arc<Register>, stream: TcpStream) -> std::io::Result<()> {
    let mut writer = stream.try_clone()?;
    for line in BufReader::new(stream).lines() {
        let line = line?;
        let words: Vec<&str> = line.split_whitespace().collect();
        let number = |word: &str| word.parse::<i64>().ok();
        let reply = match words.as_slice() {
            ["READ"] => match *register.value.lock().unwrap() {
                Some(value) => value.to_string(),
                None => "NONE".into(),
            },
            ["WRITE", value] => match number(value) {
                Some(value) => {
                    *register.value.lock().unwrap() = Some(value);
                    "OK".into()
                }
                None => "ERR".into(),
            },
            ["CAS", expected, new] => match (number(expected), number(new)) {
                (Some(expected), Some(new)) => if cas(&register, expected, new) {
                    "OK"
                } else {
                    "FAIL"
                }
                .into(),
                _ => "ERR".into(),
            },
            _ => "ERR".into(),
        };
        writer.write_all(format!("{reply}\n").as_bytes())?;
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    let buggy = std::env::var("CASREG_BUG").map_or(true, |value| value != "0");
    let register = Arc::new(Register {
        value: Mutex::new(None),
        buggy,
        yield_inside: std::env::var("CASREG_YIELD").is_ok_and(|value| value == "1"),
        work: std::env::var("CASREG_WORK")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(5000),
    });
    let listener = TcpListener::bind(("0.0.0.0", PORT))?;
    seedbox_sdk::setup_complete(json!({"buggy": buggy}));
    for stream in listener.incoming() {
        let register = register.clone();
        let stream = stream?;
        std::thread::spawn(move || {
            if let Err(error) = serve(register, stream) {
                eprintln!("casreg-server: connection: {error}");
            }
        });
    }
    Ok(())
}
