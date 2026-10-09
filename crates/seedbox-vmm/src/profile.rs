//! Per-vendor settings of the guest branch counter, chosen from the host's
//! CPUID. The branch event decides what a "branch" is, so a run replays only on
//! the vendor that made it. The margins only size how close an overflow may
//! land to a stop; they affect speed, never where a stop lands.
//! Model tables, CPUID decoding and the raw events are shared with the CLI in
//! `seedbox_engine_core::branch_counter`.

use seedbox_engine_core::branch_counter::{CounterProfile, Cpu};
use std::sync::OnceLock;

pub struct CounterTuning {
    pub name: &'static str,
    /// Raw perf event (PERF_TYPE_RAW) counting retired conditional branches.
    pub branch_event: u64,
    /// Default distance (in branches) before a stop target at which we stop
    /// trusting the overflow interrupt and advance by breakpoints. It must
    /// exceed the PMU interrupt skid. An overshoot aborts the run, which can be
    /// rerun with a larger margin.
    pub margin: u64,
    /// Host-time allowance for overflow-interrupt skid used to size an
    /// adaptive margin.
    pub skid_ns: f64,
    /// Distance of the first, rate-measuring overflow before a hard stop.
    pub approach: u64,
    /// Floor of the adaptive margin.
    pub margin_min: u64,
    /// With undo-able stops, the largest final margin tried after rollbacks.
    pub undo_max: u64,
}

/// AMD Zen (families 17h-1Ah), tuned on the Ryzen 5950X. Needs the host's
/// SpeculativeLockMap workaround (MSR 0xC0011020 bit 54).
pub const ZEN: CounterTuning = CounterTuning {
    name: CounterProfile::Zen.name(),
    branch_event: CounterProfile::Zen.branch_event(),
    margin: 12288,
    // Observed tail ~10 us, with headroom.
    skid_ns: 18_000.0,
    approach: 65536,
    // Skids of 5-14k branches occur in dense code.
    margin_min: 16384,
    undo_max: 1 << 18,
};

/// Intel Nehalem through Comet Lake: BR_INST_RETIRED.CONDITIONAL. Unqualified:
/// margins copied from Zen, no exact-count check on hardware yet.
pub const INTEL_SKYLAKE: CounterTuning = CounterTuning {
    name: CounterProfile::IntelSkylake.name(),
    branch_event: CounterProfile::IntelSkylake.branch_event(),
    ..ZEN
};

/// Intel Ice Lake and later non-hybrid cores, where umask 0x01 became
/// COND_TAKEN and all conditional branches are umask 0x11. Unqualified, as above.
pub const INTEL_ICE_LAKE: CounterTuning = CounterTuning {
    name: CounterProfile::IntelIceLake.name(),
    branch_event: CounterProfile::IntelIceLake.branch_event(),
    ..ZEN
};

/// The profile for this host's CPU, detected once.
pub fn host() -> Result<&'static CounterTuning, String> {
    static HOST: OnceLock<Result<&'static CounterTuning, String>> = OnceLock::new();
    HOST.get_or_init(detect).clone()
}

fn detect() -> Result<&'static CounterTuning, String> {
    let cpu = Cpu::current();
    match CounterProfile::for_cpu(&cpu) {
        Some(CounterProfile::Zen) => Ok(&ZEN),
        Some(CounterProfile::IntelSkylake) => Ok(&INTEL_SKYLAKE),
        Some(CounterProfile::IntelIceLake) => Ok(&INTEL_ICE_LAKE),
        None => Err(format!(
            "no branch counter profile for {} family {:#x} model {:#x} hybrid {}",
            cpu.vendor, cpu.family, cpu.model, cpu.hybrid
        )),
    }
}
