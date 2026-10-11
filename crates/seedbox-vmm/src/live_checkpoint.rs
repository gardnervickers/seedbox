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
    pub boot: Option<super::BootReport>,
    pub kills: Vec<super::Kill>,
}

pub(super) fn load(
    path: &Path,
) -> Result<Saved<seedbox_engine_core::checkpoint::LiveCheckpoint<super::Machine>>, String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| file.take(MAX_METADATA as u64 + 1).read_to_end(&mut bytes))
        .map_err(|error| format!("checkpoint {}: {error}", path.display()))?;
    if bytes.len() > MAX_METADATA {
        return Err("checkpoint metadata exceeds 64 MiB".into());
    }
    let saved: Saved<seedbox_engine_core::checkpoint::LiveCheckpoint<super::Machine>> =
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

#[derive(Default)]
pub(super) struct Checkpoints {
    pub held: seedbox_engine_core::checkpoint::LiveCheckpoints<super::Machine>,
    pub boot: Option<super::BootReport>,
    pub kills: Vec<super::Kill>,
}
impl Checkpoints {
    pub fn capture(
        &mut self,
        world: &mut super::World,
        replay: &seedbox_engine_core::intervention::Replay,
        host: &mut super::host_control::HostControl,
        output: &Path,
    ) -> Result<bool, String> {
        self.held
            .capture_pending_with(world, replay, host, |id, saved| {
                // A private directory owns all machine files until coordinator publication succeeds.
                let files = tempfile::Builder::new()
                    .prefix(".checkpoint-machines-")
                    .tempdir_in(output)
                    .map_err(|e| e.to_string())?;
                for (index, machine) in saved.world.machine_snapshots().iter().enumerate() {
                    machine.persist(&files.path().join(format!("{index}.snapshot")))?;
                }
                publish(
                    output,
                    id,
                    &Saved {
                        state: saved,
                        boot: self.boot.clone(),
                        kills: self.kills.clone(),
                    },
                )?;
                // Published metadata owns these files from here on.
                let _ = files.keep();
                Ok(())
            })
    }
}
