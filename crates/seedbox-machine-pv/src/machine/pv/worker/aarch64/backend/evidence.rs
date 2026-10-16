//! Stopped ARM register projection, NOT a snapshot/migration representation.
//! All reads are non-destructive: never read ICC_IAR, run a CPU, acknowledge an
//! interrupt, or consume host counter time. Names/order are this backend's ABI.
use crate::machine::pv::worker::{evidence::part, Result};
use serde::Serialize;
use std::collections::BTreeMap;

/// x0..x30, SP_EL1, PC, PSTATE, SP_EL0; 32 little-endian SIMD registers.
/// System registers are keyed by architectural encoding (not host ABI ID).
/// KVM additionally exposes the five saved PSRs in its core bank. HVF exposes
/// only SPSR_EL1, already in `system`; its `saved_psrs` is empty.
#[derive(Debug, Serialize)]
pub(in crate::machine::pv::worker) struct Cpu {
    pub(in crate::machine::pv::worker) core: Vec<u64>,
    pub(in crate::machine::pv::worker) system: BTreeMap<u16, u64>,
    pub(in crate::machine::pv::worker) simd: Vec<[u8; 16]>,
    pub(in crate::machine::pv::worker) saved_psrs: Vec<u64>,
}

/// Translate the public HVF register numbering without reusing or changing the
/// persisted Architecture format. In particular native 31 is PC, not SP_EL1.
#[cfg(any(target_os = "macos", test))]
pub(in crate::machine::pv::worker) fn hvf_cpu(
    native_core: Vec<u64>,
    mut system: BTreeMap<u16, u64>,
    simd: Vec<[u8; 16]>,
) -> Result<Cpu> {
    if native_core.len() != 35 || simd.len() != 32 {
        return Err("PV HVF evidence register shape".into());
    }
    let mut core = native_core[..31].to_vec();
    core.extend([
        *system
            .get(&0xe208)
            .ok_or("PV HVF evidence SP_EL1 missing")?,
        native_core[31],
        native_core[34],
        *system
            .get(&0xc208)
            .ok_or("PV HVF evidence SP_EL0 missing")?,
    ]);
    system.insert(0xda20, native_core[32]); // FPCR
    system.insert(0xda21, native_core[33]); // FPSR
    Ok(Cpu {
        core,
        system,
        simd,
        saved_psrs: Vec::new(),
    })
}

/// GICv3 without ITS/LPIs or EL2 (the actual PV board). Capture the readable
/// non-aliased state, including every implemented SPI, not just asserted ones.
/// KVM's ISPENDR getter returns the latch separately from input line levels;
/// retain both. HVF returns architectural pending state; input SPI levels are
/// also retained by Machine's actual `devices.asserted` projection.
#[derive(Debug, Serialize)]
pub(in crate::machine::pv::worker) struct Gic {
    distributor: BTreeMap<u32, u64>,
    redistributor: BTreeMap<u32, u64>,
    pub(in crate::machine::pv::worker) icc: BTreeMap<u16, u64>,
    pub(in crate::machine::pv::worker) line_levels: Option<BTreeMap<u32, u32>>,
}

pub(in crate::machine::pv::worker) const ICC: [u16; 10] = [
    0xc230, 0xc643, 0xc644, 0xc648, 0xc65b, 0xc663, 0xc664, 0xc665, 0xc666, 0xc667,
];

/// `wide` identifies the two 64-bit architectural register families. KVM must
/// read their two u32 halves; HVF exposes them as a single u64. No write alias
/// (ICENABLER/ICPENDR/ICACTIVER) is needed and no side-effecting register is read.
pub(in crate::machine::pv::worker) fn gic(
    mut dist: impl FnMut(u32, bool) -> Result<u64>,
    mut redist: impl FnMut(u32, bool) -> Result<u64>,
) -> Result<Gic> {
    let typer = dist(4, false)?;
    let count = interrupt_count(typer)?;
    let control = dist(0, false)?;
    // The actual PV GIC is single-security-state. With GICD_CTLR.DS=1,
    // GICD_IGRPMODR<n> and GICR_IGRPMODR0 are RAZ/WI, so there is no omitted
    // mutable modifier bank. Verify the stopped register, never assume DS or
    // manufacture zero modifier state for a different security configuration.
    if control & (1 << 6) == 0 {
        return Err(
            "PV ARM evidence: GICD_CTLR.DS is clear; group modifiers cannot be omitted".into(),
        );
    }
    let mut distributor = BTreeMap::from([(0, control), (4, typer)]);
    // Distributor INTIDs 0..31 are RAZ/WI in GICv3; their real state is in GICR.
    for (base, step, irqs) in [
        (0x80, 4, 32),  // group
        (0x100, 4, 32), // enable
        (0x200, 4, 32), // pending
        (0x300, 4, 32), // active
        (0x400, 4, 4),  // priority
        (0xc00, 4, 16), // configuration
    ] {
        for irq in (32..count).step_by(irqs) {
            let offset = base + (irq / irqs as u32) * step;
            distributor.insert(offset, dist(offset, false)?);
        }
    }
    for irq in 32..count {
        let offset = 0x6000 + irq * 8;
        distributor.insert(offset, dist(offset, true)?);
    }
    // The public HVF GIC API exposes these redistributor fields. CTLR/WAKER
    // are not exposed there; neither LPIs nor redistributor sleep are supported
    // by this board. KVM adds its readable CTLR/WAKER fields at the call site.
    let mut redistributor = BTreeMap::new();
    for offset in [
        8, 0xffe8, 0x10080, 0x10100, 0x10200, 0x10300, 0x10c00, 0x10c04,
    ]
    .into_iter()
    .chain((0x10400..0x10420).step_by(4))
    {
        redistributor.insert(offset, redist(offset, offset == 8)?);
    }
    Ok(Gic {
        distributor,
        redistributor,
        icc: BTreeMap::new(),
        line_levels: None,
    })
}

pub(in crate::machine::pv::worker) fn interrupt_count(typer: u64) -> Result<u32> {
    // The last four IDs are architectural special IDs, never routed SPIs.
    let count = (((typer & 31) as u32 + 1) * 32).min(1020);
    if typer & (1 << 17) != 0 {
        return Err("PV ARM evidence: GIC LPIs outside the PV board profile".into());
    }
    Ok(count)
}

/// Same invariant already enforced by ARM Machine::maintenance after native
/// stops. This getter independently verifies the actual stopped ENABLE bit
/// before excluding counter-derived fields; it is not a new execution policy.
#[cfg(any(target_os = "linux", test))]
pub(in crate::machine::pv::worker) fn virtual_timer_disabled(control: u64) -> Result<()> {
    if control & 1 != 0 {
        return Err("PV guest enabled host counter/timer".into());
    }
    Ok(())
}

/// KVM's ABI swaps TIMER_CVAL (df02, retained) and TIMER_CNT (df1a, excluded).
/// The physical counter and both TVALs are also host-counter-derived. Stored
/// physical/virtual deadlines and control registers remain actual CPU evidence.
#[cfg(any(target_os = "linux", test))]
pub(in crate::machine::pv::worker) fn kvm_system_field(register: u16) -> bool {
    !matches!(register, 0xdf01 | 0xdf10 | 0xdf18 | 0xdf1a)
}

impl Gic {
    #[cfg(target_os = "linux")]
    pub(in crate::machine::pv::worker) fn kvm_redist(&mut self, control: u64, waker: u64) {
        self.redistributor.insert(0, control);
        self.redistributor.insert(0x14, waker);
    }
}

pub(in crate::machine::pv::worker) fn parts(cpu: &Cpu, gic: &Gic) -> Result<Vec<(String, String)>> {
    if cpu.core.len() != 35 || cpu.simd.len() != 32 {
        return Err("PV ARM evidence register shape".into());
    }
    Ok(vec![part("cpu", cpu)?, part("gic", gic)?])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn sample_cpu() -> Cpu {
        Cpu {
            core: (0..35).collect(),
            system: BTreeMap::from([(0xc080, 7), (0xdf19, 0)]),
            simd: vec![[3; 16]; 32],
            saved_psrs: vec![],
        }
    }
    fn registers() -> Gic {
        gic(
            |offset, _| {
                Ok(match offset {
                    0 => 1 << 6,
                    4 => 1,
                    _ => u64::from(offset),
                })
            },
            |offset, _| Ok(u64::from(offset)),
        )
        .unwrap()
    }
    #[test]
    fn group_modifier_omission_requires_actual_single_security_state() {
        let calls = RefCell::new(Vec::new());
        let error = gic(
            |offset, _| {
                calls.borrow_mut().push(offset);
                Ok(if offset == 4 { 1 } else { 0 })
            },
            |_, _| panic!("no redistributor reads after invalid actual DS"),
        )
        .unwrap_err();
        assert!(error.contains("GICD_CTLR.DS is clear"));
        assert_eq!(calls.into_inner(), [4, 0]);
        // Preserve the actual CTLR bits in the hash, not an assumed DS constant.
        let actual = gic(
            |offset, _| {
                Ok(match offset {
                    0 => 0x47,
                    4 => 1,
                    _ => 0,
                })
            },
            |_, _| Ok(0),
        )
        .unwrap();
        assert_eq!(actual.distributor[&0], 0x47);
        assert!(!actual
            .distributor
            .keys()
            .any(|offset| (0xd00..0xd80).contains(offset)));
        assert!(!actual.redistributor.contains_key(&0x10d00));
    }
    #[test]
    fn counter_exclusions_require_existing_disabled_virtual_timer_invariant() {
        assert!(virtual_timer_disabled(0).is_ok());
        assert!(virtual_timer_disabled(2).is_ok()); // IMASK alone is disabled.
        assert!(virtual_timer_disabled(1)
            .unwrap_err()
            .contains("enabled host counter/timer"));
        assert!(virtual_timer_disabled(3).is_err()); // Masking does not disable it.
        for register in [0xdf01, 0xdf10, 0xdf18, 0xdf1a] {
            assert!(!kvm_system_field(register));
        }
        for register in [0xdf02, 0xdf11, 0xdf12, 0xdf19, 0xc080] {
            assert!(kvm_system_field(register)); // CVAL swap and stored controls.
        }
    }
    #[test]
    fn hvf_native_numbers_project_architectural_registers_not_snapshot_positions() {
        let c = hvf_cpu(
            (0..35).collect(),
            BTreeMap::from([(0xe208, 100), (0xc208, 200)]),
            vec![[0; 16]; 32],
        )
        .unwrap();
        assert_eq!(&c.core[31..], [100, 31, 34, 200]);
        assert_eq!(c.system[&0xda20], 32);
        assert_eq!(c.system[&0xda21], 33);
        assert!(c.saved_psrs.is_empty());
        assert!(hvf_cpu(vec![0; 34], BTreeMap::new(), vec![[0; 16]; 32]).is_err());
        assert!(hvf_cpu(vec![0; 35], BTreeMap::new(), vec![[0; 16]; 32]).is_err());
    }
    #[test]
    fn all_spi_banks_and_redistributor_are_read_once_without_aliases() {
        let calls = RefCell::new(Vec::new());
        let state = gic(
            |offset, wide| {
                calls.borrow_mut().push((0, offset, wide));
                Ok(match offset {
                    0 => 1 << 6,
                    4 => 1,
                    _ => 0,
                })
            },
            |offset, wide| {
                calls.borrow_mut().push((1, offset, wide));
                Ok(0)
            },
        )
        .unwrap();
        let calls = calls.into_inner();
        assert_eq!(calls.len(), 2 + 4 + 8 + 2 + 32 + 16);
        assert_eq!(state.distributor.len(), 48);
        assert_eq!(state.redistributor.len(), 16);
        assert_eq!(calls.iter().filter(|c| c.2).count(), 33);
        for offset in [0x84, 0x104, 0x204, 0x304, 0x420, 0xc08, 0x6100, 0x61f8] {
            assert!(calls.contains(&(0, offset, offset >= 0x6000)));
        }
        assert!(!calls
            .iter()
            .any(|&(bank, offset, _)| bank == 0 && (0x280..0x300).contains(&offset)));
        let mut unique = calls.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), calls.len());
    }
    #[test]
    fn projection_is_ordered_and_each_actual_bank_changes_its_part() {
        let mut cpu = sample_cpu();
        let mut gic = registers();
        let first = parts(&cpu, &gic).unwrap();
        assert_eq!(
            first.iter().map(|p| p.0.as_str()).collect::<Vec<_>>(),
            ["cpu", "gic"]
        );
        assert_eq!(first, parts(&cpu, &gic).unwrap());
        assert_eq!(
            first[0].1,
            "63cce287bb0bfb2b764c64912d0049974cf98ed1aa0a8a17b7d8a770a0f166b4"
        );
        assert_eq!(
            first[1].1,
            "bca66436fd3e5ece51b0cac298f06a986ce60214650d35a6477fc892131c779b"
        );
        // Insertion order cannot change the canonical BTreeMap projection.
        cpu.system = [(0xdf19, 0), (0xc080, 7)].into_iter().collect();
        assert_eq!(first, parts(&cpu, &gic).unwrap());
        for bank in 0..4 {
            let mut c = sample_cpu();
            match bank {
                0 => c.core[34] ^= 1,
                1 => {
                    c.system.insert(0xdf19, 1);
                }
                2 => c.simd[31][15] ^= 1,
                _ => c.saved_psrs.push(1),
            }
            let changed = parts(&c, &gic).unwrap();
            assert_ne!(changed[0], first[0]);
            assert_eq!(changed[1], first[1]);
        }
        for bank in 0..4 {
            gic = registers();
            match bank {
                0 => {
                    gic.distributor.insert(0x204, 1);
                }
                1 => {
                    gic.redistributor.insert(0x10200, 1);
                }
                2 => {
                    gic.icc.insert(0xc230, 1);
                }
                _ => {
                    gic.line_levels = Some(BTreeMap::from([(32, 1)]));
                }
            }
            let changed = parts(&cpu, &gic).unwrap();
            assert_eq!(changed[0], first[0]);
            assert_ne!(changed[1], first[1]);
        }
    }
    #[test]
    fn projection_is_fixed_json_not_architecture_or_migration_serialization() {
        let data = serde_json::to_value(registers()).unwrap();
        assert_eq!(data["distributor"]["24832"], 24832); // IROUTER32
        assert_eq!(data["redistributor"]["66048"], 66048); // ISPENDR0
        assert!(data.get("gic").is_none()); // no opaque snapshot field
        assert!(data.get("host").is_none());
        assert_eq!(serde_json::to_value(sample_cpu()).unwrap()["core"][34], 34);
        // The CPU interface inventory never contains the acknowledging IAR.
        assert_eq!(ICC.len(), 10);
        assert!(!ICC.contains(&0xc640));
        assert!(!ICC.contains(&0xc660));
        #[cfg(target_os = "linux")]
        {
            let mut state = registers();
            state.kvm_redist(7, 9);
            let data = serde_json::to_value(state).unwrap();
            assert_eq!(data["redistributor"]["0"], 7);
            assert_eq!(data["redistributor"]["20"], 9);
        }
    }
    #[test]
    fn unsupported_profile_and_getter_errors_do_not_return_partial_evidence() {
        assert!(gic(
            |_, _| Err("actual dist read failed".into()),
            |_, _| panic!()
        )
        .unwrap_err()
        .contains("dist read failed"));
        assert!(gic(
            |offset, _| if offset == 4 { Ok(1 << 17) } else { panic!() },
            |_, _| panic!()
        )
        .is_err());
        assert!(gic(
            |offset, _| Ok(match offset {
                0 => 1 << 6,
                4 => 1,
                _ => 0,
            }),
            |_, _| Err("actual redist read failed".into())
        )
        .unwrap_err()
        .contains("redist read failed"));
        assert_eq!(interrupt_count(31).unwrap(), 1020);
        let mut c = sample_cpu();
        c.simd.pop();
        assert!(parts(&c, &registers()).is_err());
    }
}
