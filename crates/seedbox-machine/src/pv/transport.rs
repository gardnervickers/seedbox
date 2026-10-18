//! Private byte transport; write boundaries are not application messages.
// TODO: Re-evaluate: separate backends consume this canonical helper.
// Crate-private access cannot cross packages; representation remains private.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleWrite {
    pub time: u64,
    pub bytes: Vec<u8>,
}

/// One actual guest emission. Vector order is authoritative for equal times
/// across UART and lifecycle device writes; channels must not be merged later.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GuestWrite {
    Console(LifecycleWrite),
    Lifecycle(LifecycleWrite),
    /// One whole Ethernet frame, stamped at its guest transmit point.
    // TODO: Re-evaluate: frames share this stream so ties with console and
    // lifecycle writes keep their actual guest order without correlation state.
    Frame(LifecycleWrite),
}
impl GuestWrite {
    // Machine-side observation; the worker retains the underlying write time directly.
    #[allow(dead_code)]
    pub fn time(&self) -> u64 {
        match self {
            Self::Console(write) | Self::Lifecycle(write) | Self::Frame(write) => write.time,
        }
    }
}

/// Pending output belongs to snapshots until transferred at a stopped boundary.
// Shared with the worker binary; the library consumes writes instead of buffering them.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct LifecycleOutput(Vec<LifecycleWrite>);
#[cfg_attr(not(test), allow(dead_code))]
impl LifecycleOutput {
    pub fn push(&mut self, time: u64, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            self.0.push(LifecycleWrite { time, bytes });
        }
    }
    pub fn drain(&mut self) -> Vec<LifecycleWrite> {
        std::mem::take(&mut self.0)
    }
}

#[cfg(test)]
mod tests;

impl LifecycleOutput {
    /// Whether all pending device output has transferred to its stopped run
    /// response. This observation never consumes, merges or reorders writes.
    // TODO: Re-evaluate: a separate backend must validate its capture boundary
    // without mutating drain or exposing this buffer's private representation.
    pub fn is_empty(&self) -> bool {
        let Self(writes) = self;
        writes.is_empty()
    }
}
