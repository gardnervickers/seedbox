//! Per-machine exit trace lines (`NAME.trace`), written by the engine when
//! tracing is on and compared by `seedbox diff`. Observation only: the exit
//! witness hashes the same event separately, not these bytes.

use std::{fmt, str::FromStr};

/// One traced exit: `kind branches time_ns extra(hex) rip(hex)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraceRecord {
    /// Single ASCII event code.
    pub kind: u8,
    pub branches: u64,
    pub time_ns: u64,
    pub extra: u64,
    pub rip: u64,
}

impl fmt::Display for TraceRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} {} {:x} {:x}",
            self.kind as char, self.branches, self.time_ns, self.extra, self.rip
        )
    }
}

/// A line that is not five whitespace-separated fields of the trace format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidTraceRecord;

impl fmt::Display for InvalidTraceRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid trace record: expected kind branches time_ns extra(hex) rip(hex)")
    }
}

impl FromStr for TraceRecord {
    type Err = InvalidTraceRecord;

    /// Parse one line, ignoring surrounding whitespace and its newline.
    fn from_str(line: &str) -> Result<Self, Self::Err> {
        let mut fields = line.split_whitespace();
        let mut next = || fields.next().ok_or(InvalidTraceRecord);
        let kind = match next()?.as_bytes() {
            [kind] => *kind,
            _ => return Err(InvalidTraceRecord),
        };
        let decimal = |field: &str| field.parse::<u64>().map_err(|_| InvalidTraceRecord);
        let hex = |field: &str| u64::from_str_radix(field, 16).map_err(|_| InvalidTraceRecord);
        let record = Self {
            kind,
            branches: decimal(next()?)?,
            time_ns: decimal(next()?)?,
            extra: hex(next()?)?,
            rip: hex(next()?)?,
        };
        if fields.next().is_some() {
            return Err(InvalidTraceRecord);
        }
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_keep_the_engine_text_format_and_round_trip() {
        let record = TraceRecord {
            kind: b'P',
            branches: 12,
            time_ns: 3_000,
            extra: 0xfe,
            rip: 0xffff_ffff_8100_0000,
        };
        assert_eq!(record.to_string(), "P 12 3000 fe ffffffff81000000");
        assert_eq!("P 12 3000 fe ffffffff81000000\n".parse(), Ok(record));
        for bad in [
            "",
            "P 1 2 3",
            "P 1 2 3 4 5",
            "PP 1 2 3 4",
            "P x 2 3 4",
            "P 1 2 g 4",
        ] {
            assert_eq!(bad.parse::<TraceRecord>(), Err(InvalidTraceRecord), "{bad}");
        }
    }
}
