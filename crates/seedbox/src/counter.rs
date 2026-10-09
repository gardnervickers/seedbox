//! Which guest branch counter the host CPU provides. The profile tables and
//! CPUID decoder are shared with the engine (`seedbox_engine_core::branch_counter`);
//! this side records the choice in a run's identity and applies the profile's
//! host workaround, if any.

use crate::{Error, Result};
pub use seedbox_engine_core::branch_counter::{CounterProfile, MsrWorkaround};

/// The profile for this host's CPU.
#[cfg(target_arch = "x86_64")]
pub fn host() -> Result<CounterProfile> {
    let cpu = seedbox_engine_core::branch_counter::Cpu::current();
    CounterProfile::for_cpu(&cpu).ok_or_else(|| {
        Error::Setup(format!(
            "no branch counter profile for {} family {:#x} model {:#x}{}; \
             use a host with a supported branch event or --timebase pv",
            cpu.vendor,
            cpu.family,
            cpu.model,
            if cpu.hybrid { " (hybrid)" } else { "" }
        ))
    })
}

#[cfg(not(target_arch = "x86_64"))]
pub fn host() -> Result<CounterProfile> {
    Err(Error::Setup(
        "branch counter detection requires an x86-64 host".into(),
    ))
}
