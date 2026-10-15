//! Guest kernel out-of-memory kills, reported by the guest supervisor on its
//! management transport (`guest/oom-report.h`). The supervisor counts kills
//! with `/proc/vmstat` `oom_kill` and takes each victim's details from the
//! kernel's own log record. Both are guest output, so a report is as
//! deterministic as the run that produced it.
use serde::{Deserialize, Serialize};

pub const PREFIX: &[u8] = b"SBX_OOM1 ";
/// Linux `TASK_COMM_LEN` less its terminator, after `/dev/kmsg` escapes each
/// unprintable byte as four characters.
pub const MAX_COMMAND_BYTES: usize = 15 * 4;

/// The process the guest kernel killed, as its log record describes it.
/// Sizes are the kernel's kB, which are KiB.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Process {
    pub pid: u32,
    /// The kernel's task name, at most 15 bytes and possibly truncated.
    pub command: String,
    pub total_vm_kib: u64,
    pub anon_rss_kib: u64,
    pub file_rss_kib: u64,
    pub shmem_rss_kib: u64,
}

/// One kill. `process` is absent when the kernel counted the kill but its log
/// record was lost or unreadable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Kill {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<Process>,
}

/// Decode one complete management line, excluding its newline. Other frame
/// kinds are ignored. The frame is `SBX_OOM1 -` for a kill without details, or
/// `SBX_OOM1 PID TOTAL_VM ANON_RSS FILE_RSS SHMEM_RSS COMMAND_HEX`.
pub fn decode(line: &[u8]) -> Result<Option<Kill>, &'static str> {
    let Some(payload) = line.strip_prefix(PREFIX) else {
        return Ok(None);
    };
    if payload == b"-" {
        return Ok(Some(Kill { process: None }));
    }
    let fields: Vec<&[u8]> = payload.split(|byte| *byte == b' ').collect();
    let [pid, total_vm, anon_rss, file_rss, shmem_rss, command] = fields[..] else {
        return Err("invalid out-of-memory frame fields");
    };
    fn number(field: &[u8]) -> Result<u64, &'static str> {
        if field.is_empty() || field.len() > 20 || !field.iter().all(u8::is_ascii_digit) {
            return Err("invalid out-of-memory frame number");
        }
        std::str::from_utf8(field)
            .ok()
            .and_then(|text| text.parse().ok())
            .ok_or("invalid out-of-memory frame number")
    }
    fn digit(byte: u8) -> Result<u8, &'static str> {
        match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            _ => Err("invalid out-of-memory command encoding"),
        }
    }
    if command.is_empty() || command.len() % 2 != 0 || command.len() > MAX_COMMAND_BYTES * 2 {
        return Err("invalid out-of-memory command length");
    }
    let command = command
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| Ok(digit(pair[0])? * 16 + digit(pair[1])?))
        .collect::<Result<Vec<_>, &'static str>>()?;
    Ok(Some(Kill {
        process: Some(Process {
            pid: u32::try_from(number(pid)?).map_err(|_| "invalid out-of-memory pid")?,
            command: String::from_utf8_lossy(&command).into_owned(),
            total_vm_kib: number(total_vm)?,
            anon_rss_kib: number(anon_rss)?,
            file_rss_kib: number(file_rss)?,
            shmem_rss_kib: number(shmem_rss)?,
        }),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_details_and_detail_free_kills_and_ignores_other_frames() {
        assert_eq!(decode(b"SBX_LOG1 1 61"), Ok(None));
        assert_eq!(decode(b"SBX_OOM1 -"), Ok(Some(Kill { process: None })));
        let kill = decode(b"SBX_OOM1 412 1203328 446228 1024 0 6e6174732d736572766572")
            .unwrap()
            .unwrap();
        assert_eq!(
            kill.process,
            Some(Process {
                pid: 412,
                command: "nats-server".into(),
                total_vm_kib: 1_203_328,
                anon_rss_kib: 446_228,
                file_rss_kib: 1024,
                shmem_rss_kib: 0,
            })
        );
    }

    #[test]
    fn rejects_malformed_frames() {
        for frame in [
            b"SBX_OOM1 ".as_slice(),
            b"SBX_OOM1 --",
            b"SBX_OOM1 1 2 3 4 5",
            b"SBX_OOM1 1 2 3 4 5 61 7",
            b"SBX_OOM1 x 2 3 4 5 61",
            b"SBX_OOM1 -1 2 3 4 5 61",
            b"SBX_OOM1 4294967296 2 3 4 5 61",
            b"SBX_OOM1 1 2 3 4 18446744073709551616 61",
            b"SBX_OOM1 1 2 3 4 5 ",
            b"SBX_OOM1 1 2 3 4 5 6",
            b"SBX_OOM1 1 2 3 4 5 6G",
            b"SBX_OOM1 1 2 3 4 5 6A",
            b"SBX_OOM1 1  3 4 5 61",
        ] {
            assert!(decode(frame).is_err(), "{}", String::from_utf8_lossy(frame));
        }
        let longest = format!("SBX_OOM1 1 2 3 4 5 {}", "61".repeat(MAX_COMMAND_BYTES));
        assert!(decode(longest.as_bytes()).is_ok());
        assert!(decode(format!("{longest}61").as_bytes()).is_err());
    }
}
