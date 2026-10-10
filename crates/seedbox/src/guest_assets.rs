//! Embedded or caller-supplied guest helpers, extracted privately and removed on drop.
include!(concat!(env!("OUT_DIR"), "/guest-helpers.rs"));
use crate::{limits::exceeds, Error, Result};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

const REQUIRED: [&str; 4] = ["compose-init", "mke2fs", "NOTICE.txt", "progress.tar"];
const ARCHIVE_LIMIT: u64 = 512 << 20;
const UNPACKED_LIMIT: u64 = 2 << 30;
#[derive(Clone, Debug)]
pub struct Runtime {
    pub root: PathBuf,
    _directory: std::sync::Arc<tempfile::TempDir>,
}
impl Runtime {
    pub fn from_archive(path: &Path) -> Result<Self> {
        if fs::metadata(path)?.len() > ARCHIVE_LIMIT {
            return Err(Error::Integrity(exceeds(
                "compressed runtime",
                ARCHIVE_LIMIT,
            )));
        }
        Self::from_reader(fs::File::open(path)?)
    }
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() as u64 > ARCHIVE_LIMIT {
            return Err(Error::Integrity(exceeds(
                "compressed runtime",
                ARCHIVE_LIMIT,
            )));
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
                || !REQUIRED.contains(&name)
                || !names.insert(name.to_owned())
            {
                return Err(Error::Integrity(format!(
                    "invalid or duplicate runtime member {name}; rebuild with python3 tools/build.py"
                )));
            }
            size = size
                .checked_add(entry.size())
                .filter(|size| *size <= UNPACKED_LIMIT)
                .ok_or_else(|| Error::Integrity(exceeds("runtime", UNPACKED_LIMIT)))?;
            entry.unpack(stage.path().join(name))?;
        }
        for name in REQUIRED {
            if !names.contains(name) {
                return Err(Error::Setup(format!(
                    "runtime archive missing {name}; rebuild with python3 tools/build.py"
                )));
            }
        }
        Ok(Self {
            root: stage.path().to_owned(),
            _directory: std::sync::Arc::new(stage),
        })
    }
    pub fn input(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn archive(names: &[&str], kind: tar::EntryType) -> Vec<u8> {
        let mut archive = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        for name in names {
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o644);
            header.set_entry_type(kind);
            if kind.is_file() {
                header.set_size(name.len() as u64);
                archive
                    .append_data(&mut header, name, name.as_bytes())
                    .unwrap();
            } else {
                header.set_size(0);
                header.set_link_name("/outside").unwrap();
                archive
                    .append_data(&mut header, name, std::io::empty())
                    .unwrap();
            }
        }
        archive.into_inner().unwrap().finish().unwrap()
    }
    #[test]
    fn helpers_without_metadata_live_only_as_long_as_their_owners() {
        let bytes = archive(&REQUIRED, tar::EntryType::Regular);
        let runtime = Runtime::from_bytes(&bytes).unwrap();
        let root = runtime.root.clone();
        let shared = runtime.clone();
        assert_eq!(
            fs::read(runtime.input("compose-init")).unwrap(),
            b"compose-init"
        );
        assert!(!runtime.input("runtime.json").exists());
        assert!(!runtime.input("provenance.json").exists());
        drop(runtime);
        assert!(root.is_dir());
        drop(shared);
        assert!(!root.exists());
    }
    #[test]
    fn missing_members_explain_how_to_rebuild() {
        let bytes = archive(&["compose-init", "NOTICE.txt"], tar::EntryType::Regular);
        let error = Runtime::from_bytes(&bytes).unwrap_err().to_string();
        assert!(error.contains("missing mke2fs"));
        assert!(error.contains("python3 tools/build.py"));
    }
    #[test]
    fn archive_size_limits_are_checked_before_extracting_payloads() {
        use std::io::Write;
        let mut header = tar::Header::new_gnu();
        header.set_path("compose-init").unwrap();
        header.set_mode(0o755);
        header.set_size((2 << 30) + 1);
        header.set_cksum();
        let mut compressed = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        compressed.write_all(header.as_bytes()).unwrap();
        let bytes = compressed.finish().unwrap();
        assert!(Runtime::from_bytes(&bytes)
            .unwrap_err()
            .to_string()
            .contains("runtime exceeds 2 GiB"));
        let file = tempfile::NamedTempFile::new().unwrap();
        file.as_file().set_len((512 << 20) + 1).unwrap();
        assert!(Runtime::from_archive(file.path())
            .unwrap_err()
            .to_string()
            .contains("compressed runtime exceeds 512 MiB"));
    }

    #[test]
    fn duplicate_unexpected_and_link_members_are_rejected() {
        for (names, kind) in [
            (
                vec!["compose-init", "compose-init"],
                tar::EntryType::Regular,
            ),
            (vec!["nested/compose-init"], tar::EntryType::Regular),
            (vec!["compose-init"], tar::EntryType::Symlink),
            (vec!["compose-init"], tar::EntryType::Link),
        ] {
            assert!(Runtime::from_bytes(&archive(&names, kind))
                .unwrap_err()
                .to_string()
                .contains("invalid or duplicate runtime member"));
        }
    }
}
