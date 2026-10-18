//! Delete only the native artifacts explicitly owned by one published capture.
use std::{
    ffi::{CString, OsStr, OsString},
    fs,
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

pub(crate) fn remove(attempt: &Path, name: &str) -> io::Result<()> {
    let output = attempt
        .join("output")
        .join(seedbox_application::execution::MACHINE_OUTPUT_DIR);
    let metadata = output.join(format!("checkpoint-{name}.json"));
    let file = match fs::File::open(&metadata) {
        Ok(file) => Some(file),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    if let Some(file) = file {
        let mut bytes = Vec::new();
        file.take(64 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 64 * 1024 * 1024 {
            return Err(io::Error::other("checkpoint metadata exceeds 64 MiB"));
        }
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        let driver = value.get("contract").and_then(|value| value.as_str())
            == Some(seedbox_engine_protocol::driver_world::CONTRACT);
        if driver {
            remove_driver(&output, name, driver_artifacts(value)?)?;
        } else {
            let machines = value
                .pointer("/state/world/machines")
                .and_then(|v| v.as_array())
                .ok_or_else(|| io::Error::other("checkpoint has no machine artifacts"))?;
            let mut artifacts = Vec::new();
            for machine in machines {
                let path = machine
                    .get("path")
                    .or_else(|| machine.pointer("/checkpoint/artifact"))
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| io::Error::other("unknown checkpoint artifact format"))?;
                let path = PathBuf::from(path);
                let relative = path.strip_prefix(&output).map_err(|_| {
                    io::Error::other("checkpoint artifact is outside its output directory")
                })?;
                let parts: Vec<_> = relative.components().collect();
                if parts.is_empty()
                    || parts
                        .iter()
                        .any(|part| !matches!(part, std::path::Component::Normal(_)))
                {
                    return Err(io::Error::other("invalid checkpoint artifact path"));
                }
                let first = parts[0].as_os_str().to_string_lossy();
                if !((parts.len() == 1 && first.starts_with(".arm-checkpoint-"))
                    || (parts.len() == 2 && first.starts_with(".checkpoint-machines-")))
                {
                    return Err(io::Error::other(
                        "unrecognized checkpoint artifact location",
                    ));
                }
                artifacts.push(path);
            }
            for path in artifacts {
                remove_file(&path)?;
                if let Some(parent) = path.parent().filter(|parent| *parent != output) {
                    match fs::remove_dir(parent) {
                        Ok(()) => {}
                        Err(e)
                            if matches!(
                                e.kind(),
                                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
                            ) => {}
                        Err(e) => return Err(e),
                    }
                }
            }
        }
        remove_file(&metadata)?;
    }
    remove_file(&attempt.join(format!("checkpoint-{name}-interventions.json")))
}
// Use the backend's closed envelope and complete inventory accessor: this
// includes retained configs/boot archives, not arbitrary world/program JSON.
fn driver_artifacts(value: serde_json::Value) -> io::Result<Vec<PathBuf>> {
    use seedbox_engine_protocol::driver_world as wire;
    let checkpoint: wire::Checkpoint = serde_json::from_value(value)?;
    checkpoint.validate().map_err(io::Error::other)?;
    Ok(checkpoint
        .inventory
        .artifacts()
        .into_iter()
        .chain(std::iter::once(&checkpoint.driver))
        .chain(checkpoint.machines.iter())
        .map(|artifact| PathBuf::from(&artifact.artifact))
        .collect())
}
// Validate the complete published closure before opening or unlinking anything.
// One exact capture directory, not several directories with the same prefix.
fn driver_names(
    output: &Path,
    name: &str,
    paths: Vec<PathBuf>,
) -> io::Result<(OsString, Vec<OsString>)> {
    let mut body = None;
    let mut files = Vec::new();
    for path in paths {
        let relative = path
            .strip_prefix(output)
            .map_err(|_| io::Error::other("checkpoint artifact is outside its output directory"))?;
        let parts = relative.components().collect::<Vec<_>>();
        if parts.len() != 2
            || parts
                .iter()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            return Err(io::Error::other("invalid Driver checkpoint artifact path"));
        }
        let directory = parts[0].as_os_str();
        if !directory
            .to_string_lossy()
            .starts_with(&format!("checkpoint-{name}-bodies-"))
        {
            return Err(io::Error::other(
                "unrecognized Driver checkpoint capture location",
            ));
        }
        if body.as_deref().is_some_and(|prior| prior != directory) {
            return Err(io::Error::other(
                "Driver checkpoint artifacts span multiple body directories",
            ));
        }
        body = Some(directory.to_owned());
        files.push(parts[1].as_os_str().to_owned());
    }
    Ok((
        body.ok_or_else(|| io::Error::other("checkpoint has no body artifacts"))?,
        files,
    ))
}
fn remove_driver(output: &Path, name: &str, paths: Vec<PathBuf>) -> io::Result<()> {
    let (body, files) = driver_names(output, name, paths)?;
    if let Some(capture) = OpenCapture::open(output, &body)? {
        capture.unlink_files(&files)?;
        capture.remove_directory()?;
    }
    Ok(())
}
fn c_name(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| io::Error::other("invalid checkpoint filename"))
}
// Own both directory descriptors. Every body unlink is relative to the one
// O_NOFOLLOW-opened capture inode, even if its pathname is swapped afterwards.
struct OpenCapture {
    output: fs::File,
    body: fs::File,
    name: CString,
}
impl OpenCapture {
    fn open(output: &Path, name: &OsStr) -> io::Result<Option<Self>> {
        let output = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(output)?;
        let name = c_name(name)?;
        // SAFETY: name is terminated and output owns a live directory descriptor.
        let fd = unsafe {
            libc::openat(
                output.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(error)
            };
        }
        // SAFETY: openat returned a new owned descriptor, transferred exactly once.
        let body = unsafe { fs::File::from_raw_fd(fd) };
        Ok(Some(Self { output, body, name }))
    }
    fn unlink_files(&self, names: &[OsString]) -> io::Result<()> {
        for name in names {
            let name = c_name(name)?;
            // SAFETY: validated basenames contain no separators; body stays open.
            if unsafe { libc::unlinkat(self.body.as_raw_fd(), name.as_ptr(), 0) } != 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::NotFound {
                    return Err(error);
                }
            }
        }
        Ok(())
    }
    fn remove_directory(&self) -> io::Result<()> {
        let mut owned = std::mem::MaybeUninit::<libc::stat>::uninit();
        let mut named = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: both stat buffers are writable and the descriptors/name live.
        if unsafe { libc::fstat(self.body.as_raw_fd(), owned.as_mut_ptr()) } != 0
            || unsafe {
                libc::fstatat(
                    self.output.as_raw_fd(),
                    self.name.as_ptr(),
                    named.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: both preceding system calls initialized their entire buffers.
        let (owned, named) = unsafe { (owned.assume_init(), named.assume_init()) };
        if owned.st_dev != named.st_dev
            || owned.st_ino != named.st_ino
            || named.st_mode & libc::S_IFMT != libc::S_IFDIR
        {
            return Err(io::Error::other(
                "checkpoint capture directory changed during cleanup",
            ));
        }
        // AT_REMOVEDIR does not follow a final symlink. Never recursively remove
        // a replacement or unlisted files; failure leaves the manifest intact.
        if unsafe {
            libc::unlinkat(
                self.output.as_raw_fd(),
                self.name.as_ptr(),
                libc::AT_REMOVEDIR,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
fn remove_file(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deletes_both_native_capture_formats_and_preserves_other_output() {
        let root = tempfile::tempdir().unwrap();
        let output = root
            .path()
            .join("output")
            .join(seedbox_application::execution::MACHINE_OUTPUT_DIR);
        fs::create_dir_all(output.join(".checkpoint-machines-test")).unwrap();
        let arm = output.join(".arm-checkpoint-test");
        let x86 = output.join(".checkpoint-machines-test/0.snapshot");
        for path in [&arm, &x86, &output.join("keep")] {
            fs::write(path, b"data").unwrap();
        }
        let metadata = output.join("checkpoint-saved.json");
        fs::write(
            &metadata,
            serde_json::to_vec(&serde_json::json!({
                "state": {"world": {"machines": [{"checkpoint": {"artifact": arm}}, {"path": x86}]}}
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            root.path().join("checkpoint-saved-interventions.json"),
            b"history",
        )
        .unwrap();
        remove(root.path(), "saved").unwrap();
        remove(root.path(), "saved").unwrap();
        assert!(!arm.exists() && !x86.exists() && !metadata.exists());
        assert!(!output.join(".checkpoint-machines-test").exists());
        assert_eq!(fs::read(output.join("keep")).unwrap(), b"data");
        fs::write(
            &metadata,
            serde_json::to_vec(&serde_json::json!({
                "state": {"world": {"machines": [{"path": root.path().join("outside")}]}}
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(remove(root.path(), "saved").is_err());
        assert!(metadata.exists());
    }
    fn driver_capture(
        output: &Path,
        name: &str,
    ) -> seedbox_engine_protocol::driver_world::Checkpoint {
        use seedbox_engine_protocol::driver_world as wire;
        use std::{collections::BTreeMap, num::NonZeroU32};
        let directory = output.join(format!("checkpoint-{name}-bodies-fixture"));
        fs::create_dir_all(&directory).unwrap();
        let artifact = |name: &str| {
            let path = directory.join(name);
            fs::write(&path, b"host-only capture bytes").unwrap();
            wire::Artifact {
                artifact: path.to_str().unwrap().into(),
                sha256: "00".repeat(32),
                length: 23,
            }
        };
        let factory = artifact("factory");
        let boot = artifact("boot");
        let config = artifact("config");
        wire::Checkpoint {
            contract: wire::CONTRACT.into(),
            version: wire::ENVELOPE_VERSION,
            inventory: wire::Inventory::Pv {
                policy: None,
                executable: factory.clone(),
                kernel: factory.clone(),
                init: factory.clone(),
                command_line: factory.clone(),
                progress: factory.clone(),
                runtime: factory.clone(),
                formatter: Some(factory),
                boot_archives: BTreeMap::from([("oracle".into(), boot)]),
                environments: BTreeMap::from([(
                    "oracle".into(),
                    wire::Environment {
                        hostname: "oracle".into(),
                        hosts: String::new(),
                        anonymous_volumes: vec![],
                        named_volumes: vec![],
                        configs: BTreeMap::from([("artifact".into(), config)]),
                        control_socket: false,
                        limits: wire::GuestLimits {
                            max_map_count: NonZeroU32::new(262144).unwrap(),
                            nofile: NonZeroU32::new(1048576).unwrap(),
                        },
                    },
                )]),
                progress_budget: NonZeroU32::new(32).unwrap().into(),
                epoch_seconds: 0,
            },
            driver: artifact("driver"),
            machines: vec![artifact("machine")],
            state: wire::State {
                world: wire::World {
                    seed: 123,
                    machine_names: vec!["oracle".into()],
                    input_program: Default::default(),
                    intervention_next: 0,
                    publication: Default::default(),
                    position: Default::default(),
                    readiness: wire::Readiness {
                        initial: vec!["oracle".into()],
                        seen: vec![],
                        latched: false,
                    },
                },
            },
        }
    }
    #[test]
    fn driver_capture_deletion_removes_full_typed_closure_only() {
        let root = tempfile::tempdir().unwrap();
        let output = root
            .path()
            .join("output")
            .join(seedbox_application::execution::MACHINE_OUTPUT_DIR);
        let capture = driver_capture(&output, "saved");
        capture.validate().unwrap();
        let other = output.join("checkpoint-other-bodies-fixture");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("keep"), b"unrelated").unwrap();
        let metadata = output.join("checkpoint-saved.json");
        fs::write(&metadata, serde_json::to_vec(&capture).unwrap()).unwrap();
        fs::write(
            root.path().join("checkpoint-saved-interventions.json"),
            b"history",
        )
        .unwrap();
        remove(root.path(), "saved").unwrap();
        remove(root.path(), "saved").unwrap();
        assert!(!metadata.exists());
        assert!(!output.join("checkpoint-saved-bodies-fixture").exists());
        assert!(!root
            .path()
            .join("checkpoint-saved-interventions.json")
            .exists());
        assert_eq!(fs::read(other.join("keep")).unwrap(), b"unrelated");
    }
    #[test]
    fn driver_invalid_version_or_unowned_paths_preserve_every_capture_file() {
        for case in ["version", "outside", "other-capture"] {
            let root = tempfile::tempdir().unwrap();
            let output = root
                .path()
                .join("output")
                .join(seedbox_application::execution::MACHINE_OUTPUT_DIR);
            let mut capture = driver_capture(&output, "saved");
            let files = fs::read_dir(output.join("checkpoint-saved-bodies-fixture"))
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<Vec<_>>();
            match case {
                "version" => capture.version += 1,
                "outside" => {
                    capture.driver.artifact = root.path().join("outside").to_str().unwrap().into()
                }
                "other-capture" => {
                    capture.machines[0].artifact = output
                        .join("checkpoint-other-bodies-fixture/machine")
                        .to_str()
                        .unwrap()
                        .into()
                }
                _ => unreachable!(),
            }
            let metadata = output.join("checkpoint-saved.json");
            fs::write(&metadata, serde_json::to_vec(&capture).unwrap()).unwrap();
            assert!(remove(root.path(), "saved").is_err(), "{case}");
            assert!(
                metadata.exists() && files.iter().all(|file| file.exists()),
                "partial cleanup on {case}"
            );
        }
    }
    #[cfg(unix)]
    #[test]
    fn driver_capture_parent_symlink_never_authorizes_unlinks() {
        let root = tempfile::tempdir().unwrap();
        let output = root
            .path()
            .join("output")
            .join(seedbox_application::execution::MACHINE_OUTPUT_DIR);
        let capture = driver_capture(&output, "saved");
        let directory = output.join("checkpoint-saved-bodies-fixture");
        let outside = root.path().join("outside");
        fs::rename(&directory, &outside).unwrap();
        std::os::unix::fs::symlink(&outside, &directory).unwrap();
        let metadata = output.join("checkpoint-saved.json");
        fs::write(&metadata, serde_json::to_vec(&capture).unwrap()).unwrap();
        assert!(remove(root.path(), "saved").is_err());
        assert!(
            outside.join("factory").exists()
                && outside.join("driver").exists()
                && outside.join("machine").exists()
        );
        assert!(metadata.exists());
    }
    #[test]
    fn driver_capture_cannot_span_two_same_prefix_directories() {
        let root = tempfile::tempdir().unwrap();
        let output = root
            .path()
            .join("output")
            .join(seedbox_application::execution::MACHINE_OUTPUT_DIR);
        let mut capture = driver_capture(&output, "saved");
        let original = output.join("checkpoint-saved-bodies-fixture");
        let other = output.join("checkpoint-saved-bodies-other");
        fs::create_dir(&other).unwrap();
        let moved = other.join("machine");
        fs::rename(original.join("machine"), &moved).unwrap();
        capture.machines[0].artifact = moved.to_str().unwrap().into();
        capture.validate().unwrap();
        let metadata = output.join("checkpoint-saved.json");
        fs::write(&metadata, serde_json::to_vec(&capture).unwrap()).unwrap();
        assert!(remove(root.path(), "saved").is_err());
        assert!(
            original.join("factory").exists() && original.join("driver").exists() && moved.exists()
        );
        assert!(metadata.exists());
    }
    #[cfg(unix)]
    #[test]
    fn opened_capture_cannot_be_redirected_by_a_post_open_symlink_swap() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let output = root
            .path()
            .join("output")
            .join(seedbox_application::execution::MACHINE_OUTPUT_DIR);
        let capture = driver_capture(&output, "saved");
        let metadata = output.join("checkpoint-saved.json");
        fs::write(&metadata, serde_json::to_vec(&capture).unwrap()).unwrap();
        let (directory, files) = driver_names(
            &output,
            "saved",
            driver_artifacts(serde_json::to_value(capture).unwrap()).unwrap(),
        )
        .unwrap();
        let pinned = OpenCapture::open(&output, &directory).unwrap().unwrap();
        let owned = root.path().join("moved-original-capture");
        fs::rename(output.join(&directory), &owned).unwrap();
        for name in &files {
            fs::write(outside.path().join(name), b"must survive").unwrap();
        }
        // This is the exact old validate-then-unlink race, after the production
        // helper has opened the real body directory but before any unlink.
        std::os::unix::fs::symlink(outside.path(), output.join(&directory)).unwrap();
        pinned.unlink_files(&files).unwrap();
        assert!(files.iter().all(|name| !owned.join(name).exists()));
        for name in &files {
            assert_eq!(
                fs::read(outside.path().join(name)).unwrap(),
                b"must survive"
            );
        }
        assert!(pinned.remove_directory().is_err());
        assert!(output.join(&directory).is_symlink() && metadata.exists());
        assert!(remove(root.path(), "saved").is_err());
    }
}
