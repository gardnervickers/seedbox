//! Closed-proposal corruption cases need access only inside its owning module.
use super::*;

struct NeverRun;
impl Machine for NeverRun {
    type Context = ();
    type Error = std::io::Error;
    type Snapshot = ();
    fn run(&mut self, _: &mut RunContext<'_, ()>, _: Timestamp) -> Result<StopReason, Self::Error> {
        panic!("stale proposal reached a machine before exact preflight");
    }
    fn snapshot(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}
fn encoded(d: &mut Driver<NeverRun>) -> Vec<u8> {
    let c = d
        .checkpoint_map(|_| Ok::<_, std::io::Error>(SnapshotReference::new([0; 32], 0)))
        .unwrap();
    let mut bytes = Vec::new();
    c.write_to(&mut bytes).unwrap();
    bytes
}
#[test]
fn changed_key_action_and_each_record_field_mismatch_before_effects() {
    for field in 0..9 {
        let mut d = Driver::new_seeded(
            Timestamp::from_nanos(0),
            Duration::from_nanos(10),
            1,
            51,
            || Ok(()),
            fixture::definition(0),
            vec![fixture::definition(1)],
            vec![],
            NetworkConfig::default(),
            |_| Ok(NeverRun),
        )
        .unwrap();
        let mut p = d
            .with_barrier(|b| {
                b.prepare_input(
                    InputKey::new("original").unwrap(),
                    Action::Pause(Target::Machine(MachineId(1))),
                )
            })
            .unwrap();
        let PreparedOutcome::Admitted(record) = &mut p.outcome else {
            unreachable!()
        };
        match field {
            0 => p.key = InputKey::new("changed").unwrap(),
            1 => p.action = Action::Crash(Target::Machine(MachineId(1))),
            2 => p.position.boundary_index += 1,
            3 => record.fault = FaultId(record.fault.get() + 1),
            4 => {
                record.origin = FaultOrigin::Input {
                    key: InputKey::new("changed").unwrap(),
                }
            }
            5 => {
                record.target = FaultTarget::Machine {
                    machine: MachineId(1),
                    boot: 1,
                }
            }
            6 => record.action = FaultAction::Crash,
            7 => record.handle = Some(FaultHandle("p123".into())),
            8 => record.requested_at = Timestamp::from_nanos(1),
            _ => unreachable!(),
        }
        let before = encoded(&mut d);
        let playback = d.playback.clone();
        assert!(matches!(
            d.with_barrier(|b| b.commit_input(&p)),
            Err(Error::PreparedInputMismatch)
        ));
        assert!(!d.execution.failed);
        assert_eq!(d.playback, playback);
        assert_eq!(encoded(&mut d), before, "field {field} mutated the source");
    }
}
