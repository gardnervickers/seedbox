pub mod backend;
pub mod config;
pub mod error;
pub mod fixtures;
pub mod kernel;
mod linux;
pub mod linux_errno;
mod network;
pub mod random;
pub mod runtime;
pub mod verification;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
