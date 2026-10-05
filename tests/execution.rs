use seedbox::{
    backend::{Process, StopReason},
    config::{self, NodeSpec, Partition, RunConfig},
    fixtures,
    random::RandomStream,
    runtime::{Simulation, canonical},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, path::PathBuf, process::Command, sync::OnceLock};
use unicorn_engine::RegisterX86 as Reg;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}
fn guests() -> &'static BTreeMap<String, Vec<u8>> {
    static GUESTS: OnceLock<BTreeMap<String, Vec<u8>>> = OnceLock::new();
    GUESTS.get_or_init(|| {
        let dir = fixtures::build_guests(&root()).unwrap();
        fs::read_dir(dir)
            .unwrap()
            .map(|p| p.unwrap().path())
            .filter(|p| p.extension().is_none())
            .map(|p| {
                (
                    p.file_name().unwrap().to_string_lossy().into_owned(),
                    fs::read(p).unwrap(),
                )
            })
            .collect()
    })
}
fn node(image: Vec<u8>, name: &str, ip: &str) -> NodeSpec {
    NodeSpec {
        name: name.into(),
        image,
        ip: ip.parse().unwrap(),
        argv: vec!["/app".into()],
        files: BTreeMap::new(),
    }
}
fn app(image: Vec<u8>) -> NodeSpec {
    node(image, "app", "10.0.0.1")
}
fn guest(name: &str) -> Vec<u8> {
    guests()[name].clone()
}
fn simulate(specs: Vec<NodeSpec>, seed: u64, config: RunConfig) -> Value {
    Simulation::new(specs, seed, config, false)
        .unwrap()
        .run()
        .unwrap()
}
fn stdout(report: &Value, name: &str) -> Vec<u8> {
    hex::decode(report["nodes"][name]["stdout_hex"].as_str().unwrap()).unwrap()
}
fn counter(report: &Value, name: &str) -> u32 {
    u32::from_le_bytes(stdout(report, name).try_into().unwrap())
}
fn cluster(seed: u64) -> Value {
    guests();
    let (config, specs) = config::load_manifest(&root().join("examples/cluster.json")).unwrap();
    simulate(specs, seed, config)
}
fn assemble(body: &str, data: &str, bss: &str) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("guest.asm");
    let output = dir.path().join("guest");
    fs::write(&source, format!("bits 64\nglobal _start\nsection .text\n_start:\n{body}\nsection .data\n{data}\nsection .bss\n{bss}\n")).unwrap();
    fixtures::assemble(&source, &output, false).unwrap();
    fs::read(output).unwrap()
}
fn assert_golden(report: &Value, golden: &str) {
    let golden: Value = serde_json::from_str(golden).unwrap();
    for (key, value) in golden.as_object().unwrap() {
        assert_eq!(&report[key], value, "Python/Rust behavior differs in {key}");
    }
}

fn exact_budget(budget: u64) {
    let mut process = Process::new(
        "spin",
        &guest("spin"),
        &mut RandomStream::new(42, "entropy:spin"),
        &["/app".into()],
        1,
    )
    .unwrap();
    let stop = process.run(1, budget).unwrap();
    assert_eq!(stop.reason, StopReason::Quantum);
    assert_eq!(stop.steps, budget);
    assert_eq!(stop.entered.len() as u64, budget);
    assert_eq!(process.register(Reg::RAX).unwrap(), budget.div_ceil(2));
    assert_eq!(process.run(1, budget).unwrap().steps, budget);
    assert_eq!(process.register(Reg::RAX).unwrap(), budget);
}
macro_rules! budgets { ($($name:ident => $budget:expr),+) => { $(#[test] fn $name() { exact_budget($budget); })+ }; }
budgets!(cpu_budget_1 => 1, cpu_budget_2 => 2, cpu_budget_3 => 3, cpu_budget_7 => 7, cpu_budget_17 => 17, cpu_budget_64 => 64);

#[test]
fn syscall_stop_and_completion() {
    let mut process = Process::new(
        "hello",
        &guest("hello"),
        &mut RandomStream::new(0, "test"),
        &["/app".into()],
        1,
    )
    .unwrap();
    let stop = process.run(1, 100).unwrap();
    assert_eq!(stop.reason, StopReason::Syscall);
    assert_eq!(stop.steps, 5);
    let req = stop.request.unwrap();
    assert_eq!(req.number, 1);
    assert_eq!(process.register(Reg::RIP).unwrap(), req.pc);
    process.complete(1, &req, 6).unwrap();
    let stop = process.run(1, 100).unwrap();
    assert_eq!(stop.request.unwrap().number, 60);
    assert_eq!(stop.steps, 3);
}

fn tcp_world() -> Vec<NodeSpec> {
    vec![
        node(guest("tcp_client"), "client", "10.0.0.1"),
        node(guest("tcp_server"), "server", "10.0.0.2"),
    ]
}
#[test]
fn tcp_fifo_partial_reads_epoll_and_exit_eof() {
    let mut config = RunConfig::default();
    config.network.jitter_ns = 50_000;
    for seed in 0..16 {
        let report = simulate(tcp_world(), seed, config.clone());
        assert_eq!(report["status"], "completed", "{}", report["error"]);
        assert_eq!(stdout(&report, "client"), b"pong");
        assert_eq!(stdout(&report, "server"), b"ping");
        if seed == 0 {
            assert_eq!(
                canonical(&report),
                canonical(&simulate(tcp_world(), seed, config.clone()))
            );
        }
    }
}
#[test]
fn tcp_transport_failure_resets_both_peers() {
    let mut config = RunConfig::default();
    config.network.loss_ppm = 1_000_000;
    let report = simulate(tcp_world(), 42, config.clone());
    assert_eq!(report["status"], "guest_failure", "{}", report["error"]);
    assert!(
        report["trace"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["event"] == "tcp_deliver" && e["reset"] == true)
    );
    assert_eq!(
        canonical(&report),
        canonical(&simulate(tcp_world(), 42, config))
    );
}
#[test]
fn anonymous_mapping_tls_protection_and_invalidated_pointer() {
    let image = assemble(
        "mov eax,9\nxor edi,edi\nmov esi,4096\nmov edx,3\nmov r10d,0x22\nmov r8,-1\nxor r9d,r9d\nsyscall\nmov r12,rax\nmov dword [r12],0x12345678\nmov eax,158\nmov edi,0x1002\nmov rsi,r12\nsyscall\nmov eax,[fs:0]\ncmp eax,0x12345678\njne fail\nmov eax,10\nmov rdi,r12\nmov esi,4096\nmov edx,1\nsyscall\nmov eax,1\nmov edi,1\nmov rsi,r12\nmov edx,4\nsyscall\nmov eax,11\nmov rdi,r12\nmov esi,4096\nsyscall\nmov eax,1\nmov edi,1\nmov rsi,r12\nmov edx,4\nsyscall\ncmp rax,-14\njne fail\nmov eax,60\nxor edi,edi\nsyscall\nfail:\nmov eax,60\nmov edi,1\nsyscall",
        "",
        "",
    );
    let report = simulate(vec![app(image)], 42, RunConfig::default());
    assert_eq!(report["status"], "completed", "{}", report["error"]);
    assert_eq!(counter(&report, "app"), 0x12345678);
}
#[test]
fn poll_timeout_ignores_negative_fds() {
    let image = assemble(
        "mov eax,7\nlea rdi,[rel fds]\nmov esi,1\nmov edx,7\nsyscall\ntest rax,rax\njnz fail\ncmp word [rel fds+6],0\njne fail\nmov eax,228\nmov edi,1\nlea rsi,[rel time]\nsyscall\nmov eax,1\nmov edi,1\nlea rsi,[rel time+8]\nmov edx,8\nsyscall\nmov eax,60\nxor edi,edi\nsyscall\nfail:\nmov eax,60\nmov edi,1\nsyscall",
        "fds: dd -1\ndw 1,1234",
        "time: resb 16",
    );
    let report = simulate(vec![app(image)], 42, RunConfig::default());
    assert_eq!(report["status"], "completed");
    let ns = u64::from_le_bytes(stdout(&report, "app").try_into().unwrap());
    assert!((7_000_000..7_010_000).contains(&ns));
}

#[test]
fn udp_packet_arrival_wakes_poll_before_timeout() {
    let image = assemble(
        "mov eax,41\nmov edi,2\nmov esi,2\nxor edx,edx\nsyscall\nmov r12,rax\nmov eax,49\nmov rdi,r12\nlea rsi,[rel address]\nmov edx,16\nsyscall\nmov eax,44\nmov rdi,r12\nlea rsi,[rel payload]\nmov edx,4\nxor r10d,r10d\nlea r8,[rel address]\nmov r9d,16\nsyscall\nmov [rel pollfd],r12d\nmov eax,7\nlea rdi,[rel pollfd]\nmov esi,1\nmov edx,1000\nsyscall\ncmp rax,1\njne fail\ntest word [rel pollfd+6],1\njz fail\nmov eax,0\nmov rdi,r12\nlea rsi,[rel buffer]\nmov edx,4\nsyscall\ncmp rax,4\njne fail\nmov eax,1\nmov edi,1\nlea rsi,[rel buffer]\nmov edx,4\nsyscall\nmov eax,60\nxor edi,edi\nsyscall\nfail:\nmov eax,60\nmov edi,1\nsyscall",
        "address: dw 2,0x2823\ndb 10,0,0,1\ndq 0\npayload: db 'ping'\npollfd: dd 0\ndw 1,0",
        "buffer: resb 4",
    );
    let mut config = RunConfig::default();
    config.network.latency_ns = 10_000_000;
    let report = simulate(vec![app(image)], 42, config);
    assert_eq!(report["status"], "completed", "{}", report["error"]);
    assert_eq!(stdout(&report, "app"), b"ping");
    assert!(report["virtual_time_ns"].as_u64().unwrap() < 20_000_000);
}
#[test]
fn pipe_nonblocking_empty_poll_wakeup_and_close_eof() {
    let image = assemble(
        "mov eax,293\nlea rdi,[rel fds]\nmov esi,0x800\nsyscall\nmov eax,0\nmov edi,[rel fds]\nlea rsi,[rel buffer]\nmov edx,1\nsyscall\ncmp rax,-11\njne fail\nmov eax,1\nmov edi,[rel fds+4]\nlea rsi,[rel payload]\nmov edx,1\nsyscall\nmov edi,[rel fds]\nmov [rel pollfd],edi\nmov eax,7\nlea rdi,[rel pollfd]\nmov esi,1\nmov edx,100\nsyscall\ncmp rax,1\njne fail\nmov eax,0\nmov edi,[rel fds]\nlea rsi,[rel buffer]\nmov edx,1\nsyscall\ncmp rax,1\njne fail\ncmp byte [rel buffer],0x5a\njne fail\nmov eax,3\nmov edi,[rel fds+4]\nsyscall\nmov eax,0\nmov edi,[rel fds]\nlea rsi,[rel buffer]\nmov edx,1\nsyscall\ntest rax,rax\njnz fail\nmov eax,60\nxor edi,edi\nsyscall\nfail:\nmov eax,60\nmov edi,1\nsyscall",
        "payload: db 0x5a\npollfd: dd 0\ndw 1,0",
        "fds: resb 8\nbuffer: resb 1",
    );
    let report = simulate(vec![app(image)], 42, RunConfig::default());
    assert_eq!(report["status"], "completed", "{}", report["error"]);
}
#[test]
fn rep_copy_preempts_and_resumes() {
    let image = assemble(
        "lea rsi,[rel source]\nlea rdi,[rel target]\nmov ecx,64\nrep movsb\nmov eax,60\nxor edi,edi\nsyscall",
        "source: times 64 db 0x5a",
        "target: resb 64",
    );
    let mut process = Process::new(
        "copy",
        &image,
        &mut RandomStream::new(0, "copy"),
        &["/app".into()],
        1,
    )
    .unwrap();
    assert_eq!(process.run(1, 4).unwrap().reason, StopReason::Quantum);
    let remaining = process.register(Reg::RCX).unwrap();
    assert!(remaining > 0 && remaining < 64);
    let target = process.register(Reg::RDI).unwrap() - (64 - remaining);
    assert_eq!(process.run(1, 4).unwrap().reason, StopReason::Quantum);
    assert_eq!(process.register(Reg::RCX).unwrap(), remaining - 4);
    let stop = process.run(1, 100).unwrap();
    assert_eq!(stop.request.unwrap().number, 60);
    assert_eq!(process.read(target, 64).unwrap(), vec![0x5a; 64]);
}
#[test]
fn fresh_runs_regenerate_entire_cluster() {
    let first = cluster(42);
    assert_eq!(first["status"], "completed");
    assert_eq!(canonical(&first), canonical(&cluster(42)));
    assert_eq!(canonical(&first), canonical(&cluster(42)));
    assert_eq!(stdout(&first, "client"), b"pong");
    assert_eq!(stdout(&first, "server"), b"ping");
    assert_golden(&first, include_str!("golden/cluster-42.json"));
}
#[test]
fn seeds_explore_different_executions() {
    let (first, second) = (cluster(1), cluster(2));
    assert_eq!(first["status"], "completed");
    assert_eq!(second["status"], "completed");
    assert_ne!(first["trace_sha256"], second["trace_sha256"]);
    assert_ne!(first["instruction_sha256"], second["instruction_sha256"]);
}
#[test]
fn race_and_atomic_control() {
    let mut config = RunConfig::default();
    config.scheduler.quantum_min = 1;
    config.scheduler.quantum_max = 7;
    let mut values = Vec::new();
    for seed in 0..64 {
        let broken = simulate(vec![app(guest("race"))], seed, config.clone());
        let fixed = simulate(vec![app(guest("atomic"))], seed, config.clone());
        assert_eq!(broken["status"], "completed");
        assert_eq!(fixed["status"], "completed");
        values.push(counter(&broken, "app"));
        assert_eq!(counter(&fixed, "app"), 64);
        assert_eq!(
            fixed["nodes"]["app"]["threads"].as_object().unwrap().len(),
            2
        );
    }
    assert!(values.iter().any(|v| *v != 64));
    assert!(values.iter().any(|v| *v != values[0]));
    for (manifest, golden) in [
        ("race", include_str!("golden/race-42.json")),
        ("atomic", include_str!("golden/atomic-42.json")),
    ] {
        let (config, specs) =
            config::load_manifest(&root().join(format!("examples/{manifest}.json"))).unwrap();
        assert_golden(&simulate(specs, 42, config), golden);
    }
}
#[test]
fn packet_deadlines_preempt_cpu_loop() {
    let mut c = RunConfig::default();
    c.scheduler.tick_ns = 7;
    c.scheduler.max_instructions = 10_000;
    c.network.latency_ns = 10_003;
    let report = simulate(
        vec![
            node(guest("client"), "client", "10.0.0.1"),
            node(guest("server"), "server", "10.0.0.2"),
            node(guest("spin"), "spin", "10.0.0.3"),
        ],
        42,
        c,
    );
    assert_eq!(report["status"], "instruction_limit");
    assert_eq!(stdout(&report, "client"), b"pong");
    let times = |kind: &str| {
        report["trace"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["event"] == kind)
            .map(|e| e["time_ns"].as_u64().unwrap())
            .collect::<Vec<_>>()
    };
    let sends = times("packet_send");
    let arrivals = times("packet_deliver");
    assert_eq!(sends.len(), 2);
    assert_eq!(arrivals.len(), 2);
    for (send, arrival) in sends.iter().zip(arrivals) {
        assert_eq!(arrival - send, 10_003);
    }
    assert!(times("advance").is_empty());
}
#[test]
fn loss_is_deterministic_and_deadlocks() {
    guests();
    let (mut c, specs) = config::load_manifest(&root().join("examples/cluster.json")).unwrap();
    c.network.loss_ppm = 1_000_000;
    let first = simulate(specs.clone(), 42, c.clone());
    assert_eq!(first["status"], "deadlock");
    assert_eq!(canonical(&first), canonical(&simulate(specs, 42, c)));
    assert!(
        first["trace"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["event"] == "packet_drop" && e["reason"] == "loss")
    );
    assert!(stdout(&first, "client").is_empty());
}
#[test]
fn in_flight_partition() {
    guests();
    let (mut c, specs) = config::load_manifest(&root().join("examples/cluster.json")).unwrap();
    c.network.jitter_ns = 0;
    c.network.partitions.push(Partition {
        source: "client".into(),
        destination: "server".into(),
        start_ns: 1000,
        end_ns: 1_000_000,
    });
    let report = simulate(specs, 42, c);
    assert_eq!(report["status"], "deadlock");
    let trace = report["trace"].as_array().unwrap();
    let send = trace.iter().find(|e| e["event"] == "packet_send").unwrap();
    let drop = trace.iter().find(|e| e["event"] == "packet_drop").unwrap();
    assert!(send["time_ns"].as_u64().unwrap() < 1000);
    assert!(drop["time_ns"].as_u64().unwrap() >= 1000);
    assert_eq!(drop["reason"], "partition");
}
#[test]
fn fault_choices_preserve_guest_entropy() {
    let normal = cluster(42);
    let (mut c, specs) = config::load_manifest(&root().join("examples/cluster.json")).unwrap();
    c.network.loss_ppm = 1_000_000;
    let failed = simulate(specs, 42, c);
    assert_eq!(
        stdout(&normal, "storage")[5..13],
        stdout(&failed, "storage")[5..13]
    );
}
#[test]
fn disk_durability_and_virtual_sleep() {
    let report = simulate(vec![app(guest("disk"))], 42, RunConfig::default());
    assert_eq!(report["status"], "completed");
    let out = stdout(&report, "app");
    assert_eq!(&out[..5], b"state");
    let words: Vec<_> = out[13..]
        .chunks_exact(8)
        .map(|b| i64::from_le_bytes(b.try_into().unwrap()))
        .collect();
    assert!((words[2] - words[0]) * 1_000_000_000 + words[3] - words[1] >= 1_000_000_000);
    assert_eq!(
        report["nodes"]["app"]["files"]["/state"],
        json!({"contents_hex": hex::encode(b"state"), "durable_hex": hex::encode(b"state")})
    );
    assert!(report["instructions"].as_u64().unwrap() < 100);
}
#[test]
fn failed_disk_operations_have_no_effect() {
    let mut c = RunConfig::default();
    c.disk.error_ppm = 1_000_000;
    let report = simulate(vec![app(guest("disk"))], 42, c);
    assert_eq!(report["status"], "completed");
    assert_eq!(
        report["nodes"]["app"]["files"]["/state"],
        json!({"contents_hex": "", "durable_hex": ""})
    );
    assert_eq!(
        report["trace"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["event"] == "disk_fault")
            .map(|e| e["op"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [1, 74, 0]
    );
}
const PRINT_RESULT: &str = "mov [rel result],eax\nmov eax,1\nmov edi,1\nlea rsi,[rel result]\nmov edx,4\nsyscall\nmov eax,60\nxor edi,edi\nsyscall";
#[test]
fn futex_timeout_uses_linux_errno() {
    let image = assemble(
        &format!(
            "mov eax,202\nlea rdi,[rel futex_word]\nmov esi,128\nxor edx,edx\nlea r10,[rel timeout]\nsyscall\n{PRINT_RESULT}"
        ),
        "futex_word: dd 0\nresult: dd 0\ntimeout: dq 0,1000000",
        "",
    );
    let report = simulate(vec![app(image)], 42, RunConfig::default());
    assert_eq!(report["status"], "completed");
    assert_eq!(counter(&report, "app") as i32, -110);
    let trace = report["trace"].as_array().unwrap();
    let block = trace.iter().find(|e| e["event"] == "block").unwrap();
    let wake = trace.iter().find(|e| e["event"] == "wake").unwrap();
    assert_eq!(wake["time_ns"], block["deadline_ns"]);
    assert_eq!(
        wake["time_ns"].as_u64().unwrap() - block["time_ns"].as_u64().unwrap(),
        1_000_000
    );
}
fn unsupported_instruction(instruction: &str) {
    let report = simulate(
        vec![app(assemble(
            &format!("{instruction}\nmov eax,60\nxor edi,edi\nsyscall"),
            "",
            "",
        ))],
        42,
        RunConfig::default(),
    );
    assert_eq!(report["status"], "unsupported");
    assert!(
        report["error"]
            .as_str()
            .unwrap()
            .contains("unsupported instruction")
    );
}
macro_rules! rejected { ($($name:ident => $instruction:expr),+) => { $(#[test] fn $name() { unsupported_instruction($instruction); })+ }; }
rejected!(reject_rdtsc => "rdtsc", reject_rdtscp => "rdtscp", reject_rdrand => "rdrand eax", reject_rdseed => "rdseed eax", reject_cpuid => "cpuid", reject_sysenter => "sysenter", reject_int80 => "int 0x80");
#[test]
fn unknown_syscall() {
    let report = simulate(
        vec![app(assemble("mov eax,9999\nsyscall", "", ""))],
        42,
        RunConfig::default(),
    );
    assert_eq!(report["status"], "unsupported");
    assert_eq!(report["error"], "unsupported syscall 9999");
}
#[test]
fn bad_pointer_is_linux_efault() {
    let image = assemble(
        &format!("mov eax,228\nmov edi,1\nmov esi,1\nsyscall\n{PRINT_RESULT}"),
        "result: dd 0",
        "",
    );
    let report = simulate(vec![app(image)], 42, RunConfig::default());
    assert_eq!(report["status"], "completed");
    assert_eq!(counter(&report, "app") as i32, -14);
}
#[test]
fn unmapped_load_faults() {
    assert_eq!(
        simulate(
            vec![app(assemble("mov rax,[0x100]", "", ""))],
            42,
            RunConfig::default()
        )["status"],
        "fault"
    );
}
#[test]
fn fault_at_start_of_quantum() {
    let mut c = RunConfig::default();
    c.scheduler.quantum_min = 1;
    c.scheduler.quantum_max = 1;
    let report = simulate(vec![app(assemble("mov eax,0x100\njmp rax", "", ""))], 42, c);
    assert_eq!(report["status"], "fault");
    assert_eq!(report["instructions"], 2);
}
#[test]
fn nonblocking_receive_is_linux_eagain() {
    let image = assemble(
        &format!(
            "mov eax,41\nmov edi,2\nmov esi,0x802\nxor edx,edx\nsyscall\nmov rdi,rax\nmov eax,45\nlea rsi,[rel buffer]\nmov edx,8\nxor r10d,r10d\nxor r8d,r8d\nxor r9d,r9d\nsyscall\n{PRINT_RESULT}"
        ),
        "result: dd 0",
        "buffer: resb 8",
    );
    let report = simulate(vec![app(image)], 42, RunConfig::default());
    assert_eq!(report["status"], "completed");
    assert_eq!(counter(&report, "app") as i32, -11);
}
#[test]
fn zero_write_does_not_extend_file() {
    let image = assemble(
        "mov eax,257\nmov rdi,-100\nlea rsi,[rel path]\nmov edx,0x42\nmov r10d,0o600\nsyscall\nmov r12,rax\nmov eax,8\nmov rdi,r12\nmov esi,100\nxor edx,edx\nsyscall\nmov eax,1\nmov rdi,r12\nxor esi,esi\nxor edx,edx\nsyscall\nmov eax,60\nxor edi,edi\nsyscall",
        "path: db \"/empty\",0",
        "",
    );
    let report = simulate(vec![app(image)], 42, RunConfig::default());
    assert_eq!(report["status"], "completed");
    assert_eq!(
        report["nodes"]["app"]["files"]["/empty"],
        json!({"contents_hex":"", "durable_hex":""})
    );
}
#[test]
fn observation_preserves_execution() {
    let first = simulate(vec![app(guest("race"))], 42, RunConfig::default());
    let detailed = Simulation::new(vec![app(guest("race"))], 42, RunConfig::default(), true)
        .unwrap()
        .run()
        .unwrap();
    assert_eq!(first["instruction_sha256"], detailed["instruction_sha256"]);
    assert_eq!(first["nodes"], detailed["nodes"]);
    let trace = detailed["trace"].as_array().unwrap();
    assert_eq!(
        trace.iter().filter(|e| e["event"] == "instruction").count() as u64,
        detailed["instructions"].as_u64().unwrap()
    );
    assert!(
        trace.windows(2).all(
            |pair| pair[0]["time_ns"].as_u64().unwrap() <= pair[1]["time_ns"].as_u64().unwrap()
        )
    );
}
#[test]
fn fresh_process_reports_are_identical() {
    guests();
    let dir = tempfile::tempdir().unwrap();
    let mut reports = Vec::new();
    for (index, timezone) in ["UTC", "America/New_York"].iter().enumerate() {
        let output = dir.path().join(format!("{index}.json"));
        let run = Command::new(env!("CARGO_BIN_EXE_seedbox"))
            .arg("run")
            .arg(root().join("examples/cluster.json"))
            .args(["--seed", "42", "--output"])
            .arg(&output)
            .env("TZ", timezone)
            .output()
            .unwrap();
        assert!(
            run.status.success(),
            "{}",
            String::from_utf8_lossy(&run.stderr)
        );
        reports.push(fs::read(output).unwrap());
    }
    assert_eq!(reports[0], reports[1]);
}
#[test]
fn random_stream_is_domain_separated_and_chunk_independent() {
    let expected = RandomStream::new(42, "entropy:a").read(100);
    let mut chunks = RandomStream::new(42, "entropy:a");
    let mut result = chunks.read(1);
    result.extend(chunks.read(63));
    result.extend(chunks.read(36));
    assert_eq!(expected, result);
    assert_ne!(expected, RandomStream::new(42, "schedule").read(100));
}
fn bad_config(field: &str) {
    let mut c = RunConfig::default();
    match field {
        "tick" => c.scheduler.tick_ns = 0,
        "min" => c.scheduler.quantum_min = 0,
        "max" => {
            c.scheduler.quantum_min = 5;
            c.scheduler.quantum_max = 4;
        }
        _ => c.scheduler.max_instructions = 0,
    }
    assert!(Simulation::new(vec![app(guest("hello"))], 42, c, false).is_err());
}
macro_rules! invalid { ($($name:ident => $field:expr),+) => { $(#[test] fn $name() { bad_config($field); })+ }; }
invalid!(invalid_tick => "tick", invalid_quantum_min => "min", invalid_quantum_max => "max", invalid_instruction_limit => "limit");

#[test]
fn malformed_elf_is_rejected() {
    assert!(Simulation::new(vec![app(vec![0; 64])], 42, RunConfig::default(), false).is_err());
}
#[test]
fn nonascii_canonical_encoding() {
    assert_eq!(
        String::from_utf8(canonical(&json!({"z": "é😀", "a": 1}))).unwrap(),
        "{\"a\":1,\"z\":\"\\u00e9\\ud83d\\ude00\"}"
    );
}

#[test]
fn full_acceptance_under_host_cpu_load() {
    guests();
    let dir = tempfile::tempdir().unwrap();
    let summary = seedbox::verification::verify(
        &root(),
        std::path::Path::new(env!("CARGO_BIN_EXE_seedbox")),
        64,
        &dir.path().join("verification.json"),
    )
    .unwrap();
    assert_eq!(summary["atomic_fix_pass_count"], 64);
    assert_eq!(summary["race_failure_count"], 64);
    assert_eq!(
        summary["reports_identical_under_two_host_cpu_workers"],
        true
    );
}
