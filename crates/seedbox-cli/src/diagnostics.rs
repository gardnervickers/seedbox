//! Raw digest-backed diagnostics from completed server resources.
use super::{
    daemon, output,
    signals::Signals,
    simulations::{client_error, runtime},
};
use seedbox_api::SimulationId;
use seedbox_application::{execution::CancellationToken, Error, Result};
use std::io::{Read, Seek};

pub(super) fn diagnostics(
    server: &super::server_args::ServerArgs,
    id: &SimulationId,
    name: Option<&str>,
    max_bytes: u64,
    json: bool,
) -> Result<u8> {
    let client = daemon::connect(server)?;
    let token = CancellationToken::default();
    let _signals = Signals::register(&token)?;
    let runtime = runtime()?;
    let report = runtime
        .block_on(client.diagnostics(id, || token.is_cancelled()))
        .map_err(client_error)?;
    if let Some(name) = name {
        let descriptor = report
            .files
            .iter()
            .find(|f| f.name == name)
            .ok_or_else(|| {
                Error::Setup(format!(
                    "diagnostic {name:?}: unavailable; choose an exact name from diagnostics"
                ))
            })?;
        let mut spool =
            tempfile::tempfile().map_err(|e| Error::Setup(format!("diagnostic spool: {e}")))?;
        let mut file = tokio::fs::File::from_std(spool.try_clone()?);
        let size = runtime
            .block_on(
                client.diagnostic_file(id, descriptor, &mut file, max_bytes, || {
                    token.is_cancelled()
                }),
            )
            .map_err(client_error)?;
        if size != descriptor.size_bytes {
            return Err(Error::Setup(
                "diagnostic size changed during transfer".into(),
            ));
        }
        spool.rewind()?;
        if json {
            output::diagnostic_document(descriptor, &report.omissions, &mut spool)?;
        } else {
            for (name, reason) in &report.omissions {
                eprintln!("omission: {}", serde_json::to_string(&(name, reason))?);
            }
            let mut bytes = [0; 64 * 1024];
            loop {
                let count = spool.read(&mut bytes)?;
                if count == 0 {
                    break;
                }
                output::bytes(&bytes[..count])?;
            }
        }
    } else if json {
        output::json(&report)?;
    } else {
        output::table(
            ["NAME", "BYTES", "SHA256"],
            report
                .files
                .iter()
                .map(|file| {
                    [
                        file.name.clone(),
                        file.size_bytes.to_string(),
                        file.sha256.clone(),
                    ]
                })
                .collect(),
        )?;
        for (name, reason) in report.omissions {
            output::field("omission", serde_json::to_string(&(name, reason))?);
        }
    }
    Ok(0)
}
