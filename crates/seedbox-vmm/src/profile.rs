//! Per-vendor settings of the guest branch counter, chosen from the host's
//! CPUID. The branch event decides what a "branch" is, so a run replays only on
//! the vendor that made it. The margins only size how close an overflow may
//! land to a stop; they affect speed, never where a stop lands.
//! The host-side half (vendor detection and MSR workaround) is mirrored in
//! crates/seedbox/src/counter.rs; keep the two in sync.

use std::sync::OnceLock;

pub struct CounterProfile {
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
pub const ZEN: CounterProfile = CounterProfile {
    name: "amd-zen",
    // Retired conditional branches, rr's choice.
    branch_event: 0x5100d1,
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
pub const INTEL_SKYLAKE: CounterProfile = CounterProfile {
    name: "intel-0x5101c4",
    branch_event: 0x5101c4,
    ..ZEN
};

/// Intel Ice Lake and later non-hybrid cores, where umask 0x01 became
/// COND_TAKEN and all conditional branches are umask 0x11. Unqualified, as above.
pub const INTEL_ICE_LAKE: CounterProfile = CounterProfile {
    name: "intel-0x5111c4",
    branch_event: 0x5111c4,
    ..ZEN
};

/// Intel family-6 display models per event, from rr's model switch and event
/// table at rr-debugger/rr@858a9fb. Hybrid parts are refused: their E-cores
/// have their own PMU and events.
const SKYLAKE_MODELS: &[u32] = &[
    0x1a, 0x1e, 0x2e, 0x25, 0x2c, 0x2f, 0x2a, 0x2d, 0x3e, 0x3a, 0x3c, 0x3f, 0x45, 0x46,
    0x3d, 0x47, 0x4f, 0x56, 0x4e, 0x55, 0x5e, 0x8e, 0x9e, 0xa5, 0xa6,
];
const ICE_LAKE_MODELS: &[u32] = &[0x6a, 0x7e, 0x86, 0x8c, 0x8d, 0xa7, 0x8f, 0xcf, 0xad];

/// The profile for this host's CPU, detected once.
pub fn host() -> Result<&'static CounterProfile, String> {
    static HOST: OnceLock<Result<&'static CounterProfile, String>> = OnceLock::new();
    HOST.get_or_init(detect).clone()
}

fn detect() -> Result<&'static CounterProfile, String> {
    let (vendor, family, model, hybrid) = cpu();
    match (vendor.as_str(), family) {
        ("AuthenticAMD", 0x17..=0x1a) => Ok(&ZEN),
        ("GenuineIntel", 6) if !hybrid && SKYLAKE_MODELS.contains(&model) => Ok(&INTEL_SKYLAKE),
        ("GenuineIntel", 6) if !hybrid && ICE_LAKE_MODELS.contains(&model) => Ok(&INTEL_ICE_LAKE),
        _ => Err(format!(
            "no branch counter profile for {vendor} family {family:#x} model {model:#x} hybrid {hybrid}"
        )),
    }
}

/// CPUID vendor string, display family and model, and hybrid flag.
// `__cpuid` is unsafe on older toolchains.
#[allow(unused_unsafe)]
fn cpu() -> (String, u32, u32, bool) {
    use std::arch::x86_64::{__cpuid, __cpuid_count};
    let leaf0 = unsafe { __cpuid(0) };
    let mut vendor = Vec::with_capacity(12);
    for reg in [leaf0.ebx, leaf0.edx, leaf0.ecx] {
        vendor.extend_from_slice(&reg.to_le_bytes());
    }
    let eax = unsafe { __cpuid(1) }.eax;
    let base = (eax >> 8) & 0xf;
    let family = if base == 0xf { base + ((eax >> 20) & 0xff) } else { base };
    let model = if base == 0x6 || base == 0xf {
        ((eax >> 4) & 0xf) | ((eax >> 12) & 0xf0)
    } else {
        (eax >> 4) & 0xf
    };
    let hybrid = leaf0.eax >= 7 && unsafe { __cpuid_count(7, 0) }.edx & (1 << 15) != 0;
    (String::from_utf8_lossy(&vendor).into_owned(), family, model, hybrid)
}
