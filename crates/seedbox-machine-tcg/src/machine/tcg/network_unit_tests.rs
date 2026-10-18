//! Portable checks of the TCG NIC adapter: the private QMP frame encoding,
//! drain validation, output ordering and receive-input validation.
use super::qemu::{drain_chunk, hex, unhex};
use super::*;
use crate::pv::transport::{GuestWrite, LifecycleWrite};
use serde_json::json;

fn frame(time: u64, byte: u8) -> LifecycleWrite {
    LifecycleWrite {
        time,
        bytes: vec![byte; 60],
    }
}

#[test]
fn frames_cross_qmp_as_lowercase_hex_only() {
    let bytes: Vec<u8> = (0..=255).collect();
    assert_eq!(unhex(&hex(&bytes)), Some(bytes));
    assert_eq!(hex(&[0x0a, 0xff]), "0aff");
    for invalid in ["0", "0A", "zz", "0a0", " 0a"] {
        assert_eq!(unhex(invalid), None, "{invalid}");
    }
}

#[test]
fn drain_chunks_are_bounded_ordered_and_in_frame_bounds() {
    let item = |time: u64, length: usize| json!({"time": time, "bytes": hex(&vec![1; length])});
    let mut frames = Vec::new();
    let chunk = json!({"frames": [item(5, 14), item(5, 1514), item(9, 60)], "more": true});
    assert!(drain_chunk(&chunk, &mut frames).unwrap());
    // A later chunk continues the same order.
    let chunk = json!({"frames": [item(9, 60)], "more": false});
    assert!(!drain_chunk(&chunk, &mut frames).unwrap());
    assert_eq!(
        frames
            .iter()
            .map(|f| (f.time, f.bytes.len()))
            .collect::<Vec<_>>(),
        [(5, 14), (5, 1514), (9, 60), (9, 60)]
    );
    let full: Vec<_> = (0..256).map(|i| item(10 + i, 60)).collect();
    let mut sink = frames.clone();
    assert!(!drain_chunk(&json!({"frames": full, "more": false}), &mut sink).unwrap());
    assert_eq!(sink.len(), 4 + 256);
    let too_many: Vec<_> = (0..257).map(|i| item(10 + i, 60)).collect();
    for invalid in [
        json!({"frames": too_many, "more": true}),
        json!({"frames": [], "more": true}),
        json!({"frames": [item(8, 60)], "more": false}),
        json!({"frames": [item(20, 13)], "more": false}),
        json!({"frames": [item(20, 1515)], "more": false}),
        json!({"frames": [{"time": 20, "bytes": "0A".repeat(60)}], "more": false}),
        json!({"frames": [{"time": -1, "bytes": "00".repeat(60)}], "more": false}),
        json!({"frames": [{"bytes": "00".repeat(60)}], "more": false}),
        json!({"frames": [item(20, 60)]}),
    ] {
        let mut sink = frames.clone();
        assert!(drain_chunk(&invalid, &mut sink).is_err(), "{invalid}");
    }
}

#[test]
fn frames_merge_into_guest_output_by_timestamp() {
    let console = |time: u64| {
        GuestWrite::Console(LifecycleWrite {
            time,
            bytes: b"c".to_vec(),
        })
    };
    let lifecycle = |time: u64| {
        GuestWrite::Lifecycle(LifecycleWrite {
            time,
            bytes: b"l".to_vec(),
        })
    };
    let merged = merge_frames(
        vec![console(2), lifecycle(4), console(7)],
        vec![frame(1, 1), frame(4, 2), frame(5, 3), frame(9, 4)],
    );
    let order: Vec<_> = merged
        .iter()
        .map(|write| match write {
            GuestWrite::Console(write) => (write.time, 'c'),
            GuestWrite::Lifecycle(write) => (write.time, 'l'),
            GuestWrite::Frame(write) => (write.time, 'f'),
        })
        .collect();
    // A frame never follows later console output; ties keep the guest's
    // console and lifecycle writes first.
    assert_eq!(
        order,
        [
            (1, 'f'),
            (2, 'c'),
            (4, 'l'),
            (4, 'f'),
            (5, 'f'),
            (7, 'c'),
            (9, 'f')
        ]
    );
    assert!(merged
        .windows(2)
        .all(|pair| pair[0].time() <= pair[1].time()));
    assert!(merge_frames(vec![], vec![]).is_empty());
}

#[test]
fn invalid_received_frames_fail_the_run_before_any_qemu_work() {
    use crate::{
        clock::SimulationClock,
        execution::ExecutionConfig,
        pv::{Inbox, Input, Outbox, Request},
        rng::SeededRng,
    };
    for bytes in [vec![0; 13], vec![0; 1515]] {
        // A missing executable proves no QEMU was started.
        let mut machine = TcgMachine::new_raw(
            Path::new("/nonexistent/seedbox-qemu"),
            Manifest {
                // A spinning program (`jmp $`); it never runs.
                boot: Boot::Program(
                    super::raw_program_data::Asm::default()
                        .spin()
                        .image()
                        .into(),
                ),
                memory_mib: 16,
                disk: None,
                network: Some([2, 0, 0, 0, 0, 1]),
            },
        )
        .unwrap();
        let at = Timestamp::from_nanos(0);
        let clock = SimulationClock::new(0);
        let mut rng = SeededRng::new(1);
        let mut inbox: std::collections::VecDeque<_> = [Input {
            at,
            payload: Request::ReceiveFrame { bytes },
        }]
        .into();
        let mut outputs = Vec::new();
        let error = machine
            .run(
                &mut RunContext {
                    worker: &mut (),
                    clock: &clock,
                    rng: &mut rng,
                    execution: ExecutionConfig::default(),
                    inbox: Inbox::new(&mut inbox, at),
                    outbox: Outbox::new(&mut outputs, at, at),
                },
                at,
            )
            .unwrap_err();
        assert!(error.to_string().contains("frame bounds"), "{error}");
        assert!(machine.snapshot().is_err(), "the machine is invalid");
    }
}
