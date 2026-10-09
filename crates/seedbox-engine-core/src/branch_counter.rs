//! Which guest branch counter a host CPU provides, decoded from its CPUID.
//! The branch event decides what a "branch" is, so a counter run replays only
//! on a CPU with the same profile. Executing CPUID stays with each caller; this
//! module holds the model tables, the decoder and the per-profile constants.

use serde::{Deserialize, Serialize};

/// A model-specific register bit that must be set on every leased core while
/// the engine runs, and restored afterwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsrWorkaround {
    pub register: u64,
    pub bit: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CounterProfile {
    /// AMD Zen (families 17h-1Ah).
    #[serde(rename = "amd-zen")]
    Zen,
    /// Intel Nehalem through Comet Lake: BR_INST_RETIRED.CONDITIONAL
    /// (0x5101c4).
    #[serde(rename = "intel-0x5101c4")]
    IntelSkylake,
    /// Intel Ice Lake and later non-hybrid cores, where umask 0x01 became
    /// COND_TAKEN and all conditional branches are umask 0x11 (0x5111c4).
    #[serde(rename = "intel-0x5111c4")]
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
pub struct Cpu {
    pub vendor: String,
    /// Display family and model (base plus extended fields).
    pub family: u32,
    pub model: u32,
    /// Mixed core types (CPUID.07H:EDX[15]).
    pub hybrid: bool,
}

/// Raw CPUID registers the decoder reads, as `(eax, ebx, ecx, edx)`.
pub type CpuidRegisters = (u32, u32, u32, u32);

impl Cpu {
    /// Decode leaf 0, leaf 1 and, when leaf 0 reports it, leaf 7 subleaf 0.
    /// `leaf7` is called only when the maximum basic leaf is at least 7.
    pub fn decode(
        leaf0: CpuidRegisters,
        leaf1: CpuidRegisters,
        leaf7: impl FnOnce() -> CpuidRegisters,
    ) -> Self {
        let (max_leaf, ebx, ecx, edx) = leaf0;
        let mut vendor = Vec::with_capacity(12);
        for reg in [ebx, edx, ecx] {
            vendor.extend_from_slice(&reg.to_le_bytes());
        }
        let eax = leaf1.0;
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
        let hybrid = max_leaf >= 7 && leaf7().3 & (1 << 15) != 0;
        Self {
            vendor: String::from_utf8_lossy(&vendor).into_owned(),
            family,
            model,
            hybrid,
        }
    }

    /// Execute CPUID on the calling core and decode it.
    #[cfg(target_arch = "x86_64")]
    // `__cpuid` is unsafe on older toolchains.
    #[allow(unused_unsafe)]
    pub fn current() -> Self {
        use std::arch::x86_64::{__cpuid, __cpuid_count, CpuidResult};
        let registers = |r: CpuidResult| (r.eax, r.ebx, r.ecx, r.edx);
        // SAFETY: CPUID is available on every x86-64 CPU and has no side effects.
        let leaf0 = registers(unsafe { __cpuid(0) });
        let leaf1 = registers(unsafe { __cpuid(1) });
        Self::decode(leaf0, leaf1, || registers(unsafe { __cpuid_count(7, 0) }))
    }
}

impl CounterProfile {
    pub const ALL: [Self; 3] = [Self::Zen, Self::IntelSkylake, Self::IntelIceLake];

    /// The profile for a CPU, or `None` when no supported branch event exists.
    pub fn for_cpu(cpu: &Cpu) -> Option<Self> {
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

    /// Raw perf event (PERF_TYPE_RAW) counting retired conditional branches.
    pub const fn branch_event(self) -> u64 {
        match self {
            // Retired conditional branches, rr's choice.
            Self::Zen => 0x5100d1,
            Self::IntelSkylake => 0x5101c4,
            Self::IntelIceLake => 0x5111c4,
        }
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
    fn cpuid_decoding_combines_extended_family_and_model() {
        let vendor = |s: &[u8; 12]| {
            let word = |i: usize| u32::from_le_bytes(s[i..i + 4].try_into().unwrap());
            (word(0), word(8), word(4))
        };
        // Zen 3 (family 19h model 21h): base family 0xf plus extended 0xa.
        let (ebx, ecx, edx) = vendor(b"AuthenticAMD");
        let zen = Cpu::decode((0x10, ebx, ecx, edx), (0x00a2_0f10, 0, 0, 0), || {
            (0, 0, 0, 0)
        });
        assert_eq!(zen, cpu("AuthenticAMD", 0x19, 0x21, false));
        // Alder Lake (family 6 model 97h) sets the hybrid bit in leaf 7.
        let (ebx, ecx, edx) = vendor(b"GenuineIntel");
        let alder = Cpu::decode((0x20, ebx, ecx, edx), (0x0009_0672, 0, 0, 0), || {
            (0, 0, 0, 1 << 15)
        });
        assert_eq!(alder, cpu("GenuineIntel", 6, 0x97, true));
        // Leaf 7 is never read when leaf 0 does not report it.
        let old = Cpu::decode((6, ebx, ecx, edx), (0x0001_06a5, 0, 0, 0), || {
            unreachable!("leaf 7 is above the maximum basic leaf")
        });
        assert_eq!(old, cpu("GenuineIntel", 6, 0x1a, false));
    }

    #[test]
    fn names_round_trip_and_only_zen_needs_a_workaround() {
        for profile in CounterProfile::ALL {
            assert_eq!(CounterProfile::from_name(profile.name()), Some(profile));
            assert_eq!(
                serde_json::to_value(profile).unwrap(),
                serde_json::json!(profile.name())
            );
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
