use super::*;
use std::os::unix::fs::PermissionsExt;

fn names(directory: &Path) -> Vec<PathBuf> {
    fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect()
}

#[test]
fn trusted_objects_clone_copy_on_write_without_disturbing_the_store() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path().join("store"));
    let digest = store.put(b"retained guest bytes").unwrap();
    let source = store.open(&digest).unwrap();
    let stamp = store
        .trusted(&digest, &source)
        .unwrap()
        .expect("put hashed the object");
    let mode = source.metadata().unwrap().permissions().mode();
    let inputs = directory.path().join("inputs");
    fs::create_dir(&inputs).unwrap();
    let mut temp = tempfile::NamedTempFile::new_in(&inputs).unwrap();
    assert!(clone_file(&source, &mut temp).unwrap(), "APFS clones");
    // The clone replaced the empty temporary file.
    assert_eq!(names(&inputs), [temp.path()]);
    // materialize keeps a clone only if the object's stamp is unchanged.
    assert_eq!(Stamp::of(&source).unwrap(), stamp);
    assert_eq!(temp.as_file().metadata().unwrap().len(), stamp.len);
    assert_eq!(fs::read(temp.path()).unwrap(), b"retained guest bytes");
    // The backend's writes and mode changes stay in its own file.
    fs::write(temp.path(), b"backend changed its private input").unwrap();
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o500)).unwrap();
    assert_eq!(store.read(&digest).unwrap(), b"retained guest bytes");
    assert_eq!(Stamp::of(&source).unwrap(), stamp);
    assert_eq!(source.metadata().unwrap().permissions().mode(), mode);
}

#[test]
fn clones_across_volumes_fall_back_to_the_empty_temporary_file() {
    // Temporary files live on the data volume, never the sealed system volume.
    let source = fs::File::open("/bin/sh").unwrap();
    let directory = tempfile::tempdir().unwrap();
    let mut temp = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
    let empty = temp.path().to_owned();
    assert!(!clone_file(&source, &mut temp).unwrap());
    assert_eq!(names(directory.path()), [empty]);
    assert_eq!(temp.as_file().metadata().unwrap().len(), 0);
}

#[test]
fn a_clone_that_cannot_be_opened_is_removed() {
    // SAFETY: geteuid has no pointer arguments.
    let euid = unsafe { libc::geteuid() };
    assert_ne!(euid, 0, "root bypasses the mode this test relies on");
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source");
    fs::write(&path, b"bytes").unwrap();
    let source = fs::File::open(&path).unwrap();
    // A clone copies its source's mode, so this one cannot be opened to read.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o200)).unwrap();
    let clones = directory.path().join("clones");
    fs::create_dir(&clones).unwrap();
    let mut temp = tempfile::NamedTempFile::new_in(&clones).unwrap();
    let empty = temp.path().to_owned();
    let error = clone_file(&source, &mut temp).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(names(&clones), [empty]);
}
