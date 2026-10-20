//! Synthetic SDK model through the service descriptor/envelope path. No native
//! execution or RAM evidence is claimed. D6 validation remains exclusively SDK-owned.
use super::super::{open, Opened};
use super::*;
use seedbox_driver::driver::{Action, InputKey, Target};
use seedbox_machine::{
    machine::{EvidenceDisposition, ExitReason},
    pv::Request,
};

struct EndModel {
    oracle: bool,
    ended: bool,
    boot: u64,
    deferred: bool,
}
enum EndSaved {
    Live(bool, bool, u64, bool),
    Stored(Opened),
}
impl EndModel {
    fn emit(c: &mut RunContext<'_, ()>, payload: Response) {
        c.outbox
            .emit(Output {
                at: Timestamp::from_nanos(c.clock.now()),
                payload,
            })
            .unwrap();
    }
    fn report(&self, disposition: EvidenceDisposition, native_booted: bool) -> Response {
        Response::BootEnded {
            ended_boot: self.boot,
            disposition,
            native_booted,
            disk_lost_sectors: 0,
            surviving_disk: None,
            retired_state_digest: None,
        }
    }
}
impl Machine for EndModel {
    type Context = ();
    type Error = String;
    type Snapshot = EndSaved;
    fn run(
        &mut self,
        c: &mut RunContext<'_, ()>,
        upto: Timestamp,
    ) -> std::result::Result<StopReason, String> {
        while let Some(input) = c.inbox.take() {
            if matches!(input.payload, Request::Reboot) {
                // Deliberately repeat an already-ended ordinal when restored
                // from Some(0): the SDK, not this adapter, must reject it.
                Self::emit(c, self.report(EvidenceDisposition::Crashed, false));
                self.boot += 1;
                self.ended = false;
                Self::emit(
                    c,
                    Response::BootConsole {
                        bytes: b"new boot".to_vec(),
                    },
                );
                return Ok(StopReason::BoundaryReached);
            }
        }
        if upto.as_nanos() == c.clock.now() && !self.ended {
            return Ok(StopReason::BoundaryReached);
        }
        if !self.oracle {
            if !self.ended {
                if !self.deferred {
                    Self::emit(c, self.report(EvidenceDisposition::Crashed, true));
                }
                self.ended = true;
            }
            return Ok(StopReason::Exited {
                reached: Timestamp::from_nanos(c.clock.now()),
                reason: ExitReason::Crashed,
            });
        }
        Ok(StopReason::BoundaryReached)
    }
    fn snapshot(&mut self) -> std::result::Result<EndSaved, String> {
        Ok(EndSaved::Live(
            self.oracle,
            self.ended,
            self.boot,
            self.deferred,
        ))
    }
}
fn decode(saved: &EndSaved) -> std::result::Result<EndModel, String> {
    let (oracle, ended, boot, deferred) = match saved {
        EndSaved::Stored(file) => {
            serde_json::from_reader(file.reader()).map_err(|e| e.to_string())?
        }
        EndSaved::Live(oracle, ended, boot, deferred) => (*oracle, *ended, *boot, *deferred),
    };
    Ok(EndModel {
        oracle,
        ended,
        boot,
        deferred,
    })
}
// A Driver step may settle a zero-budget/control boundary without reaching the
// requested quantum. This is ordinary stepping, not a retry after an error.
fn drive(
    driver: &mut Driver<EndModel>,
    upto: Timestamp,
) -> std::result::Result<(), seedbox_driver::driver::Error<String>> {
    for _ in 0..128 {
        let at = driver.with_barrier(|barrier| Ok(barrier.playback().position.at))?;
        if at >= upto {
            return Ok(());
        }
        driver.step(upto)?;
    }
    panic!("synthetic Driver did not complete the requested boundary");
}
#[test]
fn codec_service_descriptor_roundtrip_distinguishes_none_and_some_zero() {
    for already_ended in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let inventory = fixture_inventory(&root.path().join("authority"));
        let mut source = Driver::new_seeded(
            Timestamp::from_nanos(0),
            Duration::from_nanos(10),
            1,
            9338,
            || Ok(()),
            definition("oracle"),
            vec![definition("m1")],
            vec![],
            NetworkConfig::default(),
            move |definition| {
                Ok(EndModel {
                    oracle: definition.name == "oracle",
                    ended: false,
                    boot: 0,
                    deferred: !already_ended,
                })
            },
        )
        .unwrap();
        let names: BTreeMap<_, _> = source
            .with_barrier(|barrier| {
                Ok(barrier
                    .machines()
                    .zip(["oracle", "m1"])
                    .map(|((id, _), name)| (id, name.to_owned()))
                    .collect())
            })
            .unwrap();
        // Both native models have ended; only one has already published its
        // retirement. The other retains the typed report until reboot.
        drive(&mut source, Timestamp::from_nanos(10)).unwrap();
        let mut state = state(2);
        let mut publisher = crate::driver_world::events::Publisher::connect(None).unwrap();
        let path = capture(
            &mut source,
            Capture {
                inventory: &inventory,
                state: &mut state,
                prepared: false,
            },
            root.path(),
            "d6-roundtrip",
            |state, batch| {
                // This fixture prebinds current service state; no causal action
                // is reconstructed from its publication copies.
                publisher.batch(state, &names, batch)
            },
            |saved, writer| {
                let EndSaved::Live(oracle, ended, boot, deferred) = saved else {
                    panic!("new source snapshot must be owned")
                };
                serde_json::to_writer(writer, &(oracle, ended, boot, deferred))?;
                Ok(())
            },
        )
        .unwrap();
        let envelope = read(&path).unwrap();
        envelope.validate().unwrap();
        let checkpoint = open(&envelope, limits())
            .unwrap()
            .try_map_snapshots(|file| Ok::<_, Error>(EndSaved::Stored(file)))
            .unwrap();
        drop(source);
        let mut expected = None;
        for workers in [1, 4] {
            let mut branch = Driver::<EndModel>::from_checkpoint(
                &checkpoint,
                workers,
                || Ok(()),
                decode,
                |_| Err("unexpected spawn".into()),
            )
            .unwrap();
            let target = *names
                .iter()
                .find(|(_, name)| name.as_str() == "m1")
                .unwrap()
                .0;
            let now = branch
                .with_barrier(|barrier| Ok(barrier.playback().position.at))
                .unwrap();
            branch
                .with_barrier(|barrier| {
                    barrier.schedule(
                        now,
                        InputKey::new("reboot-after-restore").unwrap(),
                        Action::Reboot(Target::Machine(target)),
                    )
                })
                .unwrap();
            let result = drive(&mut branch, Timestamp::from_nanos(now.as_nanos() + 10));
            if already_ended {
                let settled = branch.drain_observations().events;
                assert!(
                    matches!(
                        &result,
                        Err(seedbox_driver::driver::Error::InvalidBootEnd { .. })
                    ),
                    "Some(0) must not become None after the service roundtrip: {result:?}; {settled:?}"
                );
                assert!(branch
                    .step(Timestamp::from_nanos(now.as_nanos() + 20))
                    .is_err());
            } else {
                result.unwrap();
                let values: Vec<_> = branch
                    .drain_observations()
                    .events
                    .iter()
                    .map(|event| crate::driver_world::events::encode(&names, event).unwrap())
                    .collect();
                let ended = values
                    .iter()
                    .find(|value| {
                        value["kind"]["payload"]["type"] == "boot_ended"
                            && value["kind"]["payload"]["ended_boot"] == 0
                    })
                    .unwrap();
                assert_eq!(
                    ended["kind"]["boot"], 1,
                    "generic prospective carrier stamp must remain unchanged"
                );
                assert_eq!(ended["kind"]["payload"]["ended_boot"], 0);
                assert_eq!(ended["kind"]["payload"]["native_booted"], false);
                assert!(ended["kind"]["payload"]["retired_state_digest"].is_null());
                assert!(values
                    .iter()
                    .any(|value| value["kind"]["payload"]["type"] == "boot_console"
                        && value["kind"]["boot"] == 1));
                if let Some(expected) = &expected {
                    assert_eq!(&values, expected);
                } else {
                    expected = Some(values);
                }
            }
        }
    }
}
