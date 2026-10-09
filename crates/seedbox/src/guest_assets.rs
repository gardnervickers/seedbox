//! Embedded or caller-supplied guest helpers, extracted privately and removed on drop.
include!(concat!(env!("OUT_DIR"), "/guest-helpers.rs"));
use crate::{store::ContentDigest, Error, Result};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

const REQUIRED: [&str; 4] = ["compose-init", "mke2fs", "NOTICE.txt", "provenance.json"];
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    version: u32,
    #[serde(deserialize_with = "unique_map")]
    files: BTreeMap<String, ContentDigest>,
}
#[derive(Clone, Debug)]
pub struct Runtime {
    pub root: PathBuf,
    _directory: std::sync::Arc<tempfile::TempDir>,
}
impl Runtime {
    pub fn from_archive(path: &Path) -> Result<Self> {
        if fs::metadata(path)?.len() > 512 << 20 {
            return Err(Error::Integrity(
                "compressed runtime exceeds 512 MiB".into(),
            ));
        }
        Self::from_reader(fs::File::open(path)?)
    }
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > 512 << 20 {
            return Err(Error::Integrity(
                "compressed runtime exceeds 512 MiB".into(),
            ));
        }
        Self::from_reader(bytes)
    }
    fn from_reader(reader: impl Read) -> Result<Self> {
        let stage = tempfile::tempdir()?;
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(reader));
        let mut names = std::collections::BTreeSet::new();
        let mut size = 0u64;
        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            let name = path
                .to_str()
                .ok_or_else(|| Error::Integrity("non-UTF-8 runtime member".into()))?;
            if !entry.header().entry_type().is_file()
                || !valid_member(name)
                || !names.insert(name.to_owned())
            {
                return Err(Error::Integrity(
                    "invalid or duplicate runtime member".into(),
                ));
            }
            size = size
                .checked_add(entry.size())
                .filter(|size| *size <= 2 << 30)
                .ok_or_else(|| Error::Integrity("runtime exceeds 2 GiB".into()))?;
            entry.unpack(stage.path().join(name))?;
        }
        Self::validate(stage.path())?;
        Ok(Self {
            root: stage.path().to_owned(),
            _directory: std::sync::Arc::new(stage),
        })
    }
    fn validate(root: &Path) -> Result<Manifest> {
        if !fs::symlink_metadata(root)?.is_dir()
            || !fs::symlink_metadata(root.join("runtime.json"))?.is_file()
        {
            return Err(Error::Integrity(
                "invalid runtime archive extraction".into(),
            ));
        }
        let manifest: Manifest =
            serde_json::from_reader(fs::File::open(root.join("runtime.json"))?.take(1 << 20))?;
        if manifest.version != 1
            || REQUIRED
                .iter()
                .any(|name| !manifest.files.contains_key(*name))
        {
            return Err(Error::Integrity("invalid runtime manifest".into()));
        }
        for (name, digest) in &manifest.files {
            if !valid_member(name) || name == "runtime.json" {
                return Err(Error::Integrity("invalid runtime manifest path".into()));
            }
            let path = root.join(name);
            if !fs::symlink_metadata(&path)?.is_file() || package_digest(&path)? != *digest {
                return Err(Error::Integrity(format!(
                    "runtime {name} hash mismatch in {}; rebuild guest assets",
                    root.display()
                )));
            }
        }
        Ok(manifest)
    }
    pub fn input(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
}

fn valid_member(name: &str) -> bool {
    REQUIRED.contains(&name)
        || [
            "runtime.json",
            "kernel.config",
            "counter-audit.txt",
            "bzImage",
            "System.map",
            "SHA256SUMS",
        ]
        .contains(&name)
}

fn package_digest(path: &Path) -> Result<ContentDigest> {
    use sha2::{Digest, Sha256};
    let mut reader = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    ContentDigest::try_from(format!("{:x}", hash.finalize()))
}

pub(crate) fn unique_map<'de, D, T>(
    deserializer: D,
) -> std::result::Result<BTreeMap<String, T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Unique<T>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for Unique<T> {
        type Value = BTreeMap<String, T>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a map with unique keys")
        }
        fn visit_map<M: serde::de::MapAccess<'de>>(
            self,
            mut input: M,
        ) -> std::result::Result<Self::Value, M::Error> {
            let mut output = BTreeMap::new();
            while let Some((key, value)) = input.next_entry::<String, T>()? {
                if output.insert(key, value).is_some() {
                    return Err(serde::de::Error::custom("duplicate metadata map key"));
                }
            }
            Ok(output)
        }
    }
    deserializer.deserialize_map(Unique(std::marker::PhantomData))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn archive(path: &Path, corrupt: bool) {
        let files = REQUIRED
            .iter()
            .map(|name| (name.to_string(), ContentDigest::of(name.as_bytes())))
            .collect();
        let manifest = Manifest { version: 1, files };
        let file = fs::File::create(path).unwrap();
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
            file,
            flate2::Compression::fast(),
        ));
        for (name, data) in REQUIRED
            .iter()
            .map(|name| (*name, name.as_bytes().to_vec()))
            .chain(std::iter::once((
                "runtime.json",
                serde_json::to_vec(&manifest).unwrap(),
            )))
        {
            let data = if corrupt && name == "compose-init" {
                b"broken".to_vec()
            } else {
                data
            };
            let mut h = tar::Header::new_gnu();
            h.set_mode(0o644);
            h.set_size(data.len() as u64);
            tar.append_data(&mut h, name, data.as_slice()).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
    }
    #[test]
    fn extraction_lives_only_as_long_as_its_owners() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("assets.tar.gz");
        archive(&path, false);
        let runtime = Runtime::from_bytes(&fs::read(&path).unwrap()).unwrap();
        let root = runtime.root.clone();
        let shared = runtime.clone();
        assert_eq!(
            fs::read(runtime.input("compose-init")).unwrap(),
            b"compose-init"
        );
        drop(runtime);
        assert!(root.is_dir());
        drop(shared);
        assert!(!root.exists());
        assert!(path.is_file());
    }
    #[test]
    fn changed_archive_member_fails_integrity_validation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("assets.tar.gz");
        archive(&path, true);
        assert!(Runtime::from_archive(&path)
            .unwrap_err()
            .to_string()
            .contains("hash mismatch"));
    }
}
