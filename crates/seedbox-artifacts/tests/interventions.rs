use seedbox_artifacts::interventions::{self, Intervention, Journal};
use std::{
    fs,
    os::unix::fs::{symlink, PermissionsExt},
};

fn entry() -> Intervention {
    serde_json::from_value(serde_json::json!({
        "requestId":"fault-1",
        "position":{"boundaryIndex":8,"completedRounds":5,"simulatedTimeNs":100},
        "plan":{"id":2,"at_ns":100,"deadline_ns":100,"action":"heal","handle":"network"}
    }))
    .unwrap()
}

#[test]
fn admission_is_durable_exclusively_owned_and_idempotent_after_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("interventions.json");
    let mut journal = Journal::open(&path).unwrap();
    assert!(Journal::open(&path).is_err());
    assert!(!path.exists());
    assert!(journal.append(entry()).unwrap());
    assert_eq!(interventions::read(&path).unwrap(), *journal.history());
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
    drop(journal);
    let mut journal = Journal::open(&path).unwrap();
    let bytes = fs::read(&path).unwrap();
    assert!(!journal.append(entry()).unwrap());
    let mut conflict = entry();
    conflict.plan.id += 1;
    assert!(journal.append(conflict).is_err());
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert_eq!(journal.history().entries, [entry()]);
}

#[test]
fn malformed_and_symlinked_data_are_not_replaced_and_failed_writers_stop() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("interventions.json");
    fs::write(&path, b"broken").unwrap();
    assert!(Journal::open(&path).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"broken");
    fs::remove_file(&path).unwrap();
    let other = directory.path().join("other");
    fs::write(&other, b"preserve").unwrap();
    symlink(&other, &path).unwrap();
    assert!(Journal::open(&path).is_err());
    assert_eq!(fs::read(&other).unwrap(), b"preserve");
    fs::remove_file(&path).unwrap();
    let mut journal = Journal::open(&path).unwrap();
    // A directory prevents atomic replacement, even for a privileged test user.
    fs::create_dir(&path).unwrap();
    assert!(journal.append(entry()).is_err());
    fs::remove_dir(&path).unwrap();
    assert!(journal
        .append(entry())
        .unwrap_err()
        .to_string()
        .contains("writer has failed"));
    assert!(journal.history().entries.is_empty());
    assert!(!path.exists());
}
