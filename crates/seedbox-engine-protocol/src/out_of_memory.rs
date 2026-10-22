//! Kernel OOM victim evidence relayed by the guest supervisor.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Kill {
    pub pid: u32,
    /// Kernel task name (TASK_COMM_LEN), with kmsg's unprintable-byte escapes.
    pub command: String,
    /// Linux reports kB in units of 1024 bytes.
    pub anon_rss_kib: u64,
    /// The victim is the supervised application leader, not another process.
    pub application: bool,
}

/// One complete management line, excluding its newline.
pub fn decode(line: &[u8]) -> Result<Option<Kill>, &'static str> {
    let Some(payload) = line.strip_prefix(b"SBX_OOM1 ") else {
        return Ok(None);
    };
    let fields: Vec<_> = payload.split(|b| *b == b' ').collect();
    let [pid, rss, application, command] = fields[..] else {
        return Err("OOM fields");
    };
    fn number(bytes: &[u8]) -> Result<u64, &'static str> {
        if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
            return Err("OOM number");
        }
        std::str::from_utf8(bytes)
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or("OOM number")
    }
    fn digit(byte: u8) -> Result<u8, &'static str> {
        match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            _ => Err("OOM command hex"),
        }
    }
    // A 15-byte comm can occupy 60 bytes when kmsg escapes each byte as \\xNN.
    if command.is_empty() || command.len() % 2 != 0 || command.len() > 120 {
        return Err("OOM command length");
    }
    let bytes = command
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| Ok(digit(p[0])? * 16 + digit(p[1])?))
        .collect::<Result<Vec<_>, &'static str>>()?;
    let pid = u32::try_from(number(pid)?).map_err(|_| "OOM pid")?;
    if pid == 0 {
        return Err("OOM pid");
    }
    Ok(Some(Kill {
        pid,
        command: String::from_utf8_lossy(&bytes).into_owned(),
        anon_rss_kib: number(rss)?,
        application: match application {
            b"0" => false,
            b"1" => true,
            _ => return Err("OOM application"),
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn kernel_victim_frame_and_invalid_inputs() {
        assert_eq!(decode(b"SBX_EXIT1 status=137"), Ok(None));
        assert_eq!(
            decode(b"SBX_OOM1 31 300948 1 707974686f6e"),
            Ok(Some(Kill {
                pid: 31,
                command: "python".into(),
                anon_rss_kib: 300948,
                application: true
            }))
        );
        for frame in [
            "SBX_OOM1 0 1 1 61",
            "SBX_OOM1 4294967296 1 1 61",
            "SBX_OOM1 1 18446744073709551616 1 61",
            "SBX_OOM1 1 1 2 61",
            "SBX_OOM1 1 1 1 6G",
            "SBX_OOM1 1 1 1 6",
            "SBX_OOM1 1 1 1 ",
            "SBX_OOM1 1 1 1 61 extra",
        ] {
            assert!(decode(frame.as_bytes()).is_err(), "{frame}");
        }
    }
}
