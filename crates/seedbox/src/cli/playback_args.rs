//! Client-side syntax for positional simulation controls and playback rates.
use clap::Args;
use seedbox_api::{PlaybackRate, SimulationId};
use std::{num::NonZeroU32, str::FromStr};

#[derive(Clone, Debug, Args)]
pub(super) struct Mutation {
    /// Short simulation ID.
    pub id: SimulationId,
    /// Reuse this ID to safely retry the same command [default: generated once].
    #[arg(long, value_parser = request_id)]
    pub request_id: Option<String>,
    /// Print JSON instead of YAML.
    #[arg(long)]
    pub json: bool,
}

pub(super) fn request_id(value: &str) -> Result<String, String> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        return Err(
            "request ID must be 1-128 ASCII letters, digits, hyphens or underscores".into(),
        );
    }
    Ok(value.into())
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Rate(pub PlaybackRate);

impl FromStr for Rate {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value == "unlimited" {
            return Ok(Self(PlaybackRate::Unlimited));
        }
        let invalid = || {
            "rate must be 0, a positive integer or fraction (such as 1/10), or unlimited"
                .to_string()
        };
        let (n, d) = value.split_once('/').unwrap_or((value, "1"));
        if !n.bytes().all(|c| c.is_ascii_digit()) || !d.bytes().all(|c| c.is_ascii_digit()) {
            return Err(invalid());
        }
        let numerator: u32 = n.parse().map_err(|_| invalid())?;
        let denominator: NonZeroU32 = d.parse().map_err(|_| invalid())?;
        Ok(Self(match NonZeroU32::new(numerator) {
            None => PlaybackRate::Paused,
            Some(numerator) => PlaybackRate::Limited {
                numerator,
                denominator,
            },
        }))
    }
}
