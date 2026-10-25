//! Fault input belongs at the client boundary; live machine and handle admission
//! remains authoritative in the stopped engine world.
use seedbox_application::{Error, Result};
use std::{io::Read, path::Path};

const MAX_ACTION_BYTES: u64 = 64 * 1024;

pub(super) fn inject(
    server: &super::server_args::ServerArgs,
    mutation: super::playback_args::Mutation,
    path: &Path,
) -> Result<u8> {
    let action: seedbox_api::FaultAction = read_document("fault", path)?;
    super::simulations::mutate_control(
        server,
        mutation,
        seedbox_api::ControlCommand::InjectFault { action },
    )
}

/// A fault profile file, parsed and checked before anything is prepared.
pub(super) fn profile(path: &Path) -> Result<seedbox_api::FaultProfile> {
    let profile: seedbox_api::FaultProfile = read_document("fault profile", path)?;
    profile.validate().map_err(|error| {
        let error = error.strip_prefix("fault profile ").unwrap_or(&error);
        Error::Setup(format!("fault profile {}: {error}", path.display()))
    })?;
    Ok(profile)
}

/// One bounded YAML or JSON document from a file, or `-` for standard input.
fn read_document<T: serde::de::DeserializeOwned>(what: &str, path: &Path) -> Result<T> {
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
    read.map_err(|error| Error::Setup(format!("{what} {}: {error}", path.display())))?;
    if bytes.len() as u64 > MAX_ACTION_BYTES {
        return Err(Error::Setup(format!(
            "{what} {}: exceeds {MAX_ACTION_BYTES} bytes",
            path.display()
        )));
    }
    serde_yaml_ng::from_slice(&bytes)
        .map_err(|error| Error::Setup(format!("{what} {}: {error}", path.display())))
}
