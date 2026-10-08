//! Embedded x86 Linux guest bytes, validated without executing host or guest tools.
//! Every installation owns private temporary storage; no shared cache is trusted.
use crate::{store::ContentDigest, Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

include!(concat!(env!("OUT_DIR"), "/linux-runtime.rs"));
const COMPRESSED_LIMIT: usize = 256 << 20;
const DECOMPRESSED_LIMIT: u64 = 512 << 20;
const MEMBERS: [&str; 7] = [
    "assets.json",
    "bzImage",
    "compose-init",
    "busybox",
    "mke2fs",
    "NOTICE.txt",
    "provenance.json",
];
const GUEST_FILES: [&str; 4] = ["bzImage", "compose-init", "busybox", "mke2fs"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssetSelection {
    Kernel,
    All,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileRecord {
    pub role: String,
    pub execution_arch: Option<String>,
    pub size: u64,
    pub sha256: ContentDigest,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: u32,
    pub host_platform: String,
    pub guest_arch: String,
    #[serde(deserialize_with = "unique_map")]
    pub files: BTreeMap<String, FileRecord>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRecord {
    pub sha256: ContentDigest,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub format: u32,
    pub source_revision: String,
    pub source_dirty: bool,
    pub flake_lock_sha256: ContentDigest,
    pub notice_sha256: ContentDigest,
    #[serde(deserialize_with = "unique_map")]
    pub recipes: BTreeMap<String, ContentDigest>,
    #[serde(deserialize_with = "unique_map")]
    pub sources: BTreeMap<String, SourceRecord>,
    #[serde(deserialize_with = "unique_map")]
    pub artifacts: BTreeMap<String, ContentDigest>,
}
#[derive(Debug)]
pub struct RuntimeAssets {
    directory: tempfile::TempDir,
    pub manifest: Manifest,
    pub provenance: Provenance,
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
fn invalid(message: impl Into<String>) -> Error {
    Error::Integrity(format!("Linux guest assets: {}", message.into()))
}
fn role(name: &str) -> Option<(&'static str, Option<&'static str>, u64)> {
    match name {
        "assets.json" => Some(("manifest", None, 64 << 10)),
        "bzImage" => Some(("kernel", None, 128 << 20)),
        "compose-init" => Some(("supervisor", Some("x86_64"), 128 << 20)),
        "busybox" => Some(("busybox", Some("x86_64"), 128 << 20)),
        "mke2fs" => Some(("filesystem_formatter", Some("x86_64"), 128 << 20)),
        "NOTICE.txt" => Some(("notice", None, 8 << 20)),
        "provenance.json" => Some(("provenance", None, 1 << 20)),
        _ => None,
    }
}
impl RuntimeAssets {
    pub fn embedded(parent: &Path, selection: AssetSelection) -> Result<Option<Self>> {
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            LINUX_RUNTIME
                .map(|bytes| Self::install(bytes, parent, "x86_64-linux", selection))
                .transpose()
        }
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
        {
            let _ = (parent, selection);
            Ok(None)
        }
    }
    pub fn input(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }
    pub fn install(
        bytes: &[u8],
        parent: &Path,
        platform: &str,
        selection: AssetSelection,
    ) -> Result<Self> {
        if platform != "x86_64-linux" {
            return Err(invalid("unsupported host CLI platform"));
        }
        if bytes.is_empty() || bytes.len() > COMPRESSED_LIMIT {
            return Err(invalid("compressed package size exceeds limit"));
        }
        let directory = tempfile::Builder::new()
            .prefix("seedbox-linux-assets-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(parent)?;
        let mut decoder = flate2::bufread::GzDecoder::new(bytes);
        let mut total = 0u64;
        let mut names = BTreeSet::new();
        let mut records = BTreeMap::new();
        let mut manifest = None;
        let mut provenance = None;
        loop {
            let mut header = [0u8; 512];
            read_bounded(&mut decoder, &mut header, &mut total)?;
            if header.iter().all(|byte| *byte == 0) {
                read_bounded(&mut decoder, &mut header, &mut total)?;
                if header.iter().any(|byte| *byte != 0) {
                    return Err(invalid("tar terminator is incomplete"));
                }
                // Standard tar record padding is zero. Hidden members or any
                // nonzero suffix after the terminator are rejected.
                loop {
                    let mut suffix = [0u8; 8192];
                    let count = decoder
                        .read(&mut suffix)
                        .map_err(|e| invalid(format!("gzip trailer: {e}")))?;
                    if count == 0 {
                        break;
                    }
                    add_size(&mut total, count as u64)?;
                    if suffix[..count].iter().any(|byte| *byte != 0) {
                        return Err(invalid("nonzero content after tar terminator"));
                    }
                }
                break;
            }
            if names.len() >= MEMBERS.len() {
                return Err(invalid("too many archive entries"));
            }
            let expected_sum: u64 = header[..148]
                .iter()
                .chain(header[156..].iter())
                .map(|b| *b as u64)
                .sum::<u64>()
                + 8 * u64::from(b' ');
            if octal(&header[148..156])? != expected_sum {
                return Err(invalid("tar header checksum mismatch"));
            }
            if !matches!(header[156], 0 | b'0')
                || header[157..257].iter().any(|b| *b != 0)
                || header[345..500].iter().any(|b| *b != 0)
                || &header[257..263] != b"ustar\0"
                || &header[263..265] != b"00"
            {
                return Err(invalid("archive requires simple USTAR regular files"));
            }
            let end = header[..100].iter().position(|b| *b == 0).unwrap_or(100);
            if header[end..100].iter().any(|b| *b != 0) {
                return Err(invalid("invalid tar member name padding"));
            }
            let name = std::str::from_utf8(&header[..end])
                .map_err(|_| invalid("archive name is not UTF-8"))?;
            let (expected_role, execution_arch, limit) =
                role(name).ok_or_else(|| invalid("unexpected archive member"))?;
            if !names.insert(name.to_owned()) {
                return Err(invalid("duplicate archive member"));
            }
            let size = octal(&header[124..136])?;
            if size == 0 || size > limit {
                return Err(invalid(format!("{name} size exceeds role limit")));
            }
            // Check total size before allocating the member buffer.
            if total
                .checked_add(size)
                .is_none_or(|size| size > DECOMPRESSED_LIMIT)
            {
                return Err(invalid("decompressed package exceeds limit"));
            }
            let mut content =
                vec![0; usize::try_from(size).map_err(|_| invalid("member size overflow"))?];
            read_bounded(&mut decoder, &mut content, &mut total)?;
            let mut padding = vec![0u8; ((512 - size % 512) % 512) as usize];
            read_bounded(&mut decoder, &mut padding, &mut total)?;
            if padding.iter().any(|byte| *byte != 0) {
                return Err(invalid("nonzero member padding"));
            }
            match name {
                "assets.json" => {
                    manifest = Some(
                        serde_json::from_slice::<Manifest>(&content)
                            .map_err(|e| invalid(format!("manifest: {e}")))?,
                    );
                }
                "provenance.json" => {
                    provenance = Some(
                        serde_json::from_slice::<Provenance>(&content)
                            .map_err(|e| invalid(format!("provenance: {e}")))?,
                    );
                }
                "NOTICE.txt" => {
                    if std::str::from_utf8(&content)
                        .map_err(|_| invalid("notices are not UTF-8"))?
                        .trim()
                        .is_empty()
                    {
                        return Err(invalid("empty notices"));
                    }
                }
                "bzImage" => validate_kernel(&content)?,
                _ => validate_elf(&content)?,
            }
            if name != "assets.json" {
                records.insert(
                    name.to_owned(),
                    FileRecord {
                        role: expected_role.into(),
                        execution_arch: execution_arch.map(str::to_owned),
                        size,
                        sha256: ContentDigest::of(&content),
                    },
                );
            }
            if GUEST_FILES.contains(&name)
                && (selection == AssetSelection::All || name == "bzImage")
            {
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(directory.path().join(name))?;
                file.write_all(&content)?;
                fs::set_permissions(
                    directory.path().join(name),
                    fs::Permissions::from_mode(if name == "bzImage" { 0o600 } else { 0o755 }),
                )?;
            }
        }
        if !decoder.into_inner().is_empty() {
            return Err(invalid(
                "trailing compressed content or multiple gzip members",
            ));
        }
        if names != MEMBERS.into_iter().map(str::to_owned).collect() {
            return Err(invalid("missing archive members"));
        }
        let manifest = manifest.ok_or_else(|| invalid("missing manifest"))?;
        if manifest.format != 1
            || manifest.host_platform != platform
            || manifest.guest_arch != "x86_64"
            || manifest.files != records
        {
            return Err(invalid(
                "manifest platform, roles, sizes or hashes disagree with payload",
            ));
        }
        let provenance = provenance.ok_or_else(|| invalid("missing provenance"))?;
        let artifacts: BTreeMap<_, _> = GUEST_FILES
            .into_iter()
            .map(|name| (name.to_owned(), records[name].sha256.clone()))
            .collect();
        if provenance.format != 1
            || provenance.notice_sha256 != records["NOTICE.txt"].sha256
            || provenance.artifacts != artifacts
        {
            return Err(invalid("notice/artifact provenance mismatch"));
        }
        Ok(Self {
            directory,
            manifest,
            provenance,
        })
    }
}
fn add_size(total: &mut u64, size: u64) -> Result<()> {
    *total = total
        .checked_add(size)
        .filter(|total| *total <= DECOMPRESSED_LIMIT)
        .ok_or_else(|| invalid("decompressed package exceeds limit"))?;
    Ok(())
}
fn read_bounded(input: &mut impl Read, bytes: &mut [u8], total: &mut u64) -> Result<()> {
    add_size(total, bytes.len() as u64)?;
    input
        .read_exact(bytes)
        .map_err(|e| invalid(format!("truncated archive: {e}")))
}
fn octal(field: &[u8]) -> Result<u64> {
    let text = std::str::from_utf8(field)
        .map_err(|_| invalid("non-ASCII tar number"))?
        .trim_matches(['\0', ' ']);
    if text.is_empty() || !text.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
        return Err(invalid("invalid tar octal field"));
    }
    u64::from_str_radix(text, 8).map_err(|_| invalid("tar number overflow"))
}
fn number(bytes: &[u8], at: usize, width: usize) -> Result<u64> {
    let field = bytes
        .get(
            at..at
                .checked_add(width)
                .ok_or_else(|| invalid("header offset overflow"))?,
        )
        .ok_or_else(|| invalid("truncated executable header"))?;
    Ok(field.iter().enumerate().fold(0u64, |value, (index, byte)| {
        value | (u64::from(*byte) << (index * 8))
    }))
}
fn validate_kernel(bytes: &[u8]) -> Result<()> {
    if bytes.len() < 0x290
        || bytes.get(0x1fe..0x200) != Some(b"\x55\xaa")
        || bytes.get(0x202..0x206) != Some(b"HdrS")
        || number(bytes, 0x206, 2)? < 0x020c
        || bytes[0x211] & 1 == 0
        || number(bytes, 0x236, 2)? & 1 == 0
        || !(1..=64).contains(&bytes[0x1f1])
        || (usize::from(bytes[0x1f1]) + 1) * 512 >= bytes.len()
    {
        return Err(invalid(
            "expected an x86_64 bzImage boot header and payload",
        ));
    }
    let payload = bytes.len() - (usize::from(bytes[0x1f1]) + 1) * 512;
    let paragraphs = number(bytes, 0x1f4, 4)?;
    if paragraphs == 0
        || paragraphs
            .checked_mul(16)
            .is_none_or(|size| size > payload as u64)
    {
        return Err(invalid("truncated x86 protected-mode kernel payload"));
    }
    Ok(())
}
fn validate_elf(bytes: &[u8]) -> Result<()> {
    if bytes.get(..7) != Some(b"\x7fELF\x02\x01\x01")
        || !matches!(number(bytes, 16, 2)?, 2 | 3)
        || number(bytes, 18, 2)? != 62
        || number(bytes, 20, 4)? != 1
        || number(bytes, 52, 2)? != 64
        || number(bytes, 54, 2)? != 56
    {
        return Err(invalid("expected an x86_64 ELF64 little-endian executable"));
    }
    let offset = number(bytes, 32, 8)?;
    let count = number(bytes, 56, 2)?;
    if offset < 64
        || !(1..=1024).contains(&count)
        || offset
            .checked_add(count * 56)
            .is_none_or(|end| end > bytes.len() as u64)
    {
        return Err(invalid("ELF program header table outside file"));
    }
    let entry = number(bytes, 24, 8)?;
    let mut executable_entry = false;
    for index in 0..count {
        let at = (offset + index * 56) as usize;
        let kind = number(bytes, at, 4)?;
        let flags = number(bytes, at + 4, 4)?;
        let start = number(bytes, at + 8, 8)?;
        let address = number(bytes, at + 16, 8)?;
        let size = number(bytes, at + 32, 8)?;
        let memory = number(bytes, at + 40, 8)?;
        let end = start
            .checked_add(size)
            .filter(|end| *end <= bytes.len() as u64)
            .ok_or_else(|| invalid("ELF segment outside file"))?;
        let virtual_end = address
            .checked_add(memory)
            .ok_or_else(|| invalid("ELF virtual segment overflow"))?;
        if kind == 3 {
            return Err(invalid("guest helper has PT_INTERP"));
        }
        if kind == 1 {
            if memory < size {
                return Err(invalid("ELF load segment is smaller than its file bytes"));
            }
            executable_entry |=
                size > 0 && flags & 1 != 0 && entry >= address && entry < virtual_end;
        }
        if kind == 2 {
            if size == 0 || size % 16 != 0 {
                return Err(invalid("invalid ELF dynamic segment"));
            }
            let mut terminated = false;
            for at in (start as usize..end as usize).step_by(16) {
                match number(bytes, at, 8)? {
                    0 => {
                        terminated = true;
                        break;
                    }
                    1 => return Err(invalid("guest helper has a dynamic DT_NEEDED dependency")),
                    _ => (),
                }
            }
            if !terminated {
                return Err(invalid("unterminated ELF dynamic segment"));
            }
        }
    }
    if !executable_entry {
        return Err(invalid(
            "ELF entry point is outside an executable load segment",
        ));
    }
    Ok(())
}
