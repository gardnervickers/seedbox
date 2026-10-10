//! Text results on stdout: one `WORD: detail` status line, then lowercase
//! `key: value` fields. Diagnostics go to stderr elsewhere.
use seedbox::Result;
use std::fmt::{self, Display};

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

/// The serialized (SCREAMING_SNAKE_CASE) name of a status enum, as in `--json`.
pub fn word(value: &impl serde::Serialize) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(word)) => word,
        _ => "UNKNOWN".into(),
    }
}

/// Seconds with millisecond precision.
pub struct Seconds(pub f64);
impl Display for Seconds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.3}", self.0)
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
        assert_eq!(Seconds(1.0).to_string(), "1.000");
        assert_eq!(Seconds(0.0004).to_string(), "0.000");
        assert_eq!(Csv::<u32>(&[]).to_string(), "");
        assert_eq!(Csv(&[3, 4, 10]).to_string(), "3,4,10");
    }
}
