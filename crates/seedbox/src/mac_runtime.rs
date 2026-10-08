//! Build-time Linux preparation payload and an exclusively owned Lima cache.
//! No compiler is launched at runtime. OCI metadata/layers are still handled
//! by the existing Skopeo/umoci preparer on a Linux filesystem.
use crate::{
    config::{ImageSource, Simulation},
    execution::{CancellationToken, PreparationBackend, PreparedInputs},
    oci::{FrozenContext, OciTools},
    preparation::{OciPreparer, PreparationContext, WorkerJob},
    store::{ContentDigest, ContentStore},
    Error, Result,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
    },
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

include!(concat!(env!("OUT_DIR"), "/mac-runtime.rs"));
const INSTANCE: &str = "seedbox-prep";
const FILES: [&str; 8] = [
    "Image",
    "compose-init",
    "busybox",
    "mke2fs",
    "linux-helper",
    "store.nar",
    "kernel.config",
    "counter-audit.txt",
];
const OWNER: &str = "seedbox-owned-linux-preparation-v1\n";

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u32,
    pub files: BTreeMap<String, ContentDigest>,
    pub tools: OciTools,
}
#[derive(Clone, Debug)]
pub struct Runtime {
    pub root: PathBuf,
    pub digest: ContentDigest,
    pub manifest: Manifest,
    pub lima: PathBuf,
    pub home: PathBuf,
}

impl Runtime {
    pub fn embedded(cache: &Path, lima: PathBuf, home: PathBuf) -> Result<Self> {
        let bytes=MAC_RUNTIME.ok_or_else(||Error::Setup("seedbox was built without the Mac preparation bundle; build the CLI with tools/mac-runtime/build-cli.sh".into()))?;
        Self::install(bytes, cache, lima, home)
    }
    pub fn install(bytes: &[u8], cache: &Path, lima: PathBuf, home: PathBuf) -> Result<Self> {
        let digest = ContentDigest::of(bytes);
        fs::create_dir_all(cache)?;
        let root = cache.join(digest.hex());
        if !root.exists() {
            let temp = tempfile::tempdir_in(cache)?;
            let reader = flate2::read::GzDecoder::new(bytes);
            let mut archive = tar::Archive::new(reader);
            let mut names = BTreeSet::new();
            let mut total = 0u64;
            for entry in archive.entries()? {
                let mut entry = entry?;
                let path = entry.path()?.into_owned();
                let name = path
                    .to_str()
                    .ok_or_else(|| Error::Integrity("runtime package path is not UTF-8".into()))?;
                if !entry.header().entry_type().is_file()
                    || !(name == "runtime.json" || FILES.contains(&name))
                    || !names.insert(name.to_owned())
                {
                    return Err(Error::Integrity(
                        "runtime package contains an unexpected or duplicate file".into(),
                    ));
                }
                total = total
                    .checked_add(entry.size())
                    .filter(|total| *total <= 2 << 30)
                    .ok_or_else(|| Error::Integrity("runtime package exceeds 2 GiB".into()))?;
                let destination = temp.path().join(name);
                entry.unpack(&destination)?;
                fs::set_permissions(destination, fs::Permissions::from_mode(0o600))?;
            }
            // Validate before publishing; another installer may have won.
            validate_package(temp.path())?;
            match fs::rename(temp.path(), &root) {
                Ok(()) => {}
                Err(e) if root.is_dir() => {
                    let _ = e;
                }
                Err(e) => return Err(e.into()),
            }
        }
        let manifest = validate_package(&root)?;
        Ok(Self {
            root,
            digest,
            manifest,
            lima,
            home,
        })
    }
    pub fn input(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
    fn command(&self) -> Command {
        let mut command = Command::new(&self.lima);
        command.env("LIMA_HOME", &self.home).arg("--tty=false");
        command
    }
    fn shell(&self, args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>) -> Command {
        let mut command = self.command();
        command
            .args(["shell", "--workdir", "/tmp", INSTANCE])
            .args(args);
        command
    }
    fn run(
        &self,
        context: &PreparationContext,
        command: &mut Command,
        phase: &str,
        cleanup: bool,
    ) -> Result<()> {
        command.current_dir(&self.root);
        context.run(command, phase, None, false, cleanup)
    }
    fn owner(&self) -> Result<fs::File> {
        if !self.home.exists() {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new().mode(0o700).create(&self.home)?;
            use std::io::Write;
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(self.home.join("seedbox-owner"))?
                .write_all(OWNER.as_bytes())?;
        }
        let metadata = fs::symlink_metadata(&self.home)?;
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || fs::read_to_string(self.home.join("seedbox-owner"))
                .ok()
                .as_deref()
                != Some(OWNER)
        {
            return Err(Error::Setup(format!("preparation cache {} is not a private Seedbox-owned VM directory; choose a clean cache",self.home.display())));
        }
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.home.join("owner.lock"))
            .map_err(Error::from)
    }
    fn start<'a>(&'a self, context: &'a PreparationContext) -> Result<OwnedVm<'a>> {
        let lock = self.owner()?;
        loop {
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if !matches!(
                error.raw_os_error(),
                Some(libc::EWOULDBLOCK) | Some(libc::EINTR)
            ) {
                return Err(error.into());
            }
            context.check()?;
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut owner = OwnedVm {
            runtime: self,
            context,
            lock,
            started: true,
        };
        let mut start = self.command();
        start.arg("start");
        if self.home.join(INSTANCE).join("lima.yaml").is_file() {
            start.arg(INSTANCE);
        } else {
            let config = self.home.join("seedbox-lima.yaml");
            fs::write(
                &config,
                include_str!("../../../tools/mac-runtime/lima.yaml"),
            )?;
            start.arg("--name").arg(INSTANCE).arg(config);
        }
        if let Err(error) = self.run(context, &mut start, "mac-vm-start", false) {
            owner.stop()?;
            return Err(error);
        }
        Ok(owner)
    }
    pub fn prepare(
        &self,
        preparer: &OciPreparer,
        simulation: &Simulation,
        store: &ContentStore,
        context: &PreparationContext,
    ) -> Result<PreparedInputs> {
        self.validate_inputs(preparer)?;
        context.check()?;
        let mut owner = self.start(context)?;
        let result = (|| {
            // The owned minimal VM lacks the policy normally supplied with
            // Podman. This standard unsigned-image policy is confined to it;
            // TLS and retained image hashes are still checked by OCI tools.
            self.run(context,&mut self.shell(["sudo","-n","sh","-c","mkdir -p /etc/containers; printf '%s\\n' '{\"default\":[{\"type\":\"insecureAcceptAnything\"}]}' > /etc/containers/policy.json"]),"mac-container-policy",false)?;
            let package = format!("/tmp/seedbox-runtime-{}", self.digest.hex());
            self.run(
                context,
                &mut self.shell(["mkdir", "-p", "--", &package]),
                "mac-package-directory",
                false,
            )?;
            let mut copy = self.command();
            copy.args(["copy", "--recursive"])
                .arg(self.root.join("."))
                .arg(format!("{INSTANCE}:{package}"));
            self.run(context, &mut copy, "mac-package-copy", false)?;
            let nar = format!("{package}/store.nar");
            self.run(
                context,
                &mut self.shell([
                    "sh",
                    "-c",
                    "\"$HOME/.nix-profile/bin/nix-store\" --import < \"$1\"",
                    "sh",
                    &nar,
                ]),
                "mac-runtime-import",
                false,
            )?;
            self.run(
                context,
                &mut self.shell(["chmod", "500", &format!("{package}/linux-helper")]),
                "mac-helper-mode",
                false,
            )?;
            let stage = tempfile::Builder::new()
                .prefix("seedbox-mac-job-")
                .tempdir()?;
            let name = stage
                .path()
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| Error::Execution("invalid preparation stage name".into()))?;
            let remote = format!("/tmp/{name}");
            // Build storage belongs to this job, not to a previously stopped
            // VM's overlay mount cache. VFS also avoids host-kernel overlay
            // requirements. The existing job cleanup removes it on every exit.
            fs::write(
                stage.path().join("storage.conf"),
                format!("[storage]\ndriver = \"vfs\"\nrunroot = \"{remote}/podman-run\"\ngraphroot = \"{remote}/podman-root\"\n"),
            )?;
            let mut job_simulation = simulation.clone();
            for (i, service) in job_simulation.services.values_mut().enumerate() {
                if let ImageSource::Build {
                    context: source,
                    dockerfile,
                } = &mut service.source
                {
                    let folder = stage.path().join(format!("context-{i}"));
                    let relative = dockerfile
                        .strip_prefix(&*source)
                        .map_err(|_| Error::Integrity("Dockerfile escaped its context".into()))?
                        .to_owned();
                    FrozenContext::capture(source, &folder, store)?;
                    *source = PathBuf::from(format!("{remote}/context-{i}/build-context"));
                    *dockerfile = source.join(relative);
                }
            }
            for (i, path) in job_simulation.configs.values_mut().enumerate() {
                let name = format!("config-{i}");
                fs::copy(&*path, stage.path().join(&name))?;
                *path = PathBuf::from(format!("{remote}/{name}"));
            }
            for (name, path) in [
                ("engine", &preparer.engine),
                ("Image", &preparer.kernel),
                ("compose-init", &preparer.init),
                ("busybox", &preparer.busybox),
            ] {
                fs::copy(path, stage.path().join(name))?;
            }
            if let Some(mke2fs) = &preparer.mke2fs {
                fs::copy(mke2fs, stage.path().join("mke2fs"))?;
            }
            let mut tools = self.manifest.tools.clone();
            tools.build_epoch = preparer.tools.build_epoch;
            let job = WorkerJob {
                simulation: job_simulation,
                tools,
                kernel: PathBuf::from(format!("{remote}/Image")),
                engine: PathBuf::from(format!("{remote}/engine")),
                init: PathBuf::from(format!("{remote}/compose-init")),
                busybox: PathBuf::from(format!("{remote}/busybox")),
                mke2fs: preparer
                    .mke2fs
                    .as_ref()
                    .map(|_| PathBuf::from(format!("{remote}/mke2fs"))),
                profile: preparer.profile.clone(),
                store: PathBuf::from(format!("{remote}/store")),
                export: Some(PathBuf::from(format!("{remote}/export"))),
            };
            fs::write(
                stage.path().join("job.json"),
                serde_json::to_vec_pretty(&job)?,
            )?;
            self.run(
                context,
                &mut self.shell(["mkdir", "--", &remote]),
                "mac-job-directory",
                false,
            )?;
            let operation = (|| {
                let mut copy = self.command();
                copy.args(["copy", "--recursive"])
                    .arg(stage.path().join("."))
                    .arg(format!("{INSTANCE}:{remote}"));
                self.run(context, &mut copy, "mac-input-copy", false)?;
                self.run(
                    context,
                    &mut self.shell([
                        "sudo",
                        "-n",
                        "env",
                        &format!("CONTAINERS_STORAGE_CONF={remote}/storage.conf"),
                        &format!("{package}/linux-helper"),
                        "__engine",
                        "arm64-preparation-v1",
                        &format!("{remote}/job.json"),
                    ]),
                    "mac-linux-prepare",
                    false,
                )?;
                let destination = stage.path().join("received");
                fs::create_dir(&destination)?;
                let mut copy = self.command();
                copy.args(["copy", "--recursive"])
                    .arg(format!("{INSTANCE}:{remote}/export/."))
                    .arg(&destination);
                self.run(context, &mut copy, "mac-output-copy", false)?;
                let prepared: PreparedInputs = serde_json::from_reader(
                    fs::File::open(destination.join("prepared.json"))?.take(8 << 20),
                )?;
                if prepared.profile != preparer.profile
                    || prepared.kernel != crate::admission::file_digest(&preparer.kernel)?
                {
                    return Err(Error::Integrity(
                        "Linux preparation changed runtime identity".into(),
                    ));
                }
                let digests: BTreeSet<_> = prepared.digests().collect();
                for digest in digests {
                    if store.put_file(&destination.join("objects").join(digest.hex()))? != *digest {
                        return Err(Error::Integrity(
                            "Linux preparation transfer hash mismatch".into(),
                        ));
                    }
                }
                Ok(prepared)
            })();
            let cleanup = self.run(
                context,
                &mut self.shell(["sudo", "-n", "rm", "-rf", "--", &remote]),
                "mac-job-cleanup",
                true,
            );
            cleanup?;
            operation
        })();
        owner.stop()?;
        result
    }
    pub fn validate_inputs(&self, preparer: &OciPreparer) -> Result<()> {
        validate_package(&self.root)?;
        for path in [&preparer.init, &preparer.busybox] {
            crate::guest::Architecture::Arm64.validate_elf(path)?;
        }
        if let Some(path) = &preparer.mke2fs {
            crate::guest::Architecture::Arm64.validate_elf(path)?;
        }
        if crate::admission::file_digest(&preparer.engine)? != preparer.profile.engine {
            return Err(Error::Integrity(
                "Mac executable changed after profile construction".into(),
            ));
        }
        if preparer.tools.docker != Path::new("podman")
            && preparer.tools.docker != self.manifest.tools.docker
        {
            return Err(Error::Setup(
                "--builder: Mac preparation uses the packaged Linux Podman; omit --builder".into(),
            ));
        }
        Ok(())
    }
}

fn validate_package(root: &Path) -> Result<Manifest> {
    if !fs::symlink_metadata(root)?.is_dir()
        || !fs::symlink_metadata(root.join("runtime.json"))?.is_file()
    {
        return Err(Error::Integrity(
            "runtime package directory or manifest is a symlink".into(),
        ));
    }
    let manifest: Manifest =
        serde_json::from_reader(fs::File::open(root.join("runtime.json"))?.take(1 << 20))?;
    if manifest.version != 1
        || manifest
            .files
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>()
            != FILES.into_iter().collect()
    {
        return Err(Error::Integrity(
            "unsupported Mac runtime package manifest".into(),
        ));
    }
    for (name, digest) in &manifest.files {
        let path = root.join(name);
        if !fs::symlink_metadata(&path)?.is_file() || package_digest(&path)? != *digest {
            return Err(Error::Integrity(format!(
                "Mac runtime package {name} hash mismatch"
            )));
        }
    }
    Ok(manifest)
}

// The runtime NAR is larger than a kernel/VMM input. Its size is bounded by
// the package contract, and hashing remains streaming on installation/reuse.
fn package_digest(path: &Path) -> Result<ContentDigest> {
    use sha2::{Digest, Sha256};
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut total = 0u64;
    let mut bytes = [0u8; 65536];
    loop {
        let count = file.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > 2 << 30 {
            return Err(Error::Integrity(
                "runtime package file exceeds 2 GiB".into(),
            ));
        }
        hash.update(&bytes[..count]);
    }
    ContentDigest::try_from(format!("{:x}", hash.finalize()))
}

struct OwnedVm<'a> {
    runtime: &'a Runtime,
    context: &'a PreparationContext,
    lock: fs::File,
    started: bool,
}
impl OwnedVm<'_> {
    fn stop(&mut self) -> Result<()> {
        if self.started {
            let mut command = self.runtime.command();
            command.args(["stop", "--force", INSTANCE]);
            self.runtime
                .run(self.context, &mut command, "mac-vm-stop", true)
                .map_err(|e| {
                    Error::PreparationCleanupUnverified(format!(
                        "owned preparation VM did not stop: {e}"
                    ))
                })?;
            self.started = false;
        }
        Ok(())
    }
}
impl Drop for OwnedVm<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            eprintln!("seedbox: preparation VM cleanup: {error}");
        }
        let _ = unsafe { libc::flock(self.lock.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[derive(Clone, Debug)]
pub struct MacPreparer {
    pub inputs: OciPreparer,
    pub runtime: Runtime,
    pub cancellation: CancellationToken,
}
impl PreparationBackend for MacPreparer {
    fn validate_runtime_inputs(&self) -> Result<()> {
        self.runtime.validate_inputs(&self.inputs)
    }
    fn prepare(&self, simulation: &Simulation, store: &ContentStore) -> Result<PreparedInputs> {
        let diagnostics = tempfile::Builder::new()
            .prefix("seedbox-mac-preparation-")
            .tempdir_in(self.runtime.root.parent().ok_or_else(|| {
                Error::Integrity("Mac runtime cache has no parent directory".into())
            })?)?;
        let context =
            PreparationContext::new(diagnostics.path().join("logs"), self.cancellation.clone())?;
        match self
            .runtime
            .prepare(&self.inputs, simulation, store, &context)
        {
            Err(Error::Execution(detail)) => {
                let directory = diagnostics.keep();
                Err(Error::Execution(format!(
                    "{detail}; preparation evidence: {}",
                    directory.display()
                )))
            }
            Err(Error::PreparationCleanupUnverified(detail)) => {
                let directory = diagnostics.keep();
                Err(Error::PreparationCleanupUnverified(format!(
                    "{detail}; preparation evidence: {}",
                    directory.display()
                )))
            }
            result => result,
        }
    }
    fn prepare_cancellable(
        &self,
        simulation: &Simulation,
        store: &ContentStore,
        context: &PreparationContext,
    ) -> Result<PreparedInputs> {
        self.runtime
            .prepare(&self.inputs, simulation, store, context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn package(extra: Option<&str>) -> Vec<u8> {
        let content = b"runtime input";
        let manifest = Manifest {
            version: 1,
            files: FILES
                .into_iter()
                .map(|name| (name.into(), ContentDigest::of(content)))
                .collect(),
            tools: OciTools {
                skopeo: "/opt/development/bin/skopeo".into(),
                umoci: "/opt/development/bin/umoci".into(),
                docker: "/opt/development/bin/podman".into(),
                build_epoch: None,
            },
        };
        let gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut tar = tar::Builder::new(gzip);
        for (name, bytes) in FILES
            .into_iter()
            .map(|name| (name, content.to_vec()))
            .chain(std::iter::once((
                "runtime.json",
                serde_json::to_vec(&manifest).expect("manifest"),
            )))
            .chain(extra.into_iter().map(|name| (name, content.to_vec())))
        {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o600);
            header.set_cksum();
            tar.append_data(&mut header, name, bytes.as_slice())
                .expect("archive");
        }
        tar.into_inner().expect("tar").finish().expect("gzip")
    }
    #[test]
    fn installation_reuses_verified_bytes_and_rejects_tampering() {
        let temp = tempfile::tempdir().expect("temp");
        let bytes = package(None);
        let install = || {
            Runtime::install(
                &bytes,
                temp.path(),
                "limactl".into(),
                temp.path().join("vm"),
            )
        };
        let first = install().expect("install");
        assert_eq!(first.root, install().expect("cached").root);
        fs::write(first.input("Image"), b"changed").expect("tamper");
        assert!(install().is_err());
    }
    #[test]
    fn package_rejects_extra_and_duplicate_members_before_publication() {
        for extra in ["unexpected", "Image"] {
            let temp = tempfile::tempdir().expect("temp");
            assert!(Runtime::install(
                &package(Some(extra)),
                temp.path(),
                "limactl".into(),
                temp.path().join("vm")
            )
            .is_err());
            assert_eq!(fs::read_dir(temp.path()).expect("cache").count(), 0);
        }
    }
    #[test]
    fn existing_vm_directory_requires_private_owned_marker() {
        let temp = tempfile::tempdir().expect("temp");
        let runtime = Runtime::install(
            &package(None),
            temp.path(),
            "limactl".into(),
            temp.path().join("vm"),
        )
        .expect("install");
        fs::create_dir(&runtime.home).expect("foreign directory");
        assert!(runtime.owner().is_err());
        fs::remove_dir(&runtime.home).expect("remove");
        drop(runtime.owner().expect("owned"));
        fs::set_permissions(&runtime.home, fs::Permissions::from_mode(0o755)).expect("permissions");
        assert!(runtime.owner().is_err());
    }
}
