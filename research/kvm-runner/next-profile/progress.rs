//! Off-host preparation only. No KVM/perf backend, calibrated hardware factor,
//! guest clocksource, guest vPMC or snapshot implementation is claimed here.
//! Guest progress is imported once at semantic stops; reads charge nothing.
//! These are machine-local clocks. The world must establish cross-machine
//! progress bounds before publishing them as globally committed time.

pub const HOST_KERNEL: &str = "6.18.54";
pub const HOST_MICROCODE: u64 = 0x0a20102e;
pub const PAGE_BYTES: usize = 128;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Error {
    HostChanged,
    InvalidCalibration,
    CalibrationNotQualified,
    CounterWentBackwards,
    TimeWentBackwards,
    Overflow,
    EpochChanged,
    DuplicateRequest,
    UnknownRequest,
}

pub fn check_host(kernel: &str, microcodes: &[u64]) -> Result<(), Error> {
    if kernel != HOST_KERNEL
        || microcodes.is_empty()
        || microcodes.iter().any(|m| *m != HOST_MICROCODE)
    {
        return Err(Error::HostChanged);
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Qualification {
    SyntheticSmoke,
    NativeBenchmark,
}

/// Numerator is native workload CPU nanoseconds, denominator is its retired
/// conditional branches. Wall/idle time is deliberately not an input. The
/// matching pinned native event must include both user and kernel CPU work.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Calibration {
    pub native_cpu_ns: u64,
    pub native_branches: u64,
    pub benchmark_input: String,
    pub counter_profile: String,
    pub native_evidence: String,
    pub qualification: Qualification,
}

impl Calibration {
    pub fn validate(&self) -> Result<(), Error> {
        if self.native_cpu_ns == 0
            || self.native_branches == 0
            || self.benchmark_input.is_empty()
            || self.counter_profile.is_empty()
            || self.native_evidence.is_empty()
        {
            return Err(Error::InvalidCalibration);
        }
        Ok(())
    }

    pub fn require_qualified(&self) -> Result<(), Error> {
        self.validate()?;
        if self.qualification != Qualification::NativeBenchmark {
            return Err(Error::CalibrationNotQualified);
        }
        // This tag alone does not authenticate measurements. The caller must
        // verify all referenced retained inputs/evidence before admitting it.
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CountClock {
    pub calibration: Calibration,
    pub epoch: u64,
    pub last_count: u64,
    pub now_ns: u64,
    pub remainder: u64,
}

impl CountClock {
    pub fn new(calibration: Calibration, epoch: u64, count: u64, ns: u64) -> Result<Self, Error> {
        calibration.validate()?;
        Ok(Self {
            calibration,
            epoch,
            last_count: count,
            now_ns: ns,
            remainder: 0,
        })
    }

    /// Includes all guest instructions counted during debugger correction.
    /// Passing the same cumulative count twice charges zero. No wall time,
    /// syscall count, VM-exit count or clock-read count enters this conversion.
    pub fn import(&mut self, epoch: u64, count: u64) -> Result<u64, Error> {
        self.calibration.validate()?;
        if self.remainder >= self.calibration.native_branches {
            return Err(Error::InvalidCalibration);
        }
        if epoch != self.epoch {
            return Err(Error::EpochChanged);
        }
        let delta = count
            .checked_sub(self.last_count)
            .ok_or(Error::CounterWentBackwards)?;
        let scaled = u128::from(delta) * u128::from(self.calibration.native_cpu_ns)
            + u128::from(self.remainder);
        let den = u128::from(self.calibration.native_branches);
        let elapsed = u64::try_from(scaled / den).map_err(|_| Error::Overflow)?;
        let now = self.now_ns.checked_add(elapsed).ok_or(Error::Overflow)?;
        self.remainder = (scaled % den) as u64;
        self.last_count = count;
        self.now_ns = now;
        Ok(elapsed)
    }

    /// Only the owning world may invoke this after establishing that every
    /// relevant participant is blocked and choosing the earliest represented
    /// event. Preserve the fractional branch-cost remainder across idle time.
    pub fn idle_jump(&mut self, earliest_ns: u64) -> Result<(), Error> {
        if earliest_ns < self.now_ns {
            return Err(Error::TimeWentBackwards);
        }
        self.now_ns = earliest_ns;
        Ok(())
    }

    /// Earliest additional branch count reaching a local deadline. Zero-count
    /// computation cannot advance this clock: such paths need a declared bound
    /// or another validated progress source, never a host-wall-clock charge.
    pub fn branches_until(&self, deadline_ns: u64) -> Result<u64, Error> {
        self.calibration.validate()?;
        if self.remainder >= self.calibration.native_branches {
            return Err(Error::InvalidCalibration);
        }
        if deadline_ns <= self.now_ns {
            return Ok(0);
        }
        let required =
            u128::from(deadline_ns - self.now_ns) * u128::from(self.calibration.native_branches);
        let required = required.saturating_sub(u128::from(self.remainder));
        let num = u128::from(self.calibration.native_cpu_ns);
        u64::try_from(required.div_ceil(num)).map_err(|_| Error::Overflow)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Owner {
    pub machine: u64,
    pub incarnation: u64,
    pub request: u64,
}

/// Caller writes the page only while this vCPU is stopped; guest mapping must
/// be read-only and excluded from allocatable RAM. Snapshot these bytes and
/// CountClock, including remainder/epoch and the actual calibrated profile.
/// Publication sequence is an even, pinned world registration sequence.
pub fn clock_page(
    clock: &CountClock,
    owner: Owner,
    sequence: u64,
    profile_digest: [u8; 32],
) -> Result<[u8; PAGE_BYTES], Error> {
    if sequence & 1 != 0 {
        return Err(Error::InvalidCalibration);
    }
    let mut page = [0; PAGE_BYTES];
    let flags: u32 = if clock.calibration.qualification == Qualification::NativeBenchmark {
        1
    } else {
        0
    };
    page[0..4].copy_from_slice(&1u32.to_le_bytes());
    page[4..8].copy_from_slice(&flags.to_le_bytes());
    for (at, value) in [
        (8, sequence),
        (16, owner.machine),
        (24, owner.incarnation),
        (32, clock.epoch),
        (40, clock.last_count),
        (48, clock.now_ns),
        (56, clock.calibration.native_cpu_ns),
        (64, clock.calibration.native_branches),
    ] {
        page[at..at + 8].copy_from_slice(&value.to_le_bytes());
    }
    page[72..104].copy_from_slice(&profile_digest);
    Ok(page)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopPhase {
    PendingEmulatedIo,
    NaturalSemanticBoundary,
    ExactPreemptionBoundary,
    InterruptWindowAdministration,
    DebugCorrectionAdministration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LegalWindow {
    pub kvm_ready: bool,
    pub if_enabled: bool,
    pub interrupt_shadow: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InterruptRequest {
    pub owner: Owner,
    pub registration: u64,
    pub deadline_ns: u64,
    pub requested_count: u64,
    pub vector: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SeededTimerPlan {
    pub programmed_deadline_ns: u64,
    pub maximum_lateness_counts: u64,
    pub choice: u64,
    pub request: InterruptRequest,
}

/// Busy-vCPU timer path: derive an exact stop/injection request from Linux's
/// programmed deadline. A nonzero jitter allowance is an explicit timing model
/// or fault-policy input, never a default shortcut. It varies a real queued IRQ
/// target rather than merely pausing/resuming the same guest. Idle-vCPU timers
/// use the world's earliest-deadline jump, not future branch targets.
pub fn seeded_timer(
    clock: &CountClock,
    owner: Owner,
    registration: u64,
    programmed_ns: u64,
    vector: u8,
    choice: u64,
    maximum_lateness_counts: u64,
) -> Result<SeededTimerPlan, Error> {
    let earliest_count = clock
        .last_count
        .checked_add(clock.branches_until(programmed_ns)?)
        .ok_or(Error::Overflow)?;
    let span = maximum_lateness_counts
        .checked_add(1)
        .ok_or(Error::Overflow)?;
    let target_count = earliest_count
        .checked_add(choice % span)
        .ok_or(Error::Overflow)?;
    let mut target_clock = clock.clone();
    target_clock.import(clock.epoch, target_count)?;
    Ok(SeededTimerPlan {
        programmed_deadline_ns: programmed_ns,
        maximum_lateness_counts,
        choice,
        request: InterruptRequest {
            owner,
            registration,
            deadline_ns: target_clock.now_ns,
            requested_count: target_count,
            vector,
        },
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Delivery {
    CompleteIoFirst,
    AdministrationOnly,
    NoneDue,
    RejectStale(Owner),
    RequestWindow(InterruptRequest),
    Inject(InterruptRequest),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InjectionRecord {
    pub request: InterruptRequest,
    pub actual_count: u64,
    pub actual_ns: u64,
    pub actual_rip: u64,
}

/// Plain snapshotable data. Enqueueing a timer, stopping a vCPU, requesting a
/// legal window and injecting the IRQ are separate operations. A host stop
/// alone never makes Linux's scheduler switch tasks.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InterruptQueue {
    pub pending: Vec<InterruptRequest>,
    pub injected: Vec<InjectionRecord>,
}

impl InterruptQueue {
    pub fn enqueue(&mut self, request: InterruptRequest) -> Result<(), Error> {
        if self.pending.iter().any(|r| r.owner == request.owner)
            || self
                .injected
                .iter()
                .any(|r| r.request.owner == request.owner)
        {
            return Err(Error::DuplicateRequest);
        }
        self.pending.push(request);
        self.pending
            .sort_by_key(|r| (r.deadline_ns, r.registration));
        Ok(())
    }

    pub fn decision(
        &self,
        phase: StopPhase,
        now: u64,
        owner: Owner,
        window: LegalWindow,
    ) -> Delivery {
        if phase == StopPhase::PendingEmulatedIo {
            return Delivery::CompleteIoFirst;
        }
        if phase == StopPhase::DebugCorrectionAdministration {
            return Delivery::AdministrationOnly;
        }
        let Some(r) = self.pending.iter().find(|r| r.deadline_ns <= now) else {
            return Delivery::NoneDue;
        };
        if (r.owner.machine, r.owner.incarnation) != (owner.machine, owner.incarnation) {
            return Delivery::RejectStale(r.owner);
        }
        if window.kvm_ready && window.if_enabled && !window.interrupt_shadow {
            Delivery::Inject(r.clone())
        } else {
            Delivery::RequestWindow(r.clone())
        }
    }

    /// Invoke only after KVM_INTERRUPT succeeds at a legal window. Save both
    /// requested and actual identity; IRQ-disabled guest work can make delivery
    /// later than its requested deadline/count without permitting forced IF.
    pub fn injected(&mut self, owner: Owner, count: u64, ns: u64, rip: u64) -> Result<(), Error> {
        let at = self
            .pending
            .iter()
            .position(|r| r.owner == owner)
            .ok_or(Error::UnknownRequest)?;
        let request = self.pending.remove(at);
        self.injected.push(InjectionRecord {
            request,
            actual_count: count,
            actual_ns: ns,
            actual_rip: rip,
        });
        Ok(())
    }

    pub fn reject_stale(&mut self, stale: Owner) -> Result<(), Error> {
        let at = self
            .pending
            .iter()
            .position(|r| r.owner == stale)
            .ok_or(Error::UnknownRequest)?;
        self.pending.remove(at);
        Ok(())
    }
}

/// W1 supplies the seed-stream choice and semantic-event registration. This
/// helper owns no RNG/world. Stop selection varies by seed; it must still be
/// capped by the caller's proven cross-machine/event progress bound. Delivering
/// a Linux timer IRQ is a separate queue operation, not an effect of this plan.
pub fn seeded_stop(
    count: u64,
    min_slice: u64,
    max_slice: u64,
    choice: u64,
    bound: u64,
) -> Result<u64, Error> {
    if min_slice == 0 || max_slice < min_slice || bound < count {
        return Err(Error::InvalidCalibration);
    }
    let span = max_slice
        .checked_sub(min_slice)
        .and_then(|n| n.checked_add(1))
        .ok_or(Error::Overflow)?;
    let slice = min_slice + choice % span;
    Ok(count.checked_add(slice).ok_or(Error::Overflow)?.min(bound))
}
