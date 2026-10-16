//! Driver service machines with a named volume backed by their retained disk.
//! The oracle is the stock `disk-probe volume` role: each round writes a file
//! on the volume, submits it to the device and waits for completion without a
//! FLUSH, sleeps one second of guest time, then fsyncs it; it finally fsyncs a
//! count, reads everything back with O_DIRECT (from the device, not the page
//! cache) and exits 0. Its output depends on the disk's contents surviving
//! every step. The raw witness also covers them: each machine contributes the
//! digest of its disk's durable sectors and unflushed layer.
use super::*;
use seedbox_api::{CheckpointState, ControlCommand, ControlState, InputSource, PlaybackRate};

const ORACLE: &str = "oracle";
const SEED: u64 = 123;
const ROUNDS: &str = "2";
/// FNV-1a of both 64 KiB rounds on a fresh volume (count 0); see
/// tests/fixtures/disk-probe.c. Any lost or misplaced block changes it.
const DONE: &str = "DISK volume done count=1 direct sum=2d1131fc008c0383\n";
const SUBMITTED_FIRST: &str = "DISK volume submitted 1\n";
const SYNCED_FIRST: &str = "DISK volume synced 1\n";

fn probe(backend: DriverBackend) -> Vec<u8> {
    let key = match backend {
        DriverBackend::Pv => "SEEDBOX_PV_DISK_PROBE",
        DriverBackend::Tcg => "SEEDBOX_TCG_DISK_PROBE",
    };
    fs::read(std::env::var_os(key).unwrap_or_else(|| panic!("{key}: disk-probe fixture"))).unwrap()
}
fn request(session: &NativeSession) -> SubmitRequest {
    let binary = probe(session.backend);
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_mode(0o755);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_size(binary.len() as u64);
    header.set_cksum();
    tar.append_data(&mut header, "application", binary.as_slice())
        .unwrap();
    let filesystem = session.root.path().join("disk-probe.tar");
    fs::write(&filesystem, tar.into_inner().unwrap()).unwrap();
    fixture_submission(
        session.backend,
        session.root.path(),
        &session.store,
        SEED,
        NativeFixture {
            name: ORACLE,
            filesystem,
            launch: GuestLaunch {
                argv: ["/application", "volume", "/data", ROUNDS]
                    .map(String::from)
                    .to_vec(),
                environment: BTreeMap::new(),
                working_directory: "/".into(),
                user: "0:0".into(),
            },
            control: false,
            request_id: "disk-volume".into(),
            extra_machines: vec![],
            volumes: vec![seedbox_spec::VolumeMount {
                source: "data".into(),
                target: "/data".into(),
            }],
        },
    )
}
fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}
fn manifest_disk(session: &NativeSession, id: &seedbox_api::SimulationId) {
    let launched = session.launch_request(id);
    assert!(
        matches!(launched.prepared.oracle.disk, Some(wire::Disk::Zero { bytes }) if bytes > 0),
        "launched Driver machine must own a retained disk: {:?}",
        launched.prepared.oracle.disk
    );
}

/// Same input twice, on one and four machine workers: both runs pass with the
/// expected durable volume contents and equal raw/compared witnesses.
pub async fn disk_repeat_workers(backend: DriverBackend) {
    // test-time: allow(hang-guard) -- Shared HANG_GUARD only catches an execution that stops making simulated progress; completion's check_bound is the failure condition and witnesses decide success.
    tokio::time::timeout(HANG_GUARD, async {
        let mut session = NativeSession::new(backend);
        let request = request(&session);
        let mut runs = vec![];
        for workers in [1, 4] {
            let id = session.submit(request.clone(), workers).await;
            session.worker(&id).await;
            let witness = session.complete(&id, false, workers, SEED).await.unwrap();
            manifest_disk(&session, &id);
            let output = text(&session.output(&id).await);
            assert!(output.starts_with("DISK volume boot=0\n"), "{output:?}");
            assert!(output.ends_with(DONE), "{output:?}");
            runs.push((id, witness, output));
        }
        assert_eq!(runs[0].1, runs[1].1, "workers 1/4 raw witnesses differ");
        assert_eq!(runs[0].2, runs[1].2, "workers 1/4 outputs differ");
        let comparison = session
            .client
            .compare(&runs[0].0, &runs[1].0)
            .await
            .unwrap();
        assert_eq!(comparison.status, ComparisonStatus::Match);
        assert_eq!(comparison.witness_matches, Some(true));
        eprintln!(
            "native disk repeat: backend={} witness={}",
            label(backend),
            runs[0].1
        );
        session.close().await;
    })
    .await
    .expect("host hang guard: disk repeat never completed");
}

/// Capture while the first round's completed device write is still unflushed
/// and its sleep pending, export, delete the source, import on four workers and continue:
/// output and raw witness equal the uninterrupted run.
pub async fn disk_checkpoint_continuation(backend: DriverBackend) {
    // test-time: allow(hang-guard) -- Shared HANG_GUARD only catches an execution that stops making simulated progress; check_bound limits the capture search in simulated time and witnesses decide success.
    tokio::time::timeout(HANG_GUARD, async {
        let mut session = NativeSession::new(backend);
        let request = request(&session);
        let baseline = session.submit(request.clone(), 1).await;
        session.worker(&baseline).await;
        let uninterrupted = session.complete(&baseline, false, 1, SEED).await.unwrap();
        let full = text(&session.output(&baseline).await);
        assert!(full.ends_with(DONE), "{full:?}");

        let mut paused = request;
        paused.options.start_paused = true;
        let source = session.submit(paused, 1).await;
        session.worker(&source).await;
        session
            .control(
                &source,
                "pause",
                ControlCommand::SetRate {
                    rate: PlaybackRate::Paused,
                },
            )
            .await;
        let mut step = 0u64;
        let before = loop {
            let rounds = session
                .client
                .playback(&source)
                .await
                .unwrap()
                .state
                .position
                .completed_rounds;
            session
                .control(&source, &format!("step-{step}"), ControlCommand::Step)
                .await;
            step += 1;
            loop {
                let status = session.client.playback(&source).await.unwrap().state;
                assert!(!status.terminal, "probe ended before capture");
                check_bound(
                    &session.client,
                    &source,
                    backend,
                    status.position.simulated_time_ns,
                    "its unflushed-write capture point",
                )
                .await;
                if !status.stepping && status.position.completed_rounds > rounds {
                    break;
                }
                // test-time: allow(hang-guard) -- Yield between positive step-completion polls; check_bound supplies the simulated cap and the committed round count, not this delay, completes the step.
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let output = text(&session.output(&source).await);
            assert!(
                !output.contains(SYNCED_FIRST),
                "capture point passed: {output:?}"
            );
            if output.contains(SUBMITTED_FIRST) {
                break output;
            }
        };
        let captured = session
            .control(&source, "saved", ControlCommand::Checkpoint)
            .await;
        let (rounds, ns) = match &captured.state {
            ControlState::Acknowledged {
                checkpoint: Some(c),
                error: None,
                ..
            } if c.state == CheckpointState::Captured => {
                (c.position.completed_rounds, c.position.simulated_time_ns)
            }
            other => panic!("checkpoint not captured: {other:?}"),
        };
        eprintln!(
            "native disk capture: backend={} rounds={rounds} ns={ns}",
            label(backend)
        );
        let exported = session
            .client
            .checkpoint_definition(&source, "saved")
            .await
            .unwrap();
        assert!(matches!(
            &exported.spec.inputs,
            InputSource::Resolved { .. }
        ));
        session.client.stop(&source).await.unwrap();
        session.complete(&source, true, 1, SEED).await;
        session
            .client
            .delete_checkpoint(&source, "saved")
            .await
            .unwrap();
        session.client.delete(&source).await.unwrap();

        session.workers.store(4, Ordering::SeqCst);
        let restored = session
            .client
            .submit_definition(&serde_json::to_value(exported).unwrap(), true)
            .await
            .unwrap()
            .metadata
            .id;
        session.submitted += 1;
        session.worker(&restored).await;
        assert!(session
            .launch_request(&restored)
            .resume_checkpoint
            .is_some());
        session
            .control(
                &restored,
                "verify-paused-restore",
                ControlCommand::SetRate {
                    rate: PlaybackRate::Paused,
                },
            )
            .await;
        let position = session
            .client
            .playback(&restored)
            .await
            .unwrap()
            .state
            .position;
        assert_eq!(
            (position.completed_rounds, position.simulated_time_ns),
            (rounds, ns)
        );
        session
            .control(
                &restored,
                "continue",
                ControlCommand::SetRate {
                    rate: PlaybackRate::Unlimited,
                },
            )
            .await;
        let continued = session.complete(&restored, false, 4, SEED).await.unwrap();
        let after = text(&session.output(&restored).await);
        assert!(
            !after.contains("DISK volume boot="),
            "restore restarted the probe: {after:?}"
        );
        assert_eq!(format!("{before}{after}"), full, "source + restored output");
        assert_eq!(
            uninterrupted, continued,
            "raw uninterrupted/continued witnesses differ"
        );
        let comparison = session.client.compare(&baseline, &restored).await.unwrap();
        assert_eq!(comparison.witness_matches, Some(true));
        eprintln!(
            "native disk checkpoint continuation: backend={} witness={continued}",
            label(backend)
        );
        session.close().await;
    })
    .await
    .expect("host hang guard: disk checkpoint continuation never completed");
}
