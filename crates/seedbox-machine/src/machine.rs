//! Machine execution, lifecycle, and snapshot contracts.

#[cfg(unix)]
pub mod tcg;

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
    /// Context under which this machine runs. This is used to associate
    /// a machine with a specific kind of worker.
    type Context;
    type Error;

    /// A self-contained capture of the manifest and machine-owned state.
    /// See [`Machine::snapshot`] for the capture and ownership guarantees.
    type Snapshot;

    /// Execute until `upto`, all work becomes blocked, or the machine exits.
    ///
    /// This call is synchronous: it returns with execution stopped. The machine
    /// retains its execution state for a later call; `context` supplies the
    /// compatible worker's execution resources.
    ///
    /// # Time and execution boundaries
    ///
    /// All timestamps use simulation time, before guest clock offset or skew.
    /// The caller must provide `upto` at or after the machine's current execution
    /// position. Execution must not expose effects beyond `upto`. Returned
    /// `reached` timestamps never move backwards or exceed `upto`.
    ///
    /// A successful return describes an accepted simulation boundary, with no
    /// speculative state or effects exposed to the caller.
    /// A blocked machine may return without making progress.
    ///
    /// # Handling the result
    ///
    /// - [`StopReason::BoundaryReached`]: the machine reached `upto` and is
    ///   paused. The caller should record that position, process events due at
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
    ///   work, and stop calling `run` on it. Exiting does not by itself establish
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
        context: &mut Self::Context,
        upto: Timestamp,
    ) -> Result<StopReason, Self::Error>;

    /// Crash the machine at a simulation barrier, without graceful shutdown.
    ///
    /// # Preconditions
    ///
    /// The caller must hold the machine at a simulation barrier, with no active
    /// `run` call or concurrent delivery of external inputs. The machine must
    /// be in a valid state and must not have exited.
    ///
    /// # Effect and caller responsibilities
    ///
    /// This operation is synchronous: `Ok(())` means the machine has exited
    /// with [`ExitReason::Crashed`]. It must not execute further machine work or
    /// advance simulation time. Each machine defines and documents how a crash
    /// affects its disk state. Previously committed external effects are not
    /// undone, and existing snapshots remain unchanged.
    ///
    /// The caller must remove the machine from runnable work. Further execution
    /// requires an explicit restart; calling `run` is not a restart. The caller
    /// records the crash at the barrier where it applied this operation, without
    /// calling `run` to obtain an exit result.
    ///
    /// # Errors
    ///
    /// `Err` does not establish that the crash completed or that the machine is
    /// resumable. The caller must stop scheduling it and propagate the failure.
    fn crash(&mut self) -> Result<(), Self::Error>;

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
    /// disk state (both durable and volatile), RNG state, and all remaining
    /// execution state needed to resume from the capture point.
    ///
    /// Worker resources supplied through [`Machine::Context`] are not captured.
    /// They are supplied again for execution.
    /// Shared simulation time, the external network, and other machines are not
    /// owned by this snapshot. Reproducing a whole simulation requires the caller
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
    /// The machine was able to run successfully to the requested timestamp and is now paused.
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
    /// The machine has exited. Calling `run` again requires an explicit restart.
    Exited {
        /// The machine has exited at this timestamp.
        reached: Timestamp,
        /// The cause of machine termination, not a simulator failure.
        reason: ExitReason,
    },
}

/// Why execution of the machine ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    /// The machine requested shutdown without a program exit status.
    Shutdown,
    /// The program defining the machine's lifetime exited with this status.
    /// Exiting work that leaves the machine running does not qualify.
    ProgramExit { code: i32 },
    /// A failure within the machine or an explicit crash ended execution.
    /// Failures of the simulator or host execution resources use `Machine::Error`.
    Crashed,
}
