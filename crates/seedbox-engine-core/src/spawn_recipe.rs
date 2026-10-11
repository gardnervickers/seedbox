//! A spawned machine's initramfs: its source's archive with the launch recipe
//! rewritten for the new machine and appended as a second newc archive. The
//! kernel unpacks concatenated archives in order, so the later recipe
//! replaces the source's before init reads it.
use crate::world::SpawnSpec;

/// The recipe version the guest init reads.
const MAGIC: &[u8; 10] = b"SBXLAUNCH5";
const RECIPE: &str = "seedbox/launch.bin";

/// The guest init's launch recipe (`SBXLAUNCH5`). Configs stay as encoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recipe {
    pub ip: Vec<u8>,
    pub hosts: Vec<u8>,
    pub hostname: Vec<u8>,
    pub user: Vec<u8>,
    pub working_dir: Vec<u8>,
    pub max_map_count: u32,
    pub wall_clock_epoch: u32,
    pub blocks_per_progress_call: u32,
    pub argv: Vec<Vec<u8>>,
    pub environment: Vec<Vec<u8>>,
    pub volumes: Vec<Vec<u8>>,
    pub disk_volumes: Vec<Vec<u8>>,
    pub configs: Vec<u8>,
}

impl Recipe {
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let rest = match bytes.split_first_chunk::<10>() {
            Some((magic, rest)) if magic == MAGIC => rest,
            _ => return Err("launch recipe version".into()),
        };
        let mut r = Reader(rest);
        Ok(Self {
            ip: r.field()?,
            hosts: r.field()?,
            hostname: r.field()?,
            user: r.field()?,
            working_dir: r.field()?,
            max_map_count: r.u32()?,
            wall_clock_epoch: r.u32()?,
            blocks_per_progress_call: r.u32()?,
            argv: r.list()?,
            environment: r.list()?,
            volumes: r.list()?,
            disk_volumes: r.list()?,
            configs: r.0.to_vec(),
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        for value in [
            &self.ip,
            &self.hosts,
            &self.hostname,
            &self.user,
            &self.working_dir,
        ] {
            field(&mut out, value);
        }
        out.extend_from_slice(&self.max_map_count.to_le_bytes());
        out.extend_from_slice(&self.wall_clock_epoch.to_le_bytes());
        out.extend_from_slice(&self.blocks_per_progress_call.to_le_bytes());
        for items in [
            &self.argv,
            &self.environment,
            &self.volumes,
            &self.disk_volumes,
        ] {
            out.extend_from_slice(&(items.len() as u32).to_le_bytes());
            for item in items {
                field(&mut out, item);
            }
        }
        out.extend_from_slice(&self.configs);
        out
    }

    /// The source's recipe as the spawned machine runs it: its own address
    /// and hostname, a hosts line for itself, environment overrides in place
    /// (new names appended in order) and, when given, a new argv.
    pub fn spawned(&self, spec: &SpawnSpec<'_>) -> Self {
        let mut recipe = self.clone();
        recipe.ip = format!("{}/24", spec.address).into_bytes();
        recipe.hostname = spec.name.as_bytes().to_vec();
        if !recipe.hosts.is_empty() && !recipe.hosts.ends_with(b"\n") {
            recipe.hosts.push(b'\n');
        }
        recipe
            .hosts
            .extend_from_slice(format!("{} {}\n", spec.address, spec.name).as_bytes());
        for (key, value) in spec.env {
            let entry = format!("{key}={value}").into_bytes();
            let prefix = format!("{key}=");
            match recipe
                .environment
                .iter_mut()
                .find(|e| e.starts_with(prefix.as_bytes()))
            {
                Some(existing) => *existing = entry,
                None => recipe.environment.push(entry),
            }
        }
        if let Some(argv) = spec.argv {
            recipe.argv = argv.iter().map(|a| a.as_bytes().to_vec()).collect();
        }
        recipe
    }
}

/// The source initramfs with the recipe rewritten for `spec` appended.
pub fn spawned_initrd(base: &[u8], spec: &SpawnSpec<'_>) -> Result<Vec<u8>, String> {
    let recipe = Recipe::parse(find(base, RECIPE)?)?.spawned(spec).encode();
    let mut out = Vec::with_capacity(base.len() + recipe.len() + 512);
    out.extend_from_slice(base);
    out.resize(out.len().next_multiple_of(4), 0);
    entry(&mut out, RECIPE, 1, 0o100400, &recipe);
    entry(&mut out, "TRAILER!!!", 2, 0, &[]);
    Ok(out)
}

/// The data of the first newc entry named `name`.
pub fn find<'a>(archive: &'a [u8], name: &str) -> Result<&'a [u8], String> {
    let mut at = 0;
    loop {
        let header = archive
            .get(at..at + 110)
            .ok_or("initramfs ends before the launch recipe")?;
        if &header[..6] != b"070701" {
            return Err("initramfs is not an uncompressed newc archive".into());
        }
        let hex = |i: usize| {
            std::str::from_utf8(&header[6 + 8 * i..14 + 8 * i])
                .ok()
                .and_then(|h| usize::from_str_radix(h, 16).ok())
                .ok_or("initramfs header field")
        };
        let (size, name_len) = (hex(6)?, hex(11)?);
        let data = (at + 110 + name_len).next_multiple_of(4);
        let entry = archive
            .get(at + 110..at + 110 + name_len)
            .ok_or("initramfs entry name")?;
        let entry = entry.strip_suffix(&[0]).ok_or("initramfs entry name")?;
        if entry == b"TRAILER!!!" {
            return Err(format!("initramfs has no {name}"));
        }
        if entry == name.as_bytes() {
            return archive
                .get(data..data + size)
                .ok_or_else(|| "initramfs entry data".into());
        }
        at = (data + size).next_multiple_of(4);
    }
}

/// Append one newc entry owned by root, with no timestamp.
pub fn entry(out: &mut Vec<u8>, name: &str, inode: u32, mode: u32, data: &[u8]) {
    let header = format!(
        "070701{inode:08x}{mode:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}",
        0,
        0,
        1,
        0,
        data.len(),
        0,
        0,
        0,
        0,
        name.len() + 1,
        0
    );
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    out.resize(out.len().next_multiple_of(4), 0);
    out.extend_from_slice(data);
    out.resize(out.len().next_multiple_of(4), 0);
}

fn field(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

struct Reader<'a>(&'a [u8]);
impl Reader<'_> {
    fn u32(&mut self) -> Result<u32, String> {
        let (head, tail) = self
            .0
            .split_first_chunk::<4>()
            .ok_or("launch recipe ends early")?;
        self.0 = tail;
        Ok(u32::from_le_bytes(*head))
    }
    fn field(&mut self) -> Result<Vec<u8>, String> {
        let len = self.u32()? as usize;
        if len > self.0.len() {
            return Err("launch recipe ends early".into());
        }
        let (head, tail) = self.0.split_at(len);
        self.0 = tail;
        Ok(head.to_vec())
    }
    fn list(&mut self) -> Result<Vec<Vec<u8>>, String> {
        let n = self.u32()?;
        if n > 65536 {
            return Err("too many launch items".into());
        }
        (0..n).map(|_| self.field()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn recipe() -> Recipe {
        Recipe {
            ip: b"10.0.0.1/24".to_vec(),
            hosts: b"127.0.0.1 localhost\n10.0.0.1 etcd-1\n10.0.0.2 client\n".to_vec(),
            hostname: b"etcd-1".to_vec(),
            user: b"0:0".to_vec(),
            working_dir: b"/".to_vec(),
            max_map_count: 262144,
            wall_clock_epoch: 1_700_000_000,
            blocks_per_progress_call: 64,
            argv: vec![b"/usr/local/bin/etcd".to_vec()],
            environment: vec![b"ETCD_NAME=etcd-1".to_vec(), b"PATH=/bin".to_vec()],
            volumes: vec![],
            disk_volumes: vec![b"data".to_vec(), b"/data".to_vec()],
            configs: [
                1u32.to_le_bytes().as_slice(),
                &3u32.to_le_bytes(),
                b"/c",
                b"x",
            ]
            .concat(),
        }
    }

    fn archive(recipe: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        entry(&mut out, "seedbox", 1, 0o040755, &[]);
        entry(&mut out, "init", 2, 0o100755, b"\x7fELFinit");
        entry(&mut out, RECIPE, 3, 0o100400, recipe);
        entry(&mut out, "rootfs", 4, 0o040755, &[]);
        entry(&mut out, "TRAILER!!!", 5, 0, &[]);
        out
    }

    #[test]
    fn a_spawned_recipe_has_its_own_identity_and_overrides() {
        assert_eq!(Recipe::parse(&recipe().encode()).unwrap(), recipe());
        let names = ["etcd-1", "client", "etcd-4"].map(String::from);
        let env = BTreeMap::from([
            ("ETCD_NAME".to_string(), "etcd-4".to_string()),
            (
                "ETCD_INITIAL_CLUSTER_STATE".to_string(),
                "existing".to_string(),
            ),
        ]);
        let spec = SpawnSpec {
            name: "etcd-4",
            index: 2,
            mac: [0x52, 0x54, 0, 0, 0, 3],
            address: "10.0.0.3".into(),
            names: &names,
            env: &env,
            argv: None,
        };
        let base = archive(&recipe().encode());
        let spawned = spawned_initrd(&base, &spec).unwrap();
        assert!(spawned.starts_with(&base));
        assert_eq!(spawned.len() % 4, 0);
        let tail = &spawned[base.len()..];
        let got = Recipe::parse(find(tail, RECIPE).unwrap()).unwrap();
        let mut want = recipe();
        want.ip = b"10.0.0.3/24".to_vec();
        want.hostname = b"etcd-4".to_vec();
        want.hosts.extend_from_slice(b"10.0.0.3 etcd-4\n");
        want.environment = vec![
            b"ETCD_NAME=etcd-4".to_vec(),
            b"PATH=/bin".to_vec(),
            b"ETCD_INITIAL_CLUSTER_STATE=existing".to_vec(),
        ];
        assert_eq!(got, want);
        let argv = ["/bin/sh".to_string(), "-c".into(), "true".into()];
        let spec = SpawnSpec {
            argv: Some(&argv),
            ..spec
        };
        let tail = spawned_initrd(&base, &spec).unwrap()[base.len()..].to_vec();
        let got = Recipe::parse(find(&tail, RECIPE).unwrap()).unwrap();
        assert_eq!(got.argv, argv.map(String::into_bytes));
    }

    #[test]
    fn a_compressed_or_recipeless_archive_is_rejected() {
        assert!(find(b"\x1f\x8b\x08\x00", RECIPE).is_err());
        let mut out = Vec::new();
        entry(&mut out, "init", 1, 0o100755, b"x");
        entry(&mut out, "TRAILER!!!", 2, 0, &[]);
        assert_eq!(
            find(&out, RECIPE).unwrap_err(),
            "initramfs has no seedbox/launch.bin"
        );
        assert!(Recipe::parse(b"SBXLAUNCH4").is_err());
    }
}
