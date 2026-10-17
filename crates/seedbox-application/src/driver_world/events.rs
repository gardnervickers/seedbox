//! Committed Driver publications become service observations and result evidence.
//! Transport loss never changes witness state. A restored branch starts a new
//! transport stream but continues the logical publication and evidence cursors.
use crate::{Error, Result};
use seedbox_driver::driver::{
    self as driver, Event, EventKind, FaultAction, FaultOrigin, FaultRecord, FaultTarget,
    MachineId, SettlementStatus, Transition,
};
use seedbox_engine_protocol::{
    application_log,
    driver_world::State,
    observation::{self, Kind, MachineTransition},
    report::RowStatus,
};
use seedbox_machine::pv::{ApplicationCommand, CommandResult, Response, Stream};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, os::unix::net::UnixDatagram, path::Path};

fn invalid(message: impl Into<String>) -> Error {
    Error::Integrity(message.into())
}

pub(super) struct Publisher {
    socket: Option<UnixDatagram>,
    sequence: u64,
}
impl Publisher {
    pub(super) fn connect(path: Option<&Path>) -> Result<Self> {
        let socket = path
            .map(|path| -> std::io::Result<_> {
                let socket = UnixDatagram::unbound()?;
                observation::configure_socket(&socket)?;
                socket.connect(path)?;
                socket.set_nonblocking(true)?;
                Ok(socket)
            })
            .transpose()?;
        Ok(Self {
            socket,
            sequence: 0,
        })
    }
    fn emit(
        &mut self,
        state: &mut State,
        machine: &str,
        boot: u64,
        at: u64,
        kind: Kind,
    ) -> Result<()> {
        let incarnation = u32::try_from(boot)
            .map_err(|_| invalid("Driver boot exceeds observation incarnation field"))?;
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| invalid("Driver observation sequence exhausted"))?;
        state.world.publication.observation_records = state
            .world
            .publication
            .observation_records
            .checked_add(1)
            .ok_or_else(|| invalid("Driver logical observation count exhausted"))?;
        if let Some(socket) = &self.socket {
            let record = observation::Observation {
                sequence: self.sequence,
                machine: machine.into(),
                incarnation,
                simulated_time_ns: at,
                kind,
            };
            let bytes = serde_json::to_vec(&record)?;
            if bytes.len() <= observation::MAX_DATAGRAM_BYTES {
                let _ = socket.send(&bytes);
            }
            // Oversize or unavailable transport consumes its sequence. The
            // receiver reports gaps; no retry or evidence mutation depends on it.
        }
        Ok(())
    }
    /// Publish the separate bounded routing-fact stream, never EventKind or
    /// event-chain evidence. Lost facts reserve sequence slots so the existing
    /// receiver reports SourceGap, including when the next record is Closed.
    pub(super) fn frames(
        &mut self,
        state: &mut State,
        names: &BTreeMap<MachineId, String>,
        facts: driver::FrameFacts,
    ) -> Result<()> {
        if self.socket.is_none() {
            return Ok(());
        }
        let mut batch = Vec::new();
        let mut at = state.world.position.simulated_time_ns;
        for fact in facts.facts {
            at = fact.position.at.as_nanos();
            batch.push(observation::Frame {
                from: name(names, fact.from)?.into(),
                to: name(names, fact.to)?.into(),
                sent_ns: fact.sent.as_nanos(),
                due_ns: fact.due.map(|due| due.as_nanos()),
                bytes: u32::try_from(fact.bytes).unwrap_or(u32::MAX),
                fate: match fact.fate {
                    driver::FrameFate::Scheduled => observation::FrameFate::Scheduled,
                    driver::FrameFate::PartitionDropped => observation::FrameFate::PartitionDropped,
                    driver::FrameFate::LossDropped => observation::FrameFate::LossDropped,
                    driver::FrameFate::QueueDropped => observation::FrameFate::QueueDropped,
                    driver::FrameFate::ReceiverDropped => observation::FrameFate::ReceiverDropped,
                },
            });
            // Small bounded datagrams; oversized names or a full socket still
            // consume a sequence in emit(), never block or silently retry.
            if batch.len() == 16 {
                self.emit(
                    state,
                    "",
                    0,
                    at,
                    Kind::Frames {
                        frames: std::mem::take(&mut batch),
                    },
                )?;
            }
        }
        if !batch.is_empty() {
            self.emit(state, "", 0, at, Kind::Frames { frames: batch })?;
        }
        if facts.omitted != 0 {
            self.sequence = self
                .sequence
                .checked_add(facts.omitted)
                .ok_or_else(|| invalid("Driver frame observation sequence exhausted"))?;
            state.world.publication.observation_records = state
                .world
                .publication
                .observation_records
                .checked_add(facts.omitted)
                .ok_or_else(|| invalid("Driver logical frame observation count exhausted"))?;
            // Publish a watermark immediately, even if no later machine output
            // follows. The receiver reports the missing observation range.
            self.emit(state, "", 0, at, Kind::Frames { frames: vec![] })?;
        }
        Ok(())
    }
    pub(super) fn event_gap(&mut self, state: &mut State, omitted: u64, next: u64) -> Result<()> {
        if omitted != 0 && self.socket.is_some() {
            self.sequence = self
                .sequence
                .checked_add(omitted)
                .ok_or_else(|| invalid("Driver event observation sequence exhausted"))?;
            state.world.publication.observation_records = state
                .world
                .publication
                .observation_records
                .checked_add(omitted)
                .ok_or_else(|| invalid("Driver event observation count exhausted"))?;
            self.emit(
                state,
                "",
                0,
                state.world.position.simulated_time_ns,
                Kind::Frames { frames: vec![] },
            )?;
        }
        state.world.publication.next_driver_sequence = next;
        Ok(())
    }

    pub(super) fn close(&mut self, state: &mut State) -> Result<()> {
        self.emit(
            state,
            "",
            0,
            state.world.position.simulated_time_ns,
            Kind::Closed,
        )
    }
    pub(super) fn publish(
        &mut self,
        state: &mut State,
        names: &BTreeMap<MachineId, String>,
        event: Event,
    ) -> Result<()> {
        if event.sequence != state.world.publication.next_driver_sequence {
            return Err(invalid(
                "Driver publication cursor differs from service continuation",
            ));
        }
        let value = encode(names, &event)?;
        let bytes = serde_json::to_vec(&value)?;
        let publication = &mut state.world.publication;
        let mut hash = Sha256::new();
        hash.update(b"seedbox-driver-event-chain-v1\0");
        hash.update(publication.witness);
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
        publication.witness = hash.finalize().into();
        publication.witness_records = publication
            .witness_records
            .checked_add(1)
            .ok_or_else(|| invalid("Driver witness record count exhausted"))?;
        publication.next_driver_sequence = publication
            .next_driver_sequence
            .checked_add(1)
            .ok_or_else(|| invalid("Driver publication sequence exhausted"))?;
        let at = event.at.as_nanos();
        match &event.kind {
            EventKind::Output(output) => {
                let machine = name(names, output.machine)?;
                match &output.payload {
                    Response::Ready => {
                        self.emit(state, machine, output.boot, at, Kind::MachineReady)?;
                    }
                    Response::ApplicationOutput { stream, bytes } => {
                        let stream = match stream {
                            Stream::Stdout => application_log::Stream::Stdout,
                            Stream::Stderr => application_log::Stream::Stderr,
                        };
                        for chunk in bytes.chunks(application_log::MAX_BYTES) {
                            self.emit(
                                state,
                                machine,
                                output.boot,
                                at,
                                Kind::Log(application_log::Chunk {
                                    stream,
                                    bytes: chunk.to_vec(),
                                }),
                            )?;
                        }
                    }
                    Response::ApplicationOutcome { code } => self.emit(
                        state,
                        machine,
                        output.boot,
                        at,
                        Kind::MachineExited { code: *code },
                    )?,
                    // The SDK owns at-most-once and ended-ordinal validation.
                    // Do not turn a carrier's prospective boot into another
                    // lifecycle transition or a fabricated application exit.
                    Response::BootEnded { .. } => {}
                    Response::BootConsole { .. }
                    | Response::CommandAcknowledged { .. }
                    | Response::SimulationControl { .. }
                    | Response::TransmitFrame { .. } => {}
                    _ => return Err(invalid("unknown Driver output publication")),
                }
            }
            EventKind::Transition {
                machine,
                boot,
                change,
                ..
            } => {
                let transition = match change {
                    Transition::Paused => Some(MachineTransition::Paused),
                    Transition::Resumed => Some(MachineTransition::Resumed),
                    Transition::Crashed => Some(MachineTransition::Crashed),
                    Transition::Rebooted { .. } => Some(MachineTransition::Rebooted),
                    Transition::ExecutionSet(config) => Some(MachineTransition::Throttled {
                        factor_permille: config.factor_permille().into(),
                    }),
                    Transition::ShutdownRequested | Transition::ClockRate(_) => None,
                    _ => return Err(invalid("unknown Driver transition publication")),
                };
                if let Some(transition) = transition {
                    let boot = if let Transition::Rebooted { new_boot } = change {
                        *new_boot
                    } else {
                        *boot
                    };
                    self.emit(
                        state,
                        name(names, *machine)?,
                        boot,
                        at,
                        Kind::MachineLifecycle { transition },
                    )?;
                }
            }
            EventKind::FaultAdmitted(record) => {
                self.fault(state, names, record, RowStatus::Requested, None, at)?
            }
            EventKind::FaultSettled {
                record,
                status,
                outcome_at,
                ..
            } => {
                let row = match status {
                    SettlementStatus::Applied => RowStatus::Applied,
                    SettlementStatus::Unavailable => RowStatus::Unavailable,
                    SettlementStatus::Rejected | SettlementStatus::Failed => RowStatus::Failed,
                };
                self.fault(
                    state,
                    names,
                    record,
                    row,
                    outcome_at.map(|at| at.as_nanos()),
                    at,
                )?;
            }
            EventKind::InputRejected { key, rejection } => self.emit(
                state,
                "",
                0,
                at,
                Kind::OverrideRejected {
                    input_key: key.as_str().into(),
                    at_ns: at,
                    reason: rejection.detail.clone(),
                },
            )?,
            EventKind::MachineAdded { .. }
            | EventKind::TriggerArmed { .. }
            | EventKind::TriggerResolved { .. }
            | EventKind::ReplyAdmitted { .. } => {}
            _ => return Err(invalid("unknown Driver event publication")),
        }
        Ok(())
    }
    fn fault(
        &mut self,
        state: &mut State,
        names: &BTreeMap<MachineId, String>,
        record: &FaultRecord,
        status: RowStatus,
        applied_time_ns: Option<u64>,
        at: u64,
    ) -> Result<()> {
        let (machine, boot) = target_machine(names, &record.target)?;
        let input_key = if let FaultOrigin::Input { key } = &record.origin {
            Some(key.as_str().to_owned())
        } else {
            None
        };
        let plan = if let Some(key) = &input_key {
            Some(Box::new(
                state
                    .world
                    .input_program
                    .inputs
                    .iter()
                    .find(|input| input.key == *key)
                    .ok_or_else(|| invalid("Driver admitted input absent from service program"))?
                    .action
                    .clone(),
            ))
        } else {
            legacy_action(names, record)?.map(Box::new)
        };
        self.emit(
            state,
            machine.unwrap_or(""),
            boot,
            at,
            Kind::FaultChanged {
                input_key,
                id: record.fault.get(),
                action: action_name(record.action)?.into(),
                machine: machine.map(str::to_owned),
                status,
                applied_time_ns,
                plan,
            },
        )
    }
}
fn name(names: &BTreeMap<MachineId, String>, machine: MachineId) -> Result<&str> {
    names
        .get(&machine)
        .map(String::as_str)
        .ok_or_else(|| invalid("Driver publication has no retained machine name"))
}
pub(super) fn target_machine<'a>(
    names: &'a BTreeMap<MachineId, String>,
    target: &FaultTarget,
) -> Result<(Option<&'a str>, u64)> {
    match target {
        FaultTarget::Machine { machine, boot } => Ok((Some(name(names, *machine)?), *boot)),
        FaultTarget::Links { .. }
        | FaultTarget::DirectedLinks { .. }
        | FaultTarget::Connection { .. } => Ok((None, 0)),
        _ => Err(invalid("unknown Driver fault target")),
    }
}
fn members(names: &BTreeMap<MachineId, String>, ids: &[MachineId]) -> Result<Vec<String>> {
    ids.iter()
        .map(|id| name(names, *id).map(str::to_owned))
        .collect()
}
pub(super) fn action_name(action: FaultAction) -> Result<&'static str> {
    Ok(match action {
        FaultAction::Spawn => "spawn",
        FaultAction::Pause => "pause",
        FaultAction::Resume => "resume",
        FaultAction::Crash => "crash",
        FaultAction::Reboot => "reboot",
        FaultAction::Shutdown => "shutdown",
        FaultAction::SetExecution(_) => "throttle",
        FaultAction::Drift(_) => "clock_drift",
        FaultAction::StepRealtime(_) => "clock_skew",
        FaultAction::Partition => "partition",
        FaultAction::Shape { .. } => "shape",
        FaultAction::DropConnection => "drop_connection",
        FaultAction::Heal => "heal",
        FaultAction::Application(command) => match command {
            ApplicationCommand::Start => "restart",
            ApplicationCommand::Stop => "stop",
            ApplicationCommand::Pause => "pause_process",
            ApplicationCommand::Resume => "resume_process",
        },
        _ => return Err(invalid("unknown Driver fault action")),
    })
}
fn legacy_action(
    names: &BTreeMap<MachineId, String>,
    record: &FaultRecord,
) -> Result<Option<seedbox_control::Action>> {
    use seedbox_control::Action;
    let (target, _) = target_machine(names, &record.target)?;
    let machine = || {
        target
            .map(str::to_owned)
            .ok_or_else(|| invalid("Driver machine fault has no machine target"))
    };
    let handle = || {
        record
            .handle
            .as_ref()
            .map(|h| h.to_string())
            .ok_or_else(|| invalid("Driver healable fault has no handle"))
    };
    Ok(Some(match record.action {
        FaultAction::Pause => Action::Pause {
            machine: machine()?,
        },
        FaultAction::Resume => Action::Resume {
            machine: machine()?,
        },
        FaultAction::Crash => Action::Crash {
            machine: machine()?,
        },
        FaultAction::Reboot => Action::Reboot {
            machine: machine()?,
        },
        FaultAction::Application(command) => match command {
            ApplicationCommand::Start => Action::Restart {
                machine: machine()?,
            },
            ApplicationCommand::Stop => Action::Stop {
                machine: machine()?,
            },
            ApplicationCommand::Pause => Action::PauseProcess {
                machine: machine()?,
            },
            ApplicationCommand::Resume => Action::ResumeProcess {
                machine: machine()?,
            },
        },
        FaultAction::SetExecution(config) => Action::Throttle {
            machine: machine()?,
            handle: handle()?,
            factor_permille: config.factor_permille().into(),
        },
        FaultAction::Drift(rate) => Action::ClockDrift {
            machine: machine()?,
            handle: handle()?,
            rate_ppb: Some(rate.ppb().into()),
            max_rate_ppb: None,
        },
        FaultAction::StepRealtime(offset) => Action::ClockSkew {
            machine: machine()?,
            handle: handle()?,
            offset_ns: Some(offset),
            max_offset_ns: None,
        },
        FaultAction::Partition => {
            let FaultTarget::Links { left, right } = &record.target else {
                return Err(invalid("Driver partition has no link target"));
            };
            Action::Partition {
                handle: handle()?,
                left: members(names, left)?,
                right: members(names, right)?,
            }
        }
        FaultAction::Shape {
            policy,
            network_faults,
        } => {
            let FaultTarget::DirectedLinks { from, to } = &record.target else {
                return Err(invalid("Driver shape has no directed link target"));
            };
            Action::Shape {
                handle: handle()?,
                from: members(names, from)?,
                to: members(names, to)?,
                latency_ns: policy.latency_ns,
                jitter_ns: policy.jitter_ns,
                loss_ppm: policy.loss_ppm,
                bandwidth_bps: policy.bandwidth_bps,
                queue_bound_ns: policy.queue_bound_ns,
                network_faults,
            }
        }
        FaultAction::DropConnection => {
            let FaultTarget::Connection { connection } = &record.target else {
                return Err(invalid("Driver connection fault has no connection target"));
            };
            Action::DropConnection {
                handle: handle()?,
                connection: connection.clone(),
            }
        }
        FaultAction::Heal => Action::Heal { handle: handle()? },
        FaultAction::Spawn | FaultAction::Shutdown => return Ok(None),
        _ => return Err(invalid("unknown Driver fault action projection")),
    }))
}
fn record(names: &BTreeMap<MachineId, String>, record: &FaultRecord) -> Result<Value> {
    let origin = match &record.origin {
        FaultOrigin::Input { key } => json!({"input":key.as_str()}),
        FaultOrigin::Trigger { trigger } => json!({"trigger":trigger}),
        FaultOrigin::Control { machine, call } => {
            json!({"machine":name(names,*machine)?,"session":call.session.get(),"request":call.request.get()})
        }
        _ => return Err(invalid("unknown Driver fault origin")),
    };
    let target = match &record.target {
        FaultTarget::Machine { machine, boot } => {
            json!({"machine":name(names,*machine)?,"boot":boot})
        }
        FaultTarget::Links { left, right } => {
            json!({"left":members(names,left)?,"right":members(names,right)?})
        }
        FaultTarget::DirectedLinks { from, to } => {
            json!({"from":members(names,from)?,"to":members(names,to)?})
        }
        FaultTarget::Connection { connection } => json!({"connection":connection}),
        _ => return Err(invalid("unknown Driver fault target")),
    };
    Ok(
        json!({"id":record.fault.get(),"origin":origin,"target":target,"action":action_name(record.action)?,"plan":legacy_action(names,record)?,"handle":record.handle.as_ref().map(|h|h.to_string()),"requested_at":record.requested_at.as_nanos()}),
    )
}
fn cause(cause: driver::Cause) -> Result<Value> {
    Ok(match cause {
        driver::Cause::Initial => json!("initial"),
        driver::Cause::Host => json!("host"),
        driver::Cause::Fault(id) => json!({"fault":id.get()}),
        _ => return Err(invalid("unknown Driver transition cause")),
    })
}
fn rejection_reason(reason: driver::RejectionReason) -> Result<&'static str> {
    Ok(match reason {
        driver::RejectionReason::InvalidState => "invalid_state",
        driver::RejectionReason::TargetUnavailable => "target_unavailable",
        driver::RejectionReason::InvalidRequest => "invalid_request",
        driver::RejectionReason::Unsupported => "unsupported",
        _ => return Err(invalid("unknown Driver input rejection class")),
    })
}
pub(super) fn encode(names: &BTreeMap<MachineId, String>, event: &Event) -> Result<Value> {
    let kind = match &event.kind {
        EventKind::Output(output) => {
            let payload = match &output.payload {
                Response::BootEnded {
                    ended_boot,
                    disposition,
                    native_booted,
                    disk_lost_sectors,
                    surviving_disk,
                    retired_state_digest,
                } => json!({
                    "type":"boot_ended", "ended_boot":ended_boot,
                    "disposition":super::results::disposition(*disposition),
                    "native_booted":native_booted, "disk_lost_sectors":disk_lost_sectors,
                    "surviving_disk":surviving_disk, "retired_state_digest":retired_state_digest
                }),
                Response::Ready => json!({"type":"ready"}),
                Response::BootConsole { bytes } => {
                    json!({"type":"boot_console","bytes":hex::encode(bytes)})
                }
                Response::ApplicationOutput { stream, bytes } => {
                    json!({"type":"application_output","stream":match stream{Stream::Stdout=>"stdout",Stream::Stderr=>"stderr"},"bytes":hex::encode(bytes)})
                }
                Response::ApplicationOutcome { code } => {
                    json!({"type":"application_outcome","code":code})
                }
                Response::SimulationControl { id, payload } => {
                    json!({"type":"control","session":id.session.get(),"request":id.request.get(),"bytes":hex::encode(payload)})
                }
                Response::CommandAcknowledged { id, result } => {
                    json!({"type":"acknowledged","id":id.get(),"result":match result{CommandResult::Completed=>"completed",CommandResult::Rejected=>"rejected"}})
                }
                Response::TransmitFrame { bytes } => {
                    json!({"type":"transmit_frame","bytes":hex::encode(bytes)})
                }
                _ => return Err(invalid("unknown Driver response evidence")),
            };
            json!({"type":"output","machine":name(names,output.machine)?,"boot":output.boot,"at":output.at.as_nanos(),"payload":payload})
        }
        EventKind::MachineAdded {
            machine,
            cause: why,
            ..
        } => json!({"type":"machine_added","machine":name(names,*machine)?,"cause":cause(*why)?}),
        EventKind::Transition {
            machine,
            boot,
            change,
            cause: why,
        } => {
            let change = match change {
                Transition::Paused => json!({"type":"paused"}),
                Transition::Resumed => json!({"type":"resumed"}),
                Transition::Crashed => json!({"type":"crashed"}),
                Transition::Rebooted { new_boot } => json!({"type":"rebooted","new_boot":new_boot}),
                Transition::ShutdownRequested => json!({"type":"shutdown_requested"}),
                Transition::ExecutionSet(config) => {
                    json!({"type":"execution","factor_permille":config.factor_permille()})
                }
                Transition::ClockRate(rate) => json!({"type":"clock_rate","ppb":rate.ppb()}),
                _ => return Err(invalid("unknown Driver transition evidence")),
            };
            json!({"type":"transition","machine":name(names,*machine)?,"boot":boot,"cause":cause(*why)?,"change":change})
        }
        EventKind::FaultAdmitted(fault) => {
            json!({"type":"fault_admitted","record":record(names,fault)?})
        }
        EventKind::FaultSettled {
            record: fault,
            status,
            outcome_at,
            detail,
            network,
        } => {
            json!({"type":"fault_settled","record":record(names,fault)?,"status":match status{SettlementStatus::Applied=>"applied",SettlementStatus::Unavailable=>"unavailable",SettlementStatus::Rejected=>"rejected",SettlementStatus::Failed=>"failed"},"outcome_at":outcome_at.map(|at|at.as_nanos()),"detail":detail,"network":network})
        }
        EventKind::InputRejected { key, rejection } => {
            json!({"type":"input_rejected","key":key.as_str(),"reason":rejection_reason(rejection.reason)?,"detail":rejection.detail})
        }
        EventKind::TriggerArmed {
            trigger,
            source,
            target,
            deadline,
        } => {
            json!({"type":"trigger_armed","trigger":trigger,"source":name(names,*source)?,"target":target.map(|id|name(names,id)).transpose()?,"deadline":deadline.as_nanos()})
        }
        EventKind::TriggerResolved {
            trigger,
            result,
            fault,
        } => {
            json!({"type":"trigger_resolved","trigger":trigger,"result":match result{driver::TriggerResult::Fired=>"fired",driver::TriggerResult::Rejected=>"rejected",driver::TriggerResult::Cancelled=>"cancelled",driver::TriggerResult::Expired=>"expired",driver::TriggerResult::CutOff=>"cut_off"},"fault":fault.map(|id|id.get())})
        }
        EventKind::ReplyAdmitted { machine, boot, id } => {
            json!({"type":"reply_admitted","machine":name(names,*machine)?,"boot":boot,"session":id.session.get(),"request":id.request.get()})
        }
        _ => return Err(invalid("unknown Driver event evidence")),
    };
    Ok(
        json!({"version":1,"sequence":event.sequence,"position":{"boundary":event.position.boundary_index,"quanta":event.position.completed_quanta,"at":event.position.at.as_nanos()},"at":event.at.as_nanos(),"kind":kind}),
    )
}
