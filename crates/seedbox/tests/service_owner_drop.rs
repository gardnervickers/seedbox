//! Process-only service-owner abort regression using the real CLI monitor and
//! anchor entries. No VM, native witness, KVM, or retained guest fixture is used.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]
use seedbox_api::{ExecutionTermination, Phase, SimulationSpec, SubmitRequest};
use seedbox_application::{
    execution::{
        CancellationToken, ExecutionBackend, ExecutionProfile, ExecutionReport, ExecutionRequest,
        PreparedImage, PreparedInputs, SemanticParameters, StopReason, WitnessMode,
    },
    image::Launch,
    store::ContentStore,
};
use seedbox_driver_execution::process_lifetime::{
    reclaim_barrier, LeasePaths, OwnedExecution, EXECUTION_LEASE_FILE, SESSION_LEASE_FILE,
};
use seedbox_server::{
    catalogue::Catalogue,
    execution::{BackendProvider, Backends, ExecutionService},
    manager::Manager,
};
use std::{
    collections::BTreeMap,
    ffi::CString,
    fs,
    io::{BufRead, BufReader},
    num::NonZeroUsize,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{ffi::OsStrExt, net::UnixStream},
    },
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::oneshot;

struct PidFd(OwnedFd);
impl PidFd {
    fn open(pid: i32) -> Self {
        // SAFETY: pidfd_open returns a new descriptor for this live fixture pid.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        assert!(
            fd >= 0,
            "pidfd_open({pid}): {}",
            std::io::Error::last_os_error()
        );
        Self(unsafe { OwnedFd::from_raw_fd(fd as i32) })
    }
    fn exited(&self, wait: bool) -> bool {
        let mut event = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            // A bounded infrastructure hang guard, not a process-speed oracle.
            // Readiness of the owned pidfd, never a sleep, establishes death.
            let result = unsafe { libc::poll(&mut event, 1, if wait { 30_000 } else { 0 }) };
            if result < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
                continue;
            }
            assert!(
                result >= 0,
                "poll pidfd: {}",
                std::io::Error::last_os_error()
            );
            return result == 1 && event.revents & libc::POLLIN != 0;
        }
    }
}
struct Processes {
    worker: PidFd,
    anchor: PidFd,
    monitor: PidFd,
}
struct Entered {
    cancellation: CancellationToken,
    scratch: PathBuf,
    lease: PathBuf,
    processes: Arc<Processes>,
}
struct Receipt {
    completion: serde_json::Value,
    group_dead_before_receipt: bool,
    scratch_kept_before_receipt: bool,
}
struct Scratch {
    directory: Option<tempfile::TempDir>,
    lease: PathBuf,
    removed: Arc<AtomicBool>,
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let directory = self.directory.take().unwrap();
        // The actual monitor must have reaped the group and released the lease
        // before its receipt permits a scratch guard to delete worker inputs.
        let Ok(_barrier) = reclaim_barrier(&self.lease, Duration::ZERO) else {
            let path = directory.keep();
            panic!(
                "scratch removal lacked exit-barrier authority; preserved {}",
                path.display()
            );
        };
        directory.close().unwrap();
        self.removed.store(true, Ordering::Release);
    }
}
struct Provider {
    session: PathBuf,
    entered: Arc<Mutex<Option<oneshot::Sender<Entered>>>>,
    release: Arc<Mutex<mpsc::Receiver<()>>>,
    receipt: Arc<Mutex<Option<oneshot::Sender<Receipt>>>>,
    removed: Arc<AtomicBool>,
    dropped: Option<oneshot::Sender<()>>,
}
impl Drop for Provider {
    fn drop(&mut self) {
        if let Some(sender) = self.dropped.take() {
            let _ = sender.send(());
        }
    }
}
impl BackendProvider for Provider {
    fn project(
        &self,
        _: &seedbox_application::config::Simulation,
        _: seedbox_spec::backend::CpuBackend,
        _: &ContentStore,
        _: &CancellationToken,
    ) -> seedbox_application::Result<Backends> {
        panic!("resolved process fixture never prepares images")
    }
    fn retained(
        &self,
        inputs: &PreparedInputs,
        _: &ContentStore,
    ) -> seedbox_application::Result<Box<dyn ExecutionBackend>> {
        Ok(Box::new(ProcessBackend {
            profile: inputs.profile.clone(),
            session: self.session.clone(),
            entered: self.entered.clone(),
            release: self.release.clone(),
            receipt: self.receipt.clone(),
            removed: self.removed.clone(),
        }))
    }
}
struct ProcessBackend {
    profile: ExecutionProfile,
    session: PathBuf,
    entered: Arc<Mutex<Option<oneshot::Sender<Entered>>>>,
    release: Arc<Mutex<mpsc::Receiver<()>>>,
    receipt: Arc<Mutex<Option<oneshot::Sender<Receipt>>>>,
    removed: Arc<AtomicBool>,
}
impl ExecutionBackend for ProcessBackend {
    fn profile(&self) -> ExecutionProfile {
        self.profile.clone()
    }
    fn execute(
        &self,
        request: ExecutionRequest<'_>,
    ) -> seedbox_application::Result<ExecutionReport> {
        let entered = self.entered.lock().unwrap().take().unwrap();
        let receipt = self.receipt.lock().unwrap().take().unwrap();
        assert!(request.start_paused);
        let scratch = tempfile::Builder::new()
            .prefix("native-scratch-")
            .tempdir_in(request.output)?;
        let scratch_path = scratch.path().to_owned();
        let lease = request.output.join(EXECUTION_LEASE_FILE);
        let fifo = scratch_path.join("paused.fifo");
        let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let (reader, writer) = UnixStream::pair()?;
        let script = "mkdir \"$1/seedbox-pv-fixture\"; printf kernel >\"$1/seedbox-pv-fixture/kernel\"; printf initrd >\"$1/seedbox-pv-fixture/initrd\"; printf '%s %s\\n' \"$$\" \"$PPID\"; read line <\"$2\"";
        let mut process = OwnedExecution::spawn(
            std::path::Path::new(env!("CARGO_BIN_EXE_seedbox")),
            std::path::Path::new("/bin/sh"),
            &[
                "-c".into(),
                script.into(),
                "paused-process-fixture".into(),
                scratch_path.clone().into_os_string(),
                fifo.into_os_string(),
            ],
            &[("PATH".into(), std::env::var_os("PATH").unwrap())],
            fs::File::from(OwnedFd::from(writer)),
            fs::File::create(request.output.join("process-stderr"))?,
            LeasePaths {
                execution: &lease,
                session: Some(&self.session),
            },
        )?;
        process.retain_until_join(Scratch {
            directory: Some(scratch),
            lease: lease.clone(),
            removed: self.removed.clone(),
        });
        let mut ready = String::new();
        BufReader::new(reader).read_line(&mut ready)?;
        let pids: Vec<i32> = ready
            .split_whitespace()
            .map(|value| value.parse().unwrap())
            .collect();
        assert_eq!(
            pids.len(),
            2,
            "worker must acknowledge scratch creation before its FIFO wait"
        );
        let anchor = unsafe { libc::getpgid(pids[0]) };
        assert!(anchor > 0 && anchor != pids[0]);
        let processes = Arc::new(Processes {
            worker: PidFd::open(pids[0]),
            monitor: PidFd::open(pids[1]),
            anchor: PidFd::open(anchor),
        });
        // Hold one additional shared lease through group death. This deliberately
        // withholds cleanup authority, without disabling or bypassing any lease.
        let hold = fs::OpenOptions::new().read(true).write(true).open(&lease)?;
        assert_eq!(
            unsafe { libc::flock(hold.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
            0
        );
        assert!(scratch_path.join("seedbox-pv-fixture/initrd").is_file());
        entered
            .send(Entered {
                cancellation: request.cancellation.clone(),
                scratch: scratch_path.clone(),
                lease: lease.clone(),
                processes: processes.clone(),
            })
            .unwrap_or_else(|_| panic!("owner disappeared before fixture entry"));
        self.release.lock().unwrap().recv().unwrap();
        assert!(request.cancellation.is_cancelled());
        process.cancel();
        assert!(processes.worker.exited(true));
        assert!(processes.anchor.exited(true));
        let group_dead_before_receipt = !processes.monitor.exited(false);
        let scratch_kept_before_receipt = scratch_path.join("seedbox-pv-fixture/initrd").exists()
            && !self.removed.load(Ordering::Acquire);
        assert!(
            group_dead_before_receipt,
            "monitor cannot complete while the extra lease is held"
        );
        assert!(
            scratch_kept_before_receipt,
            "scratch disappeared before cleanup authority"
        );
        drop(hold);
        let completion = process.finish()?;
        assert!(completion.cleanup_authorized(), "{completion:?}");
        let observation = Receipt {
            completion: serde_json::to_value(completion).unwrap(),
            group_dead_before_receipt,
            scratch_kept_before_receipt,
        };
        assert!(processes.monitor.exited(true));
        assert!(self.removed.load(Ordering::Acquire));
        assert!(!scratch_path.exists());
        receipt
            .send(observation)
            .unwrap_or_else(|_| panic!("owner stopped awaiting cleanup"));
        Ok(ExecutionReport {
            reason: StopReason::Cancelled,
            workload_exit: None,
            detail: None,
            witness: None,
            full_state: None,
            faults: vec![],
            host_wait_ns: 0,
            simulated_time_ns: Some(42),
            properties: None,
        })
    }
}
struct FailureCleanup {
    manager: Arc<Manager>,
    release: mpsc::Sender<()>,
}
impl Drop for FailureCleanup {
    fn drop(&mut self) {
        let _ = self.manager.cancel_active();
        let _ = self.release.send(());
    }
}

fn request(store: &ContentStore) -> SubmitRequest {
    let binary = store.put(b"process-only-catalogue-input").unwrap();
    let inputs = PreparedInputs {
        machine_memory_mb: std::collections::BTreeMap::from([("oracle".into(), 128)]),
        wall_clock_epoch: Some(86_400),
        images: BTreeMap::from([(
            "oracle".into(),
            PreparedImage {
                manifest: binary.clone(),
                config: binary.clone(),
                layers: vec![],
                launch: Launch {
                    argv: vec!["/unused".into()],
                    environment: BTreeMap::new(),
                    working_dir: "/".into(),
                    user: "0:0".into(),
                    volumes: vec![],
                    disk_volumes: vec![],
                },
            },
        )]),
        guests: BTreeMap::from([("oracle".into(), binary.clone())]),
        placement: BTreeMap::from([("oracle".into(), "oracle".into())]),
        profile: ExecutionProfile {
            backend: "owned-process-host-fixture".into(),
            witness_mode: WitnessMode::Exits,
            semantic_parameters: SemanticParameters::new(),
        },
        auxiliary: BTreeMap::new(),
        provenance: BTreeMap::new(),
    };
    let configuration = seedbox_application::config::parse(
        "name: process-fixture\nmachines: {}\noracle:\n  image: fixture/unused\n",
        std::path::Path::new("/fixture/seedbox.yaml"),
        &Default::default(),
    )
    .unwrap();
    let definition =
        seedbox_application::definition::resolve(&configuration, &inputs, 123, vec![]).unwrap();
    let mut request = SubmitRequest {
        request_id: "paused-owned-execution".into(),
        metadata: Default::default(),
        options: Default::default(),
        spec: SimulationSpec::resolved(definition).unwrap(),
    };
    request.options.start_paused = true;
    request
}

#[tokio::test]
async fn aborting_execution_owner_reaps_worker_monitor_anchor_before_native_scratch_cleanup() {
    // Hang guard only. Readiness, cancellation and death all use channels,
    // FIFO blocking, pidfd readiness and the actual authorized monitor receipt.
    tokio::time::timeout(Duration::from_secs(60), async {
        let root = tempfile::tempdir().unwrap().keep();
        let session = root.join(SESSION_LEASE_FILE);
        fs::write(&session, []).unwrap();
        let store = ContentStore::new(root.join("store"));
        let request = request(&store);
        let manager = Arc::new(Manager::new(Catalogue::in_memory()));
        let id = manager.submit(request).unwrap().0.metadata.id;
        let (entered_send, entered) = oneshot::channel();
        let (release, receiver) = mpsc::channel();
        let (receipt_send, receipt) = oneshot::channel();
        let (dropped_send, dropped) = oneshot::channel();
        let removed = Arc::new(AtomicBool::new(false));
        let service = Arc::new(
            ExecutionService::new(
                manager.clone(),
                root.join("execution"),
                store,
                Arc::new(Provider {
                    session: session.clone(),
                    entered: Arc::new(Mutex::new(Some(entered_send))),
                    release: Arc::new(Mutex::new(receiver)),
                    receipt: Arc::new(Mutex::new(Some(receipt_send))),
                    removed: removed.clone(),
                    dropped: Some(dropped_send),
                }),
            )
            .unwrap(),
        );
        let weak = Arc::downgrade(&service);
        let _failure_cleanup = FailureCleanup {
            manager: manager.clone(),
            release: release.clone(),
        };
        let (sender, queue) = tokio::sync::mpsc::channel(1);
        sender.send(id.clone()).await.unwrap();
        let shutdown = CancellationToken::default();
        let owner = tokio::spawn(
            service
                .clone()
                .run(queue, NonZeroUsize::MIN, shutdown.clone()),
        );
        let entered = entered.await.unwrap();
        assert!(!entered.cancellation.is_cancelled());
        assert!(entered.scratch.exists());
        assert!(!removed.load(Ordering::Acquire));
        assert!(!entered.processes.worker.exited(false));
        assert!(!entered.processes.anchor.exited(false));
        assert!(!entered.processes.monitor.exited(false));
        assert!(reclaim_barrier(&entered.lease, Duration::ZERO).is_err());
        assert!(reclaim_barrier(&session, Duration::ZERO).is_err());
        owner.abort();
        assert!(owner.await.unwrap_err().is_cancelled());
        assert!(
            entered.cancellation.is_cancelled(),
            "owner abort must cancel the paused real process backend"
        );
        assert!(shutdown.is_cancelled());
        release.send(()).unwrap();
        drop(service);
        let receipt = receipt.await.unwrap();
        assert!(receipt.group_dead_before_receipt && receipt.scratch_kept_before_receipt);
        assert_eq!(receipt.completion["barrier"], "Released");
        let mut watch = manager.watch(&id).unwrap();
        while watch.borrow().status.phase != Phase::Completed {
            watch.changed().await.unwrap();
        }
        assert_eq!(
            watch.borrow().status.termination,
            Some(ExecutionTermination::Stopped)
        );
        dropped.await.unwrap();
        assert!(weak.upgrade().is_none());
        assert!(
            entered.processes.worker.exited(false)
                && entered.processes.anchor.exited(false)
                && entered.processes.monitor.exited(false)
        );
        assert!(removed.load(Ordering::Acquire) && !entered.scratch.exists());
        let _execution_barrier = reclaim_barrier(&entered.lease, Duration::ZERO).unwrap();
        let _session_barrier = reclaim_barrier(&session, Duration::ZERO).unwrap();
        fs::remove_dir_all(root).unwrap();
    })
    .await
    .unwrap();
}
