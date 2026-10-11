//! Durable interactive inputs, separate from application logs and engine events.
//! The caller owns a private run directory. One writer holds a stable sibling
//! lock inode while complete histories are atomically replaced.

use crate::{limits::read_bounded, Context, Error, Result};
use seedbox_engine_protocol::intervention::MAX_BYTES;
pub use seedbox_engine_protocol::intervention::{History, Intervention};
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
};

pub struct Journal {
    path: PathBuf,
    // Never unlink this file: another writer must lock the same inode.
    _lock: File,
    history: History,
    storage_failed: bool,
}

impl Journal {
    /// Open or begin a history in an existing private directory. Opening an
    /// empty journal creates only its lock; the first admission creates data.
    pub fn open(path: &Path) -> Result<Self> {
        let mut lock_name = path.as_os_str().to_owned();
        lock_name.push(".lock");
        let lock_path = PathBuf::from(lock_name);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&lock_path)
            .context(Error::Setup, lock_path.display())?;
        regular(&lock).context(Error::Setup, lock_path.display())?;
        // SAFETY: lock owns this live descriptor throughout the journal lifetime.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(Error::Setup(format!(
                "{}: cannot lock intervention history: {}",
                path.display(),
                io::Error::last_os_error()
            )));
        }
        let history = read_optional(path)?.unwrap_or_default();
        Ok(Self {
            path: path.to_owned(),
            _lock: lock,
            history,
            storage_failed: false,
        })
    }

    /// Make even an empty history durable before publishing an attempt. Recovery
    /// can then treat a missing history as lost input rather than an empty run.
    pub fn initialize(path: &Path) -> Result<Self> {
        let journal = Self::open(path)?;
        journal.persist(&journal.history.encode().map_err(Error::Integrity)?)?;
        Ok(journal)
    }

    pub fn history(&self) -> &History {
        &self.history
    }

    /// Persist before acknowledging admission or advancing the guest. Returns
    /// false for an identical retry. A conflicting retry changes nothing.
    ///
    /// Any storage failure poisons this writer, including uncertain durability
    /// after rename. The owner must stop execution rather than retry admission.
    pub fn append(&mut self, entry: Intervention) -> Result<bool> {
        if self.storage_failed {
            return Err(Error::Integrity(
                "intervention writer has failed; stop execution".into(),
            ));
        }
        if let Some(existing) = self
            .history
            .entries
            .iter()
            .find(|e| e.request_id == entry.request_id)
        {
            return if existing == &entry {
                Ok(false)
            } else {
                Err(Error::Integrity(format!(
                    "intervention request {} conflicts with its retained admission",
                    entry.request_id
                )))
            };
        }
        let mut next = self.history.clone();
        next.entries.push(entry);
        let bytes = next.encode().map_err(Error::Integrity)?;
        if let Err(error) = self.persist(&bytes) {
            self.storage_failed = true;
            return Err(error);
        }
        self.history = next;
        Ok(true)
    }

    fn persist(&self, bytes: &[u8]) -> Result<()> {
        let directory = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        (|| -> io::Result<()> {
            let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
            temporary.write_all(bytes)?;
            temporary.as_file().sync_all()?;
            temporary.persist(&self.path).map_err(|e| e.error)?;
            File::open(directory)?.sync_all()
        })()
        .context(
            Error::Setup,
            format!("{}: persist intervention history", self.path.display()),
        )
    }
}

/// Read a complete committed history without acquiring the writer lock. Atomic
/// replacement means readers see either complete version, never a partial write.
pub fn read(path: &Path) -> Result<History> {
    read_optional(path)?.ok_or_else(|| {
        Error::Setup(format!(
            "{}: intervention history not found",
            path.display()
        ))
    })
}

/// Absence is permitted for a live attempt before its first intervention.
pub fn read_optional(path: &Path) -> Result<Option<History>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Error::Setup(format!("{}: {error}", path.display()))),
    };
    regular(&file).context(Error::Setup, path.display())?;
    let bytes = read_bounded(file, MAX_BYTES as u64, "intervention history")
        .context(Error::Integrity, path.display())?;
    History::decode(&bytes)
        .map(Some)
        .context(Error::Integrity, path.display())
}

fn regular(file: &File) -> io::Result<()> {
    if file.metadata()?.is_file() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a regular file",
        ))
    }
}
