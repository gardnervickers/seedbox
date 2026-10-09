//! Linux system queries only: no VM creation, MSR write, event enable or signals.
use super::*;
use std::collections::BTreeMap;
use std::{
    fs::{self, File},
    io::Read,
    os::fd::{AsRawFd, FromRawFd},
    os::unix::fs::FileExt,
};

struct Affinity {
    original: libc::cpu_set_t,
    active: bool,
}
impl Affinity {
    fn pin(cpu: u32) -> Result<Self> {
        if cpu >= libc::CPU_SETSIZE as u32 {
            return Err(Error::Setup("CPU exceeds affinity mask".into()));
        }
        // SAFETY: cpu_set_t is a plain bit array; all-zero is the empty set.
        let mut original = unsafe { std::mem::zeroed() };
        // SAFETY: the size passed is the size of the writable mask.
        if unsafe { libc::sched_getaffinity(0, std::mem::size_of_val(&original), &mut original) }
            != 0
        {
            return Err(Error::Setup(format!(
                "read thread affinity: {}",
                std::io::Error::last_os_error()
            )));
        }
        // SAFETY: as for `original`.
        let mut pinned = unsafe { std::mem::zeroed() };
        // SAFETY: `cpu` was checked against CPU_SETSIZE above.
        unsafe {
            libc::CPU_ZERO(&mut pinned);
            libc::CPU_SET(cpu as usize, &mut pinned);
        }
        // SAFETY: the size passed is the size of the mask.
        if unsafe { libc::sched_setaffinity(0, std::mem::size_of_val(&pinned), &pinned) } != 0 {
            return Err(Error::Setup(format!(
                "CPU {cpu} temporary affinity: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(Self {
            original,
            active: true,
        })
    }
    fn restore(&mut self) -> Result<()> {
        if self.active {
            let expected = affinity_bits(&self.original);
            checked_affinity_restore(
                &expected,
                || {
                    // SAFETY: the size passed is the size of the saved mask.
                    if unsafe {
                        libc::sched_setaffinity(
                            0,
                            std::mem::size_of_val(&self.original),
                            &self.original,
                        )
                    } != 0
                    {
                        return Err(Error::Execution(format!(
                            "restore preflight affinity: {}",
                            std::io::Error::last_os_error()
                        )));
                    }
                    Ok(())
                },
                || {
                    // SAFETY: as for `original` in `pin`.
                    let mut restored = unsafe { std::mem::zeroed() };
                    // SAFETY: the size passed is the size of the writable mask.
                    if unsafe {
                        libc::sched_getaffinity(0, std::mem::size_of_val(&restored), &mut restored)
                    } != 0
                    {
                        return Err(Error::Execution(format!(
                            "read restored preflight affinity: {}",
                            std::io::Error::last_os_error()
                        )));
                    }
                    Ok(affinity_bits(&restored))
                },
            )?;
            self.active = false;
        }
        Ok(())
    }
}
fn affinity_bits(mask: &libc::cpu_set_t) -> Vec<u32> {
    (0..libc::CPU_SETSIZE as u32)
        // SAFETY: every index is below CPU_SETSIZE.
        .filter(|cpu| unsafe { libc::CPU_ISSET(*cpu as usize, mask) })
        .collect()
}
impl Drop for Affinity {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            eprintln!("seedbox: {error}");
        }
    }
}

fn text(path: &str) -> Result<String> {
    let mut data = String::new();
    File::open(path)
        .and_then(|file| file.take(2 * 1024 * 1024 + 1).read_to_string(&mut data))
        .context(Error::Setup, format_args!("host admission {path}"))?;
    if data.len() > 2 * 1024 * 1024 {
        return Err(Error::Setup(format!(
            "host admission {path}: exceeds read bound"
        )));
    }
    Ok(data)
}
fn query(file: &File, command: libc::c_ulong, argument: libc::c_ulong) -> Result<i32> {
    // SAFETY: callers pass only KVM_GET_API_VERSION and KVM_CHECK_EXTENSION,
    // whose argument is an integer, never a pointer.
    let result = unsafe { libc::ioctl(file.as_raw_fd(), command, argument) };
    if result < 0 {
        return Err(Error::Setup(format!(
            "KVM admission query: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(result)
}

// Matches the VMM counter.rs::PerfAttr; no enable/fcntl/mmap follows.
#[repr(C)]
#[derive(Default)]
struct PerfAttr {
    kind: u32,
    size: u32,
    config: u64,
    period: u64,
    sample_type: u64,
    read_format: u64,
    flags: u64,
    rest: [u64; 10],
}
fn open_disabled_perf(spec: DisabledCounterSpec) -> Result<File> {
    let attr = PerfAttr {
        kind: spec.kind,
        size: std::mem::size_of::<PerfAttr>() as u32,
        config: spec.config,
        period: spec.period,
        read_format: spec.read_format,
        flags: spec.flags,
        ..Default::default()
    };
    // SAFETY: `attr` is a live perf_event_attr whose `size` field is its own
    // size; the kernel only reads it.
    let fd = unsafe { libc::syscall(libc::SYS_perf_event_open, &attr, 0, -1, -1, spec.open_flags) }
        as i32;
    if fd < 0 {
        return Err(Error::Setup(format!(
            "disabled exact-event perf admission {:#x}: {}; run as root on a supported KVM host",
            spec.config,
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: perf_event_open succeeded, so `fd` is a new descriptor nothing else owns.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn collect(cpu: u32, counter: crate::counter::CounterProfile) -> Result<CoreObservation> {
    // SAFETY: sched_getcpu takes no arguments.
    if unsafe { libc::sched_getcpu() } != cpu as i32 {
        return Err(Error::Execution("preflight CPU affinity changed".into()));
    }
    let cpuinfo = text("/proc/cpuinfo")?;
    let fields = cpuinfo
        .split("\n\n")
        .find_map(|block| {
            let fields: BTreeMap<_, _> = block
                .lines()
                .filter_map(|line| line.split_once(':'))
                .map(|(k, v)| (k.trim(), v.trim()))
                .collect();
            (fields.get("processor").and_then(|s| s.parse::<u32>().ok()) == Some(cpu))
                .then_some(fields)
        })
        .ok_or_else(|| Error::Setup(format!("CPU {cpu} missing from /proc/cpuinfo")))?;
    let field = |name: &str| {
        fields
            .get(name)
            .copied()
            .ok_or_else(|| Error::Setup(format!("CPU {cpu} missing {name}")))
    };
    let number = |name: &str| -> Result<u32> {
        field(name)?
            .parse()
            .map_err(|_| Error::Setup(format!("CPU {cpu} invalid {name}")))
    };
    let mut original = [0; 8];
    if let Some(workaround) = counter.workaround() {
        let msr_path = format!("/dev/cpu/{cpu}/msr");
        let msr = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&msr_path)
            .map_err(|e| {
                Error::Setup(format!(
                    "{msr_path}: {e}; run as root with the msr module loaded"
                ))
            })?;
        if msr
            .read_at(&mut original, workaround.register)
            .map_err(|e| Error::Setup(format!("{msr_path}: {e}; restore admitted MSR access")))?
            != 8
        {
            return Err(Error::Setup(format!(
                "{msr_path}: short workaround MSR read"
            )));
        }
    }
    let kvm = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .context(Error::Setup, "/dev/kvm admission")?;
    // KVM UAPI header plus at most 1024 complete 40-byte entries, aligned for ioctl.
    let mut words = vec![0u64; 1 + 1024 * 5];
    words[0] = 1024;
    // SAFETY: KVM_GET_SUPPORTED_CPUID writes at most `words[0]` 40-byte
    // entries after the 8-byte header, and `words` has room for exactly that.
    if unsafe {
        libc::ioctl(
            kvm.as_raw_fd(),
            0xc008ae05 as libc::c_ulong,
            words.as_mut_ptr(),
        )
    } < 0
    {
        return Err(Error::Setup(format!(
            "KVM supported CPUID: {}",
            std::io::Error::last_os_error()
        )));
    }
    let count = words[0] as u32 as usize;
    if count > 1024 {
        return Err(Error::Setup("KVM CPUID capacity exceeded".into()));
    }
    let raw_cpuid = (0..count)
        // SAFETY: `index < count <= 1024`, so the entry lies inside `words`; the
        // u64 buffer is suitably aligned and CpuidEntry is plain data.
        .map(|index| unsafe {
            words
                .as_ptr()
                .add(1 + index * 5)
                .cast::<CpuidEntry>()
                .read()
        })
        .collect();
    let microcode = field("microcode")?.trim_start_matches("0x");
    Ok(CoreObservation {
        cpu,
        architecture: "x86_64".into(),
        vendor: field("vendor_id")?.into(),
        family: number("cpu family")?,
        model: number("model")?,
        stepping: number("stepping")?,
        microcode: u64::from_str_radix(microcode, 16)
            .map_err(|_| Error::Setup(format!("CPU {cpu} invalid microcode")))?,
        kvm_api: query(&kvm, 0xae00, 0)?,
        immediate_exit: query(&kvm, 0xae03, 136)?,
        guest_debug: query(&kvm, 0xae03, 23)?,
        sync_regs: query(&kvm, 0xae03, 74)?,
        perf_type: text("/sys/bus/event_source/devices/cpu/type")?
            .trim()
            .parse()
            .map_err(|_| Error::Setup("invalid CPU PMU type".into()))?,
        perf_disabled_open: false,
        perf_error: None,
        workaround_msr: u64::from_le_bytes(original),
        raw_cpuid,
    })
}

pub(super) fn observe(
    cpu: u32,
    counter: crate::counter::CounterProfile,
) -> Result<CoreObservation> {
    let affinity = Affinity::pin(cpu)?;
    scoped_probe(
        affinity,
        || {
            // The shared check preserves static evidence and closes any disabled
            // event descriptor before affinity restoration.
            if crate::counter::host()? != counter {
                return Err(Error::Setup(format!(
                    "CPU {cpu}: branch counter differs from selected profile"
                )));
            }
            Ok(probe_disabled_perf(collect(cpu, counter)?, || {
                probe_selected_event(counter, open_disabled_perf)
            }))
        },
        Affinity::restore,
    )
}
