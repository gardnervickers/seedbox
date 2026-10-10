//! Application output carried by the guest supervisor's management transport.
//! Frames are bounded and binary-safe. Console and supervisor diagnostics are
//! intentionally outside this format.
use serde::{Deserialize, Serialize};

pub const MAX_BYTES: usize = 2048;
pub const PREFIX: &[u8] = b"SBX_LOG1 ";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Stream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Chunk {
    pub stream: Stream,
    pub bytes: Vec<u8>,
}

/// Decode one complete management line, excluding its newline. Other frame
/// kinds are ignored; malformed application output is reported explicitly.
pub fn decode(line: &[u8]) -> Result<Option<Chunk>, &'static str> {
    let Some(payload) = line.strip_prefix(PREFIX) else {
        return Ok(None);
    };
    let (stream, hex) = match payload {
        [b'1', b' ', hex @ ..] => (Stream::Stdout, hex),
        [b'2', b' ', hex @ ..] => (Stream::Stderr, hex),
        _ => return Err("invalid application log stream"),
    };
    if hex.is_empty() || hex.len() > MAX_BYTES * 2 || hex.len() % 2 != 0 {
        return Err("invalid application log length");
    }
    fn digit(byte: u8) -> Result<u8, &'static str> {
        match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            _ => Err("invalid application log encoding"),
        }
    }
    let bytes = hex
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| Ok(digit(pair[0])? * 16 + digit(pair[1])?))
        .collect::<Result<Vec<_>, &'static str>>()?;
    Ok(Some(Chunk { stream, bytes }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unrelated_management_frames_are_ignored_and_bad_log_frames_rejected() {
        assert_eq!(decode(b"SBX_RPC1 1 1 {}"), Ok(None));
        for frame in [
            b"SBX_LOG1 3 00".as_slice(),
            b"SBX_LOG1 1 ",
            b"SBX_LOG1 1 0",
            b"SBX_LOG1 1 gg",
            b"SBX_LOG1 2 0A",
        ] {
            assert!(decode(frame).is_err());
        }
        let maximum = format!("SBX_LOG1 1 {}", "ff".repeat(MAX_BYTES));
        assert_eq!(
            decode(maximum.as_bytes()).unwrap().unwrap().bytes.len(),
            MAX_BYTES
        );
        assert!(decode(format!("{maximum}00").as_bytes()).is_err());
    }
}
