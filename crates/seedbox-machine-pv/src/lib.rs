//! PV machine implementation.
pub mod machine;
#[cfg(feature = "test-support")]
pub mod test_support;

/// Serve the private PV worker protocol on process stdin/stdout until EOF.
/// Call only after recognizing the private worker launch marker, before public
/// CLI or application initialization. This entry parses no arguments or environment.
/// EOF succeeds; protocol/worker failures are returned for the executable to print
/// and map to its exit status. Unsupported hosts return an error without serving.
// The library owns native execution and its protocol; the executable owns
// launch recognition, diagnostics, exit status and native signing.
pub fn serve_worker() -> Result<(), String> {
    #[cfg(seedbox_pv)]
    {
        machine::pv::serve_worker()
    }
    #[cfg(not(seedbox_pv))]
    {
        Err("PV worker requires arm64 macOS/HVF or arm64/x86-64 Linux/KVM".into())
    }
}

// Test-only paths for the shared clock and RNG fixtures.
#[cfg(test)]
mod clock {
    pub use seedbox_machine::clock::*;
}
#[cfg(test)]
mod rng {
    pub use seedbox_machine::rng::*;
    pub use seedbox_machine::test_support::SeededRng;
}
