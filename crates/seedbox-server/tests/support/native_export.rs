//! Real three-machine PV closure export; no host-only snapshot payloads.
use super::*;
use seedbox_api::{CheckpointState, ControlCommand, ControlState, InputSource, PlaybackRate};
use std::sync::atomic::{AtomicBool, AtomicU64};

struct CpuLoad {
    stop: Arc<AtomicBool>,
    work: Arc<AtomicU64>,
    threads: Vec<std::thread::JoinHandle<()>>,
}
impl CpuLoad {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let work = Arc::new(AtomicU64::new(0));
        let threads = (0..2)
            .map(|lane| {
                let stop = stop.clone();
                let work = work.clone();
                std::thread::spawn(move || {
                    let mut value = lane + 1u64;
                    while !stop.load(Ordering::Acquire) {
                        for _ in 0..4096 {
                            value = std::hint::black_box(
                                value.wrapping_mul(6364136223846793005).wrapping_add(1),
                            );
                        }
                        work.fetch_add(4096, Ordering::Relaxed);
                    }
                    std::hint::black_box(value);
                })
            })
            .collect();
        Self {
            stop,
            work,
            threads,
        }
    }
}
impl Drop for CpuLoad {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for thread in self.threads.drain(..) {
            thread.join().unwrap();
        }
    }
}

pub async fn pv_large_export_under_load() {
    // New size/load scenario: 600 seconds is only a failure/hang guard, not an
    // expected duration or completion oracle. Existing native budgets unchanged.
    native_case_with_guard(Duration::from_secs(600), |owner| Box::pin(async move {
        let session = owner.session(CpuBackend::Pv);
        let mut request = submission_with_machines(CpuBackend::Pv, session.root.path(), &session.store, 2_000_000, 123, vec!["export-one", "export-two"]);
        request.options.start_paused = true;
        let source = session.submit(request, 1).await;
        session.worker(&source).await;
        session.control(&source, "verify-paused-source", ControlCommand::SetRate { rate: PlaybackRate::Paused }).await;
        let launch = session.launch_request(&source);
        assert_eq!(launch.prepared.machines.len() + 1, 3, "actual retained native launch must have three machines");
        session.control(&source, "initialize-three-machines", ControlCommand::Step).await;
        loop {
            let state = session.client.playback(&source).await.unwrap().state;
            assert!(!state.terminal, "source ended before native capture");
            check_bound(
                &session.client,
                &source,
                CpuBackend::Pv,
                state.position.simulated_time_ns,
                "its three-machine capture point",
            )
            .await;
            if !state.stepping && state.position.completed_rounds > 0 { break; }
            tokio::task::yield_now().await;
        }
        let captured = session.control(&source, "saved", ControlCommand::Checkpoint).await;
        assert!(matches!(captured.state, ControlState::Acknowledged { checkpoint: Some(c), error: None, .. } if c.state == CheckpointState::Captured));
        let load = CpuLoad::start();
        let probe_client = session.client.clone();
        let probe_stop = load.stop.clone();
        let probes = tokio::spawn(async move {
            let mut count = 0u64;
            while !probe_stop.load(Ordering::Acquire) {
                probe_client.info().await.unwrap();
                count += 1;
                tokio::task::yield_now().await;
            }
            count
        });

        let exported = session.client.checkpoint_definition(&source, "saved").await;
        load.stop.store(true, Ordering::Release);
        let probes = probes.await.unwrap();

        let cpu_work = load.work.load(Ordering::Acquire);
        drop(load);
        let exported = exported.unwrap();
        assert!(probes > 0 && cpu_work > 0, "load must overlap export");
        let InputSource::Resolved { definition } = &exported.spec.inputs else { panic!("resolved export required") };
        assert_eq!(definition.machines.len() + 1, 3);
        let seedbox_spec::definition::Start::Checkpoint { manifest, assets } = &definition.start else { panic!("checkpoint export required") };
        let machines = manifest["machines"].as_array().unwrap();
        assert_eq!(machines.len(), 3);
        let machine_bytes: u64 = machines.iter().map(|machine| machine["length"].as_u64().unwrap()).sum();
        assert!(machine_bytes >= 500_000_000, "actual native snapshot bodies only {machine_bytes} bytes");
        assert!(assets.len() >= 3);
        let digests: std::collections::BTreeSet<_> = assets.values().map(|asset| {
            ContentDigest::try_from(asset.0.strip_prefix("sha256:").unwrap().to_owned()).unwrap()
        }).collect();
        let mut bytes = 0u64;
        for digest in digests {
            bytes += session.store.size(&digest).unwrap();
            session.store.copy_to(&digest, &mut std::io::sink()).unwrap();
        }
        assert!(bytes >= 500_000_000, "native closure only {bytes} bytes");
        eprintln!("large public-client PV export: machines=3 native_snapshot_bytes={machine_bytes} unique_closure_bytes={bytes} cpu_load_threads=2 cpu_iterations={cpu_work} concurrent_api_probes={probes}");
        session.client.stop(&source).await.unwrap();
        session.complete(&source, true, 1, 123).await;
        session.client.delete(&source).await.unwrap();
    })).await;
}
