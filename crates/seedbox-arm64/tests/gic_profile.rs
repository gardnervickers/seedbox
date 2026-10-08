use seedbox_arm64::{HVF_GIC_NR_IRQS, KVM_GIC_NR_IRQS, TIMER_SPI, UART_SPI};

#[test]
fn kvm_count_is_kernel_valid_and_closest_to_hvf() {
    // Linux v6.8 vgic-kvm-device.c uses VGIC_MAX_RESERVED=1023, not the
    // 1024 in its comment. Exercise that rule even on non-KVM test hosts.
    let kernel_accepts = |n: u32| (64..=1023).contains(&n) && n & 31 == 0;
    assert!(!kernel_accepts(HVF_GIC_NR_IRQS));
    assert!(kernel_accepts(KVM_GIC_NR_IRQS));
    for n in (64..=1023).filter(|&n| kernel_accepts(n)) {
        assert!(KVM_GIC_NR_IRQS.abs_diff(HVF_GIC_NR_IRQS) <= n.abs_diff(HVF_GIC_NR_IRQS));
    }
    // The remaining architectural mismatch is intentional, not normalized away.
    assert_eq!(HVF_GIC_NR_IRQS / 32 - 1, 31);
    assert_eq!(KVM_GIC_NR_IRQS / 32 - 1, 30);
    for spi in [TIMER_SPI, UART_SPI] {
        assert!((32..KVM_GIC_NR_IRQS).contains(&spi));
    }
}
