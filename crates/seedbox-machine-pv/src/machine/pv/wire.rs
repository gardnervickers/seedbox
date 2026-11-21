//! Private binary trailers for guest writes and host frames. They follow a
//! JSON `Run` or `Stopped` message on the worker stream, which carries only
//! the record count. Record bytes never pass through JSON, so the JSON message
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
// A trailer keeps one ordered write stream outside the 8 MiB JSON cap without
// a side file per run. Counts are u64, so they impose no run limit narrower
// than memory.
// The client and the worker each use one direction of each trailer.
#![allow(dead_code)]
use seedbox_machine::pv::{
    ethernet::{MAX_FRAME_BYTES, MIN_FRAME_BYTES},
    transport::{GuestWrite, LifecycleWrite},
};
use std::io::{Read, Write};

type Result<T> = std::result::Result<T, String>;

const CONSOLE: u8 = 0;
const LIFECYCLE: u8 = 1;
const FRAME: u8 = 2;
const END: u8 = 0xff;

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

/// A complete contiguous packet can lend its bytes until decoding returns.
/// Wrapped packets and messages larger than the ring use the buffered stream.
pub(super) enum Input<'a> {
    Slice(&'a [u8]),
    Stream(&'a mut dyn Read),
}
impl Read for Input<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Slice(slice) => slice.read(buffer),
            Self::Stream(reader) => reader.read(buffer),
        }
    }
}
impl Input<'_> {
    pub(super) fn header<T: serde::de::DeserializeOwned>(&mut self) -> Result<Option<T>> {
        match self {
            Self::Stream(reader) => super::protocol::receive(reader),
            Self::Slice(slice) => {
                let length = super::protocol::header_length(read_exact(slice, "header")?)?;
                let body = slice
                    .split_off(..length)
                    .ok_or("PV protocol truncated header")?;
                serde_json::from_slice(body)
                    .map(Some)
                    .map_err(|e| format!("PV protocol decode: {e}"))
            }
        }
    }

    fn body(&mut self, length: usize) -> Result<Vec<u8>> {
        if let Self::Slice(slice) = self {
            return slice
                .split_off(..length)
                .map(<[u8]>::to_vec)
                .ok_or_else(|| "PV trailer truncated body".into());
        }
        // Grow only with bytes actually received from the stream.
        let mut bytes = Vec::new();
        self.take(length as u64)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("PV trailer body: {e}"))?;
        if bytes.len() != length {
            return Err("PV trailer truncated body".into());
        }
        Ok(bytes)
    }
}

fn write_record(write: &GuestWrite) -> Result<(u8, &LifecycleWrite, u32)> {
    let (tag, write) = match write {
        GuestWrite::Console(write) => (CONSOLE, write),
        GuestWrite::Lifecycle(write) => (LIFECYCLE, write),
        GuestWrite::Frame(write) => {
            frame_length(write.bytes.len())?;
            (FRAME, write)
        }
    };
    let length = u32::try_from(write.bytes.len()).map_err(|_| "PV write too large")?;
    Ok((tag, write, length))
}

/// Validate the whole trailer before its first fragment becomes visible.
pub(super) fn writes_size(writes: &[GuestWrite]) -> Result<usize> {
    // One end tag plus a tag, timestamp and length for each record.
    writes.iter().try_fold(1usize, |size, write| {
        let (_, _, length) = write_record(write)?;
        size.checked_add(13 + length as usize)
            .ok_or_else(|| "PV write trailer too large".into())
    })
}
/// Send `writes` in order. The JSON message before it carries `writes.len()`.
/// Pieces go straight to `out`, which buffers or claims them itself.
pub(super) fn send_writes(out: &mut impl Write, writes: &[GuestWrite]) -> Result<()> {
    for write in writes {
        let (tag, write, length) = write_record(write)?;
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
pub(super) fn receive_writes(input: &mut Input<'_>, count: u64) -> Result<Vec<GuestWrite>> {
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
        let bytes = input.body(length)?;
        writes.push(variant(LifecycleWrite { time, bytes }));
    }
    match read_exact::<1>(input, "end")? {
        [END] => Ok(writes),
        _ => Err("PV trailer longer than its count".into()),
    }
}

/// Send host frames in inbox order. The JSON message carries `frames.len()`.
/// Pieces go straight to `out`, which buffers or claims them itself.
pub(super) fn send_frames(out: &mut impl Write, frames: &[Vec<u8>]) -> Result<()> {
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
pub(super) fn receive_frames(input: &mut Input<'_>, count: u64) -> Result<Vec<Vec<u8>>> {
    let mut frames = Vec::new();
    for _ in 0..count {
        let length = u16::from_le_bytes(read_exact(input, "length")?) as usize;
        if length == 0 {
            return Err("PV trailer ended before its count".into());
        }
        frame_length(length)?;
        frames.push(input.body(length)?);
    }
    match u16::from_le_bytes(read_exact(input, "end")?) {
        0 => Ok(frames),
        _ => Err("PV trailer longer than its count".into()),
    }
}

#[cfg(test)]
mod tests;
