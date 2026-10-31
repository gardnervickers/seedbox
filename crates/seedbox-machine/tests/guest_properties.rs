//! The production C property relay against the Rust wire decoders.
#![cfg(unix)]
use seedbox_engine_protocol::{crash, property};
use seedbox_test::process::Guarded;
use std::{
    path::{Path, PathBuf},
    process::Command,
};

fn build(directory: &Path) -> PathBuf {
    let executable = directory.join("guest-properties-test");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/pv");
    let compiler = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
    let build = Command::new(compiler)
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(source.join("guest_properties.c"))
        .arg(source.join("guest_properties_test.c"))
        .arg("-o")
        .arg(&executable)
        .guarded_output()
        .unwrap();
    assert!(build.status.success(), "{build:?}");
    executable
}

fn lines(output: &[u8]) -> Vec<&[u8]> {
    output
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .collect()
}

#[test]
fn property_and_crash_frames_decode_on_the_host() {
    let directory = tempfile::tempdir().unwrap();
    let output = Command::new(build(directory.path()))
        .guarded_output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let lines = lines(&output.stdout);
    assert_eq!(lines.len(), 5);
    let frame = property::decode(lines[0]).unwrap().unwrap();
    assert_eq!((frame.session, frame.seq), (3, 17));
    assert_eq!(frame.payload, b"{}");
    let largest = property::decode(lines[1]).unwrap().unwrap();
    assert_eq!((largest.session, largest.seq), (u32::MAX, u64::MAX));
    assert_eq!(largest.payload, vec![0xff; property::MAX_RECORD_BYTES]);
    let reports: Vec<_> = lines[2..]
        .iter()
        .map(|line| crash::decode(line).unwrap().unwrap())
        .collect();
    assert_eq!(
        reports[0].message,
        "libdynamorio.so: keel-watchdog: potentially unexpected fatal signal 6."
    );
    assert_eq!(reports[0].signal, 6);
    assert_eq!(
        reports[1].message,
        "keel-s3dd: potentially unexpected fatal signal 11."
    );
    assert_eq!(
        reports[2].message,
        "a: b\\x5cc: potentially unexpected fatal signal 31."
    );
    assert_eq!(reports[2].signal, 31);
}

#[test]
#[cfg(target_os = "linux")]
fn sessions_are_numbered_and_replies_reach_their_own_session() {
    use std::collections::BTreeSet;
    let directory = tempfile::Builder::new()
        .prefix("sbx-guest-")
        .tempdir_in("/tmp")
        .unwrap();
    let output = Command::new(build(directory.path()))
        .arg("relay")
        .arg(directory.path())
        .guarded_output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let frames: Vec<_> = lines(&output.stdout)
        .into_iter()
        .map(|line| property::decode(line).unwrap().unwrap())
        .collect();
    let session = |n: u32| -> Vec<_> {
        frames
            .iter()
            .filter(|f| f.session == n)
            .map(|f| (f.seq, f.payload.clone()))
            .collect()
    };
    let one = session(1);
    assert_eq!(one[0], (1, b"{\"a\":1}".to_vec()));
    assert_eq!(one[1], (2, b"{\"flush\":{}}".to_vec()));
    // The oversized record took sequence 2 without being relayed.
    let two: Vec<u64> = session(2).iter().map(|(seq, _)| *seq).collect();
    assert_eq!(&two[..2], [1, 3]);
    // Session 3 came through the abstract listener, a budget at a time.
    assert_eq!(session(3).len(), 41);
    assert_eq!(session(3)[0].1, b"{\"e\":5}");
    let supervisor: Vec<_> = frames
        .iter()
        .filter(|f| f.session == 0)
        .map(|f| (f.seq, property::Record::parse(&f.payload).unwrap()))
        .collect();
    assert!(supervisor
        .iter()
        .map(|(seq, _)| *seq)
        .eq(1..=supervisor.len() as u64));
    let lost = |reason: &str| {
        supervisor
            .iter()
            .filter(|(_, r)| matches!(r, property::Record::Lost { reason: why, count: 1 } if why == reason))
            .count()
    };
    assert_eq!(lost("guest record too large"), 1);
    // The first session's closed slot was free again before new connections
    // were counted: only the one connection beyond 64 open was refused.
    assert_eq!(lost("too many guest connections"), 1);
    let closed: BTreeSet<u32> = supervisor
        .iter()
        .filter_map(|(_, r)| match r {
            property::Record::Closed { session } => Some(*session),
            _ => None,
        })
        .collect();
    assert_eq!(closed, (1..=65).collect());
}
