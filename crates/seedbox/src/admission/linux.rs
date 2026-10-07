//! Linux system queries only: no VM creation, MSR write, event enable or signals.
use super::*;
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
        let mut original = unsafe { std::mem::zeroed() };
        if unsafe { libc::sched_getaffinity(0, std::mem::size_of_val(&original), &mut original) }
            != 0
        {
            return Err(Error::Setup(format!(
                "read thread affinity: {}",
                std::io::Error::last_os_error()
            )));
        }
        let mut pinned = unsafe { std::mem::zeroed() };
        unsafe {
            libc::CPU_ZERO(&mut pinned);
            libc::CPU_SET(cpu as usize, &mut pinned);
        }
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
            if unsafe {
                libc::sched_setaffinity(0, std::mem::size_of_val(&self.original), &self.original)
            } != 0
            {
                return Err(Error::Execution(format!(
                    "restore preflight affinity: {}",
                    std::io::Error::last_os_error()
                )));
            }
            self.active = false;
        }
        Ok(())
    }
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
        .map_err(|e| Error::Setup(format!("host admission {path}: {e}")))?;
    if data.len() > 2 * 1024 * 1024 {
        return Err(Error::Setup(format!(
            "host admission {path}: exceeds read bound"
        )));
    }
    Ok(data)
}
fn kernel(path: &str) -> Result<String> {
    let mut file =
        File::open(path).map_err(|e| Error::Setup(format!("host admission {path}: {e}")))?;
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    let mut chunk = [0; 65536];
    let mut size = 0;
    loop {
        let n = file
            .read(&mut chunk)
            .map_err(|e| Error::Setup(format!("host admission {path}: {e}")))?;
        if n == 0 {
            break;
        }
        size += n;
        if size > 256 * 1024 * 1024 {
            return Err(Error::Setup(format!(
                "host admission {path}: kernel exceeds read bound"
            )));
        }
        hash.update(&chunk[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn query(file: &File, command: libc::c_ulong, argument: libc::c_ulong) -> Result<i32> {
    let result = unsafe { libc::ioctl(file.as_raw_fd(), command, argument) };
    if result < 0 {
        return Err(Error::Setup(format!(
            "KVM admission query: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(result)
}

// Matches engine168ad counter.rs::PerfAttr exactly; no enable/fcntl/mmap follows.
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
fn disabled_perf() -> Result<File> {
    let policy = required_policy()?;
    let attr = PerfAttr {
        kind: policy.perf_type,
        size: std::mem::size_of::<PerfAttr>() as u32,
        config: policy.perf_config,
        period: policy.perf_period,
        read_format: policy.perf_read_format,
        flags: policy.perf_flags,
        ..Default::default()
    };
    let fd = unsafe { libc::syscall(libc::SYS_perf_event_open, &attr, 0, -1, -1, 0) } as i32;
    if fd < 0 {
        return Err(Error::Setup(format!(
            "disabled exact-event perf admission: {}; run as root on the admitted host",
            std::io::Error::last_os_error()
        )));
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn collect(cpu: u32) -> Result<CoreObservation> {
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
    let mut original = [0; 8];
    if msr
        .read_at(&mut original, 0xc0011020)
        .map_err(|e| Error::Setup(format!("{msr_path}: {e}; restore admitted MSR access")))?
        != 8
    {
        return Err(Error::Setup(format!(
            "{msr_path}: short workaround MSR read"
        )));
    }
    let kvm =
        File::open("/dev/kvm").map_err(|e| Error::Setup(format!("/dev/kvm admission: {e}")))?;
    // KVM UAPI header plus at most 1024 complete 40-byte entries, aligned for ioctl.
    let mut words = vec![0u64; 1 + 1024 * 5];
    words[0] = 1024;
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
        booted_kernel: kernel("/run/booted-system/kernel")?,
        current_kernel: kernel("/run/current-system/kernel")?,
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

pub(super) fn observe(cpu: u32) -> Result<CoreObservation> {
    let affinity = Affinity::pin(cpu)?;
    scoped_probe(
        affinity,
        || {
            let mut observation = collect(cpu)?;
            if validate_fields(&observation, false).is_err() {
                return Ok(observation);
            }
            // Descriptor is closed before either validation or affinity restoration.
            match disabled_perf() {
                Ok(descriptor) => {
                    drop(descriptor);
                    observation.perf_disabled_open = true;
                }
                Err(error) => observation.perf_error = Some(error.to_string()),
            }
            Ok(observation)
        },
        Affinity::restore,
    )
}
