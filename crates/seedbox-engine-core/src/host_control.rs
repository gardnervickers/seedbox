//! Host execution control, checked only between world slices. No guest state.

use crate::intervention::Admission;
pub use seedbox_engine_protocol::host_control::{
    Boundary, BoundaryPosition, CheckpointState, CheckpointStatus, Command, ControlError,
    PlaybackRate, Reply, Request, INVALID, MAX_DATAGRAM, PAUSE, PAUSED, RESUME, RUNNING,
};
use seedbox_engine_protocol::intervention::Intervention;
use std::{
    io,
    os::unix::net::UnixDatagram,
    path::{Path, PathBuf},
    time::Instant,
};

pub struct HostControl {
    socket: UnixDatagram,
    path: PathBuf,
    playback: crate::playback::Playback,
    last_request: Option<(Request, Reply)>,
    wait_clock: Option<seedbox_engine_protocol::host_clock::Writer>,
    waiting: bool,
    prepared: Option<(Command, Intervention)>,
    fault_injection: bool,
    checkpoints_enabled: bool,
    checkpoint: Option<CheckpointStatus>,
    pause_before_next_fault: bool,
    pre_effect: bool,
}

impl HostControl {
    /// Enable only when the owner handles the checkpoint segment outcome and
    /// retains its world snapshot before acknowledging capture completion.
    pub fn enable_checkpoints(&mut self) {
        self.checkpoints_enabled = true;
    }
    pub fn disable_checkpoints(&mut self) {
        self.checkpoints_enabled = false;
    }

    pub fn pending_checkpoint(&self) -> Option<&CheckpointStatus> {
        self.checkpoint
            .as_ref()
            .filter(|c| c.state == CheckpointState::Requested)
    }

    /// Called by the world owner after successful capture, before reentering the
    /// scheduler. Failed capture must terminate execution rather than call this.
    pub fn checkpoint_captured(&mut self, request_id: &str) -> io::Result<()> {
        let checkpoint = self
            .checkpoint
            .as_mut()
            .filter(|c| c.request_id == request_id && c.state == CheckpointState::Requested)
            .ok_or_else(|| io::Error::other("no matching pending checkpoint"))?;
        if self.waiting {
            if let Some(clock) = &mut self.wait_clock {
                clock.end_wait()?;
            }
            self.waiting = false;
        }
        checkpoint.state = CheckpointState::Captured;
        if let Some((_, reply)) = &mut self.last_request {
            reply.checkpoint = Some(checkpoint.clone());
        }
        Ok(())
    }
    /// Replaying retained semantic inputs permits playback changes, but no
    /// additional fault admissions that could shift the recorded fault IDs.
    pub fn disable_fault_injection(&mut self) {
        self.fault_injection = false;
    }

    /// Bind with playback already paused, before any scheduler work is permitted.
    pub fn bind_paused(path: &Path) -> io::Result<Self> {
        let mut control = Self::bind(path)?;
        control.playback.set_rate(
            PlaybackRate::Paused,
            BoundaryPosition::default(),
            Instant::now(),
        );
        Ok(control)
    }

    pub fn bind(path: &Path) -> io::Result<Self> {
        let socket = UnixDatagram::bind(path)?;
        socket.set_nonblocking(true)?;
        let wait_clock = match seedbox_engine_protocol::host_clock::Writer::open(
            &path.with_file_name(seedbox_engine_protocol::host_clock::FILE_NAME),
        ) {
            Ok(clock) => Some(clock),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        Ok(Self {
            wait_clock,
            waiting: false,
            prepared: None,
            fault_injection: true,
            checkpoints_enabled: false,
            checkpoint: None,
            pause_before_next_fault: false,
            pre_effect: false,
            socket,
            path: path.to_owned(),
            playback: Default::default(),
            last_request: None,
        })
    }

    /// Acknowledge after reaching a boundary; stay here until a resume request.
    /// Cancellation remains active; intentional waits are excluded from the execution budget.
    pub fn boundary(&mut self) -> io::Result<()> {
        self.boundary_at(Boundary::default())
    }

    /// A playback-only phase before an effect, reusing the published position.
    /// No fault admission or checkpoint can be made while this stop is held.
    pub fn before_fault(&mut self, boundary: Boundary) -> io::Result<()> {
        if boundary.terminal
            || (!self.pause_before_next_fault && self.playback.rate() != PlaybackRate::Paused)
        {
            return Ok(());
        }
        if self.pause_before_next_fault {
            self.pause_before_next_fault = false;
            self.playback
                .set_rate(PlaybackRate::Paused, boundary.position, Instant::now());
        }
        // Initially paused runs may reach an input at the launch boundary
        // before their first regular host callback. Hold its effect here too.
        // Preserve an admitted step so it still permits exactly one round.
        self.pre_effect = true;
        let result = self.boundary_scope(boundary, None);
        self.pre_effect = false;
        result
    }

    pub fn boundary_at(&mut self, boundary: Boundary) -> io::Result<()> {
        self.boundary_scope(boundary, None)
    }

    pub fn boundary_with_admission(
        &mut self,
        boundary: Boundary,
        admission: &mut Admission<'_>,
    ) -> io::Result<()> {
        self.boundary_scope(boundary, Some(admission))
    }

    fn boundary_scope(
        &mut self,
        boundary: Boundary,
        admission: Option<&mut Admission<'_>>,
    ) -> io::Result<()> {
        let result = self.boundary_inner(boundary, admission);
        let accounting = if self.waiting && self.pending_checkpoint().is_none() {
            self.waiting = false;
            self.wait_clock
                .as_mut()
                .map_or(Ok(()), |clock| clock.end_wait())
        } else {
            Ok(())
        };
        result.and(accounting)
    }

    fn boundary_inner(
        &mut self,
        boundary: Boundary,
        mut admission: Option<&mut Admission<'_>>,
    ) -> io::Result<()> {
        let position = boundary.position;
        if boundary.terminal {
            self.playback.finish();
        }
        loop {
            // Settle an admitted step before answering status at this position.
            self.playback.delay(position, Instant::now());
            // Bound each drain so senders cannot indefinitely hold a running world.
            for _ in 0..64 {
                let mut bytes = [0u8; MAX_DATAGRAM + 1];
                let (size, peer) = match self.socket.recv_from(&mut bytes) {
                    Ok(message) => message,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                };
                let Some(peer) = peer.as_pathname() else {
                    continue;
                };
                let reply = match &bytes[..size] {
                    PAUSE if !boundary.terminal => {
                        self.playback
                            .set_rate(PlaybackRate::Paused, position, Instant::now());
                        PAUSED.to_vec()
                    }
                    RESUME if !boundary.terminal => {
                        self.pause_before_next_fault = false;
                        self.playback
                            .set_rate(PlaybackRate::Unlimited, position, Instant::now());
                        RUNNING.to_vec()
                    }
                    bytes if size <= MAX_DATAGRAM => {
                        match serde_json::from_slice::<Request>(bytes) {
                            Ok(request) => serde_json::to_vec(&self.process_request(
                                request,
                                boundary,
                                admission.as_deref_mut(),
                            )?)
                            .map_err(io::Error::other)?,
                            Err(_) => INVALID.to_vec(),
                        }
                    }
                    _ => INVALID.to_vec(),
                };
                // A disappearing requester must never fail the guest execution.
                let _ = self.socket.send_to(&reply, peer);
                // Drain this action through the scheduler before accepting a
                // dependent same-time host action, just like timed overrides.
                if admission.as_ref().is_some_and(|scope| scope.changed()) {
                    return Ok(());
                }
                // Release the borrowed admission scope so the owner can capture
                // the whole world. Even a paused simulation must reach this path.
                if self.pending_checkpoint().is_some() {
                    if !self.waiting {
                        if let Some(clock) = &mut self.wait_clock {
                            clock.begin_wait()?;
                        }
                        self.waiting = true;
                    }
                    return Ok(());
                }
            }
            if boundary.terminal {
                return Ok(());
            }
            let delay = if self.prepared.is_some() {
                Some(std::time::Duration::from_millis(2))
            } else {
                self.playback.delay(position, Instant::now())
            };
            match delay {
                None => return Ok(()),
                Some(delay) => {
                    if !self.waiting {
                        if let Some(clock) = &mut self.wait_clock {
                            clock.begin_wait()?;
                        }
                        self.waiting = true;
                    }
                    std::thread::sleep(delay);
                }
            }
        }
    }

    #[cfg(test)]
    fn request(&mut self, request: Request, boundary: Boundary) -> Reply {
        self.process_request(request, boundary, None).unwrap()
    }

    fn process_request(
        &mut self,
        request: Request,
        boundary: Boundary,
        admission: Option<&mut Admission<'_>>,
    ) -> io::Result<Reply> {
        let position = boundary.position;
        let mut error = None;
        let mut detail = None;
        let mut intervention = None;
        if let Some((previous, reply)) = &self.last_request {
            if request == *previous {
                return Ok(reply.clone());
            }
            if request.id == previous.id {
                error = Some(ControlError::RequestConflict);
            }
            if request.id < previous.id {
                error = Some(ControlError::StaleRequest);
            }
        }
        if error.is_none() && boundary.terminal && request.command != Command::Status {
            error = Some(ControlError::Completed);
        }
        if error.is_none()
            && self.pending_checkpoint().is_some()
            && !matches!(
                request.command,
                Command::Status | Command::Checkpoint { .. }
            )
        {
            error = Some(ControlError::CheckpointPending);
        }
        if error.is_none()
            && self.pre_effect
            && matches!(
                request.command,
                Command::Checkpoint { .. }
                    | Command::PrepareFault { .. }
                    | Command::CommitFault { .. }
            )
        {
            error = Some(ControlError::Unsupported);
            detail = Some("only playback controls are available before a fault effect".into());
        }
        if error.is_none() {
            match &request.command {
                Command::Status => {
                    intervention = self.prepared.as_ref().map(|(_, record)| record.clone());
                }
                Command::SetRate { rate } => {
                    self.pause_before_next_fault = false;
                    self.playback.set_rate(*rate, position, Instant::now())
                }
                Command::PauseBeforeNextFault { rate } => {
                    if *rate == PlaybackRate::Paused {
                        error = Some(ControlError::InvalidRate);
                        detail = Some(
                            "choose a running playback rate for pause before next fault".into(),
                        );
                    } else if self.prepared.is_some() {
                        error = Some(ControlError::FaultPending);
                    } else {
                        self.pause_before_next_fault = true;
                        self.playback.set_rate(*rate, position, Instant::now());
                    }
                }
                Command::Step => {
                    error = self.playback.step(position).err();
                    if error.is_none() {
                        self.pause_before_next_fault = false;
                    }
                }
                Command::Checkpoint { request_id } => {
                    if self
                        .checkpoint
                        .as_ref()
                        .is_some_and(|c| c.request_id == *request_id)
                    {
                        // An admitted request remains retryable when capture fills
                        // the owner's retention capacity. Never recapture it.
                    } else if !self.checkpoints_enabled {
                        error = Some(ControlError::Unsupported);
                    } else if self.prepared.is_some() {
                        error = Some(ControlError::FaultPending);
                    } else if request_id.is_empty()
                        || request_id.len() > 128
                        || !request_id
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
                    {
                        error = Some(ControlError::InvalidCheckpoint);
                    } else if self.pending_checkpoint().is_some() {
                        error = Some(ControlError::CheckpointPending);
                    } else if let Some(admission) = admission {
                        match admission.request_checkpoint() {
                            Ok(()) => {
                                self.checkpoint = Some(CheckpointStatus {
                                    request_id: request_id.clone(),
                                    position,
                                    state: CheckpointState::Requested,
                                })
                            }
                            Err(message) => {
                                error = Some(ControlError::InvalidCheckpoint);
                                detail = Some(bounded_detail(message));
                            }
                        }
                    } else {
                        error = Some(ControlError::Unsupported);
                    }
                }
                Command::PrepareFault {
                    request_id,
                    action,
                    timeout_ns,
                } => {
                    if !self.fault_injection {
                        error = Some(ControlError::Unsupported);
                        detail = Some(
                            "fault injection is disabled while replaying retained interventions"
                                .into(),
                        );
                    } else if let Some((command, record)) = &self.prepared {
                        if command == &request.command {
                            intervention = Some(record.clone());
                        } else {
                            error = Some(ControlError::FaultPending);
                        }
                    } else if let Some(admission) = admission {
                        // Initial launch replaces boot fault policy when arming
                        // the scenario. Do not admit interactive inputs there.
                        if !boundary.application_ready {
                            error = Some(ControlError::NotReady);
                        } else {
                            let at = admission.earliest_effect_ns();
                            let proposal = at
                                .checked_add(*timeout_ns)
                                .ok_or_else(|| "fault deadline overflow".to_owned())
                                .and_then(|deadline_ns| {
                                    admission.propose(
                                        request_id.clone(),
                                        seedbox_control::Plan {
                                            id: 0,
                                            at_ns: at,
                                            deadline_ns,
                                            action: action.clone(),
                                        },
                                    )
                                })
                                .and_then(|record| {
                                    // Leave room for the status envelope in the
                                    // bounded reply, before holding the world.
                                    if serde_json::to_vec(&record)
                                        .map_err(|e| e.to_string())?
                                        .len()
                                        > MAX_DATAGRAM / 2
                                    {
                                        Err("fault proposal exceeds the control transport limit"
                                            .into())
                                    } else {
                                        Ok(record)
                                    }
                                });
                            match proposal {
                                Ok(record) => {
                                    self.prepared = Some((request.command.clone(), record.clone()));
                                    intervention = Some(record);
                                }
                                Err(message) => {
                                    error = Some(ControlError::InvalidFault);
                                    detail = Some(bounded_detail(message));
                                }
                            }
                        }
                    } else {
                        error = Some(ControlError::Unsupported);
                    }
                }
                Command::CommitFault { request_id } => {
                    if let Some((_, record)) = &self.prepared {
                        if &record.request_id != request_id {
                            error = Some(ControlError::RequestConflict);
                        } else if let Some(admission) = admission {
                            // A retained proposal that cannot commit is fatal:
                            // never advance past a missing semantic input.
                            admission.commit(record).map_err(io::Error::other)?;
                            intervention = Some(record.clone());
                            self.prepared = None;
                        } else {
                            error = Some(ControlError::Unsupported);
                        }
                    } else {
                        error = Some(ControlError::NoFaultPrepared);
                    }
                }
            }
        }
        let reply = Reply {
            id: request.id,
            position,
            terminal: boundary.terminal,
            application_ready: boundary.application_ready,
            rate: self.playback.rate(),
            achieved_rate: self.playback.achieved_rate(position, Instant::now()),
            stepping: self.playback.stepping(),
            intervention,
            checkpoint: self.checkpoint.clone(),
            detail,
            error,
        };
        if !matches!(
            error,
            Some(ControlError::RequestConflict | ControlError::StaleRequest)
        ) {
            self.last_request = Some((request, reply.clone()));
        }
        Ok(reply)
    }
}

fn bounded_detail(mut message: String) -> String {
    let mut end = message.len().min(512);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    message
}

impl Drop for HostControl {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_fault_waits_for_commit_and_retries_do_not_duplicate() {
        use seedbox_control::Action;
        use std::{sync::mpsc, time::Duration};
        let directory = tempfile::tempdir().unwrap();
        let endpoint = directory.path().join("control");
        let mut control = HostControl::bind(&endpoint).unwrap();
        let client = UnixDatagram::bind(directory.path().join("client")).unwrap();
        client.connect(&endpoint).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        // A failed assertion must release the scoped worker before joining it.
        struct Release(UnixDatagram);
        impl Drop for Release {
            fn drop(&mut self) {
                let commit = Request {
                    id: u64::MAX,
                    command: Command::CommitFault {
                        request_id: "cut".into(),
                    },
                };
                let _ = self.0.send(&serde_json::to_vec(&commit).unwrap());
                let _ = self.0.send(RESUME);
            }
        }
        let cleanup = UnixDatagram::bind(directory.path().join("cleanup")).unwrap();
        cleanup.connect(&endpoint).unwrap();
        cleanup.set_nonblocking(true).unwrap();
        let boundary = Boundary {
            position: BoundaryPosition {
                boundary_index: 8,
                completed_rounds: 3,
                simulated_time_ns: 100,
            },
            application_ready: true,
            terminal: false,
        };
        let prepare = Command::PrepareFault {
            request_id: "cut".into(),
            action: Action::Partition {
                handle: "cut".into(),
                left: vec!["a".into()],
                right: vec!["b".into()],
            },
            timeout_ns: 20,
        };
        // Queue the first request before starting a freely running boundary.
        client
            .send(
                &serde_json::to_vec(&Request {
                    id: 1,
                    command: prepare.clone(),
                })
                .unwrap(),
            )
            .unwrap();
        let (done, finished) = mpsc::channel();
        std::thread::scope(|scope| {
            let worker = scope.spawn(move || {
                let mut faults =
                    crate::faults::Faults::new(vec![], vec!["a".into(), "b".into()]).unwrap();
                let mut admission = Admission::new(&mut faults, boundary, &|_| Ok(()));
                control
                    .boundary_with_admission(boundary, &mut admission)
                    .unwrap();
                control.before_fault(boundary).unwrap();
                done.send(()).unwrap();
                faults
            });
            let _release = Release(cleanup);
            let receive = |id| {
                let mut bytes = [0; MAX_DATAGRAM + 1];
                let size = client.recv(&mut bytes).unwrap();
                assert!(size <= MAX_DATAGRAM);
                let reply: Reply = serde_json::from_slice(&bytes[..size]).unwrap();
                assert_eq!(reply.id, id);
                reply
            };
            let send = |request: &Request| {
                client.send(&serde_json::to_vec(request).unwrap()).unwrap();
                receive(request.id)
            };
            let prepared = receive(1);
            assert_eq!(prepared.error, None);
            let record = prepared.intervention.unwrap();
            assert_eq!(record.position, boundary.position);
            assert_eq!(
                (record.plan.id, record.plan.at_ns, record.plan.deadline_ns),
                (1, 100, 120)
            );
            let status = send(&Request {
                id: 2,
                command: Command::Status,
            });
            assert_eq!(status.intervention, Some(record.clone()));
            assert_eq!(finished.try_recv(), Err(mpsc::TryRecvError::Empty));
            let retry = send(&Request {
                id: 3,
                command: prepare,
            });
            assert_eq!(retry.intervention, Some(record.clone()));
            let pause = send(&Request {
                id: 4,
                command: Command::SetRate {
                    rate: PlaybackRate::Paused,
                },
            });
            assert_eq!(pause.error, None);
            let commit = Request {
                id: 5,
                command: Command::CommitFault {
                    request_id: "cut".into(),
                },
            };
            let committed = send(&commit);
            assert_eq!(committed.error, None);
            assert_eq!(committed.intervention, Some(record));
            assert_eq!(send(&commit), committed);
            assert_eq!(finished.try_recv(), Err(mpsc::TryRecvError::Empty));
            let status = send(&Request {
                id: 6,
                command: Command::Status,
            });
            assert_eq!(status.intervention, None);
            send(&Request {
                id: 7,
                command: Command::SetRate {
                    rate: PlaybackRate::Unlimited,
                },
            });
            finished.recv_timeout(Duration::from_secs(2)).unwrap();
            let faults = worker.join().unwrap();
            assert_eq!(faults.log.len(), 1);
            assert_eq!(faults.log[0].id, 1);
        });
    }

    /// NEXT-100 case 97, engine arm: a requested checkpoint whose capture
    /// fails is never acknowledged. Publication failing before, during or after
    /// writing, and the second machine of a two-machine world failing its
    /// snapshot, all leave the request pending and nothing held; a retried
    /// status still reports Requested. A run that then ends refuses the
    /// checkpoint as completed instead of fabricating it.
    #[test]
    fn failed_capture_is_never_acknowledged_and_completion_fabricates_nothing() {
        use std::{io::Write, time::Duration};
        let directory = tempfile::tempdir().unwrap();
        let endpoint = directory.path().join("control");
        let mut control = HostControl::bind(&endpoint).unwrap();
        control.enable_checkpoints();
        let boundary = Boundary::default();
        let client = UnixDatagram::bind(directory.path().join("client")).unwrap();
        client.connect(&endpoint).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let request = Request {
            id: 1,
            command: Command::Checkpoint {
                request_id: "saved".into(),
            },
        };
        client.send(&serde_json::to_vec(&request).unwrap()).unwrap();
        let mut faults = crate::faults::Faults::new(vec![], vec!["a".into(), "b".into()]).unwrap();
        let mut admission =
            Admission::new(&mut faults, boundary, &|_| Ok(())).with_checkpoint_support(true);
        control
            .boundary_with_admission(boundary, &mut admission)
            .unwrap();
        assert!(admission.checkpoint_requested());
        let mut buffer = [0; MAX_DATAGRAM];
        let len = client.recv(&mut buffer).unwrap();
        let accepted: Reply = serde_json::from_slice(&buffer[..len]).unwrap();
        assert_eq!(
            accepted.checkpoint.unwrap().state,
            CheckpointState::Requested
        );

        let mut world = crate::world_mock::world(&["a", "b"]);
        let replay = crate::intervention::Replay::new(Default::default()).unwrap();
        let mut owner = crate::checkpoint::LiveCheckpoints::default();
        let still_requested = |control: &mut HostControl, id| {
            let retry = control.request(
                Request {
                    id,
                    command: request.command.clone(),
                },
                boundary,
            );
            assert_eq!(retry.error, None);
            assert_eq!(retry.checkpoint.unwrap().state, CheckpointState::Requested);
            assert!(control.pending_checkpoint().is_some());
        };
        let target = directory.path().join("checkpoint-saved.json");
        let attempts: [(&str, Box<dyn Fn(&str) -> Result<(), String>>); 3] = [
            (
                "before writing",
                Box::new(|_| Err("no space before writing".into())),
            ),
            (
                "while writing",
                Box::new(|id: &str| {
                    let mut file = std::fs::File::create(&target).unwrap();
                    file.write_all(id.as_bytes()).unwrap();
                    Err("short write".into())
                }),
            ),
            (
                "after writing",
                Box::new(|id: &str| {
                    std::fs::write(&target, id).unwrap();
                    Err("rename failed after writing".into())
                }),
            ),
        ];
        for (index, (stage, publish)) in attempts.iter().enumerate() {
            let error = owner
                .capture_pending_with(&mut world, &replay, &mut control, |id, _| publish(id))
                .unwrap_err();
            assert!(!error.is_empty(), "{stage}");
            assert!(owner.get("saved").is_none(), "{stage}: held after failure");
            still_requested(&mut control, 10 + index as u64);
        }
        // The second machine's snapshot fails after the first one's succeeded.
        world.factory.lock().unwrap().push("fail/snapshot/b".into());
        let mut published = false;
        let error = owner
            .capture_pending_with(&mut world, &replay, &mut control, |_, _| {
                published = true;
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error, "b snapshot failed");
        assert!(!published, "a partial world was published");
        assert!(owner.get("saved").is_none());
        still_requested(&mut control, 20);

        // The run ends with the request still pending: no capture, no
        // checkpoint, and later requests are refused as completed.
        let terminal = Boundary {
            terminal: true,
            ..boundary
        };
        let late = control.request(
            Request {
                id: 30,
                command: request.command.clone(),
            },
            terminal,
        );
        assert_eq!(late.error, Some(ControlError::Completed));
        assert!(late
            .checkpoint
            .is_none_or(|status| status.state == CheckpointState::Requested));
        assert!(owner.get("saved").is_none());
        assert!(control.checkpoint_captured("other").is_err());
    }

    #[test]
    fn checkpoint_acceptance_releases_paused_boundary_and_completion_is_explicit() {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = directory.path().join("control");
        let mut control = HostControl::bind(&endpoint).unwrap();
        let boundary = Boundary::default();
        let command = Command::Checkpoint {
            request_id: "saved-position".into(),
        };
        assert_eq!(
            control
                .request(
                    Request {
                        id: 1,
                        command: command.clone()
                    },
                    boundary
                )
                .error,
            Some(ControlError::Unsupported)
        );
        control.enable_checkpoints();
        control.request(
            Request {
                id: 2,
                command: Command::SetRate {
                    rate: PlaybackRate::Paused,
                },
            },
            boundary,
        );
        let client = UnixDatagram::bind(directory.path().join("client")).unwrap();
        client.connect(&endpoint).unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let request = Request { id: 3, command };
        client.send(&serde_json::to_vec(&request).unwrap()).unwrap();
        let mut faults = crate::faults::Faults::new(vec![], vec!["a".into()]).unwrap();
        let mut admission =
            Admission::new(&mut faults, boundary, &|_| Ok(())).with_checkpoint_support(true);
        control
            .boundary_with_admission(boundary, &mut admission)
            .unwrap();
        assert!(admission.checkpoint_requested());
        let mut buffer = [0; MAX_DATAGRAM];
        let len = client.recv(&mut buffer).unwrap();
        let accepted: Reply = serde_json::from_slice(&buffer[..len]).unwrap();
        assert_eq!(accepted.error, None);
        assert_eq!(accepted.rate, PlaybackRate::Paused);
        assert_eq!(
            accepted.checkpoint.unwrap().state,
            CheckpointState::Requested
        );
        assert!(control.checkpoint_captured("wrong-request").is_err());
        assert!(control.pending_checkpoint().is_some());
        assert_eq!(
            control
                .request(
                    Request {
                        id: 4,
                        command: Command::Step
                    },
                    boundary
                )
                .error,
            Some(ControlError::CheckpointPending)
        );
        let mut world = crate::world_mock::world(&["a"]);
        let replay = crate::intervention::Replay::new(Default::default()).unwrap();
        let mut owner = crate::checkpoint::LiveCheckpoints::default();
        assert!(owner
            .capture_pending_with(&mut world, &replay, &mut control, |_, _| {
                Err("storage failure".into())
            })
            .unwrap_err()
            .contains("storage failure"));
        assert!(control.pending_checkpoint().is_some());
        assert!(owner.get("saved-position").is_none());
        assert!(owner
            .capture_pending_with(&mut world, &replay, &mut control, |id, saved| {
                assert_eq!(id, "saved-position");
                assert_eq!(saved.position, boundary.position);
                assert!(!serde_json::to_vec(saved).unwrap().is_empty());
                Ok(())
            })
            .unwrap());
        assert!(control.pending_checkpoint().is_none());
        // Capturing the last available slot disables new admissions, but an
        // acknowledgement lost in transport must still be recoverable.
        control.disable_checkpoints();
        let retry = control.request(
            Request {
                id: 5,
                command: request.command,
            },
            boundary,
        );
        assert_eq!(retry.error, None);
        let captured = retry.checkpoint.unwrap();
        assert_eq!(captured.state, CheckpointState::Captured);
        assert_eq!(captured.position, boundary.position);
        assert_eq!(retry.rate, PlaybackRate::Paused);
        assert_eq!(
            control
                .request(
                    Request {
                        id: 6,
                        command: Command::Checkpoint {
                            request_id: "another-position".into(),
                        },
                    },
                    boundary,
                )
                .error,
            Some(ControlError::Unsupported)
        );
    }

    #[test]
    fn paused_start_holds_the_initial_boundary_until_resumed() {
        use std::{sync::mpsc, time::Duration};
        let directory = tempfile::tempdir().unwrap();
        let endpoint = directory.path().join("control");
        let mut control = HostControl::bind_paused(&endpoint).unwrap();
        let client = UnixDatagram::bind(directory.path().join("client")).unwrap();
        client.connect(endpoint).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let (done, completed) = mpsc::channel();
        // Always resume before joining, including on a failed assertion.
        struct Resume<'a>(&'a UnixDatagram);
        impl Drop for Resume<'_> {
            fn drop(&mut self) {
                let _ = self.0.send(RESUME);
            }
        }
        std::thread::scope(|scope| {
            scope.spawn(move || {
                control.boundary_at(Boundary::default()).unwrap();
                done.send(()).unwrap();
            });
            let release = Resume(&client);
            let request = Request {
                id: 1,
                command: Command::Status,
            };
            client.send(&serde_json::to_vec(&request).unwrap()).unwrap();
            let mut bytes = [0; MAX_DATAGRAM];
            let count = client.recv(&mut bytes).unwrap();
            let status: Reply = serde_json::from_slice(&bytes[..count]).unwrap();
            assert_eq!(status.rate, PlaybackRate::Paused);
            assert_eq!(status.position.completed_rounds, 0);
            assert_eq!(status.position.simulated_time_ns, 0);
            assert!(completed.recv_timeout(Duration::from_millis(50)).is_err());
            drop(release);
            completed.recv_timeout(Duration::from_secs(2)).unwrap();
        });
    }

    #[test]
    fn a_terminal_boundary_finishes_even_when_playback_is_paused() {
        let directory = tempfile::tempdir().unwrap();
        let mut control = HostControl::bind(&directory.path().join("control")).unwrap();
        let mut boundary = Boundary::default();
        control.request(
            Request {
                id: 1,
                command: Command::SetRate {
                    rate: PlaybackRate::Paused,
                },
            },
            boundary,
        );
        control.request(
            Request {
                id: 2,
                command: Command::Step,
            },
            boundary,
        );
        boundary.position.completed_rounds = 1;
        boundary.terminal = true;
        control.boundary_at(boundary).unwrap();
        let status = control.request(
            Request {
                id: 3,
                command: Command::Status,
            },
            boundary,
        );
        assert!(status.terminal);
        assert!(!status.stepping);
        assert_eq!(
            control
                .request(
                    Request {
                        id: 4,
                        command: Command::Step
                    },
                    boundary
                )
                .error,
            Some(ControlError::Completed)
        );
    }

    #[test]
    fn retrying_an_admitted_step_never_advances_again() {
        let directory = tempfile::tempdir().unwrap();
        let mut control = HostControl::bind(&directory.path().join("control")).unwrap();
        let initial = Boundary {
            position: BoundaryPosition {
                boundary_index: 3,
                completed_rounds: 3,
                simulated_time_ns: 100,
            },
            terminal: false,
            application_ready: false,
        };
        let next = Boundary {
            position: BoundaryPosition {
                boundary_index: 4,
                completed_rounds: 4,
                simulated_time_ns: 110,
            },
            terminal: false,
            application_ready: false,
        };
        control.request(
            Request {
                id: 1,
                command: Command::SetRate {
                    rate: PlaybackRate::Paused,
                },
            },
            initial,
        );
        let step = Request {
            id: 2,
            command: Command::Step,
        };
        let accepted = control.request(step.clone(), initial);
        assert!(accepted.stepping);
        assert_eq!(accepted.error, None);
        assert!(control
            .playback
            .delay(next.position, Instant::now())
            .is_some());
        assert_eq!(control.request(step.clone(), next), accepted);
        assert!(!control.playback.stepping());
        let status = control.request(
            Request {
                id: 3,
                command: Command::Status,
            },
            next,
        );
        assert_eq!(status.position, next.position);
        assert!(!status.stepping);
        assert_eq!(
            control.request(step, next).error,
            Some(ControlError::StaleRequest)
        );
        assert_eq!(
            control
                .request(
                    Request {
                        id: 3,
                        command: Command::Step
                    },
                    next
                )
                .error,
            Some(ControlError::RequestConflict)
        );
        assert!(!control.playback.stepping());
    }
}

#[cfg(test)]
mod pre_fault_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn one_shot_stop_preserves_position_allows_playback_and_rejects_semantic_controls() {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = directory.path().join("control");
        let mut control = HostControl::bind(&endpoint).unwrap();
        control.enable_checkpoints();
        let boundary = Boundary {
            position: BoundaryPosition {
                boundary_index: 8,
                completed_rounds: 3,
                simulated_time_ns: 100,
            },
            application_ready: true,
            terminal: false,
        };
        let armed = control.request(
            Request {
                id: 1,
                command: Command::PauseBeforeNextFault {
                    rate: PlaybackRate::Unlimited,
                },
            },
            boundary,
        );
        assert_eq!(armed.error, None);
        assert!(control.pause_before_next_fault);
        let client = UnixDatagram::bind(directory.path().join("client")).unwrap();
        client.connect(&endpoint).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        struct Release(UnixDatagram);
        impl Drop for Release {
            fn drop(&mut self) {
                let _ = self.0.send(RESUME);
            }
        }
        let cleanup = UnixDatagram::bind(directory.path().join("cleanup")).unwrap();
        cleanup.connect(&endpoint).unwrap();
        std::thread::scope(|scope| {
            let _release = Release(cleanup);
            let task = scope.spawn(|| {
                control.before_fault(boundary).unwrap();
                assert!(!control.pause_before_next_fault);
                // A consumed stop never pauses again without being rearmed.
                control.before_fault(boundary).unwrap();
            });
            let send = |id, command| {
                client
                    .send(&serde_json::to_vec(&Request { id, command }).unwrap())
                    .unwrap();
                let mut bytes = [0; MAX_DATAGRAM];
                let count = client.recv(&mut bytes).unwrap();
                serde_json::from_slice::<Reply>(&bytes[..count]).unwrap()
            };
            let paused = send(2, Command::Status);
            assert_eq!(paused.rate, PlaybackRate::Paused);
            assert_eq!(paused.position, boundary.position);
            let rejected = send(
                3,
                Command::Checkpoint {
                    request_id: "not-here".into(),
                },
            );
            assert_eq!(rejected.error, Some(ControlError::Unsupported));
            let resumed = send(
                4,
                Command::SetRate {
                    rate: PlaybackRate::Unlimited,
                },
            );
            assert_eq!(resumed.error, None);
            assert_eq!(resumed.position, boundary.position);
            task.join().unwrap();
        });
    }

    #[test]
    fn arming_requires_running_rate_and_explicit_rate_changes_cancel_the_arm() {
        let directory = tempfile::tempdir().unwrap();
        let mut control = HostControl::bind(&directory.path().join("control")).unwrap();
        let boundary = Boundary {
            position: BoundaryPosition::default(),
            application_ready: true,
            terminal: false,
        };
        assert_eq!(
            control
                .request(
                    Request {
                        id: 1,
                        command: Command::PauseBeforeNextFault {
                            rate: PlaybackRate::Paused
                        }
                    },
                    boundary
                )
                .error,
            Some(ControlError::InvalidRate)
        );
        assert!(!control.pause_before_next_fault);
        control.request(
            Request {
                id: 2,
                command: Command::PauseBeforeNextFault {
                    rate: PlaybackRate::Unlimited,
                },
            },
            boundary,
        );
        assert!(control.pause_before_next_fault);
        control.request(
            Request {
                id: 3,
                command: Command::SetRate {
                    rate: PlaybackRate::Paused,
                },
            },
            boundary,
        );
        assert!(!control.pause_before_next_fault);
    }
}

#[cfg(test)]
mod override_order_tests {
    use super::*;
    #[test]
    fn queued_live_actions_drain_each_effect_before_next_proposal() {
        use seedbox_control::Action;
        let directory = tempfile::tempdir().unwrap();
        let endpoint = directory.path().join("control");
        let mut host = HostControl::bind(&endpoint).unwrap();
        let client = UnixDatagram::bind(directory.path().join("client")).unwrap();
        client.connect(&endpoint).unwrap();
        let actions = [
            Action::Partition {
                handle: "cut".into(),
                left: vec!["a".into()],
                right: vec!["b".into()],
            },
            Action::Heal {
                handle: "cut".into(),
            },
        ];
        for (index, action) in actions.into_iter().enumerate() {
            let request_id = format!("action-{index}");
            for (id, command) in [
                (
                    index as u64 * 2 + 1,
                    Command::PrepareFault {
                        request_id: request_id.clone(),
                        action,
                        timeout_ns: 100,
                    },
                ),
                (index as u64 * 2 + 2, Command::CommitFault { request_id }),
            ] {
                client
                    .send(&serde_json::to_vec(&Request { id, command }).unwrap())
                    .unwrap();
            }
        }
        let boundary = Boundary {
            position: BoundaryPosition {
                boundary_index: 1,
                completed_rounds: 0,
                simulated_time_ns: 10,
            },
            terminal: false,
            application_ready: true,
        };
        let mut faults = crate::faults::Faults::new(vec![], vec!["a".into(), "b".into()]).unwrap();
        let mut program = crate::input_program::Program::default();
        for count in 1..=2 {
            let mut scope =
                Admission::new(&mut faults, boundary, &|_| Ok(())).with_program(&mut program);
            host.boundary_with_admission(boundary, &mut scope).unwrap();
            assert_eq!(faults.log.len(), count);
            faults.apply_due(10);
        }
        assert!(faults.active_handles().is_empty());
        assert!(faults.error.is_none());
        assert_eq!(
            program
                .admissions()
                .iter()
                .map(|row| row.key.as_str())
                .collect::<Vec<_>>(),
            ["action-0", "action-1"]
        );
    }
}
