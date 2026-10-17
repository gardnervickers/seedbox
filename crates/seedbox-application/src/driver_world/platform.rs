//! Which Driver backends a host runs, and with which guest ISA. Fresh
//! preparation, the CLI and native admission all derive the guest from here,
//! so no caller hardcodes x86-64. Pure: no device probe or environment lookup.
use super::DriverBackend;
use seedbox_spec::definition::Architecture;

/// An operating system and CPU pair, as in `std::env::consts`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Host {
    pub os: &'static str,
    pub arch: &'static str,
}

// TODO: Re-evaluate: PV needs a native hypervisor leaf (Linux KVM on x86-64,
// macOS HVF on aarch64); TCG emulates an x86-64 guest with the packaged QEMU on
// either host. Linux aarch64 PV stays out until its snapshot path is proven.
impl Host {
    /// The host this binary runs on.
    pub const fn current() -> Self {
        Self {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
        }
    }
    /// The guest ISA `backend` executes on this host, or `None` when the host
    /// has no Driver implementation for it.
    pub fn guest(self, backend: DriverBackend) -> Option<Architecture> {
        let pv = match (self.os, self.arch) {
            ("linux", "x86_64") => Some(Architecture::X86_64),
            ("macos", "aarch64") => Some(Architecture::Aarch64),
            _ => None,
        };
        match backend {
            DriverBackend::Pv => pv,
            DriverBackend::Tcg => pv.map(|_| Architecture::X86_64),
        }
    }
    /// The simulation-wide guest ISA a server on this host resolves for an
    /// omitted project architecture. Driver hosts follow the selected backend
    /// (TCG is always x86-64); other hosts keep their native ISA.
    pub fn project_architecture(self, backend: seedbox_spec::project::CpuBackend) -> Architecture {
        let backend = match backend {
            seedbox_spec::project::CpuBackend::Pv => DriverBackend::Pv,
            seedbox_spec::project::CpuBackend::Tcg => DriverBackend::Tcg,
        };
        self.guest(backend).unwrap_or(if self.arch == "aarch64" {
            Architecture::Aarch64
        } else {
            Architecture::X86_64
        })
    }
    /// Whether this host runs normal Driver executions at all.
    pub fn runs_driver(self) -> bool {
        self.guest(DriverBackend::Pv).is_some()
    }
}

/// Retained-descriptor and log spelling of a guest ISA.
pub fn architecture_name(architecture: Architecture) -> &'static str {
    match architecture {
        Architecture::X86_64 => "x86_64",
        Architecture::Aarch64 => "aarch64",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const LINUX_X86: Host = Host {
        os: "linux",
        arch: "x86_64",
    };
    const MAC_ARM: Host = Host {
        os: "macos",
        arch: "aarch64",
    };
    #[test]
    fn linux_x86_keeps_x86_pv_and_tcg() {
        assert_eq!(
            LINUX_X86.guest(DriverBackend::Pv),
            Some(Architecture::X86_64)
        );
        assert_eq!(
            LINUX_X86.guest(DriverBackend::Tcg),
            Some(Architecture::X86_64)
        );
        assert!(LINUX_X86.runs_driver());
    }
    #[test]
    fn apple_silicon_runs_native_arm_pv_and_emulated_x86_tcg() {
        assert_eq!(
            MAC_ARM.guest(DriverBackend::Pv),
            Some(Architecture::Aarch64)
        );
        assert_eq!(
            MAC_ARM.guest(DriverBackend::Tcg),
            Some(Architecture::X86_64)
        );
        assert!(MAC_ARM.runs_driver());
    }
    #[test]
    fn unproven_hosts_have_no_driver_guest() {
        for host in [
            Host {
                os: "linux",
                arch: "aarch64",
            },
            Host {
                os: "macos",
                arch: "x86_64",
            },
            Host {
                os: "windows",
                arch: "x86_64",
            },
        ] {
            assert_eq!(host.guest(DriverBackend::Pv), None, "{host:?}");
            assert_eq!(host.guest(DriverBackend::Tcg), None, "{host:?}");
            assert!(!host.runs_driver(), "{host:?}");
        }
    }
    #[test]
    fn project_architecture_follows_backend_on_driver_hosts_and_isa_elsewhere() {
        use seedbox_spec::project::CpuBackend;
        let linux_arm = Host {
            os: "linux",
            arch: "aarch64",
        };
        for (host, pv, tcg) in [
            (LINUX_X86, Architecture::X86_64, Architecture::X86_64),
            (MAC_ARM, Architecture::Aarch64, Architecture::X86_64),
            (linux_arm, Architecture::Aarch64, Architecture::Aarch64),
        ] {
            assert_eq!(host.project_architecture(CpuBackend::Pv), pv, "{host:?}");
            assert_eq!(host.project_architecture(CpuBackend::Tcg), tcg, "{host:?}");
        }
    }
    #[test]
    fn descriptor_spelling_matches_runtime_layout() {
        assert_eq!(architecture_name(Architecture::X86_64), "x86_64");
        assert_eq!(architecture_name(Architecture::Aarch64), "aarch64");
    }
}
