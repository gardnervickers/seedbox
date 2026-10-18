//! Bounded service coordination around the retained Driver. This owns no
//! scheduler, fault RNG, execution rate, or second copy of future inputs.
use super::super::transport::{Control, Incoming};
use super::*;
use std::time::Duration;
const CONTROL_BATCH: usize = 64;
const CONTROL_POLL: Duration = Duration::from_millis(10);

pub(super) fn execute<M>(
    worker: &mut Worker<M>,
    until_ns: u64,
    control: &mut Control,
) -> Result<StopReason>
where
    M: Machine + Send + 'static,
    M::Context: 'static,
    M::Error: Send + std::fmt::Display + 'static,
{
    worker.flush()?;
    loop {
        receive(worker, control)?;
        match worker.capture_pending() {
            Ok(Some(checkpoint)) => control.checkpoint_captured(&checkpoint),
            Ok(None) => {}
            Err(checkpoint::CaptureFailure::Refused(error)) => {
                control.checkpoint_refused(error.to_string())
            }
            Err(checkpoint::CaptureFailure::Publication(error)) => return Err(error),
        }
        if worker.terminal {
            control.waiting(false)?;
            return Ok(StopReason::Completed);
        }
        if worker.prepared.is_some() {
            control.waiting(true)?;
            std::thread::sleep(CONTROL_POLL);
            continue;
        }
        let playback = worker.playback()?;
        let at = playback.position.at.as_nanos();
        if at > until_ns {
            return Err(fail(
                "Driver continuation is beyond the requested run horizon",
            ));
        }
        if playback.before_fault.is_some() {
            // Only Driver's playback state releases a held effect. Do not
            // finalize a horizon or consume another input while it is held.
            match worker.step(Timestamp::from_nanos(at))? {
                StepOutcome::Finished { .. } | StepOutcome::Stopped { .. } => continue,
                StepOutcome::BeforeFault { .. } | StepOutcome::Waiting { .. } => {
                    control.waiting(true)?;
                    std::thread::sleep(CONTROL_POLL);
                    continue;
                }
                _ => continue,
            }
        }
        let next = worker
            .state
            .world
            .input_program
            .inputs
            .get(worker.state.world.input_program.next)
            .map(|input| input.at_ns);
        if next.is_some_and(|next| next < at) {
            return Err(fail("Driver passed an unconsumed authored input"));
        }
        if next == Some(at) {
            control.waiting(false)?;
            if worker.authored_next()? {
                continue;
            }
            if worker.terminal {
                continue;
            }
            control.waiting(true)?;
            std::thread::sleep(CONTROL_POLL);
            continue;
        }
        if at == until_ns {
            control.waiting(false)?;
            return Ok(StopReason::SimulatedTimeLimit);
        }
        let upto = Timestamp::from_nanos(next.unwrap_or(until_ns).min(until_ns));
        control.waiting(false)?;
        let outcome = if next.is_some() {
            worker.step_toward_external(upto)?
        } else {
            worker.step(upto)?
        };
        match outcome {
            StepOutcome::Waiting { delay, .. } => {
                control.waiting(true)?;
                std::thread::sleep(delay.min(CONTROL_POLL));
            }
            StepOutcome::BeforeFault { .. } => {
                control.waiting(true)?;
                std::thread::sleep(CONTROL_POLL);
            }
            StepOutcome::Finished { .. } | StepOutcome::Stopped { .. } => {}
            StepOutcome::BarrierReached { .. } => {}
            StepOutcome::Blocked { at: reached } => {
                if next.is_none() {
                    control.waiting(false)?;
                    return worker.quiescent_reason();
                }
                if reached.as_nanos() == at {
                    return Err(fail("Driver made no progress toward known external input"));
                }
            }
        }
    }
}
fn receive<M>(worker: &mut Worker<M>, control: &mut Control) -> Result<()>
where
    M: Machine + Send + 'static,
    M::Context: 'static,
    M::Error: Send + std::fmt::Display + 'static,
{
    for _ in 0..CONTROL_BATCH {
        let Some(incoming) = control.receive()? else {
            break;
        };
        match incoming {
            Incoming::Pause(ref peer) | Incoming::Resume(ref peer) => {
                let rate = if matches!(incoming, Incoming::Pause(_)) {
                    host::PlaybackRate::Paused
                } else {
                    host::PlaybackRate::Unlimited
                };
                let dispatch = worker.playback_request(Request {
                    id: 0,
                    command: Command::SetRate { rate },
                })?;
                let Dispatch::Reply(reply) = dispatch else {
                    return Err(fail("legacy playback unexpectedly became semantic work"));
                };
                let bytes = if reply.error.is_some() {
                    host::INVALID
                } else if rate == host::PlaybackRate::Paused {
                    host::PAUSED
                } else {
                    host::RUNNING
                };
                control.raw(peer, bytes);
            }
            Incoming::Request { request, peer } => {
                if let Some(prior) = control.prior(&request) {
                    match prior {
                        Ok(reply) => control.cached(&reply, &peer)?,
                        Err(error) => {
                            let mut reply = worker.status(request.id)?;
                            reply.error = Some(error);
                            control.cached(&reply, &peer)?;
                        }
                    }
                    continue;
                }
                let dispatch = worker.playback_request((*request).clone())?;
                let reply = match dispatch {
                    Dispatch::Reply(reply) => reply,
                    Dispatch::Semantic(ref semantic) => match semantic.command {
                        Command::PrepareFault { .. } => worker.prepare_request(semantic)?,
                        Command::CommitFault { .. } => worker.commit(semantic)?,
                        _ => return Err(fail("unexpected semantic Driver command")),
                    },
                };
                control.reply(*request, reply, &peer)?;
            }
        }
    }
    Ok(())
}
