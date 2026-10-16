use super::*;
/// Every feature the guest must not use to reach host time, entropy,
/// counters, waits or hypervisor interfaces is absent from guest CPUID.
#[test]
#[ignore = "requires /dev/kvm on an x86-64 PV host"]
fn pv_guest_cpuid_hides_host_sensitive_features() {
    let kvm = Kvm::new().unwrap();
    let cpuid = guest_cpuid(&kvm).unwrap();
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
