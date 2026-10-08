use super::*;
use crate::virtio_mmio::{Policy as MmioPolicy, Spec};
fn setup(policy: Policy, tx: bool) -> (Network, Transport, Vec<u8>) {
    let model = Network::new([2, 0, 0, 0, 0, 1], policy);
    let mut transport = Transport::new(
        Spec {
            device_id: 1,
            features: [1 << 5, 1],
        },
        2,
        if policy == Policy::Application {
            MmioPolicy::Application
        } else {
            MmioPolicy::Network
        },
    );
    transport.queues[usize::from(tx)] = Queue {
        num: 2,
        ready: true,
        desc: 0x100,
        avail: 0x200,
        used: 0x300,
        ..Default::default()
    };
    let mut memory = vec![0; 4096];
    memory[0x100..0x108].copy_from_slice(&0x400u64.to_le_bytes());
    memory[0x108..0x10c].copy_from_slice(&32u32.to_le_bytes());
    memory[0x10c..0x10e].copy_from_slice(&(if tx { 0u16 } else { 2 }).to_le_bytes());
    memory[0x202..0x204].copy_from_slice(&1u16.to_le_bytes());
    (model, transport, memory)
}
#[test]
fn header_profiles_retain_distinct_admission() {
    for policy in [Policy::Application, Policy::Network] {
        let (mut model, mut transport, mut memory) = setup(policy, true);
        memory[0x400] = 1;
        let mut dirty = vec![];
        let result = model.notify(&mut transport, &mut memory, 0, 1, |a, n| dirty.push((a, n)));
        if policy == Policy::Application {
            assert_eq!(result.err().unwrap(), "unnegotiated virtio network offload");
            assert!(dirty.is_empty());
            assert_eq!(transport.queues[1].last_avail, 0);
        } else {
            assert_eq!(result.unwrap().frames, vec![vec![0; 20]]);
            assert_eq!(dirty, vec![(0x304, 8), (0x302, 2)]);
        }
    }
}
#[test]
fn receive_error_retains_application_backlog_and_network_direct_request() {
    for policy in [Policy::Application, Policy::Network] {
        let (mut model, mut transport, mut memory) = setup(policy, false);
        memory[0x108..0x10c].copy_from_slice(&12u32.to_le_bytes());
        let before = memory.clone();
        let mut dirty = vec![];
        assert!(model
            .receive(&mut transport, &mut memory, 0, b"frame", |a, n| dirty
                .push((a, n)))
            .is_err());
        assert_eq!(memory, before);
        assert!(dirty.is_empty());
        assert_eq!(
            model.backlog.len(),
            usize::from(policy == Policy::Application)
        );
        memory[0x108..0x10c].copy_from_slice(&32u32.to_le_bytes());
        if policy == Policy::Application {
            model
                .notify(&mut transport, &mut memory, 0, 0, |a, n| dirty.push((a, n)))
                .unwrap();
        } else {
            model
                .receive(&mut transport, &mut memory, 0, b"frame", |a, n| {
                    dirty.push((a, n))
                })
                .unwrap();
        }
        assert_eq!(dirty, vec![(0x400, 17), (0x304, 8), (0x302, 2)]);
        assert_eq!(&memory[0x40c..0x411], b"frame");
        assert_eq!(transport.isr, 1);
        assert!(model.backlog.is_empty());
        assert_eq!(model.stats.rx_frames, u64::from(policy == Policy::Network));
    }
}
#[test]
fn late_transmit_error_preserves_profile_interrupt_boundary() {
    for policy in [Policy::Application, Policy::Network] {
        let (mut model, mut transport, mut memory) = setup(policy, true);
        memory[0x202..0x204].copy_from_slice(&2u16.to_le_bytes());
        memory[0x206..0x208].copy_from_slice(&2u16.to_le_bytes());
        let mut dirty = vec![];
        assert!(model
            .notify(&mut transport, &mut memory, 0, 1, |a, n| dirty.push((a, n)))
            .is_err());
        assert_eq!(transport.queues[1].last_avail, 1);
        assert_eq!(dirty, vec![(0x304, 8), (0x302, 2)]);
        assert_eq!(transport.isr, u32::from(policy == Policy::Application));
    }
}

#[test]
fn application_tx_notification_reports_interrupt_from_receive_drain() {
    let (mut model, mut transport, mut memory) = setup(Policy::Application, false);
    memory[0x202..0x204].fill(0);
    assert!(
        model
            .receive(&mut transport, &mut memory, 0, b"frame", |_, _| {})
            .unwrap()
            .accepted
    );
    memory[0x202..0x204].copy_from_slice(&1u16.to_le_bytes());
    let mut dirty = vec![];
    let output = model
        .notify(&mut transport, &mut memory, 0, 1, |a, n| dirty.push((a, n)))
        .unwrap();
    assert!(output.frames.is_empty());
    assert!(output.irq);
    assert_eq!(transport.isr, 1);
    assert_eq!(dirty, vec![(0x400, 17), (0x304, 8), (0x302, 2)]);
}
