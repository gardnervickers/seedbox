//! Portable composition regression with synthetic native components, not VM proof.
//! Exercise real Driver routing/captures; use the production stopped collector.
use super::*;
use seedbox_driver::driver::{FrameFacts, NetworkConfig, NetworkSetup};
use seedbox_machine::{
    definition::{MacAddress, NetworkInterface},
    machine::{execution_evidence::chain, EvidenceDisposition},
    pv::{Output, Request, Response},
};
use std::time::Duration;

#[derive(Clone)]
struct NetworkModel {
    sender: bool,
    sent: bool,
    received: u64,
    native: [u8; 32],
}
impl NetworkModel {
    fn new(sender: bool) -> Self {
        Self {
            sender,
            sent: false,
            received: 0,
            native: chain::initial(),
        }
    }
}
impl Machine for NetworkModel {
    type Context = ();
    type Error = String;
    type Snapshot = Self;
    fn run(
        &mut self,
        context: &mut RunContext<'_, ()>,
        upto: Timestamp,
    ) -> std::result::Result<StopReason, String> {
        while let Some(input) = context.inbox.take() {
            if let Request::ReceiveFrame { bytes } = input.payload {
                self.received += 1;
                self.native = chain::record(
                    &self.native,
                    b'I',
                    8,
                    input.at.as_nanos(),
                    bytes.len() as u64,
                    u64::from(bytes[63]),
                );
            }
        }
        if self.sender && !self.sent {
            for index in 0..3 {
                let mut bytes = vec![0; 64];
                bytes[..6].fill(255);
                bytes[6..12].copy_from_slice(&[2, 0, 0, 0, 0, 1]);
                bytes[12..14].copy_from_slice(&[0x88, 0xb5]);
                bytes[63] = index;
                context
                    .outbox
                    .emit(Output {
                        at: upto,
                        payload: Response::TransmitFrame { bytes },
                    })
                    .map_err(|error| format!("model frame output: {error:?}"))?;
                self.native =
                    chain::record(&self.native, b'O', 8, upto.as_nanos(), 64, u64::from(index));
            }
            self.sent = true;
        }
        Ok(StopReason::BoundaryReached)
    }
    fn snapshot(&mut self) -> std::result::Result<Self, String> {
        Ok(self.clone())
    }
    fn execution_evidence(&mut self) -> std::result::Result<Option<ExecutionEvidence>, String> {
        Ok(Some(ExecutionEvidence {
            boot: 0,
            disposition: EvidenceDisposition::Live,
            witness: hex::encode(self.native),
            disk: None,
        }))
    }
}
fn make(workers: usize, capacity: usize) -> Driver<NetworkModel> {
    let mut sender = factory_definition::definition(0);
    sender.name = "sender".into();
    let mut receiver = factory_definition::definition(1);
    receiver.name = "receiver".into();
    for (index, definition) in [&mut sender, &mut receiver].into_iter().enumerate() {
        definition.network = Some(
            NetworkInterface::new(
                MacAddress::new([2, 0, 0, 0, 0, (index + 1) as u8]).unwrap(),
                [10, 0, 0, (index + 1) as u8].into(),
                24,
            )
            .unwrap(),
        );
    }
    Driver::new_seeded_with_network(
        Timestamp::from_nanos(0),
        Duration::from_nanos(10),
        workers,
        51,
        || Ok(()),
        sender,
        vec![receiver],
        vec![],
        NetworkSetup::from(
            NetworkConfig::new(Duration::from_nanos(20), Duration::from_nanos(3), 0).unwrap(),
        )
        .with_frame_capacity(capacity),
        |definition| Ok(NetworkModel::new(definition.name == "sender")),
    )
    .unwrap()
}
fn evidence(driver: &mut Driver<NetworkModel>) -> (Vec<u8>, ContentDigest) {
    let mut policy = Vec::new();
    let names = driver
        .with_barrier(|barrier| {
            barrier.write_policy_evidence(&mut policy).unwrap();
            Ok(barrier
                .machines()
                .map(|(id, _)| {
                    (
                        id,
                        if id == MachineId::ORACLE {
                            "sender"
                        } else {
                            "receiver"
                        }
                        .into(),
                    )
                })
                .collect())
        })
        .unwrap();
    let witness = collect(driver, &names, &ContentDigest::of(&policy), None).unwrap();
    (policy, witness)
}
fn advance(driver: &mut Driver<NetworkModel>, start: u64, end: u64) {
    for at in start..=end {
        driver.step(Timestamp::from_nanos(at)).unwrap();
    }
}
fn assert_delivered(driver: &mut Driver<NetworkModel>) {
    driver
        .with_barrier(|barrier| {
            assert_eq!(
                barrier
                    .machines()
                    .map(|(_, m)| m.received)
                    .collect::<Vec<_>>(),
                [0, 3]
            );
            Ok(())
        })
        .unwrap();
}
#[test]
fn fresh_and_restored_frame_observation_and_workers_preserve_policy_and_native_composition() {
    let mut expected_pending = None;
    let mut expected_final = None;
    for workers in [1, 4] {
        // Zero disables observation; one exercises retained facts AND overflow.
        for capacity in [0, 1] {
            let mut source = make(workers, capacity);
            advance(&mut source, 1, 10);
            let pending = evidence(&mut source);
            if let Some(expected) = &expected_pending {
                assert_eq!(&pending, expected);
            } else {
                expected_pending = Some(pending.clone());
            }
            let checkpoint = source.checkpoint().unwrap();
            // Capture leaves source observations intact but does not retain them.
            let retained = source.drain_observations().frames;
            assert_eq!(retained.facts.len(), capacity);
            assert_eq!(retained.omitted, if capacity == 0 { 0 } else { 2 });
            advance(&mut source, 11, 80);
            assert_delivered(&mut source);
            let finished = evidence(&mut source);
            assert_ne!(
                pending.0, finished.0,
                "delivery must change policy evidence"
            );
            assert_ne!(
                pending.1, finished.1,
                "delivery must change native composition"
            );
            if let Some(expected) = &expected_final {
                assert_eq!(&finished, expected);
            } else {
                expected_final = Some(finished.clone());
            }
            for restored_workers in [1, 4] {
                for restored_capacity in [0, 1] {
                    let mut branch = Driver::<NetworkModel>::from_checkpoint(
                        &checkpoint,
                        restored_workers,
                        || Ok(()),
                        |snapshot| Ok(snapshot.clone()),
                        |definition| Ok(NetworkModel::new(definition.name == "sender")),
                    )
                    .unwrap();
                    assert_eq!(branch.drain_observations().frames, FrameFacts::default());
                    branch.set_frame_capacity(restored_capacity);
                    assert_eq!(evidence(&mut branch), pending);
                    advance(&mut branch, 11, 80);
                    assert_delivered(&mut branch);
                    assert_eq!(evidence(&mut branch), finished);
                }
            }
        }
    }
}
