//! Imported controller settings and both due queues are untrusted codec input.
use super::*;
use crate::driver::fixture::owned::*;
use crate::persistence::{Encoder, Wire};
use crate::pv::SimulationControlId;
fn valid(reply: bool) -> Checkpoint<SnapshotReference> {
    let mut d = build_network(
        vec![
            spec(0, true, vec![(0, Response::Ready)]),
            spec(1, true, vec![(0, Response::Ready)]),
        ],
        vec![],
        true,
        true,
    );
    advance(&mut d, 10);
    let mut c = d.checkpoint().unwrap();
    c.control.controller =
        Some(ControllerSetup::new(Some(MachineId::ORACLE), Duration::from_nanos(5), 5).unwrap());
    c.control.service.reply_latency_ns = 5;
    let id = SimulationControlId {
        session: nz(1),
        request: nz(1),
    };
    let event = if reply {
        let payload = crate::oracle::encode(
            nz(1),
            crate::oracle::ReplyMetadata {
                requested_at_ns: 0,
                admitted_at_ns: 5,
                effect_at_ns: Some(5),
                replied_at_ns: 10,
            },
            Ok(&serde_json::json!({})),
        )
        .unwrap();
        control::Event::Reply {
            target: MachineId::ORACLE,
            boot: c.machines[0].lifecycle.boot(),
            id,
            payload,
        }
    } else {
        control::Event::Call {
            source: MachineId::ORACLE,
            boot: c.machines[0].lifecycle.boot(),
            id,
            observed: 10,
            payload: br#"{"version":1,"id":1,"op":"now"}"#.to_vec(),
        }
    };
    c.events.insert(
        at(if reply { 10 } else { 15 }),
        VecDeque::from([Scheduled::Control(event)]),
    );
    let (bytes, _) = artifact(c);
    Checkpoint::read_from(&mut &bytes[..], limits()).unwrap()
}
fn imported(c: &Checkpoint<SnapshotReference>) -> Vec<u8> {
    let mut encoder = Encoder { output: Vec::new() };
    c.put(&mut encoder).unwrap();
    let mut bytes = b"SBXDRVR\0".to_vec();
    bytes.extend(1u32.to_le_bytes());
    bytes.extend((encoder.output.len() as u64).to_le_bytes());
    bytes.extend(encoder.output);
    bytes
}
fn rejected(c: Checkpoint<SnapshotReference>) {
    let bytes = imported(&c);
    assert!(
        matches!(Checkpoint::read_from(&mut &bytes[..], limits()), Err(CodecError::Invalid(message)) if message != "version")
    );
}
#[test]
fn malformed_controller_and_call_queue_fail_before_any_restore() {
    for case in 0..8 {
        let mut c = valid(false);
        match case {
            0 => c.control.controller.as_mut().unwrap().controller = Some(MachineId(1)),
            1 => c.control.controller.as_mut().unwrap().controller = None,
            2 => c.control.controller.as_mut().unwrap().latency_ns = 4,
            3 => c.control.service.reply_latency_ns = 6,
            4 => c.oracle = MachineId(1),
            5 => {
                let queue = c.events.remove(&at(15)).unwrap();
                c.events.insert(at(16), queue);
            }
            6 => {
                if let Scheduled::Control(control::Event::Call { boot, .. }) =
                    c.events.get_mut(&at(15)).unwrap().front_mut().unwrap()
                {
                    *boot += 1;
                }
            }
            _ => {
                let duplicate = c.events[&at(15)][0].clone();
                c.events.get_mut(&at(15)).unwrap().push_back(duplicate);
            }
        }
        rejected(c);
    }
}
#[test]
fn malformed_reply_queue_boot_authority_metadata_and_duplicates_fail() {
    for case in 0..7 {
        let mut c = valid(true);
        if case == 6 {
            let duplicate = c.events[&at(10)][0].clone();
            c.events.get_mut(&at(10)).unwrap().push_back(duplicate);
        } else if let Scheduled::Control(control::Event::Reply {
            target,
            boot,
            payload,
            ..
        }) = c.events.get_mut(&at(10)).unwrap().front_mut().unwrap()
        {
            match case {
                0 => *target = MachineId(1),
                1 => *boot += 1,
                2 => *payload = b"not json".to_vec(),
                3..=5 => {
                    let mut value: serde_json::Value = serde_json::from_slice(payload).unwrap();
                    let field = ["requested_at_ns", "replied_at_ns", "effect_at_ns"][case - 3];
                    value[field] = serde_json::json!(99);
                    *payload = serde_json::to_vec(&value).unwrap();
                }
                _ => unreachable!(),
            }
        }
        rejected(c);
    }
}
#[test]
fn delayed_queues_obey_decode_limits_and_older_versions_are_not_accepted() {
    for reply in [false, true] {
        let c = valid(reply);
        let bytes = imported(&c);
        let mut small = limits();
        small.collection_items = 0;
        let error = Checkpoint::read_from(&mut &bytes[..], small);
        assert!(
            matches!(error, Err(CodecError::Limit(_))),
            "{:?}",
            error.err()
        );
        let mut old = bytes.clone();
        old[8..12].copy_from_slice(&3u32.to_le_bytes());
        assert!(matches!(
            Checkpoint::read_from(&mut &old[..], limits()),
            Err(CodecError::Invalid(message)) if message == "version"
        ));
    }
}
