//! A scheduler checkpoint preserves queue keys; rebuilding through `schedule`
//! would assign new source identities and change deterministic continuation.
use super::*;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint<P> {
    version: u32,
    params: Params,
    machines: Vec<MachineState>,
    // Entries instead of JSON object keys, which cannot represent EventKey.
    queues: Vec<Vec<Event<P>>>,
    boundaries: Vec<Time>,
    world_seq: u64,
    committed: Time,
    round: u64,
    outcome: Option<Outcome>,
    log: Vec<RoundRecord>,
    effects: u64,
}

impl<P: Clone + Hash> World<P> {
    pub fn checkpoint(&self) -> Checkpoint<P> {
        Checkpoint {
            version: 1,
            params: self.params.clone(),
            machines: self.machines.clone(),
            queues: self
                .queues
                .iter()
                .map(|queue| {
                    queue
                        .iter()
                        .map(|(key, payload)| Event {
                            key: *key,
                            payload: payload.clone(),
                        })
                        .collect()
                })
                .collect(),
            boundaries: self.boundaries.iter().copied().collect(),
            world_seq: self.world_seq,
            committed: self.committed,
            round: self.round,
            outcome: self.outcome,
            log: self.log.clone(),
            effects: self.effects.0,
        }
    }

    /// Validate persisted structure before making it executable. The containing
    /// world artifact owns byte limits, integrity and machine-state binding.
    pub fn from_checkpoint(saved: Checkpoint<P>) -> Result<Self, Error> {
        let invalid =
            |message: &str| Error::Coordinator(format!("scheduler checkpoint: {message}"));
        if saved.version != 1 {
            return Err(invalid("unsupported version"));
        }
        let mut world = Self::with_machines(saved.machines, saved.params)?;
        if saved.queues.len() != world.machines.len() {
            return Err(invalid("queue count differs from machine count"));
        }
        if saved.log.len() as u64 != saved.round
            || saved.log.iter().enumerate().any(|(index, record)| {
                record.round != index as u64
                    || record.end < record.start
                    || record.active > world.machines.len()
            })
            || saved
                .log
                .last()
                .is_some_and(|record| record.end != saved.committed)
        {
            return Err(invalid("round history disagrees with committed position"));
        }
        if saved.boundaries.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(invalid("boundaries are not strictly ordered"));
        }
        if let Some(Outcome::Exited { machine, .. }) = saved.outcome {
            if machine >= world.machines.len() {
                return Err(invalid("outcome names an unknown machine"));
            }
        }
        for (destination, entries) in saved.queues.into_iter().enumerate() {
            let mut previous = None;
            for Event { key, payload } in entries {
                if previous.is_some_and(|prior| prior >= key) {
                    return Err(invalid("queue keys are not strictly ordered"));
                }
                let limit = match (key.class, key.source) {
                    (Class::World, Source::World) => saved.world_seq,
                    (Class::Frame, Source::Machine(source)) => {
                        world
                            .machines
                            .get(source)
                            .ok_or_else(|| invalid("queue source names an unknown machine"))?
                            .sent
                    }
                    _ => return Err(invalid("queue class and source disagree")),
                };
                if key.seq >= limit {
                    return Err(invalid("queue sequence exceeds its source counter"));
                }
                previous = Some(key);
                world.queues[destination].insert(key, payload);
            }
        }
        world.boundaries = saved.boundaries.into_iter().collect();
        world.world_seq = saved.world_seq;
        world.committed = saved.committed;
        world.round = saved.round;
        world.outcome = saved.outcome;
        world.log = saved.log;
        world.effects = Fnv(saved.effects);
        Ok(world)
    }
}
