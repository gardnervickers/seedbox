use super::*;
fn witness() -> BootWitness {
    BootWitness {
        boot: 0,
        initial: seedbox_machine::machine::execution_evidence::chain::initial(),
        mark: None,
    }
}

#[test]
fn explicit_memory_contract_rejects_old_and_mismatched_workers() {
    let memory = 2048 << 20;
    assert!(ready(
        Response::Ready {
            witness: witness(),
            memory_bytes: memory,
            layout_version: super::super::boot::LAYOUT_VERSION,
            policy: None,
        },
        memory,
        None,
        &witness(),
    )
    .is_ok());
    assert!(ready(
        Response::Ready {
            witness: witness(),
            memory_bytes: 128 << 20,
            layout_version: super::super::boot::LAYOUT_VERSION,
            policy: None,
        },
        memory,
        None,
        &witness(),
    )
    .is_err());
    assert!(ready(
        Response::Ready {
            witness: witness(),
            memory_bytes: memory,
            layout_version: 0,
            policy: None,
        },
        memory,
        None,
        &witness(),
    )
    .is_err());
    assert!(serde_json::from_slice::<Response>(br#""Ready""#).is_err());
    let old = serde_json::json!({"Boot":{"kernel":"k","initrd":"i","command_line":"", "seed":1,"now":0,"disk":null,"network":null}});
    let mut frame = Vec::new();
    send(&mut frame, &old).unwrap();
    assert!(receive::<Request>(&mut &frame[..]).is_err());
}

#[test]
fn truncated_frames_are_errors_not_eof() {
    assert!(receive::<Request>(&mut &b"\x04"[..]).is_err());
    assert!(receive::<Request>(&mut &b"\x04\0\0\0{}"[..]).is_err());
    assert!(receive::<Request>(&mut &b"\xff\xff\xff\xff"[..]).is_err());
    assert!(receive::<Request>(&mut &b""[..]).unwrap().is_none());
}

#[test]
fn retired_standalone_restore_is_structurally_rejected() {
    let mut frame = Vec::new();
    send(&mut frame, &serde_json::json!({"Restore":"unused"})).unwrap();
    let error = receive::<Request>(&mut &frame[..])
        .err()
        .expect("retired variant");
    assert!(error.contains("unknown variant `Restore`"), "{error}");
}

#[test]
fn old_absent_policy_ack_cannot_silently_enable_selected_semantics() {
    let p = BootPolicy {
        name: "node".into(),
        seed: 42,
        boot: 0,
        timer_jitter_ns: 7,
        stop_tolerance_ns: 9,
        speed_permille: 985,
    };
    let response = |policy| Response::Ready {
        witness: witness(),
        memory_bytes: 128 << 20,
        layout_version: super::super::boot::LAYOUT_VERSION,
        policy,
    };
    assert!(ready(response(None), 128 << 20, Some(&p), &witness()).is_err());
    assert!(ready(response(Some(p.clone())), 128 << 20, None, &witness()).is_err());
    assert!(ready(response(Some(p.clone())), 128 << 20, Some(&p), &witness()).is_ok());
    let mut wrong = p.clone();
    wrong.boot = 1;
    assert!(ready(response(Some(wrong)), 128 << 20, Some(&p), &witness()).is_err());
}

#[test]
fn exact_native_witness_owner_origin_ack_is_required_even_without_policy() {
    let expected = witness();
    for wrong in [
        BootWitness {
            boot: 1,
            ..witness()
        },
        BootWitness {
            initial: [0; 32],
            ..witness()
        },
        BootWitness {
            mark: Some(1),
            ..witness()
        },
    ] {
        assert!(ready(
            Response::Ready {
                witness: wrong,
                memory_bytes: 128 << 20,
                layout_version: super::super::boot::LAYOUT_VERSION,
                policy: None
            },
            128 << 20,
            None,
            &expected
        )
        .is_err());
    }
    assert!(serde_json::from_value::<Response>(serde_json::json!({"Ready":{"memory_bytes":128<<20,"layout_version":super::super::boot::LAYOUT_VERSION,"policy":null}})).is_err());
}
