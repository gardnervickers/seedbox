//! Private native worker entrypoint; never used as a public command-line API.
#[path = "../pv/transport.rs"]
pub(crate) mod transport;
mod pv {
    pub(crate) use crate::transport;
}
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
#[path = "../machine/pv/boot.rs"]
mod boot;
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
#[path = "../machine/pv/protocol.rs"]
mod protocol;
#[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
#[path = "../machine/pv/worker/mod.rs"]
mod worker;
fn main() {
    if std::env::args().nth(1).as_deref() != Some("--seedbox-pv-worker-v1") {
        eprintln!("private seedbox PV worker requires its protocol launch");
        std::process::exit(2);
    }
    #[cfg(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux")))]
    if let Err(error) = worker::serve() {
        eprintln!("{error}");
        std::process::exit(2);
    }
    #[cfg(not(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux"))))]
    {
        eprintln!("PV worker requires arm64 macOS/HVF or Linux/KVM");
        std::process::exit(2);
    }
}
