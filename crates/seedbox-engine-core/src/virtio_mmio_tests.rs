use crate::virtio_mmio::{Effect, Policy, Spec, Transport};

fn device(policy: Policy) -> Transport {
    Transport::new(
        Spec {
            device_id: 3,
            features: [2, 1],
        },
        match policy {
            Policy::Block => 1,
            Policy::Network => 2,
            _ => 4,
        },
        policy,
    )
}

#[test]
fn queue_profiles_preserve_validation_precedence_and_cursor_transitions() {
    for policy in [
        Policy::Application,
        Policy::Console,
        Policy::Block,
        Policy::Network,
    ] {
        let mut t = device(policy);
        t.write(0x30, 99).unwrap();
        assert_eq!(t.read(0x34), 0);
        assert_eq!(t.read(0x44), 0);
        let invalid_selector = t.write(0x38, 0);
        match policy {
            Policy::Application => assert_eq!(
                invalid_selector.unwrap_err(),
                "virtio queue selection out of range"
            ),
            Policy::Console => assert_eq!(
                invalid_selector.unwrap_err(),
                "invalid console queue selector"
            ),
            Policy::Block | Policy::Network => assert_eq!(invalid_selector.unwrap(), Effect::None),
        }
        t.write(0x30, 0).unwrap();
        let oversized = t.write(0x38, 65537);
        if matches!(policy, Policy::Block | Policy::Network) {
            assert_eq!(oversized.unwrap(), Effect::None);
            assert_eq!(t.queues[0].num, 1);
        } else {
            assert_eq!(
                oversized.unwrap_err(),
                if policy == Policy::Console {
                    "invalid console queue size"
                } else {
                    "invalid virtio queue size"
                }
            );
            assert_eq!(t.queues[0].num, 0);
        }
        t.write(0x38, 8).unwrap();
        t.write(0x44, 1).unwrap();
        t.queues[0].last_avail = 5;
        t.queues[0].used_idx = 7;
        t.write(0x44, 1).unwrap();
        assert_eq!(
            (t.queues[0].last_avail, t.queues[0].used_idx),
            if policy == Policy::Console {
                (5, 7)
            } else {
                (0, 0)
            }
        );
        let mutation = t.write(0x38, 0);
        if policy == Policy::Console {
            assert_eq!(mutation.unwrap_err(), "console queue changed while ready");
        } else if policy == Policy::Application {
            assert_eq!(mutation.unwrap_err(), "invalid virtio queue size");
        } else {
            mutation.unwrap();
            assert_eq!(t.queues[0].num, u16::from(policy == Policy::Network));
        }
        let invalid_ready = t.write(0x44, 2);
        if matches!(policy, Policy::Block | Policy::Network) {
            invalid_ready.unwrap();
            assert!(!t.queues[0].ready);
        } else {
            assert_eq!(
                invalid_ready.unwrap_err(),
                if policy == Policy::Console {
                    "invalid console queue ready value"
                } else {
                    "invalid virtio queue ready flag"
                }
            );
            assert!(t.queues[0].ready);
        }
        t.write(0x44, 0).unwrap();
        assert_eq!((t.queues[0].last_avail, t.queues[0].used_idx), (0, 0));
    }
}

#[test]
fn notification_and_feature_errors_keep_existing_order_and_partial_status() {
    for policy in [
        Policy::Application,
        Policy::Console,
        Policy::Block,
        Policy::Network,
    ] {
        let mut t = device(policy);
        let invalid = t.write(0x50, 99);
        match policy {
            Policy::Application => assert_eq!(
                invalid.unwrap_err(),
                "virtio notification queue out of range"
            ),
            Policy::Console => assert_eq!(invalid.unwrap_err(), "invalid console notify queue"),
            Policy::Block | Policy::Network => assert_eq!(invalid.unwrap(), Effect::None),
        }
        let early = t.write(0x50, 0);
        if policy == Policy::Console {
            assert_eq!(early.unwrap_err(), "console notify before DRIVER_OK");
        } else {
            assert_eq!(early.unwrap(), Effect::Notify(0));
        }
        let features = t.write(0x70, 12);
        if policy == Policy::Console {
            assert_eq!(
                features.unwrap_err(),
                "console DRIVER_OK without negotiated features"
            );
            assert_eq!(t.read(0x70), 4); // Existing error occurs after recording/clearing bits.
        } else {
            features.unwrap();
            assert_eq!(t.read(0x70), 12);
        }
        for (offset, value) in [
            (0x20, 2),
            (0x24, 1),
            (0x20, 1),
            (0x24, 2),
            (0x20, 999),
            (0x70, 15),
        ] {
            t.write(offset, value).unwrap();
        }
        assert_eq!(t.read(0x70), 15);
        assert_eq!(t.write(0x50, 0).unwrap(), Effect::Notify(0));
        t.isr = 3;
        t.write(0x64, 1).unwrap();
        assert_eq!(t.read(0x60), 2);
        t.write(0x64, 2).unwrap();
        assert_eq!(t.read(0x60), 0);
    }
}

#[test]
fn retained_projection_and_cloned_registers_have_stable_bytes() {
    let mut t = Transport::new(
        Spec {
            device_id: 2,
            features: [512, 1],
        },
        1,
        Policy::Application,
    );
    for (offset, value) in [
        (0x14, 1),
        (0x20, 512),
        (0x24, 1),
        (0x20, 1),
        (0x38, 8),
        (0x80, 4096),
        (0x84, 1),
        (0x90, 8192),
        (0x94, 1),
        (0xa0, 12288),
        (0xa4, 1),
        (0x44, 1),
        (0x70, 15),
    ] {
        t.write(offset, value).unwrap();
    }
    t.queues[0].last_avail = 7;
    t.queues[0].used_idx = 9;
    t.isr = 3;
    let expected = r#"{"dev_sel":1,"drv_sel":1,"features":[512,1],"queue_sel":0,"queues":[{"num":8,"ready":true,"desc":4294971392,"avail":4294975488,"used":4294979584,"last_avail":7,"used_idx":9}],"status":15,"isr":3}"#;
    assert_eq!(serde_json::to_string(&t.projection()).unwrap(), expected);
    let saved = t.clone();
    t.queues[0].last_avail = 99;
    t.write(0x64, 3).unwrap();
    assert_eq!(
        serde_json::to_string(&saved.projection()).unwrap(),
        expected
    );
    assert_eq!(t.write(0x70, 0).unwrap(), Effect::Reset);
    assert_eq!(t.read(8), 2);
    assert_eq!(t.read(0x10), 512);
    assert_eq!(t.read(0x34), 256);
    assert_eq!(t.read(0x70), 0);
    assert_eq!(t.read(0x60), 0);
    assert_eq!(t.queues.len(), 1);
    assert_eq!(
        (
            t.queues[0].desc,
            t.queues[0].last_avail,
            t.queues[0].used_idx
        ),
        (0, 0, 0)
    );
}
