//! Verified local content store. A digest alone is never proof of retention.

use crate::{limits::read_bounded, Context, Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, HashMap},
    fs,
    io::{self, Read, Seek, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ContentDigest(String);

impl ContentDigest {
    pub fn of(bytes: &[u8]) -> Self {
        Self(hex::encode(Sha256::digest(bytes)))
    }
    /// The digest of everything fed to a streaming hasher.
    pub fn finish(hash: Sha256) -> Self {
        Self(hex::encode(hash.finalize()))
    }
    pub fn hex(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ContentDigest {
    type Error = Error;
    fn try_from(value: String) -> Result<Self> {
        if value.len() != 64
            || !value
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return Err(Error::Integrity(
                "expected a lowercase SHA-256 digest".into(),
            ));
        }
        Ok(Self(value))
    }
}
impl From<ContentDigest> for String {
    fn from(value: ContentDigest) -> Self {
        value.0
    }
}

#[derive(Clone, Debug)]
pub struct ContentStore {
    root: PathBuf,
    evidence_root: Option<PathBuf>,
    /// Objects this process hashed, with the file identity they had then.
    /// Clones share it, so concurrent users verify each object once.
    verified: Arc<Mutex<HashMap<ContentDigest, Stamp>>>,
    /// Cache objects this process created for its resources, if it reclaims
    /// them. Ownership lives only in this process: after a crash or any other
    /// exit, the objects stay as ordinary cache content. Clones share it, and
    /// its lock orders every publication into the cache against reclamation.
    owned: Option<Arc<Mutex<BTreeSet<ContentDigest>>>>,
}

type OwnedGuard<'a> = std::sync::MutexGuard<'a, BTreeSet<ContentDigest>>;

/// File identity and metadata used to invalidate cached verification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl Stamp {
    fn of(file: &fs::File) -> io::Result<Self> {
        let m = file.metadata()?;
        Ok(Self {
            dev: m.dev(),
            ino: m.ino(),
            len: m.len(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
        })
    }
}

impl ContentStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            evidence_root: None,
            verified: Arc::default(),
            owned: None,
        }
    }

    /// Let `*_owned` calls create objects this process may reclaim. Without
    /// it they are ordinary cache objects.
    pub fn with_owned_objects(self) -> Self {
        Self {
            owned: Some(Arc::default()),
            ..self
        }
    }

    /// Owned objects that have been neither reclaimed nor reused as cache inputs.
    pub fn owned_objects(&self) -> BTreeSet<ContentDigest> {
        self.owned_lock()
            .map(|objects| objects.clone())
            .unwrap_or_default()
    }

    /// Remove every owned object not in `keep`. Callers must exclude
    /// concurrent work that has created an owned object it has not yet
    /// recorded a reference to. Returns the removed digests.
    pub fn reclaim_owned(&self, keep: &BTreeSet<ContentDigest>) -> Result<Vec<ContentDigest>> {
        let Some(mut objects) = self.owned_lock() else {
            return Ok(Vec::new());
        };
        let unreferenced: Vec<_> = objects.difference(keep).cloned().collect();
        for digest in &unreferenced {
            remove_if_present(&self.path(digest))?;
            self.forget(digest);
            objects.remove(digest);
        }
        Ok(unreferenced)
    }

    fn owned_lock(&self) -> Option<OwnedGuard<'_>> {
        self.owned
            .as_ref()
            .map(|owned| owned.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// An ordinary put reuses this object as a cache input: it is no longer
    /// owned.
    fn adopt(objects: Option<&mut OwnedGuard<'_>>, digest: &ContentDigest) {
        if let Some(objects) = objects {
            objects.remove(digest);
        }
    }

    /// Keep immutable inputs in a reusable cache while run identities and logs
    /// written through `put_unsynced` belong to one disposable server session.
    pub fn with_ephemeral_evidence(
        cache: impl Into<PathBuf>,
        evidence: impl Into<PathBuf>,
    ) -> Self {
        Self {
            evidence_root: Some(evidence.into()),
            ..Self::new(cache)
        }
    }

    fn path(&self, digest: &ContentDigest) -> PathBuf {
        self.root.join("sha256").join(digest.hex())
    }

    fn object_path(&self, digest: &ContentDigest) -> PathBuf {
        let cached = self.path(digest);
        if cached.exists() {
            return cached;
        }
        if let Some(root) = &self.evidence_root {
            let path = root.join("sha256").join(digest.hex());
            if path.exists() {
                return path;
            }
        }
        cached
    }

    pub fn put(&self, bytes: &[u8]) -> Result<ContentDigest> {
        self.put_reader(bytes)
    }

    /// Retain arbitrarily large artifacts with a bounded 64 KiB transfer buffer.
    pub fn put_reader(&self, reader: impl Read) -> Result<ContentDigest> {
        self.put_reader_with(reader, true)
    }

    /// `put_reader` for an object that belongs to a resource rather than to the
    /// reusable cache. A new object becomes owned and `reclaim_owned` may remove
    /// it, so callers exclude reclamation until a reference names it; an object
    /// already present is unchanged.
    pub fn put_reader_owned(&self, mut reader: impl Read) -> Result<ContentDigest> {
        self.put_reader_observed(&mut reader, true, true, &mut |_| Ok(()))
    }

    /// Retain a run's own evidence (identity, logs) without syncing it. A host
    /// crash can lose such an object, and a record that names it then fails
    /// verification as an integrity error; it is never silently wrong. Putting
    /// the same bytes unsynced again replaces such an object. Inputs
    /// that later runs replay from keep using the synced `put*` calls. With
    /// `with_ephemeral_evidence`, these objects live only in the session store.
    /// Stream session-owned evidence without promoting it into the binary cache.
    pub fn put_reader_unsynced(&self, reader: impl Read) -> Result<ContentDigest> {
        self.put_reader_with(reader, false)
    }

    pub fn put_unsynced(&self, bytes: &[u8]) -> Result<ContentDigest> {
        self.put_reader_with(bytes, false)
    }

    /// `put_unsynced` for a file.
    pub fn put_file_unsynced(&self, path: &Path) -> Result<ContentDigest> {
        self.put_reader_with(fs::File::open(path)?, false)
    }

    fn put_reader_with(&self, mut reader: impl Read, durable: bool) -> Result<ContentDigest> {
        self.put_reader_observed(&mut reader, durable, false, &mut |_| Ok(()))
    }

    fn put_reader_observed(
        &self,
        reader: &mut impl Read,
        durable: bool,
        owned: bool,
        progress: &mut impl FnMut(u64) -> io::Result<()>,
    ) -> Result<ContentDigest> {
        progress(0)?;
        let root = if durable {
            &self.root
        } else {
            self.evidence_root.as_ref().unwrap_or(&self.root)
        };
        let directory = root.join("sha256");
        fs::create_dir_all(&directory)?;
        let mut temp = tempfile::NamedTempFile::new_in(&directory)?;
        let digest = transfer_observed(reader, &mut temp, progress)?;
        let path = directory.join(digest.hex());
        if durable {
            temp.as_file().sync_all()?;
        }
        progress(0)?;
        // Naming a cache object and reclaiming one are ordered by this lock.
        let mut objects = if *root == self.root {
            self.owned_lock()
        } else {
            None
        };
        // Only a new object becomes owned; an existing one keeps its role.
        let claim = owned && objects.is_some() && !path.exists();
        if !owned {
            Self::adopt(objects.as_mut(), &digest);
        }
        let published = match temp.persist_noclobber(&path) {
            Ok(_) => true,
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
                // A host crash can leave an unsynced object named but short.
                // These bytes hash to the digest, so put them in its place.
                if !durable && self.verify(&digest).is_err() {
                    e.file.persist(&path).map_err(|e| e.error)?;
                }
                false
            }
            Err(e) => return Err(e.error.into()),
        };
        if let (true, true, Some(objects)) = (claim, published, objects.as_mut()) {
            objects.insert(digest.clone());
        }
        drop(objects);
        // Verify an existing entry too; never repair corruption implicitly.
        self.verify_observed(&digest, progress)?;
        Ok(digest)
    }

    /// Like `put_reader`, but an object already in the store is only read:
    /// the file is hashed first, so retaining the same engine, kernel or image
    /// again never rewrites and syncs a second copy of it.
    pub fn put_file(&self, path: &Path) -> Result<ContentDigest> {
        self.put_file_observed(path, &mut |_| Ok(()))
    }

    /// Retain a file with cooperative observation of every hash/copy/verification
    /// pass. The callback receives bytes processed (not logical file offsets),
    /// at most 64 KiB per call, and zero at phase boundaries/cache hits.
    /// Returning an error stops work and removes any unpublished temporary file.
    /// Completed immutable objects may remain; synchronous filesystem calls are
    /// not interruptible, so cancellation is checked before/after those calls.
    pub fn put_file_observed(
        &self,
        path: &Path,
        progress: &mut impl FnMut(u64) -> io::Result<()>,
    ) -> Result<ContentDigest> {
        self.put_file_with(path, false, progress)
    }

    /// `put_file_observed` for an object that belongs to a resource
    /// (`put_reader_owned`).
    pub fn put_file_owned_observed(
        &self,
        path: &Path,
        progress: &mut impl FnMut(u64) -> io::Result<()>,
    ) -> Result<ContentDigest> {
        self.put_file_with(path, true, progress)
    }

    fn put_file_with(
        &self,
        path: &Path,
        owned: bool,
        progress: &mut impl FnMut(u64) -> io::Result<()>,
    ) -> Result<ContentDigest> {
        progress(0)?;
        let mut file = fs::File::open(path)?;
        // External files can be rewritten or have their inode reused without
        // changing a coarse filesystem timestamp. Read their bytes each time.
        let digest = transfer_observed(&mut file, &mut io::sink(), progress)?;
        progress(0)?;
        let existing = {
            let mut objects = self.owned_lock();
            if !owned {
                Self::adopt(objects.as_mut(), &digest);
            }
            fs::File::open(self.path(&digest))
        };
        if let Ok(existing) = existing {
            // Verify the existing entry; never repair corruption implicitly.
            if self.trusted(&digest, &existing)?.is_none() {
                self.verify_observed(&digest, progress)?;
            }
            return Ok(digest);
        }
        file.seek(io::SeekFrom::Start(0))?;
        self.put_reader_observed(&mut file, true, owned, progress)
    }

    /// The object a named record points at, if the record exists and is
    /// well formed. Records name slow derivations, such as an exported image
    /// filesystem; the object they point at is verified when it is read.
    pub fn record(&self, name: &ContentDigest) -> Option<ContentDigest> {
        let text = fs::read_to_string(self.root.join("records").join(name.hex())).ok()?;
        ContentDigest::try_from(text).ok()
    }

    /// Point a named record at an object. Records are not synced: after a
    /// host crash a record is at worst missing or unreadable, which is a miss.
    pub fn set_record(&self, name: &ContentDigest, target: &ContentDigest) -> Result<()> {
        let directory = self.root.join("records");
        fs::create_dir_all(&directory)?;
        let mut temp = tempfile::NamedTempFile::new_in(&directory)?;
        temp.write_all(target.hex().as_bytes())?;
        temp.persist(directory.join(name.hex()))
            .map_err(|e| e.error)?;
        Ok(())
    }

    pub fn read(&self, digest: &ContentDigest) -> Result<Vec<u8>> {
        // Convenience for small metadata only. File artifacts must use copy_to
        // or materialize; don't accidentally allocate a guest image in RAM.
        const LIMIT: u64 = 8 * 1024 * 1024;
        let bytes = read_bounded(self.open(digest)?, LIMIT, "artifact").map_err(|e| {
            e.or_too_large(|message| {
                Error::Integrity(format!(
                    "{message} metadata read limit; use copy_to/materialize"
                ))
            })
        })?;
        if ContentDigest::of(&bytes) != *digest {
            return Err(Error::Integrity(format!("hash mismatch: {}", digest.hex())));
        }
        Ok(bytes)
    }

    /// Size of a regular retained object, for bounded streaming containers.
    pub fn size(&self, digest: &ContentDigest) -> Result<u64> {
        let metadata = fs::symlink_metadata(self.object_path(digest))
            .context(Error::Integrity, digest.hex())?;
        if !metadata.is_file() {
            return Err(Error::Integrity(format!(
                "{}: object is not a regular file",
                digest.hex()
            )));
        }
        Ok(metadata.len())
    }

    fn open(&self, digest: &ContentDigest) -> Result<fs::File> {
        fs::File::open(self.object_path(digest)).context(Error::Integrity, digest.hex())
    }

    /// Reuse a verification only while the retained file's identity and metadata
    /// match the bytes this process hashed. A changed stamp forces a fresh hash.
    pub fn verify(&self, digest: &ContentDigest) -> Result<()> {
        self.verify_observed(digest, &mut |_| Ok(()))
    }

    fn verify_observed(
        &self,
        digest: &ContentDigest,
        progress: &mut impl FnMut(u64) -> io::Result<()>,
    ) -> Result<()> {
        progress(0)?;
        let mut file = self.open(digest)?;
        if self.trusted(digest, &file)?.is_some() {
            return Ok(());
        }
        let before = Stamp::of(&file)?;
        let actual = transfer_observed(&mut file, &mut io::sink(), progress)?;
        if actual != *digest {
            self.forget(digest);
            return Err(Error::Integrity(format!("hash mismatch: {}", digest.hex())));
        }
        if Stamp::of(&file)? == before {
            self.lock().insert(digest.clone(), before);
        }
        Ok(())
    }

    /// Streams into a caller-owned destination. Publish that destination only
    /// after this returns Ok: corruption is detected after the final byte.
    pub fn copy_to(&self, digest: &ContentDigest, output: &mut impl Write) -> Result<()> {
        let mut file = self.open(digest)?;
        let before = Stamp::of(&file)?;
        let actual = transfer(&mut file, output)?;
        if actual != *digest {
            self.forget(digest);
            return Err(Error::Integrity(format!("hash mismatch: {}", digest.hex())));
        }
        // Only an object that did not change while it was hashed counts.
        if Stamp::of(&file)? == before {
            self.lock().insert(digest.clone(), before);
        }
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<ContentDigest, Stamp>> {
        self.verified.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn forget(&self, digest: &ContentDigest) {
        self.lock().remove(digest);
    }

    /// Whether this process already hashed `file` as `digest`, unchanged since.
    fn trusted(&self, digest: &ContentDigest, file: &fs::File) -> io::Result<Option<Stamp>> {
        let stamp = Stamp::of(file)?;
        Ok((self.lock().get(digest) == Some(&stamp)).then_some(stamp))
    }

    /// Copy verified bytes into the run's private input directory. Never hardlink
    /// the content store into a backend's writable execution workspace.
    ///
    /// An object this process already hashed, unchanged since, is cloned
    /// (copy-on-write where the filesystem supports it) or copied in the
    /// kernel without hashing again. Others stream through the hash as before.
    /// The copy is scratch for one execution, so it is not synced: after a
    /// crash the run is INCOMPLETE and replay materializes from the store.
    pub fn materialize(&self, digest: &ContentDigest, directory: &Path) -> Result<PathBuf> {
        fs::create_dir_all(directory)?;
        let path = directory.join(digest.hex());
        let mut temp = tempfile::NamedTempFile::new_in(directory)?;
        let mut source = self.open(digest)?;
        match self.trusted(digest, &source)? {
            Some(stamp) => {
                if !clone_file(&source, temp.as_file())? {
                    io::copy(&mut source, temp.as_file_mut())?;
                }
                // Changed while copying: fall back to hashing the bytes.
                if Stamp::of(&source)? != stamp || temp.as_file().metadata()?.len() != stamp.len {
                    self.forget(digest);
                    temp = tempfile::NamedTempFile::new_in(directory)?;
                    self.copy_to(digest, &mut temp)?;
                }
            }
            None => self.copy_to(digest, &mut temp)?,
        }
        temp.persist_noclobber(&path).map_err(|e| e.error)?;
        Ok(path)
    }

    /// `materialize` for a file this process (or one it launches) will execute,
    /// such as the retained engine. The write holds [`seedbox_exec::writing`]
    /// from opening the destination until every descriptor to it is closed, so
    /// no concurrent fork can keep a writable copy that makes the first
    /// execution fail with ETXTBSY. Only executables pay the exclusion; guest
    /// inputs are read as bytes and use plain `materialize`.
    pub fn materialize_executable(
        &self,
        digest: &ContentDigest,
        directory: &Path,
    ) -> Result<PathBuf> {
        let _writing = seedbox_exec::writing();
        self.materialize(digest, directory)
    }
}

/// Whether `path` existed; a missing file is already removed.
fn remove_if_present(path: &Path) -> io::Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Share `source`'s extents with the empty `destination` (FICLONE). False
/// where the filesystem cannot, such as ext4 or across filesystems.
#[cfg(target_os = "linux")]
fn clone_file(source: &fs::File, destination: &fs::File) -> io::Result<bool> {
    use std::os::fd::AsRawFd;
    // SAFETY: both descriptors are open for the duration of the call.
    if unsafe { libc::ioctl(destination.as_raw_fd(), libc::FICLONE, source.as_raw_fd()) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EOPNOTSUPP | libc::EXDEV | libc::EINVAL | libc::ENOTTY | libc::ENOSYS) => {
            Ok(false)
        }
        _ => Err(error),
    }
}

#[cfg(not(target_os = "linux"))]
fn clone_file(_: &fs::File, _: &fs::File) -> io::Result<bool> {
    Ok(false)
}

fn transfer(input: &mut impl Read, output: &mut impl Write) -> Result<ContentDigest> {
    transfer_observed(input, output, &mut |_| Ok(()))
}

fn transfer_observed(
    input: &mut impl Read,
    output: &mut impl Write,
    progress: &mut impl FnMut(u64) -> io::Result<()>,
) -> Result<ContentDigest> {
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        progress(0)?;
        let count = match input.read(&mut buffer) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        output.write_all(&buffer[..count])?;
        progress(count as u64)?;
    }
    Ok(ContentDigest::finish(hash))
}

#[cfg(test)]
mod observed_tests;
