//! Prepared Linux/x86-64 adapter for kvm-ioctls 0.25 / bindings 0.14.1.
//! Not linked into the retained runner, compiled on Linux, or hardware-tested.
//! Caller must provide W5a's validated cumulative guest count, complete pending
//! emulation first, and record all semantic device exits encountered during
//! correction. This helper never changes TF/IF, guest RIP, time or seed streams.
//! The record identifies KVM_INTERRUPT acceptance at the stopped CPU. Guest
//! handler entry and acknowledgement require a separate observer.
use super::progress::{Delivery, InterruptQueue, LegalWindow, Owner, StopPhase};
use kvm_bindings::kvm_interrupt;
use kvm_ioctls::VcpuFd;
use std::os::fd::AsRawFd;

#[derive(Debug, Eq, PartialEq)]
pub enum Applied {
    CompleteIoFirst,
    AdministrativeStop,
    NoneDue,
    StaleRejected(Owner),
    WindowRequested,
    Injected {
        vector: u8,
        request: u64,
        actual_count: u64,
        actual_rip: u64,
    },
}

/// Enqueue is performed separately when the caller-owned clockevent device
/// observes the guest's one-shot programming. `validated_count` is unavailable
/// until W5a qualification; absence fails closed, rather than becoming zero.
pub fn deliver(
    cpu: &mut VcpuFd,
    queue: &mut InterruptQueue,
    phase: StopPhase,
    now_ns: u64,
    owner: Owner,
    validated_count: Option<u64>,
) -> Result<Applied, String> {
    if phase == StopPhase::PendingEmulatedIo {
        return Ok(Applied::CompleteIoFirst);
    }
    if phase == StopPhase::DebugCorrectionAdministration {
        return Ok(Applied::AdministrativeStop);
    }
    let regs = cpu.get_regs().map_err(|e| e.to_string())?;
    let events = cpu.get_vcpu_events().map_err(|e| e.to_string())?;
    let window = LegalWindow {
        kvm_ready: cpu.get_kvm_run().ready_for_interrupt_injection != 0,
        if_enabled: regs.rflags & (1 << 9) != 0,
        interrupt_shadow: events.interrupt.shadow != 0,
    };
    match queue.decision(phase, now_ns, owner, window) {
        Delivery::CompleteIoFirst => Ok(Applied::CompleteIoFirst),
        Delivery::AdministrationOnly => Ok(Applied::AdministrativeStop),
        Delivery::NoneDue => Ok(Applied::NoneDue),
        Delivery::RejectStale(stale) => {
            queue.reject_stale(stale).map_err(|e| format!("{e:?}"))?;
            Ok(Applied::StaleRejected(stale))
        }
        Delivery::RequestWindow(_) => {
            cpu.get_kvm_run().request_interrupt_window = 1;
            Ok(Applied::WindowRequested)
        }
        Delivery::Inject(request) => {
            let actual_count = validated_count.ok_or("validated guest count unavailable")?;
            let interrupt = kvm_interrupt {
                irq: u32::from(request.vector),
            };
            // Linux UAPI _IOW(KVMIO=0xae,0x86,struct kvm_interrupt).
            let rc =
                unsafe { libc::ioctl(cpu.as_raw_fd(), 0x4004ae86 as libc::c_ulong, &interrupt) };
            if rc != 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            cpu.get_kvm_run().request_interrupt_window = 0;
            queue
                .injected(request.owner, actual_count, now_ns, regs.rip)
                .map_err(|e| format!("{e:?}"))?;
            Ok(Applied::Injected {
                vector: request.vector,
                request: request.owner.request,
                actual_count,
                actual_rip: regs.rip,
            })
        }
    }
}
