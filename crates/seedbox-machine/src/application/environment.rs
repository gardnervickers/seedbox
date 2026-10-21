//! Concrete per-guest setup packaged separately from the image's launch recipe.
use super::{invalid, DefinitionError};
use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU32,
};

/// A named directory on this machine's one retained disk and its image target.
/// Names are single components; targets are normalized absolute Linux paths.
/// Creation inherits target directory metadata, never its contents. This does
/// not denote a host mount or sharing between machines.
// Kept out of MachineDefinition so Driver definition codecs stay independent
// of service setup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamedVolume {
    pub name: String,
    pub target: String,
}
/// Visible application limits for configured guests. The translator reserves
/// descriptors within this limit; it does not virtualize getrlimit.
/// Default is max_map_count=262144 and nofile=1048576.
/// Linux admission remains fallible in the guest.
// Concrete bootstrap settings preserve container behavior
// without adding execution settings or environment-dependent machine identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GuestLimits {
    pub max_map_count: NonZeroU32,
    pub nofile: NonZeroU32,
}
impl Default for GuestLimits {
    fn default() -> Self {
        Self {
            max_map_count: NonZeroU32::new(262144).expect("nonzero constant"),
            nofile: NonZeroU32::new(1048576).expect("nonzero constant"),
        }
    }
}
/// Owned guest setup, separate from application launch and backend resources.
/// Configs are arbitrary bytes written root-owned 0444 and read-only bind mounted;
/// anonymous volumes are ordinary directories, not tmpfs. Named volumes require
/// a supplied disk and static guest formatter. Setup runs once per boot before
/// Ready/application startup; process Stop/Start retains setup and applies nofile
/// to each new child. The explicit socket flag affects only the guest listener,
/// not authorization of host control requests. No host path is extracted/mounted.
/// Final values are validated by configured packaging/factory creation.
/// The complete record fits the newc per-file u32 size bound. Each field is
/// at most 16 MiB, each collection at most 65,536 entries, and paths shorter
/// than 4,096 bytes. Config bytes borrow one loader-owned record through setup;
/// text/array/index accounting is derived from the actual record/schema bounds,
/// not an additional aggregate payload quota or RSS/native-resource guarantee.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestEnvironment {
    pub hostname: String,
    pub hosts: String,
    pub anonymous_volumes: Vec<String>,
    pub named_volumes: Vec<NamedVolume>,
    pub configs: BTreeMap<String, Vec<u8>>,
    pub limits: GuestLimits,
    pub control_socket: bool,
}
impl GuestEnvironment {
    /// Construct explicit hostname/hosts/socket settings with empty mounts and
    /// the container-parity limits. Validation occurs when packaged, including
    /// changes made to public fields after construction. This is not the absent
    /// environment used by ordinary Factory::create.
    pub fn new(
        hostname: impl Into<String>,
        hosts: impl Into<String>,
        control_socket: bool,
    ) -> Self {
        Self {
            hostname: hostname.into(),
            hosts: hosts.into(),
            anonymous_volumes: vec![],
            named_volumes: vec![],
            configs: BTreeMap::new(),
            limits: GuestLimits::default(),
            control_socket,
        }
    }
    /// Named volumes are formatted onto the machine disk (`/dev/vda`) by the
    /// guest asset `mke2fs`. Reject a configuration missing either attachment
    /// before preparing a machine.
    pub fn check_attachments(&self, disk: bool, formatter: bool) -> Result<(), &'static str> {
        if !self.named_volumes.is_empty() {
            if !disk {
                return Err("named guest volumes require a machine disk at /dev/vda");
            }
            if !formatter {
                return Err(
                    "named guest volumes require guest asset mke2fs; package the static guest formatter",
                );
            }
        }
        Ok(())
    }
}

pub(super) const MAGIC: &[u8; 8] = b"SBXENV1\0";
const NEWC_FILE_MAX: u64 = u32::MAX as u64;
const FIELD: usize = 16 * 1024 * 1024;
const COUNT: usize = 65536;
const ITEMS: usize = 3 * COUNT;
const PATH: usize = 4096;
fn text(value: &str) -> Result<(), DefinitionError> {
    if value.len() > FIELD || value.as_bytes().contains(&0) {
        return Err(invalid(
            "guest environment text is too large or contains NUL",
        ));
    }
    Ok(())
}
fn absolute(value: &str, root: bool) -> Result<(), DefinitionError> {
    text(value)?;
    if !value.starts_with('/')
        || value.len() >= PATH
        || (!root && value == "/")
        || (value != "/"
            && value
                .split('/')
                .skip(1)
                .any(|s| s.is_empty() || s == "." || s == ".."))
    {
        return Err(invalid(format!(
            "guest environment path {value:?}: expected normalized absolute Linux path"
        )));
    }
    Ok(())
}
fn below(path: &str, parent: &str) -> bool {
    path == parent
        || path
            .strip_prefix(parent)
            .is_some_and(|s| s.starts_with('/'))
}
fn ancestors(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/')
        .filter_map(move |(i, _)| (i > 0).then_some(&path[..i]))
}
fn config_target(path: &str, runtime: bool) -> Result<(), DefinitionError> {
    absolute(path, false)?;
    if path == "/etc/hosts"
        || ["/dev", "/proc", "/sys"].iter().any(|p| below(path, p))
        || (runtime && below(path, "/seedbox/progress"))
    {
        return Err(invalid(format!(
            "guest environment config target {path:?}: reserved runtime path"
        )));
    }
    Ok(())
}
fn validate(env: &GuestEnvironment, runtime: bool) -> Result<(), DefinitionError> {
    text(&env.hostname)?;
    text(&env.hosts)?;
    if env.hostname.is_empty()
        || env.hostname.len() > 64
        || env
            .hostname
            .bytes()
            .any(|b| b <= 32 || b == 127 || b == b'/')
    {
        return Err(invalid(
            "guest hostname must contain 1..64 bytes without whitespace, slash or NUL",
        ));
    }
    let sizes = [
        env.anonymous_volumes.len(),
        env.named_volumes.len(),
        env.configs.len(),
    ];
    if sizes.iter().any(|n| *n > COUNT)
        || sizes
            .iter()
            .try_fold(0usize, |a, n| a.checked_add(*n))
            .is_none_or(|n| n > ITEMS)
    {
        return Err(invalid("guest environment collection limit exceeded"));
    }
    let mut directories = BTreeSet::new();
    for path in &env.anonymous_volumes {
        absolute(path, true)?;
        if !directories.insert(path.as_str()) {
            return Err(invalid("duplicate anonymous volume directory"));
        }
    }
    let mut names = BTreeSet::new();
    let mut targets = BTreeSet::new();
    for volume in &env.named_volumes {
        text(&volume.name)?;
        if volume.name.is_empty()
            || matches!(volume.name.as_str(), "." | "..")
            || volume.name.contains('/')
            || volume.name.len() > 255
            || volume.name.len() + "/seedbox/disk/".len() >= PATH
        {
            return Err(invalid(
                "named volume name must be one nonempty Linux component",
            ));
        }
        absolute(&volume.target, false)?;
        if volume.target.len() + "/rootfs".len() >= PATH {
            return Err(invalid(
                "named volume image target exceeds Linux path bound",
            ));
        }
        if !names.insert(volume.name.as_str()) || !targets.insert(volume.target.as_str()) {
            return Err(invalid("duplicate named volume name or target"));
        }
    }
    // Image anonymous VOLUME paths may equal or lie below a named target;
    // guest setup creates them after mounting named directories. Index config
    // ancestors instead of a pairwise scan of attacker-controlled collections.
    let configs: BTreeSet<_> = env.configs.keys().map(String::as_str).collect();
    for (path, data) in &env.configs {
        config_target(path, runtime)?;
        if data.len() > FIELD {
            return Err(invalid("guest config content exceeds field limit"));
        }
        if directories.contains(path.as_str())
            || targets.contains(path.as_str())
            || ancestors(path).any(|p| targets.contains(p))
        {
            return Err(invalid("config target overlaps a volume target"));
        }
    }
    for path in env
        .anonymous_volumes
        .iter()
        .map(String::as_str)
        .chain(env.named_volumes.iter().map(|v| v.target.as_str()))
        .chain(env.configs.keys().map(String::as_str))
    {
        if ancestors(path).any(|p| configs.contains(p)) {
            return Err(invalid(
                "config file target is an ancestor of another setup target",
            ));
        }
    }
    Ok(())
}
/// Private closed recipe. Explicit framing and fixed schema bound the guest
/// parser; byte limits are not allocator/RSS or native resource guarantees.
pub(super) fn recipe(
    env: &GuestEnvironment,
    prefix: &[String],
) -> Result<Vec<u8>, DefinitionError> {
    let runtime = prefix
        .first()
        .is_some_and(|p| p == "/seedbox/progress/bin64/drrun");
    validate(env, runtime)?;
    // DynamoRIO moves its private descriptors to the top of the supplied range,
    // but leaves getrlimit/prlimit results unchanged. Adding its reserve here
    // would expose a different application limit on PV than on TCG.
    let nofile = env.limits.nofile.get();
    let mut length = 4 + env.hostname.len() + 4 + env.hosts.len() + 4 + 4 + 1 + 1 + 4 + 4 + 4;
    for value in &env.anonymous_volumes {
        length = add(length, 4 + value.len())?;
    }
    for value in &env.named_volumes {
        length = add(length, 8 + value.name.len() + value.target.len())?;
    }
    for (target, data) in &env.configs {
        length = add(length, 8 + target.len() + data.len())?;
    }
    let total = record_length(length)?;
    let mut out = Vec::new();
    out.try_reserve_exact(total)
        .map_err(|_| invalid("guest environment recipe allocation failed"))?;
    out.extend(MAGIC);
    out.extend((length as u64).to_le_bytes());
    field(&mut out, env.hostname.as_bytes());
    field(&mut out, env.hosts.as_bytes());
    out.extend(env.limits.max_map_count.get().to_le_bytes());
    out.extend(nofile.to_le_bytes());
    out.push(u8::from(env.control_socket));
    out.push(u8::from(runtime));
    out.extend((env.anonymous_volumes.len() as u32).to_le_bytes());
    for value in &env.anonymous_volumes {
        field(&mut out, value.as_bytes());
    }
    out.extend((env.named_volumes.len() as u32).to_le_bytes());
    for value in &env.named_volumes {
        field(&mut out, value.name.as_bytes());
        field(&mut out, value.target.as_bytes());
    }
    out.extend((env.configs.len() as u32).to_le_bytes());
    for (target, data) in &env.configs {
        field(&mut out, target.as_bytes());
        field(&mut out, data);
    }
    debug_assert_eq!(out.len(), total);
    Ok(out)
}
// Check complete newc file size, including framing, before any record reserve.
fn record_length(body: usize) -> Result<usize, DefinitionError> {
    let total = add(body, 16)?;
    if u64::try_from(total).map_or(true, |n| n > NEWC_FILE_MAX) {
        return Err(invalid("guest environment record exceeds newc file size"));
    }
    Ok(total)
}
fn add(a: usize, b: usize) -> Result<usize, DefinitionError> {
    a.checked_add(b)
        .ok_or_else(|| invalid("guest environment size overflow"))
}
fn field(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend((bytes.len() as u32).to_le_bytes());
    out.extend(bytes);
}

#[cfg(test)]
mod tests;
