//! Run with cargo bench -p seedbox-machine-pv --bench pv_transport --features transport-bench.
//! Append `-- control` for the header-only snapshot request/response benchmark.
fn main() -> Result<(), String> {
    #[cfg(seedbox_pv)]
    return seedbox_machine_pv::machine::pv::benchmark_transport();
    #[cfg(not(seedbox_pv))]
    Err("PV transport benchmark requires a supported PV host".into())
}
