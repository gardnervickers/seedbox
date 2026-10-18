//! OCI ingestion and preparation bridge to explicit Driver boot inputs.
use crate::{
    config::Simulation,
    execution::PreparedInputs,
    preparation::{OciPreparer, PreparationContext},
    store::ContentStore,
    Error, Result,
};
use seedbox_driver_execution::{driver_world::preparation::OciBootFactory, runtime_assets::Inputs};
use std::{fs, path::Path};
// ContainerTransport's archive is ready for the legacy boot assembler: every
// entry (and hard-link target) already carries its rootfs/ transport prefix.
// The SDK takes an application-root tar and supplies its own rootfs/ prefix.
// Rewrite metadata only; never extract guest paths onto the host.
fn logical_archive(path: &Path) -> Result<Vec<u8>> {
    fn relative(path: &Path) -> Result<&Path> {
        let path = path.strip_prefix("rootfs").map_err(|_| {
            Error::Integrity("OCI transport archive entry is outside rootfs".into())
        })?;
        if path
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            return Err(Error::Integrity("unsafe OCI transport archive path".into()));
        }
        Ok(if path.as_os_str().is_empty() {
            Path::new(".")
        } else {
            path
        })
    }
    let mut input = tar::Archive::new(std::io::BufReader::new(fs::File::open(path)?));
    let mut output = tar::Builder::new(Vec::new());
    for entry in input.entries()? {
        let mut entry = entry?;
        let original = entry.path()?.into_owned();
        let name = relative(&original)?;
        let mut header = crate::container_images::export_header(&mut entry, name)?;
        if let Some(link) = entry.link_name()?.map(|path| path.into_owned()) {
            let target = if header.entry_type().is_hard_link() {
                relative(&link)?
            } else {
                &link
            };
            output.append_link(&mut header, name, target)?;
        } else {
            output.append_data(&mut header, name, &mut entry)?;
        }
    }
    Ok(output.into_inner()?)
}

pub(crate) fn filesystem(rootfs: &crate::oci::RootFilesystem) -> Result<Vec<u8>> {
    let bytes = match rootfs {
        crate::oci::RootFilesystem::Archive(path) => logical_archive(path)?,
        crate::oci::RootFilesystem::Directory(root) => {
            let mut paths = vec![];
            let mut pending = vec![root.to_owned()];
            while let Some(directory) = pending.pop() {
                for entry in fs::read_dir(directory)? {
                    let entry = entry?;
                    if entry.file_type()?.is_dir() {
                        pending.push(entry.path());
                    }
                    paths.push(entry.path());
                }
            }
            paths.sort();
            let mut archive = tar::Builder::new(Vec::new());
            archive.follow_symlinks(false);
            for path in paths {
                archive.append_path_with_name(
                    &path,
                    path.strip_prefix(root)
                        .map_err(|e| Error::Integrity(e.to_string()))?,
                )?;
            }
            archive.into_inner()?
        }
    };
    Ok(bytes)
}
/// Validate the explicit SDK bootstrap package without legacy-format assumptions
/// and without opening a hypervisor, starting a worker or contacting OCI.
pub fn validate_oci_runtime(preparer: &OciPreparer, runtime: &Inputs) -> Result<()> {
    seedbox_driver_execution::driver_world::retained::validate_contract(&preparer.profile)?;
    for (path, name) in [(&preparer.kernel, "kernel"), (&preparer.init, "init")] {
        if fs::read(path)? != fs::read(runtime.guest_assets.join(name))? {
            return Err(Error::Integrity(
                "Driver OCI bootstrap differs from its selected runtime".into(),
            ));
        }
    }
    if runtime.qemu.is_none()
        && fs::read(&preparer.progress)? != fs::read(runtime.guest_assets.join("progress.tar"))?
    {
        return Err(Error::Integrity(
            "Driver PV progress differs from its selected runtime".into(),
        ));
    }
    let expected = if runtime.qemu.is_some() {
        "driver-tcg"
    } else {
        "driver-pv"
    };
    if preparer.profile.backend != expected {
        return Err(Error::Integrity(
            "Driver OCI backend differs from its selected runtime".into(),
        ));
    }
    OciBootFactory::new(
        &preparer.engine,
        runtime,
        crate::guest::MIN_WALL_CLOCK_EPOCH,
        std::num::NonZeroU32::new(1).unwrap(),
    )?;
    Ok(())
}

pub fn retain_oci_prepared(
    inputs: &mut PreparedInputs,
    simulation: &Simulation,
    runtime: &Inputs,
    store: &ContentStore,
    context: &PreparationContext,
) -> Result<()> {
    seedbox_driver_execution::driver_world::preparation::retain_oci_prepared(
        inputs,
        simulation,
        runtime,
        store,
        &|| context.check(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transport_archive_requires_rooted_entries_and_hardlink_targets() {
        for (name, link) in [("app", None), ("rootfs/link", Some("app"))] {
            let file = tempfile::NamedTempFile::new().unwrap();
            let mut tar = tar::Builder::new(file.reopen().unwrap());
            let mut header = tar::Header::new_gnu();
            header.set_uid(0);
            header.set_gid(0);
            header.set_mode(0o644);
            header.set_mtime(0);
            header.set_size(0);
            if let Some(target) = link {
                header.set_entry_type(tar::EntryType::Link);
                tar.append_link(&mut header, name, target).unwrap();
            } else {
                tar.append_data(&mut header, name, std::io::empty())
                    .unwrap();
            }
            tar.finish().unwrap();
            assert!(logical_archive(file.path()).is_err(), "{name} {link:?}");
        }
    }
}
