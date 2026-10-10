//! Checkpoint continuation across the public model/transport/RAM boundary.
use seedbox_engine_core::{
    virtio_console::{Console, Policy as ConsolePolicy},
    virtio_mmio::{Policy, Spec, Transport},
    virtio_network::{Network, Policy as NetworkPolicy},
};

#[test]
fn serialized_devices_resume_pending_dma_with_the_same_bytes_cursors_and_interrupts() {
    for network in [false, true] {
        let spec = Spec {
            device_id: if network { 1 } else { 3 },
            features: [0, 1],
        };
        let queue_count = if network { 2 } else { 4 };
        let mut transport = Transport::new(spec, queue_count, Policy::Application);
        let queue = &mut transport.queues[0];
        queue.num = 8;
        queue.ready = true;
        queue.desc = 0x100;
        queue.avail = 0x200;
        queue.used = 0x300;
        let mut memory = vec![0; 4096];
        memory[0x100..0x108].copy_from_slice(&0x400u64.to_le_bytes());
        memory[0x108..0x10c].copy_from_slice(&128u32.to_le_bytes());
        memory[0x10c..0x10e].copy_from_slice(&2u16.to_le_bytes()); // Writable buffer.
        let mut net = Network::new([2, 0, 0, 0, 0, 1], NetworkPolicy::Application);
        let mut console = Console::new(ConsolePolicy::Application);
        let payload = b"pending before checkpoint";
        // The queue exists but the guest has not offered a receive buffer yet.
        if network {
            assert!(
                net.receive(&mut transport, &mut memory, 0, payload, |_, _| {})
                    .unwrap()
                    .accepted
            );
        } else {
            console
                .receive(&mut transport, &mut memory, 0, payload, |_, _| {})
                .unwrap();
        }
        let saved_transport = serde_json::to_vec(&transport.snapshot()).unwrap();
        let saved_net = serde_json::to_vec(&net.snapshot()).unwrap();
        let saved_console = serde_json::to_vec(&console.snapshot()).unwrap();
        assert!(Transport::new(spec, 1, Policy::Application)
            .restore_snapshot(serde_json::from_slice(&saved_transport).unwrap())
            .is_err());
        let mut outcomes = Vec::new();
        for restore in [false, true] {
            let mut t = if restore {
                Transport::new(spec, queue_count, Policy::Application)
                    .restore_snapshot(serde_json::from_slice(&saved_transport).unwrap())
                    .unwrap()
            } else {
                transport.clone()
            };
            let mut n = if restore {
                Network::new([2, 0, 0, 0, 0, 1], NetworkPolicy::Application)
                    .restore_snapshot(serde_json::from_slice(&saved_net).unwrap())
                    .unwrap()
            } else {
                net.clone()
            };
            let mut c = if restore {
                Console::new(ConsolePolicy::Application)
                    .restore_snapshot(serde_json::from_slice(&saved_console).unwrap())
                    .unwrap()
            } else {
                console.clone()
            };
            let mut ram = memory.clone();
            ram[0x202..0x204].copy_from_slice(&1u16.to_le_bytes());
            let mut dirty = Vec::new();
            let irq = if network {
                n.notify(&mut t, &mut ram, 0, 0, |at, len| dirty.push((at, len)))
                    .unwrap()
                    .irq
            } else {
                c.notify(&mut t, &mut ram, 0, 0, |at, len| dirty.push((at, len)))
                    .unwrap()
                    .irq
            };
            assert!(irq);
            assert_eq!((t.queues[0].last_avail, t.queues[0].used_idx), (1, 1));
            let start = 0x400 + if network { 12 } else { 0 };
            assert_eq!(&ram[start..start + payload.len()], payload);
            outcomes.push((
                ram,
                dirty,
                serde_json::to_value(t.projection()).unwrap(),
                serde_json::to_value(n.snapshot()).unwrap(),
                serde_json::to_value(c.snapshot()).unwrap(),
            ));
        }
        assert_eq!(outcomes[0], outcomes[1]);
    }
}
