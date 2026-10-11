//! Independent ARM coordinator boundary checks. No native CPU backend is used.
use seedbox_arm64::{application::worker, disk::Disk};
use serde_json::{json, Value};
use std::{
    fs,
    io::Cursor,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

struct Fixture {
    root: PathBuf,
    _temporary: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().to_path_buf();
        Self {
            root,
            _temporary: temporary,
        }
    }
    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
    fn worker(&self, replies: &[Value], keep_open: bool) -> PathBuf {
        let mut wire = Vec::new();
        for reply in replies {
            worker::write_message(&mut wire, reply).unwrap();
        }
        self.raw_worker(&wire, keep_open)
    }
    fn raw_worker(&self, wire: &[u8], keep_open: bool) -> PathBuf {
        fs::write(self.path("responses"), wire).unwrap();
        let script = format!(
            r#"#!/usr/bin/env python3
import os, struct, sys
if sys.argv[1:] == ['--probe']:
    sys.exit(0)
with open({:?}, 'w') as pid:
    pid.write(str(os.getpid()))
with open({:?}, 'wb', buffering=0) as requests:
    header = sys.stdin.buffer.read(4)
    if len(header) == 4:
        size = struct.unpack('<I', header)[0]
        body = sys.stdin.buffer.read(size)
        requests.write(header + body)
    with open({:?}, 'rb') as replies:
        sys.stdout.buffer.write(replies.read())
        sys.stdout.buffer.flush()
    if {}:
        while True:
            data = os.read(sys.stdin.fileno(), 65536)
            if not data:
                break
            requests.write(data)
"#,
            self.path("pid"),
            self.path("requests"),
            self.path("responses"),
            if keep_open { "True" } else { "False" }
        );
        let executable = self.path("worker");
        fs::write(&executable, script).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        // A child forked by a concurrent test may still hold our write descriptor
        // until it execs, so exec fails with ETXTBSY. Nothing reopens the script
        // for writing, so once one exec succeeds every later spawn will too.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match std::process::Command::new(&executable)
                .arg("--probe")
                .status()
            {
                Ok(status) => {
                    assert!(status.success(), "worker probe: {status}");
                    break;
                }
                Err(error)
                    if error.raw_os_error() == Some(libc::ETXTBSY) && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("worker probe {}: {error}", executable.display()),
            }
        }
        executable
    }
    fn wait_for_requests(&self) -> u64 {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(meta) = fs::metadata(self.path("requests")) {
                if meta.len() > 0 {
                    return meta.len();
                }
            }
            assert!(
                Instant::now() < deadline,
                "script did not receive a request"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
    fn pid(&self) -> i32 {
        fs::read_to_string(self.path("pid"))
            .unwrap()
            .parse()
            .unwrap()
    }
    fn wait_for_operations(&self, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let bytes = fs::read(self.path("requests")).unwrap();
            let mut wire = bytes.as_slice();
            let mut count = 0;
            while let Ok(Some(_)) = worker::read_message::<worker::Request>(&mut wire) {
                count += 1;
            }
            if count == expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "worker recorded {count} of {expected} operations"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
}

#[test]
fn crash_overlay_preserves_survivors_and_future_crash_policy() {
    for survival in [0.0, 0.5, 1.0] {
        let model = seedbox_arm64::disk::parse_disk_fault_model(&json!({
            "sector_bytes": 512,
            "survival_probability": survival,
        }))
        .unwrap();
        let base = || {
            Disk::new(Some(vec![0x31; 2048]), Some(4))
                .unwrap()
                .with_fault_model(model)
        };
        let mut disk = base();
        disk.write(0, &[0x42; 512]).unwrap();
        disk.flush();
        disk.write(2, &[0x53; 512]).unwrap();
        let (survivor, lost) = disk.crashed_seeded(1009, "machine", 1);
        let mut bytes = Vec::new();
        survivor.write_crash_transfer(&mut bytes).unwrap();
        let imported = Disk::read_crash_transfer(base(), Cursor::new(bytes)).unwrap();
        assert_eq!(imported.digest(), survivor.digest());
        assert_eq!(imported.base_sha256(), survivor.base_sha256());
        assert_eq!(imported.fault_model(), model);
        assert_eq!(imported.volatile_sectors(), 0);
        let mut actual = vec![0; 2048];
        imported.read(0, &mut actual).unwrap();
        assert_eq!(&actual[..512], &[0x42; 512]);
        if survival == 0.0 {
            assert_eq!(&actual[1024..1536], &[0x31; 512]);
            assert_eq!(lost, 1);
        }
        if survival == 1.0 {
            assert_eq!(&actual[1024..1536], &[0x53; 512]);
            assert_eq!(lost, 0);
        }
        let mutate = |mut d: Disk| {
            d.write(1, &[0x64; 512]).unwrap();
            let (next, lost) = d.crashed_seeded(1010, "machine", 2);
            (next.digest(), lost)
        };
        assert_eq!(mutate(imported), mutate(survivor));
    }
}

#[test]
fn crash_overlay_rejects_changed_identity_bounds_and_corrupt_bytes() {
    let base = || Disk::new(Some(vec![0x31; 2048]), Some(4)).unwrap();
    let mut source = base();
    source.write(0, &[0x42; 512]).unwrap();
    source.write(2, &[0x53; 512]).unwrap();
    source.flush();
    let mut valid = Vec::new();
    source.write_crash_transfer(&mut valid).unwrap();
    let mut bad_cases = Vec::new();
    for offset in [0, 8, 16, 24, 25, 97, 97 + 8, valid.len() - 1] {
        let mut bad = valid.clone();
        bad[offset] ^= 0x80;
        bad_cases.push(bad);
    }
    let mut too_many = valid.clone();
    too_many[89..97].copy_from_slice(&u64::MAX.to_le_bytes());
    bad_cases.push(too_many);
    let mut duplicate = valid.clone();
    duplicate[617..625].copy_from_slice(&0u64.to_le_bytes());
    bad_cases.push(duplicate);
    let mut trailing = valid.clone();
    trailing.push(0);
    bad_cases.push(trailing);
    bad_cases.push(valid[..valid.len() - 1].to_vec());
    for (case, bytes) in bad_cases.into_iter().enumerate() {
        assert!(
            Disk::read_crash_transfer(base(), Cursor::new(bytes)).is_err(),
            "case {case}"
        );
    }
    let changed_base = Disk::new(Some(vec![0x32; 2048]), Some(4)).unwrap();
    assert!(Disk::read_crash_transfer(changed_base, Cursor::new(&valid)).is_err());
    let mut dirty = base();
    dirty.write(3, &[0x64; 512]).unwrap();
    assert!(Disk::read_crash_transfer(dirty, Cursor::new(&valid)).is_err());
}

use seedbox_arm64::{
    backend::{Backend, CoreReg, DirtyPages, Exit, RunStop, Stop as CpuStop, SysReg},
    machine::{Machine, MachineStop},
    memory::Memory,
    Result as ArmResult,
};
use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    rc::Rc,
};
#[derive(Clone, Copy)]
enum Action {
    Handshake(u64),
    Console(u64, u8),
    Yield,
}
thread_local! {
    static ACTIONS: RefCell<VecDeque<Action>> = RefCell::default();
    static ENTRIES: RefCell<usize> = const { RefCell::new(0) };
}
struct ScriptBackend {
    registers: [u64; 34],
    system: BTreeMap<u16, u64>,
    mappings: BTreeMap<u32, Rc<Memory>>,
}
impl Backend for ScriptBackend {
    fn create() -> ArmResult<Self> {
        let system = BTreeMap::from([
            (SysReg::ID_AA64MMFR0.0, 0x0100_1000_0000_0023),
            (SysReg::ID_AA64DFR0.0, 0x10305606),
        ]);
        Ok(Self {
            registers: [0; 34],
            system,
            mappings: BTreeMap::new(),
        })
    }
    fn map_memory(&mut self, slot: u32, _: u64, memory: Rc<Memory>) -> ArmResult<()> {
        self.mappings.insert(slot, memory);
        Ok(())
    }
    fn unmap_memory(&mut self, slot: u32) -> ArmResult<()> {
        self.mappings.remove(&slot);
        Ok(())
    }
    fn create_vcpu(&mut self) -> ArmResult<()> {
        Ok(())
    }
    fn get_reg(&self, reg: CoreReg) -> ArmResult<u64> {
        Ok(self.registers[reg_index(reg)])
    }
    fn set_reg(&mut self, reg: CoreReg, value: u64) -> ArmResult<()> {
        self.registers[reg_index(reg)] = value;
        Ok(())
    }
    fn get_sys_reg(&self, reg: SysReg) -> ArmResult<u64> {
        Ok(*self.system.get(&reg.0).unwrap_or(&0))
    }
    fn set_sys_reg(&mut self, reg: SysReg, value: u64) -> ArmResult<()> {
        self.system.insert(reg.0, value);
        Ok(())
    }
    fn run(&mut self) -> ArmResult<RunStop> {
        ENTRIES.with(|entries| *entries.borrow_mut() += 1);
        let action = ACTIONS
            .with(|actions| actions.borrow_mut().pop_front())
            .ok_or("unexpected CPU entry")?;
        let (addr, len, data) = match action {
            Action::Handshake(time) => {
                unsafe {
                    self.mappings[&1].set_word(8, time)?;
                }
                (seedbox_arm64::EVENT_ADDR + 32, 8, seedbox_arm64::ABI)
            }
            Action::Console(time, byte) => {
                unsafe {
                    self.mappings[&1].set_word(8, time)?;
                }
                (seedbox_arm64::UART_ADDR, 1, u64::from(byte))
            }
            Action::Yield => (seedbox_arm64::EVENT_ADDR + 24, 8, 1),
        };
        Ok(CpuStop {
            pc: seedbox_arm64::RAM_BASE,
            exit: Exit::Mmio {
                addr,
                len,
                write: true,
                data,
                reg: None,
            },
        }
        .into())
    }
    fn complete(&mut self, _: Option<u64>) -> ArmResult<()> {
        Ok(())
    }
    fn inject_spi(&mut self, _: u32, _: bool) -> ArmResult<()> {
        Ok(())
    }
    fn dirty_pages(&self, _: u32) -> ArmResult<DirtyPages> {
        Ok(DirtyPages::Unsupported)
    }
}
fn reg_index(reg: CoreReg) -> usize {
    match reg {
        CoreReg::X(n) => n as usize,
        CoreReg::Sp => 31,
        CoreReg::Pc => 32,
        CoreReg::Pstate => 33,
    }
}
fn kernel() -> [u8; 64] {
    let mut bytes = [0; 64];
    bytes[56..60].copy_from_slice(b"ARM\x64");
    bytes[16..24].copy_from_slice(&4096u64.to_le_bytes());
    bytes
}
fn boot_inputs(fixture: &Fixture) -> worker::Boot {
    fs::write(fixture.path("kernel"), kernel()).unwrap();
    fs::write(fixture.path("initrd"), b"cpio").unwrap();
    worker::Boot {
        kernel: fixture.path("kernel"),
        initrd: fixture.path("initrd"),
        seed: 7,
        memory_bytes: 128 << 20,
        command_line: String::new(),
        mac: [2, 0, 0, 0, 0, 1],
        disk_image: None,
        disk_sectors: Some(4),
        disk_fault_model: seedbox_arm64::disk::parse_disk_fault_model(&Value::Null).unwrap(),
        timer_policy: None,
        progress: None,
    }
}
#[test]
fn worker_logs_keep_first_commit_time_without_console_observation_stops() {
    use seedbox_arm64::application::log_capture::{Chunk, Stream};
    ACTIONS.with(|actions| {
        *actions.borrow_mut() = [
            Action::Handshake(5),
            Action::Console(6, b'A'),
            Action::Console(7, b'B'),
            Action::Yield,
            Action::Console(12, b'C'),
            Action::Yield,
        ]
        .into();
    });
    let fixture = Fixture::new();
    let id = identity("a", 1, 0);
    let (result, replies) = serve_requests(vec![
        request(
            0,
            id.clone(),
            worker::Operation::Boot(boot_inputs(&fixture)),
        ),
        request(
            1,
            id.clone(),
            worker::Operation::Advance {
                hard_limit: 10,
                early_slack: 3,
                observe_console: false,
            },
        ),
        request(
            2,
            id,
            worker::Operation::Advance {
                hard_limit: 20,
                early_slack: 0,
                observe_console: false,
            },
        ),
    ]);
    result.unwrap();
    for (reply, expected_time, expected_bytes, first_time) in [
        (&replies[1], 10, b"AB".as_slice(), 6),
        (&replies[2], 20, b"C".as_slice(), 12),
    ] {
        let worker::Answer::Advanced {
            time_ns,
            console,
            log_chunks,
            ..
        } = &reply.answer
        else {
            panic!("wrong answer");
        };
        assert_eq!(*time_ns, expected_time);
        assert_eq!(console, expected_bytes);
        assert_eq!(
            log_chunks,
            &[Chunk {
                stream: Stream::Console,
                time_ns: first_time,
                offset: 0,
                length: expected_bytes.len() as u64
            }]
        );
    }
}

#[test]
fn observed_console_stops_at_first_committed_byte_without_consuming_the_next() {
    ACTIONS.with(|actions| {
        *actions.borrow_mut() = [
            Action::Handshake(5),
            Action::Console(6, b'A'),
            Action::Console(7, b'B'),
            Action::Yield,
        ]
        .into()
    });
    ENTRIES.with(|entries| *entries.borrow_mut() = 0);
    let mut machine = Machine::<ScriptBackend>::new(&kernel(), b"cpio", 7).unwrap();
    assert_eq!(
        machine.run_slice(10, 3, true).unwrap(),
        MachineStop::Console { time_ns: 6 }
    );
    assert_eq!(machine.console(), b"A");
    ENTRIES.with(|entries| assert_eq!(*entries.borrow(), 2));
    assert_eq!(
        machine.run_slice(10, 3, true).unwrap(),
        MachineStop::Console { time_ns: 7 }
    );
    assert_eq!(machine.console(), b"AB");
    ENTRIES.with(|entries| assert_eq!(*entries.borrow(), 3));
    assert_eq!(
        machine.run_slice(10, 0, true).unwrap(),
        MachineStop::Horizon { time_ns: 10 }
    );
    assert_eq!(machine.stopped_observation().owed_io, None);
}

use seedbox_arm64::application::world_adapter::{ArmMachine, Factory};
use seedbox_engine_core::world::{SliceRequest, State, Stop as WorldStop, WorldMachine};
fn identity(name: &str, generation: u64, incarnation: u32) -> worker::Identity {
    worker::Identity {
        name: name.into(),
        generation,
        incarnation,
    }
}
fn observation(time: u64) -> Value {
    json!({"time_ns":time,"pending_irq":false,"next_deadline":u64::MAX,"owed_io":null,
        "abi":seedbox_arm64::ABI,"exits":time,"powered_off":false,"exit":null,
        "barrier":false,"has_control":true})
}
fn boot_reply() -> Value {
    let mut initial = observation(0);
    initial["abi"] = json!(0);
    json!({"sequence":0,"before":null,"after":identity("a",1,0),
        "observation":initial,"answer":"Ready"})
}
fn advance_reply(time: u64, stop: Value, console: &[u8], events: Value) -> Value {
    let management: usize = events
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|event| event.get("Control").and_then(Value::as_array))
        .map(Vec::len)
        .sum();
    let mut log_chunks = Vec::new();
    if !console.is_empty() {
        log_chunks
            .push(json!({"stream":"Console","time_ns":time,"offset":0,"length":console.len()}));
    }
    if management != 0 {
        log_chunks
            .push(json!({"stream":"Management","time_ns":time,"offset":0,"length":management}));
    }
    json!({"sequence":1,"before":identity("a",1,0),"after":identity("a",1,0),
        "observation":observation(time),"answer":{"Advanced":{
            "stop":stop,"time_ns":time,"events":events,"console":console,"log_chunks":log_chunks}}})
}
fn horizon_reply() -> Value {
    advance_reply(10, json!({"Horizon":{"time_ns":10}}), b"", json!([]))
}
fn proxy(fixture: &Fixture, replies: &[Value], limit: usize) -> ArmMachine {
    let executable = fixture.worker(replies, true);
    let factory = Factory::new(
        executable,
        fixture.path("output"),
        &["a".into()],
        limit,
        false,
    )
    .unwrap();
    let boot = boot_inputs(fixture);
    let machine = ArmMachine::boot(&factory, "a", &boot, false, [0; 32]).unwrap();
    fixture.wait_for_requests();
    machine
}
fn assert_reaped(pid: i32) {
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "worker still exists after cleanup"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

#[test]
fn adversarial_replies_are_terminal_before_scheduler_effect_or_capture_publication() {
    let mut cases: Vec<(&str, Value)> = Vec::new();
    for (label, field, value) in [
        ("duplicate sequence", "sequence", json!(0)),
        ("out of order sequence", "sequence", json!(3)),
        ("stale generation", "before", json!(identity("a", 0, 0))),
        ("future generation", "after", json!(identity("a", 2, 0))),
        (
            "wrong machine same generation",
            "before",
            json!(identity("b", 1, 0)),
        ),
        ("stale incarnation", "after", json!(identity("a", 1, 1))),
        ("missing stopped observation", "observation", Value::Null),
        ("wrong answer kind", "answer", json!("Ready")),
        ("unknown response field", "unrecognized", json!(1)),
    ] {
        let mut bad = horizon_reply();
        bad[field] = value;
        cases.push((label, bad));
    }
    for (label, field, value) in [
        ("invented timestamp", "time_ns", json!(6)),
        ("offset gap", "offset", json!(1)),
        ("uncovered byte", "length", json!(0)),
        ("oversized length", "length", json!(u64::MAX)),
        ("unknown stream", "stream", json!("Invalid")),
        ("unknown chunk field", "unexpected", json!(0)),
    ] {
        let mut bad = advance_reply(5, json!({"Console":{"time_ns":5}}), b"x", json!([]));
        bad["answer"]["Advanced"]["log_chunks"][0][field] = value;
        cases.push((label, bad));
    }
    let mut missing = advance_reply(5, json!({"Console":{"time_ns":5}}), b"x", json!([]));
    missing["answer"]["Advanced"]
        .as_object_mut()
        .unwrap()
        .remove("log_chunks");
    cases.push(("missing log metadata", missing));
    let mut control_topology = horizon_reply();
    control_topology["observation"]["has_control"] = json!(false);
    cases.push(("invented control topology", control_topology));
    let mut invalid_abi = horizon_reply();
    invalid_abi["observation"]["abi"] = json!(seedbox_arm64::ABI + 1);
    cases.push(("invalid ABI transition", invalid_abi));
    let mut owed = horizon_reply();
    owed["observation"]["owed_io"] = json!(1);
    cases.push(("invented owed IO", owed));
    let mut mismatch = horizon_reply();
    mismatch["observation"]["time_ns"] = json!(9);
    cases.push(("time disagreement", mismatch));
    cases.push((
        "past hard limit",
        advance_reply(11, json!({"Horizon":{"time_ns":11}}), b"x", json!([])),
    ));
    cases.push((
        "early exact horizon",
        advance_reply(9, json!({"Horizon":{"time_ns":9}}), b"x", json!([])),
    ));
    cases.push((
        "false poweroff",
        advance_reply(5, json!("PoweredOff"), b"x", json!([])),
    ));
    cases.push((
        "false barrier",
        advance_reply(5, json!("Barrier"), b"x", json!([])),
    ));
    cases.push((
        "false exit",
        advance_reply(
            5,
            json!({"ApplicationExit":{"code":0,"setup_error":false}}),
            b"x",
            json!([]),
        ),
    ));
    cases.push((
        "console batch",
        advance_reply(5, json!({"Console":{"time_ns":5}}), b"AB", json!([])),
    ));
    cases.push((
        "malformed management after staged frame",
        advance_reply(
            5,
            json!({"Device":{"time_ns":5}}),
            b"x",
            json!([
                {"Frame":[2,0,0,0,0,2,2,0,0,0,0,1,8,0]},
                "Ready", {"Control":vec![b'x'; 17000]},
            ]),
        ),
    ));
    for (label, bad) in cases {
        let fixture = Fixture::new();
        let mut machine = proxy(&fixture, &[boot_reply(), bad, horizon_reply()], 100);
        let pid = fixture.pid();
        let before = format!("{:?}", *machine);
        let mut frames = vec![vec![0x99]];
        let request = SliceRequest {
            hard_limit: 10,
            early_slack: 0,
            observe_console: true,
        };
        assert!(
            machine.advance(&mut (), request, &mut frames).is_err(),
            "{label}"
        );
        assert_eq!(
            format!("{:?}", *machine),
            before,
            "{label}: state published"
        );
        assert_eq!(frames, [vec![0x99]], "{label}: frame published");
        assert!(
            fs::read(fixture.path("output/a.console"))
                .unwrap()
                .is_empty(),
            "{label}: console published"
        );
        assert!(
            fs::read(fixture.path("output/a.mgmt")).unwrap().is_empty(),
            "{label}: management published"
        );
        let requests = fs::read(fixture.path("requests")).unwrap();
        assert!(
            machine
                .advance(&mut (), request, &mut frames)
                .unwrap_err()
                .contains("terminal"),
            "{label}"
        );
        assert_eq!(
            fs::read(fixture.path("requests")).unwrap(),
            requests,
            "{label}: request sent after poison"
        );
        assert_reaped(pid);
    }
}

#[test]
fn bounded_process_frames_reject_malformed_truncation_and_eof_without_publishing() {
    let cases = [
        vec![],
        vec![1],
        vec![5, 0, 0, 0, b'{'],
        vec![0; 4],
        ((16u32 << 20) + 1).to_le_bytes().to_vec(),
        vec![1, 0, 0, 0, b'{'],
    ];
    for bytes in cases {
        let fixture = Fixture::new();
        let executable = fixture.raw_worker(&bytes, false);
        let mut worker = worker::OwnedWorker::spawn(&executable, identity("a", 1, 0)).unwrap();
        assert!(worker
            .call(worker::Operation::Boot(boot_inputs(&fixture)))
            .is_err());
        assert_eq!(worker.identity(), &identity("a", 1, 0));
        assert!(worker.observation().is_none());
        assert!(worker
            .call(worker::Operation::Park)
            .unwrap_err()
            .contains("terminal"));
        assert_reaped(fixture.pid());
    }
}

#[test]
fn accepted_channels_are_complete_even_when_retained_capture_is_exhausted() {
    let first = advance_reply(
        3,
        json!({"Device":{"time_ns":3}}),
        b"abc",
        json!([
            {"Control":b"SBX_RPC1 1 1 ".to_vec()}, "Ready",
            {"Effect":{"id":7,"success":true}}, {"Frame":[2,0,0,0,0,2,2,0,0,0,0,1,8,0]},
        ]),
    );
    let mut second = advance_reply(
        4,
        json!({"Device":{"time_ns":4}}),
        b"def",
        json!([
            {"Control":b"{\"version\":1,\"id\":1,\"op\":\"now\"}\n".to_vec()},
        ]),
    );
    second["sequence"] = json!(2);
    let fixture = Fixture::new();
    let mut machine = proxy(&fixture, &[boot_reply(), first, second], 2);
    let request = SliceRequest {
        hard_limit: 10,
        early_slack: 2,
        observe_console: false,
    };
    let mut frames = Vec::new();
    assert_eq!(
        machine.advance(&mut (), request, &mut frames).unwrap(),
        WorldStop::Sent
    );
    assert_eq!(machine.time, 3);
    assert_eq!(machine.fault_effects, [(0, 3, true), (7, 3, true)]);
    assert!(machine.control_requests.is_empty());
    assert_eq!(frames.len(), 1);
    machine.clear_effects();
    frames.clear();
    assert_eq!(
        machine.advance(&mut (), request, &mut frames).unwrap(),
        WorldStop::Control
    );
    assert_eq!(
        machine.control_requests,
        [(
            4,
            b"SBX_RPC1 1 1 {\"version\":1,\"id\":1,\"op\":\"now\"}".to_vec()
        )]
    );
    assert_eq!(fs::read(fixture.path("output/a.console")).unwrap(), b"ab");
    assert_eq!(fs::read(fixture.path("output/a.mgmt")).unwrap(), b"SB");
    for (name, order) in [("console", 0u64), ("mgmt", 1u64)] {
        let bytes = fs::read(fixture.path(&format!("output/a.{name}.chunks"))).unwrap();
        assert_eq!(&bytes[..8], b"SBLOG001");
        let fields: Vec<u64> = bytes[8..]
            .as_chunks::<8>()
            .0
            .iter()
            .map(|bytes| u64::from_le_bytes(*bytes))
            .collect();
        assert_eq!(fields, [3, 0, order, 0, 2]);
    }
    assert_eq!(machine.state, State::Running);
    let pid = fixture.pid();
    drop(machine);
    assert_reaped(pid);
}

#[test]
fn checkpoint_restore_rewinds_only_after_an_exact_acknowledgement() {
    for forged in [false, true] {
        let fixture = Fixture::new();
        let initial = boot_reply()["observation"].clone();
        let checkpoint = json!({
            "artifact":fs::canonicalize(&fixture.root).unwrap().join("output").join(format!(".arm-checkpoint-{}-1-1", std::process::id())),
            "identity":identity("a",1,0), "observation":initial,
            "console_offset":0, "sha256":"a".repeat(64),
        });
        let captured = json!({"sequence":1,"before":identity("a",1,0),"after":identity("a",1,0),
            "observation":initial,"answer":{"Snapshot":checkpoint}});
        let mut advanced = horizon_reply();
        advanced["sequence"] = json!(2);
        let mut restored = json!({"sequence":3,"before":identity("a",1,0),"after":identity("a",1,0),
            "observation":initial,"answer":{"Restored":checkpoint}});
        if forged {
            restored["answer"]["Restored"]["sha256"] = json!("b".repeat(64));
        }
        let mut machine = proxy(&fixture, &[boot_reply(), captured, advanced, restored], 0);
        let before = format!("{:?}", *machine);
        let saved = machine.snapshot().unwrap();
        let saved = serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();
        machine
            .advance(
                &mut (),
                SliceRequest {
                    hard_limit: 10,
                    early_slack: 0,
                    observe_console: false,
                },
                &mut Vec::new(),
            )
            .unwrap();
        let advanced = format!("{:?}", *machine);
        let result = machine.restore(&saved);
        if forged {
            assert!(result.unwrap_err().contains("acknowledgement mismatch"));
            assert_eq!(format!("{:?}", *machine), advanced);
            assert!(machine.snapshot().is_err());
        } else {
            assert_eq!(
                result.unwrap(),
                boot_inputs(&fixture).memory_bytes.div_ceil(4096)
            );
            assert_eq!(format!("{:?}", *machine), before);
            assert_eq!(machine.exits(), 0);
        }
    }
}

#[test]
fn unsupported_time_reseed_does_not_send_or_fabricate_state() {
    let fixture = Fixture::new();
    let mut machine = proxy(&fixture, &[boot_reply()], 2);
    let before = format!("{:?}", *machine);
    let requests = fs::read(fixture.path("requests")).unwrap();
    assert!(machine
        .release(10, [0; 8])
        .unwrap_err()
        .contains("unsupported"));
    assert!(machine
        .seed_speed(7, "speed")
        .unwrap_err()
        .contains("unsupported"));
    assert!(!machine.supports_speed_reseed());
    assert_eq!(format!("{:?}", *machine), before);
    assert_eq!(fs::read(fixture.path("requests")).unwrap(), requests);
}

#[test]
fn external_worker_cancellation_becomes_terminal_and_is_reaped() {
    let fixture = Fixture::new();
    let executable = fixture.worker(&[boot_reply()], true);
    let mut worker = worker::OwnedWorker::spawn(&executable, identity("a", 1, 0)).unwrap();
    worker
        .call(worker::Operation::Boot(boot_inputs(&fixture)))
        .unwrap();
    fixture.wait_for_requests();
    let pid = fixture.pid();
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    assert!(worker.call(worker::Operation::Park).is_err());
    assert!(worker
        .call(worker::Operation::Park)
        .unwrap_err()
        .contains("terminal"));
    assert_reaped(pid);
}

fn serve_requests(requests: Vec<worker::Request>) -> (ArmResult<()>, Vec<worker::Response>) {
    let mut input = Vec::new();
    for request in requests {
        worker::write_message(&mut input, &request).unwrap();
    }
    let mut output = Vec::new();
    let result = worker::serve::<ScriptBackend>(&mut input.as_slice(), &mut output);
    let mut output = output.as_slice();
    let mut replies = Vec::new();
    while let Some(reply) = worker::read_message(&mut output).unwrap() {
        replies.push(reply);
    }
    (result, replies)
}
fn request(
    sequence: u64,
    expected: worker::Identity,
    operation: worker::Operation,
) -> worker::Request {
    worker::Request {
        sequence,
        expected,
        operation,
    }
}
fn export_transfer(fixture: &Fixture) -> worker::ExportedTransfer {
    let predecessor = identity("a", 1, 0);
    let descriptor = worker::TransferDescriptor {
        transaction: 17,
        predecessor: predecessor.clone(),
        successor: identity("a", 2, 1),
        artifact: fixture.path("surviving-disk"),
        at: 10,
    };
    let (result, mut replies) = serve_requests(vec![
        request(
            0,
            predecessor.clone(),
            worker::Operation::Boot(boot_inputs(fixture)),
        ),
        request(
            1,
            predecessor,
            worker::Operation::CrashExport {
                run_seed: 1009,
                full_state: false,
                transfer: descriptor.clone(),
            },
        ),
    ]);
    result.unwrap();
    assert_eq!(replies.len(), 2);
    let reply = replies.pop().unwrap();
    assert!(reply.observation.is_none());
    match reply.answer {
        worker::Answer::Crashed { transfer, .. } => {
            assert_eq!(transfer.descriptor, descriptor);
            transfer
        }
        answer => panic!("unexpected export answer {answer:?}"),
    }
}
#[test]
fn transfer_is_bound_to_the_actual_machine_even_when_both_disks_have_the_same_base() {
    let fixture = Fixture::new();
    let exported = export_transfer(&fixture);
    for forgery in ["machine", "transaction", "predecessor", "successor"] {
        let mut transfer = exported.clone();
        let mut fresh = identity("a", 2, 0);
        match forgery {
            "machine" => {
                fresh.name = "b".into();
                transfer.descriptor.predecessor.name = "b".into();
                transfer.descriptor.successor.name = "b".into();
            }
            "transaction" => transfer.descriptor.transaction += 1,
            "predecessor" => transfer.descriptor.predecessor.generation = 3,
            "successor" => {
                fresh.generation = 3;
                transfer.descriptor.successor.generation = 3;
            }
            _ => unreachable!(),
        }
        // Boot config is intentionally identical: only the transaction binds ownership.
        let (result, replies) = serve_requests(vec![
            request(
                0,
                fresh.clone(),
                worker::Operation::Boot(boot_inputs(&fixture)),
            ),
            request(
                1,
                fresh.clone(),
                worker::Operation::InstallSuccession(transfer),
            ),
            request(
                2,
                fresh.clone(),
                worker::Operation::Evidence {
                    full_state: false,
                    ram_pages: None,
                },
            ),
        ]);
        result.unwrap();
        assert_eq!(
            replies.len(),
            2,
            "{forgery}: continued after rejected install"
        );
        assert_eq!(replies[1].after, Some(fresh), "{forgery}: rebound identity");
        assert!(
            replies[1].observation.is_none(),
            "{forgery}: published physical state"
        );
        assert!(
            matches!(&replies[1].answer, worker::Answer::Error(error) if error.contains("binding mismatch")),
            "{forgery}: {:?}",
            replies[1].answer
        );
    }
}
#[test]
fn transfer_install_is_one_shot_and_commits_successor_identity_only_once() {
    let fixture = Fixture::new();
    let transfer = export_transfer(&fixture);
    let successor = transfer.descriptor.successor.clone();
    let (result, replies) = serve_requests(vec![
        request(
            0,
            identity("a", 2, 0),
            worker::Operation::Boot(boot_inputs(&fixture)),
        ),
        request(
            1,
            identity("a", 2, 0),
            worker::Operation::InstallSuccession(transfer.clone()),
        ),
        request(
            2,
            successor.clone(),
            worker::Operation::InstallSuccession(transfer),
        ),
        request(
            3,
            successor.clone(),
            worker::Operation::Evidence {
                full_state: false,
                ram_pages: None,
            },
        ),
    ]);
    result.unwrap();
    assert_eq!(
        replies.len(),
        3,
        "worker processed a request after duplicate install"
    );
    assert_eq!(replies[1].before, Some(identity("a", 2, 0)));
    assert_eq!(replies[1].after, Some(successor.clone()));
    assert!(matches!(
        replies[1].answer,
        worker::Answer::Installed { .. }
    ));
    assert_eq!(replies[1].observation.as_ref().unwrap().time_ns, 10);
    assert_eq!(
        replies[1].observation.as_ref().unwrap().exits,
        0,
        "successor entered before installation"
    );
    assert_eq!(replies[2].after, Some(successor));
    assert!(
        matches!(&replies[2].answer, worker::Answer::Error(error) if error.contains("fresh uninstalled"))
    );
    assert!(replies[2].observation.is_none());
}
#[test]
fn worker_rejects_stale_request_stamp_before_physical_entry() {
    for expected in [
        identity("a", 0, 0),
        identity("a", 1, 1),
        identity("b", 1, 0),
    ] {
        let fixture = Fixture::new();
        ENTRIES.with(|entries| *entries.borrow_mut() = 0);
        let (result, replies) = serve_requests(vec![
            request(
                0,
                identity("a", 1, 0),
                worker::Operation::Boot(boot_inputs(&fixture)),
            ),
            request(
                1,
                expected,
                worker::Operation::Advance {
                    hard_limit: 10,
                    early_slack: 0,
                    observe_console: false,
                },
            ),
        ]);
        assert!(result.unwrap_err().contains("identity mismatch"));
        assert_eq!(replies.len(), 1);
        ENTRIES.with(|entries| assert_eq!(*entries.borrow(), 0));
    }
}

#[test]
fn installed_ack_must_echo_the_complete_transaction_before_identity_publication() {
    for field in ["transaction", "disk_digest", "artifact_sha256"] {
        let fixture = Fixture::new();
        let transfer = export_transfer(&fixture);
        let mut echo = serde_json::to_value(&transfer).unwrap();
        match field {
            "transaction" => echo["descriptor"]["transaction"] = json!(18),
            "disk_digest" => echo["disk_digest"] = json!("0".repeat(64)),
            "artifact_sha256" => echo["artifact_sha256"] = json!("0".repeat(64)),
            _ => unreachable!(),
        }
        let fresh = identity("a", 2, 0);
        let mut initial = boot_reply();
        initial["after"] = json!(fresh);
        let mut stopped = observation(10);
        stopped["abi"] = json!(0);
        stopped["exits"] = json!(0);
        let forged = json!({"sequence":1,"before":fresh,"after":transfer.descriptor.successor,
            "observation":stopped,"answer":{"Installed":{"transfer":echo}}});
        let executable = fixture.worker(&[initial, forged], true);
        let mut worker = worker::OwnedWorker::spawn(&executable, fresh.clone()).unwrap();
        worker
            .call(worker::Operation::Boot(boot_inputs(&fixture)))
            .unwrap();
        fixture.wait_for_requests();
        let before = worker.observation().cloned();
        assert!(
            worker
                .call(worker::Operation::InstallSuccession(transfer))
                .is_err(),
            "{field}"
        );
        assert_eq!(worker.identity(), &fresh, "{field}: published incarnation");
        assert_eq!(
            worker.observation(),
            before.as_ref(),
            "{field}: published stopped state"
        );
        assert!(worker
            .call(worker::Operation::Park)
            .unwrap_err()
            .contains("terminal"));
        assert_reaped(fixture.pid());
    }
}

#[test]
fn actual_arm_proxy_obeys_core_strict_reply_boundary_despite_fatal_control() {
    use seedbox_engine_core::{
        control, links,
        world::{self, World, WorldInputs},
    };
    let fixture = Fixture::new();
    let mut at50 = advance_reply(50, json!({"Horizon":{"time_ns":50}}), b"", json!([]));
    at50["sequence"] = json!(1);
    let mut at100 = advance_reply(100, json!({"Horizon":{"time_ns":100}}), b"", json!([]));
    at100["sequence"] = json!(2);
    let delivered = json!({"sequence":3,"before":identity("a",1,0),"after":identity("a",1,0),
        "observation":observation(100),"answer":"Ready"});
    let mut at150 = advance_reply(150, json!({"Horizon":{"time_ns":150}}), b"", json!([]));
    at150["sequence"] = json!(4);
    let executable = fixture.worker(&[boot_reply(), at50, at100, delivered, at150], true);
    let names = vec!["a".into()];
    let factory = Factory::new(executable, fixture.path("output"), &names, 100, false).unwrap();
    let boot = boot_inputs(&fixture);
    let machine = ArmMachine::boot(&factory, "a", &boot, false, [0; 32]).unwrap();
    fixture.wait_for_requests();
    let faults = world::boot_faults(
        &names,
        &[],
        10,
        links::Links::from_config(1, &names, &json!({"latency_ns":10})).unwrap(),
    )
    .unwrap();
    let mut control = control::Control::new(
        &json!({"machine":"a","latency_ns":50,"placement":{}}),
        names,
        10,
        3,
    )
    .unwrap();
    control
        .request(
            0,
            0,
            b"SBX_RPC1 1 1 {\"version\":1,\"id\":1,\"op\":\"now\"}",
            0,
        )
        .unwrap();
    control.pending.as_mut().unwrap().fatal = true;
    control.pending.as_mut().unwrap().error = Some("effect failed".into());
    let mut world = World::new(WorldInputs {
        machines: vec![machine],
        factory,
        boot: vec![boot],
        barrier: false,
        boot_seed: 1,
        run_seed: 2,
        clock: (),
        lookahead: 10,
        early: 3,
        latency: 10,
        faults,
        control,
        macs: vec![[2, 0, 0, 0, 0, 1]],
    })
    .unwrap();
    assert_eq!(
        world::simulate(&mut world, &Value::Null, 150, &None, None, &mut || Ok(())).unwrap(),
        "fault_failed"
    );
    assert_eq!(world.control().log[0]["delivered_at_ns"], 100);
    assert!(!world.control().has_undelivered_reply());
    // Replies are preloaded; wait for the fixture to consume the queued requests
    // before dropping the transport and terminating the recording process.
    fixture.wait_for_operations(5);
    // Reaping also freezes the raw request transcript for an exact IPC-order assertion.
    let pid = fixture.pid();
    drop(world);
    assert_reaped(pid);
    let wire = fs::read(fixture.path("requests")).unwrap();
    let mut wire = wire.as_slice();
    let mut operations = Vec::new();
    while let Some(request) = worker::read_message::<worker::Request>(&mut wire).unwrap() {
        operations.push(request.operation);
    }
    assert_eq!(operations.len(), 5);
    assert!(matches!(
        operations[1],
        worker::Operation::Advance {
            hard_limit: 50,
            early_slack: 0,
            ..
        }
    ));
    assert!(matches!(
        operations[2],
        worker::Operation::Advance {
            hard_limit: 100,
            early_slack: 0,
            ..
        }
    ));
    assert!(
        matches!(&operations[3],worker::Operation::Control(bytes) if bytes.starts_with(b"SBX_REPLY1 "))
    );
    assert!(matches!(
        operations[4],
        worker::Operation::Advance {
            hard_limit: 150,
            ..
        }
    ));
}

#[test]
fn ordinary_ack_cannot_invent_scheduler_irq_or_deadline_hints() {
    for operation in [
        "evidence_deadline",
        "evidence_irq",
        "park_irq",
        "timer_irq",
        "control_deadline",
        "frame_deadline",
    ] {
        let fixture = Fixture::new();
        let mut bad = if operation.starts_with("evidence") {
            let (result, replies) = serve_requests(vec![
                request(
                    0,
                    identity("a", 1, 0),
                    worker::Operation::Boot(boot_inputs(&fixture)),
                ),
                request(
                    1,
                    identity("a", 1, 0),
                    worker::Operation::Evidence {
                        full_state: false,
                        ram_pages: None,
                    },
                ),
            ]);
            result.unwrap();
            serde_json::to_value(&replies[1]).unwrap()
        } else {
            let mut reply = boot_reply();
            reply["sequence"] = json!(1);
            reply["before"] = json!(identity("a", 1, 0));
            if operation == "frame_deadline" {
                reply["answer"] = json!({"Accepted":false});
            }
            reply
        };
        if operation.ends_with("irq") {
            bad["observation"]["pending_irq"] = json!(true);
        } else {
            bad["observation"]["next_deadline"] = json!(13);
        }
        let mut machine = proxy(&fixture, &[boot_reply(), bad], 100);
        let before = format!("{:?}", *machine);
        let result = match operation {
            "evidence_deadline" | "evidence_irq" => machine.exit_witness().map(|_| ()),
            "park_irq" => machine.park(),
            "timer_irq" => machine.seed_timer(7, "timer/a"),
            "control_deadline" => machine.deliver_control(b"SBX_REPLY1 test\n"),
            "frame_deadline" => machine.deliver_frame(&[0; 14]).map(|_| ()),
            _ => unreachable!(),
        };
        assert!(
            result.is_err(),
            "{operation}: accepted invented scheduler hint"
        );
        assert_eq!(
            format!("{:?}", *machine),
            before,
            "{operation}: published state"
        );
        assert!(!machine.has_pending_irq(), "{operation}: published IRQ");
        assert_eq!(
            machine.next_deadline(),
            u64::MAX,
            "{operation}: published deadline"
        );
        assert_eq!(machine.exits(), 0, "{operation}: published exit count");
        let requests = fs::read(fixture.path("requests")).unwrap();
        assert!(machine
            .deliver_control(b"late")
            .unwrap_err()
            .contains("terminal"));
        assert_eq!(
            fs::read(fixture.path("requests")).unwrap(),
            requests,
            "{operation}: postpoison write"
        );
        assert_reaped(fixture.pid());
    }
}
