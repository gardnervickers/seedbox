//! Pure configuration and launch planning; no process or filesystem mutation.
use std::{ffi::OsString, path::PathBuf};

pub(super) type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub(super) struct Options {
    pub root: PathBuf,
    pub world: OsString,
    pub prefix: String,
    pub seeds: Vec<String>,
    pub extra: Vec<OsString>,
    pub timeout: f64,
    pub vmm: OsString,
}

pub(super) struct Plan {
    pub output: PathBuf,
    pub args: Vec<OsString>,
}

impl Options {
    #[cfg(target_os = "linux")]
    pub(super) fn from_env() -> Result<Self> {
        Self::parse(std::env::args_os().skip(1), |name| std::env::var_os(name))
    }

    /// Explicit settings make parser/default checks portable and independent
    /// of the concurrent test runner's process environment.
    pub(super) fn parse(
        args: impl IntoIterator<Item = OsString>,
        mut setting: impl FnMut(&str) -> Option<OsString>,
    ) -> Result<Self> {
        let mut args = args.into_iter();
        let world = args
            .next()
            .ok_or("expected WORLD OUTPREFIX 'seed seed ...'")?;
        let prefix = args
            .next()
            .ok_or("expected output prefix")?
            .into_string()
            .map_err(|_| "invalid output prefix")?;
        let seed_arg = args
            .next()
            .ok_or("expected seeds")?
            .into_string()
            .map_err(|_| "invalid seeds")?;
        let seeds = seed_arg
            .split_whitespace()
            .map(|s| s.parse::<i128>().map(|s| s.to_string()))
            .collect::<std::result::Result<_, _>>()?;
        let root = setting("SEEDBOX_ROOT").ok_or("SEEDBOX_ROOT is required")?;
        // pathlib.Path('') meant the current directory in the old supervisor.
        let root = if root.is_empty() {
            PathBuf::from(".")
        } else {
            root.into()
        };
        let timeout = setting("SEEDBOX_TIMEOUT")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "3600".into());
        let vmm = setting("VMM")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "./crates/seedbox-vmm/target/release/seedbox-vmm".into());
        let options = Self {
            root,
            world,
            prefix,
            seeds,
            extra: args.collect(),
            // Python float accepted surrounding whitespace; keep that behavior.
            timeout: timeout
                .into_string()
                .map_err(|_| "invalid timeout")?
                .trim()
                .parse()?,
            vmm,
        };
        options.validate()?;
        Ok(options)
    }

    pub(super) fn validate(&self) -> Result<()> {
        if self.seeds.is_empty() || !self.timeout.is_finite() || self.timeout <= 0.0 {
            return Err("expected nonempty seeds and positive timeout".into());
        }
        Ok(())
    }

    /// Construct every plan before execution can launch or wait for a worker.
    /// Native session creation, completion and status writes remain Linux-owned.
    pub(super) fn plan(&self) -> Vec<Plan> {
        self.seeds
            .iter()
            .enumerate()
            .map(|(index, seed)| {
                let output = self
                    .root
                    .join("runs")
                    .join(format!("{}-s{seed}-r{index}", self.prefix));
                let mut args = vec![
                    "--config".into(),
                    self.world.clone(),
                    "--seed".into(),
                    seed.into(),
                    "--out".into(),
                    output.as_os_str().to_owned(),
                ];
                args.extend(self.extra.iter().cloned());
                Plan { output, args }
            })
            .collect()
    }
}
