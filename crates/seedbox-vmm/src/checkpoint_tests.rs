//! Runtime checkpoint controls, using the canonical world coordinator.
use super::*;

fn world() -> World {
    let kvm = kvm_ioctls::Kvm::new().unwrap();
    // Increment a RAM marker on every counted loop iteration, then halt.
    let code = [
        0xb9, 0x10, 0x27, 0, 0, 0xff, 0x05, 0, 0, 8, 0, 0x49, 0x75, 0xf7, 0xf4,
    ];
    let mut kernel = vec![0u8; 1024];
    kernel[0x1f1] = 1;
    kernel.extend(code);
    let boot = BootInputs {
        kernel: kernel.into(),
        initrd: Rc::new(Vec::new().into()),
        cmdline: (String::new(), String::new()),
        mem: 0x0810_0000,
        mac: None,
        control_channel: true,
        disk: Some(disk::Disk::new(None, Some(8)).unwrap()),
        tolerance: 0,
        timer_jitter: 0,
        exact_timer_ppm: 0,
        console_limit: None,
        page_timer: false,
        log_chunk_limit: None,
        deferred_tx: false,
    };
    let names = vec!["a".to_string(), "b".to_string()];
    let machines = names
        .iter()
        .map(|name| boot_machine(&kvm, name, &boot, false, [0; 32]).unwrap())
        .collect();
    World {
        failure: None,
        machines,
        factory: kvm,
        boot: vec![boot.clone(), boot],
        barrier: false,
        boot_seed: 7,
        run_seed: 7,
        branch_identity: None,
        incarnations: vec![vec![], vec![]],
        paused_ingress: vec![0; 2],
        clock: PvClock::new(std::time::Duration::from_millis(20)).unwrap(),
        events: vec![BTreeMap::new(); 2],
        strict_events: vec![BTreeMap::new(); 2],
        seq: 0,
        lookahead: 100,
        early: 0,
        latency: 100,
        partitioned: BTreeMap::new(),
        faults: boot_faults(
            &names,
            &[],
            100,
            links::Links::from_config(7, &names, &json!({"latency_ns":100,"jitter_ns":10}))
                .unwrap(),
        )
        .unwrap(),
        control: control::Control::new(&Value::Null, names, 100, 0).unwrap(),
        macs: vec![[0, 0, 0, 0, 0, 1], [0, 0, 0, 0, 0, 2]],
        dropped: 0,
        delivered: 0,
        link_counts: BTreeMap::new(),
        semantic_counts: network_counters::NetworkCounters::default(),
        slices: 0,
    }
}

#[test]
#[cfg_attr(not(feature = "guest-tests"), ignore = "requires guest-tests")]
fn runtime_checkpoint_getrandom_and_urandom_repeat_and_diverge() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut w = world();
    let mut boot = w.boot[0].clone();
    let input = |key: &str, default: &str| {
        std::env::var_os(key)
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| root.join(default))
    };
    boot.kernel = std::fs::read(input("SEEDBOX_TEST_KERNEL", "artifacts/bzImage"))
        .unwrap()
        .into();
    boot.initrd = Rc::new(
        std::fs::read(input("SEEDBOX_TEST_INITRD", "artifacts/entropy-fork.cpio"))
            .unwrap()
            .into(),
    );
    boot.mem = 256 * 1024 * 1024;
    boot.disk = None;
    boot.control_channel = false;
    boot.cmdline=("console=ttyS0 nokaslr noapic nolapic acpi=off io_delay=none tsc=reliable tsc_early_khz=1000000 lpj=1000000 clocksource=tsc random.trust_cpu=off random.trust_bootloader=on panic=-1 loglevel=4 rdinit=/init".into(),String::new());
    w.boot = vec![boot.clone(), boot.clone()];
    let names = vec!["a".to_string(), "b".to_string()];
    w.machines = names
        .iter()
        .map(|name| {
            boot_machine(
                &w.factory,
                name,
                &boot,
                false,
                entropy_seed(7, &format!("boot-entropy/{name}")),
            )
            .unwrap()
        })
        .collect();
    let at = 2_000_000_000;
    let duration = 10_000_000_000;
    w.lookahead = 1_000_000;
    w.latency = 1_000_000;
    assert_eq!(
        simulate_to(
            &mut w,
            &json!({}),
            duration,
            &None,
            None,
            Some(at),
            &mut None
        )
        .unwrap(),
        "checkpoint"
    );
    for m in &w.machines {
        let prefix = String::from_utf8_lossy(&m.com1.out);
        assert!(prefix.contains("before fork:"));
        assert!(!prefix.contains("after fork:"));
    }
    let saved = WorldSnapshot::take_checkpoint(&mut w).unwrap();
    let mut outputs = Vec::new();
    for seed in [42, 43, 42] {
        saved.restore_checkpoint(&mut w).unwrap();
        let report = reseed_checkpoint(
            &mut w,
            &names,
            7,
            seed,
            &checkpoint::branch_identity("parent", at, seed),
        )
        .unwrap();
        assert!(report.iter().all(|r| r["reseed"] == "time_page"));
        simulate(&mut w, &json!({}), duration, &None, None, &mut None).unwrap();
        outputs.push(
            w.machines
                .iter()
                .map(|m| {
                    assert_eq!(m.guest_exit_code, Some(0));
                    let output = String::from_utf8_lossy(&m.com1.out);
                    let line = output
                        .lines()
                        .find(|l| l.starts_with("after fork:"))
                        .unwrap();
                    let values = line.split_whitespace().skip(2).collect::<Vec<_>>();
                    assert_eq!(values.len(), 2);
                    for (value, source) in values.iter().zip(["getrandom=", "urandom="]) {
                        let hex = value.strip_prefix(source).unwrap();
                        assert_eq!(hex.len(), 32, "empty or short {source} output");
                        assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()));
                    }
                    values.into_iter().map(str::to_owned).collect::<Vec<_>>()
                })
                .collect::<Vec<_>>(),
        );
    }
    assert_eq!(outputs[0], outputs[2]);
    for machine in 0..2 {
        assert_eq!(outputs[0][machine].len(), 2);
        for source in 0..2 {
            assert_ne!(outputs[0][machine][source], outputs[1][machine][source]);
        }
    }
}
