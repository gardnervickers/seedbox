//! Text results on stdout: tables, documents, or status lines and result fields.
//! Diagnostics go to stderr elsewhere.
use seedbox_application::Result;
use std::fmt::Display;

/// Print the command's one `WORD: detail` status line.
pub fn status(word: &str, detail: impl Display) {
    println!("{word}: {detail}");
}

/// Print one lowercase `key: value` result field.
pub fn field(key: &str, value: impl Display) {
    println!("{key}: {value}");
}

/// Print exactly one pretty JSON document.
pub fn json(value: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

pub fn yaml(value: &impl serde::Serialize) -> Result<()> {
    use std::io::Write;
    std::io::stdout()
        .lock()
        .write_all(serde_yaml_ng::to_string(value)?.as_bytes())?;
    Ok(())
}

/// Flush each observation so downstream consumers can process live events.
pub fn json_line(value: &impl serde::Serialize) -> Result<()> {
    use std::io::Write;
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&line)?;
    stdout.flush()?;
    Ok(())
}

pub fn bytes(bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(bytes)?;
    stdout.flush()?;
    Ok(())
}

/// The serialized (SCREAMING_SNAKE_CASE) name of a status enum, as in `--json`.
pub fn word(value: &impl serde::Serialize) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(word)) => word,
        _ => "UNKNOWN".into(),
    }
}

/// An aligned text table. Cells stay on one line, including untrusted names.
pub struct Table<const N: usize> {
    widths: [usize; N],
}

impl<const N: usize> Table<N> {
    pub fn new(headers: [&str; N], minimum_widths: [usize; N]) -> Result<Self> {
        let table = Self {
            widths: std::array::from_fn(|i| minimum_widths[i].max(headers[i].len())),
        };
        table.row(headers.map(str::to_owned))?;
        Ok(table)
    }

    pub fn row(&self, cells: [String; N]) -> Result<()> {
        use std::io::Write;
        let mut stdout = std::io::stdout().lock();
        for (i, cell) in cells.iter().enumerate() {
            let cell: String = cell
                .chars()
                .map(|c| {
                    if c.is_control() || c.is_whitespace() {
                        ' '
                    } else {
                        c
                    }
                })
                .collect();
            if i + 1 == N {
                writeln!(stdout, "{cell}")?;
            } else {
                write!(stdout, "{cell:width$}  ", width = self.widths[i])?;
            }
        }
        stdout.flush()?;
        Ok(())
    }
}

/// Print a finite table, sizing columns from all of its rows.
pub fn table<const N: usize>(headers: [&str; N], rows: Vec<[String; N]>) -> Result<()> {
    let widths = std::array::from_fn(|i| {
        rows.iter()
            .map(|row| row[i].chars().count())
            .max()
            .unwrap_or(0)
    });
    let table = Table::new(headers, widths)?;
    for row in rows {
        table.row(row)?;
    }
    Ok(())
}

/// Encode the canonical API DiagnosticDocument without allocating its hex string.
pub fn diagnostic_document(
    file: &seedbox_api::DiagnosticFile,
    omissions: &std::collections::BTreeMap<String, String>,
    payload: &mut impl std::io::Read,
) -> Result<()> {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(b"{\n  \"file\": ")?;
    serde_json::to_writer_pretty(
        IndentedJson {
            output: &mut stdout,
            line_start: false,
        },
        file,
    )?;
    stdout.write_all(b",\n  \"payloadHex\": \"")?;
    let mut bytes = [0; 32 * 1024];
    let mut encoded = [0; 64 * 1024];
    const HEX: &[u8] = b"0123456789abcdef";
    loop {
        let count = payload.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        for (i, byte) in bytes[..count].iter().enumerate() {
            encoded[2 * i] = HEX[(byte >> 4) as usize];
            encoded[2 * i + 1] = HEX[(byte & 15) as usize];
        }
        stdout.write_all(&encoded[..count * 2])?;
    }
    stdout.write_all(b"\",\n  \"omissions\": ")?;
    serde_json::to_writer_pretty(
        IndentedJson {
            output: &mut stdout,
            line_start: false,
        },
        omissions,
    )?;
    stdout.write_all(b"\n}\n")?;
    stdout.flush()?;
    Ok(())
}

/// Add the enclosing document's indentation while serde streams nested metadata.
struct IndentedJson<W> {
    output: W,
    line_start: bool,
}
impl<W: std::io::Write> std::io::Write for IndentedJson<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        for line in bytes.split_inclusive(|b| *b == b'\n') {
            if self.line_start {
                self.output.write_all(b"  ")?;
            }
            self.output.write_all(line)?;
            self.line_start = line.last() == Some(&b'\n');
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }
}
