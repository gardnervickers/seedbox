//! Experimental same-ISA block executor, not a Linux application runtime.
//!
//! Decoding, source ownership and logical charges are independent of Linux. The
//! opt-in `linux-proof` example uses Linux signal frames as a deliberately slow
//! diagnostic dispatch gate. It is not the eventual linked-edge hot path.
//! Unsupported instructions fail closed; they never escape to native source.
#![no_std]
extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod aarch64;
pub mod meter;
pub mod source;
pub mod x86_64;

use alloc::vec::Vec;
use core::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Unsupported { pc: u64 },
    Truncated { pc: u64 },
    Relocation { pc: u64 },
    Range,
    GenerationExhausted,
    ChargePhase,
}

/// The trap is executor-owned, not an application instruction or a charge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcMap {
    pub cache: Range<u64>,
    pub source: u64,
}

#[derive(Debug, Clone)]
pub struct Block<T> {
    pub bytes: Vec<u8>,
    pub maps: Vec<PcMap>,
    pub source: Range<u64>,
    pub instructions: u32,
    pub terminal: T,
    pub trap: u64,
}

impl<T> Block<T> {
    /// Maps are immutable and remain usable through an invalidated generation's pin.
    pub fn source_pc(&self, cache_pc: u64) -> Option<u64> {
        self.maps
            .iter()
            .find(|m| m.cache.contains(&cache_pc))
            .map(|m| m.source)
    }
}
