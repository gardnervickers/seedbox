//! Bank server. `TRANSFER from to amount`, `TOTAL` and `BALANCE account`, one
//! request per line. With BANK_BUG=1 (the default) a transfer releases the
//! lock between reading and writing the source balance, so two concurrent
//! transfers from one account can both succeed against the same old balance.
use seedbox_eval_bank::{ACCOUNTS, INITIAL_BALANCE, PORT};
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

struct Bank {
    balances: Mutex<Vec<i64>>,
    buggy: bool,
}

impl Bank {
    fn transfer(&self, from: usize, to: usize, amount: i64) -> bool {
        if self.buggy {
            let read = self.balances.lock().unwrap()[from];
            if read < amount {
                return false;
            }
            // The window: another transfer can read the same balance here.
            std::thread::yield_now();
            let mut balances = self.balances.lock().unwrap();
            balances[from] = read - amount;
            balances[to] += amount;
            check(&balances);
            true
        } else {
            let mut balances = self.balances.lock().unwrap();
            if balances[from] < amount {
                return false;
            }
            balances[from] -= amount;
            balances[to] += amount;
            check(&balances);
            true
        }
    }
}

fn check(balances: &[i64]) {
    let lowest = *balances.iter().min().unwrap();
    seedbox_sdk::always(
        "balances are never negative",
        lowest >= 0,
        json!({"lowest": lowest}),
    );
}

fn serve(bank: Arc<Bank>, stream: TcpStream) -> std::io::Result<()> {
    let mut writer = stream.try_clone()?;
    for line in BufReader::new(stream).lines() {
        let line = line?;
        let words: Vec<&str> = line.split_whitespace().collect();
        let reply = match words.as_slice() {
            ["TRANSFER", from, to, amount] => {
                match (
                    from.parse::<usize>(),
                    to.parse::<usize>(),
                    amount.parse::<i64>(),
                ) {
                    (Ok(from), Ok(to), Ok(amount))
                        if from < ACCOUNTS && to < ACCOUNTS && from != to && amount > 0 =>
                    {
                        if bank.transfer(from, to, amount) {
                            "OK".to_string()
                        } else {
                            "NO".to_string()
                        }
                    }
                    _ => "ERR".to_string(),
                }
            }
            ["TOTAL"] => bank
                .balances
                .lock()
                .unwrap()
                .iter()
                .sum::<i64>()
                .to_string(),
            ["BALANCE", account] => match account.parse::<usize>() {
                Ok(account) if account < ACCOUNTS => {
                    bank.balances.lock().unwrap()[account].to_string()
                }
                _ => "ERR".to_string(),
            },
            _ => "ERR".to_string(),
        };
        writer.write_all(reply.as_bytes())?;
        writer.write_all(b"\n")?;
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    let buggy = std::env::var("BANK_BUG").map_or(true, |value| value != "0");
    let bank = Arc::new(Bank {
        balances: Mutex::new(vec![INITIAL_BALANCE; ACCOUNTS]),
        buggy,
    });
    let listener = TcpListener::bind(("0.0.0.0", PORT))?;
    seedbox_sdk::setup_complete(json!({"buggy": buggy}));
    for stream in listener.incoming() {
        let bank = bank.clone();
        let stream = stream?;
        std::thread::spawn(move || {
            if let Err(error) = serve(bank, stream) {
                eprintln!("bank-server: connection: {error}");
            }
        });
    }
    Ok(())
}
