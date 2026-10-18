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
        server,
        mutation,
        seedbox_api::ControlCommand::InjectFault { action },
    )
}
