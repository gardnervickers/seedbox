use super::*;
use seedbox_api::{SeedRangeRequest, SeedRangeStatus, SimulationSummary};

#[derive(Clone)]
pub(super) struct Range {
    pub(super) count: u32,
    next: u32,
    deleted: BTreeSet<u32>,
    pub(super) stop_requested: bool,
    error: Option<String>,
}

impl Catalogue {
    pub fn submit_seeds(&mut self, mut request: SeedRangeRequest) -> Result<(Simulation, bool)> {
        validate_submission_request_id(&request.request_id)?;
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
                options: request.options,
                metadata: request.metadata,
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
        self.entries.insert(id.clone(), entry);
        Ok(self
            .seed_range(id)?
            .members
            .into_iter()
            .map(|m| m.metadata.id)
            .collect())
    }

    /// Admit at most one member per call, so the owner can reserve queue capacity
    /// before accepting work. Each member has a deterministic request ID.
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
        if !matches!(
            entry.resource.spec.inputs,
            InputSource::Resolved { .. } | InputSource::Retained { .. }
        ) {
            range.error = Some("source simulation did not retain prepared inputs".into());
            self.entries.insert(id, entry);
            return Ok(None);
        }
        let index = range.next;
        let mut spec = entry.resource.spec.clone();
        spec.seed = Some(spec.seed.ok_or(Error::StorageFailed)? + u64::from(index));

        if let InputSource::Retained { simulation } = &spec.inputs {
            spec.inputs = InputSource::Prepared {
                simulation: simulation.clone(),
            };
        }
        let member = match self.submit_inner(
            SubmitRequest {
                request_id: member_request_id(&id, index),
                options: entry.resource.options.clone(),
                metadata: seedbox_spec::definition::Metadata {
                    name: entry.resource.metadata.name.clone(),
                    labels: entry.resource.metadata.labels.clone(),
                    annotations: entry.resource.metadata.annotations.clone(),
                },
                spec,
            },
            None,
        ) {
            Ok(member) => member,
            Err(error) => {
                range.error = Some(error.to_string());
                self.entries.insert(id, entry);
                return Ok(None);
            }
        };
        range.next += 1;
        self.entries.insert(id, entry);
        Ok(Some(member))
    }
}
fn member_request_id(id: &SimulationId, index: u32) -> String {
    format!("seeds-{id}-{index}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use seedbox_api::SimulationSpec;
    use std::path::Path;

    fn request(name: &str) -> SeedRangeRequest {
        SeedRangeRequest {
            metadata: Default::default(),
            options: Default::default(),
            request_id: name.into(),
            count: 3,
            spec: SimulationSpec {
                program: Vec::new(),
                start_paused: false,
                seed: Some(123),
                inputs: InputSource::Project,
                configuration: seedbox_application::config::parse(
                    "name: test\nmachines: {}\noracle:\n  image: example/oracle\n",
                    Path::new("/project/seedbox.yaml"),
                    &Default::default(),
                )
                .unwrap(),
            },
        }
    }

    fn complete(catalogue: &mut Catalogue, mut source: Simulation) -> Simulation {
        source.status.phase = Phase::Preparing;
        source = catalogue.update(source).unwrap();
        source.spec.inputs = InputSource::Retained {
            simulation: source.metadata.id.clone(),
        };
        source.status.phase = Phase::Completed;
        catalogue.update(source).unwrap()
    }

    #[test]
    fn member_namespace_is_reserved_and_range_failure_is_isolated() {
        let mut catalogue = Catalogue::in_memory();
        let (source, _) = catalogue.submit_seeds(request("broken-range")).unwrap();
        let id = source.metadata.id.clone();
        let collision = SubmitRequest {
            metadata: Default::default(),
            options: Default::default(),
            request_id: member_request_id(&id, 1),
            spec: request("unused").spec,
        };
        assert!(matches!(
            catalogue.submit(collision.clone()),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            catalogue.submit_seeds(SeedRangeRequest {
                metadata: Default::default(),
                options: Default::default(),
                request_id: collision.request_id.clone(),
                spec: collision.spec.clone(),
                count: 3,
            }),
            Err(Error::Invalid(_))
        ));
        // Model a conflicting retained entry from before namespace reservation.
        catalogue.submit_inner(collision, None).unwrap();
        complete(&mut catalogue, source);
        assert!(catalogue.advance_seeds().unwrap().is_none());
        assert!(catalogue
            .seed_range(&id)
            .unwrap()
            .error
            .unwrap()
            .contains("request ID"));
        let peer = catalogue.submit_seeds(request("healthy-range")).unwrap().0;
        let peer = complete(&mut catalogue, peer);
        assert!(catalogue.seed_range(&id).unwrap().error.is_some());
        let (member, _) = catalogue.advance_seeds().unwrap().unwrap();
        assert_eq!(member.metadata.parent, None);
        assert_eq!(member.spec.seed, Some(124));
        assert_eq!(catalogue.seed_range(&id).unwrap().members.len(), 1);
        assert_eq!(
            catalogue
                .seed_range(&peer.metadata.id)
                .unwrap()
                .members
                .len(),
            2
        );
    }

    #[test]
    fn retained_execution_keeps_seed_while_prepared_inputs_allow_exploration() {
        let mut catalogue = Catalogue::in_memory();
        let base = request("source");
        let source = catalogue
            .submit(SubmitRequest {
                metadata: Default::default(),
                options: Default::default(),
                request_id: base.request_id,
                spec: base.spec,
            })
            .unwrap()
            .0;
        let source = complete(&mut catalogue, source);
        let mut rerun = SubmitRequest {
            metadata: Default::default(),
            options: Default::default(),
            request_id: "rerun".into(),
            spec: source.spec.clone(),
        };
        rerun.spec.seed = Some(124);
        assert!(matches!(
            catalogue.submit(rerun.clone()),
            Err(Error::Invalid(_))
        ));
        rerun.spec.inputs = InputSource::Prepared {
            simulation: source.metadata.id.clone(),
        };
        assert_eq!(catalogue.submit(rerun).unwrap().0.spec.seed, Some(124));
        assert!(catalogue
            .submit(SubmitRequest {
                metadata: Default::default(),
                options: Default::default(),
                request_id: "same-seed".into(),
                spec: source.spec,
            })
            .is_ok());
    }
}
