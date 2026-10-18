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
fn header_admission_rejects_unnegotiated_offloads() {
    for policy in [Policy::Application, Policy::Network] {
        let (mut model, mut transport, mut memory) = setup(policy, true);
        memory[0x400] = 1;
        let result = model.notify(&mut transport, &mut memory, 0, 1, |_, _| {});
        assert_eq!(result.err().unwrap(), "unnegotiated virtio network offload");
        assert_eq!(transport.queues[1].last_avail, 0);
        assert_eq!(transport.isr, 0);
    }
}
#[test]
fn short_receive_preserves_backlog_without_partial_payload_dma() {
    for policy in [Policy::Application, Policy::Network] {
        let (mut model, mut transport, mut memory) = setup(policy, false);
        memory[0x108..0x10c].copy_from_slice(&12u32.to_le_bytes());
        let before = memory[0x400..0x420].to_vec();
        assert!(model
            .receive(&mut transport, &mut memory, 0, b"frame", |_, _| {})
            .is_err());
        assert_eq!(&memory[0x400..0x420], before);
        assert_eq!(model.backlog.len(), 1);
        assert_eq!(transport.queues[0].used_idx, 0);
        assert_eq!(transport.isr, 0);
        memory[0x108..0x10c].copy_from_slice(&32u32.to_le_bytes());
        model
            .notify(&mut transport, &mut memory, 0, 0, |_, _| {})
            .unwrap();
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
        assert_eq!(transport.isr, 1);
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
    transport.status = 15;
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

#[test]
fn aliased_receive_dma_preserves_the_same_completed_prefix() {
    let mut reference = None;
    for policy in [Policy::Application, Policy::Network] {
        let (mut model, mut transport, mut memory) = setup(policy, false);
        // The first payload overwrites the next descriptor. Validation of that
        // next request must use the mutated RAM, without rolling back the first.
        memory[0x100..0x108].copy_from_slice(&0x110u64.to_le_bytes());
        memory[0x110..0x118].copy_from_slice(&0x500u64.to_le_bytes());
        memory[0x118..0x11c].copy_from_slice(&32u32.to_le_bytes());
        memory[0x11c..0x11e].copy_from_slice(&2u16.to_le_bytes());
        memory[0x202..0x204].copy_from_slice(&2u16.to_le_bytes());
        memory[0x206..0x208].copy_from_slice(&1u16.to_le_bytes());
        model
            .backlog
            .extend([b"first".to_vec(), b"second".to_vec()]);
        let mut dirty = Vec::new();
        assert!(model
            .notify(&mut transport, &mut memory, 0, 0, |a, n| dirty.push((a, n)))
            .is_err());
        assert_eq!(
            (
                transport.queues[0].last_avail,
                transport.queues[0].used_idx,
                transport.isr
            ),
            (1, 1, 1)
        );
        assert_eq!(model.backlog, VecDeque::from([b"second".to_vec()]));
        assert_eq!(&memory[0x11c..0x121], b"first");
        // x86's explicit notification optimization journals its flag writes too.
        // Compare all payload and completion DMA, in their original order.
        dirty.retain(|&(a, _)| a != 0x300);
        assert_eq!(dirty, [(0x110, 17), (0x304, 8), (0x302, 2)]);
        memory[0x300..0x302].fill(0);
        if let Some(want) = &reference {
            assert_eq!(&memory, want);
        } else {
            reference = Some(memory);
        }
    }
}

/// NEXT-100 case 84: a driver reset after a partial receive. Two frames arrive,
/// the first is DMAed and interrupts, the second waits in the backlog; the
/// driver resets through Status with the interrupt still asserted and
/// negotiates fresh rings elsewhere. Nothing from before the reset may reach
/// the new rings or the interrupt line, and the device must be indistinguishable
/// from one that only ever saw the fresh negotiation.
mod reset_after_partial_rx {
    use super::*;
    use crate::virtio_mmio::Effect;

    pub(super) struct Rings {
        pub num: u16,
        pub desc: usize,
        pub avail: usize,
        pub used: usize,
        pub buffer: usize,
    }
    pub(super) const OLD: Rings = Rings {
        num: 2,
        desc: 0x100,
        avail: 0x200,
        used: 0x300,
        buffer: 0x400,
    };
    pub(super) const NEW: Rings = Rings {
        num: 4,
        desc: 0x800,
        avail: 0x900,
        used: 0xa00,
        buffer: 0xc00,
    };
    const TX: Rings = Rings {
        num: 2,
        desc: 0xe00,
        avail: 0xe40,
        used: 0xe80,
        buffer: 0xf00,
    };

    pub(super) struct Device {
        pub model: Network,
        pub transport: Transport,
        pub memory: Vec<u8>,
        pub dirty: Vec<(usize, usize)>,
    }

    impl Device {
        pub fn new(policy: Policy) -> Self {
            let offered = if policy == Policy::Network {
                (1 << 5) | EVENT_IDX
            } else {
                1 << 5
            };
            Self {
                model: Network::new([2, 0, 0, 0, 0, 1], policy),
                transport: Transport::new(
                    Spec {
                        device_id: 1,
                        features: [offered, 1],
                    },
                    2,
                    if policy == Policy::Application {
                        MmioPolicy::Application
                    } else {
                        MmioPolicy::Network
                    },
                ),
                memory: vec![0; 0x1000],
                dirty: Vec::new(),
            }
        }
        /// One register write with the adapters' effect handling.
        pub fn mmio(&mut self, offset: u64, value: u32) -> Effect {
            let dirty = &mut self.dirty;
            let effect = self.transport.write(offset, value).unwrap();
            match effect {
                Effect::Reset => self.model.reset(),
                Effect::Notify(index) => {
                    self.model
                        .notify(&mut self.transport, &mut self.memory, 0, index, |a, n| {
                            dirty.push((a, n))
                        })
                        .unwrap();
                }
                Effect::None => {}
            }
            self.model
                .after_write(&self.transport, &mut self.memory, 0, offset, |a, n| {
                    dirty.push((a, n))
                })
                .unwrap();
            effect
        }
        pub fn negotiate(&mut self, rx: &Rings, event_idx: bool) {
            self.mmio(0x70, 1);
            self.mmio(0x70, 3);
            self.mmio(0x24, 0);
            self.mmio(0x20, if event_idx { EVENT_IDX } else { 0 });
            self.mmio(0x24, 1);
            self.mmio(0x20, 1);
            self.mmio(0x70, 0xb);
            for (index, rings) in [(0, rx), (1, &TX)] {
                self.mmio(0x30, index);
                self.mmio(0x38, rings.num as u32);
                self.mmio(0x80, rings.desc as u32);
                self.mmio(0x90, rings.avail as u32);
                self.mmio(0xa0, rings.used as u32);
                self.mmio(0x44, 1);
            }
            self.mmio(0x70, 0xf);
        }
        /// Post one 128-byte receive buffer in descriptor `slot`.
        pub fn post(&mut self, rx: &Rings, slot: u16) {
            let m = &mut self.memory;
            let d = rx.desc + 16 * slot as usize;
            let at = (rx.buffer + 128 * slot as usize) as u64;
            m[d..d + 8].copy_from_slice(&at.to_le_bytes());
            m[d + 8..d + 12].copy_from_slice(&128u32.to_le_bytes());
            m[d + 12..d + 14].copy_from_slice(&2u16.to_le_bytes());
            let idx = u16::from_le_bytes(m[rx.avail + 2..rx.avail + 4].try_into().unwrap());
            let entry = rx.avail + 4 + 2 * (idx % rx.num) as usize;
            m[entry..entry + 2].copy_from_slice(&slot.to_le_bytes());
            m[rx.avail + 2..rx.avail + 4].copy_from_slice(&idx.wrapping_add(1).to_le_bytes());
        }
        pub fn receive(&mut self, frame: &[u8]) -> Received {
            let dirty = &mut self.dirty;
            self.model
                .receive(&mut self.transport, &mut self.memory, 0, frame, |a, n| {
                    dirty.push((a, n))
                })
                .unwrap()
        }
        pub fn state(&self) -> String {
            serde_json::to_string(&(self.transport.snapshot(), self.model.snapshot())).unwrap()
        }
    }

    fn within(rings: &Rings, (at, len): (usize, usize)) -> bool {
        let n = rings.num as usize;
        [
            (rings.desc, 16 * n),
            (rings.avail, 6 + 2 * n),
            (rings.used, 6 + 8 * n),
            (rings.buffer, 128 * n),
        ]
        .iter()
        .any(|&(start, size)| at >= start && at + len <= start + size)
    }

    #[test]
    fn reset_discards_stale_frames_and_interrupts_before_fresh_rings() {
        for policy in [Policy::Application, Policy::Network] {
            for event_idx in [false, true] {
                if event_idx && policy == Policy::Application {
                    continue;
                }
                let mut device = Device::new(policy);
                device.negotiate(&OLD, event_idx);
                device.post(&OLD, 0);
                let first = device.receive(b"first");
                assert_eq!(
                    first,
                    Received {
                        accepted: true,
                        irq: true
                    }
                );
                assert_eq!(&device.memory[OLD.buffer + 12..OLD.buffer + 17], b"first");
                // No second buffer: the frame is accepted into the backlog.
                assert_eq!(
                    device.receive(b"second"),
                    Received {
                        accepted: true,
                        irq: false
                    }
                );
                assert_eq!(device.model.backlog.len(), 1);
                assert_eq!(device.transport.read(0x60), 1, "interrupt left asserted");

                assert_eq!(device.mmio(0x70, 0), Effect::Reset);
                assert_eq!(device.transport.read(0x60), 0);
                assert_eq!(device.transport.read(0x70), 0);
                assert!(device.model.backlog.is_empty());
                for queue in &device.transport.queues {
                    assert!(!queue.ready);
                    assert_eq!((queue.num, queue.last_avail, queue.used_idx), (0, 0, 0));
                }
                assert!(!device.model.event_idx(&device.transport));
                let at_reset = device.memory.clone();
                device.dirty.clear();

                device.negotiate(&NEW, event_idx);
                device.post(&NEW, 0);
                device.post(&NEW, 1);
                let posted = device.memory.clone();
                // The driver kicks RX after refilling; the stale frame must not appear.
                device.mmio(0x50, 0);
                assert_eq!(device.transport.read(0x60), 0, "stale interrupt");
                assert_eq!(device.transport.queues[0].used_idx, 0);
                let changed: Vec<_> = (0..posted.len())
                    .filter(|&i| posted[i] != device.memory[i])
                    .collect();
                assert!(
                    changed.iter().all(|&i| within(&NEW, (i, 1))),
                    "kick wrote outside the fresh rings: {changed:x?}"
                );

                let third = device.receive(b"third");
                assert_eq!(
                    third,
                    Received {
                        accepted: true,
                        irq: true
                    }
                );
                assert_eq!(&device.memory[NEW.buffer + 12..NEW.buffer + 17], b"third");
                let used = NEW.used + 4;
                assert_eq!(&device.memory[used..used + 8], &[0, 0, 0, 0, 17, 0, 0, 0]);
                assert_eq!(
                    &device.memory[NEW.used + 2..NEW.used + 4],
                    &1u16.to_le_bytes()
                );
                assert_eq!(device.transport.queues[0].last_avail, 1);
                assert_eq!(device.transport.read(0x60), 1);
                // The old rings are guest memory now: untouched since the reset.
                for (i, byte) in at_reset.iter().enumerate() {
                    if within(&OLD, (i, 1)) {
                        assert_eq!(device.memory[i], *byte, "old ring byte {i:#x}");
                    }
                }
                assert!(!device.memory.windows(6).any(|w| w == b"second"));
                let payload = [(NEW.buffer, 17), (used, 8), (NEW.used + 2, 2)];
                let journal: Vec<_> = device
                    .dirty
                    .iter()
                    .copied()
                    .filter(|entry| payload.contains(entry))
                    .collect();
                assert_eq!(journal, payload);
                assert!(device.dirty.iter().all(|&entry| within(&NEW, entry)));

                // Indistinguishable from a device that only saw the fresh rings.
                let mut fresh = Device::new(policy);
                fresh.memory = at_reset.clone();
                fresh.negotiate(&NEW, event_idx);
                fresh.post(&NEW, 0);
                fresh.post(&NEW, 1);
                fresh.mmio(0x50, 0);
                fresh.receive(b"third");
                assert_eq!(fresh.memory, device.memory);
                assert_eq!(fresh.state(), device.state(), "event_idx={event_idx}");
            }
        }
    }
}
