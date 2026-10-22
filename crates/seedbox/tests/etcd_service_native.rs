//! Preparation smoke ONLY: 4 writes, 1 worker, 180-second guest write deadline.
//! Canonical examples/etcd remains byte-identical; bd-207 acceptance must run it
//! unchanged, not substitute this smoke test. The existing linux-apps suite
//! supplies a complete release CLI, OCI engine and caller-owned report root.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod etcd_native;
mod support {
    pub mod etcd_project;
    pub mod failure_capture;
    pub mod json_diff;
}
use etcd_project::Backend;
use support::etcd_project;

#[test]
fn pv_etcd_preparation_smoke_through_normal_cli_service() {
    // Temporary PR348 reproduction: a single match does not exclude #304.
    // Each round owns a fresh server and checks independent fresh/replay runs.
    for round in 1..=5 {
        eprintln!("PR348 PV reproduction round {round}/5");
        etcd_native::run(Backend::Pv);
    }
}

// Exercise the same preparation smoke through the real software backend.
#[test]
fn tcg_etcd_preparation_smoke_through_normal_cli_service() {
    etcd_native::run(Backend::Tcg);
}
