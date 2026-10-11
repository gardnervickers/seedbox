//! What happened, what is happening and what is due: fault spans, upcoming
//! inputs and machine transitions, derived for display. Everything comes from
//! the observed world, the server's fault timeline and events seen while
//! watching. A fault's meaning is never assumed beyond its target machine, its
//! handle and the `heal` action that closes a handle.
use seedbox_api::{FaultStatus, MachineTransition, TimelineEntry, WorldState};

/// Selection identity that survives refreshes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Key {
    Fault(u32),
    Input(String),
    Transition(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Applied and not yet healed.
    Active,
    /// Applied, then closed by a heal or recovery.
    Closed,
    /// Applied with no lasting state of its own.
    Instant,
    /// Scheduled or requested; its time has not come or it waits for the guest.
    Upcoming,
    /// The server could not apply it.
    Failed,
    /// A machine was down or paused for this span.
    Down,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub key: Key,
    pub lane: Lane,
    pub action: String,
    pub target: String,
    pub handle: Option<String>,
    pub start_ns: u64,
    /// For spans: when it closed, or `None` while open.
    pub end_ns: Option<u64>,
    /// What closed the span, such as `heal` or `ready`.
    pub closed_by: Option<String>,
    pub state: State,
}

impl Item {
    pub fn is_span(&self) -> bool {
        matches!(self.state, State::Active | State::Closed | State::Down)
    }
    pub fn open(&self) -> bool {
        self.is_span() && self.end_ns.is_none()
    }
}

/// A row of the time bar. Machines keep their lane after they leave the world.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Lane {
    Machine(String),
    Handle(String),
    Other(String),
}

impl Lane {
    pub fn label(&self) -> &str {
        match self {
            Lane::Machine(name) | Lane::Handle(name) | Lane::Other(name) => name,
        }
    }
}

/// A machine transition observed on the live event stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transition {
    pub at_ns: u64,
    pub machine: String,
    pub incarnation: u32,
    pub change: Change,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Crashed,
    Exited(u8),
    Paused,
    Resumed,
    Rebooted,
    Ready,
    Throttled(Option<u64>),
}

impl Change {
    pub fn from_lifecycle(transition: &MachineTransition) -> Self {
        match transition {
            MachineTransition::Paused => Change::Paused,
            MachineTransition::Resumed => Change::Resumed,
            MachineTransition::Crashed => Change::Crashed,
            MachineTransition::Rebooted => Change::Rebooted,
            MachineTransition::Throttled { speed_hz } => Change::Throttled(*speed_hz),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Activity {
    pub active: Vec<Item>,
    pub upcoming: Vec<Item>,
    pub history: Vec<Item>,
}

impl Activity {
    /// Every item in panel order: active, upcoming, then newest history first.
    pub fn rows(&self) -> impl Iterator<Item = &Item> {
        self.active
            .iter()
            .chain(self.upcoming.iter())
            .chain(self.history.iter())
    }

    /// Lanes in display order: machines in world order, departed machines,
    /// then handle lanes by first appearance.
    pub fn lanes(&self, world: Option<&WorldState>) -> Vec<Lane> {
        let mut lanes: Vec<Lane> = Vec::new();
        if let Some(world) = world {
            for machine in &world.machines {
                let lane = Lane::Machine(machine.name.clone());
                if self.rows().any(|item| item.lane == lane) {
                    lanes.push(lane);
                }
            }
        }
        let mut rest: Vec<(u64, Lane)> = Vec::new();
        for item in self.rows() {
            if lanes.contains(&item.lane) {
                continue;
            }
            match rest.iter_mut().find(|(_, lane)| *lane == item.lane) {
                Some(entry) => entry.0 = entry.0.min(item.start_ns),
                None => rest.push((item.start_ns, item.lane.clone())),
            }
        }
        rest.sort_by(|a, b| {
            let rank = |lane: &Lane| match lane {
                Lane::Machine(_) => 0,
                Lane::Handle(_) => 1,
                Lane::Other(_) => 2,
            };
            rank(&a.1).cmp(&rank(&b.1)).then(a.0.cmp(&b.0))
        });
        lanes.extend(rest.into_iter().map(|(_, lane)| lane));
        lanes
    }

    /// The next upcoming item at or after `now`.
    pub fn next(&self, now: u64) -> Option<&Item> {
        self.upcoming.iter().find(|item| item.start_ns >= now)
    }
}

pub fn action_name(action: &serde_json::Value) -> String {
    action["action"].as_str().unwrap_or("fault").to_owned()
}

/// A short, application-agnostic description of what an action targets.
pub fn fault_target(plan: &serde_json::Value) -> String {
    if let Some(machine) = plan["machine"].as_str() {
        return machine.to_owned();
    }
    let group = |key: &str| {
        plan[key].as_array().map(|values| {
            values
                .iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
    };
    let handle = plan["handle"].as_str();
    let with_handle = |text: String| match handle {
        Some(handle) => format!("{handle} {text}"),
        None => text,
    };
    if let (Some(left), Some(right)) = (group("left"), group("right")) {
        return with_handle(format!("{left}┃{right}"));
    }
    if let (Some(from), Some(to)) = (group("from"), group("to")) {
        return with_handle(format!("{from}→{to}"));
    }
    if let Some(handle) = handle {
        return handle.to_owned();
    }
    plan.as_object()
        .map(|fields| {
            fields
                .iter()
                .filter(|(key, _)| key.as_str() != "action")
                .map(|(key, value)| {
                    format!(
                        "{key}={}",
                        value
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| value.to_string())
                    )
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

fn handle_of(plan: &serde_json::Value) -> Option<String> {
    plan["handle"].as_str().map(str::to_owned)
}

fn is_heal(action: &str) -> bool {
    action == "heal"
}

/// Lane for an action: its machine, else its handle, else the lane of the
/// fault a heal closes, else the action itself.
fn lane_for(
    machine: Option<&str>,
    plan: &serde_json::Value,
    action: &str,
    world: &WorldState,
) -> Lane {
    if let Some(machine) = machine.or_else(|| plan["machine"].as_str()) {
        return Lane::Machine(machine.to_owned());
    }
    if let Some(handle) = plan["handle"].as_str() {
        if is_heal(action) {
            if let Some(healed) = world.faults.iter().find(|f| {
                !is_heal(&f.action)
                    && f.plan.as_ref().and_then(handle_of).as_deref() == Some(handle)
            }) {
                return lane_for(
                    healed.machine.as_deref(),
                    healed.plan.as_ref().unwrap_or(&serde_json::Value::Null),
                    &healed.action,
                    world,
                );
            }
        }
        return Lane::Handle(handle.to_owned());
    }
    Lane::Other(action.to_owned())
}

/// Derive panel and time-bar items. `now` is the newest committed simulated time.
pub fn derive(
    world: Option<&WorldState>,
    timeline: &[TimelineEntry],
    transitions: &[Transition],
    now: u64,
) -> Activity {
    let mut activity = Activity::default();
    let empty = WorldState {
        override_rejections: Default::default(),
        simulation: "aaaaaaaa".parse().expect("valid placeholder id"),
        revision: 0,
        simulated_time_ns: 0,
        machines: Vec::new(),
        links: Vec::new(),
        faults: Vec::new(),
    };
    let world_ref = world.unwrap_or(&empty);
    let null = serde_json::Value::Null;

    // Heals applied per handle, earliest first.
    let mut heals: Vec<(String, u64)> = world_ref
        .faults
        .iter()
        .filter(|f| is_heal(&f.action) && f.status == FaultStatus::Applied)
        .filter_map(|f| {
            Some((
                f.plan.as_ref().and_then(handle_of)?,
                f.applied_time_ns.unwrap_or(0),
            ))
        })
        .collect();
    heals.sort_by_key(|(_, at)| *at);

    let scheduled: std::collections::BTreeSet<u32> =
        timeline.iter().filter_map(|entry| entry.fault_id).collect();

    for fault in &world_ref.faults {
        let plan = fault.plan.as_ref().unwrap_or(&null);
        let lane = lane_for(fault.machine.as_deref(), plan, &fault.action, world_ref);
        let target = if plan.is_null() {
            fault.machine.clone().unwrap_or_default()
        } else {
            fault_target(plan)
        };
        let handle = handle_of(plan);
        let item = |state, start_ns, end_ns, closed_by| Item {
            key: Key::Fault(fault.id),
            lane: lane.clone(),
            action: fault.action.clone(),
            target: target.clone(),
            handle: handle.clone(),
            start_ns,
            end_ns,
            closed_by,
            state,
        };
        match fault.status {
            FaultStatus::Unrequested | FaultStatus::Requested => {
                if !scheduled.contains(&fault.id) {
                    activity.upcoming.push(item(
                        State::Upcoming,
                        fault.applied_time_ns.unwrap_or(now),
                        None,
                        None,
                    ));
                }
            }
            FaultStatus::Failed | FaultStatus::Unavailable => {
                let at = fault.applied_time_ns.unwrap_or(now);
                activity.history.push(item(State::Failed, at, None, None));
            }
            FaultStatus::Applied => {
                let at = fault.applied_time_ns.unwrap_or(0);
                if is_heal(&fault.action) {
                    let closes_something = handle.as_ref().is_some_and(|h| {
                        world_ref.faults.iter().any(|f| {
                            !is_heal(&f.action)
                                && f.plan.as_ref().and_then(handle_of).as_ref() == Some(h)
                        })
                    });
                    if !closes_something {
                        activity.history.push(item(State::Instant, at, None, None));
                    }
                    continue;
                }
                let healed_at = handle.as_ref().and_then(|h| {
                    heals
                        .iter()
                        .find(|(handle, heal_at)| handle == h && *heal_at >= at)
                        .map(|(_, heal_at)| *heal_at)
                });
                if fault.active {
                    activity.active.push(item(State::Active, at, None, None));
                } else if let Some(end) = healed_at {
                    activity
                        .history
                        .push(item(State::Closed, at, Some(end), Some("heal".into())));
                } else {
                    activity.history.push(item(State::Instant, at, None, None));
                }
            }
        }
    }

    for entry in timeline {
        let observed = entry
            .fault_id
            .and_then(|id| world_ref.faults.iter().find(|f| f.id == id));
        let pending = observed
            .is_none_or(|f| matches!(f.status, FaultStatus::Unrequested | FaultStatus::Requested));
        let rejected = entry
            .id
            .strip_prefix("program:")
            .is_some_and(|key| world_ref.override_rejections.contains_key(key));
        if !pending || rejected || entry.at_ns < now {
            continue;
        }
        let action = action_name(&entry.action);
        activity.upcoming.push(Item {
            key: Key::Input(entry.id.clone()),
            lane: lane_for(None, &entry.action, &action, world_ref),
            target: fault_target(&entry.action),
            handle: handle_of(&entry.action),
            action,
            start_ns: entry.at_ns,
            end_ns: None,
            closed_by: None,
            state: State::Upcoming,
        });
    }

    for (index, transition) in transitions.iter().enumerate() {
        let (action, recovers): (String, &[Change]) = match &transition.change {
            Change::Crashed => ("crashed".into(), &[Change::Rebooted, Change::Ready]),
            Change::Exited(code) => (format!("exited {code}"), &[Change::Rebooted, Change::Ready]),
            Change::Paused => ("paused".into(), &[Change::Resumed]),
            _ => continue,
        };
        let recovery = transitions[index + 1..].iter().find(|later| {
            later.machine == transition.machine
                && recovers.iter().any(|change| change == &later.change)
        });
        let item = Item {
            key: Key::Transition(index),
            lane: Lane::Machine(transition.machine.clone()),
            action,
            target: transition.machine.clone(),
            handle: None,
            start_ns: transition.at_ns,
            end_ns: recovery.map(|r| r.at_ns),
            closed_by: recovery.map(|r| match r.change {
                Change::Rebooted => "reboot".into(),
                Change::Resumed => "resume".into(),
                _ => "ready".into(),
            }),
            state: State::Down,
        };
        if item.end_ns.is_none() {
            activity.active.push(item);
        } else {
            activity.history.push(item);
        }
    }

    // A one-shot fault that caused an observed transition is shown once, as
    // the transition it caused.
    let caused: Vec<(Lane, u64)> = activity
        .active
        .iter()
        .chain(activity.history.iter())
        .filter(|item| item.state == State::Down)
        .map(|item| (item.lane.clone(), item.start_ns))
        .collect();
    activity.history.retain(|item| {
        item.state != State::Instant
            || !caused
                .iter()
                .any(|(lane, at)| *lane == item.lane && at.abs_diff(item.start_ns) <= 10_000_000)
    });
    activity.active.sort_by_key(|item| item.start_ns);
    activity.upcoming.sort_by_key(|item| item.start_ns);
    activity.history.sort_by(|a, b| {
        b.end_ns
            .unwrap_or(b.start_ns)
            .cmp(&a.end_ns.unwrap_or(a.start_ns))
    });
    activity
}

#[cfg(test)]
mod tests {
    use super::*;
    use seedbox_api::FaultState;

    fn world(faults: Vec<FaultState>) -> WorldState {
        WorldState {
            override_rejections: Default::default(),
            simulation: "k7m2p9qx".parse().unwrap(),
            revision: 1,
            simulated_time_ns: 0,
            machines: Vec::new(),
            links: Vec::new(),
            faults,
        }
    }

    fn fault(id: u32, action: &str, plan: serde_json::Value, at: u64, active: bool) -> FaultState {
        FaultState {
            id,
            input_key: None,
            action: action.into(),
            machine: plan["machine"].as_str().map(str::to_owned),
            status: FaultStatus::Applied,
            applied_time_ns: Some(at),
            plan: Some(plan),
            active,
        }
    }

    #[test]
    fn heal_closes_the_span_of_its_handle_and_is_not_listed_itself() {
        let w = world(vec![
            fault(
                1,
                "partition",
                serde_json::json!({"action":"partition","handle":"p1","left":["a"],"right":["b"]}),
                10,
                false,
            ),
            fault(
                2,
                "heal",
                serde_json::json!({"action":"heal","handle":"p1"}),
                30,
                false,
            ),
            fault(
                3,
                "partition",
                serde_json::json!({"action":"partition","handle":"p2","left":["a"],"right":["c"]}),
                40,
                true,
            ),
        ]);
        let activity = derive(Some(&w), &[], &[], 50);
        assert_eq!(activity.active.len(), 1);
        assert_eq!(activity.active[0].key, Key::Fault(3));
        assert!(activity.active[0].open());
        assert_eq!(activity.history.len(), 1);
        let closed = &activity.history[0];
        assert_eq!((closed.start_ns, closed.end_ns), (10, Some(30)));
        assert_eq!(closed.closed_by.as_deref(), Some("heal"));
        assert_eq!(closed.lane, Lane::Handle("p1".into()));
        assert_eq!(closed.target, "p1 a┃b");
    }

    #[test]
    fn unknown_one_shot_actions_are_instants_on_their_machine_lane() {
        let w = world(vec![fault(
            1,
            "future_kick",
            serde_json::json!({"action":"future_kick","machine":"n2","novel":7}),
            5,
            false,
        )]);
        let activity = derive(Some(&w), &[], &[], 10);
        assert_eq!(activity.history[0].state, State::Instant);
        assert_eq!(activity.history[0].lane, Lane::Machine("n2".into()));
    }

    #[test]
    fn timeline_inputs_are_upcoming_until_observed_and_skip_rejections() {
        let mut w = world(vec![]);
        w.override_rejections
            .insert("bad".into(), "handle is not active".into());
        let entry = |id: &str, at| TimelineEntry {
            id: id.into(),
            at_ns: at,
            action: serde_json::json!({"action":"heal","handle":"p1"}),
            origin: seedbox_api::TimelineOrigin::Program,
            position: None,
            fault_id: None,
        };
        let activity = derive(
            Some(&w),
            &[
                entry("program:good", 100),
                entry("program:bad", 120),
                entry("program:past", 5),
            ],
            &[],
            50,
        );
        assert_eq!(activity.upcoming.len(), 1);
        assert_eq!(activity.upcoming[0].key, Key::Input("program:good".into()));
        assert_eq!(activity.next(50).unwrap().start_ns, 100);
    }

    #[test]
    fn machine_downtime_opens_and_closes_from_observed_transitions() {
        let transitions = vec![
            Transition {
                at_ns: 10,
                machine: "n3".into(),
                incarnation: 0,
                change: Change::Crashed,
            },
            Transition {
                at_ns: 40,
                machine: "n3".into(),
                incarnation: 1,
                change: Change::Rebooted,
            },
            Transition {
                at_ns: 41,
                machine: "n3".into(),
                incarnation: 1,
                change: Change::Exited(1),
            },
        ];
        let activity = derive(None, &[], &transitions, 60);
        assert_eq!(activity.history.len(), 1);
        assert_eq!(activity.history[0].end_ns, Some(40));
        assert_eq!(activity.active.len(), 1);
        assert_eq!(activity.active[0].action, "exited 1");
        let lanes = activity.lanes(None);
        assert_eq!(lanes, [Lane::Machine("n3".into())]);
    }

    #[test]
    fn departed_machines_keep_their_lane_after_present_ones() {
        let mut w = world(vec![
            fault(
                1,
                "stop",
                serde_json::json!({"action":"stop","machine":"gone"}),
                1,
                false,
            ),
            fault(
                2,
                "stop",
                serde_json::json!({"action":"stop","machine":"here"}),
                2,
                false,
            ),
        ]);
        w.machines.push(seedbox_api::MachineState {
            name: "here".into(),
            incarnation: 0,
            condition: seedbox_api::MachineCondition::Ready,
            exit_code: None,
            throttle_speed_hz: None,
            oracle: false,
            last_transition: None,
            last_transition_at_ns: None,
            condition_since_ns: None,
            application: Default::default(),
        });
        let activity = derive(Some(&w), &[], &[], 5);
        assert_eq!(
            activity.lanes(Some(&w)),
            [Lane::Machine("here".into()), Lane::Machine("gone".into())]
        );
    }
}
