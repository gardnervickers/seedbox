//! OCI-backed preparation. Runtime binaries are explicit inputs, so library
//! callers do not depend on CLI environment or a particular checkout layout.

use crate::{
    config::Simulation,
    execution::{ExecutionProfile, PreparationBackend, PreparedInputs},
    guest::{build_initramfs_with, launch_recipe, minimum_memory_mb},
    oci::{FrozenContext, OciTools},
    store::{ContentDigest, ContentStore},
    Error, Result,
};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct OciPreparer {
    pub tools: OciTools,
    pub kernel: PathBuf,
    pub engine: PathBuf,
    pub init: PathBuf,
    pub busybox: PathBuf,
    /// Static mke2fs, needed only when a service mounts a named volume: the
    /// guest formats its blank retained disk with it on first boot.
    pub mke2fs: Option<PathBuf>,
    pub profile: ExecutionProfile,
}

pub use crate::preparation_command::PreparationContext;
pub mod native;

impl PreparationBackend for OciPreparer {
    fn validate_runtime_inputs(&self) -> Result<()> {
        if self.profile.backend == "seedbox-kvm" {
            crate::admission::validate_profile(&self.profile)?;
            crate::admission::validate_artifacts(
                &self.profile,
                &crate::admission::file_digest(&self.kernel)?,
            )?;
            if crate::admission::file_digest(&self.engine)? != self.profile.engine {
                return Err(Error::Integrity(
                    "VMM changed after profile construction".into(),
                ));
            }
        }
        Ok(())
    }
    fn prepare(&self, simulation: &Simulation, store: &ContentStore) -> Result<PreparedInputs> {
        self.validate_runtime_inputs()?;
        self.prepare_inner(simulation, store, None)
    }
    fn prepare_cancellable(
        &self,
        simulation: &Simulation,
        store: &ContentStore,
        context: &PreparationContext,
    ) -> Result<PreparedInputs> {
        context.check()?;
        self.validate_runtime_inputs()?;
        if simulation
            .services
            .values()
            .any(|s| matches!(s.source, crate::config::ImageSource::Build { .. }))
        {
            self.tools.validate_build_epoch()?;
            context.admit_builder(&self.tools.docker)?;
        }
        self.prepare_inner(simulation, store, Some(context))
    }
}
impl OciPreparer {
    fn prepare_inner(
        &self,
        simulation: &Simulation,
        store: &ContentStore,
        context: Option<&PreparationContext>,
    ) -> Result<PreparedInputs> {
        if let Some(context) = context {
            context.check()?;
        }
        let kernel = store.put_file(&self.kernel)?;
        if self.profile.backend == "seedbox-kvm" {
            crate::admission::validate_artifacts(&self.profile, &kernel)?;
        }
        let engine = store.put_file(&self.engine)?;
        if engine != self.profile.engine {
            return Err(Error::Integrity(
                "VMM changed after profile construction".into(),
            ));
        }
        let has_builds = simulation
            .services
            .values()
            .any(|service| matches!(service.source, crate::config::ImageSource::Build { .. }));
        if has_builds {
            self.tools.validate_build_epoch()?;
        }
        let closure = if has_builds {
            reuse_closure(&self.tools)?
        } else {
            None
        };
        for path in [&self.init, &self.busybox] {
            amd64_elf(path)?;
        }
        for (name, service) in &simulation.services {
            for mount in &service.volumes {
                validate_config_target(&mount.target).map_err(|_| Error::Validation {
                    path: format!("$.services.{name}.volumes"),
                    message: format!("reserved guest runtime path {}", mount.target),
                })?;
            }
        }
        let volumes = simulation.services.values().any(|s| !s.volumes.is_empty());
        let mut auxiliary = BTreeMap::from([
            ("guest-init".into(), store.put_file(&self.init)?),
            ("guest-busybox".into(), store.put_file(&self.busybox)?),
        ]);
        if volumes {
            let mke2fs = self.mke2fs.as_ref().ok_or_else(|| {
                Error::Setup(
                    "named volumes need a static mke2fs to format the guest disk; enter \
                     `nix develop`, or pass --mke2fs or set SEEDBOX_MKE2FS"
                        .into(),
                )
            })?;
            amd64_elf(mke2fs)?;
            auxiliary.insert("guest-mke2fs".into(), store.put_file(mke2fs)?);
        }
        let mut configs = BTreeMap::new();
        for (name, path) in &simulation.configs {
            let mut data = Vec::new();
            fs::File::open(path)?
                .take(16 * 1024 * 1024 + 1)
                .read_to_end(&mut data)?;
            if data.len() > 16 * 1024 * 1024 {
                return Err(Error::Execution(format!("config {name} exceeds 16 MiB")));
            }
            auxiliary.insert(format!("config/{name}"), store.put(&data)?);
            configs.insert(name, data);
        }
        if let Some(context) = context {
            context.setup_inputs(&auxiliary, &engine, &kernel)?;
        }
        let addresses: BTreeMap<_, _> = simulation
            .services
            .keys()
            .enumerate()
            .map(|(index, name)| (name, format!("10.0.0.{}", index + 1)))
            .collect();
        let mut hosts = "127.0.0.1 localhost\n::1 localhost\n".to_owned();
        for (name, address) in &addresses {
            hosts.push_str(&format!("{address} {name}\n"));
        }
        let temporary = tempfile::tempdir()?;
        let init = store.materialize(&auxiliary["guest-init"], &temporary.path().join("init"))?;
        let busybox = store.materialize(
            &auxiliary["guest-busybox"],
            &temporary.path().join("busybox"),
        )?;
        amd64_elf(&init)?;
        amd64_elf(&busybox)?;
        let mke2fs = auxiliary
            .get("guest-mke2fs")
            .map(|digest| store.materialize(digest, &temporary.path().join("mke2fs")))
            .transpose()?;
        let mut images = BTreeMap::new();
        let mut guests = BTreeMap::new();
        let mut provenance = BTreeMap::new();
        let mut frozen_contexts = BTreeMap::<PathBuf, FrozenContext>::new();
        let mut imported_sources = BTreeMap::new();
        for (name, service) in &simulation.services {
            if let Some(context) = context {
                context.check()?;
            }
            let mounts = service
                .configs
                .iter()
                .map(|mount| {
                    validate_config_target(&mount.target)?;
                    Ok((
                        mount.target.clone(),
                        configs
                            .get(&mount.source)
                            .ok_or_else(|| Error::Integrity("unknown config source".into()))?
                            .clone(),
                    ))
                })
                .collect::<Result<BTreeMap<_, _>>>()?;
            let key = if let (
                Some(closure),
                crate::config::ImageSource::Build {
                    context: source,
                    dockerfile,
                },
            ) = (&closure, &service.source)
            {
                if reuse_closure(&self.tools)?.as_ref() != Some(closure) {
                    return Err(Error::Integrity(
                        "preparation tools changed after freezing the build closure".into(),
                    ));
                }
                let canonical = fs::canonicalize(source).map_err(|error| {
                    Error::Setup(format!(
                        "build context {}: {error}; restore the declared context",
                        source.display()
                    ))
                })?;
                let frozen = frozen_source(
                    &mut frozen_contexts,
                    &canonical,
                    source,
                    temporary.path(),
                    store,
                )?;
                let relative = dockerfile
                    .strip_prefix(source)
                    .map_err(|_| Error::Validation {
                        path: "build.dockerfile".into(),
                        message: "Dockerfile must be inside the build context".into(),
                    })?
                    .to_path_buf();
                Some(BuildKey {
                    context: canonical,
                    context_digest: frozen.digest.clone(),
                    dockerfile_digest: store.put_file(&frozen.root.join(&relative))?,
                    dockerfile: relative,
                    closure: closure.clone(),
                    platform: "linux/amd64",
                    epoch: self.tools.build_epoch,
                })
            } else {
                None
            };
            let imported = if let Some(key) = key {
                // A failed import (including cleanup-unverified) never enters this map.
                let (owner, source) =
                    successful_source(&mut imported_sources, key.clone(), || {
                        let imported = self.tools.import_frozen(
                            service,
                            &temporary.path().join(name),
                            store,
                            context.map(|c| (c, name.as_str())),
                            Some(&frozen_contexts[&key.context]),
                        )?;
                        Ok((name.clone(), imported))
                    })?;
                if let Some(context) = context {
                    context.source_group(
                        name,
                        &owner,
                        &key.context_digest,
                        &key.dockerfile_digest,
                        &source.image.manifest,
                        key.epoch,
                    )?;
                }
                source.for_service(service, store)?
            } else {
                self.tools.import_with_context(
                    service,
                    &temporary.path().join(name),
                    store,
                    context.map(|c| (c, name.as_str())),
                )?
            };
            if let Some(context) = context {
                context.check()?;
            }
            let recipe = launch_recipe(
                &imported.image.launch,
                &format!("{}/24", addresses[name]),
                &hosts,
                &mounts,
            )?;
            let archive = temporary.path().join(format!("{name}.cpio"));
            // Package only retained bootstrap bytes. The source paths may be
            // replaced by another build while an image is being downloaded.
            let mke2fs = mke2fs.as_deref().filter(|_| !service.volumes.is_empty());
            build_initramfs_with(&imported.rootfs, &init, &busybox, mke2fs, &recipe, &archive)?;
            let memory_mb = self
                .profile
                .semantic_parameters
                .get("memory_mb")
                .and_then(|value| value.parse::<u32>().ok())
                .ok_or_else(|| Error::Integrity("OCI profile missing memory_mb".into()))?;
            let minimum = minimum_memory_mb(fs::metadata(&archive)?.len())?;
            if memory_mb < minimum {
                return Err(Error::Execution(format!("service {name}: {memory_mb} MiB guest RAM is below the archive boot floor of {minimum} MiB; increase --memory-mb (application memory may require more)")));
            }
            if let Some(context) = context {
                context.check()?;
            }
            guests.insert(name.clone(), store.put_file(&archive)?);
            images.insert(name.clone(), imported.image);
            auxiliary.extend(
                imported
                    .auxiliary
                    .into_iter()
                    .map(|(key, value)| (format!("image/{name}/{key}"), value)),
            );
            provenance.extend(
                imported
                    .provenance
                    .into_iter()
                    .map(|(key, value)| (format!("image/{name}/{key}"), value)),
            );
        }
        Ok(PreparedInputs {
            kernel,
            images,
            guests,
            placement: simulation
                .services
                .keys()
                .map(|name| (name.clone(), name.clone()))
                .collect(),
            profile: self.profile.clone(),
            auxiliary,
            provenance,
        })
    }
}

/// A per-call key: no entry or frozen context survives this preparation.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct BuildKey {
    context: PathBuf,
    context_digest: ContentDigest,
    dockerfile: PathBuf,
    dockerfile_digest: ContentDigest,
    closure: ContentDigest,
    platform: &'static str,
    epoch: Option<u32>,
}
fn frozen_source<'a>(
    contexts: &'a mut BTreeMap<PathBuf, FrozenContext>,
    canonical: &Path,
    source: &Path,
    temporary: &Path,
    store: &ContentStore,
) -> Result<&'a FrozenContext> {
    if !contexts.contains_key(canonical) {
        let directory = temporary.join(format!("source-{}", contexts.len()));
        contexts.insert(
            canonical.to_owned(),
            FrozenContext::capture(source, &directory, store)?,
        );
    }
    Ok(&contexts[canonical])
}
fn successful_source<K: Ord + Clone, V: Clone>(
    sources: &mut BTreeMap<K, V>,
    key: K,
    import: impl FnOnce() -> Result<V>,
) -> Result<V> {
    if let Some(value) = sources.get(&key) {
        return Ok(value.clone());
    }
    let value = import()?;
    sources.insert(key, value.clone());
    Ok(value)
}
fn reuse_closure(tools: &OciTools) -> Result<Option<ContentDigest>> {
    let Ok(proof) = crate::preparation_command::inspect_native_builder(&tools.docker) else {
        return Ok(None);
    };
    if !proof.admitted {
        return Ok(None);
    }
    // These explicit tool paths are resolved by the CLI, never by library env reads.
    let mut transports = Vec::new();
    for path in [&tools.skopeo, &tools.umoci] {
        let Ok(resolved) = fs::canonicalize(path) else {
            return Ok(None);
        };
        let digest = ContentDigest::of(&fs::read(&resolved)?);
        transports.push((resolved, digest));
    }
    Ok(Some(ContentDigest::of(&serde_json::to_vec(&(
        proof, transports,
    ))?)))
}

fn amd64_elf(path: &Path) -> Result<()> {
    let mut header = [0; 20];
    fs::File::open(path)?.read_exact(&mut header)?;
    if &header[..6] != b"\x7fELF\x02\x01" || header[18..20] != [62, 0] {
        return Err(Error::Integrity(format!(
            "expected an amd64 ELF guest helper: {}",
            path.display()
        )));
    }
    Ok(())
}

fn validate_config_target(path: &str) -> Result<()> {
    // Mounting over the runtime's devices or name resolution would invalidate
    // its own contract. Other paths remain inside the guest chroot.
    if path == "/"
        || path == "/etc/hosts"
        || ["/dev", "/proc", "/sys"]
            .iter()
            .any(|reserved| path == *reserved || path.starts_with(&format!("{reserved}/")))
    {
        return Err(Error::Validation {
            path: "configs.target".into(),
            message: format!("reserved guest runtime path {path}"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod source_tests {
    use super::*;
    #[test]
    fn shared_context_is_frozen_once_and_next_preparation_observes_changes() {
        let root = tempfile::tempdir().unwrap();
        let context = root.path().join("context");
        fs::create_dir(&context).unwrap();
        fs::write(context.join("Dockerfile"), "FROM scratch").unwrap();
        fs::write(context.join("app"), "first").unwrap();
        let store = ContentStore::new(root.path().join("store"));
        let mut contexts = BTreeMap::new();
        let canonical = fs::canonicalize(&context).unwrap();
        let first = frozen_source(
            &mut contexts,
            &canonical,
            &context,
            &root.path().join("first-call"),
            &store,
        )
        .unwrap()
        .digest
        .clone();
        fs::write(context.join("app"), "changed after freeze").unwrap();
        let shared = frozen_source(
            &mut contexts,
            &canonical,
            &context,
            &root.path().join("first-call"),
            &store,
        )
        .unwrap();
        assert_eq!(shared.digest, first);
        assert_eq!(fs::read(shared.root.join("app")).unwrap(), b"first");
        assert_eq!(contexts.len(), 1);
        let mut next = BTreeMap::new();
        assert_ne!(
            frozen_source(
                &mut next,
                &canonical,
                &context,
                &root.path().join("next-call"),
                &store
            )
            .unwrap()
            .digest,
            first
        );
    }
    #[test]
    fn source_keys_invalidate_complete_inputs_and_never_memoize_failure() {
        let key = BuildKey {
            context: "declared-context".into(),
            context_digest: ContentDigest::of(b"full context"),
            dockerfile: "Dockerfile".into(),
            dockerfile_digest: ContentDigest::of(b"dockerfile"),
            closure: ContentDigest::of(b"tools"),
            platform: "linux/amd64",
            epoch: None,
        };
        let mut sources = BTreeMap::new();
        let mut calls = 0;
        for _ in 0..3 {
            let image = successful_source(&mut sources, key.clone(), || {
                calls += 1;
                Ok(17)
            })
            .unwrap();
            assert_eq!(image, 17);
        }
        assert_eq!(calls, 1);
        let mut variants = Vec::new();
        let mut changed = key.clone();
        changed.context = "different-declared-context".into();
        variants.push(changed);
        let mut changed = key.clone();
        changed.context_digest = ContentDigest::of(b"changed full context including metadata");
        variants.push(changed);
        let mut changed = key.clone();
        changed.dockerfile = "Dockerfile.other".into();
        variants.push(changed);
        let mut changed = key.clone();
        changed.dockerfile_digest = ContentDigest::of(b"changed Dockerfile");
        variants.push(changed);
        let mut changed = key.clone();
        changed.closure = ContentDigest::of(b"changed tool closure");
        variants.push(changed);
        let mut changed = key.clone();
        changed.platform = "other-platform";
        variants.push(changed);
        let mut changed = key.clone();
        changed.epoch = Some(0);
        variants.push(changed);
        for variant in variants {
            successful_source(&mut sources, variant, || {
                calls += 1;
                Ok(23)
            })
            .unwrap();
        }
        assert_eq!(calls, 8);
        for error in [
            Error::Execution("import failed".into()),
            Error::PreparationCancelled,
            Error::PreparationCleanupUnverified("cancelled builder; tag retained".into()),
        ] {
            let mut fresh = BTreeMap::<BuildKey, i32>::new();
            let result = successful_source(&mut fresh, key.clone(), || Err(error));
            assert!(result.is_err());
            assert!(fresh.is_empty());
            assert_eq!(
                successful_source(&mut fresh, key.clone(), || Ok(29)).unwrap(),
                29
            );
            assert_eq!(fresh.len(), 1);
        }
        assert!(reuse_closure(&OciTools::default()).unwrap().is_none());
    }
}
