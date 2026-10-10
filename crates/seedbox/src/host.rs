//! Shared per-core reservation: the lease on a physical core and its SMT
//! siblings. This module owns no process: the executor must reap its VMM
//! before releasing it.

use crate::{Error, Result};
#[cfg(target_os = "linux")]
use std::fs;
use std::{
    collections::BTreeSet,
    fs::{File, OpenOptions},
    os::unix::{fs::OpenOptionsExt, io::AsRawFd},
    path::Path,
};

/// KVM is a device permission, not a reason to run the entire CLI as root.
/// Called before image preparation and again by execution admission.
#[cfg(target_os = "linux")]
pub(crate) fn check_kvm_access() -> Result<()> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .map(|_| ())
        .map_err(|error| kvm_access_error(error, Path::new("/etc/NIXOS").exists()))
}
fn kvm_access_error(error: std::io::Error, nixos: bool) -> Error {
    let remedy = match error.kind() {
        std::io::ErrorKind::PermissionDenied if nixos =>
            "add your user to the kvm group in your NixOS configuration (users.users.<your-user>.extraGroups = [ \"kvm\" ];), rebuild with nixos-rebuild switch, then log out and back in",
        std::io::ErrorKind::PermissionDenied =>
            "grant your user read/write access through your distribution's kvm group (usually sudo usermod -aG kvm \"$USER\"), then log out and back in; verify ls -l /dev/kvm and id",
        std::io::ErrorKind::NotFound =>
            "enable hardware virtualization and load kvm_amd or kvm_intel; in a VM, enable nested virtualization on its host",
        _ => "check /dev/kvm and your host's KVM configuration",
    };
    Error::Setup(format!("cannot open /dev/kvm: {error}; {remedy}"))
}

/// Held CPU/SMT leases, released on drop after the executor reaps its child.
pub(crate) struct HostLease {
    _locks: Vec<File>,
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
        // The lock-file name is shared with harness/batch.py; it predates the
        // removal of the MSR workaround and is retained unchanged.
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
        // SAFETY: `file` is open for the duration of the call.
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
    use crate::Context;
    fs::read_to_string(path).context(Error::Setup, path)
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
    // SAFETY: cpu_set_t is a plain bit array; all-zero is the empty set.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: the size passed is the size of the writable mask.
    if unsafe { libc::sched_getaffinity(0, std::mem::size_of_val(&set), &mut set) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok((0..libc::CPU_SETSIZE as u32)
        // SAFETY: every index is below CPU_SETSIZE.
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
        check: impl FnOnce(&CpuReservation) -> Result<()>,
    ) -> Result<(u32, Self)> {
        let reservation = CpuReservation::acquire(cpu)?;
        Self::with_reservation(reservation, check, CpuReservation::revalidate)
    }

    fn with_reservation(
        reservation: CpuReservation,
        check: impl FnOnce(&CpuReservation) -> Result<()>,
        recheck: impl FnOnce(&CpuReservation) -> Result<()>,
    ) -> Result<(u32, Self)> {
        check(&reservation)?; // Shared portable/Linux path, before any guest starts.
        recheck(&reservation)?; // After capability collection/affinity restore, before spawn.
        let selected = reservation.cpu;
        Ok((
            selected,
            Self {
                _locks: reservation.locks,
            },
        ))
    }

    #[cfg(test)]
    fn acquire_at(cpus: &BTreeSet<u32>, lock_directory: &Path) -> Result<Self> {
        Ok(Self {
            _locks: locks(cpus, lock_directory)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn kvm_permission_errors_offer_platform_specific_fixes() {
        for (nixos, expected) in [(true, "extraGroups"), (false, "usermod -aG kvm")] {
            let error =
                kvm_access_error(std::io::ErrorKind::PermissionDenied.into(), nixos).to_string();
            assert!(error.contains("/dev/kvm"));
            assert!(error.contains(expected));
            assert!(error.contains("log out and back in"));
        }
        let missing = kvm_access_error(std::io::ErrorKind::NotFound.into(), false).to_string();
        assert!(missing.contains("nested virtualization"));
        assert!(!missing.contains("usermod"));
    }

    #[test]
    fn leased_check_failure_releases_locks() {
        let dir = tempfile::tempdir().unwrap();
        let cpus = BTreeSet::from([2, 18]);
        let reservation = CpuReservation {
            cpu: 2,
            cpus: cpus.clone(),
            locks: locks(&cpus, dir.path()).unwrap(),
        };
        let mut rechecked = false;
        let result = HostLease::with_reservation(
            reservation,
            |_| {
                assert!(locks(&cpus, dir.path()).is_err());
                Err(Error::Setup("capability changed under reservation".into()))
            },
            |_| {
                rechecked = true;
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(!rechecked);
        assert!(released(&cpus, dir.path()));
    }
    #[test]
    fn topology_and_eligibility_drift_under_locks_refuse_the_lease() {
        for drift in 0..4 {
            let dir = tempfile::tempdir().unwrap();
            let cpus = BTreeSet::from([2, 18]);
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
            assert!(released(&cpus, dir.path()));
        }
    }
    #[test]
    fn checked_reservation_keeps_all_locks_until_drop() {
        let root = tempfile::tempdir().unwrap();
        let cpus = BTreeSet::from([2, 18]);
        let reserved = CpuReservation {
            cpu: 2,
            cpus: cpus.clone(),
            locks: locks(&cpus, root.path()).unwrap(),
        };
        let (cpu, lease) = HostLease::with_reservation(
            reserved,
            |r| {
                assert_eq!(r.cpus, cpus);
                assert!(locks(&cpus, root.path()).is_err());
                Ok(())
            },
            |r| r.validate_topology(&cpus, &cpus, |_| Ok(cpus.clone())),
        )
        .unwrap();
        assert_eq!(cpu, 2);
        assert!(locks(&cpus, root.path()).is_err());
        assert!(locks(&BTreeSet::from([18]), root.path()).is_err());
        drop(lease);
        assert!(released(&cpus, root.path()));
    }
    #[test]
    fn shared_lease_survives_until_drop() {
        let _process_creation = crate::preparation_command::PROCESS_TEST_LOCK
            .lock()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cpus = BTreeSet::from([2, 18]);
        {
            let _lease = HostLease::acquire_at(&cpus, dir.path()).unwrap();
            assert!(locks(&BTreeSet::from([18]), dir.path()).is_err());
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
