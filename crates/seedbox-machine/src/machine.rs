//! Machine execution, lifecycle, and snapshot contracts.

use crate::{clock::Clock, execution::ExecutionConfig, rng::RngStream};

/// Dependencies supplied for one stopped execution interval.
/// Worker resources belong to the executing thread; clock and RNG belong to
/// the driver. The execution rate is a copied driver-owned setting.
/// The RNG cursor follows the machine, independently of the worker.
pub struct RunContext<'a, C> {
    pub worker: &'a mut C,
    pub clock: &'a dyn Clock,
    pub rng: &'a mut dyn RngStream,
    /// CPU-work pricing for this run, independent of clocks and idle time.
    /// Already-charged partial work retains its earlier price. The configured
    /// rate belongs to the caller; snapshots retain work progress and charge
    /// caches, not authority over this external setting.
    pub execution: ExecutionConfig,
    pub inbox: crate::pv::Inbox<'a>,
    pub outbox: crate::pv::Outbox<'a>,
}

#[cfg(test)]
mod test_environment;

#[cfg(all(test, unix))]
mod control_snapshot_tests;
#[cfg(all(test, unix))]
mod guest_service_snapshot_tests;
#[cfg(all(test, unix))]
mod prepared_application_tests;
#[cfg(all(test, unix))]
mod prepared_fixture;

#[cfg(unix)]
pub mod tcg;

#[cfg(unix)]
mod assets;
#[cfg(all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos")))]
pub mod pv;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(u64);

impl Timestamp {
    /// Construct a simulation timestamp in nanoseconds.
    pub const fn from_nanos(nanos: u64) -> Self {
        Self(nanos)
    }

    /// Return the simulation timestamp in nanoseconds.
    pub const fn as_nanos(self) -> u64 {
        self.0
    }
}

pub trait Machine {
    /// Worker-local host resources required to execute this machine. They contain
    /// no persistent simulation dependencies and need not implement Send.
    type Context;
    type Error;

    /// A self-contained capture of the manifest and machine-owned state.
    /// See [`Machine::snapshot`] for the capture and ownership guarantees.
    type Snapshot;

    /// Execute until `upto`, all work becomes blocked, or the machine exits.
    ///
    /// This call is synchronous: it returns with execution stopped. The machine
    /// retains its execution state for a later call; `context` supplies the
    /// compatible worker's execution resources and borrows driver-owned simulation
    /// dependencies for this call. The machine must not retain those borrows.
    /// Initialization, including boot entropy, occurs through this context on
    /// the first run after construction or reboot. Initialization may occur with
    /// zero execution budget, but must execute no program instructions.
    ///
    /// # Time and execution boundaries
    ///
    /// All timestamps use simulation time, before guest clock offset or skew.
    /// Execution starts at the later of the machine's current execution position
    /// and the current simulation time supplied by its environment. The caller
    /// must provide `upto` at or after both. Execution must not expose effects
    /// beyond `upto`. Returned `reached` timestamps never move backwards or
    /// exceed `upto`.
    ///
    /// The caller may withhold execution while simulation time advances. On the
    /// next call, the machine adopts the later time without executing CPU work
    /// for the elapsed interval. Timers that became due during that interval
    /// become eligible for delivery at the adopted time, never retroactively at
    /// their original deadlines. Other newly delivered effects also belong to
    /// the adopted time or later. Adopting time does not advance the shared clock.
    ///
    /// With `upto` equal to the starting time, the machine may adopt time and
    /// make due wakeups visible, but must not execute program instructions.
    /// A runnable machine remains eligible for subsequent calls; no separate
    /// pause or resume operation is required. Due requests are accepted in FIFO
    /// order, including on zero-budget calls while execution is withheld or the
    /// machine has exited. An exited machine preserves its exit state and reason until
    /// a Reboot request is accepted; positive-budget execution requires a live
    /// machine. Invalid state remains an error. Lifecycle request acceptance
    /// executes no program instructions. Crash and Reboot discard old-boot work;
    /// repeated Reboot requests perform distinct resets. Reboot clears an
    /// earlier shutdown request; Shutdown after Reboot targets the new boot.
    ///
    /// A successful return describes an accepted simulation boundary, with no
    /// speculative state or effects exposed to the caller.
    /// A blocked machine may return without making progress.
    ///
    /// # Handling the result
    ///
    /// - [`StopReason::BoundaryReached`]: the machine reached `upto` and is
    ///   stopped. The caller should record that position, process events due at
    ///   that boundary in simulation order, and schedule another call when the
    ///   machine is allowed to execute further.
    /// - [`StopReason::Blocked`]: all work in the machine is blocked at `reached`.
    ///   The caller should record that position and stop scheduling execution
    ///   until an external stimulus arrives or the optional `deadline` becomes
    ///   due. Register that deadline as a possible wakeup; an external stimulus
    ///   may wake the machine earlier. With no deadline, wait for external
    ///   stimulus. Make the wakeup visible through the machine's environment
    ///   before calling `run` again. Do not busy-poll or treat blocking as exit.
    /// - [`StopReason::Exited`]: execution ended at `reached`. The caller should
    ///   record the position and exit reason, remove the machine from runnable
    ///   work. Zero-budget calls may still service lifecycle requests. Exiting does not by itself establish
    ///   whether the simulation's test passed or failed.
    ///
    /// These results describe this machine's progress. The caller must account
    /// for other machines and pending events before advancing shared simulation
    /// time; a returned position or wakeup deadline is not permission to skip
    /// intervening events.
    ///
    /// # Errors
    ///
    /// `Err` indicates that the machine could not execute according to this
    /// contract. Machine shutdown, program exit, and machine crash
    /// are represented by [`StopReason::Exited`] instead. An error does not
    /// promise rollback or a resumable state. The caller must stop scheduling
    /// the machine and propagate the failure rather than blindly retrying.
    fn run(
        &mut self,
        context: &mut RunContext<'_, Self::Context>,
        upto: Timestamp,
    ) -> Result<StopReason, Self::Error>;

    /// Capture the stopped machine without advancing execution.
    ///
    /// # Capture point
    ///
    /// Capture is synchronous and requires a valid, stopped machine: either its
    /// initial state before execution or an accepted boundary returned by
    /// [`Machine::run`]. All captured state must describe that same boundary;
    /// speculative state must not be captured.
    /// The caller must prevent concurrent delivery of external inputs during
    /// capture.
    ///
    /// # Contents and ownership
    ///
    /// The snapshot contains the machine manifest and all machine-owned state
    /// needed to reconstruct the captured machine. This includes its execution
    /// position, runnable/blocked/exited status, pending operations, timers,
    /// disk state (both durable and volatile), pending lifecycle transitions,
    /// and all remaining execution state needed to resume from the capture point.
    ///
    /// Worker resources and dependencies supplied through [`RunContext`] are not
    /// captured as configuration authority. Execution progress and already-charged
    /// work caches are captured; the caller supplies the matching execution
    /// configuration again at restore. RNG continuation belongs
    /// to the driver. Shared simulation time, the external network, and other
    /// machines are not owned by this snapshot. Reproducing a whole simulation requires the caller
    /// to capture and restore that environment consistently as well.
    ///
    /// The returned snapshot must remain valid and unchanged if the source
    /// machine continues running or is destroyed. The snapshot must retain
    /// access to all required data independently of the source machine's live
    /// resources.
    ///
    /// # Effect on the machine
    ///
    /// Capturing must preserve future simulation behavior: it must not execute
    /// machine work, advance time, consume randomness, deliver external effects,
    /// or change which disk writes are durable. The machine remains stopped
    /// in the same lifecycle state.
    ///
    /// # Errors
    ///
    /// `Err` returns no usable snapshot and must leave a valid source machine's
    /// simulation state unchanged. The caller may continue using that machine
    /// or retry capture after addressing the failure. A machine left in an
    /// invalid state by an earlier execution error must reject capture; calling
    /// `snapshot` does not recover it.
    fn snapshot(&mut self) -> Result<Self::Snapshot, Self::Error>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The machine reached the requested timestamp and is stopped.
    BoundaryReached,
    /// All work is blocked until an external stimulus or a timer wakeup.
    Blocked {
        /// The timestamp at which the machine became blocked.
        reached: Timestamp,
        /// The earliest timer wakeup in simulation time, strictly after `reached`.
        /// This may be later than the requested execution boundary. External
        /// stimulus can wake the machine sooner. If `None`, only external
        /// stimulus can wake the machine.
        deadline: Option<Timestamp>,
    },
    /// The machine has exited. Zero-budget runs may accept lifecycle requests;
    /// execution requires an accepted Reboot request.
    Exited {
        /// Accepted boundary at which the exited state was observed.
        /// Later zero-budget observations may adopt a later boundary; callers
        /// retain the original exit occurrence separately.
        reached: Timestamp,
        /// The cause of machine termination, not a simulator failure.
        reason: ExitReason,
    },
}

/// Why execution of the machine ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    /// Shutdown without a program exit status. In a Machine exit this means
    /// the machine requested shutdown. A Driver result also uses this reason
    /// for a confirmed stop of its oracle application; that does not imply
    /// physical machine exit.
    Shutdown,
    /// The program defining the observed lifetime exited with this status.
    /// Machine::run uses this only when the machine also exits. A Driver
    /// result may instead report its oracle application's outcome while the
    /// corresponding machine remains alive.
    ProgramExit { code: i32 },
    /// A failure within the machine or an explicit crash ended execution.
    /// Failures of the simulator or host execution resources use `Machine::Error`.
    Crashed,
}
