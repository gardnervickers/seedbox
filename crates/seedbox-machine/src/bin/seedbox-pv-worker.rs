//! Private native worker entrypoint; never used as a public command-line API.
#[path = "../pv/transport.rs"]
pub(crate) mod transport;
mod pv {
    pub(crate) use crate::transport;
}
#[cfg(seedbox_pv)]
#[path = "../machine/pv/boot.rs"]
mod boot;
#[cfg(seedbox_pv)]
#[path = "../machine/pv/protocol.rs"]
mod protocol;
#[cfg(seedbox_pv)]
#[path = "../machine/pv/worker/mod.rs"]
mod worker;
fn main() {
    if std::env::args().nth(1).as_deref() != Some("--seedbox-pv-worker-v1") {
        eprintln!("private seedbox PV worker requires its protocol launch");
        std::process::exit(2);
    }
    #[cfg(seedbox_pv)]
    if let Err(error) = worker::serve() {
        eprintln!("{error}");
        std::process::exit(2);
    }
    #[cfg(not(seedbox_pv))]
    {
        eprintln!("PV worker requires arm64 macOS/HVF or arm64/x86-64 Linux/KVM");
        std::process::exit(2);
    }
}
