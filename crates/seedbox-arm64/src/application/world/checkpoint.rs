//! Publish coordinator metadata only after all referenced machine artifacts
//! have been captured. The rename boundary never exposes a partial document.
use std::{
    fs::File,
    io::{self, Write},
    path::Path,
};

const MAX_METADATA: usize = 64 * 1024 * 1024;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Saved<T> {
    pub state: T,
    pub initial_launch: Option<serde_json::Value>,
}

pub(super) fn load(
    path: &Path,
) -> Result<Saved<seedbox_engine_core::checkpoint::LiveCheckpoint<super::ArmMachine>>, String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| file.take(MAX_METADATA as u64 + 1).read_to_end(&mut bytes))
        .map_err(|error| format!("checkpoint {}: {error}", path.display()))?;
    if bytes.len() > MAX_METADATA {
        return Err("checkpoint metadata exceeds 64 MiB".into());
    }
    let saved: Saved<seedbox_engine_core::checkpoint::LiveCheckpoint<super::ArmMachine>> =
        serde_json::from_slice(&bytes)
            .map_err(|error| format!("checkpoint {}: {error}", path.display()))?;
    if saved.state.position != saved.state.replay.position {
        return Err("checkpoint replay position disagrees with world position".into());
    }
    Ok(saved)
}

struct Bounded<'a> {
    file: &'a mut File,
    remaining: usize,
}
impl Write for Bounded<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err(io::Error::other("checkpoint metadata exceeds 64 MiB"));
        }
        let written = self.file.write(bytes)?;
        self.remaining -= written;
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

pub(super) fn publish(
    output: &Path,
    request_id: &str,
    saved: &impl serde::Serialize,
) -> Result<(), String> {
    // Defense at the filesystem boundary even though host admission validates IDs.
    if request_id.is_empty()
        || request_id.len() > 128
        || !request_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err("invalid checkpoint request ID".into());
    }
    let destination = output.join(format!("checkpoint-{request_id}.json"));
    let save = || -> Result<(), Box<dyn std::error::Error>> {
        let mut temporary = tempfile::Builder::new()
            .prefix(".checkpoint-")
            .tempfile_in(output)?;
        serde_json::to_writer(
            Bounded {
                file: temporary.as_file_mut(),
                remaining: MAX_METADATA,
            },
            saved,
        )?;
        temporary.persist_noclobber(&destination)?;
        Ok(())
    };
    save().map_err(|error| format!("checkpoint {}: {error}", destination.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_is_complete_and_cannot_replace_an_existing_checkpoint() {
        let directory = tempfile::tempdir().unwrap();
        let saved = serde_json::json!({"position":42});
        publish(directory.path(), "capture", &saved).unwrap();
        let path = directory.path().join("checkpoint-capture.json");
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
            saved
        );
        assert!(publish(
            directory.path(),
            "capture",
            &serde_json::json!({"position":43})
        )
        .is_err());
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        assert!(publish(directory.path(), "../escape", &saved).is_err());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}
