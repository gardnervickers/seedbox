//! One serialized worker connection for playback and semantic interventions.
//! The catalogue lock is never held during transport or journal I/O.
use seedbox_api::{ControlCommand, ControlRequest, ControlState, Intervention, PlaybackStatus};
use seedbox_artifacts::interventions::Journal;
use seedbox_driver_execution::host_control::{Client, ClientError};
use seedbox_engine_protocol::host_control::{Command, Reply};
use std::{path::PathBuf, time::Duration};

const ATTEMPT: Duration = Duration::from_millis(250);

/// The connection gate must drain before retirement can publish completion or
/// permit deletion of files still owned by control I/O.
pub(crate) fn lock_for_retirement(
    worker: &std::sync::Mutex<Worker>,
) -> std::sync::LockResult<std::sync::MutexGuard<'_, Worker>> {
    #[cfg(test)]
    if let Some(blocked) = RETIREMENT_BLOCKED.with(|probe| probe.borrow_mut().take()) {
        match worker.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(std::sync::TryLockError::Poisoned(error)) => return Err(error),
            Err(std::sync::TryLockError::WouldBlock) => {
                // This is an observed failed acquisition of the actual gate,
                // not a thread-start notification or elapsed-silence inference.
                blocked.send(()).expect("retirement probe receiver dropped");
            }
        }
    }
    worker.lock()
}

#[cfg(test)]
thread_local! {
    static RETIREMENT_BLOCKED: std::cell::RefCell<Option<std::sync::mpsc::Sender<()>>> = const { std::cell::RefCell::new(None) };
}

/// Observe contention on this thread's next retirement only. Per-thread probes
/// cannot interfere with concurrent simulations or tests.
#[cfg(test)]
pub(crate) fn observe_retirement_blocked(blocked: std::sync::mpsc::Sender<()>) {
    RETIREMENT_BLOCKED.with(|probe| {
        assert!(probe.borrow_mut().replace(blocked).is_none());
    });
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("worker control connection is retired")]
    Retired,
    #[error("checkpoint capture is pending")]
    Pending,
    #[error(transparent)]
    Transport(#[from] ClientError),
    #[error("{0}")]
    Fatal(String),
}

struct FaultTransaction {
    request: ControlRequest,
    /// Set only after the exact proposal has been durably retained.
    retained: Option<Intervention>,
}

pub(crate) struct Worker {
    retired: bool,
    run: PathBuf,
    client: Option<Client>,
    journal: Option<Journal>,
    fault: Option<FaultTransaction>,
    #[cfg(test)]
    failure_gate: Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
}

impl Worker {
    pub fn new(run: PathBuf) -> Self {
        Self {
            retired: false,
            run,
            client: None,
            journal: None,
            fault: None,
            #[cfg(test)]
            failure_gate: None,
        }
    }

    /// Pause after real protocol failure, while the caller still owns this
    /// connection gate. A transport timeout cannot release this explicit gate.
    #[cfg(test)]
    pub(crate) fn gate_failure_publication(
        &mut self,
        entered: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    ) {
        assert!(self.failure_gate.replace((entered, release)).is_none());
    }

    #[cfg(test)]
    pub(crate) fn await_failure_publication(&mut self) {
        if let Some((entered, release)) = self.failure_gate.take() {
            entered.send(()).expect("failure observer dropped");
            release.recv().expect("failure publication gate dropped");
        }
    }

    pub(crate) fn retire(&mut self) {
        self.retired = true;
    }

    pub(crate) fn is_retired(&self) -> bool {
        self.retired
    }

    fn client(&mut self) -> Result<&mut Client, ClientError> {
        if self.client.is_none() {
            self.client = Some(Client::for_run(&self.run)?);
        }
        self.client
            .as_mut()
            .ok_or_else(|| ClientError::Protocol("missing worker connection".into()))
    }

    pub fn apply(&mut self, request: &ControlRequest) -> Result<ControlState, Error> {
        if self.retired {
            return Err(Error::Retired);
        }
        let client = self.client()?;
        // Resolve a timed-out read before any mutation can overtake it.
        if client
            .pending()
            .is_some_and(|request| request.command == Command::Status)
        {
            client.execute(Command::Status, ATTEMPT)?;
        }
        if let ControlCommand::InjectFault { action } = &request.command {
            let result = self.inject(request, action.clone());
            if matches!(result, Err(Error::Transport(ClientError::Unsupported))) {
                self.fault = None;
            }
            return result;
        }
        if self.fault.is_some() {
            return Err(Error::Fatal(
                "another command overtook a pending fault".into(),
            ));
        }
        let command = match request.command {
            ControlCommand::SetRate { rate } => Command::SetRate { rate },
            ControlCommand::PauseBeforeNextFault { rate } => Command::PauseBeforeNextFault { rate },
            ControlCommand::Step => Command::Step,
            ControlCommand::Checkpoint => Command::Checkpoint {
                request_id: request.request_id.clone(),
            },
            ControlCommand::InjectFault { .. } => {
                return Err(Error::Fatal("invalid control dispatch".into()))
            }
        };
        let reply = self.client()?.execute(command, ATTEMPT)?;
        if request.command == ControlCommand::Checkpoint && reply.error.is_none() {
            let checkpoint = reply
                .checkpoint
                .as_ref()
                .ok_or_else(|| Error::Fatal("worker omitted checkpoint status".into()))?;
            if checkpoint.request_id != request.request_id {
                return Err(Error::Fatal(
                    "worker returned a different checkpoint".into(),
                ));
            }
            if checkpoint.state == seedbox_engine_protocol::host_control::CheckpointState::Requested
            {
                return Err(Error::Pending);
            }
            // No later mutation can overtake this request before its receipt.
            // Freeze the prefix now: the source may accept another fault at
            // this same paused boundary after capture has been acknowledged.
            let mut history =
                seedbox_artifacts::interventions::read(&self.run.join("interventions.json"))
                    .map_err(|e| Error::Fatal(e.to_string()))?;
            history.entries.retain(|entry| {
                entry.position.boundary_index <= checkpoint.position.boundary_index
            });
            let bytes = history.encode().map_err(Error::Fatal)?;
            std::fs::write(
                self.run.join(format!(
                    "checkpoint-{}-interventions.json",
                    request.request_id
                )),
                bytes,
            )
            .map_err(|e| Error::Fatal(format!("checkpoint intervention history: {e}")))?;
        }
        Ok(receipt(reply))
    }

    fn inject(
        &mut self,
        request: &ControlRequest,
        action: seedbox_api::FaultAction,
    ) -> Result<ControlState, Error> {
        self.client()?;
        let Self {
            run,
            client,
            journal,
            fault,
            ..
        } = self;
        Self::inject_transaction(run, journal, fault, request, action, |command| {
            client
                .as_mut()
                .ok_or_else(|| ClientError::Protocol("missing worker connection".into()))?
                .execute(command, ATTEMPT)
        })
    }

    // Keep the transaction's durable ordering independent of transport timing.
    // The production executor is the same serialized Client and attempt budget;
    // tests can explicitly choose which phase times out without racing a peer.
    fn inject_transaction(
        run: &std::path::Path,
        journal: &mut Option<Journal>,
        fault: &mut Option<FaultTransaction>,
        request: &ControlRequest,
        action: seedbox_api::FaultAction,
        mut execute: impl FnMut(Command) -> Result<Reply, ClientError>,
    ) -> Result<ControlState, Error> {
        if let Some(transaction) = fault.as_ref() {
            if &transaction.request != request {
                return Err(Error::Fatal(
                    "fault request changed during admission".into(),
                ));
            }
        } else {
            if journal.is_none() {
                *journal = Some(
                    Journal::open(&run.join("interventions.json"))
                        .map_err(|e| Error::Fatal(e.to_string()))?,
                );
            }
            *fault = Some(FaultTransaction {
                request: request.clone(),
                retained: None,
            });
        }
        if fault.as_ref().is_some_and(|t| t.retained.is_none()) {
            let reply = execute(Command::PrepareFault {
                request_id: request.request_id.clone(),
                action: action.clone(),
                timeout_ns: seedbox_control::PROCESS_EFFECT_TIMEOUT_NS,
            })?;
            if reply.error.is_some() {
                *fault = None;
                return Ok(receipt(reply));
            }
            let record = reply
                .intervention
                .ok_or_else(|| Error::Fatal("worker omitted its fault proposal".into()))?;
            if record.request_id != request.request_id
                || record.plan.action != action
                || record.position != reply.position
                || record.plan.at_ns != reply.position.simulated_time_ns
                || record
                    .plan
                    .at_ns
                    .checked_add(seedbox_control::PROCESS_EFFECT_TIMEOUT_NS)
                    != Some(record.plan.deadline_ns)
            {
                return Err(Error::Fatal(
                    "worker fault proposal differs from the accepted request".into(),
                ));
            }
            journal
                .as_mut()
                .ok_or_else(|| Error::Fatal("missing intervention journal".into()))?
                .append(record.clone())
                .map_err(|e| Error::Fatal(e.to_string()))?;
            fault
                .as_mut()
                .ok_or_else(|| Error::Fatal("missing fault transaction".into()))?
                .retained = Some(record);
        }
        let committed = execute(Command::CommitFault {
            request_id: request.request_id.clone(),
        });
        let reply = match committed {
            Ok(reply) => reply,
            Err(
                error @ (ClientError::Unsupported
                | ClientError::Protocol(_)
                | ClientError::Exhausted),
            ) => {
                return Err(Error::Fatal(format!(
                    "cannot commit retained intervention: {error}"
                )));
            }
            Err(error) => return Err(error.into()),
        };
        let retained = fault
            .as_ref()
            .and_then(|t| t.retained.as_ref())
            .ok_or_else(|| Error::Fatal("fault commit preceded retention".into()))?;
        if reply.error.is_some() || reply.intervention.as_ref() != Some(retained) {
            return Err(Error::Fatal(format!(
                "worker did not acknowledge the retained fault: {:?}",
                reply.error
            )));
        }
        *fault = None;
        Ok(receipt(reply))
    }

    pub fn inspect(&mut self) -> Result<PlaybackStatus, ClientError> {
        if self.retired {
            return Err(ClientError::Protocol(
                "worker control connection is retired".into(),
            ));
        }
        let reply = self.client()?.execute(Command::Status, ATTEMPT)?;
        if let Some(error) = reply.error {
            return Err(ClientError::Protocol(format!("status rejected: {error:?}")));
        }
        Ok(status(&reply))
    }
}

fn status(reply: &Reply) -> PlaybackStatus {
    PlaybackStatus {
        position: reply.position,
        rate: reply.rate,
        achieved_rate: reply.achieved_rate,
        stepping: reply.stepping,
        terminal: reply.terminal,
        application_ready: reply.application_ready,
    }
}

fn receipt(reply: Reply) -> ControlState {
    ControlState::Acknowledged {
        status: status(&reply),
        error: reply.error,
        intervention: reply.intervention.map(Box::new),
        detail: reply.detail,
        checkpoint: reply.checkpoint,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seedbox_engine_protocol::host_control::{
        BoundaryPosition, PlaybackRate, Request, MAX_DATAGRAM,
    };
    use std::{fs, os::unix::net::UnixDatagram, path::Path};

    fn fixture() -> (tempfile::TempDir, UnixDatagram, Worker) {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("output")).unwrap();
        let endpoint = directory.path().join("socket");
        let socket = UnixDatagram::bind(&endpoint).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        fs::write(
            directory.path().join("output/host-control"),
            endpoint.as_os_str().as_encoded_bytes(),
        )
        .unwrap();
        let worker = Worker::new(directory.path().to_owned());
        (directory, socket, worker)
    }

    fn request() -> ControlRequest {
        ControlRequest {
            request_id: "cut".into(),
            command: ControlCommand::InjectFault {
                action: seedbox_api::FaultAction::Heal {
                    handle: "cut".into(),
                },
            },
        }
    }

    fn receive(socket: &UnixDatagram) -> (Request, PathBuf) {
        let mut bytes = [0; MAX_DATAGRAM + 1];
        let (length, peer) = socket.recv_from(&mut bytes).unwrap();
        (
            serde_json::from_slice(&bytes[..length]).unwrap(),
            peer.as_pathname().unwrap().to_owned(),
        )
    }

    fn proposal(id: u64) -> Reply {
        let position = BoundaryPosition {
            boundary_index: 3,
            completed_rounds: 2,
            simulated_time_ns: 100,
        };
        Reply {
            id,
            position,
            terminal: false,
            application_ready: true,
            rate: PlaybackRate::Unlimited,
            achieved_rate: None,
            stepping: false,
            checkpoint: None,
            error: None,
            detail: None,
            intervention: Some(Intervention {
                request_id: "cut".into(),
                position,
                plan: seedbox_control::Plan {
                    id: 1,
                    at_ns: 100,
                    deadline_ns: 100 + seedbox_control::PROCESS_EFFECT_TIMEOUT_NS,
                    action: seedbox_api::FaultAction::Heal {
                        handle: "cut".into(),
                    },
                },
            }),
        }
    }

    fn send(socket: &UnixDatagram, peer: &Path, reply: &Reply) {
        socket
            .send_to(&serde_json::to_vec(reply).unwrap(), peer)
            .unwrap();
    }

    #[test]
    fn checkpoint_freezes_history_before_later_faults_at_the_same_boundary() {
        let (directory, socket, mut worker) = fixture();
        let path = directory.path().join("interventions.json");
        let mut journal = Journal::initialize(&path).unwrap();
        let first = proposal(1).intervention.unwrap();
        journal.append(first.clone()).unwrap();
        std::thread::scope(|scope| {
            let engine = scope.spawn(move || {
                let (request, peer) = receive(&socket);
                assert_eq!(
                    request.command,
                    Command::Checkpoint {
                        request_id: "saved".into()
                    }
                );
                let mut reply = proposal(request.id);
                reply.intervention = None;
                reply.checkpoint = Some(seedbox_engine_protocol::host_control::CheckpointStatus {
                    request_id: "saved".into(),
                    position: reply.position,
                    state: seedbox_engine_protocol::host_control::CheckpointState::Captured,
                });
                send(&socket, &peer, &reply);
            });
            worker
                .apply(&ControlRequest {
                    request_id: "saved".into(),
                    command: ControlCommand::Checkpoint,
                })
                .unwrap();
            engine.join().unwrap();
        });
        let mut later = first.clone();
        later.request_id = "later".into();
        later.plan.id += 1;
        journal.append(later).unwrap();
        let saved = seedbox_artifacts::interventions::read(
            &directory.path().join("checkpoint-saved-interventions.json"),
        )
        .unwrap();
        assert_eq!(saved.entries, vec![first]);
        assert_eq!(
            seedbox_artifacts::interventions::read(&path)
                .unwrap()
                .entries
                .len(),
            2
        );
    }

    fn transaction(
        worker: &mut Worker,
        execute: impl FnMut(Command) -> Result<Reply, ClientError>,
    ) -> Result<ControlState, Error> {
        let request = request();
        let ControlCommand::InjectFault { action } = &request.command else {
            unreachable!()
        };
        Worker::inject_transaction(
            &worker.run,
            &mut worker.journal,
            &mut worker.fault,
            &request,
            action.clone(),
            execute,
        )
    }

    fn prepare() -> Command {
        let ControlCommand::InjectFault { action } = request().command else {
            unreachable!()
        };
        Command::PrepareFault {
            request_id: "cut".into(),
            action,
            timeout_ns: seedbox_control::PROCESS_EFFECT_TIMEOUT_NS,
        }
    }
    fn commit() -> Command {
        Command::CommitFault {
            request_id: "cut".into(),
        }
    }

    #[test]
    fn commit_follows_durable_retention_and_a_timeout_retries_the_same_transaction() {
        let (directory, _socket, mut worker) = fixture();
        let path = directory.path().join("interventions.json");
        let retained = proposal(1).intervention.unwrap();
        let mut commands = Vec::new();
        let first = transaction(&mut worker, |command| {
            commands.push(command.clone());
            if command == prepare() {
                return Ok(proposal(1));
            }
            assert_eq!(command, commit());
            // Check at the exact transport call, before reporting its timeout.
            assert_eq!(
                seedbox_artifacts::interventions::read(&path)
                    .unwrap()
                    .entries
                    .as_slice(),
                std::slice::from_ref(&retained)
            );
            Err(ClientError::Timeout { id: 2 })
        });
        assert!(matches!(
            first,
            Err(Error::Transport(ClientError::Timeout { id: 2 }))
        ));
        let pending = worker.fault.as_ref().unwrap();
        assert_eq!(pending.request, request());
        assert_eq!(pending.retained.as_ref(), Some(&retained));
        let result = transaction(&mut worker, |command| {
            commands.push(command.clone());
            assert_eq!(command, commit());
            assert_eq!(
                seedbox_artifacts::interventions::read(&path)
                    .unwrap()
                    .entries
                    .as_slice(),
                std::slice::from_ref(&retained)
            );
            Ok(proposal(2))
        })
        .unwrap();
        assert!(
            matches!(result, ControlState::Acknowledged { error: None, intervention: Some(ref record), .. } if **record == retained)
        );
        assert!(worker.fault.is_none());
        assert_eq!(commands, [prepare(), commit(), commit()]);
        assert_eq!(
            seedbox_artifacts::interventions::read(&path)
                .unwrap()
                .entries,
            [retained]
        );
    }

    #[test]
    fn a_prepare_timeout_retries_prepare_before_retaining_or_committing() {
        let (directory, _socket, mut worker) = fixture();
        let path = directory.path().join("interventions.json");
        let mut commands = Vec::new();
        assert!(matches!(
            transaction(&mut worker, |command| {
                commands.push(command.clone());
                assert_eq!(command, prepare());
                Err(ClientError::Timeout { id: 1 })
            }),
            Err(Error::Transport(ClientError::Timeout { id: 1 }))
        ));
        assert_eq!(worker.fault.as_ref().unwrap().request, request());
        assert!(worker.fault.as_ref().unwrap().retained.is_none());
        assert!(!path.exists());
        let result = transaction(&mut worker, |command| {
            commands.push(command.clone());
            if command == prepare() {
                assert!(!path.exists());
                return Ok(proposal(1));
            }
            assert_eq!(command, commit());
            assert_eq!(
                seedbox_artifacts::interventions::read(&path)
                    .unwrap()
                    .entries,
                [proposal(1).intervention.unwrap()]
            );
            Ok(proposal(2))
        })
        .unwrap();
        assert!(matches!(
            result,
            ControlState::Acknowledged {
                error: None,
                intervention: Some(_),
                ..
            }
        ));
        assert!(worker.fault.is_none());
        assert_eq!(commands, [prepare(), prepare(), commit()]);
        assert_eq!(
            seedbox_artifacts::interventions::read(&path)
                .unwrap()
                .entries
                .len(),
            1
        );
    }

    #[test]
    fn timed_out_prepare_and_commit_keep_the_exact_client_request() {
        // Zero budget establishes a pending real Client request without any
        // send/read race. Re-entry must retain its full wire identity and refuse
        // an overtaking command, independently of host scheduling.
        for command in [prepare(), commit()] {
            let (_directory, _socket, mut worker) = fixture();
            let client = worker.client().unwrap();
            assert!(matches!(
                client.execute(command.clone(), Duration::ZERO),
                Err(ClientError::Timeout { id: 1 })
            ));
            let pending = client.pending().unwrap().clone();
            assert_eq!(
                pending,
                Request {
                    id: 1,
                    command: command.clone()
                }
            );
            assert!(matches!(
                client.execute(command, Duration::ZERO),
                Err(ClientError::Timeout { id: 1 })
            ));
            assert_eq!(client.pending(), Some(&pending));
            assert!(matches!(
                client.execute(Command::Status, Duration::ZERO),
                Err(ClientError::Pending { id: 1 })
            ));
            assert_eq!(client.pending(), Some(&pending));
        }
    }

    #[test]
    fn a_failed_journal_write_never_sends_commit() {
        let (directory, _socket, mut worker) = fixture();
        let path = directory.path().join("interventions.json");
        let mut commands = Vec::new();
        let result = transaction(&mut worker, |command| {
            commands.push(command.clone());
            assert_eq!(command, prepare());
            // Journal::open has acquired its real lock, but the first admission
            // has not been persisted. Fail atomic replacement at this exact
            // transaction boundary, independently of transport retransmission.
            assert!(path.with_file_name("interventions.json.lock").is_file());
            assert!(!path.exists());
            fs::create_dir(&path).unwrap();
            Ok(proposal(1))
        });
        let Err(Error::Fatal(error)) = result else {
            panic!("failed journal replacement must be fatal: {result:?}");
        };
        assert!(error.contains("persist intervention history"), "{error}");
        // A queued Prepare retransmission is legal transport behavior, not a
        // Commit. Observe every transaction command instead of timing a socket
        // receive and incorrectly requiring that no datagram at all arrives.
        assert!(commands
            .iter()
            .all(|command| !matches!(command, Command::CommitFault { .. })));
        assert_eq!(commands, [prepare()]);
        assert!(path.is_dir());
        assert!(worker
            .journal
            .as_ref()
            .unwrap()
            .history()
            .entries
            .is_empty());
        let pending = worker.fault.as_ref().unwrap();
        assert_eq!(pending.request, request());
        assert!(pending.retained.is_none());
    }
}
