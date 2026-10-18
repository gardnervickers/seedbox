//! Canonical Ethernet frame bounds shared by the frame contract, the Driver
//! and NIC models. The PV worker binary includes this file by path.

// TODO: Re-evaluate: separate backends consume this canonical helper.
// Crate-private access cannot cross packages; representation remains private.
/// Smallest frame on the network: an Ethernet header with no payload.
pub const MIN_FRAME_BYTES: usize = 14;
/// Largest frame on the network: a 1500-byte MTU payload plus its 14-byte
/// Ethernet header, without FCS. Only lengths are checked; EtherTypes are
/// opaque, and a VLAN-tagged frame gets no extra allowance beyond this bound.
pub const MAX_FRAME_BYTES: usize = 1514;
/// Frames a NIC holds while the guest has no free receive buffer.
pub const RECEIVE_BACKLOG_FRAMES: usize = 256;

/// Whether `bytes` lies within the network's frame bounds.
pub fn frame_in_bounds(bytes: &[u8]) -> bool {
    (MIN_FRAME_BYTES..=MAX_FRAME_BYTES).contains(&bytes.len())
}

#[cfg(test)]
mod tests;
