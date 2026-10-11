//! Delete only the native artifacts explicitly owned by one published capture.
use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

pub(crate) fn remove(attempt: &Path, name: &str) -> io::Result<()> {
    let output = attempt
        .join("output")
        .join(seedbox_application::engine_adapter::MACHINE_OUTPUT_DIR);
    let metadata = output.join(format!("checkpoint-{name}.json"));
    let file = match fs::File::open(&metadata) {
        Ok(file) => Some(file),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    if let Some(file) = file {
        let mut bytes = Vec::new();
        file.take(64 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 64 * 1024 * 1024 {
            return Err(io::Error::other("checkpoint metadata exceeds 64 MiB"));
        }
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        let machines = value
            .pointer("/state/world/machines")
            .and_then(|v| v.as_array())
            .ok_or_else(|| io::Error::other("checkpoint has no machine artifacts"))?;
        let mut artifacts = Vec::new();
        for machine in machines {
            let path = machine
                .get("path")
                .or_else(|| machine.pointer("/checkpoint/artifact"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| io::Error::other("unknown checkpoint artifact format"))?;
            let path = PathBuf::from(path);
            let relative = path.strip_prefix(&output).map_err(|_| {
                io::Error::other("checkpoint artifact is outside its output directory")
            })?;
            let parts: Vec<_> = relative.components().collect();
            if parts.is_empty()
                || parts
                    .iter()
                    .any(|part| !matches!(part, std::path::Component::Normal(_)))
            {
                return Err(io::Error::other("invalid checkpoint artifact path"));
            }
            let first = parts[0].as_os_str().to_string_lossy();
            if !((parts.len() == 1 && first.starts_with(".arm-checkpoint-"))
                || (parts.len() == 2 && first.starts_with(".checkpoint-machines-")))
            {
                return Err(io::Error::other(
                    "unrecognized checkpoint artifact location",
                ));
            }
            artifacts.push(path);
        }
        for path in artifacts {
            remove_file(&path)?;
            if let Some(parent) = path.parent().filter(|parent| *parent != output) {
                match fs::remove_dir(parent) {
                    Ok(()) => {}
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
                        ) => {}
                    Err(e) => return Err(e),
                }
            }
        }
        remove_file(&metadata)?;
    }
    remove_file(&attempt.join(format!("checkpoint-{name}-interventions.json")))
}
fn remove_file(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}
