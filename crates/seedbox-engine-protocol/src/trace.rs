//! Per-machine exit trace lines (`NAME.trace`), written by the engine when
//! tracing is on and compared by `seedbox diff`. Observation only: the exit
//! witness hashes the same event separately, not these bytes.

use std::{fmt, str::FromStr};

/// One traced exit: `kind time_ns extra(hex) rip(hex)`. Traces written before
/// PV-only execution carry a decimal branch count after `kind`; it is read
/// and dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraceRecord {
    /// Single ASCII event code.
    pub kind: u8,
    pub time_ns: u64,
    pub extra: u64,
    pub rip: u64,
}

impl fmt::Display for TraceRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} {:x} {:x}",
            self.kind as char, self.time_ns, self.extra, self.rip
        )
    }
}

/// A line that is not four (or, from older engines, five) whitespace-separated
/// fields of the trace format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidTraceRecord;

impl fmt::Display for InvalidTraceRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid trace record: expected kind time_ns extra(hex) rip(hex)")
    }
}

impl FromStr for TraceRecord {
    type Err = InvalidTraceRecord;

    /// Parse one line, ignoring surrounding whitespace and its newline.
    fn from_str(line: &str) -> Result<Self, Self::Err> {
        let fields: Vec<_> = line.split_whitespace().collect();
        let decimal = |field: &str| field.parse::<u64>().map_err(|_| InvalidTraceRecord);
        let hex = |field: &str| u64::from_str_radix(field, 16).map_err(|_| InvalidTraceRecord);
        let (kind, rest) = match fields.as_slice() {
            [kind, rest @ ..] if rest.len() == 3 => (kind, rest),
            [kind, branches, rest @ ..] if rest.len() == 3 => {
                decimal(branches)?;
                (kind, rest)
            }
            _ => return Err(InvalidTraceRecord),
        };
        let kind = match kind.as_bytes() {
            [kind] => *kind,
            _ => return Err(InvalidTraceRecord),
        };
        Ok(Self {
            kind,
            time_ns: decimal(rest[0])?,
            extra: hex(rest[1])?,
            rip: hex(rest[2])?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_keep_the_engine_text_format_and_round_trip() {
        let record = TraceRecord {
            kind: b'P',
            time_ns: 3_000,
            extra: 0xfe,
            rip: 0xffff_ffff_8100_0000,
        };
        assert_eq!(record.to_string(), "P 3000 fe ffffffff81000000");
        assert_eq!("P 3000 fe ffffffff81000000\n".parse(), Ok(record));
        // An older engine's branch count is read and dropped.
        assert_eq!("P 12 3000 fe ffffffff81000000".parse(), Ok(record));
        for bad in [
            "",
            "P 1 2",
            "P 1 2 3 4 5",
            "PP 1 2 3",
            "P x 2 3 4",
            "P 1 g 4",
        ] {
            assert_eq!(bad.parse::<TraceRecord>(), Err(InvalidTraceRecord), "{bad}");
        }
    }
}
