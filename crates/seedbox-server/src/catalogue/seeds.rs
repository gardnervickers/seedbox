use super::*;
use seedbox_api::{SeedRangeRequest, SeedRangeStatus, SimulationSummary};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Range {
    pub(super) count: u32,
    next: u32,
    #[serde(default)]
    deleted: BTreeSet<u32>,
    pub(super) stop_requested: bool,
    error: Option<String>,
}

impl Range {
    pub(super) fn validate(&self, seed: Option<u64>) -> Result<()> {
        if !(1..=4096).contains(&self.count)
            || !(1..=self.count).contains(&self.next)
            || self
                .deleted
                .iter()
                .any(|index| *index == 0 || *index >= self.next)
            || seed
                .and_then(|seed| seed.checked_add(u64::from(self.count - 1)))
                .is_none()
        {
            return Err(Error::Invalid("invalid retained seed range".into()));
        }
        Ok(())
    }
}

impl Catalogue {
    pub fn stopped_seed_members(&self) -> Result<Vec<SimulationId>> {
        let mut ids = Vec::new();
        for (id, entry) in &self.entries {
            if entry
                .range
                .as_ref()
                .is_some_and(|range| range.stop_requested)
            {
                ids.extend(
                    self.seed_range(id)?
                        .members
                        .into_iter()
                        .filter(|m| m.status.phase != Phase::Completed)
                        .map(|m| m.metadata.id),
                );
            }
        }
        Ok(ids)
    }

    pub fn submit_seeds(&mut self, mut request: SeedRangeRequest) -> Result<(Simulation, bool)> {
        if !(1..=4096).contains(&request.count) {
            return Err(Error::Invalid("seed count must be in 1..=4096".into()));
        }
        if matches!(request.spec.inputs, InputSource::Checkpoint { .. }) {
            return Err(Error::Invalid(
                "seed exploration requires fresh or retained preparation inputs".into(),
            ));
        }
        if let InputSource::Retained { simulation } = &request.spec.inputs {
            request.spec.inputs = InputSource::Prepared {
                simulation: simulation.clone(),
            };
        }
        if let Some(seed) = request.spec.seed.or(request.spec.configuration.seed) {
            seed.checked_add(u64::from(request.count - 1))
                .ok_or_else(|| Error::Invalid("seed range overflows u64".into()))?;
        }
        self.submit_inner(
            SubmitRequest {
                request_id: request.request_id,
                spec: request.spec,
            },
            Some(Range {
                count: request.count,
                next: 1,
                deleted: BTreeSet::new(),
                stop_requested: false,
                error: None,
            }),
        )
    }

    pub(super) fn remove_seed_member(&mut self, request_id: &str) -> Result<()> {
        let mut updates = Vec::new();
        for (id, entry) in &self.entries {
            if let Some(range) = &entry.range {
                if let Some(index) =
                    (1..range.next).find(|index| member_request_id(id, *index) == request_id)
                {
                    let mut entry = entry.clone();
                    entry
                        .range
                        .as_mut()
                        .expect("range was checked")
                        .deleted
                        .insert(index);
                    updates.push((id.clone(), entry));
                }
            }
        }
        for (id, entry) in updates {
            self.persist(&entry)?;
            self.entries.insert(id, entry);
        }
        Ok(())
    }

    pub fn seed_range(&self, id: &SimulationId) -> Result<SeedRangeStatus> {
        let source = self
            .entries
            .get(id)
            .ok_or_else(|| Error::NotFound(id.clone()))?;
        let range = source
            .range
            .as_ref()
            .ok_or_else(|| Error::Invalid("simulation has no seed range".into()))?;
        let mut members = vec![&source.resource];
        for index in 1..range.next {
            if range.deleted.contains(&index) {
                continue;
            }
            let member = self
                .requests
                .get(&member_request_id(id, index))
                .and_then(|id| self.entries.get(id))
                .ok_or_else(|| Error::Invalid("seed range member is missing".into()))?;
            members.push(&member.resource);
        }
        Ok(SeedRangeStatus {
            source: id.clone(),
            first_seed: source
                .resource
                .spec
                .seed
                .ok_or_else(|| Error::Invalid("seed range has no resolved seed".into()))?,
            count: range.count,
            deleted_members: range.deleted.len() as u32,
            stop_requested: range.stop_requested,
            error: range.error.clone(),
            members: members
                .into_iter()
                .map(|s| SimulationSummary {
                    metadata: s.metadata.clone(),
                    status: s.status.clone(),
                    oracle: s.spec.configuration.check.service.clone(),
                })
                .collect(),
        })
    }

    pub fn stop_seeds(&mut self, id: &SimulationId) -> Result<Vec<SimulationId>> {
        let mut entry = self
            .entries
            .get(id)
            .ok_or_else(|| Error::NotFound(id.clone()))?
            .clone();
        let range = entry
            .range
            .as_mut()
            .ok_or_else(|| Error::Invalid("simulation has no seed range".into()))?;
        range.stop_requested = true;
        self.persist(&entry)?;
        self.entries.insert(id.clone(), entry);
        Ok(self
            .seed_range(id)?
            .members
            .into_iter()
            .map(|m| m.metadata.id)
            .collect())
    }

    /// Admit at most one member per call, so the owner can reserve queue capacity
    /// before accepting work. Each member's deterministic request ID closes the
    /// crash window between accepting it and advancing the range cursor.
    pub fn advance_seeds(&mut self) -> Result<Option<(Simulation, bool)>> {
        let ready = self.entries.iter().find_map(|(id, entry)| {
            let range = entry.range.as_ref()?;
            (!range.stop_requested
                && range.error.is_none()
                && range.next < range.count
                && entry.resource.status.phase == Phase::Completed)
                .then(|| id.clone())
        });
        let Some(id) = ready else {
            return Ok(None);
        };
        let mut entry = self.entries[&id].clone();
        let range = entry.range.as_mut().ok_or(Error::StorageFailed)?;
        if !matches!(entry.resource.spec.inputs, InputSource::Retained { .. }) {
            range.error = Some("source simulation did not retain prepared inputs".into());
            self.persist(&entry)?;
            self.entries.insert(id, entry);
            return Ok(None);
        }
        let index = range.next;
        let mut spec = entry.resource.spec.clone();
        spec.seed = Some(spec.seed.ok_or(Error::StorageFailed)? + u64::from(index));
        spec.inputs = InputSource::Prepared {
            simulation: id.clone(),
        };
        let member = self.submit(SubmitRequest {
            request_id: member_request_id(&id, index),
            spec,
        })?;
        range.next += 1;
        self.persist(&entry)?;
        self.entries.insert(id, entry);
        Ok(Some(member))
    }
}
fn member_request_id(id: &SimulationId, index: u32) -> String {
    format!("seeds-{id}-{index}")
}
