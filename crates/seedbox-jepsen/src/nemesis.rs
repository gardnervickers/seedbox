//! Fault schedules built from the World's faults, every choice drawn from the
//! oracle's seeded [`Rng`] and every applied fault recorded as `fault`
//! evidence. A schedule waits through a [`Pace`]: the World's simulated sleep,
//! or an oracle's serve-while-waiting loop.
use crate::kit::{evidence, now, Ctl, Result, Rng};
use seedbox_control_client::Partition;
use serde_json::{json, Value};
use std::time::Instant;

/// How a schedule waits.
pub trait Pace {
    fn wait(&mut self, ctl: &mut Ctl, seconds: f64);
}

/// Wait with the World's `sleep` RPC (wall time natively).
pub struct Sleep;

impl Pace for Sleep {
    fn wait(&mut self, ctl: &mut Ctl, seconds: f64) {
        if seconds > 0.0 && ctl.sleep(seconds).is_err() {
            crate::kit::pause(seconds);
        }
    }
}

/// Apply one action to each target in order, then record it.
pub fn step(ctl: &mut Ctl, action: &str, targets: &[&str]) -> Result<()> {
    step_with(ctl, action, targets, json!({}))
}

/// [`step`], with extra evidence fields.
pub fn step_with(ctl: &mut Ctl, action: &str, targets: &[&str], extra: Value) -> Result<()> {
    for target in targets {
        match action {
            "stop" => ctl.stop(target)?,
            "restart" => ctl.restart(target)?,
            "pause_process" => ctl.pause_process(target)?,
            "resume_process" => ctl.resume_process(target)?,
            "pause" => ctl.pause(target)?,
            "resume" => ctl.resume(target)?,
            "crash" => ctl.crash(target)?,
            "reboot" => ctl.reboot(target)?,
            other => crate::bail!("unknown fault action {other}"),
        }
    }
    let mut fields = extra.as_object().cloned().unwrap_or_default();
    fields.insert("action".into(), action.into());
    fields.insert("targets".into(), json!(targets));
    evidence("fault", Value::Object(fields));
    Ok(())
}

/// Cut `left` from `right` and record it.
pub fn cut(
    ctl: &mut Ctl,
    left: &[&str],
    right: &[&str],
    extra: Value,
) -> Result<crate::kit::world::Cut> {
    let cut = ctl.partition(left, right)?;
    let mut fields = extra.as_object().cloned().unwrap_or_default();
    fields.insert("action".into(), "partition".into());
    fields.insert("targets".into(), json!([left, right]));
    evidence("fault", Value::Object(fields));
    Ok(cut)
}

/// Heal a cut and record it.
pub fn heal(ctl: &mut Ctl, cut: &crate::kit::world::Cut) -> Result<()> {
    ctl.heal(cut)?;
    evidence(
        "fault",
        json!({"action": "heal", "targets": [cut.left, cut.right]}),
    );
    Ok(())
}

/// Jepsen's kill-nemesis target sizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Subset {
    One,
    Minority,
    Majority,
    All,
}

impl Subset {
    pub fn name(self) -> &'static str {
        match self {
            Self::One => "one",
            Self::Minority => "minority",
            Self::Majority => "majority",
            Self::All => "all",
        }
    }

    pub fn count(self, nodes: usize) -> usize {
        match self {
            Self::One => 1,
            Self::Minority => nodes.saturating_sub(1) / 2,
            Self::Majority => nodes / 2 + 1,
            Self::All => nodes,
        }
        .clamp(1, nodes.max(1))
    }

    pub const ALL: [Subset; 4] = [Self::One, Self::Minority, Self::Majority, Self::All];
}

/// A random subset of the given size class, sorted.
pub fn pick<'a>(rng: &mut Rng, nodes: &[&'a str], subset: Subset) -> Vec<&'a str> {
    let mut chosen = rng.sample(nodes, subset.count(nodes.len()));
    chosen.sort();
    chosen
}

/// A random minority and the rest of the nodes.
pub fn minority_split<'a>(rng: &mut Rng, nodes: &[&'a str]) -> (Vec<&'a str>, Vec<&'a str>) {
    let minority = pick(rng, nodes, Subset::Minority);
    let rest = nodes
        .iter()
        .copied()
        .filter(|n| !minority.contains(n))
        .collect();
    (minority, rest)
}

/// Random halves: a shuffled split as even as possible.
pub fn random_halves<'a>(rng: &mut Rng, nodes: &[&'a str]) -> (Vec<&'a str>, Vec<&'a str>) {
    let mut shuffled = nodes.to_vec();
    rng.shuffle(&mut shuffled);
    let (mut left, mut right) = (
        shuffled[..nodes.len() / 2].to_vec(),
        shuffled[nodes.len() / 2..].to_vec(),
    );
    left.sort();
    right.sort();
    (left, right)
}

/// `node` against every other node.
pub fn isolate<'a>(node: &'a str, nodes: &[&'a str]) -> (Vec<&'a str>, Vec<&'a str>) {
    (
        vec![node],
        nodes.iter().copied().filter(|n| *n != node).collect(),
    )
}

/// Which partition a [`partitions`] round makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Split {
    Minority,
    Halves,
    /// Isolate the node the leader probe names (or a random one without a leader).
    Leader,
}

/// Jepsen's kill nemesis: every round SIGKILLs a random one, minority,
/// majority or all of `nodes` (Seedbox `stop`), waits `down` seconds,
/// restarts them, then waits `up` seconds; rounds run for `hold` seconds.
/// `before` runs before each round, for evidence such as a leader probe.
#[allow(clippy::too_many_arguments)]
pub fn kill_restart(
    ctl: &mut Ctl,
    pace: &mut impl Pace,
    rng: &mut Rng,
    nodes: &[&str],
    hold: f64,
    down: (f64, f64),
    up: (f64, f64),
    mut before: impl FnMut(),
) -> Result<u64> {
    let end = Instant::now() + std::time::Duration::from_secs_f64(hold.max(0.0));
    let mut rounds = 0;
    while Instant::now() < end {
        rng.refresh();
        let subset = *rng.choice(&Subset::ALL);
        let victims = pick(rng, nodes, subset);
        before();
        step_with(ctl, "stop", &victims, json!({"subset": subset.name()}))?;
        pace.wait(ctl, rng.seconds(down.0, down.1));
        step(ctl, "restart", &victims)?;
        rng.refresh();
        pace.wait(ctl, rng.seconds(up.0, up.1));
        rounds += 1;
    }
    Ok(rounds)
}

/// Pause a random node's process for `length` seconds, then `gap` seconds of
/// quiet, until `hold` seconds pass (Jepsen's pause nemesis).
#[allow(clippy::too_many_arguments)]
pub fn random_pauses(
    ctl: &mut Ctl,
    pace: &mut impl Pace,
    rng: &mut Rng,
    nodes: &[&str],
    hold: f64,
    length: f64,
    gap: f64,
    mut context: impl FnMut(&str) -> Value,
) -> Result<u64> {
    let end = Instant::now() + std::time::Duration::from_secs_f64(hold.max(0.0));
    let mut count = 0;
    while Instant::now() < end {
        rng.refresh();
        let target = *rng.choice(nodes);
        step_with(ctl, "pause_process", &[target], context(target))?;
        pace.wait(ctl, length);
        step(ctl, "resume_process", &[target])?;
        count += 1;
        pace.wait(ctl, gap);
    }
    Ok(count)
}

/// Partition rounds: a [`Split`] for `length` seconds, then `gap` seconds healed.
#[allow(clippy::too_many_arguments)]
pub fn partitions(
    ctl: &mut Ctl,
    pace: &mut impl Pace,
    rng: &mut Rng,
    nodes: &[&str],
    split: Split,
    hold: f64,
    length: (f64, f64),
    gap: (f64, f64),
    mut leader: impl FnMut() -> Option<String>,
) -> Result<u64> {
    let end = Instant::now() + std::time::Duration::from_secs_f64(hold.max(0.0));
    let mut count = 0;
    while Instant::now() < end {
        rng.refresh();
        let leader_name = if split == Split::Leader {
            leader()
        } else {
            None
        };
        let (left, right) = match split {
            Split::Minority => minority_split(rng, nodes),
            Split::Halves => random_halves(rng, nodes),
            Split::Leader => {
                let node = leader_name
                    .as_deref()
                    .and_then(|l| nodes.iter().copied().find(|n| *n == l))
                    .unwrap_or_else(|| *rng.choice(nodes));
                isolate(node, nodes)
            }
        };
        let handle = cut(ctl, &left, &right, json!({"leader": leader_name}))?;
        pace.wait(ctl, rng.seconds(length.0, length.1));
        heal(ctl, &handle)?;
        count += 1;
        rng.refresh();
        pace.wait(ctl, rng.seconds(gap.0, gap.1));
    }
    Ok(count)
}

/// Power-fail every node at once, wait `down` seconds, then reboot them all.
/// A refused crash (say, of a node another fault already has down) still
/// waits and reboots every node this call crashed, then returns the error.
pub fn crash_all(
    ctl: &mut Ctl,
    pace: &mut impl Pace,
    nodes: &[&str],
    down: f64,
    extra: Value,
) -> Result<()> {
    let mut crashed = Vec::new();
    let mut failure = None;
    for node in nodes {
        match ctl.crash(node) {
            Ok(()) => crashed.push(*node),
            Err(error) => {
                failure = Some(crate::kit::Error(format!("crash {node}: {}", error.0)));
                break;
            }
        }
    }
    let mut fields = extra.as_object().cloned().unwrap_or_default();
    fields.insert("action".into(), "crash".into());
    fields.insert("targets".into(), json!(crashed));
    evidence("fault", Value::Object(fields));
    if crashed.is_empty() {
        return Err(failure.expect("no nodes crashed without an error"));
    }
    pace.wait(ctl, down);
    let mut rebooted = Vec::new();
    for node in &crashed {
        match ctl.reboot(node) {
            Ok(()) => rebooted.push(*node),
            Err(error) => {
                failure.get_or_insert(crate::kit::Error(format!("reboot {node}: {}", error.0)));
            }
        }
    }
    evidence("fault", json!({"action": "reboot", "targets": rebooted}));
    failure.map_or(Ok(()), Err)
}

/// Power-fail one node and reboot it after `down` seconds.
pub fn crash_one(ctl: &mut Ctl, pace: &mut impl Pace, node: &str, down: f64) -> Result<()> {
    step(ctl, "crash", &[node])?;
    pace.wait(ctl, down);
    step(ctl, "reboot", &[node])
}

/// A partition a console trigger applied, once it has fired.
pub struct Fired {
    pub partition: Partition,
    pub status: Value,
}

/// How a console trigger ended without firing.
pub enum NotFired {
    /// The window ended first, and the trigger was cancelled.
    Cancelled,
    /// Its final status (expired, rejected, cut_off, ...).
    Other(String),
}

/// Arm a partition on `source`'s console `marker`, wait (through `pace`)
/// until it fires or `until` passes, and cancel it if it never matched.
/// A matched trigger applies its partition shortly afterwards, so a match
/// is always waited out.
#[allow(clippy::too_many_arguments)]
pub fn console_partition(
    ctl: &mut Ctl,
    pace: &mut impl Pace,
    source: &str,
    marker: &[u8],
    left: &[&str],
    right: &[&str],
    until: Instant,
    poll: f64,
) -> Result<std::result::Result<Fired, NotFired>> {
    let window = until
        .saturating_duration_since(Instant::now())
        .as_secs_f64()
        .max(1.0);
    let window = (window * 1000.0).round() / 1000.0;
    let trigger = ctl.arm_console_partition(source, marker, left, right, window)?;
    let state = |ctl: &mut Ctl| -> Result<(String, Value)> {
        let status = ctl.trigger_status(&trigger)?;
        Ok((status["status"].as_str().unwrap_or("").to_string(), status))
    };
    let (mut name, mut status) = state(ctl)?;
    while name == "armed" && Instant::now() < until {
        pace.wait(ctl, poll);
        (name, status) = state(ctl)?;
    }
    if name == "armed" {
        // It may match between the status read and the cancel.
        if ctl.cancel_trigger(&trigger).is_ok() {
            return Ok(Err(NotFired::Cancelled));
        }
        (name, status) = state(ctl)?;
    }
    while name == "matched" {
        pace.wait(ctl, 0.01);
        (name, status) = state(ctl)?;
    }
    if name != "fired" {
        return Ok(Err(NotFired::Other(name)));
    }
    let partition = trigger
        .partition
        .clone()
        .ok_or("partition trigger without a handle")?;
    Ok(Ok(Fired { partition, status }))
}

/// Seconds since the oracle started, for evidence.
pub fn at() -> f64 {
    now()
}

/// A background schedule of coordinated power failures: about every `every`
/// seconds (half to one and a half times it, from a seeded RNG), power-fail
/// every node of `nodes` together and reboot them all after `down` seconds.
/// With `startup_partition` above zero, 0.05-0.4 s after each reboot one
/// random node is cut off from the others for that many seconds while they
/// start up. No failure starts that would not finish `end` seconds after
/// this call. Returns at once; the schedule runs on its own thread with its
/// own World connection.
pub fn crash_all_repeatedly(
    nodes: Vec<String>,
    every: f64,
    down: f64,
    startup_partition: f64,
    end: f64,
) {
    let mut ctl = match crate::kit::connect_world(None) {
        Ok(ctl) => ctl,
        Err(error) => {
            evidence("nemesis-unavailable", json!({"error": error.0}));
            return;
        }
    };
    let start = Instant::now();
    std::thread::spawn(move || {
        let nodes: Vec<&str> = nodes.iter().map(String::as_str).collect();
        let mut pace = Sleep;
        let mut rng = Rng::new(crate::kit::rng::entropy_seed());
        loop {
            rng.refresh();
            let gap = rng.uniform(0.5, 1.5) * every;
            let tail = down + startup_partition + 3.0;
            if start.elapsed().as_secs_f64() + gap + tail > end {
                return;
            }
            pace.wait(&mut ctl, gap);
            if let Err(error) = crash_all(&mut ctl, &mut pace, &nodes, down, json!({})) {
                evidence("nemesis-failed", json!({"error": error.0}));
            }
            if startup_partition > 0.0 {
                rng.refresh();
                pace.wait(&mut ctl, rng.uniform(0.05, 0.4));
                let (left, right) = isolate(rng.choice(&nodes), &nodes);
                match cut(&mut ctl, &left, &right, json!({"startup": true})) {
                    Ok(cut) => {
                        pace.wait(&mut ctl, startup_partition);
                        if let Err(error) = heal(&mut ctl, &cut) {
                            evidence("nemesis-failed", json!({"error": error.0}));
                        }
                    }
                    Err(error) => evidence("nemesis-failed", json!({"error": error.0})),
                }
            }
        }
    });
}
