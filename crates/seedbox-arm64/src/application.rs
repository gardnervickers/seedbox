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
    /// The same boot inputs, whatever the incarnation: a rebooted
    /// incarnation boots with its own entropy seed and without the launch
    /// barrier, and a checkpoint of it restores into a replacement built from
    /// the first incarnation's inputs. Restored RAM already holds everything
    /// the seed and the barrier decided.
    pub(crate) fn same_inputs(&self, other: &Self) -> bool {
        let line = |l: &str| -> String {
            l.split(' ')
                .filter(|token| *token != "seedbox.barrier=1")
                .collect::<Vec<_>>()
                .join(" ")
        };
        self.memory_bytes == other.memory_bytes
            && self.initrd_addr == other.initrd_addr
            && self.dtb_addr == other.dtb_addr
            && line(&self.command_line) == line(&other.command_line)
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
    /// The machine's configured CPU speed in Hz, which a throttle is a fraction of.
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
    Effect {
        id: u32,
        success: bool,
    },
    Exit {
        code: u8,
        setup_error: bool,
    },
    Barrier,
    /// The block device held the request a trigger selected.
    Held {
        trigger: u32,
    },
}

/// Exactly `serde_json::to_vec(event)`, the bytes the world's event witness
/// hashes, without the serializer's per-element cost for payload bytes.
pub(crate) fn event_json(event: &Event) -> Result<Vec<u8>> {
    let (name, bytes): (&[u8], &[u8]) = match event {
        Event::Frame(bytes) => (b"Frame", bytes),
        Event::Control(bytes) => (b"Control", bytes),
        _ => return serde_json::to_vec(event).map_err(|e| e.to_string()),
    };
    let mut out = Vec::with_capacity(name.len() + 7 + bytes.len() * 4);
    out.extend_from_slice(b"{\"");
    out.extend_from_slice(name);
    out.extend_from_slice(b"\":[");
    for (i, &byte) in bytes.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        if byte >= 100 {
            out.push(b'0' + byte / 100);
        }
        if byte >= 10 {
            out.push(b'0' + byte / 10 % 10);
        }
        out.push(b'0' + byte % 10);
    }
    out.extend_from_slice(b"]}");
    Ok(out)
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
    fn event_witness_bytes_match_serde_json() {
        let every: Vec<u8> = (0..=255).collect();
        let mut events = vec![
            Event::Frame(Vec::new()),
            Event::Control(Vec::new()),
            Event::Frame(every.clone()),
            Event::Control(every.iter().rev().copied().collect()),
            Event::Ready,
            Event::Effect {
                id: 7,
                success: false,
            },
            Event::Exit {
                code: 3,
                setup_error: true,
            },
            Event::Barrier,
        ];
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        for length in [1, 2, 9, 60, 1514, 65536] {
            let bytes = (0..length)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    x as u8
                })
                .collect();
            events.push(Event::Frame(bytes));
        }
        for event in &events {
            assert_eq!(
                event_json(event).unwrap(),
                serde_json::to_vec(event).unwrap()
            );
        }
    }
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
