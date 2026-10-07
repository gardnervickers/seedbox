//! Experimental single-vCPU arm64 PV machine. No production x86 dependencies.
pub mod backend;
pub mod devices;
pub mod fdt;
pub mod machine;
pub mod memory;
pub mod pv;
pub mod timebase;
pub mod witness;

pub type Result<T> = std::result::Result<T, String>;

pub const RAM_BASE: u64 = 0x4000_0000;
pub const RAM_SIZE: usize = 128 << 20;
pub const DTB_ADDR: u64 = RAM_BASE + 0x0700_0000;
pub const INITRD_ADDR: u64 = RAM_BASE + 0x0600_0000;
pub const GIC_DIST: u64 = 0x0800_0000;
pub const GIC_REDIST: u64 = 0x080a_0000;
pub const UART_ADDR: u64 = 0x0900_0000;
pub const TIME_ADDR: u64 = 0x0a00_0000;
pub const EVENT_ADDR: u64 = 0x0a01_0000;
pub const TIMER_SPI: u32 = 40;
pub const UART_SPI: u32 = 33;
pub const PAGE_SIZE: usize = 0x4000; // also meets Apple Silicon's host alignment
pub const ABI: u64 = 1;
