use super::*;

pub(in crate::machine::pv) fn fixture(
    memory: u64,
    disk: bool,
    network: Option<[u8; 6]>,
    now: u64,
) -> Header {
    let layout = boot::RamLayout::native(memory).unwrap();
    Header(Capture {
        memory_bytes: memory,
        layout_version: boot::LAYOUT_VERSION,
        page_bytes: PAGE_BYTES as u64,
        segments: layout
            .segments
            .iter()
            .map(|s| (s.start, s.end - s.start))
            .collect(),
        state: State {
            witness: seedbox_machine::machine::execution_evidence::chain::initial(),
            witness_origin: seedbox_machine::machine::execution_evidence::chain::initial(),
            now,
            net: network.map(net::Net::new),
            ..State::default()
        },
        architecture: fixture_architecture(),
        block: disk.then(block::Transport::default),
        disk_faults: disk.then(Default::default),
    })
}
pub(in crate::machine::pv::worker) fn fixture_architecture() -> Architecture {
    // Structural fixture only: zeroed ABI bytes are NOT certified native state.
    #[cfg(target_arch = "x86_64")]
    let value = {
        use kvm_bindings::*;
        serde_json::json!({
            "regs":vec![0u8;std::mem::size_of::<kvm_regs>()],
            "sregs":vec![0u8;std::mem::size_of::<kvm_sregs>()],
            "xsave":vec![0u8;std::mem::size_of::<kvm_xsave>()],
            "xcrs":vec![0u8;std::mem::size_of::<kvm_xcrs>()],
            "msrs":[],"debugregs":vec![0u8;std::mem::size_of::<kvm_debugregs>()],
            "events":vec![0u8;std::mem::size_of::<kvm_vcpu_events>()],"mp_state":0
        })
    };
    #[cfg(target_arch = "aarch64")]
    let value = serde_json::json!({"core":vec![0u64;35],"system":{},"simd":vec![[0u8;16];32],"icc":{},"gic":[0]});
    serde_json::from_value(value).unwrap()
}
mod checks {
    use super::*;
    #[test]
    fn pure_header_validation_covers_shapes_presence_layout_devices_and_time() {
        let memory = 2 << 20;
        let valid = fixture(memory, true, Some([2, 0, 0, 0, 0, 1]), 7);
        valid
            .validate(memory, true, Some([2, 0, 0, 0, 0, 1]), 7)
            .unwrap();
        for kind in 0..10 {
            let mut h = fixture(memory, true, Some([2, 0, 0, 0, 0, 1]), 7);
            match kind {
                0 => h.0.page_bytes = 4096,
                1 => h.0.layout_version = 0,
                2 => h.0.segments[0].1 -= 16384,
                3 => h.0.memory_bytes += 16384,
                4 => h.0.block = None,
                5 => h.0.state.net = None,
                6 => h.0.state.now = 8,
                7 => {
                    h.0.state
                        .writes
                        .push(seedbox_machine::pv::transport::GuestWrite::Console(
                            seedbox_machine::pv::transport::LifecycleWrite {
                                time: 7,
                                bytes: vec![1],
                            },
                        ))
                }
                8 => {
                    let mut v = serde_json::to_value(&h.0.architecture).unwrap();
                    #[cfg(target_arch = "x86_64")]
                    {
                        v["regs"] = serde_json::json!([]);
                    }
                    #[cfg(target_arch = "aarch64")]
                    {
                        v["core"] = serde_json::json!([]);
                    }
                    h.0.architecture = serde_json::from_value(v).unwrap();
                }
                9 => {
                    let mut v = serde_json::to_value(&h.0.state.console).unwrap();
                    v["queues"][0]["size"] = serde_json::json!(257);
                    h.0.state.console = serde_json::from_value(v).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(
                h.validate(memory, true, Some([2, 0, 0, 0, 0, 1]), 7)
                    .is_err(),
                "kind {kind}"
            );
        }
    }
}

#[test]
fn native_device_structure_backlog_controls_irq_and_register_shapes() {
    let memory = 2 << 20;
    let check = |modify: fn(&mut serde_json::Value)| {
        let h = fixture(memory, true, Some([2, 0, 0, 0, 0, 1]), 7);
        let mut value = serde_json::to_value(&h.0).unwrap();
        modify(&mut value);
        let bad = Header(serde_json::from_value(value).unwrap());
        assert!(bad
            .validate(memory, true, Some([2, 0, 0, 0, 0, 1]), 7)
            .is_err());
    };
    for modify in [
        (|v: &mut serde_json::Value| v["state"]["console"]["interrupt"] = serde_json::json!(2))
            as fn(&mut serde_json::Value),
        |v| v["block"]["interrupt"] = serde_json::json!(2),
        |v| v["state"]["net"]["interrupt"] = serde_json::json!(2),
        |v| v["state"]["net"]["queues"][0]["size"] = serde_json::json!(257),
        |v| v["state"]["net"]["backlog"] = serde_json::json!(vec![vec![1u8; 14]; 257]),
        |v| v["state"]["net"]["backlog"] = serde_json::json!([[1, 2, 3]]),
        |v| v["state"]["net"]["backlog"] = serde_json::json!(vec![vec![1u8; 14]]),
        |v| v["state"]["net"]["transmitted"] = serde_json::json!([{"time":7,"bytes":vec![1u8;14]}]),
        |v| v["state"]["console"]["controls"] = serde_json::json!([[0, 1]]),
        |v| v["state"]["console"]["output"] = serde_json::json!([{"time":7,"bytes":[1]}]),
    ] {
        check(modify);
    }
    #[cfg(target_arch = "x86_64")]
    for name in ["regs", "sregs", "xsave", "xcrs", "debugregs", "events"] {
        let h = fixture(memory, false, None, 0);
        let mut value = serde_json::to_value(&h.0).unwrap();
        value["architecture"][name] = serde_json::json!([]);
        assert!(Header(serde_json::from_value(value).unwrap())
            .validate(memory, false, None, 0)
            .is_err());
    }
    // Writable feature words may no longer match FEATURES_OK. Import must not
    // manufacture an invariant the MMIO write path does not maintain.
    let h = fixture(memory, true, None, 0);
    let mut value = serde_json::to_value(&h.0).unwrap();
    value["state"]["console"]["status"] = serde_json::json!(8);
    value["state"]["console"]["features"] = serde_json::json!([u32::MAX, u32::MAX]);
    Header(serde_json::from_value(value).unwrap())
        .validate(memory, true, None, 0)
        .unwrap();
}
#[test]
fn closed_native_records_budget_reject_huge_nested_counts_and_identity_duplicates() {
    use seedbox_machine::snapshot::{DecodeLimits, Reader, Writer};
    let limits = DecodeLimits {
        encoded_bytes: 1 << 20,
        allocation_bytes: 1 << 20,
        collection_items: 1000,
        total_items: 1000,
        string_bytes: 1000,
        blob_bytes: 10000,
    };
    let mut body = vec![];
    let mut w = Writer::new(&mut body);
    w.u64(2 << 20).unwrap();
    w.u32(boot::LAYOUT_VERSION).unwrap();
    w.u64(16384).unwrap();
    w.u64(u64::MAX).unwrap();
    let mut input = &body[..];
    let mut r = Reader::new(&mut input, body.len() as u64, 0, &limits).unwrap();
    assert!(Header::read(&mut r).is_err());
    // The architecture's map/set records reject duplicates while decoding,
    // rather than relying on BTree collection normalization by serde.
    #[cfg(target_arch = "aarch64")]
    {
        use seedbox_machine::snapshot::Record;
        let mut bytes = vec![];
        let mut w = Writer::new(&mut bytes);
        w.u64(2).unwrap();
        w.u32(1).unwrap();
        w.u32(1).unwrap();
        let mut input = &bytes[..];
        let mut r = Reader::new(&mut input, bytes.len() as u64, 0, &limits).unwrap();
        assert!(std::collections::BTreeSet::<u32>::decode(&mut r).is_err());
    }
}

/// Reachable device state built through actual pure MMIO/admission methods,
/// not by editing the serialized backlog/status. No native machine is booted.
pub(in crate::machine::pv) fn backlog_without_driver_ok_fixture(memory: u64, now: u64) -> Header {
    use super::super::virtio::*;
    let mac = [2, 0, 0, 0, 0, 1];
    let mut header = fixture(memory, false, Some(mac), now);
    let net = header.0.state.net.as_mut().unwrap();
    let mut ram = vec![0; 0x10000];
    let mut write = |offset, value| net.write(&mut ram, offset, 4, value, now).unwrap();
    write(DRIVER_FEATURES_SEL, 1);
    write(DRIVER_FEATURES, 1);
    write(QUEUE_SEL, 0);
    write(QUEUE_NUM, 16);
    for (low, high, address) in [
        (QUEUE_DESC_LOW, QUEUE_DESC_HIGH, boot::RAM_BASE),
        (QUEUE_AVAIL_LOW, QUEUE_AVAIL_HIGH, boot::RAM_BASE + 0x1000),
        (QUEUE_USED_LOW, QUEUE_USED_HIGH, boot::RAM_BASE + 0x2000),
    ] {
        write(low, address as u32);
        write(high, (address >> 32) as u32);
    }
    write(QUEUE_READY, 1);
    write(STATUS, 15);
    assert_eq!(
        net.receive(&mut ram, vec![0x42; 14]).unwrap(),
        net::Received::Backlogged
    );
    net.write(&mut ram, STATUS, 4, 0x0b, now).unwrap();
    assert_eq!(net.read(STATUS, 4).unwrap(), 0x0b);
    assert_eq!(net.read(QUEUE_READY, 4).unwrap(), 1);
    assert_eq!(
        serde_json::to_value(&net).unwrap()["backlog"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    header
}
#[test]
fn nonzero_status_clearing_driver_ok_keeps_reachable_backlog_and_private_reader_accepts() {
    let memory = 2 << 20;
    let h = backlog_without_driver_ok_fixture(memory, 7);
    h.validate(memory, false, Some([2, 0, 0, 0, 0, 1]), 7)
        .unwrap();
    let mut file = tempfile::NamedTempFile::new().unwrap();
    protocol::send(&mut file, &h.0).unwrap();
    let start = file.stream_position().unwrap();
    file.as_file().set_len(start + 16384 + memory).unwrap();
    let inputs = BootInputs {
        witness: protocol::BootWitness {
            mark: None,
            boot: 0,
            initial: seedbox_machine::machine::execution_evidence::chain::initial(),
        },
        memory_bytes: memory,
        kernel: vec![],
        initrd: vec![],
        seed: 0,
        command_line: String::new(),
        policy: None,
        disk: None,
        network: Some([2, 0, 0, 0, 0, 1]),
    };
    let saved = read_snapshot(file.path(), &inputs).unwrap();
    assert_eq!(
        serde_json::to_value(saved.capture.state.net.unwrap()).unwrap()["status"],
        serde_json::json!(0x0b)
    );
}
