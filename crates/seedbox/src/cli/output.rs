//! Text results on stdout: one `WORD: detail` status line, then lowercase
//! `key: value` fields. Diagnostics go to stderr elsewhere.
use seedbox_application::Result;
#[cfg(any(target_os = "linux", test))]
use std::fmt;
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

/// Separate and flush each resource in a live YAML document stream.
pub fn yaml_document(value: &impl serde::Serialize) -> Result<()> {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(b"---\n")?;
    stdout.write_all(serde_yaml_ng::to_string(value)?.as_bytes())?;
    stdout.flush()?;
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

/// Values separated by commas, without spaces.
#[cfg(any(target_os = "linux", test))]
pub struct Csv<'a, T>(pub &'a [T]);
#[cfg(any(target_os = "linux", test))]
impl<T: Display> Display for Csv<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, value) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str(",")?;
            }
            value.fmt(f)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrappers_match_the_documented_text() {
        assert_eq!(Csv::<u32>(&[]).to_string(), "");
        assert_eq!(Csv(&[3, 4, 10]).to_string(), "3,4,10");
    }
}
