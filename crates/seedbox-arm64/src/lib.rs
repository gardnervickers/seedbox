//! ARM64 PV machines and a native application world. Each machine has one vCPU;
//! portable network, control, fault and disk policies are shared with x86.
pub mod application;
pub mod backend;
pub mod boot;
pub mod cli;
pub mod devices;
pub mod diagnostics;
pub mod fdt;
pub mod machine;
pub mod memory;
pub mod pv;
pub mod timebase;
pub mod witness;

// Reuse the existing disk/crash policy and random streams verbatim. Formatting
// this ARM crate must not rewrite the separately qualified x86 engine sources.
#[rustfmt::skip]
#[path = "../../seedbox-vmm/src/disk.rs"]
pub mod disk;
#[rustfmt::skip]
#[path = "../../seedbox-vmm/src/rng.rs"]
pub(crate) mod rng;

// The portable world policies are shared verbatim with the existing engine.
#[rustfmt::skip]
#[path = "../../seedbox-vmm/src/links.rs"]
#[allow(dead_code)] // Shared module also exposes APIs used only by the x86 coordinator.
pub(crate) mod links;
#[rustfmt::skip]
#[path = "../../seedbox-vmm/src/faults.rs"]
#[allow(dead_code)] // Preserve the qualified shared source verbatim.
pub(crate) mod faults;
#[rustfmt::skip]
#[path = "../../seedbox-vmm/src/control.rs"]
#[allow(dead_code)] // ARM virtio framing does not use the legacy UART write API.
pub(crate) mod control;
#[rustfmt::skip]
#[path = "../../seedbox-vmm/src/network_counters.rs"]
pub(crate) mod network_counters;
#[rustfmt::skip]
#[path = "../../seedbox-vmm/src/host_control.rs"]
pub(crate) mod host_control;

pub type Result<T> = std::result::Result<T, String>;

pub const RAM_BASE: u64 = 0x4000_0000;
pub const RAM_SIZE: usize = 128 << 20;
pub const DTB_ADDR: u64 = RAM_BASE + 0x0700_0000;
pub const INITRD_ADDR: u64 = RAM_BASE + 0x0600_0000;
// HVF advertises ITLinesNumber=31: 32 private IDs plus 988 usable SPIs;
// IDs 1020..1023 are reserved. HVF has no interrupt-count setter.
pub const HVF_GIC_NR_IRQS: u32 = 1024;
// Linux 6.8 vgic_set_common_attr caps NR_IRQS at VGIC_MAX_RESERVED (1023),
// despite its "at most 1024" comment, and requires a multiple of 32.
// 992 is the closest accepted total to HVF: 32 private IDs + 960 SPIs,
// ITLinesNumber=30. Keep this remaining guest-visible difference explicit.
pub const KVM_GIC_NR_IRQS: u32 = 992;
pub const GIC_DIST: u64 = 0x0800_0000;
pub const GIC_REDIST: u64 = 0x080a_0000;
pub const UART_ADDR: u64 = 0x0900_0000;
pub const TIME_ADDR: u64 = 0x0a00_0000;
pub const EVENT_ADDR: u64 = 0x0a01_0000;
pub const TIMER_SPI: u32 = 40;
pub const UART_SPI: u32 = 33;
pub const PAGE_SIZE: usize = 0x4000; // also meets Apple Silicon's host alignment
pub const ABI: u64 = 2;
