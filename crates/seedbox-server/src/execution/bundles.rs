//! Completed-resource snapshots. Evidence I/O and deletion share a lock; no
//! archive field is ever used to derive a host path or launch work on import.
use super::*;
use futures_util::FutureExt;
use seedbox_api::{ControlCommand, ControlState, EventDelivery, LogDelivery};
use seedbox_artifacts::archive::{self, Options, Snapshot};
use std::io::{Read, Seek};

const LOG_BYTES: u64 = 512 * 1024 * 1024;
const EVENT_BYTES: u64 = 128 * 1024 * 1024;
const MAX_DELIVERIES: usize = 1_000_000;

impl ExecutionService {
    pub fn export_bundle(
        &self,
        id: &SimulationId,
        output: impl Write,
        options: &Options,
    ) -> Result<(), Error> {
        let _guard = self
            .evidence
            .lock()
            .map_err(|_| catalogue::Error::StorageFailed)?;
        let resource = self.manager.describe(id)?;
        if resource.status.phase != Phase::Completed
            || resource.metadata.deletion_requested_at_unix_ms.is_some()
        {
            return Err(Error::Catalogue(catalogue::Error::Invalid(
                "bundle export requires a completed simulation not being deleted".into(),
            )));
        }
        let simulation = self.portable_simulation(id)?;
        if !matches!(simulation.spec.inputs, InputSource::Resolved { .. }) {
            return Err(Error::Bundle(
                "simulation has no resolved inputs to export".into(),
            ));
        }
        let mut omissions = BTreeMap::from([
            (
                "frames".into(),
                "frame history is not retained by the server".into(),
            ),
            (
                "observations".into(),
                "bounded server history; delivery/source gaps are preserved".into(),
            ),
        ]);
        if !self.manager.observation_history_available(id)? {
            omissions.insert(
                "logs".into(),
                "server observation history unavailable".into(),
            );
            omissions.insert(
                "events".into(),
                "server observation history unavailable".into(),
            );
        }
        let record = if self.manager.read(|catalogue| catalogue.is_imported(id))? {
            let previous = self.imported_snapshot(id)?;
            omissions.extend(previous.omissions);
            previous.record
        } else {
            match fs::File::open(self.root.join("inputs").join(format!("{id}.json"))) {
                Ok(file) => {
                    let inputs: RetainedInputs =
                        serde_json::from_reader(file).map_err(seedbox_application::Error::from)?;
                    match inputs.attempt {
                        Some(attempt) => Some(seedbox_artifacts::record::inspect(&attempt)?),
                        None => {
                            omissions.insert(
                                "record".into(),
                                "execution did not create an attempt".into(),
                            );
                            None
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    omissions.insert("record".into(), "execution inputs unavailable".into());
                    None
                }
                Err(e) => return Err(e.into()),
            }
        };
        if let Some(record) = &record {
            omissions.extend(
                record
                    .omitted_logs
                    .iter()
                    .map(|(name, reason)| (format!("diagnostic:{name}"), reason.clone())),
            );
        }
        let controls = self.manager.read(|catalogue| catalogue.checkpoints(id))?;
        let mut checkpoints = BTreeMap::new();
        for receipt in &controls {
            if let ControlState::Acknowledged {
                error: None,
                checkpoint: Some(capture),
                ..
            } = &receipt.state
            {
                if capture.state == seedbox_api::CheckpointState::Captured {
                    let resource = self.checkpoint_definition(id, &receipt.request.request_id)?;
                    if let InputSource::Resolved { definition } = resource.spec.inputs {
                        checkpoints.insert(receipt.request.request_id.clone(), *definition);
                    }
                } else {
                    omissions.insert(
                        format!("checkpoint:{}", receipt.request.request_id),
                        "capture unavailable".into(),
                    );
                }
            } else {
                omissions.insert(
                    format!("checkpoint:{}", receipt.request.request_id),
                    "capture unavailable".into(),
                );
            }
        }
        let logs = snapshot_logs(self.manager.all_logs(id, false)?)?;
        let events = snapshot_events(self.manager.events(id, false)?)?;
        let mut observations = BTreeMap::new();
        observations.insert("logs".into(), put_json(&self.store, &logs)?);
        observations.insert("events".into(), put_json(&self.store, &events)?);
        for machine in simulation.spec.configuration.services.keys() {
            let key = format!("logs:{machine}");
            let logs = snapshot_logs(self.manager.logs(id, machine, false)?)?;
            observations.insert(key.clone(), put_json(&self.store, &logs)?);
            if !self.manager.observation_history_available(id)? {
                omissions.insert(key, "server machine log history unavailable".into());
            }
        }
        let snapshot = Snapshot {
            simulation,
            checkpoints,
            record,
            controls,
            world: self.manager.world(id)?,
            observations,
            omissions,
        };
        archive::export(&snapshot, &self.store, output, options)
            .map_err(|e| Error::Bundle(e.to_string()))
    }

    /// All validation and owned evidence preparation precede catalogue
    /// publication. Failure can leave verified cache assets, never a resource.
    pub fn import_bundle(
        &self,
        input: impl Read,
        options: &Options,
    ) -> Result<seedbox_api::Simulation, Error> {
        let _guard = self
            .evidence
            .lock()
            .map_err(|_| catalogue::Error::StorageFailed)?;
        let snapshot = archive::import(input, &self.store, options)
            .map_err(|e| Error::Bundle(e.to_string()))?;
        validate_snapshot(&snapshot)?;
        let InputSource::Resolved { definition } = &snapshot.simulation.spec.inputs else {
            unreachable!("archive validation")
        };
        // This resolves retained binaries and launch settings without asking a
        // BackendProvider for host admission, worker execution or Docker.
        seedbox_application::definition::prepared(definition, &self.store)
            .map_err(|e| Error::Bundle(e.to_string()))?;
        for definition in snapshot.checkpoints.values() {
            if options.cancellation.is_cancelled() {
                return Err(Error::Bundle("transfer cancelled".into()));
            }
            seedbox_application::definition::prepared(definition, &self.store)
                .map_err(|e| Error::Bundle(e.to_string()))?;
        }
        let mut log_bytes = 0u64;
        for (key, digest) in &snapshot.observations {
            if key == "logs" || key.starts_with("logs:") {
                log_bytes = log_bytes
                    .checked_add(self.store.size(digest)?)
                    .ok_or_else(|| Error::Bundle("log metadata size overflow".into()))?;
                if log_bytes > LOG_BYTES {
                    return Err(Error::Bundle(
                        "combined log snapshots exceed metadata limit".into(),
                    ));
                }
            }
        }
        let logs: Vec<LogDelivery> = read_observations(&snapshot, "logs", &self.store, LOG_BYTES)?;
        let events: Vec<EventDelivery> =
            read_observations(&snapshot, "events", &self.store, EVENT_BYTES)?;
        validate_deliveries(&logs, &events, &snapshot)?;
        let mut machine_logs = BTreeMap::new();
        for machine in snapshot.simulation.spec.configuration.services.keys() {
            let key = format!("logs:{machine}");
            if snapshot.observations.contains_key(&key) {
                let deliveries = read_observations(&snapshot, &key, &self.store, LOG_BYTES)?;
                validate_deliveries(&deliveries, &[EventDelivery::Closed], &snapshot)?;
                for delivery in &deliveries {
                    if let LogDelivery::Entry {
                        entry:
                            seedbox_api::LogEntry::Bytes {
                                machine: attributed,
                                ..
                            }
                            | seedbox_api::LogEntry::MachineGap {
                                machine: attributed,
                                ..
                            },
                        ..
                    } = delivery
                    {
                        if attributed != machine {
                            return Err(Error::Bundle(
                                "machine log snapshot has wrong attribution".into(),
                            ));
                        }
                    }
                }
                machine_logs.insert(machine.clone(), deliveries);
            } else if !snapshot
                .omissions
                .get(&key)
                .is_some_and(|reason| !reason.is_empty())
            {
                return Err(Error::Bundle(format!(
                    "missing {key} snapshot requires an explicit omission reason"
                )));
            }
        }
        if options.cancellation.is_cancelled() {
            return Err(Error::Bundle("transfer cancelled".into()));
        }
        let mut published_path = None;
        let result =
            self.manager
                .import_bundle(&snapshot, logs, machine_logs, events, |resource| {
                    if options.cancellation.is_cancelled() {
                        return Err(catalogue::Error::Invalid("transfer cancelled".into()));
                    }
                    let path = self.root.join("runs").join(resource.metadata.id.as_str());
                    fs::create_dir(&path).map_err(|source| catalogue::Error::Io {
                        path: path.clone(),
                        source,
                    })?;
                    published_path = Some(path.clone());
                    let mut owned = snapshot.clone();
                    owned.simulation = resource.clone();
                    owned.world.simulation = resource.metadata.id.clone();
                    fs::write(
                        path.join("import.json"),
                        serde_json::to_vec(&owned)
                            .map_err(|e| catalogue::Error::Invalid(e.to_string()))?,
                    )
                    .map_err(|source| catalogue::Error::Io { path, source })?;
                    Ok(())
                });
        if result.is_err() {
            if let Some(path) = published_path {
                fs::remove_dir_all(path)?;
            }
        }
        result.map_err(Error::Catalogue)
    }

    pub(super) fn portable_simulation(
        &self,
        id: &SimulationId,
    ) -> seedbox_application::Result<seedbox_api::Simulation> {
        let simulation = self
            .describe(id)
            .map_err(|e| seedbox_application::Error::Setup(e.to_string()))?;
        let mut simulation: seedbox_api::Simulation =
            serde_json::from_value(serde_json::to_value(simulation)?)?;
        if self
            .manager
            .read(|catalogue| catalogue.is_imported(id))
            .map_err(|e| seedbox_application::Error::Setup(e.to_string()))?
        {
            return Ok(simulation);
        }
        let path = self.root.join("inputs").join(format!("{id}.json"));
        match fs::File::open(path) {
            Ok(file) => {
                let inputs: RetainedInputs = serde_json::from_reader(file)?;
                if let (Some(path), InputSource::Resolved { definition }) =
                    (&inputs.checkpoint, &mut simulation.spec.inputs)
                {
                    definition.start = seedbox_application::definition::capture_start(
                        path,
                        &self.root.join("runs"),
                        &self.store,
                    )?;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(simulation)
    }

    pub(super) fn imported_snapshot(&self, id: &SimulationId) -> Result<Snapshot, Error> {
        let file = fs::File::open(self.root.join("runs").join(id.as_str()).join("import.json"))?;
        serde_json::from_reader(file).map_err(|e| Error::Bundle(e.to_string()))
    }
}

fn put_json(store: &ContentStore, value: &impl serde::Serialize) -> Result<ContentDigest, Error> {
    let mut file = tempfile::tempfile()?;
    serde_json::to_writer(&mut file, value).map_err(seedbox_application::Error::from)?;
    file.rewind()?;
    Ok(store.put_reader_unsynced(file)?)
}
fn read_observations<T: serde::de::DeserializeOwned>(
    snapshot: &Snapshot,
    key: &str,
    store: &ContentStore,
    limit: u64,
) -> Result<Vec<T>, Error> {
    let Some(digest) = snapshot.observations.get(key) else {
        return Err(Error::Bundle(format!("missing {key} delivery snapshot")));
    };
    if store.size(digest)? > limit {
        return Err(Error::Bundle(format!(
            "{key} snapshot exceeds metadata limit"
        )));
    }
    let mut file = tempfile::tempfile()?;
    store.copy_to(digest, &mut file)?;
    file.rewind()?;
    struct Rows<T>(Vec<T>);
    impl<'de, T: serde::Deserialize<'de>> serde::Deserialize<'de> for Rows<T> {
        fn deserialize<D: serde::Deserializer<'de>>(
            deserializer: D,
        ) -> std::result::Result<Self, D::Error> {
            struct Visitor<T>(std::marker::PhantomData<T>);
            impl<'de, T: serde::Deserialize<'de>> serde::de::Visitor<'de> for Visitor<T> {
                type Value = Rows<T>;
                fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    formatter.write_str("a bounded observation delivery array")
                }
                fn visit_seq<A: serde::de::SeqAccess<'de>>(
                    self,
                    mut sequence: A,
                ) -> std::result::Result<Rows<T>, A::Error> {
                    let mut rows = Vec::new();
                    while let Some(row) = sequence.next_element()? {
                        if rows.len() == MAX_DELIVERIES {
                            return Err(serde::de::Error::custom(
                                "observation count exceeds limit",
                            ));
                        }
                        rows.push(row);
                    }
                    Ok(Rows(rows))
                }
            }
            deserializer.deserialize_seq(Visitor(std::marker::PhantomData))
        }
    }
    let deliveries: Rows<T> =
        serde_json::from_reader(file).map_err(|e| Error::Bundle(e.to_string()))?;
    Ok(deliveries.0)
}
fn snapshot_logs(
    mut stream: crate::stream::Subscription<seedbox_api::LogEntry>,
) -> Result<Vec<LogDelivery>, Error> {
    let mut output = Vec::new();
    loop {
        let delivery = stream
            .recv()
            .now_or_never()
            .ok_or_else(|| Error::Bundle("sealed log snapshot unexpectedly blocked".into()))?;
        match delivery {
            crate::stream::Delivery::Record(record) => output.push(LogDelivery::Entry {
                sequence: record.sequence,
                entry: record.payload.clone(),
            }),
            crate::stream::Delivery::Gap { first, last } => {
                output.push(LogDelivery::Gap { first, last })
            }
            crate::stream::Delivery::Closed => {
                output.push(LogDelivery::Closed);
                return Ok(output);
            }
        }
    }
}
fn snapshot_events(
    mut stream: crate::stream::Subscription<seedbox_api::SimulationEvent>,
) -> Result<Vec<EventDelivery>, Error> {
    let mut output = Vec::new();
    loop {
        let delivery = stream
            .recv()
            .now_or_never()
            .ok_or_else(|| Error::Bundle("sealed event snapshot unexpectedly blocked".into()))?;
        match delivery {
            crate::stream::Delivery::Record(record) => output.push(EventDelivery::Event {
                sequence: record.sequence,
                observation: record.payload.clone(),
            }),
            crate::stream::Delivery::Gap { first, last } => {
                output.push(EventDelivery::Gap { first, last })
            }
            crate::stream::Delivery::Closed => {
                output.push(EventDelivery::Closed);
                return Ok(output);
            }
        }
    }
}
fn validate_snapshot(snapshot: &Snapshot) -> Result<(), Error> {
    let bad = |text: &str| Error::Bundle(text.into());
    if snapshot.record.is_none()
        && !snapshot
            .omissions
            .get("record")
            .is_some_and(|reason| !reason.is_empty())
    {
        return Err(bad(
            "missing execution record requires an explicit omission reason",
        ));
    }
    if snapshot.world.simulation != snapshot.simulation.metadata.id {
        return Err(bad("world belongs to another simulation"));
    }
    let mut names = BTreeSet::new();
    for receipt in &snapshot.controls {
        let name = &receipt.request.request_id;
        if name.is_empty()
            || name.len() > 128
            || !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
            || !names.insert(name)
        {
            return Err(bad("invalid or duplicate checkpoint receipt"));
        }
        if receipt.request.command != ControlCommand::Checkpoint
            || receipt.state == ControlState::Accepted
        {
            return Err(bad("archive has an unsealed checkpoint receipt"));
        }
        let captured = matches!(&receipt.state, ControlState::Acknowledged { error: None, checkpoint: Some(c), .. } if c.state == seedbox_api::CheckpointState::Captured && c.request_id == *name);
        if captured != snapshot.checkpoints.contains_key(name) {
            return Err(bad("checkpoint receipt and binary capture disagree"));
        }
    }
    if snapshot
        .checkpoints
        .keys()
        .any(|name| !names.contains(name))
    {
        return Err(bad("checkpoint has no receipt"));
    }
    Ok(())
}
fn validate_deliveries(
    logs: &[LogDelivery],
    events: &[EventDelivery],
    snapshot: &Snapshot,
) -> Result<(), Error> {
    use crate::stream::Payload;
    fn step(next: &mut u64, closed: &mut bool, range: Option<(u64, u64)>) -> Result<(), Error> {
        if *closed {
            return Err(Error::Bundle("delivery after stream closure".into()));
        }
        if let Some((first, last)) = range {
            if first != *next || last < first || last == u64::MAX {
                return Err(Error::Bundle("invalid observation sequence or gap".into()));
            }
            *next = last + 1;
        } else {
            *closed = true;
        }
        Ok(())
    }
    let (mut next, mut closed) = (1, false);
    for delivery in logs {
        let range = match delivery {
            LogDelivery::Entry { sequence, entry } => {
                if entry.payload_bytes() > 4095 {
                    return Err(Error::Bundle("log observation exceeds stream limit".into()));
                }
                if let seedbox_api::LogEntry::Bytes { machine, .. }
                | seedbox_api::LogEntry::MachineGap { machine, .. } = entry
                {
                    if !snapshot
                        .simulation
                        .spec
                        .configuration
                        .services
                        .contains_key(machine)
                    {
                        return Err(Error::Bundle("unknown log machine".into()));
                    }
                }
                Some((*sequence, *sequence))
            }
            LogDelivery::Gap { first, last } => Some((*first, *last)),
            LogDelivery::Closed => None,
        };
        step(&mut next, &mut closed, range)?;
    }
    if !closed {
        return Err(Error::Bundle("unsealed log observations".into()));
    }
    let (mut next, mut closed) = (1, false);
    for delivery in events {
        let range = match delivery {
            EventDelivery::Event {
                sequence,
                observation,
            } => {
                if observation.simulation != snapshot.simulation.metadata.id
                    || observation.payload_bytes() > 8191
                {
                    return Err(Error::Bundle("invalid event observation".into()));
                }
                Some((*sequence, *sequence))
            }
            EventDelivery::Gap { first, last } => Some((*first, *last)),
            EventDelivery::Closed => None,
        };
        step(&mut next, &mut closed, range)?;
    }
    if !closed {
        return Err(Error::Bundle("unsealed event observations".into()));
    }
    Ok(())
}

/// Ignore only provenance at its owning schema paths. Actual architecture,
/// backend, launch kind/ownership/observation contract, policies, binaries and
/// checkpoint state remain comparable execution inputs.
pub(super) fn semantic_definition(
    mut definition: Box<seedbox_spec::definition::Definition>,
) -> seedbox_application::Result<Box<seedbox_spec::definition::Definition>> {
    for machine in definition
        .machines
        .values_mut()
        .chain(std::iter::once(&mut definition.oracle))
    {
        machine.image = None;
    }
    if let Some(value) = definition
        .runtime
        .execution
        .pointer_mut("/semantic_parameters/engine")
        .filter(|value| !value.is_null())
    {
        let mut descriptor: seedbox_artifacts::execution::EngineDescriptor =
            serde_json::from_value(value.clone())?;
        descriptor.simulator_version.clear();
        descriptor.host_os.clear();
        descriptor.host_arch.clear();
        descriptor.guest_arch.clear();
        descriptor.engine_timebase.clear();
        *value = serde_json::to_value(descriptor)?;
    }
    Ok(definition)
}
