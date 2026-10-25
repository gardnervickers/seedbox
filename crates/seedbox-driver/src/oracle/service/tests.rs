use super::*;
struct Owner {
    facts: Facts,
    next: u32,
    commands: BTreeMap<(u64, u32), (u32, CommandKind, bool)>,
    retired: std::collections::BTreeSet<(u64, u32)>,
    draws: std::collections::VecDeque<u64>,
}
impl Default for Owner {
    fn default() -> Self {
        Self {
            facts: Facts {
                name: "app".into(),
                boot: 0,
                vm: Vm::Up,
                process: Process::Running,
                physically_ended: false,
                ready: true,
            },
            next: 40,
            commands: BTreeMap::new(),
            retired: Default::default(),
            draws: Default::default(),
        }
    }
}
impl Owner {
    fn accept(&mut self, plans: &[Plan]) {
        for p in plans {
            if let Plan::Effect {
                scope: Scope::Machine { boot, .. },
                command: Some(id),
                ..
            } = p
            {
                self.commands.get_mut(&(*boot, id.get())).unwrap().2 = true;
            }
        }
    }
}
impl Adapter for Owner {
    fn draw(&mut self, _: &str) -> Result<u64, OperationError> {
        Ok(self.draws.pop_front().expect("scripted seeded draw"))
    }
    fn resolve(&self, target: &Target) -> Result<Facts, OperationError> {
        match target {
            Target::Machine(n) | Target::Service(n) | Target::Key(n)
                if n == "app" || n == "service" =>
            {
                Ok(self.facts.clone())
            }
            _ => Err(OperationError::invalid("unknown target")),
        }
    }
    fn network_member(&self, target: &Target) -> Result<String, OperationError> {
        match target {
            Target::Machine(n) | Target::Service(n) | Target::Key(n)
                if ["a", "b", "c", "app"].contains(&n.as_str()) =>
            {
                Ok(n.clone())
            }
            Target::Machine(n) if n == "later" => Err(error(
                ErrorCode::TargetUnavailable,
                "partition member is not spawned",
            )),
            Target::Machine(n) if n == "bare" => Err(OperationError::invalid(
                "partition member has no network interface",
            )),
            _ => Err(OperationError::invalid("unknown target")),
        }
    }
    fn command_id(
        &mut self,
        _: &str,
        boot: u64,
        fault: u32,
        action: CommandKind,
    ) -> Result<NonZeroU32, String> {
        assert_eq!(boot, self.facts.boot);
        self.next += 1;
        self.commands
            .insert((boot, self.next), (fault, action, false));
        Ok(NonZeroU32::new(self.next).unwrap())
    }
    fn command_owner(
        &self,
        target: &str,
        boot: u64,
        id: NonZeroU32,
    ) -> Result<CommandOwner, String> {
        if target != "app" {
            return Err("unknown acknowledgment target".into());
        }
        if boot < self.facts.boot {
            return Ok(CommandOwner::RetiredBoot);
        }
        if boot != self.facts.boot {
            return Err("unknown boot".into());
        }
        if self.retired.contains(&(boot, id.get())) {
            return Ok(CommandOwner::RetiredApplication);
        }
        if id.get() == 40 {
            return Ok(CommandOwner::External);
        }
        self.commands
            .get(&(boot, id.get()))
            .filter(|(_, _, accepted)| *accepted)
            .map(|&(fault, action, _)| CommandOwner::Service { fault, action })
            .ok_or_else(|| "unissued command".into())
    }
    fn inspect(
        &self,
        _: &[String],
        _: &[super::super::triggers::Record],
    ) -> Result<Value, OperationError> {
        Err(OperationError::unsupported())
    }
}
fn id(request: u32) -> SimulationControlId {
    SimulationControlId {
        session: NonZeroU32::new(1).unwrap(),
        request: NonZeroU32::new(request).unwrap(),
    }
}
fn call(s: &mut Service, o: &mut Owner, n: u32, at: u64, op: Value) -> Vec<Plan> {
    let mut v = op;
    v["version"] = json!(1);
    v["id"] = json!(n);
    let plans = s
        .admit(
            o,
            "caller".into(),
            id(n),
            at,
            at,
            &serde_json::to_vec(&v).unwrap(),
        )
        .unwrap();
    o.accept(&plans);
    plans
}
fn reply(plans: &[Plan]) -> Value {
    let payload = plans
        .iter()
        .find_map(|p| {
            if let Plan::Reply { payload, .. } = p {
                Some(payload)
            } else {
                None
            }
        })
        .unwrap_or_else(|| panic!("not reply: {plans:?}"));
    serde_json::from_slice(payload).unwrap()
}
fn settlement(plans: &[Plan]) -> &Settlement {
    plans
        .iter()
        .find_map(|p| {
            if let Plan::Settled(s) = p {
                Some(s)
            } else {
                None
            }
        })
        .expect("settlement")
}
fn fault(plans: &[Plan]) -> u32 {
    let Plan::Effect { fault, .. } = plans[0] else {
        panic!()
    };
    fault
}
#[test]
fn now_sleep_use_commit_and_survive_source_loss() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    let p = s
        .admit(
            &mut o,
            "caller".into(),
            id(1),
            3,
            10,
            br#"{"version":1,"id":1,"op":"now"}"#,
        )
        .unwrap();
    let r = reply(&p);
    assert_eq!(r["requested_at_ns"], 3);
    assert_eq!(r["admitted_at_ns"], 10);
    assert_eq!(r["result"]["now_ns"], 10);
    let p = call(
        &mut s,
        &mut o,
        2,
        10,
        json!({"op":"sleep","duration_ns":20}),
    );
    let Plan::Wake { token, at } = p[0] else {
        panic!()
    };
    assert_eq!(at, 30);
    s.source_reset("caller").unwrap();
    let p = s.wake(token, 30).unwrap();
    assert_eq!(reply(&[p])["result"], json!({}));
}
#[test]
fn process_waits_ready_and_exact_correlated_completion() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    o.facts.ready = false;
    let p = call(
        &mut s,
        &mut o,
        1,
        10,
        json!({"op":"stop","target":{"service":"service"}}),
    );
    assert!(matches!(p[0], Plan::Deadline { .. }));
    assert_eq!(o.next, 40);
    o.facts.ready = true;
    let p = s.ready(&mut o, "app", 0, 12, 12).unwrap();
    o.accept(&p);
    let Plan::Effect {
        command: Some(command),
        effect: Effect::Process(ApplicationCommand::Stop),
        ..
    } = p[0]
    else {
        panic!()
    };
    assert_eq!(command.get(), 41);
    assert!(s
        .acknowledged(
            &o,
            "app",
            0,
            NonZeroU32::new(40).unwrap(),
            CommandResult::Completed,
            13,
            13
        )
        .unwrap()
        .is_empty());
    let r = reply(
        &s.acknowledged(&o, "app", 0, command, CommandResult::Completed, 14, 14)
            .unwrap(),
    );
    assert_eq!(r["effect_at_ns"], 14);
    assert_eq!(r["ok"], true);
}
#[test]
fn target_end_uses_first_transition_and_never_new_boot() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    call(
        &mut s,
        &mut o,
        1,
        10,
        json!({"op":"pause_process","target":{"machine":"app"}}),
    );
    let r = reply(&s.ended("app", 0, 12, 12).unwrap());
    assert_eq!(r["error"]["code"], "target_unavailable");
    assert_eq!(r["replied_at_ns"], 12);
    assert_eq!(r["effect_at_ns"], 12);
    o.facts.boot = 1;
    assert!(s.ready(&mut o, "app", 1, 100, 100).unwrap().is_empty());
}
#[test]
fn throttle_heal_only_confirm_after_effect_receipts() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    let p = call(
        &mut s,
        &mut o,
        1,
        10,
        json!({"op":"throttle","target":{"machine":"app"},"factor_permille":2000}),
    );
    assert!(s.handles.active_handles().unwrap().is_empty());
    let rejected = call(
        &mut s,
        &mut o,
        2,
        10,
        json!({"op":"throttle","target":{"service":"service"},"factor_permille":3000}),
    );
    assert_eq!(reply(&rejected)["error"]["code"], "invalid_state");
    let r = reply(&s.applied(fault(&p), 10, 10).unwrap());
    assert_eq!(r["result"]["handle"], "p1");
    o.facts.vm = Vm::Down;
    o.facts.boot = 1; // execution persists across machine lifecycle
    let heal = call(&mut s, &mut o, 3, 20, json!({"op":"heal","handle":"p1"}));
    assert!(
        matches!(heal[0],Plan::Effect{effect:Effect::Execution(c),..}if c==ExecutionConfig::default())
    );
    let r = s
        .rejected(
            fault(&heal),
            20,
            20,
            error(ErrorCode::InvalidState, "known unapplied"),
        )
        .unwrap();
    assert_eq!(reply(&r)["ok"], false);
    assert_eq!(s.handles.active_handles().unwrap(), ["p1"]);
    let heal = call(&mut s, &mut o, 4, 20, json!({"op":"heal","handle":"p1"}));
    s.applied(fault(&heal), 20, 20).unwrap();
    assert!(s.handles.active_handles().unwrap().is_empty());
}
#[test]
fn failures_do_not_fabricate_success_or_continue() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    let p = call(
        &mut s,
        &mut o,
        1,
        1,
        json!({"op":"stop","target":{"machine":"app"}}),
    );
    let Plan::Effect {
        command: Some(c), ..
    } = p[0]
    else {
        panic!()
    };
    assert!(s
        .acknowledged(&o, "app", 0, c, CommandResult::Rejected, 2, 2)
        .is_err());
    assert!(s
        .admit(
            &mut o,
            "caller".into(),
            id(2),
            2,
            2,
            br#"{"version":1,"id":2,"op":"now"}"#
        )
        .is_err());
}
#[test]
fn batch_trigger_arm_order_and_no_retroactive_effect() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    for n in 1..=2 {
        let p = call(
            &mut s,
            &mut o,
            n,
            1,
            json!({"op":"arm_trigger","source":{"machine":"app"},"marker":[n],"match_timeout_ns":100,"fault":{"action":"stop","target":{"machine":"app"}}}),
        );
        assert_eq!(reply(&p)["result"]["trigger_id"], n);
    }
    s.committed_bytes("app", 3, &[2], 10).unwrap();
    s.committed_bytes("app", 4, &[1], 10).unwrap();
    assert!(s.trigger_effects(&mut o, 9).unwrap().is_empty());
    let p = s.trigger_effects(&mut o, 10).unwrap();
    assert!(matches!(p[0], Plan::Effect { at: 10, .. }));
    assert_eq!(s.triggers.records()[0].fault_id, Some(fault(&p)));
    assert!(s.trigger_effects(&mut o, 10).unwrap().is_empty());
}
#[test]
fn unsupported_projection_and_invalid_target_have_no_effect() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    assert_eq!(
        reply(&call(&mut s, &mut o, 1, 0, json!({"op":"inspect"})))["error"]["code"],
        "unsupported"
    );
    let p = call(
        &mut s,
        &mut o,
        3,
        0,
        json!({"op":"throttle","target":{"machine":"missing"},"factor_permille":2000}),
    );
    assert_eq!(reply(&p)["error"]["code"], "invalid_request");
    assert!(s.pending.is_empty());
    assert!(s.handles.active_handles().unwrap().is_empty());
}

#[test]
fn lifecycle_state_policy_and_receipts_are_distinct_from_enqueue() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    let p = call(
        &mut s,
        &mut o,
        1,
        10,
        json!({"op":"pause","target":{"machine":"app"}}),
    );
    assert_eq!(o.facts.vm, Vm::Up);
    assert!(matches!(
        p[0],
        Plan::Effect {
            effect: Effect::Pause,
            ..
        }
    ));
    o.facts.vm = Vm::Paused;
    assert_eq!(reply(&s.applied(fault(&p), 10, 10).unwrap())["ok"], true);
    assert_eq!(
        reply(&call(
            &mut s,
            &mut o,
            2,
            10,
            json!({"op":"stop","target":{"machine":"app"}})
        ))["error"]["code"],
        "invalid_state"
    );
    let p = call(
        &mut s,
        &mut o,
        3,
        11,
        json!({"op":"crash","target":{"machine":"app"}}),
    );
    o.facts.vm = Vm::Down;
    o.facts.boot = 1;
    s.applied(fault(&p), 11, 11).unwrap();
    let p = call(
        &mut s,
        &mut o,
        4,
        12,
        json!({"op":"reboot","target":{"machine":"app"}}),
    );
    o.facts.vm = Vm::Up;
    o.facts.boot = 2;
    s.applied(fault(&p), 12, 12).unwrap();
    assert_eq!(
        reply(&call(
            &mut s,
            &mut o,
            5,
            12,
            json!({"op":"reboot","target":{"machine":"app"}})
        ))["error"]["code"],
        "invalid_state"
    );
}
#[test]
fn concurrent_process_effects_deadline_and_early_wake_guards() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    let p = call(
        &mut s,
        &mut o,
        1,
        10,
        json!({"op":"stop","target":{"machine":"app"}}),
    );
    // A restart may follow the stop before its acknowledgment.
    let restart = call(
        &mut s,
        &mut o,
        2,
        10,
        json!({"op":"restart","target":{"service":"service"}}),
    );
    assert_ne!(fault(&restart), fault(&p));
    assert!(s.applied(fault(&p), 10, 10).is_err()); // cannot bypass guest ack
    assert!(s.deadline(fault(&p), 11).is_err());
    assert!(!s.poisoned);
    assert!(s
        .deadline(fault(&p), 10 + PROCESS_EFFECT_TIMEOUT_NS)
        .is_err());
    assert!(s.poisoned);
    let (mut s, mut o) = (Service::default(), Owner::default());
    let p = call(&mut s, &mut o, 1, 10, json!({"op":"sleep","duration_ns":5}));
    let Plan::Wake { token, .. } = p[0] else {
        panic!()
    };
    assert!(s.wake(token, 14).is_err());
    assert!(s.wake(token, 15).is_ok());
    assert!(s.wake(token, 15).is_err());
}
#[test]
fn service_copy_retains_owned_pending_work_without_machine_snapshot_claim() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    let p = call(
        &mut s,
        &mut o,
        1,
        10,
        json!({"op":"stop","target":{"machine":"app"}}),
    );
    let Plan::Effect {
        command: Some(command),
        ..
    } = p[0]
    else {
        panic!()
    };
    let mut copy = s.clone();
    s.source_reset("caller").unwrap();
    let first = s
        .acknowledged(&o, "app", 0, command, CommandResult::Completed, 11, 11)
        .unwrap();
    let second = copy
        .acknowledged(&o, "app", 0, command, CommandResult::Completed, 11, 11)
        .unwrap();
    assert_eq!(reply(&first), reply(&second));
    assert!(copy.pending.is_empty());
}

#[test]
fn earlier_observations_only_emit_at_current_commit() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    o.facts.ready = false;
    call(
        &mut s,
        &mut o,
        1,
        10,
        json!({"op":"stop","target":{"machine":"app"}}),
    );
    o.facts.ready = true;
    let p = s.ready(&mut o, "app", 0, 11, 20).unwrap();
    o.accept(&p);
    let Plan::Effect {
        command: Some(c),
        at: 20,
        ..
    } = p[0]
    else {
        panic!()
    };
    let plans = s
        .acknowledged(&o, "app", 0, c, CommandResult::Completed, 22, 30)
        .unwrap();
    assert!(plans
        .iter()
        .any(|p| matches!(p, Plan::Reply { at: 30, .. })));
    assert_eq!(reply(&plans)["effect_at_ns"], 22);
    assert_eq!(reply(&plans)["replied_at_ns"], 30);
    call(
        &mut s,
        &mut o,
        2,
        30,
        json!({"op":"stop","target":{"machine":"app"}}),
    );
    let plans = s.ended("app", 0, 32, 40).unwrap();
    assert!(plans
        .iter()
        .any(|p| matches!(p, Plan::Reply { at: 40, .. })));
    let r = reply(&plans);
    assert_eq!(r["effect_at_ns"], 32);
    assert_eq!(r["replied_at_ns"], 40);
    assert_eq!(r["error"]["code"], "target_unavailable");
}

#[test]
fn down_process_commands_are_unavailable_regardless_of_process_phase() {
    for phase in [Process::Running, Process::Stopped, Process::Frozen] {
        for op in ["stop", "restart", "pause_process", "resume_process"] {
            let (mut s, mut o) = (Service::default(), Owner::default());
            o.facts.vm = Vm::Down;
            o.facts.physically_ended = true; // retained new-Machine crash reason
            o.facts.process = phase;
            let p = call(
                &mut s,
                &mut o,
                1,
                10,
                json!({"op":op,"target":{"machine":"app"}}),
            );
            let r = reply(&p);
            assert_eq!(r["error"]["code"], "target_unavailable");
            assert_eq!(r["effect_at_ns"], 10);
            assert!(s.pending.is_empty());
            assert_eq!(s.last, 1);
        }
    }
}
#[test]
fn unknown_and_mismatched_service_acks_poison_but_known_external_is_ignored() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    let p = call(
        &mut s,
        &mut o,
        1,
        10,
        json!({"op":"stop","target":{"machine":"app"}}),
    );
    let Plan::Effect {
        command: Some(c), ..
    } = p[0]
    else {
        panic!()
    };
    o.commands.get_mut(&(0, c.get())).unwrap().1 =
        CommandKind::Application(ApplicationCommand::Resume);
    assert!(s
        .acknowledged(&o, "app", 0, c, CommandResult::Completed, 11, 20)
        .is_err());
    assert!(s.poisoned);
    let mut s = Service::default();
    assert!(s
        .acknowledged(
            &o,
            "app",
            0,
            NonZeroU32::new(999).unwrap(),
            CommandResult::Completed,
            11,
            20
        )
        .is_err());
    assert!(s.poisoned);
}

#[test]
fn shared_allocator_excludes_future_external_ids_and_unaccepted_receipts() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    o.next = 41; // 41 reserved by an external future timeline input, not admitted.
    let p = s
        .admit(
            &mut o,
            "caller".into(),
            id(1),
            10,
            10,
            br#"{"version":1,"id":1,"op":"stop","target":{"machine":"app"}}"#,
        )
        .unwrap();
    let Plan::Effect {
        command: Some(c), ..
    } = p[0]
    else {
        panic!()
    };
    assert_eq!(c.get(), 42);
    assert!(s
        .acknowledged(&o, "app", 0, c, CommandResult::Completed, 11, 20)
        .is_err());
    assert!(s.poisoned);
    let mut s = Service::default();
    o.facts.boot = 1;
    assert!(s
        .acknowledged(&o, "app", 0, c, CommandResult::Completed, 11, 20)
        .unwrap()
        .is_empty());
    assert!(!s.poisoned);
}

#[test]
fn acknowledgment_cannot_precede_dispatch_commit() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    o.facts.ready = false;
    call(
        &mut s,
        &mut o,
        1,
        10,
        json!({"op":"stop","target":{"machine":"app"}}),
    );
    o.facts.ready = true;
    let p = s.ready(&mut o, "app", 0, 11, 20).unwrap();
    o.accept(&p);
    let Plan::Effect {
        command: Some(c), ..
    } = p[0]
    else {
        panic!()
    };
    assert!(s
        .acknowledged(&o, "app", 0, c, CommandResult::Completed, 12, 30)
        .is_err());
    assert!(s.poisoned);
}
#[test]
fn sleep_wakes_do_not_consume_fault_namespace() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    call(
        &mut s,
        &mut o,
        1,
        1,
        json!({"op":"sleep","duration_ns":100}),
    );
    call(
        &mut s,
        &mut o,
        2,
        1,
        json!({"op":"arm_trigger","source":{"machine":"app"},"marker":[1],"match_timeout_ns":100,"fault":{"action":"stop","target":{"machine":"app"}}}),
    );
    s.committed_bytes("app", 3, &[1], 10).unwrap();
    let p = s.trigger_effects(&mut o, 10).unwrap();
    assert_eq!(fault(&p), 1);
    assert_eq!(s.triggers.records()[0].fault_id, Some(1));
}

#[test]
fn physical_peer_exit_preflight_precedes_policy_and_preserves_crash_reboot_path() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    // A peer machine physically exits while the oracle continues. The API's
    // world_windowed admission wrapper preflights physical availability before
    // Faults checks the retained Up/Running policy flags.
    o.facts.physically_ended = true;
    assert!(s.ended("app", 0, 8, 10).unwrap().is_empty());
    assert_eq!(o.facts.vm, Vm::Up);
    assert_eq!(o.facts.process, Process::Running);
    for (i, op) in [
        "restart",
        "pause",
        "resume",
        "stop",
        "pause_process",
        "resume_process",
    ]
    .into_iter()
    .enumerate()
    {
        let p = call(
            &mut s,
            &mut o,
            i as u32 + 1,
            10,
            json!({"op":op,"target":{"machine":"app"}}),
        );
        let r = reply(&p);
        assert_eq!(r["error"]["code"], "target_unavailable");
        assert_eq!(r["effect_at_ns"], 10);
        assert_eq!(r["replied_at_ns"], 10);
        assert_eq!(o.facts.vm, Vm::Up);
        assert_eq!(o.facts.process, Process::Running);
    }
    assert_eq!(o.next, 40);
    assert_eq!(s.last, 0);
    let p = call(
        &mut s,
        &mut o,
        7,
        11,
        json!({"op":"crash","target":{"machine":"app"}}),
    );
    assert!(matches!(
        p[0],
        Plan::Effect {
            effect: Effect::Crash,
            ..
        }
    ));
    o.facts.vm = Vm::Down;
    s.applied(fault(&p), 11, 11).unwrap();
    // A crashed Machine may retain its ProgramExit; Down skips physical
    // preflight, so a Down process fault still admits and settles Unavailable.
    let unavailable = call(
        &mut s,
        &mut o,
        8,
        11,
        json!({"op":"stop","target":{"machine":"app"}}),
    );
    assert_eq!(reply(&unavailable)["error"]["code"], "target_unavailable");
    assert_eq!(s.last, 2); // Crash plus admitted Down process fault
    let p = call(
        &mut s,
        &mut o,
        9,
        12,
        json!({"op":"reboot","target":{"machine":"app"}}),
    );
    assert!(matches!(
        p[0],
        Plan::Effect {
            effect: Effect::Reboot,
            ..
        }
    ));
    o.facts.vm = Vm::Up;
    o.facts.boot = 1;
    o.facts.physically_ended = false;
    s.applied(fault(&p), 12, 12).unwrap();
    assert!(s.pending.is_empty());
}

#[test]
fn trigger_admission_bypasses_control_only_physical_exit_preflight() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    call(
        &mut s,
        &mut o,
        1,
        1,
        json!({"op":"arm_trigger","source":{"machine":"app"},"marker":[1],"match_timeout_ns":100,"fault":{"action":"stop","target":{"machine":"app"}}}),
    );
    // Earlier committed bytes can trigger against a peer which has since exited.
    o.facts.physically_ended = true;
    let p = call(
        &mut s,
        &mut o,
        2,
        10,
        json!({"op":"stop","target":{"machine":"app"}}),
    );
    assert_eq!(reply(&p)["error"]["code"], "target_unavailable");
    assert_eq!(s.last, 0);
    s.committed_bytes("app", 8, &[1], 10).unwrap();
    let observations = s.trigger_effects(&mut o, 10).unwrap();
    assert_eq!(
        settlement(&observations).status,
        SettlementStatus::Unavailable
    );
    let record = &s.triggers.records()[0];
    assert_eq!(record.status, super::super::triggers::Status::Fired);
    assert_eq!(record.fault_id, Some(1));
    assert!(s.pending.is_empty());
    assert_eq!(o.next, 40);
}

#[test]
fn process_settlement_preserves_correlation_and_only_applied_proves_mutation() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    let p = call(
        &mut s,
        &mut o,
        1,
        10,
        json!({"op":"stop","target":{"machine":"app"}}),
    );
    let Plan::Effect {
        fault,
        command: Some(command),
        ..
    } = p[0]
    else {
        panic!()
    };
    let p = s
        .acknowledged(&o, "app", 0, command, CommandResult::Completed, 12, 20)
        .unwrap();
    let outcome = settlement(&p);
    assert_eq!(outcome.fault, fault);
    assert_eq!(outcome.command, Some(command));
    assert_eq!(
        outcome.scope,
        Scope::Machine {
            name: "app".into(),
            boot: 0
        }
    );
    assert_eq!(
        outcome.action,
        FaultAction::Effect(Effect::Process(ApplicationCommand::Stop))
    );
    assert_eq!(outcome.status, SettlementStatus::Applied);
    assert_eq!(outcome.outcome_at, Some(12));
    assert_eq!(outcome.committed_at, 20);
    assert_eq!(outcome.admitted_at, 10);
    assert_eq!(reply(&p)["effect_at_ns"], 12);
}
#[test]
fn callerless_trigger_unavailable_is_owned_observation_not_fake_reply() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    call(
        &mut s,
        &mut o,
        1,
        1,
        json!({"op":"arm_trigger","source":{"machine":"app"},"marker":[1],"match_timeout_ns":100,"fault":{"action":"stop","target":{"machine":"app"}}}),
    );
    o.facts.physically_ended = true;
    s.committed_bytes("app", 8, &[1], 10).unwrap();
    let p = s.trigger_effects(&mut o, 10).unwrap();
    assert_eq!(p.len(), 1);
    let outcome = settlement(&p);
    assert_eq!(outcome.fault, 1);
    assert_eq!(outcome.status, SettlementStatus::Unavailable);
    assert_eq!(outcome.outcome_at, Some(10));
    assert_eq!(outcome.committed_at, 10);
    assert_eq!(outcome.command, None);
    assert_eq!(
        s.triggers.records()[0].status,
        super::super::triggers::Status::Fired
    );
    assert!(s.pending.is_empty());
    assert!(s.trigger_effects(&mut o, 10).unwrap().is_empty());
}
#[test]
fn malformed_ack_failure_retains_known_fault_without_success_reply() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    let p = call(
        &mut s,
        &mut o,
        1,
        10,
        json!({"op":"stop","target":{"machine":"app"}}),
    );
    let Plan::Effect {
        fault,
        command: Some(command),
        ..
    } = p[0]
    else {
        panic!()
    };
    o.commands.get_mut(&(0, command.get())).unwrap().1 =
        CommandKind::Application(ApplicationCommand::Resume);
    let e = s
        .acknowledged(&o, "app", 0, command, CommandResult::Completed, 12, 20)
        .unwrap_err();
    assert_eq!(e.settlements.len(), 1);
    let outcome = &e.settlements[0];
    assert_eq!(outcome.fault, fault);
    assert_eq!(outcome.command, Some(command));
    assert_eq!(outcome.status, SettlementStatus::Failed);
    assert_eq!(outcome.outcome_at, Some(12));
    assert_eq!(outcome.committed_at, 20);
    assert!(s.poisoned);
    let mut s = Service::default();
    let e = s
        .acknowledged(
            &o,
            "app",
            0,
            NonZeroU32::new(999).unwrap(),
            CommandResult::Completed,
            12,
            20,
        )
        .unwrap_err();
    assert!(e.settlements.is_empty());
}
#[test]
fn known_rejected_heal_and_uncertain_host_failure_have_distinct_owned_outcomes() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    let p = call(
        &mut s,
        &mut o,
        1,
        10,
        json!({"op":"throttle","target":{"machine":"app"},"factor_permille":2000}),
    );
    let p = s.applied(fault(&p), 10, 10).unwrap();
    assert_eq!(settlement(&p).handle.as_deref(), Some("p1"));
    let p = call(&mut s, &mut o, 2, 11, json!({"op":"heal","handle":"p1"}));
    let p = s
        .rejected(
            fault(&p),
            11,
            11,
            error(ErrorCode::InvalidState, "known not applied"),
        )
        .unwrap();
    assert_eq!(settlement(&p).status, SettlementStatus::Rejected);
    assert_eq!(settlement(&p).action, FaultAction::Heal);
    assert_eq!(s.handles.active_handles().unwrap(), ["p1"]);
    assert!(!s.poisoned);
    assert_eq!(reply(&p)["ok"], false);
    let p = call(&mut s, &mut o, 3, 12, json!({"op":"heal","handle":"p1"}));
    let e = s.execution_failed(
        fault(&p),
        None,
        20,
        "Driver failed; partial effect unknown".into(),
    );
    assert_eq!(e.settlements.len(), 1);
    assert_eq!(e.settlements[0].status, SettlementStatus::Failed);
    assert_eq!(e.settlements[0].outcome_at, None);
    assert_eq!(e.settlements[0].committed_at, 20);
    assert_eq!(e.settlements[0].handle.as_deref(), Some("p1"));
    assert!(s.poisoned);
    assert!(s.handles.active_handles().is_err());
}

#[test]
fn guest_rejection_and_deadline_return_failed_correlated_observations() {
    for rejected in [true, false] {
        let (mut s, mut o) = (Service::default(), Owner::default());
        let p = call(
            &mut s,
            &mut o,
            1,
            10,
            json!({"op":"stop","target":{"machine":"app"}}),
        );
        let Plan::Effect {
            fault,
            command: Some(command),
            ..
        } = p[0]
        else {
            panic!()
        };
        let e = if rejected {
            s.acknowledged(&o, "app", 0, command, CommandResult::Rejected, 12, 20)
                .unwrap_err()
        } else {
            s.deadline(fault, 10 + PROCESS_EFFECT_TIMEOUT_NS)
                .unwrap_err()
        };
        assert_eq!(e.settlements.len(), 1);
        let outcome = &e.settlements[0];
        assert_eq!(outcome.fault, fault);
        assert_eq!(outcome.command, Some(command));
        assert_eq!(outcome.status, SettlementStatus::Failed);
        assert!(s.pending.is_empty());
        assert!(s.poisoned);
        let again = s.execution_failed(fault, None, 30, "not a new settlement".into());
        assert!(again.settlements.is_empty());
    }
}
#[test]
fn dispatch_allocation_failure_does_not_lose_admitted_fault_evidence() {
    struct Exhausted(Owner);
    impl Adapter for Exhausted {
        fn resolve(&self, t: &Target) -> Result<Facts, OperationError> {
            self.0.resolve(t)
        }
        fn command_id(
            &mut self,
            _: &str,
            _: u64,
            _: u32,
            _: CommandKind,
        ) -> Result<NonZeroU32, String> {
            Err("command IDs exhausted".into())
        }
        fn command_owner(&self, t: &str, b: u64, id: NonZeroU32) -> Result<CommandOwner, String> {
            self.0.command_owner(t, b, id)
        }
        fn inspect(
            &self,
            h: &[String],
            t: &[super::super::triggers::Record],
        ) -> Result<Value, OperationError> {
            self.0.inspect(h, t)
        }
    }
    let (mut s, mut o) = (Service::default(), Exhausted(Owner::default()));
    let e = s
        .admit(
            &mut o,
            "caller".into(),
            id(1),
            8,
            10,
            br#"{"version":1,"id":1,"op":"stop","target":{"machine":"app"}}"#,
        )
        .unwrap_err();
    assert_eq!(e.settlements.len(), 1);
    let outcome = &e.settlements[0];
    assert_eq!(outcome.fault, 1);
    assert_eq!(outcome.status, SettlementStatus::Failed);
    assert_eq!(outcome.command, None);
    assert_eq!(outcome.requested_at, 8);
    assert_eq!(outcome.admitted_at, 10);
    assert!(s.poisoned);
}

#[test]
fn swapped_owner_fault_does_not_retarget_failure_settlement() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    let a = call(
        &mut s,
        &mut o,
        1,
        10,
        json!({"op":"stop","target":{"machine":"app"}}),
    );
    let b = call(
        &mut s,
        &mut o,
        2,
        10,
        json!({"op":"restart","target":{"machine":"app"}}),
    );
    let Plan::Effect {
        fault: fa,
        command: Some(ca),
        ..
    } = a[0]
    else {
        panic!()
    };
    let Plan::Effect {
        fault: fb,
        command: Some(cb),
        ..
    } = b[0]
    else {
        panic!()
    };
    o.commands.get_mut(&(0, ca.get())).unwrap().0 = fb;
    let failure = s
        .acknowledged(&o, "app", 0, ca, CommandResult::Completed, 12, 20)
        .unwrap_err();
    assert_eq!(failure.settlements.len(), 1);
    let outcome = &failure.settlements[0];
    assert_eq!(outcome.fault, fa);
    assert_eq!(outcome.command, Some(ca));
    assert_eq!(outcome.status, SettlementStatus::Failed);
    assert!(!s.pending.contains_key(&fa));
    assert_eq!(s.pending[&fb].command, Some(cb));
    assert!(s.poisoned);
}

#[test]
fn fatal_later_trigger_preserves_earlier_owned_settlement_in_arm_order() {
    struct Targets;
    impl Adapter for Targets {
        fn resolve(&self, t: &Target) -> Result<Facts, OperationError> {
            let (Target::Machine(name) | Target::Service(name) | Target::Key(name)) = t;
            if !["source", "gone", "live"].contains(&name.as_str()) {
                return Err(OperationError::invalid("unknown target"));
            }
            let mut facts = Owner::default().facts;
            facts.name = name.clone();
            facts.physically_ended = name == "gone";
            Ok(facts)
        }
        fn command_id(
            &mut self,
            _: &str,
            _: u64,
            _: u32,
            _: CommandKind,
        ) -> Result<NonZeroU32, String> {
            Err("shared command IDs exhausted".into())
        }
        fn command_owner(&self, _: &str, _: u64, _: NonZeroU32) -> Result<CommandOwner, String> {
            Err("no accepted command".into())
        }
        fn inspect(
            &self,
            _: &[String],
            _: &[super::super::triggers::Record],
        ) -> Result<Value, OperationError> {
            Err(OperationError::unsupported())
        }
    }
    let (mut s, mut o) = (Service::default(), Targets);
    for (n, target) in [(1, "gone"), (2, "live")] {
        let payload=serde_json::to_vec(&json!({"version":1,"id":n,"op":"arm_trigger","source":{"machine":"source"},"marker":[n],"match_timeout_ns":100,"fault":{"action":"stop","target":{"machine":target}}})).unwrap();
        s.admit(&mut o, "caller".into(), id(n), 1, 1, &payload)
            .unwrap();
    }
    s.committed_bytes("source", 3, &[1, 2], 10).unwrap();
    let failure = s.trigger_effects(&mut o, 10).unwrap_err();
    assert_eq!(
        failure
            .settlements
            .iter()
            .map(|s| (s.fault, s.status))
            .collect::<Vec<_>>(),
        [
            (1, SettlementStatus::Unavailable),
            (2, SettlementStatus::Failed)
        ]
    );
    assert_eq!(failure.settlements[0].scope.machine().unwrap().0, "gone");
    assert_eq!(failure.settlements[1].scope.machine().unwrap().0, "live");
    for record in s.triggers.records() {
        assert_eq!(record.status, super::super::triggers::Status::Fired);
        assert_eq!(record.fault_id, Some(record.id));
    }
    assert!(s.poisoned);
    assert!(s.pending.is_empty());
}

#[test]
fn invalid_receipt_time_is_diagnostic_not_fault_occurrence() {
    for (observed, commit, dispatch) in [(9, 20, 10), (12, 30, 20), (30, 20, 10)] {
        let (mut s, mut o) = (Service::default(), Owner::default());
        o.facts.ready = dispatch == 10;
        let initial = call(
            &mut s,
            &mut o,
            1,
            10,
            json!({"op":"stop","target":{"machine":"app"}}),
        );
        let plans = if dispatch == 10 {
            initial
        } else {
            o.facts.ready = true;
            let p = s.ready(&mut o, "app", 0, 11, dispatch).unwrap();
            o.accept(&p);
            p
        };
        let Plan::Effect {
            command: Some(command),
            ..
        } = plans[0]
        else {
            panic!()
        };
        let failure = s
            .acknowledged(
                &o,
                "app",
                0,
                command,
                CommandResult::Completed,
                observed,
                commit,
            )
            .unwrap_err();
        assert_eq!(failure.settlements.len(), 1);
        let outcome = &failure.settlements[0];
        assert_eq!(outcome.status, SettlementStatus::Failed);
        assert_eq!(outcome.outcome_at, None);
        assert_eq!(outcome.committed_at, commit);
        assert!(outcome
            .detail
            .contains(&format!("receipt_time_ns={observed}")));
        assert!(s.poisoned);
    }
}

#[test]
fn naturally_exited_application_on_live_peer_can_restart_same_boot() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    // The observation owner supplies the approved live-supervisor projection.
    o.facts.process = Process::Stopped;
    let plans = call(
        &mut s,
        &mut o,
        1,
        10,
        json!({"op":"restart","target":{"machine":"app"}}),
    );
    let command = plans
        .iter()
        .find_map(|plan| match plan {
            Plan::Effect {
                effect: Effect::Process(ApplicationCommand::Start),
                scope: Scope::Machine { boot: 0, .. },
                command: Some(id),
                ..
            } => Some(*id),
            _ => None,
        })
        .expect("same-boot Start plan");
    assert!(!o.facts.physically_ended);
    let done = s
        .acknowledged(&o, "app", 0, command, CommandResult::Completed, 12, 15)
        .unwrap();
    assert_eq!(reply(&done)["ok"], true);
    assert_eq!(reply(&done)["effect_at_ns"], 12);
}

#[test]
fn application_end_settles_old_commands_once_but_preserves_new_start() {
    for result in [CommandResult::Completed, CommandResult::Rejected] {
        let (mut s, mut o) = (Service::default(), Owner::default());
        let stop = call(
            &mut s,
            &mut o,
            1,
            10,
            json!({"op":"stop","target":{"machine":"app"}}),
        );
        let old = match stop[0] {
            Plan::Effect {
                command: Some(id), ..
            } => id,
            _ => panic!(),
        };
        o.facts.process = Process::Stopped;
        let start = call(
            &mut s,
            &mut o,
            2,
            10,
            json!({"op":"restart","target":{"machine":"app"}}),
        );
        let fresh = match start[0] {
            Plan::Effect {
                command: Some(id), ..
            } => id,
            _ => panic!(),
        };
        let ended = s.application_ended("app", 0, 11, 12).unwrap();
        assert_eq!(settlement(&ended).status, SettlementStatus::Unavailable);
        assert_eq!(s.pending.len(), 1);
        o.retired.insert((0, old.get()));
        assert!(s
            .acknowledged(&o, "app", 0, old, result, 13, 13)
            .unwrap()
            .is_empty());
        let done = s
            .acknowledged(&o, "app", 0, fresh, CommandResult::Completed, 14, 14)
            .unwrap();
        assert_eq!(settlement(&done).status, SettlementStatus::Applied);
        assert!(s.pending.is_empty());
    }
}

#[test]
fn ordinary_control_preflight_does_not_depend_on_a_reply_destination() {
    let (mut service, mut owner) = (Service::default(), Owner::default());
    owner.facts.physically_ended = true;
    let command = || Command::Effect {
        target: Target::Machine("app".into()),
        effect: Effect::Process(ApplicationCommand::Stop),
    };
    // Reply correlation cannot choose policy. A host input has no RPC
    // destination, yet ordinary control rejects it before fault allocation;
    // bare trigger admission allocates that same fault and reports Unavailable.
    let rejected = service
        .admit_input(&mut owner, "key".into(), command(), 10, 7)
        .unwrap();
    assert_eq!(rejected.unwrap_err().code, ErrorCode::TargetUnavailable);
    assert_eq!(service.last, 0);
    assert!(service.pending.is_empty());
    assert!(service.take_notices().is_empty());
    let (fault, plans) = match service.prepare(
        &mut owner,
        AdmissionMode::Trigger,
        Requester::Trigger(1),
        None,
        command(),
        10,
        PROCESS_EFFECT_TIMEOUT_NS,
    ) {
        Ok(result) => result,
        Err(_) => panic!("bare trigger must admit an unavailable fault"),
    };
    assert_eq!(fault, 1);
    assert_eq!(settlement(&plans).status, SettlementStatus::Unavailable);
    assert_eq!(settlement(&plans).outcome_at, Some(10));
    assert!(!plans.iter().any(|plan| matches!(plan, Plan::Reply { .. })));
    assert!(service.pending.is_empty());
    assert_eq!(owner.next, 40, "unavailable fault offers no app command");
}

#[test]
fn host_input_admission_uses_its_own_deadline_and_never_replies() {
    let (mut service, mut owner) = (Service::default(), Owner::default());
    let command = Command::Effect {
        target: Target::Machine("app".into()),
        effect: Effect::Process(ApplicationCommand::Pause),
    };
    let (fault, plans) = service
        .admit_input(&mut owner, "freeze".into(), command, 10, 7)
        .unwrap()
        .unwrap();
    assert!(plans
        .iter()
        .any(|plan| matches!(plan, Plan::Deadline { fault: f, at: 17 } if *f == fault)));
    assert!(!plans.iter().any(|plan| matches!(plan, Plan::Reply { .. })));
    let notices = service.take_notices();
    assert!(
        matches!(&notices[..], [Notice::Admitted(Admitted { requester: Requester::Input(key), requested_at: 10, .. })] if key == "freeze")
    );
    // Overflowing deadlines reject without allocating a fault.
    let command = Command::Effect {
        target: Target::Machine("app".into()),
        effect: Effect::Pause,
    };
    let rejected = service
        .admit_input(&mut owner, "late".into(), command, 10, u64::MAX)
        .unwrap();
    assert_eq!(rejected.unwrap_err().code, ErrorCode::InvalidRequest);
    assert_eq!(service.last, fault);
}
fn effect(plans: &[Plan]) -> (u32, Effect, Option<NonZeroU32>) {
    plans
        .iter()
        .find_map(|p| match p {
            Plan::Effect {
                fault,
                effect,
                command,
                ..
            } => Some((*fault, effect.clone(), *command)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no effect: {plans:?}"))
}
fn admitted_actions(s: &mut Service) -> Vec<FaultAction> {
    s.take_notices()
        .into_iter()
        .filter_map(|n| match n {
            Notice::Admitted(a) => Some(a.action),
            _ => None,
        })
        .collect()
}
fn rate(ppb: i64) -> ClockRate {
    ClockRate::new(ppb).unwrap()
}
#[test]
fn drift_applies_at_dispatch_never_stacks_and_heals_to_nominal() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    o.facts.ready = false; // Drift needs no guest and no readiness.
    let p = call(
        &mut s,
        &mut o,
        1,
        10,
        json!({"op":"clock_drift","target":{"machine":"app"},"rate_ppb":100_000_000}),
    );
    let (drift, change, command) = effect(&p);
    assert_eq!((change, command), (Effect::Drift(rate(100_000_000)), None));
    assert!(!p.iter().any(|p| matches!(p, Plan::Deadline { .. })));
    let r = reply(&s.applied(drift, 10, 10).unwrap());
    assert_eq!(r["result"], json!({"handle":"p1","rate_ppb":100_000_000}));
    assert_eq!(r["effect_at_ns"], 10);
    let r = reply(&call(
        &mut s,
        &mut o,
        2,
        11,
        json!({"op":"clock_drift","target":{"machine":"app"},"rate_ppb":-5}),
    ));
    assert_eq!(r["error"]["code"], "invalid_state");
    // Drift is a Driver-owned dependency: it survives the machine's end.
    s.ended("app", 0, 12, 12).unwrap();
    o.facts.vm = Vm::Down;
    let p = call(&mut s, &mut o, 3, 13, json!({"op":"heal","handle":"p1"}));
    let (heal, change, _) = effect(&p);
    assert_eq!(change, Effect::Drift(ClockRate::NOMINAL));
    s.applied(heal, 13, 13).unwrap();
    assert!(s.handles.active_handles().unwrap().is_empty());
    let p = call(
        &mut s,
        &mut o,
        4,
        14,
        json!({"op":"clock_drift","target":{"machine":"app"},"rate_ppb":-5}),
    );
    assert_eq!(effect(&p).1, Effect::Drift(rate(-5)));
    assert_eq!(
        admitted_actions(&mut s),
        [
            FaultAction::Effect(Effect::Drift(rate(100_000_000))),
            FaultAction::Heal,
            FaultAction::Effect(Effect::Drift(rate(-5))),
        ]
    );
}
#[test]
fn step_waits_ready_completes_on_ack_and_heals_inversely() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    o.facts.ready = false;
    let p = call(
        &mut s,
        &mut o,
        1,
        10,
        json!({"op":"clock_skew","target":{"machine":"app"},"offset_ns":-7}),
    );
    assert!(matches!(
        p[..],
        [Plan::Deadline {
            at: 5_000_000_010,
            ..
        }]
    ));
    o.facts.ready = true;
    let p = s.ready(&mut o, "app", 0, 12, 12).unwrap();
    o.accept(&p);
    let (step, change, command) = effect(&p);
    assert_eq!(change, Effect::StepRealtime(-7));
    let command = command.unwrap();
    assert!(s.applied(step, 13, 13).is_err());
    let r = reply(
        &s.acknowledged(&o, "app", 0, command, CommandResult::Completed, 14, 15)
            .unwrap(),
    );
    assert_eq!(r["result"], json!({"handle":"p1","offset_ns":-7}));
    assert_eq!(r["effect_at_ns"], 14);
    let p = call(&mut s, &mut o, 2, 20, json!({"op":"heal","handle":"p1"}));
    let (_, change, command) = effect(&p);
    assert_eq!(change, Effect::StepRealtime(7));
    let p = s
        .acknowledged(
            &o,
            "app",
            0,
            command.unwrap(),
            CommandResult::Completed,
            21,
            21,
        )
        .unwrap();
    assert_eq!(settlement(&p).status, SettlementStatus::Applied);
    assert!(s.handles.active_handles().unwrap().is_empty());
}
#[test]
fn step_admission_uses_process_state_rules_before_drawing() {
    // Paused and physically exited (not down) targets are refused before any
    // seeded draw or handle reservation, exactly as process commands are.
    for (vm, ended, code) in [
        (Vm::Paused, false, "invalid_state"),
        (Vm::Up, true, "target_unavailable"),
    ] {
        let (mut s, mut o) = (Service::default(), Owner::default());
        o.facts.vm = vm;
        o.facts.physically_ended = ended;
        let r = reply(&call(
            &mut s,
            &mut o,
            1,
            10,
            json!({"op":"clock_skew","target":{"machine":"app"},"max_offset_ns":5}),
        ));
        assert_eq!(r["error"]["code"], code);
        assert!(s.pending.is_empty() && admitted_actions(&mut s).is_empty());
        // No scripted draw (draw() would have panicked) and no handle number.
        assert_eq!(s.handles.reserve().unwrap().to_string(), "p1");
    }
    // A down target is admitted (drawing its value) and settles Unavailable
    // with no guest command, like a process command; zero draws included.
    for draw in [105, 5] {
        let (mut s, mut o) = (Service::default(), Owner::default());
        o.facts.vm = Vm::Down;
        o.facts.physically_ended = true;
        o.draws.push_back(draw);
        let p = call(
            &mut s,
            &mut o,
            1,
            10,
            json!({"op":"clock_skew","target":{"machine":"app"},"max_offset_ns":5}),
        );
        let offset = draw as i64 % 11 - 5;
        assert_eq!(
            admitted_actions(&mut s),
            [FaultAction::Effect(Effect::StepRealtime(offset))]
        );
        assert!(!p.iter().any(|p| matches!(p, Plan::Effect { .. })));
        assert_eq!(settlement(&p).status, SettlementStatus::Unavailable);
        assert_eq!(reply(&p)["error"]["code"], "target_unavailable");
        assert!(s.handles.active_handles().unwrap().is_empty() && o.draws.is_empty());
    }
}
#[test]
fn zero_steps_skip_only_the_guest_command_after_ordinary_checks() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    o.facts.ready = false;
    o.draws.push_back(5); // 5 % 11 - 5 == 0
    let p = call(
        &mut s,
        &mut o,
        1,
        10,
        json!({"op":"clock_skew","target":{"machine":"app"},"max_offset_ns":5}),
    );
    // Not ready: no command is needed, so it settles at admission.
    assert!(!p
        .iter()
        .any(|p| matches!(p, Plan::Effect { .. } | Plan::Deadline { .. })));
    assert_eq!(settlement(&p).status, SettlementStatus::Applied);
    assert_eq!(reply(&p)["result"], json!({"handle":"p1","offset_ns":0}));
    // Healing it follows the same state rules: refused while paused.
    o.facts.vm = Vm::Paused;
    let r = reply(&call(
        &mut s,
        &mut o,
        2,
        11,
        json!({"op":"heal","handle":"p1"}),
    ));
    assert_eq!(r["error"]["code"], "invalid_state");
    assert_eq!(s.handles.active_handles().unwrap(), ["p1"]);
    o.facts.vm = Vm::Up;
    let p = call(&mut s, &mut o, 3, 12, json!({"op":"heal","handle":"p1"}));
    assert_eq!(settlement(&p).status, SettlementStatus::Applied);
    assert_eq!(settlement(&p).detail, "zero realtime step");
    assert!(s.handles.active_handles().unwrap().is_empty());
}
#[test]
fn refused_duplicate_drift_consumes_a_handle_number_like_throttle() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    for (op, extra) in [
        ("throttle", ("factor_permille", 2000)),
        ("clock_drift", ("rate_ppb", 5)),
    ] {
        let mut request = json!({"op":op,"target":{"machine":"app"}});
        request[extra.0] = json!(extra.1);
        let first = call(&mut s, &mut o, 1, 10, request.clone());
        let fault = effect(&first).0;
        s.applied(fault, 10, 10).unwrap();
        let r = reply(&call(&mut s, &mut o, 2, 11, request));
        assert_eq!(r["error"]["code"], "invalid_state");
    }
    // p1 throttle, p2 refused throttle, p3 drift, p4 refused drift.
    assert_eq!(s.handles.active_handles().unwrap(), ["p1", "p3"]);
    assert_eq!(s.handles.reserve().unwrap().to_string(), "p5");
}
#[test]
fn settled_steps_are_lost_with_their_boot_and_healing_them_applies_without_guest() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    let p = call(
        &mut s,
        &mut o,
        1,
        10,
        json!({"op":"clock_skew","target":{"machine":"app"},"offset_ns":3}),
    );
    let command = effect(&p).2.unwrap();
    s.acknowledged(&o, "app", 0, command, CommandResult::Completed, 11, 11)
        .unwrap();
    // A second step still pending at the crash settles Unavailable.
    let p = call(
        &mut s,
        &mut o,
        2,
        12,
        json!({"op":"clock_skew","target":{"machine":"app"},"offset_ns":4}),
    );
    let pending = effect(&p).0;
    let p = s.ended("app", 0, 13, 13).unwrap();
    let ended = settlement(&p);
    assert_eq!(
        (ended.fault, ended.status),
        (pending, SettlementStatus::Unavailable)
    );
    o.facts.vm = Vm::Down;
    o.facts.boot = 1;
    let p = call(&mut s, &mut o, 3, 14, json!({"op":"heal","handle":"p1"}));
    assert!(!p.iter().any(|p| matches!(p, Plan::Effect { .. })));
    let healed = settlement(&p);
    assert_eq!(healed.status, SettlementStatus::Applied);
    assert_eq!(
        healed.detail,
        "realtime step ended with its machine incarnation"
    );
    assert_eq!(reply(&p)["ok"], true);
    assert!(s.handles.active_handles().unwrap().is_empty());
    // Natural application exit does not end a pending kernel step.
    o.facts.vm = Vm::Up;
    let p = call(
        &mut s,
        &mut o,
        4,
        15,
        json!({"op":"clock_skew","target":{"machine":"app"},"offset_ns":5}),
    );
    assert!(s.application_ended("app", 1, 16, 16).unwrap().is_empty());
    let command = effect(&p).2.unwrap();
    let p = s
        .acknowledged(&o, "app", 1, command, CommandResult::Completed, 17, 17)
        .unwrap();
    assert_eq!(settlement(&p).status, SettlementStatus::Applied);
}
#[test]
fn seeded_draws_are_uniform_rejection_samples_resolved_at_admission() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    // span 201: u64::MAX is in the rejected tail; 205 maps to 205 % 201 - 100.
    assert_ne!(u64::MAX % 201, 200);
    o.draws.extend([u64::MAX, 205, 100]);
    let p = call(
        &mut s,
        &mut o,
        1,
        10,
        json!({"op":"clock_skew","target":{"machine":"app"},"max_offset_ns":100}),
    );
    assert_eq!(effect(&p).1, Effect::StepRealtime(-96));
    assert_eq!(
        admitted_actions(&mut s),
        [FaultAction::Effect(Effect::StepRealtime(-96))]
    );
    // A zero draw is a no-op step that settles at once with its handle.
    let p = call(
        &mut s,
        &mut o,
        2,
        11,
        json!({"op":"clock_skew","target":{"machine":"app"},"max_offset_ns":100}),
    );
    assert!(!p.iter().any(|p| matches!(p, Plan::Effect { .. })));
    assert_eq!(settlement(&p).detail, "zero realtime step");
    assert_eq!(reply(&p)["result"], json!({"handle":"p2","offset_ns":0}));
    o.draws.push_back(7);
    let p = call(
        &mut s,
        &mut o,
        3,
        12,
        json!({"op":"clock_drift","target":{"machine":"app"},"max_rate_ppb":5}),
    );
    assert_eq!(effect(&p).1, Effect::Drift(rate(2)));
    assert!(o.draws.is_empty());
}
#[test]
fn guest_refusal_of_a_step_is_rejected_without_poisoning() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    let p = call(
        &mut s,
        &mut o,
        1,
        10,
        json!({"op":"clock_skew","target":{"machine":"app"},"offset_ns":-9}),
    );
    let command = effect(&p).2.unwrap();
    let p = s
        .acknowledged(&o, "app", 0, command, CommandResult::Rejected, 11, 11)
        .unwrap();
    assert_eq!(settlement(&p).status, SettlementStatus::Rejected);
    assert_eq!(reply(&p)["error"]["code"], "invalid_state");
    assert!(s.handles.active_handles().unwrap().is_empty());
    assert_eq!(
        reply(&call(&mut s, &mut o, 2, 12, json!({"op":"now"})))["ok"],
        true
    );
}

fn members(names: &[&str]) -> Value {
    Value::Array(names.iter().map(|n| json!({"machine":n})).collect())
}
fn links(left: &[&str], right: &[&str]) -> Scope {
    let names = |side: &[&str]| side.iter().map(|n| n.to_string()).collect();
    Scope::Links {
        left: names(left),
        right: names(right),
    }
}
#[test]
fn guest_partitions_validate_members_before_allocation_and_heal_their_links() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    for (n, (left, right, code)) in [
        (&[][..], &["a"][..], ErrorCode::InvalidRequest),
        (&["a"], &["a"], ErrorCode::InvalidRequest),
        (&["a", "a"], &["b"], ErrorCode::InvalidRequest),
        (&["a"], &["bare"], ErrorCode::InvalidRequest),
        (&["a"], &["later"], ErrorCode::TargetUnavailable),
        (&["a"], &["nobody"], ErrorCode::InvalidRequest),
    ]
    .into_iter()
    .enumerate()
    {
        let p = call(
            &mut s,
            &mut o,
            n as u32 + 1,
            1,
            json!({"op":"partition","left":members(left),"right":members(right)}),
        );
        let r = reply(&p);
        assert_eq!(r["ok"], false, "{left:?} {right:?}");
        assert_eq!(
            r["error"]["code"],
            serde_json::to_value(code).unwrap(),
            "{left:?} {right:?}"
        );
    }
    assert!(s.take_notices().is_empty(), "rejections admit nothing");
    // Sides are sorted; the first successful partition still gets p1.
    let p = call(
        &mut s,
        &mut o,
        10,
        2,
        json!({"op":"partition","left":members(&["c", "b"]),"right":members(&["a"])}),
    );
    let Plan::Effect {
        fault,
        scope,
        effect: Effect::Partition,
        handle,
        command: None,
        ..
    } = &p[0]
    else {
        panic!("partition dispatches at admission: {p:?}");
    };
    assert_eq!(
        (scope, handle.as_deref()),
        (&links(&["b", "c"], &["a"]), Some("p1"))
    );
    let p = s.applied(*fault, 2, 2).unwrap();
    assert_eq!(settlement(&p).status, SettlementStatus::Applied);
    assert_eq!(settlement(&p).scope, links(&["b", "c"], &["a"]));
    assert_eq!(
        settlement(&p).action,
        FaultAction::Effect(Effect::Partition)
    );
    assert_eq!(reply(&p)["result"], json!({"handle":"p1"}));
    // A machine ending changes no partition, and heal restores its links.
    assert!(s.ended("app", 0, 3, 3).unwrap().is_empty());
    let p = call(&mut s, &mut o, 11, 4, json!({"op":"heal","handle":"p1"}));
    let Plan::Effect {
        fault,
        scope,
        effect: Effect::Unpartition,
        handle,
        ..
    } = &p[0]
    else {
        panic!("heal dispatches at admission: {p:?}");
    };
    assert_eq!(
        (scope, handle.as_deref()),
        (&links(&["b", "c"], &["a"]), Some("p1"))
    );
    let p = s.applied(*fault, 4, 4).unwrap();
    assert_eq!(settlement(&p).action, FaultAction::Heal);
    assert_eq!(settlement(&p).scope, links(&["b", "c"], &["a"]));
}
#[test]
fn triggered_partitions_use_the_same_membership_and_admission() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    let p = call(
        &mut s,
        &mut o,
        1,
        1,
        json!({"op":"arm_trigger","source":{"machine":"app"},"marker":[1],"match_timeout_ns":100,"fault":{"action":"partition","left":members(&["a"]),"right":members(&["bare"])}}),
    );
    assert_eq!(reply(&p)["error"]["code"], "invalid_request");
    let p = call(
        &mut s,
        &mut o,
        2,
        1,
        json!({"op":"arm_trigger","source":{"machine":"app"},"marker":[1],"match_timeout_ns":100,"fault":{"action":"partition","left":members(&["b"]),"right":members(&["a"])}}),
    );
    assert_eq!(reply(&p)["result"], json!({"trigger_id":1}));
    // The watched source keeps its identity; a partition names no target.
    assert!(s.take_notices().iter().any(|n| matches!(n,
        Notice::TriggerArmed { source, target: None, .. } if source == "app")));
    s.committed_bytes("app", 3, &[1], 10).unwrap();
    let p = s.trigger_effects(&mut o, 10).unwrap();
    let Plan::Effect {
        scope,
        effect: Effect::Partition,
        handle,
        ..
    } = &p[0]
    else {
        panic!("triggered partition dispatches: {p:?}");
    };
    assert_eq!(
        (scope, handle.as_deref()),
        (&links(&["b"], &["a"]), Some("p1"))
    );
    assert!(s.take_notices().iter().any(|n| matches!(n,
        Notice::Admitted(a) if a.requester == Requester::Trigger(1))));
}
