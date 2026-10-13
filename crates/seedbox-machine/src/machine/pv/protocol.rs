//! Private process protocol. No world scheduling or worker-local semantic state.
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io::{Read, Write};
use std::path::PathBuf;

pub(super) type Result<T> = std::result::Result<T, String>;
const MAX_MESSAGE: usize = 8 << 20;
#[derive(Serialize, Deserialize)]
pub(super) enum Request {
    Boot {
        kernel: PathBuf,
        initrd: PathBuf,
        command_line: String,
        seed: u64,
        now: u64,
    },
    Run {
        now: u64,
        upto: u64,
        syscall_cost_ns: u64,
        shutdown: bool,
        lifecycle: Vec<u8>,
    },
    Capture(PathBuf),
    Restore(PathBuf),
}
#[derive(Debug, Serialize, Deserialize)]
pub(super) enum Response {
    Ready,
    Stopped {
        time: u64,
        stop: Stop,
        writes: Vec<crate::pv::transport::GuestWrite>,
    },
    Captured,
    Error(String),
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(super) enum Stop {
    Horizon,
    Blocked(Option<u64>),
    PoweredOff,
}

pub(super) fn send(out: &mut impl Write, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value).map_err(|e| format!("PV encode: {e}"))?;
    if bytes.len() > MAX_MESSAGE {
        return Err("PV protocol message too large".into());
    }
    out.write_all(&(bytes.len() as u32).to_le_bytes())
        .and_then(|_| out.write_all(&bytes))
        .and_then(|_| out.flush())
        .map_err(|e| format!("PV protocol write: {e}"))
}
pub(super) fn receive<T: DeserializeOwned>(input: &mut impl Read) -> Result<Option<T>> {
    let mut length = [0; 4];
    match input.read(&mut length[..1]) {
        Ok(0) => return Ok(None),
        Ok(_) => (),
        Err(e) => return Err(format!("PV protocol read: {e}")),
    }
    input
        .read_exact(&mut length[1..])
        .map_err(|e| format!("PV protocol header: {e}"))?;
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_MESSAGE {
        return Err("PV protocol message too large".into());
    }
    let mut bytes = vec![0; length];
    input
        .read_exact(&mut bytes)
        .map_err(|e| format!("PV protocol body: {e}"))?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| format!("PV protocol decode: {e}"))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn truncated_frames_are_errors_not_eof() {
        assert!(receive::<Request>(&mut &b"\x04"[..]).is_err());
        assert!(receive::<Request>(&mut &b"\x04\0\0\0{}"[..]).is_err());
        assert!(receive::<Request>(&mut &b"\xff\xff\xff\xff"[..]).is_err());
        assert!(receive::<Request>(&mut &b""[..]).unwrap().is_none());
    }
}
