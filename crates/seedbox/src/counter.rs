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
    /// Intel family 6. Untested: no Intel host has run Seedbox yet.
    Intel,
}

impl CounterProfile {
    /// The profile for this host's CPU.
    pub fn host() -> Result<Self> {
        let (vendor, family) = cpu()?;
        Self::for_cpu(&vendor, family).ok_or_else(|| {
            Error::Setup(format!(
                "no branch counter profile for {vendor} family {family:#x}; \
                 run on an AMD Zen host"
            ))
        })
    }

    fn for_cpu(vendor: &str, family: u32) -> Option<Self> {
        match (vendor, family) {
            ("AuthenticAMD", 0x17..=0x1a) => Some(Self::Zen),
            ("GenuineIntel", 6) => Some(Self::Intel),
            _ => None,
        }
    }

    /// Name recorded in run identity and reported by the engine.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Zen => "amd-zen",
            Self::Intel => "intel",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        [Self::Zen, Self::Intel]
            .into_iter()
            .find(|profile| profile.name() == name)
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
            Self::Intel => None,
        }
    }
}

/// CPUID vendor string and display family.
#[cfg(target_arch = "x86_64")]
// `__cpuid` is unsafe on older toolchains.
#[allow(unused_unsafe)]
fn cpu() -> Result<(String, u32)> {
    use std::arch::x86_64::__cpuid;
    let leaf0 = unsafe { __cpuid(0) };
    let mut vendor = Vec::with_capacity(12);
    for reg in [leaf0.ebx, leaf0.edx, leaf0.ecx] {
        vendor.extend_from_slice(&reg.to_le_bytes());
    }
    let eax = unsafe { __cpuid(1) }.eax;
    let base = (eax >> 8) & 0xf;
    let family = if base == 0xf {
        base + ((eax >> 20) & 0xff)
    } else {
        base
    };
    Ok((String::from_utf8_lossy(&vendor).into_owned(), family))
}

#[cfg(not(target_arch = "x86_64"))]
fn cpu() -> Result<(String, u32)> {
    Err(Error::Setup(
        "run and replay require the qualified Linux KVM host".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zen_and_intel_families_map_to_profiles() {
        for family in [0x17, 0x19, 0x1a] {
            assert_eq!(
                CounterProfile::for_cpu("AuthenticAMD", family),
                Some(CounterProfile::Zen)
            );
        }
        assert_eq!(
            CounterProfile::for_cpu("GenuineIntel", 6),
            Some(CounterProfile::Intel)
        );
        assert_eq!(CounterProfile::for_cpu("AuthenticAMD", 0x15), None);
        assert_eq!(CounterProfile::for_cpu("HygonGenuine", 0x18), None);
    }

    #[test]
    fn names_round_trip_and_only_zen_needs_a_workaround() {
        for profile in [CounterProfile::Zen, CounterProfile::Intel] {
            assert_eq!(CounterProfile::from_name(profile.name()), Some(profile));
        }
        assert_eq!(
            CounterProfile::Zen.workaround(),
            Some(MsrWorkaround {
                register: 0xc0011020,
                bit: 1 << 54
            })
        );
        assert_eq!(CounterProfile::Intel.workaround(), None);
    }
}
