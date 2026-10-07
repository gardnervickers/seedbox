//! The PV clock timebase: no host perf counter. Compute between exits costs
//! no virtual time (the infinite-CPU model); time moves only at
//!
//! - exits the guest makes anyway (I/O, MMIO, HLT), with their usual costs,
//! - guest clock reads, 1 ns each as in every timebase (`seedbox_rdtsc`),
//! - syscalls, `SYSCALL_NS` each, counted by the guest in the time page.
//!
//! So a run needs no counted stop: the guest kernel exits by itself when its
//! clock would pass the soft stop (the `bound` we publish) or at the syscall
//! that reaches it (`syscall_target`), both behind a static key that is off in
//! the counter timebase. Every exit is a deterministic point.
//!
//! A loop with no exit at all would never end and never let time move. A
//! watchdog (host CPU time, a performance setting) catches a run with no exit
//! for a while and steps it branch by branch. If the whole machine state
//! repeats, the loop provably never ends by itself: the machine steps to a
//! canonical point of the cycle and is idle there, like HLT, until an
//! interrupt. Any other long run resumes untouched (stepping changes no guest
//! state), so the watchdog changes how fast a spin is noticed, never where it
//! lands. See /mnt/project-files/seedbox-perf/spin-fixed-point.md.

use super::{map, Machine, Mapping, PAGE_BOUND, PAGE_SYSCALLS, PAGE_SYSCALL_TARGET};
use crate::timebase::{Judge, Prepared, Run, Timebase, Verdict};
use crate::undo::VcpuState;
use std::time::Duration;

/// Virtual time one syscall costs. Part of the PV clock's semantics.
pub const SYSCALL_NS: u64 = 1000;
/// Steps one spin check may take before it gives up.
const PROBE_STEPS: usize = 4096;

pub struct PvClock {
    /// Held for its timer.
    _watchdog: Watchdog,
    /// A natural exit happened since the last watchdog tick.
    progressed: bool,
    /// Ticks to let pass before the next check, after checks that found no
    /// spin in a run that still has made no exit (doubling each time).
    skip: u32,
    backoff: u32,
    syscalls0: u64,
    remaining: u64,
    probe: Option<Probe>,
    /// The vCPU stands at a step landing the probe has not seen yet.
    landed: bool,
}

/// A step landing: RIP, then the general registers and flags.
type Key = (u64, [u64; 17]);

enum Probe {
    /// Step and look for a landing RIP seen more than `min_period` steps ago.
    Find { landings: Vec<u64>, min_period: usize, steps: usize },
    /// Step `period` times from `saved` and compare. `last`: the cycle is
    /// already proven, this pins down its least period.
    Verify { saved: Box<Saved>, period: usize, keys: Vec<Key>, steps: usize, last: bool },
    /// A proven cycle: step to its canonical landing, `target`.
    Canon { target: Key, left: usize },
}

impl PvClock {
    /// `watchdog`: host CPU time with no exit before a run is checked for a
    /// spin. Performance only.
    pub fn new(watchdog: Duration) -> Result<Self, String> {
        Ok(Self {
            _watchdog: Watchdog::new(watchdog)?,
            progressed: true,
            skip: 0,
            backoff: 0,
            syscalls0: 0,
            remaining: 0,
            probe: None,
            landed: false,
        })
    }

    /// One probe step at a landing. `None`: the check gave up, run freely.
    fn probe_step(&mut self, m: &mut Machine) -> Result<Option<Prepared>, String> {
        let regs = unsafe { m.vcpu.get_kvm_run().s.regs.regs };
        let key = key(&regs);
        let next = match self.probe.take().unwrap() {
            Probe::Find { mut landings, min_period, steps } => {
                if steps >= PROBE_STEPS || min_period >= PROBE_STEPS {
                    m.stats.spin_false += 1;
                    return Ok(None);
                }
                let older = landings.len().saturating_sub(min_period);
                match landings[..older].iter().rposition(|&r| r == key.0) {
                    Some(i) => Probe::Verify {
                        saved: Box::new(Saved::take(m)?),
                        period: landings.len() - i,
                        keys: Vec::new(),
                        steps,
                        last: false,
                    },
                    None => {
                        landings.push(key.0);
                        Probe::Find { landings, min_period, steps: steps + 1 }
                    }
                }
            }
            Probe::Verify { saved, period, mut keys, steps, last } => {
                keys.push(key);
                let steps = steps + 1;
                if keys.len() < period {
                    Probe::Verify { saved, period, keys, steps, last }
                } else if saved.same(m)? {
                    // The state after `period` steps is the state before them:
                    // the guest repeats this cycle forever. Find its least
                    // period: the landings repeat with it, and it must be
                    // checked like any other.
                    let least = (1..period)
                        .find(|&d| period % d == 0 && (d..period).all(|i| keys[i] == keys[i - d]))
                        .unwrap_or(period);
                    if least < period {
                        Probe::Verify { saved: Box::new(Saved::take(m)?), period: least, keys: Vec::new(), steps, last: true }
                    } else {
                        // Its landings are the same on every pass, so the
                        // least one is a point we reach whichever pass we
                        // noticed on.
                        let target = *keys.iter().min().unwrap();
                        if keys.iter().filter(|&&k| k == target).count() > 1 {
                            return Err(format!(
                                "{}: spin cycle at rip {:#x} has no unique canonical point (equal registers at two points of it)",
                                m.name, target.0
                            ));
                        }
                        if key == target {
                            return Ok(Some(Prepared::Idle));
                        }
                        Probe::Canon { target, left: period }
                    }
                } else if last {
                    return Err(format!(
                        "{}: spin cycle at rip {:#x} has no unique canonical point (its registers repeat before its memory does)",
                        m.name, key.0
                    ));
                } else {
                    // Not a cycle of this length (or not yet one): look for a
                    // longer one from here. Doubling bounds the cost.
                    Probe::Find { landings: vec![key.0], min_period: period * 2, steps }
                }
            }
            Probe::Canon { target, left } => {
                if key == target {
                    return Ok(Some(Prepared::Idle));
                }
                if left == 0 {
                    return Err(format!("{}: lost the spin cycle at rip {:#x}", m.name, target.0));
                }
                Probe::Canon { target, left: left - 1 }
            }
        };
        self.probe = Some(next);
        m.arm_step()?;
        Ok(Some(Prepared::Run))
    }
}

fn key(r: &kvm_bindings::kvm_regs) -> Key {
    let rflags = r.rflags & !(1 << 16); // RF is ours
    (
        r.rip,
        [
            r.rax, r.rbx, r.rcx, r.rdx, r.rsi, r.rdi, r.rsp, r.rbp, r.r8, r.r9, r.r10, r.r11, r.r12,
            r.r13, r.r14, r.r15, rflags,
        ],
    )
}

impl Timebase for PvClock {
    const BRANCHES: bool = false;

    fn prepare(&mut self, m: &mut Machine, remaining: u64, soft: u64) -> Result<Prepared, String> {
        self.remaining = remaining;
        if self.probe.is_some() {
            if !std::mem::take(&mut self.landed) {
                // A watchdog tick interrupted the step: plan it again from here.
                m.arm_step()?;
                return Ok(Prepared::Run);
            }
            match self.probe_step(m)? {
                Some(Prepared::Idle) => {
                    self.probe = None;
                    return Ok(Prepared::Idle);
                }
                Some(p) => return Ok(p),
                None => {
                    self.backoff = (self.backoff * 2).clamp(1, 1 << 10);
                    self.skip = self.backoff;
                }
            }
        }
        m.set_stepping(false)?;
        // Exit when the guest clock would pass the soft stop, or at the
        // syscall whose cost reaches it.
        m.page_set(PAGE_BOUND, soft);
        let syscalls = m.page_get(PAGE_SYSCALLS);
        let left = soft.saturating_sub(m.time).div_ceil(SYSCALL_NS).max(1);
        m.page_set(PAGE_SYSCALL_TARGET, syscalls + left);
        Ok(Prepared::Run)
    }

    fn start(&mut self, syscalls: u64) -> u64 {
        self.syscalls0 = syscalls;
        0
    }

    fn read(&mut self, syscalls: u64) -> u64 {
        (syscalls - self.syscalls0).saturating_mul(SYSCALL_NS).min(self.remaining)
    }

    fn inject_guard(&self) -> u64 {
        0
    }

    fn judge(&mut self, j: Judge<'_>, run: &Run) -> Result<Verdict, String> {
        if run.interrupted {
            // A watchdog tick. Two in a row with no exit between: check.
            if self.skip > 0 {
                self.skip -= 1;
            } else if self.probe.is_none() && !std::mem::take(&mut self.progressed) {
                j.stats.spin_probes += 1;
                self.probe = Some(Probe::Find { landings: Vec::new(), min_period: 0, steps: 0 });
                self.landed = true;
            }
            return Ok(Verdict::Advance { legal: false });
        }
        if run.debug {
            if self.probe.is_none() {
                return Err(format!("{}: debug exit outside a spin check", j.name));
            }
            self.landed = true;
            return Ok(Verdict::Advance { legal: false });
        }
        self.progressed = true;
        self.backoff = 0;
        self.skip = 0;
        if self.probe.take().is_some() {
            j.stats.spin_false += 1;
        }
        Ok(Verdict::Advance { legal: true })
    }
}

/// The machine state a cycle check compares.
struct Saved {
    cpu: VcpuState,
    page: Box<[u8; 4096]>,
    /// A copy of RAM, when no dirty log tracks it.
    ram: Option<Mapping>,
}

impl Saved {
    fn take(m: &mut Machine) -> Result<Self, String> {
        let mut cpu = VcpuState::new(m.msrs.clone());
        cpu.save(&m.vcpu, "spin check")?;
        let ram = match m.undo.as_mut() {
            Some(u) => {
                u.sync(&m._vm, m.mem)?;
                None
            }
            None => {
                let ram = Mapping { ptr: map(m.mem_size)?, size: m.mem_size };
                unsafe { std::ptr::copy_nonoverlapping(m.mem, ram.ptr, m.mem_size) };
                Some(ram)
            }
        };
        Ok(Self { cpu, page: page_copy(m), ram })
    }

    fn same(&self, m: &mut Machine) -> Result<bool, String> {
        let mut cpu = VcpuState::new(m.msrs.clone());
        cpu.save(&m.vcpu, "spin check")?;
        if !self.cpu.same(&cpu) || *self.page != *page_copy(m) {
            return Ok(false);
        }
        match (&self.ram, m.undo.as_mut()) {
            (None, Some(u)) => u.unchanged_since_sync(&m._vm, m.mem),
            (Some(ram), _) => Ok(unsafe {
                std::slice::from_raw_parts(ram.ptr, ram.size)
                    == std::slice::from_raw_parts(m.mem, m.mem_size)
            }),
            (None, None) => Err("spin check: RAM not saved".into()),
        }
    }
}

fn page_copy(m: &Machine) -> Box<[u8; 4096]> {
    let mut page = Box::new([0u8; 4096]);
    unsafe { std::ptr::copy_nonoverlapping(m.page as *const u8, page.as_mut_ptr(), 4096) };
    page
}

/// A periodic signal to this thread, every `period` of its CPU time, so a
/// KVM_RUN that makes no exit returns with EINTR.
struct Watchdog(libc::timer_t);

extern "C" fn on_tick(_: libc::c_int) {}

impl Watchdog {
    fn new(period: Duration) -> Result<Self, String> {
        let err = |what: &str| format!("pv watchdog {what}: {}", std::io::Error::last_os_error());
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_tick as *const () as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            if libc::sigaction(libc::SIGUSR2, &sa, std::ptr::null_mut()) != 0 {
                return Err(err("sigaction"));
            }
            let mut sev: libc::sigevent = std::mem::zeroed();
            sev.sigev_notify = libc::SIGEV_THREAD_ID;
            sev.sigev_signo = libc::SIGUSR2;
            sev.sigev_notify_thread_id = libc::syscall(libc::SYS_gettid) as i32;
            let mut timer: libc::timer_t = std::ptr::null_mut();
            if libc::timer_create(libc::CLOCK_THREAD_CPUTIME_ID, &mut sev, &mut timer) != 0 {
                return Err(err("timer"));
            }
            let w = Self(timer);
            let ts = libc::timespec {
                tv_sec: period.as_secs() as libc::time_t,
                tv_nsec: period.subsec_nanos() as libc::c_long,
            };
            let spec = libc::itimerspec { it_interval: ts, it_value: ts };
            if libc::timer_settime(timer, 0, &spec, std::ptr::null_mut()) != 0 {
                return Err(err("arm"));
            }
            Ok(w)
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        unsafe { libc::timer_delete(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::super::{BootSpec, Stop, INITRD_ADDR};
    use super::*;
    use kvm_ioctls::Kvm;

    /// A 32-bit payload at the kernel entry, interrupts enabled and unmasked.
    fn machine(kvm: &Kvm, code: &[u8], undo: u64) -> Machine {
        let mut kernel = vec![0u8; 1024];
        kernel[0x1f1] = 1;
        kernel.extend(code);
        let spec = BootSpec {
            kernel: &kernel, initrd: &[], cmdline: String::new(), mem: INITRD_ADDR + 0x100000,
            rng_seed: [0; 32], mac: None, control_channel: false, undo_margin: undo, barrier: false,
            pv_clock: true,
        };
        let mut m = Machine::new(kvm, "pv-probe", &spec).unwrap();
        let mut r = m.vcpu.get_regs().unwrap();
        r.rflags = 0x202;
        m.vcpu.set_regs(&r).unwrap();
        m.pic.set_masks(0);
        m
    }

    /// Run to the first stop under several watchdog periods and undo
    /// settings; all must agree. Returns the stop, rip, rcx and the machine.
    fn run(code: &[u8]) -> (Stop, u64, u64, Machine) {
        let kvm = Kvm::new().unwrap();
        let mut first: Option<(u64, u64, u64, String)> = None;
        let mut last = None;
        for (ms, undo) in [(2, 256), (7, 0), (3, 0)] {
            let mut clock = PvClock::new(Duration::from_millis(ms)).unwrap();
            let mut m = machine(&kvm, code, undo);
            let stop = m.run_until(&mut clock, 1_000_000_000, 0, &mut Vec::new()).unwrap();
            let r = m.vcpu.get_regs().unwrap();
            let evidence = (m.time, r.rip, r.rcx, m.exit_witness());
            if let Some(want) = &first {
                assert_eq!(&evidence, want, "watchdog {ms} ms undo {undo}");
            } else {
                first = Some(evidence);
            }
            last = Some((stop, r.rip, r.rcx, m));
        }
        last.unwrap()
    }

    #[test]
    #[ignore = "requires KVM"]
    fn a_jump_to_itself_is_idle_at_the_jump() {
        let (stop, rip, _, m) = run(&[0xeb, 0xfe]); // jmp $
        assert_eq!(stop, Stop::Halted);
        assert_eq!(rip, 0x100000);
        assert_eq!(m.time, 0, "compute costs no virtual time");
        assert_eq!(m.stats.spin_idles, 1);
    }

    #[test]
    #[ignore = "requires KVM"]
    fn a_cycle_with_an_inner_loop_is_idle_at_its_canonical_point() {
        // outer: mov ecx,5; inner: dec ecx; jnz inner; jmp outer
        let (stop, rip, rcx, m) = run(&[0xb9, 5, 0, 0, 0, 0x49, 0x75, 0xfd, 0xeb, 0xf6]);
        assert_eq!(stop, Stop::Halted);
        // The least landing: inner head, with ecx at its least there.
        assert_eq!((rip, rcx as u32), (0x100005, 1));
        assert_eq!(m.stats.spin_idles, 1);
    }

    #[test]
    #[ignore = "requires KVM"]
    fn a_long_finite_loop_is_left_alone() {
        // mov ecx,0x20000000; l: dec ecx; jnz l; hlt
        let (stop, rip, rcx, m) = run(&[0xb9, 0, 0, 0, 0x20, 0x49, 0x75, 0xfd, 0xf4]);
        assert_eq!(stop, Stop::Halted);
        assert_eq!((rip, rcx as u32), (0x100009, 0));
        assert_eq!(m.stats.spin_idles, 0);
        assert!(m.stats.spin_probes > 0, "the loop should outlast the watchdog");
        assert_eq!(m.stats.spin_probes, m.stats.spin_false);
    }
}
