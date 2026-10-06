//! Shared per-core reservation and scoped MSR workaround. This module owns no
//! process: the executor must reap its VMM before restoring and releasing it.

use crate::{Error, Result};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::{
        fs::{FileExt, OpenOptionsExt},
        io::AsRawFd,
    },
    path::{Path, PathBuf},
};

const MSR: u64 = 0xc0011020;
const WORKAROUND: u64 = 1 << 54;

pub(crate) struct HostLease {
    _locks: Vec<File>,
    registers: Vec<(u32, File, u64)>,
    snapshot: PathBuf,
    restored: bool,
}

pub(crate) fn cpu_list(value: &str) -> Result<BTreeSet<u32>> {
    let mut result = BTreeSet::new();
    let invalid = || Error::Execution("invalid host CPU list".into());
    for item in value.trim().split(',') {
        let (first, last) = item.split_once('-').unwrap_or((item, item));
        let first = first.parse::<u32>().map_err(|_| invalid())?;
        let last = last.parse::<u32>().map_err(|_| invalid())?;
        if last < first || last > 1_048_576 {
            return Err(invalid());
        }
        result.extend(first..=last);
    }
    if result.is_empty() {
        return Err(invalid());
    }
    Ok(result)
}

fn locks(cpus: &BTreeSet<u32>, directory: &Path) -> Result<Vec<File>> {
    try_locks(cpus, directory)?.ok_or_else(|| {
        Error::Execution(format!(
            "CPU lease unavailable for {cpus:?}: another run holds it"
        ))
    })
}

/// Lease every CPU in `cpus`, or none: `None` when another holder has any.
fn try_locks(cpus: &BTreeSet<u32>, directory: &Path) -> Result<Option<Vec<File>>> {
    let mut files = Vec::new();
    for cpu in cpus {
        let path = directory.join(format!("seedbox-msr-cpu-{cpu}.lock"));
        let open = || {
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&path)
        };
        let file = match open() {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .mode(0o644)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(&path)
                {
                    Ok(file) => file,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => open()?,
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) => return Err(error.into()),
        };
        if !file.metadata()?.is_file() {
            return Err(Error::Execution("CPU lease is not a regular file".into()));
        }
        // Same sorted, nonblocking flock protocol as harness/batch.py.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Ok(None);
            }
            return Err(Error::Execution(format!("CPU {cpu} lease: {error}")));
        }
        files.push(file);
    }
    Ok(Some(files))
}

#[cfg(target_os = "linux")]
fn sysfs(path: &str) -> Result<String> {
    fs::read_to_string(path).map_err(|error| Error::Setup(format!("{path}: {error}")))
}

#[cfg(target_os = "linux")]
fn siblings(cpu: u32) -> Result<BTreeSet<u32>> {
    let siblings = cpu_list(&sysfs(&format!(
        "/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list"
    ))?)?;
    if !siblings.contains(&cpu) {
        return Err(Error::Execution(
            "CPU missing from its SMT sibling set".into(),
        ));
    }
    Ok(siblings)
}

/// CPUs this process may run on (respects `taskset` and cgroup cpusets).
#[cfg(target_os = "linux")]
fn allowed_cpus() -> Result<BTreeSet<u32>> {
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    if unsafe { libc::sched_getaffinity(0, std::mem::size_of_val(&set), &mut set) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok((0..libc::CPU_SETSIZE as u32)
        .filter(|cpu| unsafe { libc::CPU_ISSET(*cpu as usize, &set) })
        .collect())
}

/// Candidate CPUs in lease order: allowed and online, one per SMT core, never
/// the core of CPU 0, which keeps host housekeeping off the measured core.
fn candidates(
    allowed: &BTreeSet<u32>,
    mut siblings: impl FnMut(u32) -> Result<BTreeSet<u32>>,
) -> Result<Vec<(u32, BTreeSet<u32>)>> {
    let mut seen = siblings(0)?;
    let mut result = Vec::new();
    for &cpu in allowed {
        if seen.contains(&cpu) {
            continue;
        }
        let core = siblings(cpu)?;
        seen.extend(&core);
        result.push((cpu, core));
    }
    Ok(result)
}

impl HostLease {
    #[cfg(target_os = "linux")]
    pub(crate) fn acquire(cpu: u32, output: &Path) -> Result<Self> {
        Self::acquire_at(
            &siblings(cpu)?,
            Path::new("/tmp"),
            Path::new("/dev/cpu"),
            output,
        )
    }

    /// Lease the first free candidate CPU and return it with the lease.
    #[cfg(target_os = "linux")]
    pub(crate) fn acquire_any(output: &Path) -> Result<(u32, Self)> {
        let online = cpu_list(&sysfs("/sys/devices/system/cpu/online")?)?;
        let allowed = &online & &allowed_cpus()?;
        let candidates = candidates(&allowed, siblings)?;
        if candidates.is_empty() {
            return Err(Error::Setup(format!(
                "no eligible CPU: the allowed CPUs {allowed:?} are all on CPU 0's core; \
                 pass --cpu to use one anyway"
            )));
        }
        for (cpu, core) in candidates {
            if let Some(lease) =
                Self::try_acquire_at(&core, Path::new("/tmp"), Path::new("/dev/cpu"), output)?
            {
                return Ok((cpu, lease));
            }
        }
        Err(Error::Execution(
            "no free CPU: every eligible core is leased by another run".into(),
        ))
    }

    fn acquire_at(
        cpus: &BTreeSet<u32>,
        lock_directory: &Path,
        devices: &Path,
        output: &Path,
    ) -> Result<Self> {
        let locks = locks(cpus, lock_directory)?;
        Self::with_locks(locks, cpus, devices, output)
    }

    fn try_acquire_at(
        cpus: &BTreeSet<u32>,
        lock_directory: &Path,
        devices: &Path,
        output: &Path,
    ) -> Result<Option<Self>> {
        match try_locks(cpus, lock_directory)? {
            Some(locks) => Self::with_locks(locks, cpus, devices, output).map(Some),
            None => Ok(None),
        }
    }

    fn with_locks(
        locks: Vec<File>,
        cpus: &BTreeSet<u32>,
        devices: &Path,
        output: &Path,
    ) -> Result<Self> {
        let mut registers = Vec::new();
        for cpu in cpus {
            let path = devices.join(cpu.to_string()).join("msr");
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .map_err(|error| msr_error(&path, error))?;
            let original = read(&file)?;
            registers.push((*cpu, file, original));
        }
        let snapshot = output.join("msr-original.json");
        let saved: std::collections::BTreeMap<_, _> = registers
            .iter()
            .map(|(cpu, _, value)| (*cpu, *value))
            .collect();
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&snapshot)?;
        file.write_all(&serde_json::to_vec(&saved)?)?;
        file.sync_all()?;
        let lease = Self {
            _locks: locks,
            registers,
            snapshot,
            restored: false,
        };
        // Once the snapshot is durable, Drop restores every saved register even
        // if setting or reading back any individual CPU fails.
        for (_, file, original) in &lease.registers {
            write_verified(file, *original | WORKAROUND)?;
        }
        Ok(lease)
    }

    pub(crate) fn restore(&mut self) -> Result<()> {
        if self.restored {
            return Ok(());
        }
        let mut failures = Vec::new();
        for (cpu, file, original) in &self.registers {
            if let Err(error) = write_verified(file, *original) {
                failures.push(format!("CPU {cpu}: {error}"));
            }
        }
        if !failures.is_empty() {
            return Err(Error::Execution(format!(
                "MSR restoration failed; snapshot retained at {}: {}",
                self.snapshot.display(),
                failures.join("; ")
            )));
        }
        fs::remove_file(&self.snapshot)?;
        self.restored = true;
        Ok(())
    }
}

impl Drop for HostLease {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            eprintln!("seedbox: {error}");
        }
    }
}

fn msr_error(path: &Path, error: std::io::Error) -> Error {
    let hint = match error.kind() {
        std::io::ErrorKind::PermissionDenied => " (run as root, e.g. with sudo)",
        std::io::ErrorKind::NotFound => " (load the msr kernel module: modprobe msr)",
        _ => "",
    };
    Error::Setup(format!("{}: {error}{hint}", path.display()))
}

fn read(file: &File) -> Result<u64> {
    let mut bytes = [0; 8];
    if file.read_at(&mut bytes, MSR)? != 8 {
        return Err(Error::Execution("short MSR read".into()));
    }
    Ok(u64::from_le_bytes(bytes))
}
fn write_verified(file: &File, value: u64) -> Result<()> {
    if file.write_at(&value.to_le_bytes(), MSR)? != 8 || read(file)? != value {
        return Err(Error::Execution("MSR write/readback mismatch".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_lease_and_original_register_values_survive_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let cpus = BTreeSet::from([2, 18]);
        for cpu in &cpus {
            fs::create_dir(dir.path().join(cpu.to_string())).unwrap();
            let file = File::create(dir.path().join(cpu.to_string()).join("msr")).unwrap();
            file.write_at(&(*cpu as u64 + 9).to_le_bytes(), MSR)
                .unwrap();
        }
        {
            let _lease = HostLease::acquire_at(&cpus, dir.path(), dir.path(), dir.path()).unwrap();
            assert!(locks(&BTreeSet::from([18]), dir.path()).is_err());
            assert!(dir.path().join("msr-original.json").exists());
            for cpu in &cpus {
                assert_eq!(
                    read(&File::open(dir.path().join(cpu.to_string()).join("msr")).unwrap())
                        .unwrap(),
                    (*cpu as u64 + 9) | WORKAROUND
                );
            }
        }
        assert!(!dir.path().join("msr-original.json").exists());
        for cpu in &cpus {
            assert_eq!(
                read(&File::open(dir.path().join(cpu.to_string()).join("msr")).unwrap()).unwrap(),
                *cpu as u64 + 9
            );
        }
        assert!(locks(&cpus, dir.path()).is_ok());
    }
    #[test]
    fn missing_sibling_register_does_not_mutate_first_cpu() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("2")).unwrap();
        let path = dir.path().join("2/msr");
        File::create(&path)
            .unwrap()
            .write_at(&17u64.to_le_bytes(), MSR)
            .unwrap();
        assert!(HostLease::acquire_at(
            &BTreeSet::from([2, 18]),
            dir.path(),
            dir.path(),
            dir.path()
        )
        .is_err());
        assert_eq!(read(&File::open(path).unwrap()).unwrap(), 17);
        assert!(!dir.path().join("msr-original.json").exists());
    }
    #[test]
    fn busy_core_is_skipped_rather_than_failing_selection() {
        let dir = tempfile::tempdir().unwrap();
        let _held = locks(&BTreeSet::from([2, 18]), dir.path()).unwrap();
        assert!(try_locks(&BTreeSet::from([2, 18]), dir.path())
            .unwrap()
            .is_none());
        assert!(try_locks(&BTreeSet::from([3, 19]), dir.path())
            .unwrap()
            .is_some());
    }
    #[test]
    fn candidates_take_one_cpu_per_core_and_skip_cpu_zero_core() {
        // Two SMT threads per core, siblings numbered n and n + 4.
        let core = |cpu: u32| Ok(BTreeSet::from([cpu % 4, cpu % 4 + 4]));
        let allowed = BTreeSet::from([0, 1, 2, 4, 5, 6, 7]);
        let picked: Vec<_> = candidates(&allowed, core)
            .unwrap()
            .into_iter()
            .map(|(cpu, _)| cpu)
            .collect();
        assert_eq!(picked, [1, 2, 7]);
    }
    #[test]
    fn parses_cpu_topology_ranges_and_rejects_bad_bounds() {
        assert_eq!(
            cpu_list("2,4-6,18\n").unwrap(),
            BTreeSet::from([2, 4, 5, 6, 18])
        );
        for bad in ["", "2-1", "2,,4", "-1", "1-999999999"] {
            assert!(cpu_list(bad).is_err());
        }
    }
}
