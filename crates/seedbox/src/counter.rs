//! Which guest branch counter the host CPU provides. The engine picks the
//! matching perf event and tuning itself (crates/seedbox-vmm/src/profile.rs,
//! keep the two in sync); this side records the choice in a run's identity
//! and applies the profile's host workaround, if any.

use crate::{Error, Result};

/// A model-specific register bit that must be set on every leased core while
/// the engine runs, and restored afterwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsrWorkaround {
    pub register: u64,
    pub bit: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CounterProfile {
    /// AMD Zen (families 17h-1Ah); the only qualified profile.
    Zen,
    /// Intel Nehalem through Comet Lake: BR_INST_RETIRED.CONDITIONAL
    /// (0x5101c4). Unqualified.
    IntelSkylake,
    /// Intel Ice Lake and later non-hybrid cores, where umask 0x01 became
    /// COND_TAKEN and all conditional branches are umask 0x11 (0x5111c4).
    /// Unqualified.
    IntelIceLake,
}

/// Intel family-6 display models per event, from rr's model switch
/// (src/PerfCounters_x86.h) and event table (src/PerfCounters.cc) at
/// rr-debugger/rr@858a9fb. Hybrid parts (Alder Lake and later clients) are
/// refused: their E-cores have their own PMU and events.
const INTEL_SKYLAKE: &[u32] = &[
    0x1a, 0x1e, 0x2e, // Nehalem
    0x25, 0x2c, 0x2f, // Westmere
    0x2a, 0x2d, 0x3e, // Sandy Bridge (rr files Ivy Bridge-E 0x3e here)
    0x3a, // Ivy Bridge
    0x3c, 0x3f, 0x45, 0x46, // Haswell
    0x3d, 0x47, 0x4f, 0x56, // Broadwell
    0x4e, 0x55, 0x5e, // Skylake
    0x8e, 0x9e, // Kaby Lake
    0xa5, 0xa6, // Comet Lake
];
const INTEL_ICE_LAKE: &[u32] = &[
    0x6a, 0x7e, 0x86, // Ice Lake
    0x8c, 0x8d, // Tiger Lake
    0xa7, // Rocket Lake
    0x8f, // Sapphire Rapids
    0xcf, // Emerald Rapids
    0xad, // Granite Rapids
];

/// The host CPU as CPUID identifies it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Cpu {
    vendor: String,
    /// Display family and model (base plus extended fields).
    family: u32,
    model: u32,
    /// Mixed core types (CPUID.07H:EDX[15]).
    hybrid: bool,
}

impl CounterProfile {
    const ALL: [Self; 3] = [Self::Zen, Self::IntelSkylake, Self::IntelIceLake];

    /// The profile for this host's CPU.
    pub fn host() -> Result<Self> {
        let cpu = cpu()?;
        Self::for_cpu(&cpu).ok_or_else(|| {
            Error::Setup(format!(
                "no branch counter profile for {} family {:#x} model {:#x}{}; \
                 run on an AMD Zen host",
                cpu.vendor,
                cpu.family,
                cpu.model,
                if cpu.hybrid { " (hybrid)" } else { "" }
            ))
        })
    }

    fn for_cpu(cpu: &Cpu) -> Option<Self> {
        match (cpu.vendor.as_str(), cpu.family) {
            ("AuthenticAMD", 0x17..=0x1a) => Some(Self::Zen),
            ("GenuineIntel", 6) if cpu.hybrid => None,
            ("GenuineIntel", 6) if INTEL_SKYLAKE.contains(&cpu.model) => Some(Self::IntelSkylake),
            ("GenuineIntel", 6) if INTEL_ICE_LAKE.contains(&cpu.model) => Some(Self::IntelIceLake),
            _ => None,
        }
    }

    /// Name recorded in run identity and reported by the engine. Intel names
    /// carry the raw event, since its meaning differs between models.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Zen => "amd-zen",
            Self::IntelSkylake => "intel-0x5101c4",
            Self::IntelIceLake => "intel-0x5111c4",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|profile| profile.name() == name)
    }

    /// Whether a host with this profile has passed qualification. Results
    /// from an unqualified profile are unverified.
    pub const fn qualified(self) -> bool {
        matches!(self, Self::Zen)
    }

    /// Host change the counter needs to be exact on this CPU.
    pub const fn workaround(self) -> Option<MsrWorkaround> {
        match self {
            // SpeculativeLockMap off (rr's workaround). Without it, counts
            // drift on lock-heavy code.
            Self::Zen => Some(MsrWorkaround {
                register: 0xc0011020,
                bit: 1 << 54,
            }),
            Self::IntelSkylake | Self::IntelIceLake => None,
        }
    }
}

/// CPUID vendor, display family and model, and hybrid flag.
#[cfg(target_arch = "x86_64")]
// `__cpuid` is unsafe on older toolchains.
#[allow(unused_unsafe)]
fn cpu() -> Result<Cpu> {
    use std::arch::x86_64::{__cpuid, __cpuid_count};
    let leaf0 = unsafe { __cpuid(0) };
    let mut vendor = Vec::with_capacity(12);
    for reg in [leaf0.ebx, leaf0.edx, leaf0.ecx] {
        vendor.extend_from_slice(&reg.to_le_bytes());
    }
    let eax = unsafe { __cpuid(1) }.eax;
    let base_family = (eax >> 8) & 0xf;
    let base_model = (eax >> 4) & 0xf;
    let family = if base_family == 0xf {
        base_family + ((eax >> 20) & 0xff)
    } else {
        base_family
    };
    let model = if base_family == 0x6 || base_family == 0xf {
        base_model | ((eax >> 12) & 0xf0)
    } else {
        base_model
    };
    let hybrid = leaf0.eax >= 7 && unsafe { __cpuid_count(7, 0) }.edx & (1 << 15) != 0;
    Ok(Cpu {
        vendor: String::from_utf8_lossy(&vendor).into_owned(),
        family,
        model,
        hybrid,
    })
}

#[cfg(not(target_arch = "x86_64"))]
fn cpu() -> Result<Cpu> {
    Err(Error::Setup(
        "run and replay require the qualified Linux KVM host".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cpu(vendor: &str, family: u32, model: u32, hybrid: bool) -> Cpu {
        Cpu {
            vendor: vendor.into(),
            family,
            model,
            hybrid,
        }
    }

    #[test]
    fn profiles_follow_vendor_family_and_model() {
        for family in [0x17, 0x19, 0x1a] {
            assert_eq!(
                CounterProfile::for_cpu(&cpu("AuthenticAMD", family, 0x21, false)),
                Some(CounterProfile::Zen)
            );
        }
        // Skylake and Ice Lake server count all conditional branches with
        // different umasks.
        assert_eq!(
            CounterProfile::for_cpu(&cpu("GenuineIntel", 6, 0x55, false)),
            Some(CounterProfile::IntelSkylake)
        );
        assert_eq!(
            CounterProfile::for_cpu(&cpu("GenuineIntel", 6, 0x6a, false)),
            Some(CounterProfile::IntelIceLake)
        );
        for unsupported in [
            cpu("AuthenticAMD", 0x15, 0x02, false),
            cpu("HygonGenuine", 0x18, 0x00, false),
            // Alder Lake: hybrid.
            cpu("GenuineIntel", 6, 0x97, true),
            // Unlisted model, e.g. an Atom.
            cpu("GenuineIntel", 6, 0x5c, false),
        ] {
            assert_eq!(CounterProfile::for_cpu(&unsupported), None);
        }
    }

    #[test]
    fn names_round_trip_and_only_zen_is_qualified_or_needs_a_workaround() {
        for profile in CounterProfile::ALL {
            assert_eq!(CounterProfile::from_name(profile.name()), Some(profile));
            assert_eq!(profile.qualified(), profile == CounterProfile::Zen);
        }
        assert_eq!(
            CounterProfile::Zen.workaround(),
            Some(MsrWorkaround {
                register: 0xc0011020,
                bit: 1 << 54
            })
        );
        assert_eq!(CounterProfile::IntelSkylake.workaround(), None);
        assert_eq!(CounterProfile::IntelIceLake.workaround(), None);
    }
}
