//! Standalone Seedbox world worker: flags (or a CLI launch request) in, the
//! engine report on stdout.

use clap::Parser;
use seedbox_engine_core::launch::MAX_MACHINE_WORKERS;
use seedbox_vmm::{DebugEnv, MachineFeatures, Performance, WorkerRequest};
use std::path::PathBuf;

/// Run one Seedbox world on KVM and print its engine report as JSON.
#[derive(Debug, Parser)]
#[command(name = "seedbox-vmm")]
struct Args {
    /// Launch request JSON written by the Seedbox CLI; conflicts with every
    /// other flag.
    #[arg(long, exclusive = true)]
    request: Option<PathBuf>,
    /// World JSON.
    #[arg(long, required_unless_present = "request")]
    config: Option<PathBuf>,
    /// Run seed.
    #[arg(long, required_unless_present = "request")]
    seed: Option<u64>,
    /// Further comma-separated seeds released from the same booted world.
    #[arg(long, value_delimiter = ',')]
    fork_seeds: Vec<u64>,
    /// Output directory.
    #[arg(long, required_unless_present = "request")]
    out: Option<PathBuf>,
    /// Write full-state page digests.
    #[arg(long)]
    digest: bool,
    /// Host control socket path.
    #[arg(long)]
    host_control: Option<PathBuf>,
    /// Private live observation datagram socket.
    #[arg(long)]
    observations: Option<PathBuf>,
    /// Bytes retained per console stream.
    #[arg(long)]
    console_limit: Option<usize>,
    /// Chunk index entries retained per console stream.
    #[arg(long)]
    log_chunk_limit: Option<usize>,
    /// Program the guest timer through the time page.
    #[arg(long)]
    page_timer: bool,
    /// Defer virtio-net TX kicks to deterministic polls.
    #[arg(long)]
    deferred_tx: bool,
    /// Comma-separated runtime checkpoint times, strictly increasing.
    #[arg(long, value_delimiter = ',')]
    checkpoint_at_ns: Vec<u64>,
    /// Write raw console logs while the world runs.
    #[arg(long)]
    stream_console: bool,
    /// Write per-machine execution traces.
    #[arg(long)]
    trace: bool,
    /// Machine threads for the windowed scheduler (performance only).
    #[arg(long, default_value_t = 1,
          value_parser = clap::value_parser!(u64).range(1..=MAX_MACHINE_WORKERS as u64))]
    machine_workers: u64,
}

impl Args {
    fn request(self) -> Result<WorkerRequest, String> {
        if let Some(path) = self.request {
            let bytes =
                std::fs::read(&path).map_err(|e| format!("--request {}: {e}", path.display()))?;
            return serde_json::from_slice(&bytes)
                .map_err(|e| format!("--request {}: {e}", path.display()));
        }
        let request = WorkerRequest {
            // Present unless --request was given (clap enforces it).
            config: self.config.unwrap_or_default(),
            seed: self.seed.unwrap_or_default(),
            fork_seeds: self.fork_seeds,
            out: self.out.unwrap_or_default(),
            digest: self.digest,
            host_control: self.host_control,
            interventions: None,
            resume_checkpoint: None,
            observations: self.observations,
            console_limit: self.console_limit,
            log_chunk_limit: self.log_chunk_limit,
            features: MachineFeatures {
                page_timer: self.page_timer,
                deferred_tx: self.deferred_tx,
            },
            checkpoint_at_ns: self.checkpoint_at_ns,
            machine_workers: self.machine_workers as usize,
            stream_console: self.stream_console,
            trace: self.trace,
            performance: Performance::default(),
            windowed_skew_us: 0,
        };
        request.validate()?;
        Ok(request)
    }
}

fn run(args: Args) -> Result<seedbox_vmm::RunReport, String> {
    let mut request = args.request()?;
    let trace = DebugEnv::from_env(|name| std::env::var_os(name))?.apply(&mut request);
    seedbox_vmm::run(&request, trace)
}

fn main() {
    // Usage errors keep this worker's historical exit status 1.
    let args = Args::try_parse().unwrap_or_else(|error| {
        let code = if error.use_stderr() { 1 } else { 0 };
        let _ = error.print();
        std::process::exit(code);
    });
    match run(args) {
        Ok(v) => println!("{}", serde_json::to_string_pretty(&v).unwrap()),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<WorkerRequest, String> {
        Args::try_parse_from(std::iter::once("seedbox-vmm").chain(args.iter().copied()))
            .map_err(|e| e.to_string())?
            .request()
    }

    const BASE: [&str; 6] = ["--config", "world.json", "--seed", "7", "--out", "output"];

    fn with(extra: &[&str]) -> Result<WorkerRequest, String> {
        parse(&[&BASE[..], extra].concat())
    }

    #[test]
    fn flags_build_the_launch_request() {
        let request = with(&[
            "--fork-seeds",
            "8,9",
            "--digest",
            "--host-control",
            "control.sock",
            "--console-limit",
            "123",
            "--log-chunk-limit",
            "456",
            "--stream-console",
            "--trace",
            "--page-timer",
            "--deferred-tx",
            "--checkpoint-at-ns",
            "100,200",
        ])
        .unwrap();
        let mut expected = WorkerRequest::new("world.json".into(), 7, "output".into());
        expected.fork_seeds = vec![8, 9];
        expected.digest = true;
        expected.host_control = Some("control.sock".into());
        expected.console_limit = Some(123);
        expected.log_chunk_limit = Some(456);
        expected.features = MachineFeatures {
            page_timer: true,
            deferred_tx: true,
        };
        expected.checkpoint_at_ns = vec![100, 200];
        expected.stream_console = true;
        expected.trace = true;
        assert_eq!(request, expected);
        assert_eq!(
            with(&[]).unwrap(),
            WorkerRequest::new("world.json".into(), 7, "output".into())
        );
    }

    #[test]
    fn a_flag_in_a_value_slot_is_not_a_boolean_flag() {
        // `--host-control --digest` names a socket called "--digest" or fails;
        // either way digest stays off.
        if let Ok(request) = with(&["--host-control", "--digest"]) {
            assert!(!request.digest);
        }
    }

    #[test]
    fn invalid_values_are_usage_errors() {
        assert!(parse(&[]).unwrap_err().contains("--config"));
        assert!(with(&["--seed", "invalid"]).unwrap_err().contains("--seed"));
        assert!(with(&["--fork-seeds", "8,invalid"])
            .unwrap_err()
            .contains("--fork-seeds"));
        assert!(with(&["--console-limit", "x"])
            .unwrap_err()
            .contains("--console-limit"));
        assert!(with(&["--checkpoint-at-ns"])
            .unwrap_err()
            .contains("--checkpoint-at-ns"));
        assert!(with(&["--machine-workers", "0"])
            .unwrap_err()
            .contains("--machine-workers"));
        let o = with(&["--machine-workers", "4"]).unwrap();
        assert_eq!(o.machine_workers, 4);
        assert!(with(&["--machine-cpus", "4,5"])
            .unwrap_err()
            .contains("--machine-cpus"));
    }

    #[test]
    fn a_request_file_replaces_every_flag() {
        let directory = std::env::temp_dir().join(format!("seedbox-vmm-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("request.json");
        let mut request = WorkerRequest::new("w.json".into(), 3, "o".into());
        request.features.deferred_tx = true;
        std::fs::write(&path, serde_json::to_vec(&request).unwrap()).unwrap();
        let path = path.to_str().unwrap();
        assert_eq!(parse(&["--request", path]).unwrap(), request);
        assert!(parse(&["--request", path, "--seed", "4"]).is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
