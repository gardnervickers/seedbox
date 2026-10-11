//! Verified local content store. A digest alone is never proof of retention.

use crate::{limits::read_bounded, Context, Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::{self, Read, Seek, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
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
    /// Clones share it, so exploration workers verify each object once.
    verified: Arc<Mutex<HashMap<ContentDigest, Stamp>>>,
    /// Shared copies made by `materialize_linked`, with the identity each had
    /// when this process wrote it. Holding the lock serializes their creation.
    shared: Arc<Mutex<HashMap<PathBuf, Stamp>>>,
}

/// What any write, truncate, rename over or chmod of an object changes.
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

/// Digests of files outside the store that this process hashed, by inode,
/// with the stamp each had then. Profile construction, admission and
/// preparation each hash the same engine and kernel; this reads them once.
static HASHED_FILES: Mutex<BTreeMap<(u64, u64), (Stamp, ContentDigest)>> =
    Mutex::new(BTreeMap::new());

/// The digest of `file`, hashed with `hash` unless this process already
/// hashed the same inode and its stamp is unchanged since. A file that
/// changes while it is hashed is not remembered.
pub fn file_digest_with(
    file: &mut fs::File,
    hash: impl FnOnce(&mut fs::File) -> Result<ContentDigest>,
) -> Result<ContentDigest> {
    let before = Stamp::of(file)?;
    let key = (before.dev, before.ino);
    let lock = || HASHED_FILES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((stamp, digest)) = lock().get(&key) {
        if *stamp == before {
            return Ok(digest.clone());
        }
    }
    let digest = hash(file)?;
    if Stamp::of(file)? == before {
        lock().insert(key, (before, digest.clone()));
    }
    Ok(digest)
}

impl ContentStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            evidence_root: None,
            verified: Arc::default(),
            shared: Arc::default(),
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

    /// Retain a run's own evidence (identity, logs) without syncing it. A host
    /// crash can lose such an object, and a record that names it then fails
    /// verification as an integrity error; it is never silently wrong. Putting
    /// the same bytes unsynced again replaces such an object. Inputs
    /// that later runs replay from keep using the synced `put*` calls. With
    /// `with_ephemeral_evidence`, these objects live only in the session store.
    pub fn put_unsynced(&self, bytes: &[u8]) -> Result<ContentDigest> {
        self.put_reader_with(bytes, false)
    }

    /// `put_unsynced` for a file.
    pub fn put_file_unsynced(&self, path: &Path) -> Result<ContentDigest> {
        self.put_reader_with(fs::File::open(path)?, false)
    }

    fn put_reader_with(&self, mut reader: impl Read, durable: bool) -> Result<ContentDigest> {
        let root = if durable {
            &self.root
        } else {
            self.evidence_root.as_ref().unwrap_or(&self.root)
        };
        let directory = root.join("sha256");
        fs::create_dir_all(&directory)?;
        let mut temp = tempfile::NamedTempFile::new_in(&directory)?;
        let digest = transfer(&mut reader, &mut temp)?;
        let path = directory.join(digest.hex());
        if durable {
            temp.as_file().sync_all()?;
        }
        match temp.persist_noclobber(&path) {
            Ok(_) => {}
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
                // A host crash can leave an unsynced object named but short.
                // These bytes hash to the digest, so put them in its place.
                if !durable && self.verify(&digest).is_err() {
                    e.file.persist(&path).map_err(|e| e.error)?;
                }
            }
            Err(e) => return Err(e.error.into()),
        }
        // Verify an existing entry too; never repair corruption implicitly.
        self.verify(&digest)?;
        Ok(digest)
    }

    /// Like `put_reader`, but an object already in the store is only read:
    /// the file is hashed first, so retaining the same engine, kernel or image
    /// again never rewrites and syncs a second copy of it.
    pub fn put_file(&self, path: &Path) -> Result<ContentDigest> {
        let mut file = fs::File::open(path)?;
        let digest = file_digest_with(&mut file, |file| transfer(file, &mut io::sink()))?;
        if let Ok(existing) = fs::File::open(self.path(&digest)) {
            // Verify the existing entry; never repair corruption implicitly.
            if self.trusted(&digest, &existing)?.is_none() {
                self.verify(&digest)?;
            }
            return Ok(digest);
        }
        file.seek(io::SeekFrom::Start(0))?;
        self.put_reader(file)
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
    /// match the bytes this process hashed. Replacement or mutation rehashes it.
    pub fn verify(&self, digest: &ContentDigest) -> Result<()> {
        let file = self.open(digest)?;
        if self.trusted(digest, &file)?.is_some() {
            return Ok(());
        }
        self.copy_to(digest, &mut io::sink())
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
        let mut copy = temp.persist_noclobber(&path).map_err(|e| e.error)?;
        // The copy holds exactly the object's bytes, so a later put_file or
        // file_digest of it needs no second read.
        file_digest_with(&mut copy, |_| Ok(digest.clone()))?;
        Ok(path)
    }

    /// Like `materialize`, but copies each object once into `shared` and
    /// hard-links that read-only copy into `directory`. Exploration attempts
    /// use it so a batch copies its engine and images once, not per seed.
    ///
    /// The shared copy is reused only while both it and the store object are
    /// unchanged since this process verified them; otherwise it is replaced
    /// through `materialize`, which hashes again. Without hard links (another
    /// filesystem) this falls back to a private copy.
    pub fn materialize_linked(
        &self,
        digest: &ContentDigest,
        shared: &Path,
        directory: &Path,
    ) -> Result<PathBuf> {
        let source = shared.join(digest.hex());
        let mut copies = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        let reusable = match (copies.get(&source), fs::File::open(&source)) {
            (Some(stamp), Ok(copy)) => {
                Stamp::of(&copy)? == *stamp && self.trusted(digest, &self.open(digest)?)?.is_some()
            }
            _ => false,
        };
        if !reusable {
            copies.remove(&source);
            match fs::remove_file(&source) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
            let copy = self.materialize(digest, shared)?;
            // Read-only for every attempt; the engine copy also needs execute.
            fs::set_permissions(&copy, fs::Permissions::from_mode(0o500))?;
            copies.insert(copy, Stamp::of(&fs::File::open(&source)?)?);
        }
        fs::create_dir_all(directory)?;
        let path = directory.join(digest.hex());
        match fs::hard_link(&source, &path) {
            // Linking changes the copy's ctime; remember the new identity.
            Ok(()) => {
                copies.insert(source.clone(), Stamp::of(&fs::File::open(&source)?)?);
                Ok(path)
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(e.into()),
            Err(_) => {
                drop(copies);
                self.materialize(digest, directory)
            }
        }
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
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = match input.read(&mut buffer) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        output.write_all(&buffer[..count])?;
    }
    Ok(ContentDigest::finish(hash))
}
