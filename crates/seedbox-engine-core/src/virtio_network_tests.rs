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
            assert_eq!(dirty, vec![(0x300, 2), (0x304, 8), (0x302, 2), (0x300, 2)]);
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
        let received = model.receive(&mut transport, &mut memory, 0, b"frame", |a, n| {
            dirty.push((a, n))
        });
        if policy == Policy::Application {
            assert!(received.is_err());
            assert_eq!(memory, before);
            assert!(dirty.is_empty());
            assert_eq!(model.backlog.len(), 1);
        } else {
            assert_eq!(
                received.unwrap(),
                super::Received {
                    accepted: true,
                    irq: true
                }
            );
            assert_eq!(&memory[0x400..0x40c], &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0]);
            assert_eq!(dirty, [(0x400, 12), (0x304, 8), (0x302, 2)]);
            assert!(model.backlog.is_empty());
            assert_eq!(model.stats.rx_frames, 1);
            continue;
        }
        memory[0x108..0x10c].copy_from_slice(&32u32.to_le_bytes());
        model
            .notify(&mut transport, &mut memory, 0, 0, |a, n| dirty.push((a, n)))
            .unwrap();
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
        assert_eq!(
            dirty,
            if policy == Policy::Application {
                vec![(0x304, 8), (0x302, 2)]
            } else {
                vec![(0x300, 2), (0x304, 8), (0x302, 2)]
            }
        );
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

#[test]
fn application_profile_never_enables_ring_extensions_or_deferred_tx() {
    let (mut model, mut transport, mut memory) = setup(Policy::Application, true);
    let projection = model.projection();
    model.set_deferred_tx(true);
    transport.write(0x20, EVENT_IDX).unwrap();
    transport.write(0x70, 15).unwrap();
    assert!(!model.deferred_tx());
    assert!(!model.event_idx(&transport));
    assert_eq!(model.projection(), projection);
    memory[0x200..0x202].copy_from_slice(&1u16.to_le_bytes());
    let mut dirty = vec![];
    assert!(
        model
            .notify(&mut transport, &mut memory, 0, 1, |a, n| dirty.push((a, n)))
            .unwrap()
            .irq
    );
    assert_eq!(dirty, [(0x304, 8), (0x302, 2)]);
    assert_eq!(model.stats.tx_frames, 0);
}

#[test]
fn notification_extensions_validate_ring_spans_before_dma() {
    for event_idx in [false, true] {
        for malformed in 0..5 {
            let mut memory = vec![0; 4096];
            let mut queue = Queue {
                num: 2,
                ready: true,
                desc: 0x100,
                avail: 0x200,
                used: 0x300,
                ..Default::default()
            };
            match malformed {
                0 => queue.num = 3,
                1 => queue.avail = u64::MAX - 1,
                2 => queue.used = 4092,
                3 => queue.desc = u64::MAX - 15,
                4 => memory[0x202..0x204].copy_from_slice(&3u16.to_le_bytes()),
                _ => unreachable!(),
            }
            let before = memory.clone();
            let mut dirty = vec![];
            assert!(queue
                .arm_notify(&mut memory, 0, event_idx, true, |a, n| dirty.push((a, n)))
                .is_err());
            assert!(queue.interrupt(&memory, 0, event_idx, 0).is_err());
            assert!(queue.pending(&memory, 0).is_err());
            assert!(dirty.is_empty());
            assert_eq!(memory, before);
        }
    }
    let queue = Queue::default();
    assert!(!queue
        .arm_notify(&mut [], 0, true, true, |_, _| panic!("unready DMA"))
        .unwrap());
}
