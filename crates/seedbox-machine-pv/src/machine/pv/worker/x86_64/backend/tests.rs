use super::*;

fn pmu_cpuid(version: u8) -> kvm_cpuid_entry2 {
    kvm_cpuid_entry2 {
        function: 0xa,
        eax: 0x0730_0400 | u32::from(version),
        ebx: 0x7f,
        edx: 0x603,
        ..Default::default()
    }
}

fn vendor_cpuid(vendor: &[u8; 12]) -> kvm_cpuid_entry2 {
    kvm_cpuid_entry2 {
        function: 0,
        eax: 0xa,
        ebx: u32::from_le_bytes(vendor[..4].try_into().unwrap()),
        edx: u32::from_le_bytes(vendor[4..8].try_into().unwrap()),
        ecx: u32::from_le_bytes(vendor[8..].try_into().unwrap()),
        ..Default::default()
    }
}

#[test]
fn pmu_disable_capability_requires_explicit_disable() {
    for vendor in [b"GenuineIntel", b"AuthenticAMD", b"UnknownCPU!!"] {
        for version in [0, 2, 5] {
            assert_eq!(
                pmu_needs_disable(
                    KVM_PMU_CAP_DISABLE as i32,
                    &[vendor_cpuid(vendor), pmu_cpuid(version)]
                ),
                Ok(true)
            );
        }
    }
    assert_eq!(pmu_needs_disable(KVM_PMU_CAP_DISABLE as i32, &[]), Ok(true));
}

#[test]
fn absent_pmu_disable_capability_accepts_only_intel_with_no_vpmu() {
    let intel = vendor_cpuid(b"GenuineIntel");
    // Non-version EAX bits do not advertise an Intel PMU when version is zero.
    assert_eq!(pmu_needs_disable(0, &[intel, pmu_cpuid(0)]), Ok(false));
    assert_eq!(pmu_needs_disable(2, &[intel, pmu_cpuid(0)]), Ok(false));
    for version in [1, 2, 5, 255] {
        assert_eq!(
            pmu_needs_disable(0, &[intel, pmu_cpuid(version)]),
            Err("PV KVM cannot disable the guest PMU".into())
        );
    }
    assert!(pmu_needs_disable(0, &[]).is_err());
    assert!(pmu_needs_disable(0, &[intel]).is_err());
    assert!(pmu_needs_disable(0, &[pmu_cpuid(0)]).is_err());
    assert!(pmu_needs_disable(0, &[intel, pmu_cpuid(0), pmu_cpuid(2)]).is_err());
    assert!(pmu_needs_disable(0, &[intel, vendor_cpuid(b"AuthenticAMD"), pmu_cpuid(0)]).is_err());
}

#[test]
fn absent_pmu_disable_capability_rejects_amd_legacy_counters_with_leaf_a_zero() {
    // KVM's AMD PMU can expose K7 counters/MSRs/RDPMC despite a zero leaf A.
    for vendor in [b"AuthenticAMD", b"UnknownCPU!!"] {
        assert_eq!(
            pmu_needs_disable(0, &[vendor_cpuid(vendor), pmu_cpuid(0)]),
            Err("PV KVM cannot disable the guest PMU".into())
        );
    }
}

#[test]
fn guest_pmu_cpuid_is_identical_with_disabled_or_absent_vpmu() {
    let disabled = guest_cpuid(CpuId::from_entries(&[pmu_cpuid(5)]).unwrap());
    let absent = guest_cpuid(CpuId::from_entries(&[pmu_cpuid(0)]).unwrap());
    assert_eq!(disabled, absent);
    let entry = &disabled.as_slice()[0];
    assert_eq!([entry.eax, entry.ebx, entry.ecx, entry.edx], [0; 4]);
}

/// Every feature the guest must not use to reach host time, entropy,
/// counters, waits or hypervisor interfaces is absent from guest CPUID.
#[test]
#[ignore = "requires /dev/kvm on an x86-64 PV host"]
fn pv_guest_cpuid_hides_host_sensitive_features() {
    let kvm = Kvm::new().unwrap();
    let cpuid = guest_cpuid(kvm.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES).unwrap());
    let mut seen = 0;
    for entry in cpuid.as_slice() {
        let registers = [entry.eax, entry.ebx, entry.ecx, entry.edx];
        let hidden: [u32; 4] = match (entry.function, entry.index) {
            // MONITOR, PDCM, x2APIC, TSC-deadline, RDRAND, hypervisor; APIC.
            (1, _) => [
                0,
                0,
                1 << 3 | 1 << 15 | 1 << 21 | 1 << 24 | 1 << 30 | 1 << 31,
                1 << 9,
            ],
            (6, _) => [0, 0, 1, 0],
            // TSC_ADJUST, HLE, RTM, RDSEED; WAITPKG, RDPID.
            (7, 0) => [0, 1 << 1 | 1 << 4 | 1 << 11 | 1 << 18, 1 << 5 | 1 << 22, 0],
            (0xa | 0xb | 0x15 | 0x16 | 0x1f | 0x8000_001e | 0x8000_0022, _) => [u32::MAX; 4],
            (0x4000_0000..=0x4fff_ffff, _) => [u32::MAX; 4],
            // SVM, perf counters, MONITORX.
            (0x8000_0001, _) => [0, 0, 1 << 2 | 1 << 23 | 1 << 24 | 1 << 28 | 1 << 29, 0],
            (0x8000_0007, _) => [u32::MAX, u32::MAX, u32::MAX, !(1 << 8)],
            (0x8000_0008, _) => [0, 1 << 4, 0xff, 0],
            _ => continue,
        };
        seen += 1;
        for (register, mask) in registers.iter().zip(hidden) {
            assert_eq!(
                register & mask,
                0,
                "{:#x}.{}: {registers:#x?}",
                entry.function,
                entry.index
            );
        }
    }
    assert!(seen >= 6, "expected the basic and extended feature leaves");
    let one = cpuid.as_slice().iter().find(|e| e.function == 1).unwrap();
    assert_eq!(one.ebx >> 24, 0, "initial APIC ID");
}

/// Host clocks the guest cannot read are never captured, restored or
/// witnessed; guest-writable architectural MSRs still are (bd-82e).
#[test]
fn host_clock_msrs_are_host_owned_and_architectural_msrs_are_not() {
    // TSC family, APIC base, TSC deadline, KVM PV range and Hyper-V synthetic
    // range, including VP_RUNTIME (0x40000010) and TIME_REF_COUNT (0x40000020).
    for index in [
        0x10,
        0x11,
        0x12,
        0x1b,
        0x3b,
        0x6e0,
        0x4b56_4d00,
        0x4b56_4dff,
        0x4000_0000,
        0x4000_0010,
        0x4000_0020,
        0x4000_00ff,
    ] {
        assert!(host_owned_msr(index), "{index:#x}");
    }
    // SYSENTER, PAT, EFER-adjacent syscall MSRs and FS/GS bases stay state.
    for index in [
        0x174,
        0x175,
        0x176,
        0x277,
        0xc000_0081,
        0xc000_0082,
        0xc000_0100,
        0xc000_0101,
        0xc000_0102,
        0x3fff_ffff,
        0x4000_0100,
    ] {
        assert!(!host_owned_msr(index), "{index:#x}");
    }
}

/// A stale exception or interrupt left by an earlier host-assisted delivery
/// is not deliverable state; an injected or pending one is witnessed whole.
#[test]
fn undeliverable_vcpu_event_leftovers_are_canonical() {
    let mut stale = kvm_vcpu_events::default();
    stale.exception.nr = 14;
    stale.exception.has_error_code = 1;
    stale.exception.error_code = 4;
    stale.exception_payload = 0xdead;
    stale.interrupt.nr = 48;
    stale.interrupt.soft = 1;
    stale.interrupt.shadow = 1;
    assert_eq!(evidence::exception_evidence(&stale), [0; 7]);
    assert_eq!(evidence::interrupt_evidence(&stale), [0, 0, 0, 1]);
    let mut live = stale;
    live.exception.pending = 1;
    live.interrupt.injected = 1;
    assert_eq!(
        evidence::exception_evidence(&live),
        [0, 14, 1, 1, 4, 0, 0xdead]
    );
    assert_eq!(evidence::interrupt_evidence(&live), [1, 48, 1, 1]);
    live.exception.pending = 0;
    live.exception.injected = 1;
    assert_eq!(evidence::exception_evidence(&live)[0], 1);
}

/// Two vCPUs created at different host times report identical MSR state:
/// no captured register advances with host time (bd-82e: Hyper-V counters).
#[test]
#[ignore = "requires /dev/kvm on an x86-64 PV host"]
fn pv_msr_state_is_independent_of_host_time() {
    let make = || {
        let layout = super::super::boot::RamLayout::native(128 << 20).unwrap();
        Native::create(
            Rc::new(Memory::ram(layout).unwrap()),
            Rc::new(Memory::new(16384).unwrap()),
        )
        .unwrap()
    };
    let first = make();
    let a = first.read_msrs().unwrap();
    // The completed first sample precedes a second real KVM vCPU creation.
    // That positive kernel-work boundary separates observations without a
    // hoped-for host-time delay; only deterministic MSR contents are compared.
    let second = make();
    let b = second.read_msrs().unwrap();
    assert!(a.iter().all(|(index, _)| !host_owned_msr(*index)));
    assert_eq!(a, b);
}
