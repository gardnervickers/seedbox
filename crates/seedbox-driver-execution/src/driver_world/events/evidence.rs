//! Test-only structural encoding of committed Driver events. Unit tests compare
//! these values across worker counts and observation settings; production keeps
//! no per-event evidence.
use super::{action_name, control_action, invalid, members, name};
use crate::Result;
use seedbox_driver::driver::{
    self as driver, Event, EventKind, FaultOrigin, FaultRecord, FaultTarget, MachineId,
    SettlementStatus, Transition,
};
use seedbox_machine::pv::{CommandResult, Response, Stream};
use serde_json::{json, Value};
use std::collections::BTreeMap;

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
        json!({"id":record.fault.get(),"origin":origin,"target":target,"action":action_name(record.action)?,"plan":control_action(names,record)?,"handle":record.handle.as_ref().map(|h|h.to_string()),"requested_at":record.requested_at.as_nanos()}),
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
pub(in crate::driver_world) fn encode(
    names: &BTreeMap<MachineId, String>,
    event: &Event,
) -> Result<Value> {
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
                    "disposition":crate::driver_world::results::disposition(*disposition),
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
                Response::OutOfMemoryKill { kill } => {
                    json!({"type":"out_of_memory_kill", "kill":kill})
                }
                Response::Property { frame } => {
                    json!({"type":"property","session":frame.session,"seq":frame.seq,"bytes":hex::encode(&frame.payload)})
                }
                Response::Crash { report } => {
                    json!({"type":"crash", "report":report})
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
