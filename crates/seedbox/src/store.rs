//! Verified local content store. A digest alone is never proof of retention.

use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ContentDigest(String);

impl ContentDigest {
    pub fn of(bytes: &[u8]) -> Self {
        Self(format!("{:x}", Sha256::digest(bytes)))
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
}

impl ContentStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path(&self, digest: &ContentDigest) -> PathBuf {
        self.root.join("sha256").join(digest.hex())
    }

    pub fn put(&self, bytes: &[u8]) -> Result<ContentDigest> {
        self.put_reader(bytes)
    }

    /// Retain arbitrarily large artifacts with a bounded 64 KiB transfer buffer.
    pub fn put_reader(&self, mut reader: impl Read) -> Result<ContentDigest> {
        self.put_reader_checked(&mut reader, &|| Ok(()))
    }

    /// As `put_reader`, checking cancellation during both capture and the final
    /// existing-object verification. Published complete bytes remain immutable.
    pub fn put_reader_checked(
        &self,
        mut reader: impl Read,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<ContentDigest> {
        check()?;
        let directory = self.root.join("sha256");
        fs::create_dir_all(&directory)?;
        let mut temp = tempfile::NamedTempFile::new_in(&directory)?;
        let digest = transfer(&mut reader, &mut temp, check)?;
        let path = self.path(&digest);
        temp.as_file().sync_all()?;
        check()?;
        match temp.persist_noclobber(&path) {
            Ok(_) => {}
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.error.into()),
        }
        // Verify an existing entry too; never repair corruption implicitly.
        self.verify_checked(&digest, check)?;
        Ok(digest)
    }

    pub fn put_file(&self, path: &Path) -> Result<ContentDigest> {
        self.put_reader(fs::File::open(path)?)
    }

    pub fn put_file_checked(
        &self,
        path: &Path,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<ContentDigest> {
        check()?;
        self.put_reader_checked(fs::File::open(path)?, check)
    }

    pub fn read(&self, digest: &ContentDigest) -> Result<Vec<u8>> {
        self.read_checked(digest, &|| Ok(()))
    }

    pub fn read_checked(
        &self,
        digest: &ContentDigest,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<Vec<u8>> {
        // Convenience for small metadata only. File artifacts must use copy_to
        // or materialize; don't accidentally allocate a guest image in RAM.
        const LIMIT: u64 = 8 * 1024 * 1024;
        let mut bytes = Vec::new();
        check()?;
        let actual = transfer(&mut self.open(digest)?.take(LIMIT + 1), &mut bytes, check)?;
        if bytes.len() as u64 > LIMIT {
            return Err(Error::Integrity(
                "artifact exceeds 8 MiB metadata read limit; use copy_to/materialize".into(),
            ));
        }
        if actual != *digest {
            return Err(Error::Integrity(format!("hash mismatch: {}", digest.hex())));
        }
        Ok(bytes)
    }

    /// Size of a regular retained object, for bounded streaming containers.
    pub fn size(&self, digest: &ContentDigest) -> Result<u64> {
        let metadata = fs::symlink_metadata(self.path(digest))
            .map_err(|e| Error::Integrity(format!("{}: {e}", digest.hex())))?;
        if !metadata.is_file() {
            return Err(Error::Integrity(format!(
                "{}: object is not a regular file",
                digest.hex()
            )));
        }
        Ok(metadata.len())
    }

    fn open(&self, digest: &ContentDigest) -> Result<fs::File> {
        fs::File::open(self.path(digest))
            .map_err(|e| Error::Integrity(format!("{}: {e}", digest.hex())))
    }

    pub fn verify(&self, digest: &ContentDigest) -> Result<()> {
        self.copy_to(digest, &mut io::sink())
    }

    pub fn verify_checked(
        &self,
        digest: &ContentDigest,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        self.copy_to_checked(digest, &mut io::sink(), check)
    }

    /// Streams into a caller-owned destination. Publish that destination only
    /// after this returns Ok: corruption is detected after the final byte.
    pub fn copy_to(&self, digest: &ContentDigest, output: &mut impl Write) -> Result<()> {
        self.copy_to_checked(digest, output, &|| Ok(()))
    }

    pub fn copy_to_checked(
        &self,
        digest: &ContentDigest,
        output: &mut impl Write,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        check()?;
        let actual = transfer(&mut self.open(digest)?, output, check)?;
        if actual != *digest {
            return Err(Error::Integrity(format!("hash mismatch: {}", digest.hex())));
        }
        Ok(())
    }

    /// Copy verified bytes into the run's private input directory. Never hardlink
    /// the content store into a backend's writable execution workspace.
    pub fn materialize(&self, digest: &ContentDigest, directory: &Path) -> Result<PathBuf> {
        self.materialize_checked(digest, directory, &|| Ok(()))
    }

    pub fn materialize_checked(
        &self,
        digest: &ContentDigest,
        directory: &Path,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<PathBuf> {
        check()?;
        fs::create_dir_all(directory)?;
        let path = directory.join(digest.hex());
        let mut temp = tempfile::NamedTempFile::new_in(directory)?;
        self.copy_to_checked(digest, &mut temp, check)?;
        temp.as_file().sync_all()?;
        check()?;
        temp.persist_noclobber(&path).map_err(|e| e.error)?;
        Ok(path)
    }
}

fn transfer(
    input: &mut impl Read,
    output: &mut impl Write,
    check: &dyn Fn() -> Result<()>,
) -> Result<ContentDigest> {
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        check()?;
        let count = match input.read(&mut buffer) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        check()?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        output.write_all(&buffer[..count])?;
    }
    Ok(ContentDigest(format!("{:x}", hash.finalize())))
}
