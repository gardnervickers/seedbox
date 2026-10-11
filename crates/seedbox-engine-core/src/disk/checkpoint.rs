//! A checkpoint keeps the two overlay layers distinct. It references the
//! immutable prepared image rather than duplicating it or flattening writes.
use super::*;

const MAGIC: &[u8; 8] = b"SBXDSNP1";

fn read<const N: usize>(input: &mut impl Read) -> Result<[u8; N], String> {
    let mut bytes = [0; N];
    input
        .read_exact(&mut bytes)
        .map_err(|e| format!("read disk checkpoint: {e}"))?;
    Ok(bytes)
}

impl Disk {
    /// Stream both durable and volatile sectors at a stopped checkpoint. This
    /// does not flush the simulated disk or sample its crash-loss policy.
    pub fn write_checkpoint(&self, mut output: impl Write) -> Result<(), String> {
        validate_disk_fault_model(self.fault_model).map_err(str::to_owned)?;
        let mut write = || -> std::io::Result<()> {
            output.write_all(MAGIC)?;
            output.write_all(&self.capacity.to_le_bytes())?;
            output.write_all(
                &self
                    .fault_model
                    .survival_probability
                    .to_bits()
                    .to_le_bytes(),
            )?;
            output.write_all(&[u8::from(self.base_sha256.is_some())])?;
            output.write_all(
                self.base_sha256
                    .as_deref()
                    .unwrap_or(&"0".repeat(64))
                    .as_bytes(),
            )?;
            for layer in [&self.stable, &self.volatile] {
                output.write_all(&(layer.len() as u64).to_le_bytes())?;
                for (&sector, data) in layer {
                    output.write_all(&sector.to_le_bytes())?;
                    output.write_all(&data[..])?;
                }
            }
            output.write_all(self.digest().as_bytes())?;
            output.flush()
        };
        write().map_err(|e| format!("write disk checkpoint: {e}"))
    }

    /// Restore over a fresh copy of the original prepared base. Each layer is
    /// bounded by its capacity and streamed sector-by-sector; partial data never
    /// escapes on failure. Caller bounds the containing artifact when embedded.
    pub fn read_checkpoint(mut base: Disk, mut input: impl Read) -> Result<Disk, String> {
        if !base.stable.is_empty() || !base.volatile.is_empty() {
            return Err("disk checkpoint destination is not a fresh base".into());
        }
        validate_disk_fault_model(base.fault_model).map_err(str::to_owned)?;
        if read::<8>(&mut input)? != *MAGIC {
            return Err("invalid disk checkpoint version".into());
        }
        let capacity = u64::from_le_bytes(read(&mut input)?);
        let survival = u64::from_le_bytes(read(&mut input)?);
        let has_base = read::<1>(&mut input)?[0];
        let hash = read::<64>(&mut input)?;
        if capacity != base.capacity
            || survival != base.fault_model.survival_probability.to_bits()
            || has_base != u8::from(base.base_sha256.is_some())
            || hash.as_slice()
                != base
                    .base_sha256
                    .as_deref()
                    .unwrap_or(&"0".repeat(64))
                    .as_bytes()
        {
            return Err("disk checkpoint base, capacity or fault model mismatch".into());
        }
        for layer in [&mut base.stable, &mut base.volatile] {
            let count = u64::from_le_bytes(read(&mut input)?);
            if count > capacity {
                return Err("disk checkpoint sector count exceeds capacity".into());
            }
            let mut previous = None;
            for _ in 0..count {
                let sector = u64::from_le_bytes(read(&mut input)?);
                if sector >= capacity || previous.is_some_and(|p| sector <= p) {
                    return Err("disk checkpoint sectors are unordered or out of range".into());
                }
                layer.insert(sector, Box::new(read::<SECTOR>(&mut input)?));
                previous = Some(sector);
            }
        }
        if read::<64>(&mut input)?.as_slice() != base.digest().as_bytes() {
            return Err("disk checkpoint digest mismatch".into());
        }
        let mut trailing = [0];
        if input
            .read(&mut trailing)
            .map_err(|e| format!("read disk checkpoint trailer: {e}"))?
            != 0
        {
            return Err("disk checkpoint has trailing data".into());
        }
        Ok(base)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_checkpoint_preserves_overwrites_and_future_crash_loss() {
        for survival_probability in [0.0, 0.5, 1.0] {
            let base = Disk::new(Some(vec![b'.'; SECTOR * 8]), None)
                .unwrap()
                .with_fault_model(DiskFaultModel {
                    sector_bytes: SECTOR,
                    survival_probability,
                });
            let mut source = base.clone();
            source.write(0, &[b'A'; SECTOR * 2]).unwrap();
            source.flush();
            source.write(1, &[b'B'; SECTOR * 3]).unwrap();
            let mut bytes = Vec::new();
            source.write_checkpoint(&mut bytes).unwrap();
            let restored = Disk::read_checkpoint(base.clone(), bytes.as_slice()).unwrap();
            assert_eq!(source.digest(), restored.digest());
            let mut visible = vec![0; SECTOR * 8];
            restored.read(0, &mut visible).unwrap();
            assert_eq!(&visible[..SECTOR], &[b'A'; SECTOR]);
            assert_eq!(&visible[SECTOR..4 * SECTOR], &[b'B'; 3 * SECTOR]);
            for seed in [0, 17, u64::MAX] {
                let (expected, lost) = source.crashed_seeded(seed, "db", 3);
                let (actual, actual_lost) = restored.crashed_seeded(seed, "db", 3);
                assert_eq!((actual.digest(), actual_lost), (expected.digest(), lost));
                if survival_probability == 0.0 {
                    let mut old = [0; SECTOR];
                    actual.read(1, &mut old).unwrap();
                    assert_eq!(
                        old, [b'A'; SECTOR],
                        "overwritten stable data survives a crash"
                    );
                }
            }
            assert!(Disk::read_crash_transfer(base.clone(), bytes.as_slice()).is_err());
            assert!(Disk::read_checkpoint(source.clone(), bytes.as_slice()).is_err());
            let mut corrupt = bytes.clone();
            let middle = corrupt.len() / 2;
            corrupt[middle] ^= 1;
            assert!(Disk::read_checkpoint(base.clone(), corrupt.as_slice()).is_err());
            assert!(Disk::read_checkpoint(base.clone(), &bytes[..bytes.len() - 1]).is_err());
            bytes.push(0);
            assert!(Disk::read_checkpoint(base, bytes.as_slice()).is_err());
        }
    }
}
