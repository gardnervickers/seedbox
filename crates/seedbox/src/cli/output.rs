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
