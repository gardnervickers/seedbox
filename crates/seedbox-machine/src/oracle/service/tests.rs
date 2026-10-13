use super::*;
struct Owner {
    facts: Facts,
    next: u32,
    commands: BTreeMap<(u64, u32), (u32, ApplicationCommand, bool)>,
    retired: std::collections::BTreeSet<(u64, u32)>,
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
                execution: ExecutionConfig::default(),
            },
            next: 40,
            commands: BTreeMap::new(),
            retired: Default::default(),
        }
    }
}
impl Owner {
    fn accept(&mut self, plans: &[Plan]) {
        for p in plans {
            if let Plan::Effect {
                boot,
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
    fn command_id(
        &mut self,
        _: &str,
        boot: u64,
        fault: u32,
        action: ApplicationCommand,
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
    o.facts.execution = ExecutionConfig::new(2000).unwrap();
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
    o.facts.execution = ExecutionConfig::default();
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
fn unsupported_projection_clock_and_invalid_target_have_no_effect() {
    let (mut s, mut o) = (Service::default(), Owner::default());
    for (n, op) in [
        (1, json!({"op":"inspect"})),
        (
            2,
            json!({"op":"clock_skew","target":{"machine":"app"},"offset_ns":1}),
        ),
    ] {
        assert_eq!(
            reply(&call(&mut s, &mut o, n, 0, op))["error"]["code"],
            "unsupported"
        );
    }
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
    let duplicate = call(
        &mut s,
        &mut o,
        2,
        10,
        json!({"op":"stop","target":{"service":"service"}}),
    );
    assert_ne!(fault(&duplicate), fault(&p));
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
    o.commands.get_mut(&(0, c.get())).unwrap().1 = ApplicationCommand::Resume;
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
    // New Machine may retain its ProgramExit after logical Crash. Excluding
    // Down from physical preflight preserves legacy fresh-unrun Down behavior.
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
    assert_eq!(outcome.target, "app");
    assert_eq!(outcome.boot, 0);
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
    o.commands.get_mut(&(0, command.get())).unwrap().1 = ApplicationCommand::Resume;
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
            _: ApplicationCommand,
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
        json!({"op":"stop","target":{"machine":"app"}}),
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
            _: ApplicationCommand,
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
    assert_eq!(failure.settlements[0].target, "gone");
    assert_eq!(failure.settlements[1].target, "live");
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
                boot: 0,
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
