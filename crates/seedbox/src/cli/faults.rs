//! Fault input belongs at the client boundary; live machine and handle admission
//! remains authoritative in the stopped engine world.
use seedbox_application::{Error, Result};
use std::{io::Read, path::Path};

const MAX_ACTION_BYTES: u64 = 64 * 1024;

/// Read and validate a fault profile before connecting to the server.
pub(super) fn profile(
    path: Option<&Path>,
) -> Result<Option<seedbox_spec::fault_profile::FaultProfile>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let path = super::defaults::existing("--fault-profile", path.to_owned())?;
    let error =
        |message: String| Error::Setup(format!("--fault-profile {}: {message}", path.display()));
    let mut bytes = Vec::new();
    std::fs::File::open(&path)
        .and_then(|file| file.take(MAX_ACTION_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|e| error(e.to_string()))?;
    if bytes.len() as u64 > MAX_ACTION_BYTES {
        return Err(error(format!("exceeds {MAX_ACTION_BYTES} bytes")));
    }
    let profile: seedbox_spec::fault_profile::FaultProfile =
        serde_yaml_ng::from_slice(&bytes).map_err(|e| error(e.to_string()))?;
    profile.validate().map_err(error)?;
    Ok(Some(profile))
}

pub(super) fn inject(mutation: super::playback_args::Mutation, path: &Path) -> Result<u8> {
    let mut bytes = Vec::new();
    let read = if path == Path::new("-") {
        std::io::stdin()
            .lock()
            .take(MAX_ACTION_BYTES + 1)
            .read_to_end(&mut bytes)
    } else {
        std::fs::File::open(path)
            .and_then(|file| file.take(MAX_ACTION_BYTES + 1).read_to_end(&mut bytes))
    };
    read.map_err(|error| Error::Setup(format!("fault {}: {error}", path.display())))?;
    if bytes.len() as u64 > MAX_ACTION_BYTES {
        return Err(Error::Setup(format!(
            "fault {}: exceeds {MAX_ACTION_BYTES} bytes",
            path.display()
        )));
    }
    let action: seedbox_api::FaultAction = serde_yaml_ng::from_slice(&bytes)
        .map_err(|error| Error::Setup(format!("fault {}: {error}", path.display())))?;
    super::simulations::mutate_control(
        mutation,
        seedbox_api::ControlCommand::InjectFault { action },
    )
}
