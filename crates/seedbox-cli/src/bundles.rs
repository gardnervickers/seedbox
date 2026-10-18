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

pub(super) fn export(
    id: &SimulationId,
    path: Option<PathBuf>,
    max_bytes: u64,
    json: bool,
) -> Result<u8> {
    let destination =
        Destination::new(path.unwrap_or_else(|| PathBuf::from(format!("{id}.seedbox.tar"))))?;
    let token = CancellationToken::default();
    let _signals = Signals::register(&token)?;
    let client = daemon::connect(None)?;
    let mut file = tokio::fs::File::from_std(
        destination
            .file
            .try_clone()
            .map_err(|e| path_error(&destination.path, e))?,
    );
    let size = runtime()?
        .block_on(client.export_bundle(id, &mut file, max_bytes, || token.is_cancelled()))
        .map_err(client_error)?;
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
        output::status("EXPORTED", destination.path.display());
    }
    Ok(0)
}

pub(super) fn import(path: &Path, max_bytes: u64, json: bool) -> Result<u8> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| {
            Error::Setup(format!(
                "bundle {}: {}; choose a readable regular bundle file",
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
        .map_err(|e| Error::Setup(format!("bundle {}: {e}", path.display())))?;
    if !metadata.is_file() {
        return Err(Error::Setup(format!(
            "bundle {}: not a regular file",
            path.display()
        )));
    }
    if metadata.len() > max_bytes {
        return Err(Error::Setup(format!(
            "bundle {}: exceeds --max-bytes {max_bytes}",
            path.display()
        )));
    }
    let token = CancellationToken::default();
    let _signals = Signals::register(&token)?;
    let client = daemon::connect(None)?;
    let simulation = runtime()?
        .block_on(
            client.import_bundle(tokio::fs::File::from_std(file), max_bytes, || {
                token.is_cancelled()
            }),
        )
        .map_err(client_error)?;
    if json {
        output::json(&simulation)?;
    } else {
        output::status("IMPORTED", simulation.metadata.id);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
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
