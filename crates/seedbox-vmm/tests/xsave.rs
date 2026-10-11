//! Check the KVM restore semantics underlying canonical live-state evidence.
use kvm_bindings::{kvm_regs, kvm_segment, kvm_userspace_memory_region, KVM_MAX_CPUID_ENTRIES};
use kvm_ioctls::{Kvm, VcpuExit};

#[test]
#[cfg_attr(not(feature = "kvm-tests"), ignore = "requires Linux x86-64 KVM")]
fn ymm_presence_changes_storage_but_only_nonzero_values_change_guest_registers() {
    let kvm = Kvm::new().unwrap();
    let cpuid = kvm.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES).unwrap();
    let avx = cpuid
        .as_slice()
        .iter()
        .find(|e| e.function == 0xd && e.index == 2)
        .expect("AVX-capable KVM host");
    assert_eq!((avx.ebx, avx.eax), (576, 256));

    // Page alignment is required by KVM; keep the allocation alive until the
    // VM and vCPU have been dropped.
    #[repr(align(4096))]
    struct Ram([u8; 8192]);
    let mut ram = Box::new(Ram([0; 8192]));
    let vm = kvm.create_vm().unwrap();
    unsafe {
        vm.set_user_memory_region(kvm_userspace_memory_region {
            slot: 0,
            flags: 0,
            guest_phys_addr: 0,
            memory_size: ram.0.len() as u64,
            userspace_addr: ram.0.as_mut_ptr() as u64,
        })
    }
    .unwrap();
    let mut vcpu = vm.create_vcpu(0).unwrap();
    vcpu.set_cpuid2(&cpuid).unwrap();
    let mut s = vcpu.get_sregs().unwrap();
    s.cr0 = 0x31;
    s.cr4 = (1 << 9) | (1 << 10) | (1 << 18); // OSFXSR, OSXMMEXCPT, OSXSAVE
    let data = kvm_segment {
        base: 0,
        limit: u32::MAX,
        selector: 0x10,
        type_: 3,
        present: 1,
        db: 1,
        s: 1,
        g: 1,
        ..Default::default()
    };
    s.cs = kvm_segment {
        selector: 8,
        type_: 11,
        ..data
    };
    s.ds = data;
    s.es = data;
    s.ss = data;
    vcpu.set_sregs(&s).unwrap();
    let mut xcrs = vcpu.get_xcrs().unwrap();
    xcrs.xcrs[0].value = 7; // x87, SSE, AVX
    vcpu.set_xcrs(&xcrs).unwrap();

    // 32-bit guest: store each accessible YMM register to a separate buffer.
    let mut code = Vec::new();
    for register in 0..8u8 {
        code.extend([0xc5, 0xfe, 0x7f, 0x05 | (register << 3)]); // vmovdqu
        code.extend((4096 + u32::from(register) * 32).to_le_bytes());
    }
    code.push(0xf4); // hlt
    ram.0[..code.len()].copy_from_slice(&code);
    let initial = vcpu.get_xsave().unwrap();
    for (present, nonzero) in [(false, false), (true, false), (true, true)] {
        let mut state = kvm_bindings::kvm_xsave {
            region: initial.region,
            ..Default::default()
        };
        state.region[128] = 2 | if present { 4 } else { 0 };
        // Preserve nonzero lower halves; ignored upper bytes deliberately
        // contain garbage in the absent case to test actual restore behavior.
        state.region[40..104].fill(0x11223344);
        state.region[144..208].fill(if !present || nonzero { 0x55667788 } else { 0 });
        unsafe { vcpu.set_xsave(&state) }.unwrap();
        vcpu.set_regs(&kvm_regs {
            rip: 0,
            rflags: 2,
            ..Default::default()
        })
        .unwrap();
        assert!(matches!(vcpu.run().unwrap(), VcpuExit::Hlt));
        for register in 0..8 {
            let saved = &ram.0[4096 + register * 32..4096 + (register + 1) * 32];
            for &word in saved[..16].as_chunks::<4>().0 {
                assert_eq!(word, 0x11223344u32.to_le_bytes());
            }
            for &word in saved[16..].as_chunks::<4>().0 {
                assert_eq!(word, if nonzero { 0x55667788u32 } else { 0 }.to_le_bytes());
            }
        }
    }
}
