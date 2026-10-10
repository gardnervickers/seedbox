//! Folds a simulation's engine observations into the world clients discover.
//! The spec seeds the initial machines and link policies; everything after
//! that, including machines the spec never named, comes from observations.
use seedbox_api::{
    EventKind, FaultAction, FaultState, FaultStatus, Frame, FrameFate, LinkCounters, LinkPolicy,
    LinkState, MachineCondition, MachineState, MachineTransition, SimulationId, WorldState,
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
    counters: BTreeMap<Pair, LinkCounters>,
    /// Active partition and shape handles, with the fault that set them.
    partitions: BTreeMap<String, (u32, BTreeSet<Pair>)>,
    shapes: BTreeMap<String, (u32, BTreeSet<Pair>, LinkPolicy)>,
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
                            bandwidth_bps: link.bandwidth_bps.or(network.bandwidth_bps),
                        },
                    )
                })
                .collect(),
            machines: BTreeMap::new(),
            faults: BTreeMap::new(),
            counters: BTreeMap::new(),
            partitions: BTreeMap::new(),
            shapes: BTreeMap::new(),
        };
        for name in configuration.services.keys() {
            tracker.machine(name);
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
                    throttle_permille: None,
                    oracle: name == self.oracle,
                    last_transition: None,
                },
            );
        }
        self.machines.get_mut(name).expect("inserted above")
    }

    /// Fold one engine event. Events that describe no world entity are ignored.
    pub fn event(&mut self, time: u64, event: &EventKind) {
        self.time = self.time.max(time);
        match event {
            EventKind::MachineReady {
                machine,
                incarnation,
            } => {
                let m = self.machine(machine);
                m.incarnation = *incarnation;
                m.condition = MachineCondition::Ready;
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
            }
            EventKind::MachineLifecycle {
                machine,
                incarnation,
                transition,
            } => {
                let m = self.machine(machine);
                m.incarnation = *incarnation;
                m.last_transition = Some(*transition);
                match transition {
                    MachineTransition::Paused => m.condition = MachineCondition::Paused,
                    MachineTransition::Resumed => m.condition = MachineCondition::Ready,
                    MachineTransition::Crashed => m.condition = MachineCondition::Crashed,
                    MachineTransition::Rebooted => {
                        m.condition = MachineCondition::Starting;
                        m.exit_code = None;
                    }
                    MachineTransition::Throttled { factor_permille } => {
                        m.throttle_permille =
                            (*factor_permille != 1000).then_some(*factor_permille);
                    }
                }
            }
            EventKind::FaultChanged {
                id,
                action,
                machine,
                status,
                applied_time_ns,
                plan,
            } => {
                if let Some(machine) = machine {
                    self.machine(machine);
                }
                let fault = self.faults.entry(*id).or_insert_with(|| FaultState {
                    id: *id,
                    action: action.clone(),
                    machine: machine.clone(),
                    status: *status,
                    applied_time_ns: None,
                    plan: None,
                    active: false,
                });
                fault.status = *status;
                fault.applied_time_ns = *applied_time_ns;
                if plan.is_some() {
                    fault.plan = plan.clone();
                }
                if *status == FaultStatus::Applied {
                    if let Some(plan) = fault.plan.clone() {
                        self.apply(*id, &plan);
                    }
                }
            }
            _ => return,
        }
        self.revision += 1;
    }

    fn apply(&mut self, id: u32, plan: &FaultAction) {
        match plan {
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
            FaultAction::Heal { handle } => {
                self.partitions.remove(handle);
                self.shapes.remove(handle);
            }
            _ => {}
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
            .collect();
        WorldState {
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
        let service = |_| seedbox_spec::Service {
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
                bandwidth_bps: None,
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
            id,
            action: plan_name(&plan),
            machine: None,
            status,
            applied_time_ns: (status == FaultStatus::Applied).then_some(10),
            plan: Some(plan),
        }
    }

    fn plan_name(plan: &FaultAction) -> String {
        plan.name().into()
    }

    #[test]
    fn spec_seeds_machines_and_policies() {
        let world = tracker().snapshot();
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
