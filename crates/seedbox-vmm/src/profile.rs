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
    /// Floor of the adaptive margin. The syscall-exit window before a soft
    /// stop must outlast the skid.
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

/// Intel family 6. UNTESTED: event from rr (BR_INST_RETIRED.CONDITIONAL) and
/// margins copied from Zen; needs an exact-count check and skid tuning on real
/// hardware before any qualification.
pub const INTEL: CounterProfile = CounterProfile {
    name: "intel",
    branch_event: 0x5101c4,
    ..ZEN
};

/// The profile for this host's CPU, detected once.
pub fn host() -> Result<&'static CounterProfile, String> {
    static HOST: OnceLock<Result<&'static CounterProfile, String>> = OnceLock::new();
    HOST.get_or_init(detect).clone()
}

fn detect() -> Result<&'static CounterProfile, String> {
    let (vendor, family) = cpu();
    match (vendor.as_str(), family) {
        ("AuthenticAMD", 0x17..=0x1a) => Ok(&ZEN),
        ("GenuineIntel", 6) => Ok(&INTEL),
        _ => Err(format!("no branch counter profile for {vendor} family {family:#x}")),
    }
}

/// CPUID vendor string and display family.
// `__cpuid` is unsafe on older toolchains.
#[allow(unused_unsafe)]
fn cpu() -> (String, u32) {
    use std::arch::x86_64::__cpuid;
    let leaf0 = unsafe { __cpuid(0) };
    let mut vendor = Vec::with_capacity(12);
    for reg in [leaf0.ebx, leaf0.edx, leaf0.ecx] {
        vendor.extend_from_slice(&reg.to_le_bytes());
    }
    let eax = unsafe { __cpuid(1) }.eax;
    let base = (eax >> 8) & 0xf;
    let family = if base == 0xf { base + ((eax >> 20) & 0xff) } else { base };
    (String::from_utf8_lossy(&vendor).into_owned(), family)
}
