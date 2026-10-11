//! Application-only device personality, separate from the accepted PV fixture.
pub mod log_capture;
pub mod virtio;
pub mod worker;
pub mod world;
pub mod world_adapter;
use crate::Result;
use serde::{Deserialize, Serialize};

pub const SUPERVISOR_MMIO: u64 = 0x0a02_0000;
pub const NET_MMIO: u64 = 0x0b00_0000;
pub const CONSOLE_MMIO: u64 = 0x0b01_0000;
pub const BLOCK_MMIO: u64 = 0x0b02_0000;
pub const NET_SPI: u32 = 41;
pub const CONSOLE_SPI: u32 = 42;
pub const BLOCK_SPI: u32 = 43;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootIdentity {
    pub memory_bytes: u64,
    pub initrd_addr: u64,
    pub dtb_addr: u64,
    pub command_line: String,
    pub seed: u64,
}
impl BootIdentity {
    pub fn from_spec(spec: crate::boot::BootSpec<'_>) -> Result<Self> {
        Ok(Self::from_layout(&spec.validate()?))
    }
    pub(crate) fn from_layout(layout: &crate::boot::BootLayout<'_>) -> Self {
        Self {
            memory_bytes: layout.memory_bytes() as u64,
            initrd_addr: layout.initrd_range().start,
            dtb_addr: layout.dtb_range().start,
            command_line: layout.command_line().into(),
            seed: layout.seed(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Devices {
    pub net: virtio::Virtio,
    pub console: virtio::Virtio,
    pub block: Option<virtio::Virtio>,
    pub events: Vec<Event>,
    pub exit: Option<(u8, bool)>,
    pub barrier: bool,
    pub entropy: VecDeque<u32>,
}
use std::collections::VecDeque;
impl Devices {
    pub fn new(mac: [u8; 6], disk: Option<crate::disk::Disk>) -> Self {
        Self {
            net: virtio::Virtio::net(mac),
            console: virtio::Virtio::console(),
            block: disk.map(virtio::Virtio::block),
            events: Vec::new(),
            exit: None,
            barrier: false,
            entropy: VecDeque::new(),
        }
    }
    pub fn state_digest(&self) -> Result<String> {
        let device = (
            &self.events,
            self.exit,
            self.barrier,
            &self.entropy,
            self.net.state_digest()?,
            self.console.state_digest()?,
            self.block
                .as_ref()
                .map(virtio::Virtio::state_digest)
                .transpose()?,
        );
        Ok(crate::witness::hash(
            &serde_json::to_vec(&device).map_err(|e| e.to_string())?,
        ))
    }
}

/// Application evidence has its own version and includes the actual boot
/// layout. It must never be interpreted as a legacy fixture witness.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub format: u32,
    pub boot: BootIdentity,
    pub kernel_sha256: String,
    pub initrd_sha256: String,
    pub timer_policy: Option<TimerPolicy>,
    pub disk_digest: Option<String>,
    #[serde(default)]
    pub control_channel: Option<ControlChannel>,
    pub time_ns: u64,
    pub exits: u64,
    pub trace_hash: String,
    pub console_hash: String,
    pub witness: String,
    pub state_digest: Option<String>,
    pub state_digest_parts: Option<std::collections::BTreeMap<String, String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlChannel {
    pub transport: String,
    pub tx_bytes: u64,
    pub rx_dma_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimerPolicy {
    pub seed: u64,
    pub domain: String,
    pub jitter_ns: u64,
}

/// The bound on the seeded extra charge the guest kernel adds to each PV
/// progress call, and the seed of the stream it draws from. Both go to the
/// time page at boot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Progress {
    /// The project's CPU speed in Hz, which a throttle is a fraction of.
    pub speed_hz: u64,
    /// Nominal charge per progress call; zero charges a call like a syscall.
    pub ns: u64,
    /// Bound on the seeded variation of that charge, each way; zero is off.
    pub jitter_ns: u64,
    /// First seed of the stream the guest kernel draws the variation from.
    pub seed: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    Frame(Vec<u8>),
    Control(Vec<u8>),
    Ready,
    Effect { id: u32, success: bool },
    Exit { code: u8, setup_error: bool },
    Barrier,
}

/// The dedicated guest driver admits only these logical supervisor registers.
/// No physical address, arbitrary register, or access width is guest-selected.
pub fn supervisor_write(offset: u64, width: u8, value: u64) -> Result<Event> {
    if !offset.is_multiple_of(8) {
        return Err("unaligned supervisor register".into());
    }
    match (offset / 8, width) {
        (0xf4 | 0xf5, 1) if value <= u8::MAX as u64 => Ok(Event::Exit {
            code: value as u8,
            setup_error: offset / 8 == 0xf5,
        }),
        (0xf6, 4) if value == 0 => Ok(Event::Ready),
        (0xf6 | 0xf7, 4) if value > 0 && value <= u32::MAX as u64 => Ok(Event::Effect {
            id: value as u32,
            success: offset / 8 == 0xf6,
        }),
        (0xf8, 4) if value == 1 => Ok(Event::Barrier),
        _ => Err("invalid supervisor register write".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn supervisor_registers_are_width_checked_and_do_not_conflate_errors() {
        assert_eq!(
            supervisor_write(0xf4 * 8, 1, 0).unwrap(),
            Event::Exit {
                code: 0,
                setup_error: false
            }
        );
        assert_eq!(
            supervisor_write(0xf5 * 8, 1, 0).unwrap(),
            Event::Exit {
                code: 0,
                setup_error: true
            }
        );
        assert_eq!(supervisor_write(0xf6 * 8, 4, 0).unwrap(), Event::Ready);
        assert_eq!(
            supervisor_write(0xf7 * 8, 4, 1).unwrap(),
            Event::Effect {
                id: 1,
                success: false
            }
        );
        for (offset, width, value) in [
            (0xf4 * 8, 4, 0),
            (0xf6 * 8, 1, 1),
            (0xf7 * 8, 4, 0),
            (0xf8 * 8, 4, 0),
            (0xf4 * 8 + 1, 1, 0),
            (0xf9 * 8, 4, 1),
            (0xf4 * 8, 1, 256),
        ] {
            assert!(supervisor_write(offset, width, value).is_err());
        }
    }
}
