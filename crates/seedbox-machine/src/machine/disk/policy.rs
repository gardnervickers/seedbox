//! Seeded boot-end loss and stopped disk evidence, shared by both backends.
use super::{Error, Store, SECTOR};
use sha2::{Digest, Sha256};

impl Store {
    /// Digest of the actual stable sectors and the ordered latest unflushed
    /// sectors, never of a snapshot encoding. `torn` is the policy's
    /// torn-write setting, which selects how a crash would treat them.
    pub fn state_digest(&self, torn: bool) -> Result<String, Error> {
        let mut volatile = Sha256::new();
        let mut count = 0u64;
        let (stable, _) = self.crashed_keeping(|sector, bytes| {
            volatile.update(sector.to_le_bytes());
            volatile.update(bytes);
            count += 1;
            false
        });
        let mut all = Sha256::new();
        all.update(b"seedbox-disk-state-v1");
        all.update(self.capacity().to_le_bytes());
        all.update([u8::from(torn)]);
        let mut sector = [0u8; SECTOR];
        for index in 0..stable.capacity() {
            stable.read(index, &mut sector)?;
            all.update(sector);
        }
        all.update(count.to_le_bytes());
        all.update(volatile.finalize());
        Ok(crate::machine::hex(&all.finalize()))
    }

    /// Crash at the end of `boot`. Without `torn` (the policy seed and machine
    /// name) every unflushed sector is lost and no randomness is drawn. With
    /// it, each unflushed sector in ascending order survives with probability
    /// one half, drawn from the dedicated `disk-loss/<name>/<boot>` stream.
    pub fn crash(&self, torn: Option<(u64, &str)>, boot: u64) -> Result<(Self, u64), &'static str> {
        let Some((seed, name)) = torn else {
            return Ok(self.crashed_keeping(|_, _| false));
        };
        let mut key = Sha256::new();
        key.update(b"seedbox-v1");
        key.update(seed.to_le_bytes());
        key.update(format!("disk-loss/{name}/{boot}").as_bytes());
        let key = key.finalize();
        let mut counter = 0u64;
        let mut overflow = false;
        let result = self.crashed_keeping(|_, _| {
            let Some(next) = counter.checked_add(1) else {
                overflow = true;
                return false;
            };
            let mut draw = Sha256::new();
            draw.update(key);
            draw.update(counter.to_le_bytes());
            counter = next;
            let bytes = draw.finalize();
            let draw = u64::from_le_bytes(bytes[..8].try_into().expect("eight digest bytes"));
            // An f64 fraction compared with 0.5; its rounding near 0.5 is part
            // of the defined stream, so do not replace it with a bit test.
            (draw as f64 / (u64::MAX as f64 + 1.0)) < 0.5
        });
        if overflow {
            return Err("disk-loss counter overflow");
        }
        Ok(result)
    }
}
