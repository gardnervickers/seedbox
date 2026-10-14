//! Shared guest-facing operations and messages for machine backends.
//!
//! This module is the home for contracts shared by native PV and TCG guests.
//! Backends own their transports and translate between guest interactions and
//! these contracts. The native execution backend remains in
//! `machine::pv`; this module does not select an execution backend.
//!
//! Asynchronous inputs and outputs cross a host simulation boundary through
//! inboxes and outboxes borrowed during a run, distinct from the guest wire
//! protocol. The driver offers ordered, due-only inputs; the machine owns work
//! it accepts. Outputs become visible only at a committed simulation barrier.
//! Clock reads, RNG requests, and device servicing instead require immediate
//! handling during execution; they are not queued control messages.
//!
//! Shared contract types belong at this module's top level. Internal submodules
//! organize implementations as needed.

pub(crate) mod bridge;
pub(crate) mod oracle;
pub(crate) mod protocol;
pub(crate) mod transport;

#[cfg(all(test, unix))]
mod guest_supervisor_tests;

use crate::machine::Timestamp;
use std::{
    collections::VecDeque,
    num::{NonZeroI64, NonZeroU32},
};

/// Identity of a simulation-control call on one machine.
/// Sessions are never reused across crash or reboot. Request IDs must not be
/// reused within a session. Callers retain the corresponding machine identity.
/// Restoring older machine history also requires its matching external queues,
/// or a separate machine/branch identity; restoration can revisit that history's
/// IDs. Session allocation and correlation survive machine snapshots, and
/// exhaustion fails before any identity can wrap or alias.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimulationControlId {
    /// Machine session, never reused across crash or reboot.
    pub session: NonZeroU32,
    /// Caller request sequence within the session.
    pub request: NonZeroU32,
}

/// Ordered work offered to one machine.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Request {
    /// End the current boot immediately without graceful cleanup.
    /// Discard old-boot work and pending shutdown. Disk effects are defined by
    /// the machine. Existing snapshots and committed external effects remain
    /// unchanged.
    Crash,
    /// Start a fresh boot, discarding old-boot work and pending shutdown, with
    /// the machine's crash disk semantics. This is not snapshot restoration.
    /// Existing snapshots and committed external effects remain unchanged.
    /// The caller clears any execution pause when admitting this request.
    /// Repeated requests perform distinct resets using the caller's RNG stream.
    Reboot,
    /// Request graceful cleanup without guaranteeing completion.
    /// Repetition is harmless; an exited boot remains exited. No timeout or
    /// forced termination is implied, and execution remains caller-controlled.
    /// Pending acceptance and delivery belong to machine snapshots. Crash and
    /// Reboot cancel old-boot shutdown; Shutdown after Reboot targets the new
    /// boot. Exited-boot work may be discarded without a fabricated guest
    /// acknowledgment.
    Shutdown,
    /// Return the reply payload for a corresponding simulation-control call.
    /// Acceptance transfers responsibility for delivery, not evidence that the
    /// guest has read it. Accepted reply delivery and its correlation survive
    /// machine snapshots. Replies to retired sessions, including across crash
    /// and reboot, are silently discarded; accepted host effects are not
    /// cancelled. The caller preserves the corresponding machine association.
    SimulationReply {
        /// Identity from the corresponding simulation-control observation.
        id: SimulationControlId,
        /// Reply payload excluding transport framing.
        payload: Vec<u8>,
    },
    /// A correlated operation on the prepared application.
    /// Command IDs are distinct from simulation-control RPC IDs and must not be reused
    /// within a machine incarnation, even after completion. The caller supplies
    /// incarnation association and rejects acknowledgments from stale boots.
    Application {
        /// Caller-assigned command correlation ID.
        id: NonZeroU32,
        /// Operation on the application's process group.
        command: ApplicationCommand,
    },
    // TODO: Re-evaluate: a realtime step is guest kernel state (offset, timer
    // re-evaluation, cancel-on-set); changing the injected time base would move
    // monotonic too, so the guest must perform it. It reuses the guest-command
    // ID space and acknowledgment rather than adding another completion path.
    /// Step the guest's CLOCK_REALTIME by a signed offset, leaving monotonic,
    /// boot and raw clocks and the machine's time base unchanged. The guest
    /// re-evaluates absolute realtime timers and cancels timers armed with
    /// cancel-on-set, as for any Linux clock step. Completion is the matching
    /// [`Response::CommandAcknowledged`]: Completed after the guest kernel has
    /// applied the step, Rejected if it refused. A refusal is a valid outcome
    /// (the Driver settles it Rejected without failing); a malformed, missing
    /// or late acknowledgment remains a fatal protocol failure. Linux refuses a step that would
    /// put CLOCK_REALTIME below CLOCK_MONOTONIC, so a negative step can be at
    /// most the boot's realtime lead over monotonic (its epoch plus the counter
    /// that elapsed before the kernel clocksource started). The ID shares the per-boot correlation space of
    /// `Application` and must not be reused within a machine incarnation.
    /// The step is guest kernel state: it ends with crash, reboot or exit, and
    /// every boot starts realtime from the machine's epoch again.
    StepRealtime {
        /// Caller-assigned command correlation ID.
        id: NonZeroU32,
        /// Signed realtime step in nanoseconds.
        nanoseconds: NonZeroI64,
    },
}

/// Operations on the application process group; its kernel keeps running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplicationCommand {
    /// Start the unchanged prepared application when none is alive.
    /// Reject if one is alive. Success confirms successful execution setup,
    /// not application health or subsequent application progress.
    Start,
    /// Kill and reap the live application group, including a paused group.
    /// Drain already queued stdout/stderr without waiting for inherited
    /// descendant writers. Reject if no application is alive. Success leaves
    /// the supervisor available for Start and emits no application outcome.
    Stop,
    /// Suspend the live group; complete once its leader is observed stopped.
    /// Reject if no live leader can be stopped. Repeating Pause on an already
    /// stopped live group succeeds. The kernel and supervisor keep running.
    Pause,
    /// Send continuation to a live group. Success confirms signal dispatch,
    /// not subsequent application execution. Repeated Resume succeeds.
    Resume,
}

/// Observed guest messages and supervisor effects. The caller supplies machine
/// identity and incarnation; application restarts within one boot retain that
/// incarnation.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Response {
    /// A complete guest simulation-control call observed through its final byte.
    /// Machine association belongs to the caller. Publishing this request
    /// transfers ownership of the call to the caller without executing its
    /// operation. Disconnect, crash and reboot do not revoke a published call.
    SimulationControl {
        /// Guest session and request identity, distinct from application IDs.
        id: SimulationControlId,
        /// Request payload excluding transport framing.
        payload: Vec<u8>,
    },
    /// Commands are ready after the initial application launch succeeds.
    /// Emitted once per boot. This is not application health; a successful
    /// later Start produces an acknowledgment rather than another Ready.
    Ready,
    /// Firmware, kernel or supervisor diagnostics, separate from application logs.
    BootConsole {
        /// Exact console bytes, without decoration or an added newline.
        bytes: Vec<u8>,
    },
    /// Exact application output in observation order. Chunk boundaries are not
    /// lines; Stop/Start does not create a new machine incarnation.
    ApplicationOutput {
        /// Application pipe from which the bytes were observed.
        stream: Stream,
        /// Exact bytes, without decoration or an added newline.
        bytes: Vec<u8>,
    },
    /// Natural application termination, after draining already queued output.
    /// Commanded Stop produces only its acknowledgment. This observation does
    /// not itself assert machine exit or a test verdict. Verdicts belong to
    /// caller/test policy, and machine exit is reported separately.
    ApplicationOutcome {
        /// Normal exit status, or 128 plus the terminating signal number,
        /// represented in eight bits.
        code: u8,
    },
    /// Guest completion or rejection at the command's documented completion
    /// point, distinct from accepting responsibility for an input. A failed
    /// Start produces rejection without readiness or an application outcome.
    /// Initial launch failure is a setup diagnostic and machine failure.
    /// `Application` and `StepRealtime` requests acknowledge through this
    /// same response.
    CommandAcknowledged {
        /// Correlation ID from the accepted `Application` or `StepRealtime`
        /// request; both share one per-boot guest-command ID namespace.
        id: NonZeroU32,
        /// Guest-observed operation result.
        result: CommandResult,
    },
}

/// Application output pipe, distinct from boot console bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// Guest acknowledgment result; detailed diagnostics remain console observations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandResult {
    /// The command reached its documented completion point.
    Completed,
    /// The guest could not perform the command.
    Rejected,
}

/// Input offered to one machine, timestamped in the time base of the inbox's
/// owner. The Driver keeps inputs in simulation time and offers them to a
/// machine in that machine's time base (see [`crate::machine::Machine::run`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Input {
    /// Earliest delivery time. Guest realtime steps never change it.
    pub at: Timestamp,
    /// Work offered to the guest supervisor.
    pub payload: Request,
}

/// Borrowed due-only access to caller-owned ordered inputs, usable without Driver.
/// The caller preserves its single admission order across lifecycle actions and
/// inputs, including ties. It constructs/delivers these views in that order;
/// lifecycle operations must not be grouped ahead of same-time earlier inputs.
/// The caller prevents concurrent queue mutation and machine execution. This
/// inbox is scoped to one boot: crash/reboot invalidate old-boot entries;
/// later inputs admitted for the new boot are preserved. Native reset still
/// waits for run. Future scheduled actions and external
/// in-flight delivery stay with their external owners. Natural exit discards
/// ended-boot inputs without carrying them into a later boot or fabricating
/// acknowledgments.
pub struct Inbox<'a> {
    pending: &'a mut VecDeque<Input>,
    due_through: Timestamp,
}

impl<'a> Inbox<'a> {
    /// Borrow a queue ordered by timestamp then original admission order.
    /// `due_through` is the current committed delivery boundary, not the future
    /// run horizon. The caller admits inputs incrementally from its single
    /// ordered timeline and performs required machine refresh before applying
    /// the next lifecycle action. Do not pre-admit later inputs across a pending
    /// lifecycle action, even when their timestamps are equal. Later entries
    /// remain caller-owned until that action has been processed.
    pub fn new(pending: &'a mut VecDeque<Input>, due_through: Timestamp) -> Self {
        Self {
            pending,
            due_through,
        }
    }

    /// Inspect the first due input without accepting it. Returns None for an
    /// empty queue or a future front entry; entries cannot be skipped.
    pub fn front(&self) -> Option<&Input> {
        self.pending
            .front()
            .filter(|input| input.at <= self.due_through)
    }

    /// Accept the first due input, transferring responsibility to the machine.
    /// Before reporting a successful run boundary, the machine must complete
    /// the work or capture its pending accepted state in its snapshot.
    /// Taking is neither guest acknowledgment nor operation completion.
    /// Untaken input remains caller-owned, outside the machine snapshot.
    pub fn take(&mut self) -> Option<Input> {
        self.front()?;
        self.pending.pop_front()
    }
}

/// Output staged by one machine. The caller supplies machine identity and
/// incarnation on publication. Buffer position supplies emission order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    /// Time when observed, in the emitting machine's time base (the Driver
    /// converts it to simulation time); never a guest realtime reading.
    pub at: Timestamp,
    /// Observed supervisor effect.
    pub payload: Response,
}

/// Borrowed output staging for one run, usable without Driver. The caller starts
/// with an empty buffer; no staged output is externally visible before commit.
/// On execution error discard staging and stop the machine; rollback is not implied.
pub struct Outbox<'a> {
    staged: &'a mut Vec<Output>,
    start: Timestamp,
    upto: Timestamp,
}

/// An output timestamp lies outside the permitted execution interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidOutputTime;

impl<'a> Outbox<'a> {
    /// Borrow an empty buffer with start <= upto; caller enforces preconditions.
    /// `start` is after adopting current shared time. Overdue observed effects
    /// belong to start or later, never their historical due times.
    pub fn new(staged: &'a mut Vec<Output>, start: Timestamp, upto: Timestamp) -> Self {
        Self {
            staged,
            start,
            upto,
        }
    }

    /// Stage a stamped output in emission order; no publication occurs.
    /// Reject without mutation outside [start, upto]. At successful run commit,
    /// caller also validates every stamp <= returned reached. Merge successive
    /// buffers in machine execution order, then publish globally by
    /// (timestamp, machine identity, per-machine emission order), never worker
    /// completion order. Causal scheduling remains caller-owned; publication
    /// does not permit another machine to execute retroactively.
    pub fn emit(&mut self, output: Output) -> Result<(), InvalidOutputTime> {
        if output.at < self.start || output.at > self.upto {
            return Err(InvalidOutputTime);
        }
        self.staged.push(output);
        Ok(())
    }
}

#[cfg(test)]
mod queue_tests {
    use super::*;
    fn at(n: u64) -> Timestamp {
        Timestamp::from_nanos(n)
    }
    fn input(n: u64, id: u32) -> Input {
        Input {
            at: at(n),
            payload: Request::Application {
                id: NonZeroU32::new(id).unwrap(),
                command: ApplicationCommand::Start,
            },
        }
    }
    #[test]
    fn take_transfers_only_due_prefix_in_admission_order() {
        let a = input(2, 1);
        let b = input(2, 2);
        let future = input(7, 3);
        let mut pending = VecDeque::from([a.clone(), b.clone(), future.clone()]);
        {
            let mut inbox = Inbox::new(&mut pending, at(2));
            assert_eq!(inbox.front(), Some(&a));
            assert_eq!(inbox.front(), Some(&a)); // Inspection does not accept.
            assert_eq!(inbox.take(), Some(a));
            assert_eq!(inbox.take(), Some(b));
            assert_eq!(inbox.front(), None);
            assert_eq!(inbox.take(), None);
        }
        assert_eq!(pending, VecDeque::from([future.clone()]));
        assert_eq!(Inbox::new(&mut pending, at(7)).take(), Some(future));
    }
    #[test]
    fn dropped_view_preserves_unaccepted_work_and_next_view_sees_new_admissions() {
        let first = input(0, 1);
        let after_action = input(0, 2);
        let mut pending = VecDeque::from([first.clone()]);
        {
            let inbox = Inbox::new(&mut pending, at(0));
            assert_eq!(inbox.front(), Some(&first));
        }
        assert_eq!(pending.pop_front(), Some(first)); // Caller owns unaccepted work.
                                                      // Caller processes intervening lifecycle action before this admission.
        pending.push_back(after_action.clone());
        assert_eq!(Inbox::new(&mut pending, at(0)).take(), Some(after_action));
    }
    #[test]
    fn outbox_rejects_outside_interval_without_changing_staging() {
        let mut staged = Vec::new();
        {
            let mut outbox = Outbox::new(&mut staged, at(5), at(8));
            assert_eq!(
                outbox.emit(Output {
                    at: at(4),
                    payload: Response::Ready
                }),
                Err(InvalidOutputTime)
            );
            outbox
                .emit(Output {
                    at: at(5),
                    payload: Response::Ready,
                })
                .unwrap();
            outbox
                .emit(Output {
                    at: at(8),
                    payload: Response::ApplicationOutput {
                        stream: Stream::Stdout,
                        bytes: vec![0, 255],
                    },
                })
                .unwrap();
            assert_eq!(
                outbox.emit(Output {
                    at: at(9),
                    payload: Response::Ready
                }),
                Err(InvalidOutputTime)
            );
        }
        assert_eq!(staged.len(), 2);
        assert_eq!(staged[0].at, at(5));
        assert_eq!(staged[1].at, at(8));
        assert_eq!(
            staged[1].payload,
            Response::ApplicationOutput {
                stream: Stream::Stdout,
                bytes: vec![0, 255]
            }
        );
    }
    #[test]
    fn zero_budget_outbox_keeps_same_time_emission_order() {
        let mut staged = Vec::new();
        {
            let mut outbox = Outbox::new(&mut staged, at(3), at(3));
            outbox
                .emit(Output {
                    at: at(3),
                    payload: Response::Ready,
                })
                .unwrap();
            outbox
                .emit(Output {
                    at: at(3),
                    payload: Response::ApplicationOutcome { code: 7 },
                })
                .unwrap();
        }
        assert_eq!(
            staged
                .iter()
                .map(|event| &event.payload)
                .collect::<Vec<_>>(),
            [&Response::Ready, &Response::ApplicationOutcome { code: 7 }]
        );
    }
}
