//! Linux system queries only: no VM creation or signals.
use super::*;
use std::collections::BTreeMap;
use std::{
    fs::{self, File},
    io::Read,
    os::fd::AsRawFd,
};

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
    let result = unsafe { libc::ioctl(file.as_raw_fd(), command, argument) };
    if result < 0 {
        return Err(Error::Setup(format!(
            "KVM admission query: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(result)
}

fn collect() -> Result<CoreObservation> {
    // CPU identity is diagnostic metadata only; scheduling remains inherited.
    let cpu = unsafe { libc::sched_getcpu() };
    if cpu < 0 {
        return Err(Error::Setup(format!(
            "observe current CPU: {}",
            std::io::Error::last_os_error()
        )));
    }
    let cpu = cpu as u32;
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
    let kvm = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .context(Error::Setup, "/dev/kvm admission")?;
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
        kvm_api: query(&kvm, 0xae00, 0)?,
        immediate_exit: query(&kvm, 0xae03, 136)?,
        guest_debug: query(&kvm, 0xae03, 23)?,
        sync_regs: query(&kvm, 0xae03, 74)?,
        raw_cpuid,
    })
}

pub(super) fn observe() -> Result<CoreObservation> {
    collect()
}
