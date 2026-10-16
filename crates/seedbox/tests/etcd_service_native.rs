//! Preparation smoke ONLY: 4 writes, 1 worker, 180-second guest write deadline.
//! Canonical examples/etcd remains byte-identical; bd-207 acceptance must run it
//! unchanged, not substitute this smoke test. The existing linux-apps suite
//! supplies a complete release CLI, OCI engine and caller-owned report root.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[path = "support/etcd_native.rs"]
mod etcd_native;
#[path = "support/etcd_project.rs"]
mod etcd_project;
use etcd_project::Backend;

#[test]
fn pv_etcd_preparation_smoke_through_normal_cli_service() {
    etcd_native::run(Backend::Pv);
}

// Compile the common backend-parameterized path, but do not schedule TCG while
// no approved/implemented normal user route exists. No imaginary flag or PV
// fallback is used. Activation belongs to bd-e9e/bd-207.
#[test]
#[ignore = "normal TCG selection/preparation pending bd-e9e; not scheduled by linux-apps"]
fn tcg_etcd_preparation_smoke_through_normal_cli_service() {
    etcd_native::run(Backend::Tcg);
}
