//! Private binary trailers for guest writes and host frames. They follow a
//! JSON `Run` or `Stopped` message on the worker pipe, which carries only the
//! record count. Record bytes never pass through JSON, so the JSON message
//! limit does not bound how much a run may emit or receive.
//!
//! Guest writes: `count` records of `tag: u8`, `time: u64 LE`, `length: u32
//! LE`, then `length` bytes, then the end tag. Tags are Console 0, Lifecycle 1
//! and Frame 2. Record order is the guest's emission order, including ties.
//!
//! Host frames: `count` records of `length: u16 LE` and the frame, then a zero
//! length as the end mark.
//!
//! A reader checks the declared count against the end mark, validates each
//! length before reading, and allocates only as bytes actually arrive.
//
// TODO: Re-evaluate: the trailer keeps one ordered write stream, with bytes
// outside the 8 MiB JSON cap, without a new crate or a side file per run.
// Counts are u64, so they impose no run limit narrower than memory.
// The client and the worker each use one direction of each trailer.
#![allow(dead_code)]
use seedbox_machine::pv::{
    ethernet::{MAX_FRAME_BYTES, MIN_FRAME_BYTES},
    transport::{GuestWrite, LifecycleWrite},
};
use std::io::{BufWriter, Read, Write};

type Result<T> = std::result::Result<T, String>;

const CONSOLE: u8 = 0;
const LIFECYCLE: u8 = 1;
const FRAME: u8 = 2;
const END: u8 = 0xff;
const BUFFER: usize = 64 << 10;

fn frame_length(length: usize) -> Result<()> {
    if (MIN_FRAME_BYTES..=MAX_FRAME_BYTES).contains(&length) {
        Ok(())
    } else {
        Err(format!("PV frame of {length} bytes"))
    }
}

fn write_error(error: std::io::Error) -> String {
    format!("PV trailer write: {error}")
}

fn read_exact<const N: usize>(input: &mut impl Read, what: &str) -> Result<[u8; N]> {
    let mut bytes = [0; N];
    input
        .read_exact(&mut bytes)
        .map_err(|e| format!("PV trailer {what}: {e}"))?;
    Ok(bytes)
}

/// Read exactly `length` bytes, growing only with bytes actually received.
fn read_bytes(input: &mut impl Read, length: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    input
        .take(length as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("PV trailer body: {e}"))?;
    if bytes.len() != length {
        return Err("PV trailer truncated body".into());
    }
    Ok(bytes)
}

/// Send `writes` in order. The JSON message before it carries `writes.len()`.
pub(super) fn send_writes(out: &mut impl Write, writes: &[GuestWrite]) -> Result<()> {
    let mut out = BufWriter::with_capacity(BUFFER, out);
    for write in writes {
        let (tag, write) = match write {
            GuestWrite::Console(write) => (CONSOLE, write),
            GuestWrite::Lifecycle(write) => (LIFECYCLE, write),
            GuestWrite::Frame(write) => {
                frame_length(write.bytes.len())?;
                (FRAME, write)
            }
        };
        let length = u32::try_from(write.bytes.len()).map_err(|_| "PV write too large")?;
        out.write_all(&[tag])
            .and_then(|_| out.write_all(&write.time.to_le_bytes()))
            .and_then(|_| out.write_all(&length.to_le_bytes()))
            .and_then(|_| out.write_all(&write.bytes))
            .map_err(write_error)?;
    }
    out.write_all(&[END])
        .and_then(|_| out.flush())
        .map_err(write_error)
}

/// Receive exactly `count` writes and the end tag.
pub(super) fn receive_writes(input: &mut impl Read, count: u64) -> Result<Vec<GuestWrite>> {
    let mut writes = Vec::new();
    for _ in 0..count {
        // Judge the tag before reading further: a live peer that sent END or
        // garbage may send nothing more until it gets a reply.
        let [tag] = read_exact::<1>(input, "tag")?;
        let variant: fn(LifecycleWrite) -> GuestWrite = match tag {
            CONSOLE => GuestWrite::Console,
            LIFECYCLE => GuestWrite::Lifecycle,
            FRAME => GuestWrite::Frame,
            END => return Err("PV trailer ended before its count".into()),
            tag => return Err(format!("PV trailer tag {tag}")),
        };
        let time = u64::from_le_bytes(read_exact(input, "time")?);
        let length = u32::from_le_bytes(read_exact(input, "length")?) as usize;
        if tag == FRAME {
            frame_length(length)?;
        }
        let bytes = read_bytes(input, length)?;
        writes.push(variant(LifecycleWrite { time, bytes }));
    }
    match read_exact::<1>(input, "end")? {
        [END] => Ok(writes),
        _ => Err("PV trailer longer than its count".into()),
    }
}

/// Send host frames in inbox order. The JSON message carries `frames.len()`.
pub(super) fn send_frames(out: &mut impl Write, frames: &[Vec<u8>]) -> Result<()> {
    let mut out = BufWriter::with_capacity(BUFFER, out);
    for frame in frames {
        frame_length(frame.len())?;
        out.write_all(&(frame.len() as u16).to_le_bytes())
            .and_then(|_| out.write_all(frame))
            .map_err(write_error)?;
    }
    out.write_all(&0u16.to_le_bytes())
        .and_then(|_| out.flush())
        .map_err(write_error)
}

/// Receive exactly `count` host frames and the end mark.
pub(super) fn receive_frames(input: &mut impl Read, count: u64) -> Result<Vec<Vec<u8>>> {
    let mut frames = Vec::new();
    for _ in 0..count {
        let length = u16::from_le_bytes(read_exact(input, "length")?) as usize;
        if length == 0 {
            return Err("PV trailer ended before its count".into());
        }
        frame_length(length)?;
        frames.push(read_bytes(input, length)?);
    }
    match u16::from_le_bytes(read_exact(input, "end")?) {
        0 => Ok(frames),
        _ => Err("PV trailer longer than its count".into()),
    }
}

#[cfg(test)]
mod tests;
