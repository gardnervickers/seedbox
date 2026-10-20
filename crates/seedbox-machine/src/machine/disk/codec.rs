//! Private `SBXSTOR1` encoding of a store's two overlays, never its base.
//!
//! Layout, little-endian: the magic `SBXSTOR1`; capacity in sectors (u64);
//! then the durable layer and the unflushed layer, each a count (u64) followed
//! by that many entries of sector index (u64) and 512 data bytes, in strictly
//! increasing sector order. The base stays with its owner: whole-machine
//! snapshots retain it next to this encoding, which alone is incomplete.
//!
//! Decoding validates everything before replacing either overlay. Counts are
//! bounded by the remaining input and the capacity before any allocation;
//! wrong magic or capacity, truncation, trailing bytes, duplicate, unordered
//! or out-of-range sectors are errors that leave the store unchanged.

// Overlays cross process boundaries (QEMU migration,
// boot inputs, exports, PV captures) while each owner keeps the immutable
// base; a private versioned encoding with exact validation is the narrowest.

use super::{Sector, Store, SECTOR};
use std::{collections::BTreeMap, sync::Arc};

const MAGIC: &[u8; 8] = b"SBXSTOR1";
const ENTRY: usize = 8 + SECTOR;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodecError {
    Magic,
    Capacity,
    Truncated,
    Count,
    Order,
    Range,
    Trailing,
}

/// The longest valid encoding for `capacity` sectors: both layers full.
pub fn encoded_limit(capacity: u64) -> u128 {
    32 + 2 * u128::from(capacity) * ENTRY as u128
}

/// Validate one complete `SBXSTOR1` encoding without allocating or changing data.
///
/// Uses the same framing, count, sector-order/range and trailing-byte checks as
/// [`Store::replace_overlays`], with identical errors for the same capacity.
/// `capacity` is a sector count, not a byte length. The caller must first
/// validate that it is nonzero and matches its independently owned base. This
/// scanner only compares the encoded capacity to that supplied value; it does
/// not validate a base or impose byte-representability or host-memory limits.
/// All nonzero `u64` capacities supported by [`Store::zeroed`] remain valid.
///
/// Both layers may contain the same sector. Zero-filled sectors are data, not
/// holes; empty layers do not imply an empty or zero-filled base. The base is
/// never present in this encoding and stays with its owner.
// Offline decoders in both backends validate structure without building
// temporary overlay maps; this borrowed scan shares restore's rules.
pub fn validate_overlays(bytes: &[u8], capacity: u64) -> Result<(), CodecError> {
    walk(bytes, capacity, |_, _, _| {})
}

impl Store {
    /// Encode both overlays; this never flushes or changes the store.
    pub fn encode_overlays(&self) -> Vec<u8> {
        let entries = self.durable.len() + self.unflushed.len();
        let mut bytes = Vec::with_capacity(32 + entries * ENTRY);
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&self.capacity.to_le_bytes());
        for layer in [&self.durable, &self.unflushed] {
            bytes.extend_from_slice(&(layer.len() as u64).to_le_bytes());
            for (sector, data) in layer {
                bytes.extend_from_slice(&sector.to_le_bytes());
                bytes.extend_from_slice(&data[..]);
            }
        }
        bytes
    }

    /// Replace both overlays with a validated encoding for this capacity.
    pub fn replace_overlays(&mut self, bytes: &[u8]) -> Result<(), CodecError> {
        let mut layers: [BTreeMap<u64, Sector>; 2] = [BTreeMap::new(), BTreeMap::new()];
        walk(bytes, self.capacity, |layer, sector, data| {
            layers[layer].insert(sector, Arc::new(*data));
        })?;
        let [durable, unflushed] = layers;
        (self.durable, self.unflushed) = (durable, unflushed);
        Ok(())
    }
}

// Two concrete consumers: validation ignores borrowed entries; restoration
// copies them into temporary maps and commits only after the complete scan.
fn walk(
    bytes: &[u8],
    capacity: u64,
    mut visit: impl FnMut(usize, u64, &[u8; SECTOR]),
) -> Result<(), CodecError> {
    let mut input = Input(bytes);
    if input.take(MAGIC.len())? != MAGIC {
        return Err(CodecError::Magic);
    }
    if input.word()? != capacity {
        return Err(CodecError::Capacity);
    }
    for layer in 0..2 {
        input.layer(capacity, |sector, data| visit(layer, sector, data))?;
    }
    if !input.0.is_empty() {
        return Err(CodecError::Trailing);
    }
    Ok(())
}

struct Input<'a>(&'a [u8]);

impl<'a> Input<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8], CodecError> {
        if self.0.len() < length {
            return Err(CodecError::Truncated);
        }
        let (taken, rest) = self.0.split_at(length);
        self.0 = rest;
        Ok(taken)
    }

    fn word(&mut self) -> Result<u64, CodecError> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("eight bytes"),
        ))
    }

    fn layer(
        &mut self,
        capacity: u64,
        mut visit: impl FnMut(u64, &'a [u8; SECTOR]),
    ) -> Result<(), CodecError> {
        let count = self.word()?;
        if count > capacity || count > (self.0.len() / ENTRY) as u64 {
            return Err(CodecError::Count);
        }
        let mut previous = None;
        for _ in 0..count {
            let sector = self.word()?;
            if sector >= capacity {
                return Err(CodecError::Range);
            }
            if previous.is_some_and(|p| sector <= p) {
                return Err(CodecError::Order);
            }
            previous = Some(sector);
            let data: &[u8; SECTOR] = self.take(SECTOR)?.try_into().expect("one sector");
            visit(sector, data);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
