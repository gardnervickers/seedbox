//! Guest kernel crash reports relayed by the guest supervisor, the evidence
//! for [`crate::property::DEFAULT_NO_CRASH`].
//!
//! The supervisor already follows `/dev/kmsg` for out-of-memory victims. It
//! also relays one line per crash: the fatal-signal report the kernel prints,
//! with `print-fatal-signals=1`, when a core-dumping signal (SIGSEGV, SIGBUS,
//! SIGILL, SIGFPE, SIGTRAP, SIGABRT and the rest) kills a process,
//! `[EXECUTABLE: ]COMMAND: potentially unexpected fatal signal N.`, where
//! EXECUTABLE is the executable's file name and COMMAND the thread's name.
//!
//! No other line counts. A segfault, trap or unhandled-exception line comes
//! before the fatal-signal report of the same crash, and register dumps after
//! it; relaying those too would count one crash twice. A fault the process
//! handles prints neither. A crash on a machine with `allowCrashes: true` is
//! not a violation.
//!
//! The frame is `SBX_CRASH1 MESSAGE_HEX`, where the message is the line's text
//! after its kmsg header. Nothing emits or reads this frame yet.
use serde::{Deserialize, Serialize};

pub const PREFIX: &[u8] = b"SBX_CRASH1 ";
/// A kmsg record's text holds at most this many bytes after its escapes.
pub const MAX_MESSAGE_BYTES: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct Report {
    /// The kernel's line, with kmsg's escapes for unprintable bytes.
    pub message: String,
    /// The signal number the line reports.
    pub signal: i32,
}

/// What separates the reporting task from the signal number.
pub const FATAL_SIGNAL_REPORT: &str = ": potentially unexpected fatal signal ";

/// Decode one complete management line, excluding its newline. Other frame
/// kinds are ignored.
pub fn decode(line: &[u8]) -> Result<Option<Report>, &'static str> {
    let Some(hex) = line.strip_prefix(PREFIX) else {
        return Ok(None);
    };
    if hex.is_empty() || hex.len() % 2 != 0 || hex.len() > MAX_MESSAGE_BYTES * 2 {
        return Err("crash message length");
    }
    fn digit(byte: u8) -> Result<u8, &'static str> {
        match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            _ => Err("crash message hex"),
        }
    }
    let bytes = hex
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| Ok(digit(p[0])? * 16 + digit(p[1])?))
        .collect::<Result<Vec<_>, &'static str>>()?;
    // kmsg escapes every unprintable byte, so a report is printable ASCII.
    if !bytes.iter().all(|b| (0x20..0x7f).contains(b)) {
        return Err("crash message bytes");
    }
    let message = String::from_utf8(bytes).map_err(|_| "crash message bytes")?;
    let signal = message
        .rsplit_once(FATAL_SIGNAL_REPORT)
        .filter(|(task, _)| !task.is_empty())
        .and_then(|(_, number)| number.strip_suffix('.'))
        .filter(|n| !n.is_empty() && !n.starts_with('0') && n.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|n| n.parse::<i32>().ok())
        .filter(|n| (1..=64).contains(n))
        .ok_or("crash message is not a fatal-signal report")?;
    Ok(Some(Report { message, signal }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> String {
        text.bytes().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn kernel_crash_line_and_invalid_inputs() {
        assert_eq!(decode(b"SBX_OOM1 31 300948 1 707974686f6e"), Ok(None));
        let line = "libdynamorio.so: keel-watchdog: potentially unexpected fatal signal 6.";
        assert_eq!(
            decode(format!("SBX_CRASH1 {}", hex(line)).as_bytes()),
            Ok(Some(Report {
                message: line.into(),
                signal: 6,
            }))
        );
        let bare = "keel-watchdog: potentially unexpected fatal signal 11.";
        assert_eq!(
            decode(format!("SBX_CRASH1 {}", hex(bare)).as_bytes())
                .unwrap()
                .unwrap()
                .signal,
            11
        );
        let longest = format!(
            "SBX_CRASH1 {}{}",
            "61".repeat(MAX_MESSAGE_BYTES - 40),
            hex(": potentially unexpected fatal signal 6.")
        );
        assert!(decode(longest.as_bytes()).is_ok());
        // Other crash lines are never relayed; a frame carrying one is malformed.
        for other in [
            "keel-watchdog[77]: segfault at 0 ip 0000 sp 0000 error 4",
            "traps: keel-watchdog[77] trap invalid opcode ip:1 sp:2 error:0",
            ": potentially unexpected fatal signal 6.",
            "x: potentially unexpected fatal signal 6",
            "x: potentially unexpected fatal signal 06.",
            "x: potentially unexpected fatal signal 99.",
        ] {
            let frame = format!("SBX_CRASH1 {}", hex(other));
            assert!(decode(frame.as_bytes()).is_err(), "{other}");
        }
        for frame in [
            "SBX_CRASH1 ".to_string(),
            "SBX_CRASH1 6".into(),
            "SBX_CRASH1 6G".into(),
            "SBX_CRASH1 4A".into(),
            "SBX_CRASH1 0a".into(),
            "SBX_CRASH1 61 62".into(),
            format!("{longest}61"),
        ] {
            assert!(decode(frame.as_bytes()).is_err(), "{frame}");
        }
    }
}
