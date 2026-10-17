//! Folds a simulation's engine observations into the world clients discover.
//! The spec seeds the initial machines and link policies; everything after
//! that, including machines the spec never named, comes from observations.
use seedbox_api::{
    ApplicationCondition, ApplicationState, EventKind, FaultAction, FaultState, FaultStatus, Frame,
    FrameFate, LinkCounters, LinkPolicy, LinkState, MachineCondition, MachineState,
    MachineTransition, OutOfMemoryKill, SimulationId, WorldState, MAX_OUT_OF_MEMORY_KILLS,
};
use std::collections::{BTreeMap, BTreeSet};

type Pair = (String, String);

pub(crate) struct Tracker {
    simulation: SimulationId,
    revision: u64,
    time: u64,
    oracle: String,
    base: LinkPolicy,
    configured: BTreeMap<Pair, LinkPolicy>,
    machines: BTreeMap<String, MachineState>,
    faults: BTreeMap<u32, FaultState>,
    override_rejections: BTreeMap<String, String>,
    triggers: BTreeMap<String, seedbox_api::TriggerState>,
    choice_inputs: BTreeMap<String, seedbox_api::ChoiceInputState>,
    counters: BTreeMap<Pair, LinkCounters>,
    /// Active partition and shape handles, with the fault that set them.
    partitions: BTreeMap<String, (u32, BTreeSet<Pair>)>,
    shapes: BTreeMap<String, (u32, BTreeSet<Pair>, LinkPolicy)>,
    healable: BTreeMap<String, u32>,
    process_effects: BTreeMap<String, u32>,
    machine_pauses: BTreeMap<String, u32>,
    before_pause: BTreeMap<String, MachineCondition>,
}

fn pairs(from: &[String], to: &[String]) -> BTreeSet<Pair> {
    from.iter()
        .flat_map(|a| to.iter().map(move |b| (a.clone(), b.clone())))
        .filter(|(a, b)| a != b)
        .collect()
}

impl Tracker {
    pub fn new(simulation: SimulationId, configuration: &seedbox_spec::Simulation) -> Self {
        let network = &configuration.network;
        let mut tracker = Self {
            simulation,
            revision: 1,
            time: 0,
            oracle: configuration.check.service.clone(),
            base: LinkPolicy {
                latency_ns: network.latency_ns,
                jitter_ns: network.jitter_ns,
                loss_ppm: 0,
                bandwidth_bps: network.bandwidth_bps,
            },
            configured: network
                .links
                .iter()
                .map(|link| {
                    (
                        (link.from.clone(), link.to.clone()),
                        LinkPolicy {
                            latency_ns: link.latency_ns,
                            jitter_ns: link.jitter_ns,
                            loss_ppm: link.loss_ppm,
                            bandwidth_bps: link.bandwidth_bps,
                        },
                    )
                })
                .collect(),
            machines: BTreeMap::new(),
            faults: BTreeMap::new(),
            override_rejections: BTreeMap::new(),
            triggers: BTreeMap::new(),
            choice_inputs: BTreeMap::new(),
            counters: BTreeMap::new(),
            partitions: BTreeMap::new(),
            shapes: BTreeMap::new(),
            healable: BTreeMap::new(),
            process_effects: BTreeMap::new(),
            machine_pauses: BTreeMap::new(),
            before_pause: BTreeMap::new(),
        };
        for (name, service) in &configuration.services {
            tracker.machine(name).cpu_speed_hz = Some(service.cpu.speed_hz);
        }
        tracker.revision = 1;
        tracker
    }

    fn machine(&mut self, name: &str) -> &mut MachineState {
        if !self.machines.contains_key(name) {
            self.revision += 1;
            self.machines.insert(
                name.to_owned(),
                MachineState {
                    name: name.to_owned(),
                    incarnation: 0,
                    condition: MachineCondition::Starting,
                    exit_code: None,
                    cpu_speed_hz: None,
                    throttle_speed_hz: None,
                    oracle: name == self.oracle,
                    last_transition: None,
                    last_transition_at_ns: None,
                    condition_since_ns: None,
                    application: ApplicationState::default(),
                    out_of_memory_kills: Vec::new(),
                },
            );
        }
        self.machines.get_mut(name).expect("inserted above")
    }

    /// Fold one engine event. Events that describe no world entity are ignored.
    pub fn event(&mut self, time: u64, event: &EventKind) {
        self.time = self.time.max(time);
        match event {
            EventKind::OverrideRejected {
                input_key, reason, ..
            } => {
                self.override_rejections
                    .insert(input_key.clone(), reason.clone());
            }
            EventKind::ChoiceChanged { input_key, choice } => {
                self.choice_inputs.insert(input_key.clone(), choice.clone());
                self.revision += 1;
            }
            EventKind::TriggerChanged { input_key, trigger } => {
                self.triggers.insert(input_key.clone(), trigger.clone());
                self.revision += 1;
            }
            EventKind::MachineReady {
                machine,
                incarnation,
            } => {
                let m = self.machine(machine);
                m.incarnation = *incarnation;
                m.condition = MachineCondition::Ready;
                m.condition_since_ns = Some(time);
                m.application = ApplicationState {
                    condition: ApplicationCondition::Running,
                    changed_at_ns: Some(time),
                    exit_code: None,
                };
            }
            EventKind::MachineExited {
                machine,
                incarnation,
                code,
            } => {
                let m = self.machine(machine);
                m.incarnation = *incarnation;
                m.condition = MachineCondition::Exited;
                m.exit_code = Some(*code);
                m.condition_since_ns = Some(time);
                m.application = ApplicationState {
                    condition: ApplicationCondition::Exited,
                    changed_at_ns: Some(time),
                    exit_code: Some(*code),
                };
                self.process_effects.remove(machine);
                self.machine_pauses.remove(machine);
                self.before_pause.remove(machine);
            }
            EventKind::OutOfMemoryKill {
                machine,
                incarnation,
                process,
            } => {
                let kills = &mut self.machine(machine).out_of_memory_kills;
                if kills.len() == MAX_OUT_OF_MEMORY_KILLS {
                    kills.remove(0);
                }
                kills.push(OutOfMemoryKill {
                    incarnation: *incarnation,
                    simulated_time_ns: time,
                    process: process.clone(),
                });
            }
            EventKind::MachineLifecycle {
                machine,
                incarnation,
                transition,
            } => {
                let resumed_condition = match transition {
                    MachineTransition::Paused => {
                        let condition = self.machine(machine).condition;
                        self.before_pause
                            .entry(machine.clone())
                            .or_insert(condition);
                        None
                    }
                    MachineTransition::Resumed => self.before_pause.remove(machine),
                    MachineTransition::Crashed | MachineTransition::Rebooted => {
                        self.before_pause.remove(machine);
                        None
                    }
                    MachineTransition::Throttled { .. } => None,
                };
                let m = self.machine(machine);
                m.incarnation = *incarnation;
                m.last_transition = Some(*transition);
                m.last_transition_at_ns = Some(time);
                if !matches!(transition, MachineTransition::Throttled { .. }) {
                    m.condition_since_ns = Some(time);
                }
                match transition {
                    MachineTransition::Paused => m.condition = MachineCondition::Paused,
                    MachineTransition::Resumed => {
                        m.condition = resumed_condition.unwrap_or(MachineCondition::Starting);
                    }
                    MachineTransition::Crashed => {
                        m.condition = MachineCondition::Crashed;
                        m.application = ApplicationState {
                            condition: ApplicationCondition::Unavailable,
                            changed_at_ns: Some(time),
                            exit_code: None,
                        };
                    }
                    MachineTransition::Rebooted => {
                        m.condition = MachineCondition::Starting;
                        m.exit_code = None;
                        m.application = ApplicationState {
                            condition: ApplicationCondition::Starting,
                            changed_at_ns: Some(time),
                            exit_code: None,
                        };
                    }
                    MachineTransition::Throttled { speed_hz } => {
                        m.throttle_speed_hz = *speed_hz;
                    }
                }
                if matches!(
                    transition,
                    MachineTransition::Crashed | MachineTransition::Rebooted
                ) {
                    self.process_effects.remove(machine);
                    self.machine_pauses.remove(machine);
                } else if matches!(transition, MachineTransition::Resumed) {
                    self.machine_pauses.remove(machine);
                }
            }
            EventKind::FaultChanged {
                counts,
                id,
                action,
                machine,
                status,
                applied_time_ns,
                plan,
                input_key,
            } => {
                if let Some(machine) = machine {
                    self.machine(machine);
                }
                let fault = self.faults.entry(*id).or_insert_with(|| FaultState {
                    counts: None,
                    id: *id,
                    action: action.clone(),
                    machine: machine.clone(),
                    status: *status,
                    applied_time_ns: None,
                    plan: None,
                    active: false,
                    input_key: input_key.clone(),
                });
                if input_key.is_some() {
                    fault.input_key = input_key.clone();
                }
                fault.status = *status;
                fault.applied_time_ns = *applied_time_ns;
                if counts.is_some() {
                    fault.counts = counts.as_deref().cloned();
                }
                if plan.is_some() {
                    fault.plan = plan.as_deref().cloned();
                }
                if *status == FaultStatus::Applied {
                    if let Some(plan) = fault
                        .plan
                        .clone()
                        .and_then(|plan| serde_json::from_value::<FaultAction>(plan).ok())
                    {
                        self.apply(*id, applied_time_ns.unwrap_or(time), &plan);
                    }
                }
            }
            _ => return,
        }
        self.revision += 1;
    }

    fn apply(&mut self, id: u32, at: u64, plan: &FaultAction) {
        match plan {
            FaultAction::Stop { machine }
            | FaultAction::PauseProcess { machine }
            | FaultAction::Restart { machine }
            | FaultAction::ResumeProcess { machine } => {
                let condition = match plan {
                    FaultAction::Stop { .. } => ApplicationCondition::Stopped,
                    FaultAction::PauseProcess { .. } => ApplicationCondition::Paused,
                    _ => ApplicationCondition::Running,
                };
                self.machine(machine).application = ApplicationState {
                    condition,
                    changed_at_ns: Some(at),
                    exit_code: None,
                };
                if matches!(
                    condition,
                    ApplicationCondition::Stopped | ApplicationCondition::Paused
                ) {
                    self.process_effects.insert(machine.clone(), id);
                } else {
                    self.process_effects.remove(machine);
                }
            }
            // A debugging probe runs beside the application and changes no
            // world state.
            FaultAction::Exec { .. } => {}
            FaultAction::Pause { machine } => {
                self.machine_pauses.insert(machine.clone(), id);
            }
            FaultAction::Resume { machine } => {
                self.machine_pauses.remove(machine);
            }
            FaultAction::Crash { machine } | FaultAction::Reboot { machine } => {
                self.machine_pauses.remove(machine);
                self.process_effects.remove(machine);
            }
            FaultAction::Partition {
                handle,
                left,
                right,
            } => {
                let mut cut = pairs(left, right);
                cut.extend(pairs(right, left));
                for name in left.iter().chain(right) {
                    self.machine(name);
                }
                self.partitions.insert(handle.clone(), (id, cut));
            }
            FaultAction::Shape {
                handle,
                from,
                to,
                latency_ns,
                jitter_ns,
                loss_ppm,
                bandwidth_bps,
                ..
            } => {
                for name in from.iter().chain(to) {
                    self.machine(name);
                }
                let policy = LinkPolicy {
                    latency_ns: *latency_ns,
                    jitter_ns: *jitter_ns,
                    loss_ppm: *loss_ppm,
                    bandwidth_bps: *bandwidth_bps,
                };
                self.shapes
                    .insert(handle.clone(), (id, pairs(from, to), policy));
            }
            FaultAction::ClockSkew { handle, .. }
            | FaultAction::ClockDrift { handle, .. }
            | FaultAction::Throttle { handle, .. }
            | FaultAction::DiskError { handle, .. }
            | FaultAction::DiskLatency { handle, .. }
            | FaultAction::Preempt { handle, .. }
            | FaultAction::DropConnection { handle, .. } => {
                self.healable.insert(handle.clone(), id);
            }
            FaultAction::Heal { handle } => {
                self.healable.remove(handle);
                self.partitions.remove(handle);
                self.shapes.remove(handle);
            }
        }
    }

    /// Count a batch of routed frames. Counters do not advance the revision.
    pub fn frames(&mut self, time: u64, frames: &[Frame]) {
        self.time = self.time.max(time);
        for frame in frames {
            if !self.machines.contains_key(&frame.from) || !self.machines.contains_key(&frame.to) {
                self.machine(&frame.from);
                self.machine(&frame.to);
            }
            let counters = self
                .counters
                .entry((frame.from.clone(), frame.to.clone()))
                .or_default();
            match frame.fate {
                FrameFate::Scheduled => counters.scheduled += 1,
                FrameFate::PartitionDropped => counters.partition_dropped += 1,
                FrameFate::LossDropped => counters.loss_dropped += 1,
                FrameFate::QueueDropped => counters.queue_dropped += 1,
                FrameFate::ReceiverDropped => counters.receiver_dropped += 1,
            }
            counters.bytes += u64::from(frame.bytes);
            counters.last_sent_ns = Some(counters.last_sent_ns.unwrap_or(0).max(frame.sent_ns));
            self.time = self.time.max(frame.sent_ns);
        }
    }

    /// Logs contribute observed time and incarnation only after the manager
    /// has checked their machine attribution.
    pub fn log(&mut self, entry: &seedbox_api::LogEntry) {
        if let seedbox_api::LogEntry::Bytes {
            machine,
            incarnation,
            simulated_time_ns,
            ..
        }
        | seedbox_api::LogEntry::MachineGap {
            machine,
            incarnation,
            simulated_time_ns,
            ..
        } = entry
        {
            self.time = self.time.max(*simulated_time_ns);
            let m = self.machine(machine);
            if *incarnation > m.incarnation {
                m.incarnation = *incarnation;
                self.revision += 1;
            }
        }
    }

    pub fn snapshot(&self) -> WorldState {
        let names: Vec<&String> = self.machines.keys().collect();
        let mut links = Vec::new();
        for from in &names {
            for to in &names {
                if from == to {
                    continue;
                }
                let pair = ((*from).clone(), (*to).clone());
                let shape = self
                    .shapes
                    .iter()
                    .filter(|(_, (_, set, _))| set.contains(&pair))
                    .max_by_key(|(_, (id, _, _))| *id);
                let partition = self
                    .partitions
                    .iter()
                    .filter(|(_, (_, set))| set.contains(&pair))
                    .max_by_key(|(_, (id, _))| *id);
                links.push(LinkState {
                    policy: shape
                        .map(|(_, (_, _, policy))| policy.clone())
                        .or_else(|| self.configured.get(&pair).cloned())
                        .unwrap_or_else(|| self.base.clone()),
                    partitioned_by: partition.map(|(handle, _)| handle.clone()),
                    shaped_by: shape.map(|(handle, _)| handle.clone()),
                    counters: self.counters.get(&pair).cloned().unwrap_or_default(),
                    from: pair.0,
                    to: pair.1,
                });
            }
        }
        let active: BTreeSet<u32> = self
            .partitions
            .values()
            .map(|(id, _)| *id)
            .chain(self.shapes.values().map(|(id, _, _)| *id))
            .chain(self.healable.values().copied())
            .chain(self.process_effects.values().copied())
            .chain(self.machine_pauses.values().copied())
            .collect();
        WorldState {
            triggers: self.triggers.clone(),
            choice_inputs: self.choice_inputs.clone(),
            override_rejections: self.override_rejections.clone(),
            simulation: self.simulation.clone(),
            revision: self.revision,
            simulated_time_ns: self.time,
            machines: self.machines.values().cloned().collect(),
            links,
            faults: self
                .faults
                .values()
                .map(|fault| FaultState {
                    active: active.contains(&fault.id),
                    ..fault.clone()
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker() -> Tracker {
        tracker_with_bandwidth(None)
    }

    fn tracker_with_bandwidth(bandwidth_bps: Option<u64>) -> Tracker {
        let service = |name| seedbox_spec::Service {
            memory_mb: None,
            cpu: seedbox_spec::project::Cpu {
                speed_hz: match name {
                    "a" => 800_000_000,
                    "b" => 2_000_000_000,
                    _ => 1_000_000_000,
                },
                jitter_percent: 0,
            },
            source: seedbox_spec::ImageSource::Registry {
                reference: "example".into(),
            },
            platform: "linux/amd64".into(),
            entrypoint: None,
            command: None,
            environment: Default::default(),
            working_dir: None,
            user: None,
            configs: Vec::new(),
            volumes: Vec::new(),
            network: "default".into(),
        };
        let mut spec = seedbox_spec::Simulation {
            name: "test".into(),
            source: "/project/compose.yaml".into(),
            services: ["a", "b", "oracle"]
                .into_iter()
                .map(|name| (name.to_owned(), service(name)))
                .collect(),
            configs: Default::default(),
            network: seedbox_spec::Network {
                name: "default".into(),
                latency_ns: 1_000_000,
                jitter_ns: 0,
                bandwidth_bps,
                queue_bound_ns: None,
                links: Vec::new(),
            },
            schedule: Default::default(),
            control: None,
            limits: seedbox_spec::Limits {
                simulated_time_ns: 1,
                wall_time_ns: 1,
            },
            check: seedbox_spec::Check {
                service: "oracle".into(),
                exit_code: 0,
            },
            seed: None,
            wall_clock_epoch: None,
            machine: Default::default(),
            disk: Default::default(),
            progress_client: Default::default(),
        };
        spec.network.links.push(seedbox_spec::Link {
            from: "a".into(),
            to: "b".into(),
            latency_ns: 7,
            jitter_ns: 0,
            loss_ppm: 0,
            bandwidth_bps: None,
            queue_bound_ns: None,
        });
        Tracker::new("k7m2p9qx".parse().unwrap(), &spec)
    }

    fn link<'a>(world: &'a WorldState, from: &str, to: &str) -> &'a LinkState {
        world
            .links
            .iter()
            .find(|l| l.from == from && l.to == to)
            .unwrap()
    }

    fn fault(id: u32, status: FaultStatus, plan: FaultAction) -> EventKind {
        EventKind::FaultChanged {
            counts: None,
            id,
            action: plan_name(&plan),
            machine: None,
            status,
            applied_time_ns: (status == FaultStatus::Applied).then_some(10),
            plan: Some(Box::new(serde_json::to_value(plan).unwrap())),
            input_key: None,
        }
    }

    #[test]
    fn selected_connection_loss_is_discoverable_and_healable() {
        let mut t = tracker();
        let action: FaultAction = serde_json::from_value(serde_json::json!({
            "action":"drop_connection", "handle":"selected", "connection":{
                "from":"a", "to":"b", "src_addr":"10.0.0.1", "dst_addr":"10.0.0.2",
                "src_port":1234, "dst_port":80, "incarnation":1
            }
        }))
        .unwrap();
        t.event(10, &fault(1, FaultStatus::Applied, action));
        assert!(t.snapshot().faults[0].active);
        let forms = crate::fault_catalogue::discover(&t.snapshot(), 1_000_000);
        let heal = forms.faults.iter().find(|f| f.name == "heal").unwrap();
        assert_eq!(
            heal.schema["properties"]["handle"]["enum"],
            serde_json::json!(["selected"])
        );
        t.event(
            20,
            &fault(
                2,
                FaultStatus::Applied,
                FaultAction::Heal {
                    handle: "selected".into(),
                },
            ),
        );
        assert!(!t.snapshot().faults[0].active);
    }

    fn plan_name(plan: &FaultAction) -> String {
        plan.name().into()
    }

    #[test]
    fn process_state_uses_confirmed_effect_time_and_survives_machine_pause() {
        let mut t = tracker();
        t.event(
            10,
            &EventKind::MachineReady {
                machine: "a".into(),
                incarnation: 0,
            },
        );
        let action = FaultAction::PauseProcess {
            machine: "a".into(),
        };
        t.event(20, &fault(1, FaultStatus::Requested, action.clone()));
        assert_eq!(
            t.snapshot().machines[0].application.condition,
            ApplicationCondition::Running
        );
        let mut applied = fault(1, FaultStatus::Applied, action);
        if let EventKind::FaultChanged {
            applied_time_ns, ..
        } = &mut applied
        {
            *applied_time_ns = Some(25);
        }
        t.event(30, &applied);
        let world = t.snapshot();
        assert_eq!(world.machines[0].condition, MachineCondition::Ready);
        assert_eq!(
            world.machines[0].application.condition,
            ApplicationCondition::Paused
        );
        assert_eq!(world.machines[0].application.changed_at_ns, Some(25));
        assert!(world.faults[0].active);
        t.event(
            40,
            &EventKind::MachineLifecycle {
                machine: "a".into(),
                incarnation: 0,
                transition: MachineTransition::Paused,
            },
        );
        let world = t.snapshot();
        assert_eq!(world.machines[0].condition, MachineCondition::Paused);
        assert_eq!(world.machines[0].condition_since_ns, Some(40));
        assert_eq!(world.machines[0].last_transition_at_ns, Some(40));
        assert_eq!(world.machines[0].application.changed_at_ns, Some(25));
        t.event(
            50,
            &EventKind::MachineLifecycle {
                machine: "a".into(),
                incarnation: 0,
                transition: MachineTransition::Resumed,
            },
        );
        assert_eq!(
            t.snapshot().machines[0].application.condition,
            ApplicationCondition::Paused
        );
        t.event(
            60,
            &fault(
                2,
                FaultStatus::Applied,
                FaultAction::ResumeProcess {
                    machine: "a".into(),
                },
            ),
        );
        assert_eq!(
            t.snapshot().machines[0].application.condition,
            ApplicationCondition::Running
        );
        assert!(!t.snapshot().faults[0].active);
    }

    #[test]
    fn resuming_during_boot_does_not_invent_readiness() {
        let mut t = tracker();
        for (time, transition) in [
            (10, MachineTransition::Paused),
            (20, MachineTransition::Resumed),
            (30, MachineTransition::Paused),
            (40, MachineTransition::Resumed),
        ] {
            t.event(
                time,
                &EventKind::MachineLifecycle {
                    machine: "a".into(),
                    incarnation: 0,
                    transition,
                },
            );
        }
        let world = t.snapshot();
        assert_eq!(world.machines[0].condition, MachineCondition::Starting);
        assert_eq!(
            world.machines[0].application.condition,
            ApplicationCondition::Unknown
        );
        assert_eq!(world.machines[0].condition_since_ns, Some(40));
    }

    #[test]
    fn crash_clears_process_effect_and_reboot_waits_for_readiness() {
        let mut t = tracker();
        t.event(
            10,
            &fault(
                1,
                FaultStatus::Applied,
                FaultAction::Stop {
                    machine: "a".into(),
                },
            ),
        );
        assert_eq!(
            t.snapshot().machines[0].application.condition,
            ApplicationCondition::Stopped
        );
        assert!(t.snapshot().faults[0].active);
        t.event(
            20,
            &EventKind::MachineLifecycle {
                machine: "a".into(),
                incarnation: 0,
                transition: MachineTransition::Crashed,
            },
        );
        assert_eq!(
            t.snapshot().machines[0].application.condition,
            ApplicationCondition::Unavailable
        );
        assert!(!t.snapshot().faults[0].active);
        t.event(
            30,
            &EventKind::MachineLifecycle {
                machine: "a".into(),
                incarnation: 1,
                transition: MachineTransition::Rebooted,
            },
        );
        let world = t.snapshot();
        assert_eq!(world.machines[0].incarnation, 1);
        assert_eq!(
            world.machines[0].application.condition,
            ApplicationCondition::Starting
        );
        t.event(
            40,
            &EventKind::MachineReady {
                machine: "a".into(),
                incarnation: 1,
            },
        );
        assert_eq!(
            t.snapshot().machines[0].application.condition,
            ApplicationCondition::Running
        );
        t.event(
            50,
            &EventKind::MachineExited {
                machine: "a".into(),
                incarnation: 1,
                code: 7,
            },
        );
        assert_eq!(t.snapshot().machines[0].application.exit_code, Some(7));
        assert_eq!(t.snapshot().machines[0].application.changed_at_ns, Some(50));
    }

    #[test]
    fn out_of_memory_kills_survive_exit_and_reboot_and_stay_bounded() {
        let mut t = tracker();
        let kill = |incarnation| EventKind::OutOfMemoryKill {
            machine: "a".into(),
            incarnation,
            process: Some(seedbox_api::OutOfMemoryProcess {
                pid: 77,
                command: "memhog".into(),
                total_vm_kib: 300_000,
                anon_rss_kib: 250_000,
                file_rss_kib: 900,
                shmem_rss_kib: 0,
            }),
        };
        let before = t.snapshot().revision;
        t.event(10, &kill(0));
        assert!(t.snapshot().revision > before);
        t.event(
            10,
            &EventKind::MachineExited {
                machine: "a".into(),
                incarnation: 0,
                code: 137,
            },
        );
        let machine = &t.snapshot().machines[0];
        assert_eq!(machine.out_of_memory_kills.len(), 1);
        assert_eq!(machine.out_of_memory_kills[0].simulated_time_ns, 10);
        assert_eq!(
            machine.exited_out_of_memory(),
            machine.out_of_memory_kills.last()
        );
        t.event(
            20,
            &EventKind::MachineLifecycle {
                machine: "a".into(),
                incarnation: 1,
                transition: MachineTransition::Rebooted,
            },
        );
        t.event(
            30,
            &EventKind::MachineExited {
                machine: "a".into(),
                incarnation: 1,
                code: 137,
            },
        );
        // The earlier incarnation's kill is retained but explains no later exit.
        let machine = &t.snapshot().machines[0];
        assert_eq!(machine.out_of_memory_kills.len(), 1);
        assert_eq!(machine.exited_out_of_memory(), None);
        for _ in 0..MAX_OUT_OF_MEMORY_KILLS {
            t.event(40, &kill(1));
        }
        let kills = &t.snapshot().machines[0].out_of_memory_kills;
        assert_eq!(kills.len(), MAX_OUT_OF_MEMORY_KILLS);
        assert!(kills.iter().all(|kill| kill.incarnation == 1));
    }

    #[test]
    fn rejected_override_is_observable_without_inventing_a_fault_id() {
        let mut tracker = tracker();
        let before = tracker.snapshot().revision;
        tracker.event(
            42,
            &EventKind::OverrideRejected {
                input_key: "heal-missing".into(),
                at_ns: 42,
                reason: "handle is not active".into(),
            },
        );
        let world = tracker.snapshot();
        assert!(world.revision > before);
        assert_eq!(
            world.override_rejections["heal-missing"],
            "handle is not active"
        );
        assert!(world.faults.is_empty());
    }

    #[test]
    fn clock_and_throttle_handles_are_discovered_until_healed() {
        let mut t = tracker();
        for (id, plan) in [
            (
                1,
                FaultAction::ClockSkew {
                    handle: "skew".into(),
                    machine: "a".into(),
                    offset_ns: Some(1),
                    max_offset_ns: None,
                },
            ),
            (
                2,
                FaultAction::ClockDrift {
                    handle: "drift".into(),
                    machine: "a".into(),
                    rate_ppb: Some(1),
                    max_rate_ppb: None,
                },
            ),
            (
                3,
                FaultAction::Throttle {
                    handle: "slow".into(),
                    machine: "a".into(),
                    speed_hz: 1_000_000_000,
                },
            ),
        ] {
            t.apply(id, 10, &plan);
            t.faults.insert(
                id,
                FaultState {
                    counts: None,
                    id,
                    action: plan.name().into(),
                    machine: Some("a".into()),
                    status: FaultStatus::Applied,
                    applied_time_ns: Some(10),
                    plan: Some(serde_json::to_value(plan).unwrap()),
                    input_key: None,
                    active: false,
                },
            );
        }
        let forms = crate::fault_catalogue::discover(&t.snapshot(), 1_000_000);
        let heal = forms.faults.iter().find(|f| f.name == "heal").unwrap();
        assert_eq!(
            heal.schema["properties"]["handle"]["enum"],
            serde_json::json!(["drift", "skew", "slow"])
        );
        t.apply(
            4,
            20,
            &FaultAction::Heal {
                handle: "skew".into(),
            },
        );
        let forms = crate::fault_catalogue::discover(&t.snapshot(), 1_000_000);
        let heal = forms.faults.iter().find(|f| f.name == "heal").unwrap();
        assert_eq!(
            heal.schema["properties"]["handle"]["enum"],
            serde_json::json!(["drift", "slow"])
        );
    }

    #[test]
    fn spec_seeds_machines_and_policies() {
        let world = tracker().snapshot();
        assert_eq!(
            world
                .machines
                .iter()
                .map(|m| m.cpu_speed_hz)
                .collect::<Vec<_>>(),
            [Some(800_000_000), Some(2_000_000_000), Some(1_000_000_000)]
        );
        assert_eq!(
            world
                .machines
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "oracle"]
        );
        assert!(world
            .machines
            .iter()
            .all(|m| m.condition == MachineCondition::Starting));
        assert!(
            world
                .machines
                .iter()
                .find(|m| m.name == "oracle")
                .unwrap()
                .oracle
        );
        assert_eq!(world.links.len(), 6);
        assert_eq!(link(&world, "a", "b").policy.latency_ns, 7);
        assert_eq!(link(&world, "b", "a").policy.latency_ns, 1_000_000);
    }

    #[test]
    fn throttle_and_heal_preserve_each_machines_configured_speed() {
        let mut tracker = tracker();
        for (time, speed_hz) in [(10, Some(200_000_000)), (20, None)] {
            tracker.event(
                time,
                &EventKind::MachineLifecycle {
                    machine: "a".into(),
                    incarnation: 0,
                    transition: MachineTransition::Throttled { speed_hz },
                },
            );
            let world = tracker.snapshot();
            assert_eq!(world.machines[0].cpu_speed_hz, Some(800_000_000));
            assert_eq!(world.machines[0].throttle_speed_hz, speed_hz);
            assert_eq!(world.machines[1].cpu_speed_hz, Some(2_000_000_000));
            assert_eq!(world.machines[1].throttle_speed_hz, None);
        }
    }

    #[test]
    fn baseline_without_bandwidth_overrides_finite_global_bandwidth() {
        let world = tracker_with_bandwidth(Some(1000)).snapshot();
        assert_eq!(link(&world, "a", "b").policy.bandwidth_bps, None);
        assert_eq!(link(&world, "b", "a").policy.bandwidth_bps, Some(1000));
    }

    #[test]
    fn partitions_and_shapes_follow_applied_faults_until_healed() {
        let mut t = tracker();
        let partition = FaultAction::Partition {
            handle: "p".into(),
            left: vec!["a".into()],
            right: vec!["b".into(), "oracle".into()],
        };
        t.event(5, &fault(1, FaultStatus::Requested, partition.clone()));
        assert!(link(&t.snapshot(), "a", "b").partitioned_by.is_none());
        t.event(10, &fault(1, FaultStatus::Applied, partition));
        let world = t.snapshot();
        for (from, to) in [("a", "b"), ("b", "a"), ("oracle", "a")] {
            assert_eq!(link(&world, from, to).partitioned_by.as_deref(), Some("p"));
        }
        assert!(link(&world, "b", "oracle").partitioned_by.is_none());
        assert!(world.faults[0].active);
        t.event(
            11,
            &fault(
                2,
                FaultStatus::Applied,
                FaultAction::Shape {
                    handle: "s".into(),
                    from: vec!["b".into()],
                    to: vec!["oracle".into()],
                    latency_ns: 9,
                    jitter_ns: 1,
                    loss_ppm: 5,
                    bandwidth_bps: None,
                    queue_bound_ns: None,
                    network_faults: Default::default(),
                },
            ),
        );
        let world = t.snapshot();
        assert_eq!(link(&world, "b", "oracle").policy.latency_ns, 9);
        assert_eq!(link(&world, "b", "oracle").shaped_by.as_deref(), Some("s"));
        assert!(link(&world, "oracle", "b").shaped_by.is_none());
        t.event(
            12,
            &fault(
                3,
                FaultStatus::Applied,
                FaultAction::Heal { handle: "p".into() },
            ),
        );
        let world = t.snapshot();
        assert!(world.links.iter().all(|l| l.partitioned_by.is_none()));
        assert!(!world.faults[0].active && world.faults[1].active);
    }

    #[test]
    fn machines_appear_from_observations_and_follow_their_lifecycle() {
        let mut t = tracker();
        let before = t.snapshot().revision;
        t.event(
            3,
            &EventKind::MachineReady {
                machine: "late".into(),
                incarnation: 0,
            },
        );
        t.event(
            4,
            &EventKind::MachineLifecycle {
                machine: "a".into(),
                incarnation: 0,
                transition: MachineTransition::Crashed,
            },
        );
        t.event(
            6,
            &EventKind::MachineLifecycle {
                machine: "a".into(),
                incarnation: 1,
                transition: MachineTransition::Rebooted,
            },
        );
        let world = t.snapshot();
        assert!(world.revision > before);
        assert_eq!(world.machines.len(), 4);
        assert_eq!(world.links.len(), 12);
        let a = world.machines.iter().find(|m| m.name == "a").unwrap();
        assert_eq!(
            (a.incarnation, a.condition),
            (1, MachineCondition::Starting)
        );
        let late = world.machines.iter().find(|m| m.name == "late").unwrap();
        assert_eq!(late.condition, MachineCondition::Ready);
    }

    #[test]
    fn attributed_logs_advance_observed_time_without_changing_condition() {
        let mut t = tracker();
        t.log(&seedbox_api::LogEntry::MachineGap {
            machine: "a".into(),
            incarnation: 2,
            simulated_time_ns: 42,
            records: 1,
        });
        let world = t.snapshot();
        let machine = world.machines.iter().find(|m| m.name == "a").unwrap();
        assert_eq!(world.simulated_time_ns, 42);
        assert_eq!(machine.incarnation, 2);
        assert_eq!(machine.condition, MachineCondition::Starting);
    }

    #[test]
    fn frames_count_per_direction_without_a_new_revision() {
        let mut t = tracker();
        let revision = t.snapshot().revision;
        let frame = |fate, sent_ns| Frame {
            from: "a".into(),
            to: "b".into(),
            sent_ns,
            due_ns: None,
            bytes: 60,
            fate,
        };
        t.frames(
            20,
            &[
                frame(FrameFate::Scheduled, 12),
                frame(FrameFate::PartitionDropped, 15),
            ],
        );
        let world = t.snapshot();
        assert_eq!(world.revision, revision);
        assert_eq!(world.simulated_time_ns, 20);
        let counters = &link(&world, "a", "b").counters;
        assert_eq!(
            (
                counters.scheduled,
                counters.partition_dropped,
                counters.bytes
            ),
            (1, 1, 120)
        );
        assert_eq!(counters.last_sent_ns, Some(15));
        assert_eq!(link(&world, "b", "a").counters, LinkCounters::default());
    }
}
