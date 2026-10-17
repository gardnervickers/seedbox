//! Merge image-owned layers without mounting or extracting guest filesystem paths.
use crate::{execution::CancellationToken, limits::exceeds, Error, Result};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};
const LIMIT: u64 = 4 << 30;
const METADATA_LIMIT: u64 = 4 << 20;
fn error(message: impl std::fmt::Display) -> Error {
    Error::Setup(format!("image archive: {message}"))
}
fn relative(path: &Path) -> Result<PathBuf> {
    let mut result = PathBuf::new();
    for part in path.components() {
        match part {
            Component::Normal(part) => result.push(part),
            Component::CurDir => (),
            _ => return Err(error(format!("unsafe path {}", path.display()))),
        }
    }
    Ok(result)
}
struct Output<'a> {
    file: std::io::BufWriter<fs::File>,
    remaining: u64,
    cancellation: &'a CancellationToken,
}
impl Write for Output<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.cancellation.is_cancelled() {
            return Err(std::io::Error::other("preparation cancelled"));
        }
        if bytes.len() as u64 > self.remaining {
            return Err(std::io::Error::other("merged filesystem exceeds 4 GiB"));
        }
        let n = self.file.write(bytes)?;
        self.remaining -= n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Manifest {
    config: String,
    layers: Vec<String>,
}

pub(crate) fn merge(
    input: &Path,
    directory: &Path,
    output: &Path,
    cancellation: &CancellationToken,
) -> Result<serde_json::Value> {
    fs::create_dir(directory)?;
    let mut names = BTreeSet::new();
    let mut archive = tar::Archive::new(std::io::BufReader::new(fs::File::open(input)?));
    let mut total = 0u64;
    // Only the image archive envelope is extracted. Its members are metadata
    // and opaque layer blobs; layer entries never become host filesystem paths.
    for entry in archive.entries()? {
        if cancellation.is_cancelled() {
            return Err(Error::PreparationCancelled);
        }
        let mut entry = entry?;
        let name = relative(&entry.path()?)?;
        if name.as_os_str().is_empty() || entry.header().entry_type().is_dir() {
            continue;
        }
        // Podman includes unused Docker v1 compatibility symlinks. Never
        // materialize or follow those: the manifest must reference real blobs.
        if entry.header().entry_type().is_symlink() {
            continue;
        }
        if !entry.header().entry_type().is_file() || !names.insert(name.clone()) {
            return Err(error(format!(
                "invalid or duplicate member {}",
                name.display()
            )));
        }
        total = total
            .checked_add(entry.size())
            .filter(|n| *n <= LIMIT)
            .ok_or_else(|| error("image archive exceeds 4 GiB"))?;
        let path = directory.join(name);
        fs::create_dir_all(
            path.parent()
                .ok_or_else(|| error("missing member parent"))?,
        )?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            if cancellation.is_cancelled() {
                return Err(Error::PreparationCancelled);
            }
            let n = entry.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            file.write_all(&buffer[..n])?;
        }
    }
    let read_json = |name: &Path| -> Result<serde_json::Value> {
        let name = relative(name)?;
        if !names.contains(&name) {
            return Err(error(format!("missing member {}", name.display())));
        }
        let file = fs::File::open(directory.join(name))?;
        if file.metadata()?.len() > METADATA_LIMIT {
            return Err(error(exceeds("image metadata", METADATA_LIMIT)));
        }
        Ok(serde_json::from_reader(file)?)
    };
    let manifest: Vec<Manifest> = serde_json::from_value(read_json(Path::new("manifest.json"))?)?;
    let [manifest]: [Manifest; 1] = manifest
        .try_into()
        .map_err(|_| error("expected exactly one exported image"))?;
    let config = read_json(Path::new(&manifest.config))?;
    let layers = manifest
        .layers
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let name = relative(Path::new(name))?;
            if !names.contains(&name) {
                return Err(error(format!("missing layer {}", name.display())));
            }
            let path = directory.join(name);
            let media_type = ocirender::image::detect_media_type(&path)
                .map_err(error)?
                .to_owned();
            Ok(ocirender::image::LayerBlob {
                path,
                media_type,
                index,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut sink = Output {
        file: std::io::BufWriter::new(fs::File::create(output)?),
        remaining: LIMIT,
        cancellation,
    };
    // ocirender deliberately omits the root directory. Preserve the newest
    // image-owned root metadata before its merged entries.
    'root: for layer in layers.iter().rev() {
        let source =
            ocirender::layers::open_layer(&layer.path, &layer.media_type).map_err(error)?;
        let mut archive = tar::Archive::new(source.into_inner().take(LIMIT + 1));
        for entry in archive.entries()? {
            if cancellation.is_cancelled() {
                return Err(Error::PreparationCancelled);
            }
            let mut entry = entry?;
            if relative(&entry.path()?)?.as_os_str().is_empty() {
                if !entry.header().entry_type().is_dir() || entry.size() != 0 {
                    return Err(error("invalid image root directory"));
                }
                let mut header =
                    crate::container_images::export_header(&mut entry, Path::new("."))?;
                header.set_path(".")?;
                header.set_cksum();
                sink.write_all(header.as_bytes())?;
                break 'root;
            }
        }
    }
    let result = merge_folded(layers, directory, sink, cancellation);
    if cancellation.is_cancelled() {
        return Err(Error::PreparationCancelled);
    }
    result?;
    Ok(config)
}

/// Merge `layers`, oldest first, as applying them in order would.
///
/// ocirender 0.2.3 resolves the whiteouts of every layer in one path trie that
/// loses suppressions once more than one layer carries whiteouts: an older
/// layer's marker replaces a newer one on the same path, and a directory made
/// opaque in a layer hides newer whiteouts below it from that layer's own
/// entries. Both bring deleted files back. Merging is exact while whiteouts
/// appear only in the newest layer, so apply the layers in groups that each end
/// at a layer with whiteouts, each group over the merged result of those before.
fn merge_folded(
    layers: Vec<ocirender::image::LayerBlob>,
    directory: &Path,
    sink: Output<'_>,
    cancellation: &CancellationToken,
) -> Result<()> {
    let mut groups = Vec::new();
    let mut group = Vec::new();
    for layer in layers {
        let whiteouts = has_whiteouts(&layer, cancellation)?;
        group.push(layer);
        if whiteouts {
            groups.push(std::mem::take(&mut group));
        }
    }
    if group.is_empty() {
        group = groups.pop().unwrap_or_default();
    }
    // The merged result so far is the oldest layer of the next group.
    let stack = |base: Option<&tempfile::NamedTempFile>,
                 group: Vec<ocirender::image::LayerBlob>| {
        let mut blobs: Vec<_> = base
            .map(|merged| ocirender::image::LayerBlob {
                path: merged.path().to_owned(),
                media_type: "application/vnd.oci.image.layer.v1.tar".into(),
                index: 0,
            })
            .into_iter()
            .collect();
        let offset = blobs.len();
        blobs.extend(group.into_iter().enumerate().map(|(index, mut layer)| {
            layer.index = offset + index;
            layer
        }));
        blobs
    };
    let mut base = None;
    for earlier in groups {
        // A fresh name never collides with an extracted envelope member.
        let merged = tempfile::NamedTempFile::new_in(directory)?;
        let output = Output {
            file: std::io::BufWriter::new(merged.reopen()?),
            remaining: LIMIT,
            cancellation,
        };
        ocirender::overlay::merge_layers_into(stack(base.as_ref(), earlier), output)
            .map_err(error)?;
        base = Some(merged);
    }
    ocirender::overlay::merge_layers_into(stack(base.as_ref(), group), sink).map_err(error)
}

fn has_whiteouts(
    layer: &ocirender::image::LayerBlob,
    cancellation: &CancellationToken,
) -> Result<bool> {
    let mut archive =
        ocirender::layers::open_layer(&layer.path, &layer.media_type).map_err(error)?;
    for entry in archive.entries()? {
        if cancellation.is_cancelled() {
            return Err(Error::PreparationCancelled);
        }
        if entry?
            .path()?
            .file_name()
            .is_some_and(|name| name.as_encoded_bytes().starts_with(b".wh."))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(tar: &mut tar::Builder<fs::File>, name: &str, bytes: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(bytes.len() as u64);
        tar.append_data(&mut header, name, bytes).unwrap();
    }

    #[test]
    fn envelope_rejects_duplicate_members_and_manifest_traversal() {
        for duplicate in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let input = root.path().join("image.tar");
            let mut tar = tar::Builder::new(fs::File::create(&input).unwrap());
            member(&mut tar, "config.json", b"{}");
            if duplicate {
                member(&mut tar, "config.json", b"{}");
            } else {
                member(
                    &mut tar,
                    "manifest.json",
                    br#"[{"Config":"../outside","Layers":[]}]"#,
                );
            }
            tar.finish().unwrap();
            let failure = merge(
                &input,
                &root.path().join("blobs"),
                &root.path().join("rootfs"),
                &CancellationToken::default(),
            )
            .unwrap_err()
            .to_string();
            assert!(
                failure.contains(if duplicate {
                    "duplicate member"
                } else {
                    "unsafe path"
                }),
                "{failure}"
            );
        }
    }

    #[test]
    fn manifest_cannot_reference_ignored_compatibility_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("image.tar");
        let mut tar = tar::Builder::new(fs::File::create(&input).unwrap());
        member(&mut tar, "config.json", b"{}");
        member(
            &mut tar,
            "manifest.json",
            br#"[{"Config":"alias","Layers":[]}]"#,
        );
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        tar.append_link(&mut header, "alias", "config.json")
            .unwrap();
        tar.finish().unwrap();
        let blobs = root.path().join("blobs");
        assert!(merge(
            &input,
            &blobs,
            &root.path().join("rootfs"),
            &CancellationToken::default()
        )
        .unwrap_err()
        .to_string()
        .contains("missing member alias"));
        assert!(!blobs.join("alias").exists());
    }

    #[test]
    fn newest_root_metadata_survives_layer_merging() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("image.tar");
        let mut image = tar::Builder::new(fs::File::create(&input).unwrap());
        member(&mut image, "config.json", b"{}");
        member(
            &mut image,
            "manifest.json",
            br#"[{"Config":"config.json","Layers":["old.tar","new.tar"]}]"#,
        );
        for (name, uid, mode) in [("old.tar", 123, 0o755), ("new.tar", 456, 0o750)] {
            let mut layer = tar::Builder::new(Vec::new());
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(mode);
            header.set_uid(uid);
            header.set_gid(uid);
            header.set_mtime(uid);
            header.set_size(0);
            layer
                .append_data(&mut header, ".", std::io::empty())
                .unwrap();
            member(&mut image, name, &layer.into_inner().unwrap());
        }
        image.finish().unwrap();
        let output = root.path().join("rootfs");
        merge(
            &input,
            &root.path().join("blobs"),
            &output,
            &CancellationToken::default(),
        )
        .unwrap();
        let mut tar = tar::Archive::new(fs::File::open(output).unwrap());
        let entry = tar.entries().unwrap().next().unwrap().unwrap();
        assert_eq!(entry.path().unwrap(), Path::new("."));
        assert_eq!(entry.header().mode().unwrap(), 0o750);
        assert_eq!(entry.header().uid().unwrap(), 456);
        assert_eq!(entry.header().gid().unwrap(), 456);
        assert_eq!(entry.header().mtime().unwrap(), 456);
    }

    /// A layer of empty entries; a trailing `/` makes a directory.
    fn layer(entries: &[&str]) -> Vec<u8> {
        let mut layer = tar::Builder::new(Vec::new());
        for entry in entries {
            let mut header = tar::Header::new_ustar();
            header.set_mode(0o755);
            header.set_size(0);
            if entry.ends_with('/') {
                header.set_entry_type(tar::EntryType::Directory);
            }
            layer
                .append_data(&mut header, entry, std::io::empty())
                .unwrap();
        }
        layer.into_inner().unwrap()
    }

    fn merged_paths(layers: Vec<Vec<u8>>) -> Vec<String> {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("image.tar");
        let names: Vec<_> = (0..layers.len()).map(|i| format!("l{i}.tar")).collect();
        let mut image = tar::Builder::new(fs::File::create(&input).unwrap());
        member(&mut image, "config.json", b"{}");
        let manifest = serde_json::json!([{"Config": "config.json", "Layers": names}]);
        member(&mut image, "manifest.json", manifest.to_string().as_bytes());
        for (name, layer) in names.iter().zip(layers) {
            member(&mut image, name, &layer);
        }
        image.finish().unwrap();
        let output = root.path().join("rootfs");
        merge(
            &input,
            &root.path().join("blobs"),
            &output,
            &CancellationToken::default(),
        )
        .unwrap();
        let mut paths: Vec<_> = tar::Archive::new(fs::File::open(output).unwrap())
            .entries()
            .unwrap()
            .map(|entry| entry.unwrap().path().unwrap().display().to_string())
            .collect();
        paths.sort();
        paths
    }

    #[test]
    fn whiteouts_in_several_layers_all_stay_applied() {
        // An older layer's whiteout must not undo a newer one on the same path.
        assert_eq!(
            merged_paths(vec![
                layer(&["d/", "d/old"]),
                layer(&[".wh.d", "d/", "d/new"]),
                layer(&[".wh.d", "d/"]),
            ]),
            ["d/"]
        );
        // A directory made opaque in one layer keeps a later whiteout below it.
        assert_eq!(
            merged_paths(vec![
                layer(&["d/", "d/f"]),
                layer(&["d/", "d/.wh..wh..opq", "d/f", "d/g"]),
                layer(&["d/", "d/.wh.f"]),
            ]),
            ["d/", "d/g"]
        );
    }

    #[test]
    fn output_stops_at_limit_and_on_cancellation() {
        let token = CancellationToken::default();
        let mut output = Output {
            file: std::io::BufWriter::new(tempfile::tempfile().unwrap()),
            remaining: 3,
            cancellation: &token,
        };
        output.write_all(b"abc").unwrap();
        assert!(output
            .write_all(b"d")
            .unwrap_err()
            .to_string()
            .contains("exceeds"));
        output.flush().unwrap();
        assert_eq!(output.file.get_ref().metadata().unwrap().len(), 3);
        token.cancel();
        assert!(output
            .write_all(b"d")
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
    }
}
