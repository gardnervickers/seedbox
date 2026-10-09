//! Byte bounds on retained metadata, with messages derived from the bound itself.
use std::{
    fmt::{self, Display},
    io::{self, Read},
};

/// A byte count in its largest exact binary unit, as in "8 MiB".
#[derive(Clone, Copy)]
pub(crate) struct Size(pub u64);
impl Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (shift, unit) in [(30, "GiB"), (20, "MiB"), (10, "KiB")] {
            if self.0 >= 1 << shift && self.0.is_multiple_of(1 << shift) {
                return write!(f, "{} {unit}", self.0 >> shift);
            }
        }
        write!(f, "{} bytes", self.0)
    }
}

/// `what exceeds <limit>`.
pub(crate) fn exceeds(what: impl Display, limit: u64) -> String {
    format!("{what} exceeds {}", Size(limit))
}

/// A bounded read failed: the reader failed, or held more than the bound.
#[derive(Debug)]
pub(crate) enum Bounded {
    Io(io::Error),
    /// The `exceeds` message.
    TooLarge(String),
}
impl Display for Bounded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(f),
            Self::TooLarge(message) => f.write_str(message),
        }
    }
}
impl Bounded {
    /// Keep I/O errors as `Error::Io` and categorize the size refusal.
    pub(crate) fn or_too_large(
        self,
        category: impl FnOnce(String) -> crate::Error,
    ) -> crate::Error {
        match self {
            Self::Io(error) => error.into(),
            Self::TooLarge(message) => category(message),
        }
    }
}

/// Read all of `reader`, refusing more than `limit` bytes without allocating them.
pub(crate) fn read_bounded(
    reader: impl Read,
    limit: u64,
    what: impl Display,
) -> Result<Vec<u8>, Bounded> {
    let mut bytes = Vec::new();
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(Bounded::Io)?;
    if bytes.len() as u64 > limit {
        return Err(Bounded::TooLarge(exceeds(what, limit)));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_use_the_largest_exact_unit() {
        assert_eq!(Size(8 * 1024 * 1024).to_string(), "8 MiB");
        assert_eq!(Size(64 * 1024).to_string(), "64 KiB");
        assert_eq!(Size(512 << 20).to_string(), "512 MiB");
        assert_eq!(Size(2 << 30).to_string(), "2 GiB");
        assert_eq!(Size(1536).to_string(), "1536 bytes");
        assert_eq!(Size(0).to_string(), "0 bytes");
    }

    #[test]
    fn reads_up_to_the_bound_and_names_what_exceeds_it() {
        assert_eq!(read_bounded(&[1u8; 4][..], 4, "x").unwrap(), vec![1; 4]);
        let error = read_bounded(&[1u8; 5][..], 4, "config a").unwrap_err();
        assert_eq!(error.to_string(), "config a exceeds 4 bytes");
    }
}
