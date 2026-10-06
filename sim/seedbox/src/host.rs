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
        // Shared sorted, nonblocking flock protocol used by every Rust runner.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Err(Error::LeaseBusy(*cpu));
            }
            return Err(error.into());
        }
        files.push(file);
    }
    Ok(files)
}

impl HostLease {
    #[cfg(target_os = "linux")]
    pub(crate) fn acquire(cpu: u32, output: &Path) -> Result<Self> {
        let siblings = cpu_list(&fs::read_to_string(format!(
            "/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list"
        ))?)?;
        if !siblings.contains(&cpu) {
            return Err(Error::Execution(
                "CPU missing from its SMT sibling set".into(),
            ));
        }
        Self::acquire_at(&siblings, Path::new("/tmp"), Path::new("/dev/cpu"), output)
    }

    fn acquire_at(
        cpus: &BTreeSet<u32>,
        lock_directory: &Path,
        devices: &Path,
        output: &Path,
    ) -> Result<Self> {
        let locks = locks(cpus, lock_directory)?;
        let mut registers = Vec::new();
        for cpu in cpus {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(devices.join(cpu.to_string()).join("msr"))?;
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
