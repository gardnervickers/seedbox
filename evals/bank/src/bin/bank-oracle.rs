//! Bank oracle: concurrent random transfers, then check that money was
//! neither created nor destroyed. Exits 0 unless the workload itself could
//! not run; correctness is reported through properties.
use seedbox_eval_bank::{ACCOUNTS, INITIAL_BALANCE, PORT};
use serde_json::json;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const CLIENTS: usize = 4;
const TRANSFERS: usize = 50;

struct Connection {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Connection {
    fn open(address: &str) -> std::io::Result<Self> {
        let stream = TcpStream::connect(address)?;
        Ok(Self {
            writer: stream.try_clone()?,
            reader: BufReader::new(stream),
        })
    }
    fn request(&mut self, line: &str) -> std::io::Result<String> {
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        let mut reply = String::new();
        self.reader.read_line(&mut reply)?;
        Ok(reply.trim().to_string())
    }
}

/// Guest entropy is seeded by Seedbox, so these choices replay exactly.
fn random_bytes(count: usize) -> Vec<u8> {
    let mut bytes = vec![0; count];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .expect("urandom");
    bytes
}

fn connect(address: &str) -> Connection {
    loop {
        match Connection::open(address) {
            Ok(connection) => return connection,
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn main() {
    let address = std::env::var("BANK_ADDRESS").unwrap_or_else(|_| format!("bank:{PORT}"));
    let expected = INITIAL_BALANCE * ACCOUNTS as i64;
    seedbox_sdk::declare("total balance is conserved", seedbox_sdk::Kind::Always);
    seedbox_sdk::declare("some transfer is refused", seedbox_sdk::Kind::Sometimes);
    drop(connect(&address));
    seedbox_sdk::setup_complete(serde_json::Value::Null);
    let clients: Vec<_> = (0..CLIENTS)
        .map(|client| {
            let address = address.clone();
            std::thread::spawn(move || {
                let mut connection = connect(&address);
                let mut refused = 0;
                for choice in random_bytes(TRANSFERS * 3).chunks(3) {
                    let from = choice[0] as usize % ACCOUNTS;
                    let to = (from + 1 + choice[1] as usize % (ACCOUNTS - 1)) % ACCOUNTS;
                    let amount = 1 + choice[2] as i64 % 60;
                    let reply = connection
                        .request(&format!("TRANSFER {from} {to} {amount}"))
                        .expect("transfer");
                    refused += (reply == "NO") as u32;
                }
                seedbox_sdk::sometimes(
                    "some transfer is refused",
                    refused > 0,
                    json!({"client": client}),
                );
            })
        })
        .collect();
    for client in clients {
        client.join().expect("client thread");
    }
    let total: i64 = connect(&address)
        .request("TOTAL")
        .expect("total")
        .parse()
        .expect("number");
    seedbox_sdk::always(
        "total balance is conserved",
        total == expected,
        json!({"total": total, "expected": expected}),
    );
    seedbox_sdk::reachable("all transfers completed", json!({"total": total}));
    seedbox_sdk::flush().expect("flush properties");
    println!("total {total}, expected {expected}");
}
