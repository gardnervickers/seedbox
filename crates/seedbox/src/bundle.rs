//! Transfer one retained attempt without preparation or executing its contents.
use crate::{
    execution::CancellationToken,
    orchestration::{inspect_bytes, Run, RunRecord},
    store::{ContentDigest, ContentStore},
    Error, Result,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::Path,
};

pub const DEFAULT_MAX_BYTES: u64 = 32 * 1024 * 1024 * 1024;
const MAX_OBJECTS: usize = 8192;
const MAX_METADATA: u64 = 8 * 1024 * 1024;
const MAX_LOG_NAME: usize = 4096;

#[derive(Clone, Debug)]
pub struct Options {
    pub max_bytes: u64,
    pub cancellation: CancellationToken,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_BYTES,
            cancellation: CancellationToken::default(),
        }
    }
}
impl Options {
    fn check(&self) -> Result<()> {
        if self.max_bytes == 0 {
            return Err(bad("max-bytes must be positive"));
        }
        if self.cancellation.is_cancelled() {
            return Err(bad("transfer cancelled"));
        }
        Ok(())
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u32,
    manifest: Object,
    #[serde(deserialize_with = "read_objects")]
    objects: Vec<Object>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Object {
    digest: ContentDigest,
    bytes: u64,
}
fn read_objects<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<Object>, D::Error> {
    struct Objects;
    impl<'de> serde::de::Visitor<'de> for Objects {
        type Value = Vec<Object>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("at most 8192 object descriptors")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut objects = Vec::new();
            while let Some(object) = seq.next_element()? {
                if objects.len() == MAX_OBJECTS {
                    return Err(serde::de::Error::custom("more than 8192 objects"));
                }
                objects.push(object);
            }
            Ok(objects)
        }
    }
    deserializer.deserialize_seq(Objects)
}
fn bad(message: impl Into<String>) -> Error {
    Error::Integrity(format!("bundle: {}", message.into()))
}
fn closure(record: &RunRecord) -> BTreeSet<ContentDigest> {
    std::iter::once(&record.run_id)
        .chain(record.identity.inputs.digests())
        .chain(record.logs.values())
        .cloned()
        .collect()
}
fn inventory(record: &RunRecord, objects: &[Object], options: &Options) -> Result<()> {
    options.check()?;
    if objects.len() > MAX_OBJECTS {
        return Err(bad("more than 8192 objects"));
    }
    if objects
        .windows(2)
        .any(|pair| pair[0].digest >= pair[1].digest)
    {
        return Err(bad("object inventory must be unique and sorted"));
    }
    let mut sizes = BTreeMap::new();
    let mut bytes = 0u64;
    for object in objects {
        if sizes.insert(object.digest.clone(), object.bytes).is_some() {
            return Err(bad("duplicate object"));
        }
        bytes = bytes
            .checked_add(object.bytes)
            .ok_or_else(|| bad("object byte count overflow"))?;
        if bytes > options.max_bytes {
            return Err(bad("object payload exceeds max-bytes"));
        }
    }
    if sizes.keys().cloned().collect::<BTreeSet<_>>() != closure(record) {
        return Err(bad("object inventory does not match retained closure"));
    }
    if record.logs.len() > MAX_OBJECTS {
        return Err(bad("more than 8192 materialized log paths"));
    }
    let mut log_bytes = 0u64;
    let mut names = BTreeSet::new();
    for (name, digest) in &record.logs {
        validate_log_name(name)?;
        log_bytes = log_bytes
            .checked_add(sizes[digest])
            .ok_or_else(|| bad("materialized log byte count overflow"))?;
        if log_bytes > options.max_bytes {
            return Err(bad("materialized logs exceed max-bytes"));
        }
        names.insert(name.as_str());
    }
    for name in &names {
        let mut prefix = String::new();
        let components: Vec<_> = name.split('/').collect();
        for part in &components[..components.len() - 1] {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            if names.contains(prefix.as_str()) {
                return Err(bad("log file/directory prefix collision"));
            }
        }
    }
    Ok(())
}
fn validate_log_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > MAX_LOG_NAME
        || name.contains('\\')
        || name.contains('\0')
        || name
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(bad(format!("unsafe retained log name {name:?}")));
    }
    Ok(())
}
fn bounded_file(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_METADATA + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_METADATA {
        return Err(bad("manifest exceeds 8 MiB"));
    }
    Ok(bytes)
}
fn header(name: &str, size: u64) -> Result<[u8; 512]> {
    let mut h = tar::Header::new_ustar();
    h.set_path(name)?;
    h.set_size(size);
    h.set_mode(0o600);
    h.set_mtime(0);
    h.set_cksum();
    Ok(*h.as_bytes())
}
fn padding(size: u64) -> usize {
    ((512 - size % 512) % 512) as usize
}
fn append_bytes(output: &mut impl Write, name: &str, bytes: &[u8]) -> Result<()> {
    output.write_all(&header(name, bytes.len() as u64)?)?;
    output.write_all(bytes)?;
    output.write_all(&[0u8; 512][..padding(bytes.len() as u64)])?;
    Ok(())
}
struct CheckedWriter<'a, W> {
    inner: &'a mut W,
    options: &'a Options,
    count: u64,
}
impl<W: Write> Write for CheckedWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.options.cancellation.is_cancelled() {
            return Err(std::io::Error::other("transfer cancelled"));
        }
        let count = self.inner.write(bytes)?;
        self.count = self
            .count
            .checked_add(count as u64)
            .ok_or_else(|| std::io::Error::other("byte count overflow"))?;
        Ok(count)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}
/// Export only declared retained bytes. Existing destinations are never replaced.
pub fn export(
    run: &Path,
    store: &ContentStore,
    destination: &Path,
    options: &Options,
) -> Result<RunRecord> {
    let bytes = bounded_file(&run.join("manifest.json"))?;
    let record = inspect_bytes(&bytes)?;
    let objects = closure(&record)
        .into_iter()
        .map(|digest| {
            Ok(Object {
                bytes: store.size(&digest)?,
                digest,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    inventory(&record, &objects, options)?;
    let envelope = Envelope {
        version: 1,
        manifest: Object {
            digest: ContentDigest::of(&bytes),
            bytes: bytes.len() as u64,
        },
        objects,
    };
    let metadata = serde_json::to_vec(&envelope)?;
    if metadata.len() as u64 > MAX_METADATA {
        return Err(bad("envelope exceeds 8 MiB"));
    }
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    append_bytes(&mut temp, "bundle.json", &metadata)?;
    append_bytes(&mut temp, "manifest.json", &bytes)?;
    for object in &envelope.objects {
        options.check()?;
        temp.write_all(&header(
            &format!("objects/sha256/{}", object.digest.hex()),
            object.bytes,
        )?)?;
        let mut writer = CheckedWriter {
            inner: &mut temp,
            options,
            count: 0,
        };
        store.copy_to(&object.digest, &mut writer)?;
        if writer.count != object.bytes {
            return Err(bad("object length changed during export"));
        }
        temp.write_all(&[0u8; 512][..padding(object.bytes)])?;
    }
    temp.write_all(&[0u8; 1024])?;
    options.check()?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(destination).map_err(|e| e.error)?;
    Ok(record)
}
// Read the fixed USTAR container directly: extension entries are never processed,
// and no archive path is passed to filesystem extraction.
fn read_header(input: &mut impl Read, expected: &str) -> Result<u64> {
    let mut bytes = [0u8; 512];
    input.read_exact(&mut bytes)?;
    let h = tar::Header::from_byte_slice(&bytes);
    let checksum: u32 = bytes
        .iter()
        .enumerate()
        .map(|(i, b)| {
            if (148..156).contains(&i) {
                32
            } else {
                u32::from(*b)
            }
        })
        .sum();
    if h.cksum()? != checksum
        || &bytes[257..263] != b"ustar\0"
        || &bytes[263..265] != b"00"
        || !matches!(bytes[156], 0 | b'0')
        || h.path_bytes().as_ref() != expected.as_bytes()
    {
        return Err(bad(format!("expected regular USTAR entry {expected}")));
    }
    Ok(h.size()?)
}
fn read_padding(input: &mut impl Read, size: u64) -> Result<()> {
    let mut bytes = [0u8; 512];
    let count = padding(size);
    input.read_exact(&mut bytes[..count])?;
    if bytes[..count].iter().any(|b| *b != 0) {
        return Err(bad("nonzero entry padding"));
    }
    Ok(())
}
fn metadata(input: &mut impl Read, name: &str) -> Result<Vec<u8>> {
    let size = read_header(input, name)?;
    if size > MAX_METADATA {
        return Err(bad(format!("{name} exceeds 8 MiB")));
    }
    let mut bytes = vec![0; size as usize];
    input.read_exact(&mut bytes)?;
    read_padding(input, size)?;
    Ok(bytes)
}
fn copy_hash(
    input: &mut impl Read,
    output: &mut impl Write,
    size: u64,
    options: &Options,
) -> Result<ContentDigest> {
    let mut remaining = size;
    let mut buffer = [0u8; 64 * 1024];
    let mut hash = Sha256::new();
    while remaining != 0 {
        options.check()?;
        let count = remaining.min(buffer.len() as u64) as usize;
        input.read_exact(&mut buffer[..count])?;
        hash.update(&buffer[..count]);
        output.write_all(&buffer[..count])?;
        remaining -= count as u64;
    }
    options.check()?;
    ContentDigest::try_from(format!("{:x}", hash.finalize()))
}
/// Import without preparation or execution. Publish the complete attempt last.
pub fn import(bundle: &Path, state: &Path, options: &Options) -> Result<Run> {
    #[cfg(unix)]
    {
        import_unix(bundle, state, options)
    }
    #[cfg(not(unix))]
    {
        let _ = (bundle, state, options);
        Err(Error::Setup("bundle import requires Linux or macOS".into()))
    }
}
#[cfg(unix)]
fn import_unix(bundle: &Path, state: &Path, options: &Options) -> Result<Run> {
    options.check()?;
    let mut input = fs::File::open(bundle)?;
    let envelope: Envelope = serde_json::from_slice(&metadata(&mut input, "bundle.json")?)?;
    if envelope.version != 1 {
        return Err(bad("unsupported bundle format"));
    }
    let bytes = metadata(&mut input, "manifest.json")?;
    if bytes.len() as u64 != envelope.manifest.bytes
        || ContentDigest::of(&bytes) != envelope.manifest.digest
    {
        return Err(bad("manifest hash/length mismatch"));
    }
    let record = inspect_bytes(&bytes)?;
    inventory(&record, &envelope.objects, options)?;
    let root = safe::Dir::state(state)?;
    let objects = root.child("objects")?.child("sha256")?;
    let runs = root.child("runs")?;
    let staging = safe::Stage::new(&root)?;
    let staged_objects = staging.directory.child("objects")?;
    for object in &envelope.objects {
        let name = format!("objects/sha256/{}", object.digest.hex());
        if read_header(&mut input, &name)? != object.bytes {
            return Err(bad("object header length mismatch"));
        }
        let mut file = staged_objects.create(object.digest.hex())?;
        if copy_hash(&mut input, &mut file, object.bytes, options)? != object.digest {
            return Err(bad(format!(
                "object hash mismatch: {}",
                object.digest.hex()
            )));
        }
        file.sync_all()?;
        read_padding(&mut input, object.bytes)?;
    }
    let mut end = [0u8; 1024];
    input.read_exact(&mut end)?;
    if end.iter().any(|b| *b != 0) || input.read(&mut [0u8; 1])? != 0 {
        return Err(bad("unexpected trailing entries/data"));
    }
    let staged_run = staging.directory.child("run")?;
    let mut manifest = staged_run.create("manifest.json")?;
    manifest.write_all(&bytes)?;
    manifest.sync_all()?;
    let output = staged_run.child("output")?;
    let lengths: BTreeMap<_, _> = envelope
        .objects
        .iter()
        .map(|o| (&o.digest, o.bytes))
        .collect();
    for (name, digest) in &record.logs {
        options.check()?;
        let mut parts = name.split('/').peekable();
        let mut parent = output.clone()?;
        while let Some(part) = parts.next() {
            if parts.peek().is_some() {
                parent = parent.child(part)?;
            } else {
                let mut source = staged_objects.file(digest.hex())?;
                let mut target = parent.create(part)?;
                if copy_hash(&mut source, &mut target, lengths[digest], options)? != *digest {
                    return Err(bad("staged log changed"));
                }
                target.sync_all()?;
            }
        }
    }
    // Existing objects are checked before committing any new ones.
    for object in &envelope.objects {
        if let Some(mut existing) = objects.existing_file(object.digest.hex())? {
            if existing.metadata()?.len() != object.bytes
                || copy_hash(&mut existing, &mut std::io::sink(), object.bytes, options)?
                    != object.digest
            {
                return Err(bad("existing object is corrupt; not replaced"));
            }
        }
    }
    for object in &envelope.objects {
        options.check()?;
        objects.link_from(&staged_objects, object, options)?;
    }
    objects.sync()?;
    staged_run.sync()?;
    options.check()?;
    let name = format!(
        "{}-{}",
        &record.run_id.hex()[..16],
        safe::unique_name("imported").trim_start_matches('.')
    );
    runs.publish(&staging.directory, "run", &name)?;
    runs.sync()?;
    Ok(Run {
        directory: std::path::absolute(state)?.join("runs").join(name),
        record,
    })
}

#[cfg(unix)]
mod safe {
    use super::*;
    use std::{
        ffi::{CStr, CString},
        fs::File,
        os::{
            fd::{AsRawFd, FromRawFd, IntoRawFd},
            unix::fs::OpenOptionsExt,
        },
        sync::atomic::{AtomicU64, Ordering},
    };
    pub(super) struct Dir(File);
    fn c(name: &str) -> Result<CString> {
        CString::new(name).map_err(|_| bad("NUL in path"))
    }
    fn io(result: i32) -> Result<i32> {
        if result < 0 {
            Err(std::io::Error::last_os_error().into())
        } else {
            Ok(result)
        }
    }
    impl Dir {
        pub(super) fn state(path: &Path) -> Result<Self> {
            // Strip trailing separators/dot so they cannot make O_NOFOLLOW
            // treat a symlinked root as an intermediate component.
            let normalized: std::path::PathBuf = path.components().collect();
            let path = normalized.as_path();
            // Preserve root symlink evidence. No canonicalization/StateArg::open.
            match fs::create_dir_all(
                path.parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new(".")),
            ) {
                Ok(()) => {}
                Err(e) => return Err(e.into()),
            }
            match fs::create_dir(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
            fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(path)
                .map(Self)
                .map_err(|e| {
                    Error::Setup(format!(
                        "--state {}: {e}; use a real directory without symlinks",
                        path.display()
                    ))
                })
        }
        pub(super) fn clone(&self) -> Result<Self> {
            Ok(Self(self.0.try_clone()?))
        }
        pub(super) fn child(&self, name: &str) -> Result<Self> {
            let name = c(name)?;
            let rc = unsafe { libc::mkdirat(self.0.as_raw_fd(), name.as_ptr(), 0o700) };
            if rc < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(std::io::Error::last_os_error().into());
            }
            let fd = io(unsafe {
                libc::openat(
                    self.0.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            })?;
            Ok(Self(unsafe { File::from_raw_fd(fd) }))
        }
        pub(super) fn create(&self, name: &str) -> Result<File> {
            let name = c(name)?;
            let fd = io(unsafe {
                libc::openat(
                    self.0.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o600,
                )
            })?;
            Ok(unsafe { File::from_raw_fd(fd) })
        }
        pub(super) fn existing_file(&self, name: &str) -> Result<Option<File>> {
            let name = c(name)?;
            let fd = unsafe {
                libc::openat(
                    self.0.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::NotFound {
                    return Ok(None);
                }
                return Err(e.into());
            }
            let file = unsafe { File::from_raw_fd(fd) };
            if !file.metadata()?.is_file() {
                return Err(bad("existing object is not a regular file"));
            }
            Ok(Some(file))
        }
        pub(super) fn file(&self, name: &str) -> Result<File> {
            self.existing_file(name)?
                .ok_or_else(|| bad("missing staged object"))
        }
        pub(super) fn link_from(
            &self,
            source: &Self,
            object: &Object,
            options: &Options,
        ) -> Result<()> {
            let name = c(object.digest.hex())?;
            let rc = unsafe {
                libc::linkat(
                    source.0.as_raw_fd(),
                    name.as_ptr(),
                    self.0.as_raw_fd(),
                    name.as_ptr(),
                    0,
                )
            };
            if rc < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(std::io::Error::last_os_error().into());
            }
            // Verify the actual published/existing entry too, including race winners.
            let mut file = self.file(object.digest.hex())?;
            if file.metadata()?.len() != object.bytes
                || copy_hash(&mut file, &mut std::io::sink(), object.bytes, options)?
                    != object.digest
            {
                return Err(bad("existing object is corrupt; not replaced"));
            }
            Ok(())
        }
        pub(super) fn sync(&self) -> Result<()> {
            self.0.sync_all()?;
            Ok(())
        }
        pub(super) fn publish(&self, source: &Self, old: &str, new: &str) -> Result<()> {
            let old = c(old)?;
            let new = c(new)?;
            #[cfg(target_os = "linux")]
            let rc = unsafe {
                libc::syscall(
                    libc::SYS_renameat2,
                    source.0.as_raw_fd(),
                    old.as_ptr(),
                    self.0.as_raw_fd(),
                    new.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            } as i32;
            #[cfg(target_os = "macos")]
            let rc = unsafe {
                libc::renameatx_np(
                    source.0.as_raw_fd(),
                    old.as_ptr(),
                    self.0.as_raw_fd(),
                    new.as_ptr(),
                    libc::RENAME_EXCL,
                )
            };
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            let rc = -1;
            io(rc)?;
            Ok(())
        }
    }
    pub(super) fn unique_name(prefix: &str) -> String {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!(
            ".{prefix}-{}-{time:x}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }
    pub(super) struct Stage {
        root: Dir,
        name: String,
        pub(super) directory: Dir,
    }
    impl Stage {
        pub(super) fn new(root: &Dir) -> Result<Self> {
            let name = unique_name("bundle");
            let raw = c(&name)?;
            io(unsafe { libc::mkdirat(root.0.as_raw_fd(), raw.as_ptr(), 0o700) })?;
            let directory = root.child(&name)?;
            Ok(Self {
                root: root.clone()?,
                name,
                directory,
            })
        }
    }
    fn clean(dir: &Dir) -> Result<()> {
        struct Listing(*mut libc::DIR);
        impl Drop for Listing {
            fn drop(&mut self) {
                unsafe {
                    libc::closedir(self.0);
                }
            }
        }
        let fd = dir.0.try_clone()?.into_raw_fd();
        let raw = unsafe { libc::fdopendir(fd) };
        if raw.is_null() {
            unsafe {
                libc::close(fd);
            }
            return Err(std::io::Error::last_os_error().into());
        }
        let listing = Listing(raw);
        unsafe {
            libc::rewinddir(listing.0);
        }
        loop {
            #[cfg(target_os = "macos")]
            unsafe {
                *libc::__error() = 0;
            }
            #[cfg(target_os = "linux")]
            unsafe {
                *libc::__errno_location() = 0;
            }
            let entry = unsafe { libc::readdir(listing.0) };
            if entry.is_null() {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(0) {
                    return Err(error.into());
                }
                break;
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if matches!(name.to_bytes(), b"." | b"..") {
                continue;
            }
            let fd = unsafe {
                libc::openat(
                    dir.0.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd >= 0 {
                let child = Dir(unsafe { File::from_raw_fd(fd) });
                clean(&child)?;
                io(unsafe {
                    libc::unlinkat(dir.0.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR)
                })?;
            } else {
                io(unsafe { libc::unlinkat(dir.0.as_raw_fd(), name.as_ptr(), 0) })?;
            }
        }
        Ok(())
    }
    impl Drop for Stage {
        fn drop(&mut self) {
            let result = clean(&self.directory).and_then(|()| {
                let name = c(&self.name)?;
                io(unsafe {
                    libc::unlinkat(self.root.0.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR)
                })
                .map(|_| ())
            });
            if let Err(e) = result {
                eprintln!("cleanup failed: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn streaming_read_and_cancellation_are_bounded() {
        struct Source {
            remaining: u64,
            token: Option<CancellationToken>,
        }
        impl Read for Source {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                assert!(bytes.len() <= 64 * 1024);
                let n = self.remaining.min(bytes.len() as u64) as usize;
                bytes[..n].fill(7);
                self.remaining -= n as u64;
                if let Some(token) = self.token.take() {
                    token.cancel();
                }
                Ok(n)
            }
        }
        let options = Options::default();
        let size = 2 * 1024 * 1024;
        let digest = copy_hash(
            &mut Source {
                remaining: size,
                token: None,
            },
            &mut std::io::sink(),
            size,
            &options,
        )
        .unwrap();
        assert_eq!(digest, ContentDigest::of(&vec![7; size as usize]));
        let options = Options::default();
        let mut output = Vec::new();
        assert!(copy_hash(
            &mut Source {
                remaining: size,
                token: Some(options.cancellation.clone())
            },
            &mut output,
            size,
            &options
        )
        .is_err());
        assert_eq!(output.len(), 64 * 1024);
    }
}
