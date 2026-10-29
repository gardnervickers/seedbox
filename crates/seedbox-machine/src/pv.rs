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

pub mod bridge;
pub mod ethernet;
pub(crate) mod oracle;
pub(crate) mod protocol;
pub mod transport;

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
    /// the machine's crash disk semantics. A boot forcibly ended by this
    /// teardown is retired as [`crate::machine::EvidenceDisposition::Crashed`],
    /// not graceful shutdown, so crash disk loss applies.
    /// An already-ended boot is not reclassified or reported a second time.
    /// This is not snapshot restoration.
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
    /// Answer a flush record of one guest session of the machine's
    /// current boot. `payload` is the compact JSON of a property reply, which
    /// the supervisor sends to that session as one message; a session that has
    /// closed discards it. Replies have no acknowledgment and the caller never
    /// offers one to a later boot, whose sessions are unrelated.
    PropertyReply {
        session: NonZeroU32,
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
    // A realtime step is guest kernel state (offset, timer
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
    // Frames reuse the timed inbox so arrival shares the
    // admission order of lifecycle requests and adopts the machine's time base;
    // a separate delivery path would need its own ordering and snapshot rules.
    /// Offer one Ethernet frame to the machine's NIC. The input's `at` is the
    /// frame's simulation arrival time mapped into this receiver's time base
    /// (see [`Input`]); the sender's time base never appears here.
    /// The machine always takes an in-bounds frame. Taking transfers it to the NIC, not to the
    /// guest: without a NIC, in an exited boot, or before the guest driver has
    /// readied its receive queue, the frame is dropped. Otherwise it fills a
    /// guest receive buffer, or waits in a NIC backlog of at most
    /// [`RECEIVE_BACKLOG_FRAMES`] frames until the guest supplies one; a
    /// frame arriving at a full backlog is dropped. The backlog belongs to
    /// machine snapshots; device reset, crash and reboot clear it. A frame
    /// outside [`MIN_FRAME_BYTES`]..=[`MAX_FRAME_BYTES`] is invalid caller
    /// input: the machine fails the run with `Err`, with no rollback or
    /// resumability guarantee. The Driver offers only frames it validated
    /// when they were transmitted.
    ReceiveFrame {
        /// One Ethernet frame starting at the destination address, without
        /// FCS or a virtio-net header. The EtherType and payload are opaque.
        bytes: Vec<u8>,
    },
    // Disk and scheduler faults act inside the machine's own devices and
    // guest, so the Driver installs them through the timed inbox, like
    // lifecycle requests, and reads their counts back after a heal.
    /// Start a [`DeviceFault`] identified by `id`, with draws keyed by `seed`.
    /// It applies to this boot and every later one, surviving guest device
    /// resets, until [`Request::HealFault`]; each boot draws on a fresh
    /// stream derived from `seed` and that boot. An exited machine keeps it
    /// for its next boot. Reusing a live `id` is invalid caller input.
    InstallFault {
        id: u64,
        seed: u64,
        fault: DeviceFault,
    },
    /// Stop the fault `id`. Its final counts become readable once through
    /// [`crate::machine::Machine::take_fault_counts`]. Requests a latency
    /// fault already delayed still complete at their times.
    HealFault { id: u64 },
    /// Hold the `nth` guest disk request of kind `op` from now on, counted
    /// across boots, if it arrives before machine time `until`. The held
    /// request has no effect and nothing after it is processed: the machine
    /// emits [`Response::BlockRequestHeld`] and stays blocked, taking no
    /// input but Crash, Reboot and fault requests, until its boot ends. The
    /// hold survives reboots until it matches or is released.
    HoldBlockRequest {
        trigger: u32,
        op: DiskOp,
        nth: u64,
        until: Option<Timestamp>,
    },
    /// Stop counting for the hold of `trigger`; a request it already holds
    /// stays held.
    ReleaseBlockRequest { trigger: u32 },
}

pub use seedbox_engine_protocol::control::{DiskEffect, DiskOp, DiskScope, FaultCounts};

/// A fault a machine carries out in its own devices or guest kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DeviceFault {
    /// Select disk requests of kind `op` with probability `ppm` per million:
    /// complete them with IOERR, or with one sector of seeded bytes.
    DiskError {
        op: DiskOp,
        ppm: u32,
        effect: DiskEffect,
        scope: DiskScope,
    },
    /// Complete disk requests of kind `op` after `latency_ns` plus a draw of
    /// at most `jitter_ns`, in submission order.
    DiskLatency {
        op: DiskOp,
        latency_ns: u64,
        jitter_ns: u64,
    },
    /// Yield the guest CPU at `ppm` per million syscalls and progress charges.
    Preempt { ppm: u32 },
}

pub use ethernet::frame_in_bounds;
pub use ethernet::{MAX_FRAME_BYTES, MIN_FRAME_BYTES, RECEIVE_BACKLOG_FRAMES};

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
    /// One ended logical boot, emitted only under configured service policy.
    ///
    /// `ended_boot` is the committed boot that ended, including logical boot
    /// zero before any native startup. It is authoritative for this payload:
    /// a surrounding Driver output may carry a prospective new boot during
    /// Reboot admission. Consumers must not substitute that carrier identity.
    /// `native_booted = false` explicitly means no native RAM ever existed;
    /// its retired digest must be absent. Otherwise an absent retired digest
    /// means full-state capture was not requested. Surviving disk state is
    /// after loss selection; any retired state digest is before loss/teardown.
    /// Reboot's forced teardown retires the ended boot as `Crashed` and applies
    /// crash disk loss; it does not turn an earlier natural exit into a second
    /// retirement. Without a disk, lost sectors is zero and the surviving
    /// digest is None.
    // Reboot output carriers name the prospective boot;
    // retired state needs its own validated committed ordinal. A typed output
    // preserves identity without rewriting every other response's stamping.
    BootEnded {
        ended_boot: u64,
        disposition: crate::machine::EvidenceDisposition,
        native_booted: bool,
        disk_lost_sectors: u64,
        surviving_disk: Option<String>,
        retired_state_digest: Option<String>,
    },
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
    /// Actual kernel victim report; emitted before the outcome it explains.
    OutOfMemoryKill {
        kill: seedbox_engine_protocol::out_of_memory::Kill,
    },
    /// One guest property record, relayed uninterpreted with its supervisor
    /// session and sequence. Records of one session keep their order; a
    /// session belongs to the boot that emitted it. The caller reduces it.
    Property {
        frame: seedbox_engine_protocol::property::Frame,
    },
    /// The kernel's fatal-signal report of one crashed process, before the
    /// outcome of the application it ends.
    Crash {
        report: seedbox_engine_protocol::crash::Report,
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
    /// One Ethernet frame the guest transmitted. The output's `at` is the
    /// guest's transmit notification in the sender's own time base; the Driver
    /// maps it to simulation time before routing. Frames are emitted in guest
    /// transmission order. No checksum, segmentation or VLAN offload is
    /// offered. The NIC drops guest frames outside
    /// [`MIN_FRAME_BYTES`]..=[`MAX_FRAME_BYTES`] or whose device header
    /// requests an offload; guest payload never causes `Err`, while a
    /// malformed descriptor ring remains a machine error. Only a machine whose
    /// definition has a NIC may emit this; the Driver rejects anything else.
    TransmitFrame {
        /// One Ethernet frame starting at the destination address, without
        /// FCS or a virtio-net header. The EtherType and payload are opaque.
        bytes: Vec<u8>,
    },
    /// The hold of `trigger` (see [`Request::HoldBlockRequest`]) holds a
    /// guest disk request: from this output on, the machine is blocked until
    /// a Crash or Reboot ends its boot.
    BlockRequestHeld { trigger: u32 },
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
mod queue_tests;
