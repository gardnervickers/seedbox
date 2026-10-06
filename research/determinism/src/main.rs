use seedbox_witness::*;
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

fn put(fields: &mut Fields, key: &str, value: u64) {
    fields.insert(key.into(), value.to_le_bytes().to_vec());
}
fn page(gpa: u64, bytes: Vec<u8>) -> Page {
    Page {
        id: PageId {
            machine: 7,
            incarnation: 3,
            slot: 0,
            gpa,
        },
        bytes,
    }
}
fn profile(seed: u64) -> Profile {
    Profile {
        schema:"synthetic-x86-world-landing-v2".into(),
        inputs:BTreeMap::from([
            ("seed".into(),hash(&seed.to_le_bytes())),
            ("lib.rs".into(),hash(include_bytes!("lib.rs"))),
            ("main.rs".into(),hash(include_bytes!("main.rs"))),
            ("Cargo.lock".into(),hash(include_bytes!("../Cargo.lock"))),
            ("shared-env/flake.lock".into(),hash(include_bytes!("../../env/flake.lock"))),
            ("shared-env/flake.nix".into(),hash(include_bytes!("../../env/flake.nix"))),
            ("fixture/time-tie-bounds".into(),hash(b"12 boundaries; dt=10ns; registration sequence ascending; 64-byte synthetic pages; checkpoint=observation only")),
        ]),
        observed:vec!["synthetic CPU register state".into(),"complete synthetic RAM".into(),"complete synthetic world/device state".into()],
        unavailable:vec!["real x86/KVM state: GPR/sregs/debug/MSRs/FPU/XSAVE/XCRS/LAPIC/vCPU-events/MP-state/CPUID/TSC offset".into(),"hidden KVM/MMU/translation/interrupt state, actual Linux/device queues and scenario continuation serialization".into()],
        counter:CounterProfile {source:"synthetic".into(),profile_id:"synthetic-branch-count-v1".into(),width_bits:Some(64),configuration:Fields::from([("semantics".into(),b"fixture branches=7*boundary; not hardware acceptance".to_vec())]),validation:CounterValidation::SyntheticOnly},
    }
}
fn run(seed: u64, mode: &str, cadence: u64, interval: Option<(u64, u64)>) -> Result<Trace, String> {
    let mut witness = Witness::new(profile(seed), cadence, interval)?;
    let mut ram = vec![page(0, vec![0; 64]), page(4096, vec![0; 64])];
    ram.push(Page {
        id: PageId {
            machine: 8,
            incarnation: 1,
            slot: 0,
            gpa: 0,
        },
        bytes: vec![0; 64],
    });
    let mut rng = seed;
    let mut tie_order: Option<Vec<u64>> = None;
    for b in 0..12_u64 {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        if mode == "rng" && b == 5 {
            rng ^= 1;
        }
        let mut dirty = vec![];
        if b == 4 || b == 8 {
            ram[0].bytes[b as usize] = b as u8;
            dirty.push(ram[0].clone());
        }
        if mode == "page" && b == 5 {
            ram[1].bytes[17] = 99;
            dirty.push(ram[1].clone());
        }
        if mode == "omitted-dirty" && b == 5 {
            ram[1].bytes[17] = 99;
        }
        let mut cpu = Fields::new();
        for reg in [
            "rax", "rbx", "rcx", "rdx", "rsi", "rdi", "rbp", "rsp", "r8", "r9", "r10", "r11",
            "r12", "r13", "r14", "r15",
        ] {
            put(&mut cpu, &format!("machine/7/boot/3/cpu/0/{reg}"), 0);
        }
        put(&mut cpu, "machine/7/boot/3/cpu/0/rip", 0x1000 + b * 4);
        put(&mut cpu, "machine/7/boot/3/cpu/0/rflags", 2);
        for register in [
            "rcx",
            "rsi",
            "rdi",
            "interrupt_shadow",
            "pending_exception",
            "dr6",
            "cr2",
        ] {
            put(&mut cpu, &format!("machine/7/boot/3/cpu/0/{register}"), 0);
        }
        // A second inactive machine is still observed at every world boundary.
        for register in [
            "rax", "rbx", "rcx", "rdx", "rsi", "rdi", "rbp", "rsp", "r8", "r9", "r10", "r11",
            "r12", "r13", "r14", "r15", "rip", "rflags",
        ] {
            put(
                &mut cpu,
                &format!("machine/8/boot/1/cpu/0/{register}"),
                if register == "rflags" { 2 } else { 0 },
            );
        }
        if b == 5 && mode == "wrong-rip" {
            put(&mut cpu, "machine/7/boot/3/cpu/0/rip", 0x1001 + b * 4);
        }
        if b == 5 && mode == "wrong-rep" {
            put(&mut cpu, "machine/7/boot/3/cpu/0/rcx", 1);
        }
        if b == 5 && mode == "wrong-rep-rsi" {
            put(&mut cpu, "machine/7/boot/3/cpu/0/rsi", 1);
        }
        if b == 5 && mode == "wrong-rep-rdi" {
            put(&mut cpu, "machine/7/boot/3/cpu/0/rdi", 1);
        }
        if b == 5 && mode == "inactive-cpu" {
            put(&mut cpu, "machine/8/boot/1/cpu/0/rax", 1);
        }
        cpu.insert(
            "machine/7/boot/3/cpu/0/synthetic-vector".into(),
            vec![0; 16],
        );
        if b >= 5 && mode == "clock" {
            put(
                &mut cpu,
                "machine/7/boot/3/cpu/0/rax",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos() as u64,
            );
        }
        if b >= 5 && mode == "environment" {
            cpu.insert(
                "machine/7/boot/3/cpu/0/rax".into(),
                std::env::var("SEEDBOX_OUTSIDE")
                    .unwrap_or_default()
                    .into_bytes(),
            );
        }
        let mut world = Fields::new();
        for (key, value) in [
            ("time/ns", b * 10),
            ("tie/next_registration_sequence", 14),
            ("request/next_id", 102),
            ("machine/7/incarnation", 3),
            ("scenario/continuation/pc", b),
            ("seed/stream/network/state", rng),
            ("seed/stream/network/draws", b + 1),
            ("seed/stream/faults/state", seed),
            ("seed/stream/faults/draws", 0),
            ("fault/9/handle", 9),
            ("fault/9/owner/scenario", 0),
            ("fault/9/selector/az", 2),
            ("fault/9/enabled", u64::from(b < 8)),
            ("event/11/time_ns", 1000),
            ("event/11/registration_sequence", 11),
            ("event/11/completion/machine", 7),
            (
                "event/11/completion/incarnation",
                if mode == "pending" && b >= 5 { 4 } else { 3 },
            ),
            ("event/11/completion/request", 101),
            ("event/11/device/incarnation", 2),
            ("event/11/block/offset", 4096),
            ("device/2/queue/used_index", 0),
            ("fabric/tx/request/101/owner/machine", 7),
            ("fabric/tx/request/101/owner/incarnation", 3),
            ("fabric/delivery/12/time_ns", 1000),
            ("fabric/delivery/12/registration_sequence", 12),
            ("fabric/delivery/12/destination/machine", 8),
            ("fabric/delivery/12/destination/incarnation", 1),
        ] {
            put(&mut world, key, value);
        }
        world.insert("event/11/block/operation".into(), b"write".to_vec());
        world.insert("application/output".into(), b"OK".to_vec());
        put(
            &mut world,
            "control/preemption/next_target_count",
            (b + 1) * 7,
        );
        put(&mut world, "control/protocol/next_message_id", b + 102);
        world.insert(
            "control/protocol/phase".into(),
            b"stopped; response-ready".to_vec(),
        );
        world.insert(
            "control/response/101/payload".into(),
            if b == 5 && mode == "control-protocol" {
                b"wrong".to_vec()
            } else {
                b"ready".to_vec()
            },
        );
        world.insert("event/11/block/data".into(), vec![1, 2, 3]);
        world.insert("device/2/volatile_cache".into(), vec![5, 6]);
        world.insert("device/2/durable_bytes".into(), vec![0, 0]);
        world.insert("fabric/tx/request/101/frame".into(), vec![0xde, 0xad]);
        world.insert(
            "fabric/tx/request/101/status".into(),
            b"complete; delivery pending".to_vec(),
        );
        if b == 6 {
            tie_order = Some(if mode == "unordered" {
                let map: HashMap<_, _> = (0..16_u64).map(|id| (id, id)).collect();
                map.keys().copied().collect()
            } else {
                (0..16).collect()
            });
        }
        if let Some(order) = &tie_order {
            world.insert(
                "event/ties/delivery_order".into(),
                serde_json::to_vec(order).unwrap(),
            );
        }
        let request = witness.request();
        let layout = ram.iter().map(|p| (p.id.key(), p.bytes.len())).collect();
        let active = CpuId {
            machine: 7,
            incarnation: 3,
            vcpu: 0,
        };
        let rip = u64::from_le_bytes(
            cpu["machine/7/boot/3/cpu/0/rip"]
                .clone()
                .try_into()
                .unwrap(),
        );
        let registers: Fields = [
            "rcx",
            "rsi",
            "rdi",
            "rflags",
            "interrupt_shadow",
            "pending_exception",
            "dr6",
            "cr2",
        ]
        .iter()
        .map(|r| {
            (
                r.to_string(),
                cpu[&format!("machine/7/boot/3/cpu/0/{r}")].clone(),
            )
        })
        .collect();
        let landing = LandingIdentity {
            count: BranchCount {
                source: "synthetic".into(),
                profile_id: "synthetic-branch-count-v1".into(),
                value: Some(b * 7 + u64::from(b == 5 && mode == "wrong-count")),
                validity: CountValidity::Valid,
                overflow: OverflowState::None,
                epoch: 3,
            },
            rip,
            registers,
        };
        let kind = if b == 0 {
            BoundaryKind::Lifecycle {
                tag: "initial".into(),
            }
        } else if b == 4 || (b == 5 && mode == "wrong-kind") {
            BoundaryKind::NaturalVmExit {
                reason: 2,
                semantic_tag: "PIO-completed".into(),
            }
        } else if b == 8 {
            BoundaryKind::CooperativeYield {
                tag: "fallback".into(),
            }
        } else {
            BoundaryKind::PrecisePreemption
        };
        if mode == "correction-noise" {
            for correction in 0..3 {
                let mut transient = landing.clone();
                transient.rip += correction;
                witness.record_correction(CorrectionObservation {
                    cpu: active.clone(),
                    phase: format!("single-step-{correction}"),
                    landing: transient,
                });
            }
            assert_eq!(witness.request().boundary, b);
        }
        witness.observe(Observation {
            identity: BoundaryIdentity {
                kind,
                cpu: active,
                landing,
            },
            cpu_inventory: vec![
                CpuId {
                    machine: 7,
                    incarnation: 3,
                    vcpu: 0,
                },
                CpuId {
                    machine: 8,
                    incarnation: 1,
                    vcpu: 0,
                },
            ],
            cpu,
            world,
            layout,
            dirty,
            full: request.full_memory.then(|| ram.clone()),
        })?;
    }
    Ok(witness.trace)
}
fn load(path: &Path) -> Trace {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}
fn save<T: serde::Serialize>(path: &Path, v: &T) {
    fs::write(path, serde_json::to_vec_pretty(v).unwrap()).unwrap();
}
fn fresh(
    dir: &Path,
    name: &str,
    mode: &str,
    outside: &str,
    detail: Option<(u64, u64)>,
    cadence: u64,
) -> PathBuf {
    let path = dir.join(format!("{name}.json"));
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "run",
            "42",
            mode,
            &cadence.to_string(),
            &detail
                .map(|(a, b)| format!("{a}:{b}"))
                .unwrap_or("none".into()),
        ])
        .arg(&path)
        .env("SEEDBOX_OUTSIDE", outside);
    let output = command.output().unwrap();
    save(
        &dir.join(format!("{name}.process.json")),
        &serde_json::json!({"args":format!("{command:?}"),"exit":output.status.code(),"stdout":String::from_utf8_lossy(&output.stdout),"stderr":String::from_utf8_lossy(&output.stderr)}),
    );
    assert!(
        output.status.success(),
        "{name}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    path
}
fn twice(
    dir: &Path,
    left_mode: &str,
    right_mode: &str,
    left_outside: &str,
    right_outside: &str,
    cadence: u64,
) -> Investigation {
    fs::create_dir_all(dir).unwrap();
    let mut call = 0;
    let result = investigate(cadence, |right, config| {
        let path = fresh(
            dir,
            &format!("fresh-{call}"),
            if right { right_mode } else { left_mode },
            if right { right_outside } else { left_outside },
            config.detail_interval,
            config.checkpoint_every,
        );
        call += 1;
        Ok(load(&path))
    })
    .unwrap();
    save(&dir.join("investigation.json"), &result);
    result
}
fn suite(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    let a = fresh(dir, "baseline-a", "baseline", "north", None, 3);
    let b = fresh(dir, "baseline-b", "baseline", "south", None, 3);
    let left = load(&a);
    let right = load(&b);
    assert!(compare(&left, &right).is_none());
    let mut results = vec![
        serde_json::json!({"test":"fresh baseline + actual environment perturbation","match":true}),
    ];
    let cadence = fresh(dir, "baseline-cadence-1", "baseline", "south", None, 1);
    assert!(compare(&left, &load(&cadence)).is_none());
    results.push(serde_json::json!({"test":"checkpoint cadence does not alter canonical chain","match":true}));
    let noise = fresh(
        dir,
        "correction-noise",
        "correction-noise",
        "north",
        None,
        3,
    );
    let noisy = load(&noise);
    assert!(compare(&left, &noisy).is_none());
    assert_eq!(noisy.corrections.len(), 36);
    assert_eq!(noisy.boundaries.len(), 12);
    assert_eq!(
        left.boundaries.last().unwrap().world,
        noisy.boundaries.last().unwrap().world
    );
    results.push(serde_json::json!({"test":"36 intermediate debug correction exits add zero committed boundaries/world charges","match":true,"diagnostic_corrections":36,"committed_boundaries":12}));
    for (mode, category, first) in [
        ("pending", "world", 5),
        ("rng", "world", 5),
        ("page", "page", 5),
        ("environment", "register/CPU", 5),
        ("wrong-count", "branch-count", 5),
        ("wrong-rip", "landing/RIP", 5),
        ("wrong-rep", "landing/register", 5),
        ("wrong-rep-rsi", "landing/register", 5),
        ("wrong-rep-rdi", "landing/register", 5),
        ("wrong-kind", "boundary-kind", 5),
        ("inactive-cpu", "register/CPU", 5),
        ("control-protocol", "world", 5),
    ] {
        let base = if mode == "environment" {
            fresh(dir, "environment-north", mode, "north", None, 3)
        } else {
            a.clone()
        };
        let changed = fresh(dir, &format!("{mode}-south"), mode, "south", None, 3);
        let x = load(&base);
        let y = load(&changed);
        let diff = compare(&x, &y).unwrap();
        assert_eq!(diff.boundary, Some(first));
        assert_eq!(diff.category, category);
        assert_eq!(
            x.boundaries[first as usize].world["application/output"],
            y.boundaries[first as usize].world["application/output"]
        );
        if mode == "wrong-count" {
            assert_eq!(
                x.boundaries[first as usize].cpu,
                y.boundaries[first as usize].cpu
            );
            assert_eq!(
                x.boundaries[first as usize].pages,
                y.boundaries[first as usize].pages
            );
        }
        if mode.starts_with("wrong-rep") {
            assert_eq!(
                x.boundaries[first as usize].identity.landing.count,
                y.boundaries[first as usize].identity.landing.count
            );
            assert_eq!(
                x.boundaries[first as usize].identity.landing.rip,
                y.boundaries[first as usize].identity.landing.rip
            );
        }
        if mode == "pending" || mode == "rng" {
            assert_eq!(
                x.boundaries[first as usize].cpu,
                y.boundaries[first as usize].cpu
            );
            assert_eq!(
                x.boundaries[first as usize].pages,
                y.boundaries[first as usize].pages
            );
        }
        // Start fresh at boundary zero. Only the output detail interval derives
        // from the comparison. No previous trace/checkpoint is an execution input.
        let interval = Some((first / 3 * 3, first + 1));
        let xd = fresh(
            dir,
            &format!("{mode}-detail-left"),
            if mode == "environment" {
                mode
            } else {
                "baseline"
            },
            "north",
            interval,
            3,
        );
        let yd = fresh(
            dir,
            &format!("{mode}-detail-right"),
            mode,
            "south",
            interval,
            3,
        );
        let xx = load(&xd);
        let yy = load(&yd);
        assert!(compare(&x, &xx).is_none());
        assert!(compare(&y, &yy).is_none());
        let detailed = compare(&xx, &yy).unwrap();
        assert!(detailed.left_bytes.is_some());
        assert!(detailed.right_bytes.is_some());
        results.push(serde_json::json!({"test":mode,"compact_difference":diff,"detail_difference":detailed,"rerun_from_zero":true,"cpu_ram_equal_at_divergence":mode=="pending"||mode=="rng"}));
    }
    let ca = fresh(dir, "clock-a", "clock", "north", None, 3);
    let cb = fresh(dir, "clock-b", "clock", "north", None, 3);
    let clock = compare(&load(&ca), &load(&cb)).unwrap();
    assert_eq!(clock.boundary, Some(5));
    results.push(serde_json::json!({"test":"actual SystemTime clock leak across fresh processes","difference":clock}));
    let ua = fresh(dir, "unordered-a", "unordered", "north", None, 3);
    let ub = fresh(dir, "unordered-b", "unordered", "north", None, 3);
    let unordered = compare(&load(&ua), &load(&ub)).unwrap();
    assert_eq!(unordered.boundary, Some(6));
    assert_eq!(unordered.field, "event/ties/delivery_order");
    results.push(
        serde_json::json!({"test":"actual randomized HashMap iteration","difference":unordered}),
    );
    let omission = run(42, "omitted-dirty", 3, None).unwrap_err();
    assert!(omission.contains("checkpoint 6"));
    results.push(serde_json::json!({"test":"dirty-log omission audit","error":omission,"true_mutation_boundary":5,"detected_boundary":6,"localization_limit":"first audited checkpoint; must rerun full RAM each boundary to localize"}));
    let full_omission = run(42, "omitted-dirty", 1, None).unwrap_err();
    assert!(full_omission.contains("checkpoint 5"));
    results.push(serde_json::json!({"test":"dirty-log omission full-every-boundary","error":full_omission,"detected_boundary":5}));
    let automatic = twice(
        &dir.join("automatic"),
        "baseline",
        "pending",
        "north",
        "south",
        3,
    );
    assert_eq!(
        automatic.first_difference.as_ref().unwrap().boundary,
        Some(5)
    );
    assert_eq!(automatic.rerun_reproduced_original_chains, Some(true));
    assert!(
        automatic
            .detailed_difference
            .as_ref()
            .unwrap()
            .left_bytes
            .is_some()
    );
    results.push(serde_json::json!({"test":"public automatic investigate API with fresh subprocesses","first_difference":automatic.first_difference,"detailed_difference":automatic.detailed_difference,"rerun_reproduced_original_chains":automatic.rerun_reproduced_original_chains}));
    save(&dir.join("results.json"), &results);
    println!(
        "{} checks passed; results={}",
        results.len(),
        dir.join("results.json").display()
    );
}
fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("suite") => suite(Path::new(&args[2])),
        Some("twice") => {
            let result = twice(
                Path::new(&args[2]),
                &args[3],
                &args[4],
                &args[5],
                &args[6],
                args[7].parse().unwrap(),
            );
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &result
                        .detailed_difference
                        .as_ref()
                        .or(result.first_difference.as_ref())
                )
                .unwrap()
            );
            if result.first_difference.is_some() {
                std::process::exit(1);
            }
        }
        Some("run") => {
            let seed = args[2].parse().unwrap();
            let cadence = args[4].parse().unwrap();
            let interval = if args[5] == "none" {
                None
            } else {
                let (a, b) = args[5].split_once(':').unwrap();
                Some((a.parse().unwrap(), b.parse().unwrap()))
            };
            match run(seed, &args[3], cadence, interval) {
                Ok(trace) => save(Path::new(&args[6]), &trace),
                Err(error) => {
                    eprintln!("{error}");
                    std::process::exit(1);
                }
            }
        }
        Some("compare") => {
            let difference = compare(&load(Path::new(&args[2])), &load(Path::new(&args[3])));
            println!("{}", serde_json::to_string_pretty(&difference).unwrap());
            if difference.is_some() {
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!(
                "usage: seedbox-witness suite DIR | run SEED MODE CADENCE START:END|none OUT | compare LEFT RIGHT | twice DIR LEFT-MODE RIGHT-MODE LEFT-OUTSIDE RIGHT-OUTSIDE CADENCE (seed 42)"
            );
            std::process::exit(2);
        }
    }
}
