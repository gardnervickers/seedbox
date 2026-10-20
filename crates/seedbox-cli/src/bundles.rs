//! Server resource transfers and completed raw diagnostics. Output files are
//! published by linkat on a pinned parent descriptor, never by path rename.
use super::{
    daemon, output,
    signals::Signals,
    simulations::{client_error, runtime},
};
use seedbox_api::SimulationId;
use seedbox_application::{execution::CancellationToken, Error, Result};
use std::{
    ffi::CString,
    fs::File,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::ffi::OsStrExt,
    },
    path::{Component, Path, PathBuf},
};

fn path_error(path: &Path, error: impl std::fmt::Display) -> Error {
    Error::Setup(format!(
        "--output {}: {error}; choose a new file under real directories",
        path.display()
    ))
}

struct Destination {
    parent: File,
    name: CString,
    staging: PrivateStaging,
    file: File,
    path: PathBuf,
}
impl Destination {
    fn new(path: PathBuf) -> Result<Self> {
        let name = path
            .file_name()
            .ok_or_else(|| path_error(&path, "no file name"))?;
        let name = CString::new(name.as_bytes()).map_err(|e| path_error(&path, e))?;
        let parent_path = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut parent = File::open(if parent_path.is_absolute() { "/" } else { "." })
            .map_err(|e| path_error(&path, e))?;
        for component in parent_path.components() {
            let value = match component {
                Component::Normal(value) => value,
                Component::ParentDir => std::ffi::OsStr::new(".."),
                Component::CurDir | Component::RootDir => continue,
                Component::Prefix(_) => return Err(path_error(&path, "invalid directory")),
            };
            let value = CString::new(value.as_bytes()).map_err(|e| path_error(&path, e))?;
            // SAFETY: valid descriptor and NUL-terminated component; own returned fd.
            let fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    value.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(path_error(&path, std::io::Error::last_os_error()));
            }
            parent = unsafe { File::from_raw_fd(fd) };
        }
        // fstatat sees dangling symlinks as existing, without following them.
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        let exists = unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if exists == 0 {
            return Err(path_error(&path, "already exists"));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::NotFound {
            return Err(path_error(&path, error));
        }
        let mut staging = PrivateStaging::new(&parent, &path)?;
        let file = staging.create_payload(&path)?;
        Ok(Self {
            parent,
            name,
            staging,
            file,
            path,
        })
    }
    fn publish(&self) -> Result<()> {
        self.file
            .sync_all()
            .map_err(|e| path_error(&self.path, e))?;
        // The source is inside a private directory pinned by its descriptor:
        // replacing its public staging name cannot substitute the downloaded file.
        // linkat never replaces an existing destination file or symlink.
        let result = unsafe {
            libc::linkat(
                self.staging.directory.as_raw_fd(),
                c"bundle".as_ptr(),
                self.parent.as_raw_fd(),
                self.name.as_ptr(),
                0,
            )
        };
        if result != 0 {
            return Err(path_error(&self.path, std::io::Error::last_os_error()));
        }
        Ok(())
    }
}
/// The private payload is descriptor-relative. Public directory cleanup is
/// allowed only in an owner-controlled namespace, excluding same-UID/root races.
struct PrivateStaging {
    parent: File,
    name: CString,
    directory: File,
    payload_created: bool,
    path: PathBuf,
}
impl PrivateStaging {
    fn new(parent: &File, path: &Path) -> Result<Self> {
        let parent = parent.try_clone().map_err(|e| path_error(path, e))?;
        let name = CString::new(format!(
            ".seedbox-export-{}",
            seedbox_client::Client::request_id().map_err(client_error)?
        ))
        .map_err(|e| path_error(path, e))?;
        if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
            return Err(path_error(path, std::io::Error::last_os_error()));
        }
        Self::open_created(parent, name, path)
    }

    fn open_created(parent: File, name: CString, path: &Path) -> Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let staging_path = path
            .parent()
            .unwrap_or(Path::new("."))
            .join(name.to_string_lossy().as_ref());
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            staging_warning(
                &staging_path,
                "directory could not be opened and its ownership is unverified",
            );
            return Err(path_error(path, error));
        }
        let directory = unsafe { File::from_raw_fd(fd) };
        // No cleanup guard owns this descriptor until validation succeeds.
        let metadata = directory.metadata().map_err(|e| {
            staging_warning(&staging_path, "directory ownership could not be verified");
            path_error(path, e)
        })?;
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o777 != 0o700
            || !no_extended_acl(&directory)
        {
            staging_warning(&staging_path, "directory privacy could not be verified");
            return Err(path_error(
                path,
                "export staging directory is not private or its ACL cannot be verified",
            ));
        }
        Ok(Self {
            parent,
            name,
            directory,
            payload_created: false,
            path: staging_path,
        })
    }

    fn create_payload(&mut self, path: &Path) -> Result<File> {
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                c"bundle".as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(path_error(path, std::io::Error::last_os_error()));
        }
        self.payload_created = true;
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn staging_warning(path: &Path, reason: &str) {
    eprintln!("export staging cleanup skipped for {path:?}: {reason}; inspect this directory before manual cleanup");
}

/// An absent ACL is the only proven case. Unsupported inspection is conservative.
#[cfg(target_os = "linux")]
fn no_extended_acl(file: &File) -> bool {
    let result = unsafe {
        libc::fgetxattr(
            file.as_raw_fd(),
            c"system.posix_acl_access".as_ptr(),
            std::ptr::null_mut(),
            0,
        )
    };
    result < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENODATA)
}
#[cfg(target_os = "macos")]
fn no_extended_acl(file: &File) -> bool {
    // Darwin's ACL API is not exposed by libc; null with ENOENT means no ACL.
    unsafe extern "C" {
        fn acl_get_fd(fd: libc::c_int) -> *mut libc::c_void;
        fn acl_free(acl: *mut libc::c_void) -> libc::c_int;
    }
    let acl = unsafe { acl_get_fd(file.as_raw_fd()) };
    if acl.is_null() {
        return std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT);
    }
    unsafe {
        acl_free(acl);
    }
    false
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn no_extended_acl(_: &File) -> bool {
    false
}

fn owner_controlled_namespace(parent: &File) -> bool {
    use std::os::unix::fs::MetadataExt;
    parent.metadata().is_ok_and(|metadata| {
        metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o022 == 0
            && no_extended_acl(parent)
    })
}

impl Drop for PrivateStaging {
    fn drop(&mut self) {
        if self.payload_created
            && unsafe { libc::unlinkat(self.directory.as_raw_fd(), c"bundle".as_ptr(), 0) } != 0
        {
            staging_warning(&self.path, "owned payload could not be removed");
            return;
        }
        if !owner_controlled_namespace(&self.parent) {
            staging_warning(
                &self.path,
                "output parent is shared or its ACL cannot be verified",
            );
            return;
        }
        let mut named = std::mem::MaybeUninit::<libc::stat>::uninit();
        let mut opened = std::mem::MaybeUninit::<libc::stat>::uninit();
        let unchanged = unsafe {
            libc::fstat(self.directory.as_raw_fd(), opened.as_mut_ptr()) == 0
                && libc::fstatat(
                    self.parent.as_raw_fd(),
                    self.name.as_ptr(),
                    named.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                ) == 0
                && {
                    let opened = opened.assume_init();
                    let named = named.assume_init();
                    opened.st_dev == named.st_dev && opened.st_ino == named.st_ino
                }
        };
        // This identity check detects an already-substituted name, not a portable
        // stat/unlink atomic operation. Other users cannot mutate the proven parent;
        // same-UID/root actors capable of changing it are outside this boundary.
        if !unchanged {
            staging_warning(&self.path, "public directory name changed");
            return;
        }
        if unsafe {
            libc::unlinkat(
                self.parent.as_raw_fd(),
                self.name.as_ptr(),
                libc::AT_REMOVEDIR,
            )
        } != 0
        {
            staging_warning(&self.path, "directory could not be removed");
        }
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportReceipt<'a> {
    simulation: &'a SimulationId,
    output: &'a Path,
    size_bytes: u64,
}

/// Recognize the archive header rather than requiring a particular file suffix.
pub(super) fn is_checkpoint(path: &Path) -> Result<bool> {
    use std::io::Read;
    if path.is_dir() || !path.exists() {
        return Ok(false);
    }
    use std::os::unix::fs::OpenOptionsExt;
    // Project documents may be symlinks. Only the actual archive load applies
    // the stricter regular-file/no-follow transfer contract.
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| Error::Setup(format!("{}: {e}", path.display())))?;
    if !file
        .metadata()
        .map_err(|e| Error::Setup(format!("{}: {e}", path.display())))?
        .is_file()
    {
        return Ok(false);
    }
    let mut header = [0; 16];
    let count = file
        .read(&mut header)
        .map_err(|e| Error::Setup(format!("{}: {e}", path.display())))?;
    Ok(count == header.len() && &header == b"simulation.json\0")
}

pub(super) fn checkpoint(
    server: &super::server_args::ServerArgs,
    id: &SimulationId,
    path: PathBuf,
    json: bool,
) -> Result<u8> {
    let destination = Destination::new(path)?;
    let token = CancellationToken::default();
    let _signals = Signals::register(&token)?;
    let client = daemon::connect(server)?;
    let mut file = tokio::fs::File::from_std(
        destination
            .file
            .try_clone()
            .map_err(|e| path_error(&destination.path, e))?,
    );
    let size = runtime()?.block_on(async {
        let captured = capture(&client, id, &token).await?;
        client
            .export_checkpoint_bundle(
                id,
                &captured.request.request_id,
                &mut file,
                seedbox_api::BUNDLE_MAX_BYTES,
                || token.is_cancelled(),
            )
            .await
            .map_err(client_error)
    })?;
    if token.is_cancelled() {
        return Err(client_error(seedbox_client::Error::Cancelled));
    }
    destination.publish()?;
    if json {
        output::json(&ExportReceipt {
            simulation: id,
            output: &destination.path,
            size_bytes: size,
        })?;
    } else {
        output::status("CHECKPOINTED", destination.path.display());
    }
    Ok(0)
}

/// Temporarily pause at a committed boundary and restore the caller's pacing.
async fn capture(
    client: &seedbox_client::Client,
    id: &SimulationId,
    token: &CancellationToken,
) -> Result<seedbox_api::ControlReceipt> {
    use seedbox_api::{ControlCommand, ControlState, PlaybackRate};
    let original = client.playback(id).await.map_err(client_error)?.state;
    if original.terminal {
        return Err(Error::Setup(format!(
            "simulation {id} has completed; capture a checkpoint while it is running"
        )));
    }
    if !original.application_ready {
        return Err(Error::Setup(format!("simulation {id} applications are not ready; use seedbox wait {id} --for machine/NAME=ready for each machine, then retry checkpoint")));
    }
    if original.stepping {
        return Err(Error::Setup(format!(
            "simulation {id} is stepping; wait for the step to finish, then retry checkpoint"
        )));
    }
    let resume = original.rate != PlaybackRate::Paused;
    let captured = async {
        if resume {
            control(
                client,
                id,
                ControlCommand::SetRate {
                    rate: PlaybackRate::Paused,
                },
                Some(token),
            )
            .await?;
        }
        control(client, id, ControlCommand::Checkpoint, Some(token)).await
    }
    .await;
    // Restore host pacing even after a rejected capture, lost response or
    // cancellation. Capture is immutable once acknowledged, so the source
    // may keep running while the file is exported.
    if resume {
        let restoration = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            control(
                client,
                id,
                if original.before_next_fault {
                    ControlCommand::PauseBeforeNextFault {
                        rate: original.rate,
                    }
                } else {
                    ControlCommand::SetRate {
                        rate: original.rate,
                    }
                },
                None,
            ),
        )
        .await
        .map_err(|_| {
            Error::Setup(format!(
                "simulation {id}: restoring playback timed out; use seedbox control resume {id}"
            ))
        })
        .and_then(|result| result);
        if let Err(error) = restoration {
            if captured.is_ok() {
                return Err(error);
            }
            eprintln!("playback restoration: {error}");
        }
    }
    let captured = captured?;
    let ControlState::Acknowledged {
        checkpoint: Some(_),
        ..
    } = captured.state
    else {
        return Err(Error::Setup(
            "checkpoint capture returned no saved state".into(),
        ));
    };
    Ok(captured)
}

/// Wait for durable acknowledgement, including deferred checkpoint capture.
async fn control(
    client: &seedbox_client::Client,
    id: &SimulationId,
    command: seedbox_api::ControlCommand,
    cancellation: Option<&CancellationToken>,
) -> Result<seedbox_api::ControlReceipt> {
    use seedbox_api::{CheckpointState, ControlRequest, ControlState};
    let request = ControlRequest {
        request_id: seedbox_client::Client::request_id().map_err(client_error)?,
        command,
    };
    eprintln!("request: {}", request.request_id);
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        return Err(client_error(seedbox_client::Error::Cancelled));
    }
    let mut receipt: seedbox_api::ControlReceipt = client
        .request_control(id, &request)
        .await
        .map_err(client_error)?;
    loop {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(client_error(seedbox_client::Error::Cancelled));
        }
        match &receipt.state {
            ControlState::Accepted => {}
            ControlState::Acknowledged {
                error: None,
                checkpoint: Some(checkpoint),
                ..
            } if checkpoint.state == CheckpointState::Requested => {}
            ControlState::Acknowledged {
                error: None,
                checkpoint,
                ..
            } if checkpoint
                .as_ref()
                .is_none_or(|capture| capture.state == CheckpointState::Captured) =>
            {
                return Ok(receipt)
            }
            state => {
                return Err(Error::Setup(format!(
                    "checkpoint control failed: {}",
                    serde_json::to_string(state)?
                )))
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        receipt = client
            .control_receipt(id, &request.request_id)
            .await
            .map_err(client_error)?;
    }
}

fn open_checkpoint(path: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| {
            Error::Setup(format!(
                "checkpoint {}: {}; choose a readable regular checkpoint file",
                path.display(),
                if e.kind() == std::io::ErrorKind::NotFound {
                    "not found".into()
                } else {
                    e.to_string()
                }
            ))
        })?;
    let metadata = file
        .metadata()
        .map_err(|e| Error::Setup(format!("checkpoint {}: {e}", path.display())))?;
    if !metadata.is_file() {
        return Err(Error::Setup(format!(
            "checkpoint {}: not a regular file",
            path.display()
        )));
    }
    if metadata.len() > seedbox_api::BUNDLE_MAX_BYTES {
        return Err(Error::Setup(format!(
            "checkpoint {}: exceeds {} bytes",
            path.display(),
            seedbox_api::BUNDLE_MAX_BYTES
        )));
    }
    Ok(file)
}

/// Upload all captured inputs before submitting a new simulation. No source ID
/// or original-server content cache participates in this operation.
pub(super) fn load_checkpoint(
    server: &super::server_args::ServerArgs,
    path: &Path,
) -> Result<seedbox_spec::definition::Definition> {
    let file = open_checkpoint(path)?;
    let token = CancellationToken::default();
    let _signals = Signals::register(&token)?;
    let client = daemon::connect(server)?;
    runtime()?
        .block_on(client.import_checkpoint_bundle(
            tokio::fs::File::from_std(file),
            seedbox_api::BUNDLE_MAX_BYTES,
            || token.is_cancelled(),
        ))
        .map_err(client_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    #[tokio::test]
    async fn capture_restores_playback_after_success_rejection_and_cancellation() {
        use seedbox_api::{
            CheckpointState, ControlCommand, ControlReceipt, ControlRequest, ControlState,
            PlaybackObservation, PlaybackRate, PlaybackStatus,
        };
        use std::{num::NonZeroU32, time::Duration};
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        let limited = PlaybackRate::Limited {
            numerator: NonZeroU32::new(1).unwrap(),
            denominator: NonZeroU32::new(10).unwrap(),
        };
        for (rate, armed) in [
            (PlaybackRate::Paused, false),
            (PlaybackRate::Unlimited, false),
            (limited, false),
            (PlaybackRate::Unlimited, true),
            (limited, true),
        ] {
            for outcome in ["captured", "deferred", "rejected", "cancelled", "not-ready"] {
                let directory = tempfile::tempdir().unwrap();
                let socket = directory.path().join("api.sock");
                let listener = tokio::net::UnixListener::bind(&socket).unwrap();
                let token = CancellationToken::default();
                let server_token = token.clone();
                let server = tokio::spawn(async move {
                    let mut status = PlaybackStatus {
                        position: Default::default(),
                        rate,
                        achieved_rate: None,
                        stepping: false,
                        before_next_fault: armed,
                        terminal: false,
                        application_ready: outcome != "not-ready",
                    };
                    let requests = if outcome == "not-ready" {
                        1
                    } else {
                        2 + usize::from(rate != PlaybackRate::Paused) * 2
                            + usize::from(outcome == "deferred")
                    };
                    let mut commands = Vec::new();
                    let mut pending: Option<ControlReceipt> = None;
                    for _ in 0..requests {
                        let (connection, _) = listener.accept().await.unwrap();
                        let mut connection = BufReader::new(connection);
                        let mut first = String::new();
                        connection.read_line(&mut first).await.unwrap();
                        let mut length = 0;
                        loop {
                            let mut header = String::new();
                            connection.read_line(&mut header).await.unwrap();
                            if header == "\r\n" {
                                break;
                            }
                            if let Some(value) =
                                header.to_ascii_lowercase().strip_prefix("content-length:")
                            {
                                length = value.trim().parse::<usize>().unwrap();
                            }
                        }
                        let mut body = vec![0; length];
                        connection.read_exact(&mut body).await.unwrap();
                        let response = if first.contains("/playback ") {
                            serde_json::to_vec(&PlaybackObservation {
                                state: status.clone(),
                                age_ns: 0,
                            })
                            .unwrap()
                        } else if first.starts_with("GET ") {
                            let mut receipt = pending.take().unwrap();
                            if let ControlState::Acknowledged {
                                checkpoint: Some(checkpoint),
                                ..
                            } = &mut receipt.state
                            {
                                checkpoint.state = CheckpointState::Captured;
                            }
                            serde_json::to_vec(&receipt).unwrap()
                        } else {
                            let request: ControlRequest = serde_json::from_slice(&body).unwrap();
                            commands.push(request.command.clone());
                            let mut checkpoint = None;
                            let rejected = request.command == ControlCommand::Checkpoint
                                && outcome == "rejected";
                            match request.command {
                                ControlCommand::SetRate { rate } => {
                                    status.rate = rate;
                                    status.before_next_fault = false;
                                }
                                ControlCommand::PauseBeforeNextFault { rate } => {
                                    status.rate = rate;
                                    status.before_next_fault = true;
                                }
                                ControlCommand::Checkpoint => {
                                    assert_eq!(status.rate, PlaybackRate::Paused);
                                    checkpoint = Some(seedbox_api::CheckpointStatus {
                                        request_id: request.request_id.clone(),
                                        position: Default::default(),
                                        state: if outcome == "deferred" {
                                            CheckpointState::Requested
                                        } else {
                                            CheckpointState::Captured
                                        },
                                    });
                                    if outcome == "cancelled" {
                                        server_token.cancel();
                                    }
                                }
                                _ => panic!("unexpected command"),
                            }
                            let receipt = ControlReceipt {
                                request,
                                revision: 1,
                                state: if rejected {
                                    ControlState::Unsupported
                                } else {
                                    ControlState::Acknowledged {
                                        status: status.clone(),
                                        error: None,
                                        intervention: None,
                                        detail: None,
                                        checkpoint,
                                    }
                                },
                            };
                            pending = Some(receipt.clone());
                            serde_json::to_vec(&receipt).unwrap()
                        };
                        let header = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", response.len());
                        connection
                            .get_mut()
                            .write_all(header.as_bytes())
                            .await
                            .unwrap();
                        connection.get_mut().write_all(&response).await.unwrap();
                    }
                    assert_eq!(status.rate, rate);
                    assert_eq!(status.before_next_fault, armed);
                    commands
                });
                let client = seedbox_client::Client::connect_to(&socket).unwrap();
                let result = tokio::time::timeout(
                    Duration::from_secs(30),
                    capture(&client, &"abcdefgh".parse().unwrap(), &token),
                )
                .await
                .unwrap();
                assert_eq!(
                    result.is_ok(),
                    matches!(outcome, "captured" | "deferred"),
                    "{outcome}: {result:?}"
                );
                let commands = tokio::time::timeout(Duration::from_secs(30), server)
                    .await
                    .unwrap()
                    .unwrap();
                if outcome == "not-ready" {
                    assert!(commands.is_empty());
                    assert!(result.unwrap_err().to_string().contains("seedbox wait"));
                } else if rate == PlaybackRate::Paused {
                    assert_eq!(commands, vec![ControlCommand::Checkpoint]);
                } else {
                    assert_eq!(
                        commands,
                        vec![
                            ControlCommand::SetRate {
                                rate: PlaybackRate::Paused
                            },
                            ControlCommand::Checkpoint,
                            if armed {
                                ControlCommand::PauseBeforeNextFault { rate }
                            } else {
                                ControlCommand::SetRate { rate }
                            }
                        ]
                    );
                }
            }
        }
    }

    #[test]
    fn checkpoint_detection_uses_content_and_preserves_symlink_projects() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("seedbox.yaml");
        std::fs::write(&project, b"machines: {}\noracle: {}\n").unwrap();
        let link = directory.path().join("project-link");
        std::os::unix::fs::symlink(&project, &link).unwrap();
        assert!(!is_checkpoint(&project).unwrap());
        assert!(!is_checkpoint(&link).unwrap());
        assert!(!is_checkpoint(directory.path()).unwrap());
        let checkpoint = directory.path().join("saved-without-an-extension");
        std::fs::write(&checkpoint, b"simulation.json\0rest of archive").unwrap();
        assert!(is_checkpoint(&checkpoint).unwrap());
    }

    #[test]
    fn destination_never_replaces_files_or_symlinks_and_pins_parent() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let parent = root.as_path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        let destination = Destination::new(parent.join("result")).unwrap();
        (&destination.file).write_all(b"verified archive").unwrap();
        std::fs::rename(&parent, root.as_path().join("moved")).unwrap();
        std::os::unix::fs::symlink(root.as_path(), &parent).unwrap();
        destination.publish().unwrap();
        assert!(!root.as_path().join("result").exists());
        assert_eq!(
            std::fs::read(root.as_path().join("moved/result")).unwrap(),
            b"verified archive"
        );
        assert!(Destination::new(parent.join("other")).is_err());
        let destination = Destination::new(root.as_path().join("collision")).unwrap();
        std::os::unix::fs::symlink("missing", root.as_path().join("collision")).unwrap();
        assert!(destination.publish().is_err());
        assert_eq!(
            std::fs::read_link(root.as_path().join("collision")).unwrap(),
            Path::new("missing")
        );
    }
    #[test]
    fn private_staging_name_substitution_cannot_replace_published_bytes() {
        use std::os::unix::fs::{symlink, MetadataExt};
        for replacement in 0..3 {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let destination = Destination::new(root.join("result")).unwrap();
            (&destination.file).write_all(b"verified archive").unwrap();
            let staging = root.join(destination.staging.name.to_str().unwrap());
            assert_eq!(std::fs::metadata(&staging).unwrap().mode() & 0o777, 0o700);
            let moved = root.join("moved-staging");
            std::fs::rename(&staging, &moved).unwrap();
            let hostile = root.join("hostile");
            std::fs::create_dir(&hostile).unwrap();
            std::fs::write(hostile.join("bundle"), b"substituted archive").unwrap();
            match replacement {
                0 => std::fs::write(&staging, b"substituted archive").unwrap(),
                1 => symlink(&hostile, &staging).unwrap(),
                _ => std::fs::create_dir(&staging).unwrap(),
            }
            destination.publish().unwrap();
            assert_eq!(
                std::fs::read(root.join("result")).unwrap(),
                b"verified archive"
            );
            drop(destination);
            assert!(!moved.join("bundle").exists());
            assert!(
                std::fs::symlink_metadata(&staging).is_ok(),
                "replacement must not be removed"
            );
            assert_eq!(
                std::fs::read(hostile.join("bundle")).unwrap(),
                b"substituted archive"
            );
        }
    }

    #[test]
    fn private_staging_is_cleaned_without_publication() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let destination = Destination::new(root.join("result")).unwrap();
        let staging = root.join(destination.staging.name.to_str().unwrap());
        (&destination.file).write_all(b"partial archive").unwrap();
        drop(destination);
        assert!(!root.join("result").exists());
        assert!(!staging.exists());
    }
    #[test]
    fn rejected_substituted_staging_directory_does_not_own_cleanup() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let name = CString::new("staging").unwrap();
        std::fs::create_dir(root.join("staging")).unwrap();
        std::fs::rename(root.join("staging"), root.join("moved")).unwrap();
        std::fs::create_dir(root.join("staging")).unwrap();
        std::fs::set_permissions(root.join("staging"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::write(root.join("staging/bundle"), b"preexisting payload").unwrap();
        assert!(PrivateStaging::open_created(
            File::open(&root).unwrap(),
            name,
            &root.join("result")
        )
        .is_err());
        assert_eq!(
            std::fs::read(root.join("staging/bundle")).unwrap(),
            b"preexisting payload"
        );
        assert!(root.join("moved").is_dir());
    }

    #[test]
    fn exclusive_payload_create_failure_does_not_unlink_preexisting_payload() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let mut staging =
            PrivateStaging::new(&File::open(&root).unwrap(), &root.join("result")).unwrap();
        let staging_path = staging.path.clone();
        std::fs::write(staging_path.join("bundle"), b"preexisting payload").unwrap();
        assert!(staging.create_payload(&root.join("result")).is_err());
        drop(staging);
        assert_eq!(
            std::fs::read(staging_path.join("bundle")).unwrap(),
            b"preexisting payload"
        );
    }

    #[test]
    fn owner_controlled_0755_parent_cleans_directory_and_shared_parent_retains_it() {
        use std::os::unix::fs::PermissionsExt;
        for mode in [0o755, 0o777] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(mode)).unwrap();
            let destination = Destination::new(root.join("result")).unwrap();
            let staging_path = destination.staging.path.clone();
            (&destination.file).write_all(b"verified archive").unwrap();
            destination.publish().unwrap();
            drop(destination);
            assert_eq!(
                std::fs::read(root.join("result")).unwrap(),
                b"verified archive"
            );
            assert_eq!(staging_path.exists(), mode == 0o777);
            if mode == 0o777 {
                assert!(std::fs::read_dir(staging_path).unwrap().next().is_none());
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn inherited_staging_acl_is_rejected_without_deleting_preexisting_payload() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let name = CString::new("staging").unwrap();
        let staging_path = root.join("staging");
        std::fs::create_dir(&staging_path).unwrap();
        std::fs::set_permissions(&staging_path, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(staging_path.join("bundle"), b"preexisting payload").unwrap();
        assert!(std::process::Command::new("/bin/chmod")
            .args([
                "+a",
                "everyone allow read,write,execute,file_inherit,directory_inherit"
            ])
            .arg(&root)
            .status()
            .unwrap()
            .success());
        // Apply an inherited ACL as mkdir beneath this parent would receive.
        assert!(std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone allow read,write,execute"])
            .arg(&staging_path)
            .status()
            .unwrap()
            .success());
        assert!(!owner_controlled_namespace(&File::open(&root).unwrap()));
        assert!(PrivateStaging::open_created(
            File::open(&root).unwrap(),
            name,
            &root.join("result")
        )
        .is_err());
        assert_eq!(
            std::fs::read(staging_path.join("bundle")).unwrap(),
            b"preexisting payload"
        );
        assert!(
            PrivateStaging::new(&File::open(&root).unwrap(), &root.join("result")).is_err(),
            "new directory inherits the unsafe ACL too"
        );
    }
}
