//! Serializable native controller using only the reliable byte protocol.
//! Scripted service replaces guest Linux in these contract fixtures.
use seedbox_world::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
const HEALTH: u8 = 1;
const HEALTH_REPLY: u8 = 2;
const PUT: u8 = 3;
const PUT_REPLY: u8 = 4;
fn mid(n: u32) -> MachineId {
    MachineId(n)
}
fn frame(kind: u8, id: RequestId, data: &[u8]) -> Vec<u8> {
    let mut b = vec![0; 14];
    b[..6].copy_from_slice(&[2, 0, 0, 0, 0, 1]);
    b[6..12].copy_from_slice(&[2, 0, 0, 0, 0, 2]);
    b[12..14].copy_from_slice(&[0x88, 0xb5]);
    b.push(kind);
    b.extend(id.to_le_bytes());
    b.extend(data);
    b
}
fn app_id(b: &[u8]) -> RequestId {
    RequestId(u64::from_le_bytes(b[15..23].try_into().unwrap()))
}
#[derive(Clone, Default, Debug, Serialize, Deserialize)]
struct Service {
    pending: BTreeMap<RequestId, (Owner, Endpoint, RequestId)>,
}
impl Service {
    fn boundary(&mut self, w: &mut World) {
        let requests: Vec<_> = self.pending.keys().copied().collect();
        for req in requests {
            if let Some(c) = w.completion(req) {
                let (owner, to, query) = self.pending.remove(&req).unwrap();
                if c.kind == CompletionKind::Flush && w.alive(owner.machine) {
                    let data = w.bytes(owner.machine, true);
                    let tx = w.owner(owner.machine);
                    w.transmit(tx, to, frame(PUT_REPLY, query, &data), None)
                        .unwrap();
                }
            }
        }
        for n in 1..=6 {
            let machine = mid(n);
            for f in w.take_frames(Endpoint::Machine(machine)) {
                if f.bytes.len() < 23 {
                    continue;
                }
                let query = app_id(&f.bytes);
                match f.bytes[14] {
                    HEALTH => {
                        let mut data = w.incarnation(machine).to_le_bytes().to_vec();
                        data.extend(w.bytes(machine, false));
                        data.extend(w.bytes(machine, true));
                        let tx = w.owner(machine);
                        w.transmit(tx, f.from, frame(HEALTH_REPLY, query, &data), None)
                            .unwrap();
                    }
                    PUT => {
                        let owner = w.owner(machine);
                        w.block(
                            owner,
                            BlockOp::Write {
                                offset: 0,
                                data: f.bytes[23..].to_vec(),
                            },
                            3,
                        )
                        .unwrap();
                        let flush = w.owner(machine);
                        w.block(flush, BlockOp::Flush, 1).unwrap();
                        self.pending.insert(flush.request, (flush, f.from, query));
                    }
                    _ => {}
                }
            }
        }
        for f in w.take_frames(Endpoint::Controller) {
            if f.bytes.len() >= 23 && (f.bytes[14] == HEALTH_REPLY || f.bytes[14] == PUT_REPLY) {
                w.resolve_query(app_id(&f.bytes), f.bytes[23..].to_vec());
            }
        }
        w.checkpoint_backend("scripted_service", serde_json::to_value(&self).unwrap());
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Bundle {
    world: World,
    controller: NativeController,
    service: Service,
}
impl Bundle {
    fn new(seed: u64, defects: Defects, program: Vec<Instruction>) -> Self {
        let mut world = World::new(
            seed,
            &[
                (mid(1), 1),
                (mid(2), 1),
                (mid(3), 2),
                (mid(4), 2),
                (mid(5), 3),
                (mid(6), 3),
            ],
            8,
            8,
            defects,
        );
        world.register_controller(ControllerId(1), 1).unwrap();
        Self {
            world,
            controller: NativeController::new(program),
            service: Service::default(),
        }
    }
    fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap()
    }
    fn decode(bytes: &[u8]) -> Result<Self> {
        let b: Self = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        b.world.validate_state()?;
        b.controller.validate()?;
        Ok(b)
    }
    fn advance(&mut self) -> Result<()> {
        if let Some(bytes) = self.world.peek_control(self.controller.controller)? {
            let ack = self.controller.observe(&bytes)?;
            self.world.receive_control(&ack)?;
            return Ok(());
        }
        if let Some(bytes) = self.controller.issue()? {
            self.world.receive_control(&bytes)?;
            return Ok(());
        }
        if self.controller.result.is_some() {
            return Ok(());
        }
        match self.world.dispatch_one() {
            DispatchOutcome::Event { .. } => {
                self.service.boundary(&mut self.world);
                self.world.complete_backend_boundary();
            }
            DispatchOutcome::Quiescent => {
                return Err("native_continuation_deadlocked_without_event".into());
            }
            DispatchOutcome::GuestProgressRequired => {
                return Err("fixture_unexpected_guest_progress_requirement".into());
            }
            _ => {}
        }
        Ok(())
    }
    fn run(&mut self) -> Result<()> {
        for _ in 0..20000 {
            if let Some(result) = &self.controller.result {
                return result.clone();
            }
            self.advance()?;
        }
        Err("protocol_fixture_budget_exhausted".into())
    }
    fn report(&self, case: &str, result: Result<()>) -> Value {
        json!({"case":case,"result":result,"world":self.world.snapshot(),"controller":self.controller,"service":self.service,"trace":self.world.trace()})
    }
}
fn query(machine: MachineId, kind: u8, data: &[u8], timeout: u64) -> Command {
    Command::Query {
        destination: machine,
        frame: frame(kind, RequestId(0), data),
        correlation_u64_offset: 15,
        timeout_ticks: Tick(timeout),
    }
}
fn push(p: &mut Vec<Instruction>, command: Template, save: &str) {
    p.push(Instruction {
        command,
        save: save.into(),
    });
}
fn literal(p: &mut Vec<Instruction>, command: Command, save: &str) {
    push(p, Template::Literal(command), save)
}
fn sleep(p: &mut Vec<Instruction>, ticks: u64) {
    literal(
        p,
        Command::Sleep {
            after_ticks: Tick(ticks),
        },
        "sleep",
    )
}
fn loss(from: Scope, to: Scope, n: u64, d: u64) -> LossRule {
    LossRule {
        from,
        to,
        bidirectional: false,
        numerator: n,
        denominator: d,
    }
}
fn fault(p: &mut Vec<Instruction>, rule: LossRule, save: &str) {
    literal(p, Command::Fault { rule }, save)
}
fn heal(p: &mut Vec<Instruction>, local: &str) {
    push(p, Template::Heal(local.into()), "heal")
}
fn send(p: &mut Vec<Instruction>, from: u32, to: u32, byte: u8, delay: Option<u64>, save: &str) {
    literal(
        p,
        Command::Transmit {
            from: mid(from),
            to: Endpoint::Machine(mid(to)),
            bytes: frame(0, RequestId(0), &[byte]),
            wire_delay_ticks: delay.map(Tick),
        },
        save,
    )
}
fn inspect_frame(p: &mut Vec<Instruction>, packet: &str, save: &str) {
    push(p, Template::InspectFrame(packet.into()), save)
}
fn check(p: &mut Vec<Instruction>, label: &str, predicate: Predicate) {
    push(
        p,
        Template::Assert {
            label: label.into(),
            predicate,
        },
        "assertion",
    )
}
fn delivered(local: &str, expected: bool) -> Predicate {
    Predicate::FrameDelivered {
        local: local.into(),
        expected,
    }
}
fn completion(local: &str, kind: CompletionKind) -> Predicate {
    Predicate::Completion {
        local: local.into(),
        kind,
    }
}
fn health(p: &mut Vec<Instruction>, machine: u32, save: &str) {
    literal(p, query(mid(machine), HEALTH, &[], 12), save)
}
fn put(p: &mut Vec<Instruction>, machine: u32, value: u64) {
    literal(
        p,
        query(mid(machine), PUT, &value.to_le_bytes(), 40),
        "put_reply",
    );
    check(
        p,
        "write_ack_contains_actual_durable_value",
        completion(
            "put_reply",
            CompletionKind::Reply(value.to_le_bytes().to_vec()),
        ),
    )
}
fn inspect_bytes(p: &mut Vec<Instruction>, machine: u32, durable: bool, save: &str) {
    literal(
        p,
        Command::Inspect(Inspect::Storage {
            machine: mid(machine),
            durable,
        }),
        save,
    )
}
fn lifecycle(p: &mut Vec<Instruction>, machine: u32, reboot: bool) {
    literal(
        p,
        if reboot {
            Command::Reboot {
                machine: mid(machine),
            }
        } else {
            Command::Crash {
                machine: mid(machine),
            }
        },
        "lifecycle",
    )
}
fn await_local(p: &mut Vec<Instruction>, owner: &str, save: &str) {
    push(p, Template::Await(owner.into()), save)
}
fn block(p: &mut Vec<Instruction>, machine: u32, op: BlockOp, delay: u64, save: &str) {
    literal(
        p,
        Command::Block {
            machine: mid(machine),
            op,
            delay_ticks: Tick(delay),
        },
        save,
    )
}
fn links_program() -> Vec<Instruction> {
    let mut p = vec![];
    sleep(&mut p, 60);
    fault(&mut p, loss(Scope::Az(2), Scope::Az(1), 1, 1), "az");
    fault(&mut p, loss(Scope::Az(3), Scope::Az(1), 1, 2), "partial");
    fault(
        &mut p,
        loss(Scope::Machine(mid(3)), Scope::Machine(mid(1)), 1, 1),
        "overlap",
    );
    send(&mut p, 4, 2, 10, None, "blocked");
    send(&mut p, 2, 4, 11, None, "reverse");
    let mut samples = vec![];
    for b in 0..32 {
        let local = format!("sample{b}");
        send(&mut p, 5, 1, b, None, &local);
        samples.push(local);
    }
    sleep(&mut p, 5);
    inspect_frame(&mut p, "blocked", "blocked_seen");
    inspect_frame(&mut p, "reverse", "reverse_seen");
    check(
        &mut p,
        "directed_partition_drops_AZ2_to_AZ1",
        delivered("blocked_seen", false),
    );
    check(
        &mut p,
        "directed_partition_preserves_reverse_delivery",
        delivered("reverse_seen", true),
    );
    let mut observed = vec![];
    for (i, s) in samples.iter().enumerate() {
        let local = format!("seen{i}");
        inspect_frame(&mut p, s, &local);
        observed.push(local);
    }
    push(
        &mut p,
        Template::PartialNote(observed.clone()),
        "partial_note",
    );
    check(
        &mut p,
        "partial_AZ3_to_AZ1_actual_loss_and_delivery",
        Predicate::Partial { locals: observed },
    );
    heal(&mut p, "az");
    send(&mut p, 3, 1, 12, None, "still");
    send(&mut p, 4, 2, 13, None, "allowed");
    sleep(&mut p, 5);
    inspect_frame(&mut p, "still", "still_seen");
    inspect_frame(&mut p, "allowed", "allowed_seen");
    check(
        &mut p,
        "healing_one_handle_preserves_overlapping_machine_fault",
        delivered("still_seen", false),
    );
    check(
        &mut p,
        "healed_AZ_route_delivers",
        delivered("allowed_seen", true),
    );
    heal(&mut p, "overlap");
    heal(&mut p, "partial");
    send(&mut p, 3, 1, 14, None, "recovered");
    sleep(&mut p, 5);
    inspect_frame(&mut p, "recovered", "recovered_seen");
    check(
        &mut p,
        "healed_link_delivers_actual_payload",
        Predicate::FramePayload {
            local: "recovered_seen".into(),
            bytes: frame(0, RequestId(0), &[14]),
        },
    );
    put(&mut p, 1, 7);
    fault(
        &mut p,
        loss(Scope::Az(0), Scope::Az(1), 1, 1),
        "health_fault",
    );
    health(&mut p, 1, "faulted_health");
    check(
        &mut p,
        "immune_control_health_obeys_faultable_fabric",
        completion("faulted_health", CompletionKind::Timeout),
    );
    heal(&mut p, "health_fault");
    health(&mut p, 1, "healthy");
    check(
        &mut p,
        "health_recovers_through_healed_path",
        Predicate::Health {
            local: "healthy".into(),
            generation: Some(1),
            value: 7,
        },
    );
    p
}
fn storage_program() -> Vec<Instruction> {
    let mut p = vec![];
    block(
        &mut p,
        1,
        BlockOp::Write {
            offset: 0,
            data: 17u64.to_le_bytes().to_vec(),
        },
        12,
        "write17",
    );
    block(&mut p, 1, BlockOp::Flush, 1, "flush17");
    await_local(&mut p, "flush17", "flushed");
    check(
        &mut p,
        "flush_callback_arrives",
        completion("flushed", CompletionKind::Flush),
    );
    block(
        &mut p,
        1,
        BlockOp::Write {
            offset: 0,
            data: 18u64.to_le_bytes().to_vec(),
        },
        1,
        "write18",
    );
    await_local(&mut p, "write18", "written");
    check(
        &mut p,
        "unflushed_ack_exercised",
        completion("written", CompletionKind::Write),
    );
    inspect_bytes(&mut p, 1, false, "cache18");
    check(
        &mut p,
        "unflushed_cache_has_new_data",
        Predicate::Bytes {
            local: "cache18".into(),
            bytes: 18u64.to_le_bytes().to_vec(),
        },
    );
    block(
        &mut p,
        1,
        BlockOp::Write {
            offset: 0,
            data: 99u64.to_le_bytes().to_vec(),
        },
        30,
        "old_write",
    );
    literal(
        &mut p,
        Command::Dma {
            machine: mid(1),
            offset: 0,
            data: vec![b'X'; 8],
            delay_ticks: Tick(30),
        },
        "old_dma",
    );
    send(&mut p, 1, 2, 77, Some(30), "wire");
    block(
        &mut p,
        3,
        BlockOp::WriteThrough {
            offset: 0,
            data: 55u64.to_le_bytes().to_vec(),
        },
        30,
        "media",
    );
    lifecycle(&mut p, 3, false);
    lifecycle(&mut p, 3, true);
    lifecycle(&mut p, 1, false);
    lifecycle(&mut p, 2, false);
    health(&mut p, 1, "down");
    check(
        &mut p,
        "controller_survives_crash_and_down_query",
        completion("down", CompletionKind::Timeout),
    );
    lifecycle(&mut p, 1, true);
    lifecycle(&mut p, 2, true);
    literal(
        &mut p,
        Command::Dma {
            machine: mid(1),
            offset: 0,
            data: vec![b'N'; 8],
            delay_ticks: Tick(1),
        },
        "fresh_dma",
    );
    await_local(&mut p, "fresh_dma", "fresh_dma_done");
    health(&mut p, 1, "reboot_health");
    check(
        &mut p,
        "reboot_reads_flushed_data_and_loses_unflushed_ack",
        Predicate::Health {
            local: "reboot_health".into(),
            generation: Some(2),
            value: 17,
        },
    );
    sleep(&mut p, 35);
    inspect_bytes(&mut p, 1, false, "cache17");
    check(
        &mut p,
        "obsolete_disk_completion_cannot_mutate_new_cache",
        Predicate::Bytes {
            local: "cache17".into(),
            bytes: 17u64.to_le_bytes().to_vec(),
        },
    );
    literal(
        &mut p,
        Command::Inspect(Inspect::Memory { machine: mid(1) }),
        "memory",
    );
    check(
        &mut p,
        "obsolete_DMA_cannot_overwrite_new_incarnation",
        Predicate::Bytes {
            local: "memory".into(),
            bytes: vec![b'N'; 8],
        },
    );
    literal(&mut p, Command::Inspect(Inspect::StaleCount), "stale");
    check(
        &mut p,
        "obsolete_owners_were_actually_rejected",
        Predicate::Stale {
            local: "stale".into(),
            minimum: 2,
        },
    );
    inspect_frame(&mut p, "wire", "wire_seen");
    check(
        &mut p,
        "wire_survives_sender_and_receiver_reboot",
        Predicate::FramePayload {
            local: "wire_seen".into(),
            bytes: frame(0, RequestId(0), &[77]),
        },
    );
    inspect_bytes(&mut p, 3, true, "durable55");
    check(
        &mut p,
        "physical_media_write_survives_reboot_without_old_guest_callback",
        Predicate::Bytes {
            local: "durable55".into(),
            bytes: 55u64.to_le_bytes().to_vec(),
        },
    );
    await_local(&mut p, "media", "media_callback");
    check(
        &mut p,
        "surviving_media_callback_rejects_old_incarnation",
        completion("media_callback", CompletionKind::Obsolete),
    );
    block(&mut p, 1, BlockOp::Read { offset: 0, len: 8 }, 1, "read");
    await_local(&mut p, "read", "read_done");
    check(
        &mut p,
        "offset_read_returns_recovered_bytes",
        completion(
            "read_done",
            CompletionKind::Read(17u64.to_le_bytes().to_vec()),
        ),
    );
    p
}
fn quorum(p: &mut Vec<Instruction>, value: u64, down: Option<u32>) {
    let mut locals = vec![];
    for machine in 1..=3 {
        let local = format!("quorum{machine}");
        health(p, machine, &local);
        if down == Some(machine) {
            check(
                p,
                "down_node_health_request_times_out",
                completion(&local, CompletionKind::Timeout),
            );
        } else {
            check(
                p,
                "healthy_replica_reports_actual_expected_data",
                Predicate::Health {
                    local: local.clone(),
                    generation: None,
                    value,
                },
            );
        }
        locals.push(local);
    }
    push(
        p,
        Template::QuorumNote {
            locals: locals.clone(),
            value,
            down: down.map(mid),
        },
        "quorum_note",
    );
    check(
        p,
        "service_read_available_with_two_matching_durable_replicas",
        Predicate::Quorum {
            locals,
            value,
            minimum: 2,
        },
    );
}
fn rolling_program(skip_recovery: bool) -> Vec<Instruction> {
    let mut p = vec![];
    let mut value = 100;
    for machine in 1..=3 {
        put(&mut p, machine, value);
    }
    quorum(&mut p, value, None);
    for down in 1..=3 {
        literal(
            &mut p,
            Command::Note {
                label: "rolling_phase".into(),
                detail: json!({"machine":down,"expected":value,"stage":"crash"}),
            },
            "phase",
        );
        lifecycle(&mut p, down, false);
        quorum(&mut p, value, Some(down));
        value += 1;
        for machine in 1..=3 {
            if machine != down {
                put(&mut p, machine, value);
            }
        }
        quorum(&mut p, value, Some(down));
        lifecycle(&mut p, down, true);
        let peer = if down == 1 { 2 } else { 1 };
        health(&mut p, peer, "source");
        check(
            &mut p,
            "recovery_source_returns_latest_durable_data",
            Predicate::Health {
                local: "source".into(),
                generation: None,
                value,
            },
        );
        if !skip_recovery {
            push(
                &mut p,
                Template::PutFromHealth {
                    machine: mid(down),
                    source: "source".into(),
                },
                "recovery_put",
            );
            check(
                &mut p,
                "recovery_flush_ack_contains_peer_data",
                completion(
                    "recovery_put",
                    CompletionKind::Reply(value.to_le_bytes().to_vec()),
                ),
            );
        }
        health(&mut p, down, "recovered");
        check(
            &mut p,
            "rejoined_replica_recovers_current_data_before_next_reboot",
            Predicate::Health {
                local: "recovered".into(),
                generation: Some(2),
                value,
            },
        );
        quorum(&mut p, value, None);
    }
    p
}
fn order_program() -> Vec<Instruction> {
    let mut p = vec![];
    literal(
        &mut p,
        Command::ArmSleep {
            after_ticks: Tick(20),
        },
        "wake",
    );
    send(&mut p, 1, 2, 88, Some(20), "wire");
    await_local(&mut p, "wake", "awake");
    fault(
        &mut p,
        loss(Scope::Machine(mid(1)), Scope::Machine(mid(2)), 1, 1),
        "fault",
    );
    sleep(&mut p, 1);
    inspect_frame(&mut p, "wire", "seen");
    check(
        &mut p,
        "equal_time_controller_continuation_precedes_later_wire",
        delivered("seen", false),
    );
    heal(&mut p, "fault");
    p
}
fn execute(seed: u64, case: &str, defects: Defects, program: Vec<Instruction>) -> Value {
    let mut b = Bundle::new(seed, defects, program);
    let result = b.run();
    b.report(case, result)
}
fn continuation(seed: u64) -> Value {
    let mut p = vec![];
    sleep(&mut p, 5);
    fault(&mut p, loss(Scope::Az(0), Scope::Az(1), 1, 1), "fault");
    health(&mut p, 1, "faulted");
    check(
        &mut p,
        "restored_query_observes_correlated_fault",
        completion("faulted", CompletionKind::Timeout),
    );
    heal(&mut p, "fault");
    health(&mut p, 1, "healed");
    check(
        &mut p,
        "restored_query_recovers_after_heal",
        Predicate::Health {
            local: "healed".into(),
            generation: Some(1),
            value: 0,
        },
    );
    let mut original = Bundle::new(seed, Defects::default(), p);
    let mut checkpoints = vec![];
    for wanted in [0usize, 1, 2] {
        while !(original.controller.pc == wanted && original.controller.pending.is_some()) {
            original.advance().unwrap();
        }
        if wanted != 1 {
            original.advance().unwrap();
        } // Sleep/query are already waiting on events.
        let bytes = original.encode();
        let prefix = original.world.trace().len();
        let mut uninterrupted = original.clone();
        let mut resumed = Bundle::decode(&bytes).unwrap();
        assert_eq!(uninterrupted.run(), Ok(()));
        assert_eq!(resumed.run(), Ok(()));
        assert_eq!(uninterrupted.encode(), resumed.encode());
        assert_eq!(
            &uninterrupted.world.trace()[prefix..],
            &resumed.world.trace()
        );
        checkpoints.push(json!({"pc":wanted,"command":original.controller.program[wanted],"snapshot_sha256":format!("{:x}",Sha256::digest(&bytes)),"snapshot_bytes":bytes.len(),"state":serde_json::from_slice::<Value>(&bytes).unwrap(),"future_trace_entries":resumed.world.trace().len(),"final_state_sha256":format!("{:x}",Sha256::digest(resumed.encode()))}));
        if wanted == 1 {
            let mut lost = original.clone();
            assert!(lost.world.lose_pending_command_control());
            assert_eq!(
                Bundle::decode(&lost.encode()).unwrap_err(),
                "pending control command lost"
            );
        }
        // Preserve this native continuation and advance to the next checkpoint.
        original.advance().unwrap();
    }
    assert_eq!(original.run(), Ok(()));
    json!({"case":"encode_decode_resume_sleep_fault_query","result":{"Ok":null},"checkpoints":checkpoints,"lost_pending_command_control":"pending control command lost","final_state":serde_json::to_value(&original).unwrap()})
}
fn suite(seed: u64) -> Value {
    let positives = vec![
        execute(
            seed,
            "AZ_partition_partial_loss_heal",
            Defects::default(),
            links_program(),
        ),
        execute(
            seed,
            "crash_pending_io_durability_ownership",
            Defects::default(),
            storage_program(),
        ),
        execute(
            seed,
            "rolling_three_replica_service",
            Defects::default(),
            rolling_program(false),
        ),
        execute(
            seed,
            "equal_time_continuation",
            Defects::default(),
            order_program(),
        ),
    ];
    for v in &positives {
        assert_eq!(
            v["result"],
            json!({"Ok":null}),
            "positive failed: {}",
            v["case"]
        );
    }
    let negatives = vec![
        (
            execute(
                seed,
                "overlap",
                Defects {
                    erase_overlap: true,
                    ..Default::default()
                },
                links_program(),
            ),
            "healing_one_handle_preserves_overlapping_machine_fault",
        ),
        (
            execute(
                seed,
                "early_flush",
                Defects {
                    early_flush: true,
                    ..Default::default()
                },
                storage_program(),
            ),
            "reboot_reads_flushed_data_and_loses_unflushed_ack",
        ),
        (
            execute(
                seed,
                "stale_owner",
                Defects {
                    ignore_owner: true,
                    ..Default::default()
                },
                storage_program(),
            ),
            "obsolete_disk_completion_cannot_mutate_new_cache",
        ),
        (
            execute(
                seed,
                "wire",
                Defects {
                    cancel_wire: true,
                    ..Default::default()
                },
                storage_program(),
            ),
            "wire_survives_sender_and_receiver_reboot",
        ),
        (
            execute(
                seed,
                "physical",
                Defects {
                    cancel_media: true,
                    ..Default::default()
                },
                storage_program(),
            ),
            "physical_media_write_survives_reboot_without_old_guest_callback",
        ),
        (
            execute(seed, "rolling", Defects::default(), rolling_program(true)),
            "rejoined_replica_recovers_current_data_before_next_reboot",
        ),
        (
            execute(
                seed,
                "batch",
                Defects {
                    batch_events: true,
                    ..Default::default()
                },
                order_program(),
            ),
            "equal_time_controller_continuation_precedes_later_wire",
        ),
    ];
    for (v, label) in &negatives {
        assert_eq!(
            v["result"]["Err"], *label,
            "negative failed for wrong reason"
        );
    }
    json!({"schema":2,"seed":seed,"scope":"native serializable command/observation controller and scripted service; no executed guest VM or hardware preemption","time_policy":"branch-count-domain integer ticks; fixture idle event jumps; rational Linux-ns rate is declared integration input, not measured physical time","sources":{"lib_sha256":format!("{:x}",Sha256::digest(include_bytes!("lib.rs"))),"protocol_sha256":format!("{:x}",Sha256::digest(include_bytes!("protocol.rs"))),"client_sha256":format!("{:x}",Sha256::digest(include_bytes!("client.rs"))),"fixture_sha256":format!("{:x}",Sha256::digest(include_bytes!("main.rs")))},"positives":positives,"negatives":negatives.iter().map(|(v,_)|v).collect::<Vec<_>>(),"continuation":continuation(seed)})
}
fn main() {
    let seed = std::env::args()
        .nth(1)
        .map(|x| x.parse().unwrap())
        .unwrap_or(42);
    println!("{}", serde_json::to_string(&suite(seed)).unwrap());
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn protocol_scenarios_and_sensitive_controls() {
        suite(42);
    }
    #[test]
    fn restored_continuations_match() {
        continuation(42);
    }
    #[test]
    fn changed_seed_changes_real_deliveries() {
        let a = execute(42, "links", Defects::default(), links_program());
        let b = execute(43, "links", Defects::default(), links_program());
        let obs = |v: &Value| {
            v["trace"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["event"] == "partial_loss_observation")
                .unwrap()["detail"]
                .clone()
        };
        assert_ne!(obs(&a), obs(&b));
    }
}
