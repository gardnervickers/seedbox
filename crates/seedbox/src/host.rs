//! Shared per-core reservation and the counter profile's scoped MSR workaround.
//! This module owns no process: the executor must reap its VMM before restoring
//! and releasing it.

use crate::{counter::MsrWorkaround, Error, Result};
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

pub(crate) struct HostLease {
    _locks: Vec<File>,
    /// Empty when the profile needs no workaround.
    registers: Vec<(u32, File, u64)>,
    register: u64,
    bit: u64,
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

#[cfg(test)]
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

#[cfg(any(target_os = "linux", test))]
pub(crate) struct CpuReservation {
    pub(crate) cpu: u32,
    pub(crate) cpus: BTreeSet<u32>,
    locks: Vec<File>,
}
impl CpuReservation {
    fn validate_topology(
        &self,
        online: &BTreeSet<u32>,
        allowed: &BTreeSet<u32>,
        mut siblings: impl FnMut(u32) -> Result<BTreeSet<u32>>,
    ) -> Result<()> {
        if !self.cpus.contains(&self.cpu)
            || !self.cpus.is_subset(online)
            || !self.cpus.is_subset(allowed)
        {
            return Err(Error::Setup(
                "reserved CPU core is no longer online and allowed; stop before execution".into(),
            ));
        }
        for &cpu in &self.cpus {
            if siblings(cpu)? != self.cpus {
                return Err(Error::Setup("reserved CPU core has changed or nonreciprocal SMT topology; stop before execution".into()));
            }
        }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    fn revalidate(&self) -> Result<()> {
        self.validate_topology(
            &cpu_list(&sysfs("/sys/devices/system/cpu/online")?)?,
            &allowed_cpus()?,
            siblings,
        )
    }
}
#[cfg(target_os = "linux")]
impl CpuReservation {
    pub(crate) fn acquire(selected: Option<u32>) -> Result<Self> {
        let online = cpu_list(&sysfs("/sys/devices/system/cpu/online")?)?;
        let allowed = &online & &allowed_cpus()?;
        let choices = match selected {
            Some(cpu) => vec![(cpu, siblings(cpu)?)],
            None => candidates(&allowed, siblings)?,
        };
        for (cpu, core) in choices {
            if !core.is_subset(&allowed) || !core.contains(&cpu) {
                if selected.is_none() {
                    continue;
                }
                return Err(Error::Setup(format!("CPU {cpu}: all SMT siblings must be online and allowed; choose an eligible core")));
            }
            for sibling in &core {
                if siblings(*sibling)? != core {
                    return Err(Error::Setup("inconsistent SMT topology".into()));
                }
            }
            if let Some(locks) = try_locks(&core, Path::new("/tmp"))? {
                let reservation = Self {
                    cpu,
                    cpus: core,
                    locks,
                };
                reservation.revalidate()?; // Fresh eligibility/topology while every lease is held.
                return Ok(reservation);
            }
        }
        Err(Error::Setup(
            "no free eligible CPU core; release an existing lease or select an admitted --cpu"
                .into(),
        ))
    }
}

impl HostLease {
    #[cfg(target_os = "linux")]
    pub(crate) fn acquire_checked(
        cpu: Option<u32>,
        workaround: Option<MsrWorkaround>,
        output: &Path,
        check: impl FnOnce(&CpuReservation) -> Result<()>,
    ) -> Result<(u32, Self)> {
        let reservation = CpuReservation::acquire(cpu)?;
        Self::with_reservation(
            reservation,
            workaround,
            Path::new("/dev/cpu"),
            output,
            check,
            CpuReservation::revalidate,
        )
    }

    fn with_reservation(
        reservation: CpuReservation,
        workaround: Option<MsrWorkaround>,
        devices: &Path,
        output: &Path,
        check: impl FnOnce(&CpuReservation) -> Result<()>,
        recheck: impl FnOnce(&CpuReservation) -> Result<()>,
    ) -> Result<(u32, Self)> {
        check(&reservation)?; // Shared portable/Linux path, before any MSR write.
        recheck(&reservation)?; // After capability collection/affinity restore, before MSR access.
        let selected = reservation.cpu;
        let lease = Self::with_locks(
            reservation.locks,
            &reservation.cpus,
            workaround,
            devices,
            output,
        )?;
        Ok((selected, lease))
    }

    #[cfg(test)]
    fn acquire_at(
        cpus: &BTreeSet<u32>,
        workaround: Option<MsrWorkaround>,
        lock_directory: &Path,
        devices: &Path,
        output: &Path,
    ) -> Result<Self> {
        let locks = locks(cpus, lock_directory)?;
        Self::with_locks(locks, cpus, workaround, devices, output)
    }

    fn with_locks(
        locks: Vec<File>,
        cpus: &BTreeSet<u32>,
        workaround: Option<MsrWorkaround>,
        devices: &Path,
        output: &Path,
    ) -> Result<Self> {
        let snapshot = output.join("msr-original.json");
        let Some(MsrWorkaround { register, bit }) = workaround else {
            return Ok(Self {
                _locks: locks,
                registers: Vec::new(),
                register: 0,
                bit: 0,
                snapshot,
                restored: true,
            });
        };
        let mut registers = Vec::new();
        for cpu in cpus {
            let path = devices.join(cpu.to_string()).join("msr");
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .map_err(|error| msr_error(&path, error))?;
            let original = read(&file, register)?;
            registers.push((*cpu, file, original));
        }
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
            register,
            bit,
            snapshot,
            restored: false,
        };
        // Once the snapshot is durable, Drop restores every saved register even
        // if setting or reading back any individual CPU fails.
        for (_, file, original) in &lease.registers {
            write_verified(file, register, *original | bit)?;
        }
        Ok(lease)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn original_registers(&self) -> std::collections::BTreeMap<u32, u64> {
        self.registers
            .iter()
            .map(|(cpu, _, original)| (*cpu, *original))
            .collect()
    }
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn enabled_registers(&self) -> Result<std::collections::BTreeMap<u32, u64>> {
        self.registers
            .iter()
            .map(|(cpu, file, original)| {
                let value = read(file, self.register)?;
                if value != (*original | self.bit) {
                    return Err(Error::Execution("MSR workaround readback drifted".into()));
                }
                Ok((*cpu, value))
            })
            .collect()
    }

    pub(crate) fn restore(&mut self) -> Result<()> {
        if self.restored {
            return Ok(());
        }
        let mut failures = Vec::new();
        for (cpu, file, original) in &self.registers {
            if let Err(error) = write_verified(file, self.register, *original) {
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

fn read(file: &File, register: u64) -> Result<u64> {
    let mut bytes = [0; 8];
    if file.read_at(&mut bytes, register)? != 8 {
        return Err(Error::Execution("short MSR read".into()));
    }
    Ok(u64::from_le_bytes(bytes))
}
fn write_verified(file: &File, register: u64, value: u64) -> Result<()> {
    if file.write_at(&value.to_le_bytes(), register)? != 8 || read(file, register)? != value {
        return Err(Error::Execution("MSR write/readback mismatch".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::counter::CounterProfile;

    const ZEN: Option<MsrWorkaround> = CounterProfile::Zen.workaround();
    const MSR: u64 = 0xc0011020;
    const WORKAROUND: u64 = 1 << 54;

    /// Whether the leases on `cpus` can be taken again. A process another
    /// test spawns in parallel holds copies of these lock descriptors until
    /// it execs, so wait that out; a lease this process leaked stays held.
    fn released(cpus: &BTreeSet<u32>, directory: &Path) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if locks(cpus, directory).is_ok() {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    #[test]
    fn leased_recheck_failure_releases_locks_without_msr_write_or_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let cpus = BTreeSet::from([2, 18]);
        for cpu in &cpus {
            fs::create_dir(dir.path().join(cpu.to_string())).unwrap();
            File::create(dir.path().join(cpu.to_string()).join("msr"))
                .unwrap()
                .write_at(&17u64.to_le_bytes(), MSR)
                .unwrap();
        }
        let reservation = CpuReservation {
            cpu: 2,
            cpus: cpus.clone(),
            locks: locks(&cpus, dir.path()).unwrap(),
        };
        let result = HostLease::with_reservation(
            reservation,
            ZEN,
            dir.path(),
            dir.path(),
            |_| {
                assert!(locks(&cpus, dir.path()).is_err());
                Err(Error::Setup("microcode changed under reservation".into()))
            },
            |_| Ok(()),
        );
        assert!(result.is_err());
        assert!(!dir.path().join("msr-original.json").exists());
        for cpu in &cpus {
            assert_eq!(
                read(
                    &File::open(dir.path().join(cpu.to_string()).join("msr")).unwrap(),
                    MSR
                )
                .unwrap(),
                17
            );
        }
        assert!(released(&cpus, dir.path()));
    }
    #[test]
    fn topology_and_eligibility_drift_under_locks_refuse_before_msr_mutation() {
        for drift in 0..4 {
            let dir = tempfile::tempdir().unwrap();
            let cpus = BTreeSet::from([2, 18]);
            for cpu in &cpus {
                fs::create_dir(dir.path().join(cpu.to_string())).unwrap();
                File::create(dir.path().join(cpu.to_string()).join("msr"))
                    .unwrap()
                    .write_at(&17u64.to_le_bytes(), MSR)
                    .unwrap();
            }
            let reservation = CpuReservation {
                cpu: 2,
                cpus: cpus.clone(),
                locks: locks(&cpus, dir.path()).unwrap(),
            };
            reservation
                .validate_topology(&cpus, &cpus, |_| Ok(cpus.clone()))
                .unwrap();
            // Inject a change after discovery, with all original locks still held.
            let result = HostLease::with_reservation(
                reservation,
                ZEN,
                dir.path(),
                dir.path(),
                |_| Ok(()),
                |reserved| {
                    assert!(locks(&cpus, dir.path()).is_err());
                    let online = if drift == 0 {
                        BTreeSet::from([2])
                    } else {
                        cpus.clone()
                    };
                    let allowed = if drift == 1 {
                        BTreeSet::from([18])
                    } else {
                        cpus.clone()
                    };
                    reserved.validate_topology(&online, &allowed, |cpu| {
                        Ok(if drift == 2 {
                            BTreeSet::from([2, 18, 34])
                        } else if drift == 3 && cpu == 18 {
                            BTreeSet::from([18])
                        } else {
                            cpus.clone()
                        })
                    })
                },
            );
            assert!(result.is_err());
            assert!(!dir.path().join("msr-original.json").exists());
            for cpu in &cpus {
                assert_eq!(
                    read(
                        &File::open(dir.path().join(cpu.to_string()).join("msr")).unwrap(),
                        MSR
                    )
                    .unwrap(),
                    17
                );
            }
            assert!(released(&cpus, dir.path()));
        }
    }
    #[test]
    fn shared_lease_and_original_register_values_survive_cleanup() {
        let _process_creation = crate::preparation_command::PROCESS_TEST_LOCK
            .lock()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cpus = BTreeSet::from([2, 18]);
        for cpu in &cpus {
            fs::create_dir(dir.path().join(cpu.to_string())).unwrap();
            let file = File::create(dir.path().join(cpu.to_string()).join("msr")).unwrap();
            file.write_at(&(*cpu as u64 + 9).to_le_bytes(), MSR)
                .unwrap();
        }
        {
            let _lease =
                HostLease::acquire_at(&cpus, ZEN, dir.path(), dir.path(), dir.path()).unwrap();
            assert!(locks(&BTreeSet::from([18]), dir.path()).is_err());
            assert!(dir.path().join("msr-original.json").exists());
            assert_eq!(
                _lease.enabled_registers().unwrap()[&2],
                (2 + 9) | WORKAROUND
            );
            for cpu in &cpus {
                assert_eq!(
                    read(
                        &File::open(dir.path().join(cpu.to_string()).join("msr")).unwrap(),
                        MSR
                    )
                    .unwrap(),
                    (*cpu as u64 + 9) | WORKAROUND
                );
            }
        }
        assert!(!dir.path().join("msr-original.json").exists());
        for cpu in &cpus {
            assert_eq!(
                read(
                    &File::open(dir.path().join(cpu.to_string()).join("msr")).unwrap(),
                    MSR
                )
                .unwrap(),
                *cpu as u64 + 9
            );
        }
        assert!(released(&cpus, dir.path()));
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
            ZEN,
            dir.path(),
            dir.path(),
            dir.path()
        )
        .is_err());
        assert_eq!(read(&File::open(path).unwrap(), MSR).unwrap(), 17);
        assert!(!dir.path().join("msr-original.json").exists());
    }
    #[test]
    fn checked_unqualified_reservation_keeps_all_locks_without_any_msr_path() {
        let root = tempfile::tempdir().unwrap();
        let cpus = BTreeSet::from([2, 18]);
        let reserved = CpuReservation {
            cpu: 2,
            cpus: cpus.clone(),
            locks: locks(&cpus, root.path()).unwrap(),
        };
        let (cpu, mut lease) = HostLease::with_reservation(
            reserved,
            None,
            &root.path().join("no-msr-devices"),
            root.path(),
            |r| {
                assert_eq!(r.cpus, cpus);
                assert!(locks(&cpus, root.path()).is_err());
                Ok(())
            },
            |r| r.validate_topology(&cpus, &cpus, |_| Ok(cpus.clone())),
        )
        .unwrap();
        assert_eq!(cpu, 2);
        assert!(lease.registers.is_empty());
        assert!(!root.path().join("msr-original.json").exists());
        assert!(locks(&cpus, root.path()).is_err());
        lease.restore().unwrap();
        assert!(locks(&cpus, root.path()).is_err());
        drop(lease);
        assert!(released(&cpus, root.path()));
    }

    #[test]
    fn profile_without_workaround_leases_cores_and_never_touches_msrs() {
        let dir = tempfile::tempdir().unwrap();
        let cpus = BTreeSet::from([2, 18]);
        {
            let mut lease =
                HostLease::acquire_at(&cpus, None, dir.path(), dir.path(), dir.path()).unwrap();
            assert!(locks(&BTreeSet::from([18]), dir.path()).is_err());
            assert!(!dir.path().join("msr-original.json").exists());
            lease.restore().unwrap();
        }
        assert!(released(&cpus, dir.path()));
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
