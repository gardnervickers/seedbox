//! Spawns publish against the declared standby name, including a guest spawn
//! whose machine construction fails.
use super::*;
use seedbox_engine_protocol::{
    control::Action as Public,
    fault_status::FaultStatus,
    observation::{self, Kind, Observation},
};
use seedbox_machine::pv::SimulationControlId;
use std::os::unix::net::UnixDatagram;

/// The oracle asks to spawn `target` from its first run, like `World.spawn`.
struct SpawnMock(Mock, Option<String>);
impl Machine for SpawnMock {
    type Context = ();
    type Error = String;
    type Snapshot = Saved;
    fn run(
        &mut self,
        c: &mut seedbox_machine::machine::RunContext<'_, ()>,
        upto: Timestamp,
    ) -> std::result::Result<MachineStop, String> {
        let stopped = self.0.run(c, upto)?;
        if let Some(target) = self.1.take() {
            c.outbox
                .emit(Output {
                    at: upto,
                    payload: Response::SimulationControl {
                        id: SimulationControlId {
                            session: std::num::NonZeroU32::new(1).unwrap(),
                            request: std::num::NonZeroU32::new(1).unwrap(),
                        },
                        payload: format!(
                            r#"{{"version":1,"id":1,"op":"spawn","machine":"{target}"}}"#
                        )
                        .into_bytes(),
                    },
                })
                .unwrap();
        }
        Ok(stopped)
    }
    fn snapshot(&mut self) -> std::result::Result<Saved, String> {
        self.0.snapshot()
    }
}

fn spawn_worker(target: &str) -> (tempfile::TempDir, Worker<SpawnMock>, UnixDatagram) {
    let (root, old, runs) = worker(1);
    let socket_path = root.path().join("spawn.sock");
    let socket = UnixDatagram::bind(&socket_path).unwrap();
    observation::configure_socket(&socket).unwrap();
    socket.set_nonblocking(true).unwrap();
    let constructed: Constructed = Arc::default();
    let ledger = constructed.clone();
    let target = target.to_owned();
    let driver = Driver::new_seeded(
        Timestamp::from_nanos(0),
        Duration::from_nanos(10),
        1,
        42,
        || Ok(()),
        definition("oracle"),
        vec![definition("n1")],
        vec![definition("standby"), definition("broken")],
        NetworkConfig::default(),
        move |definition| {
            if definition.name == "broken" {
                return Err("broken standby cannot be built".into());
            }
            ledger.lock().unwrap().push_back(definition.name.clone());
            let spawns = (definition.name == "oracle").then(|| target.clone());
            Ok(SpawnMock(
                Mock {
                    data: Data {
                        name: definition.name.clone(),
                        at: 0,
                        first: true,
                        dead: false,
                        crashes: 0,
                        random: [0; 8],
                    },
                    runs: runs.clone(),
                },
                spawns,
            ))
        },
    )
    .unwrap();
    let mut state = old.state.clone();
    state.world.machine_names.clear();
    state.world.publication = Default::default();
    let mut worker = Worker::new(
        driver,
        state,
        old.inventory.clone(),
        old.output.clone(),
        BTreeMap::new(),
        constructed,
        Some(socket_path.as_path()),
        |saved, out| {
            serde_json::to_writer(out, &saved.0)?;
            Ok(())
        },
    )
    .unwrap();
    worker.flush().unwrap();
    (root, worker, socket)
}

/// Every published fault row as (machine, status, plan).
fn fault_rows(socket: &UnixDatagram) -> Vec<(Option<String>, FaultStatus, Option<Public>)> {
    let mut rows = vec![];
    let mut bytes = [0; observation::MAX_DATAGRAM_BYTES + 1];
    while let Ok(n) = socket.recv(&mut bytes) {
        let observation: Observation = serde_json::from_slice(&bytes[..n]).unwrap();
        if let Kind::FaultChanged {
            machine,
            status,
            plan,
            ..
        } = observation.kind
        {
            rows.push((machine, status, plan.map(|plan| *plan)));
        }
    }
    rows
}

#[test]
fn guest_spawn_publishes_the_standby_name_and_public_plan() {
    let (_root, mut worker, socket) = spawn_worker("standby");
    for at in [10, 20, 30, 40] {
        worker.step(Timestamp::from_nanos(at)).unwrap();
    }
    assert!(worker.names.values().any(|name| name == "standby"));
    let plan = Some(Public::Spawn {
        machine: "standby".into(),
    });
    let standby = Some("standby".to_owned());
    assert_eq!(
        fault_rows(&socket),
        [
            (standby.clone(), FaultStatus::Requested, plan.clone()),
            (standby, FaultStatus::Applied, plan),
        ]
    );
}

#[test]
fn failed_spawn_construction_reports_the_construction_error() {
    let (_root, mut worker, socket) = spawn_worker("broken");
    let error = (1..=4)
        .find_map(|n| worker.step(Timestamp::from_nanos(n * 10)).err())
        .expect("construction failure stops the worker")
        .to_string();
    assert!(error.contains("broken standby cannot be built"), "{error}");
    assert!(!worker.names.values().any(|name| name == "broken"));
    let broken = Some("broken".to_owned());
    let plan = Some(Public::Spawn {
        machine: "broken".into(),
    });
    assert_eq!(
        fault_rows(&socket),
        [
            (broken.clone(), FaultStatus::Requested, plan.clone()),
            (broken, FaultStatus::Failed, plan),
        ]
    );
}

#[test]
fn authored_spawn_publishes_the_standby_name_and_input_plan() {
    let (_root, mut worker, _) = worker_with(1, false, vec![definition("standby")]);
    worker.step(Timestamp::from_nanos(10)).unwrap();
    worker.state.world.input_program.inputs =
        vec![seedbox_engine_protocol::input_program::TimedInput {
            key: "spawn".into(),
            at_ns: 10,
            timeout_ns: 5_000_000_000,
            action: Public::Spawn {
                machine: "standby".into(),
            },
        }];
    assert!(worker.authored_next().unwrap());
    assert!(worker.state.world.input_program.rejections.is_empty());
    worker.step(Timestamp::from_nanos(40)).unwrap();
    assert!(worker.names.values().any(|name| name == "standby"));
}
