//! One virtio-blk request executed synchronously against a [`Store`].
//!
//! The transport supplies the request's device-readable and device-writable
//! bytes as two logical streams in chain order ([`Buffers`]). Execution copies
//! at most one sector at a time between those streams and the store, so its
//! memory does not grow with request size. Sector numbers are 512-byte units,
//! as virtio-blk defines them.
//!
//! A request without a 16-byte header or a status byte, or whose chain
//! exceeds `u32::MAX` bytes, cannot be answered and is [`Malformed`]: nothing
//! is read past the header and nothing is written. Every answerable request is
//! fully validated before any effect, then completes: an unsupported type is
//! `UNSUPP`; a known type with misplaced or partial data, or a range past the
//! end, is `IOERR`. Neither changes the store or writes data. A completed
//! request writes its data in increasing writable offset, then its status
//! into the last writable byte. A write reads all of its data before writing
//! the status, so aliasing buffers observe the same order on every transport.

// TODO: Re-evaluate: separate backends consume this canonical helper.
// Crate-private access cannot cross packages; representation remains private.
// TODO: Re-evaluate: request semantics are the one device policy PV's ring
// and the QEMU adapter must share; their transports differ. Streams in, data
// and status out, so no caller can bypass store durability or validation.

use super::{Error, Store, SECTOR};

const TYPE_IN: u32 = 0;
const TYPE_OUT: u32 = 1;
const TYPE_FLUSH: u32 = 4;
const TYPE_GET_ID: u32 = 8;
pub const STATUS_OK: u8 = 0;
pub const STATUS_IOERR: u8 = 1;
pub const STATUS_UNSUPP: u8 = 2;
const HEADER: usize = 16;
/// Device serial for GET_ID: 20 bytes, NUL-padded.
const ID: &[u8; 20] = b"seedbox-disk\0\0\0\0\0\0\0\0";

/// A request that cannot be answered: no header, no status byte, or a chain
/// longer than virtio's `u32` lengths can describe.
#[derive(Debug, PartialEq, Eq)]
pub struct Malformed;

/// A request's device-readable and device-writable bytes, each one logical
/// stream in chain order. Callers of `read` and `write` stay within the stream
/// lengths. Writes become visible to later reads where buffers alias.
pub trait Buffers {
    fn readable(&self) -> usize;
    fn writable(&self) -> usize;
    fn read(&mut self, offset: usize, out: &mut [u8]);
    fn write(&mut self, offset: usize, bytes: &[u8]);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Completion {
    /// Bytes written into the writable stream, including the status byte.
    pub written: u32,
    pub status: u8,
}

/// Virtio limits a chain to `u32::MAX` bytes and its used length to `u32`.
pub fn representable(readable: usize, writable: usize) -> bool {
    readable
        .checked_add(writable)
        .is_some_and(|total| u32::try_from(total).is_ok())
}

/// Execute one request against `store` through its transport `buffers`.
pub fn execute(store: &mut Store, buffers: &mut impl Buffers) -> Result<Completion, Malformed> {
    let (readable, writable) = (buffers.readable(), buffers.writable());
    if readable < HEADER || writable == 0 || !representable(readable, writable) {
        return Err(Malformed);
    }
    let mut header = [0; HEADER];
    buffers.read(0, &mut header);
    let kind = u32::from_le_bytes(header[..4].try_into().expect("header"));
    let sector = u64::from_le_bytes(header[8..].try_into().expect("header"));
    let (input, room) = (readable - HEADER, writable - 1);
    // Validate in full; afterwards no step can fail.
    let valid = match kind {
        TYPE_IN if input == 0 => store.check(sector, room).map(|()| room),
        TYPE_OUT if room == 0 => store.check(sector, input).map(|()| 0),
        TYPE_FLUSH if input == 0 && room == 0 => Ok(0),
        TYPE_GET_ID if input == 0 => Ok(room.min(ID.len())),
        TYPE_IN | TYPE_OUT | TYPE_FLUSH | TYPE_GET_ID => Err(Error::Alignment),
        _ => return Ok(complete(buffers, 0, STATUS_UNSUPP)),
    };
    let Ok(data) = valid else {
        return Ok(complete(buffers, 0, STATUS_IOERR));
    };
    match kind {
        TYPE_IN => {
            for index in 0..room / SECTOR {
                buffers.write(index * SECTOR, store.sector(sector + index as u64));
            }
        }
        TYPE_OUT => {
            let mut bytes = [0; SECTOR];
            for index in 0..input / SECTOR {
                buffers.read(HEADER + index * SECTOR, &mut bytes);
                store
                    .write(sector + index as u64, &bytes)
                    .expect("validated write");
            }
        }
        TYPE_FLUSH => store.flush(),
        _ => buffers.write(0, &ID[..data]),
    }
    Ok(complete(buffers, data, STATUS_OK))
}

fn complete(buffers: &mut impl Buffers, data: usize, status: u8) -> Completion {
    buffers.write(buffers.writable() - 1, &[status]);
    Completion {
        written: (data + 1) as u32,
        status,
    }
}

#[cfg(test)]
mod tests;
