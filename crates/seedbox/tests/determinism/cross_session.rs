//! Oracles carried across a server session boundary by retained output and
//! an exported checkpoint file.
use crate::session::{err, fixture, Session, REAL_TIME};
use crate::trace::{same_output, suffix, Record};
use crate::{Outcome, SECOND};
use seedbox_api::PlaybackRate;
use seedbox_test::process::Guarded;
use std::path::{Path, PathBuf};

/// What the second session needs from the first.
pub struct Carried {
    /// A copy of examples/redis whose pulled image is named by registry digest.
    pinned: PathBuf,
    bundles: PathBuf,
    /// Each first-session fresh run's output, by project.
    outputs: Vec<(PathBuf, Vec<Record>)>,
    /// The checkpointed source's output from its capture time on.
    suffix: Vec<Record>,
    checkpoint: String,
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// The registry digest the container engine recorded for a pulled tag.
fn repository_digest(reference: &str) -> Result<String, String> {
    let repository = reference
        .rsplit_once(':')
        .map_or(reference, |(name, _)| name);
    let output = std::process::Command::new("docker")
        .args([
            "image",
            "inspect",
            "--format",
            "{{json .RepoDigests}}",
            reference,
        ])
        .guarded_output()
        .map_err(|e| format!("docker image inspect {reference}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "docker image inspect {reference}: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let digests: Vec<String> = serde_json::from_slice(&output.stdout).map_err(err)?;
    // The engine may record the short name (redis@sha256:…) for a docker.io image.
    let short = repository.trim_start_matches("docker.io/library/");
    digests
        .into_iter()
        .find_map(|d| {
            let (name, digest) = d.split_once('@')?;
            (name == repository || name == short || name == format!("library/{short}"))
                .then(|| format!("{repository}@{digest}"))
        })
        .ok_or_else(|| format!("{reference} has no registry digest"))
}

impl Session {
    /// In the first session: fresh runs of the fixture and of an example with
    /// images pulled by tag, whose output is kept, and a run with a checkpoint
    /// whose source is deleted after the checkpoint file is exported.
    pub async fn carry(&self, root: &Path) -> Result<Carried, String> {
        let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/redis");
        let mut carried = Carried {
            pinned: root.join("redis-pinned"),
            bundles: root.join("bundles"),
            outputs: Vec::new(),
            suffix: Vec::new(),
            checkpoint: String::new(),
        };
        std::fs::create_dir_all(&carried.bundles).map_err(err)?;
        let twin = self.run_project(&fixture()).await?;
        carried.outputs.push((fixture(), self.logs(&twin).await?));
        let tagged = self.run_project(&example).await?;
        let tagged = self.logs(&tagged).await?;
        // The same example with its pulled image named by a registry digest.
        let tag = "docker.io/library/redis:7.2";
        let digest = repository_digest(tag)?;
        copy_tree(&example, &carried.pinned).map_err(err)?;
        let project = carried.pinned.join("seedbox.yaml");
        let text = std::fs::read_to_string(&project).map_err(err)?;
        let pinned = text.replace(tag, &digest);
        if pinned == text {
            return Err(format!("examples/redis no longer names {tag}"));
        }
        std::fs::write(&project, pinned).map_err(err)?;
        carried.outputs.push((carried.pinned.clone(), tagged));

        // A checkpoint with open TCP streams, volume data and guest entropy in use.
        let source = self.submit(&self.document, true).await?;
        self.live(&source).await?;
        self.rate(&source, REAL_TIME).await?;
        if !self.reach(&source, 9 * SECOND).await? {
            return Err(format!("{source} ended before 9 s"));
        }
        let (name, at) = self.checkpoint(&source).await?;
        self.rate(&source, PlaybackRate::Unlimited).await?;
        self.complete(&source).await?;
        carried.suffix = suffix(self.logs(&source).await?, at);
        self.export_checkpoint(&source, &name, &carried.bundles.join("checkpoint.tar"))
            .await?;
        carried.checkpoint = name;
        self.client.delete(&source).await.map_err(err)?;
        Ok(carried)
    }

    // ---- TwinFreshRunsCrossSession ----------------------------------------------------

    /// In a new session with an empty cache, the fixture and the example
    /// (pinned by digest) resolve, prepare and run again; each must pass with
    /// its first-session run's output. Full run bundles are gone, so the
    /// server cannot compare witnesses across sessions.
    pub async fn twin_fresh_runs(&self, carried: &Carried) -> Outcome {
        for (project, output) in &carried.outputs {
            let fresh = self.run_project(project).await?;
            same_output(output, &self.logs(&fresh).await?).map_err(|e| {
                format!(
                    "a fresh run of {} differs from the first session's:\n{e}",
                    project.display()
                )
            })?;
        }
        Ok(())
    }

    // ---- ImportedCheckpointExecutes ---------------------------------------------------

    /// A checkpoint file exported from a deleted source, imported into a
    /// session that never saw it, must branch twice to equal results and
    /// continue with the source's output.
    pub async fn imported_checkpoint(&self, carried: &Carried) -> Outcome {
        let document = self
            .import_checkpoint(&carried.bundles.join("checkpoint.tar"))
            .await?;
        let first = self.submit(&document, false).await?;
        let second = self.submit(&document, false).await?;
        self.complete(&first).await?;
        self.complete(&second).await?;
        self.same_results(&first, &second).await?;
        for branch in [&first, &second] {
            same_output(&carried.suffix, &self.logs(branch).await?).map_err(|e| {
                format!(
                    "{branch} output differs from the deleted source after its checkpoint:\n{e}"
                )
            })?;
        }
        Ok(())
    }
}
