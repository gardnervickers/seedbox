//! Seeded boot-end loss and stopped disk evidence, shared by both backends.
use super::{Base, Error, Store, SECTOR};
use sha2::{Digest, Sha256};

impl Store {
    /// Digest of the actual stable sectors and the ordered latest unflushed
    /// sectors, never of a snapshot encoding. `torn` is the policy's
    /// torn-write setting, which selects how a crash would treat them.
    pub fn state_digest(&self, torn: bool) -> Result<String, Error> {
        let mut volatile = Sha256::new();
        let mut count = 0u64;
        for (&sector, bytes) in &self.unflushed {
            volatile.update(sector.to_le_bytes());
            volatile.update(&bytes[..]);
            count += 1;
        }
        let mut all = Sha256::new();
        all.update(b"seedbox-disk-state-v1");
        all.update(self.capacity().to_le_bytes());
        all.update([u8::from(torn)]);
        // Hash the same stable sector stream without looking up and copying
        // every sector. Unflushed writes belong only to the separate stream
        // below; a durable zero overwrite still replaces its base sector.
        let mut next = 0;
        for (&index, bytes) in &self.durable {
            self.digest_base(&mut all, next, index);
            all.update(&bytes[..]);
            next = index + 1;
        }
        self.digest_base(&mut all, next, self.capacity());
        all.update(count.to_le_bytes());
        all.update(volatile.finalize());
        Ok(crate::machine::hex(&all.finalize()))
    }

    fn digest_base(&self, digest: &mut Sha256, start: u64, end: u64) {
        match &self.base {
            Base::Image(image) => {
                // Image capacity is derived from its addressable byte length.
                digest.update(&image[start as usize * SECTOR..end as usize * SECTOR]);
            }
            Base::Zero => {
                // Count sectors rather than bytes: a zero store may have a
                // capacity whose byte length does not fit in u64 or usize.
                static ZERO: [u8; 64 * 1024] = [0; 64 * 1024];
                const SECTORS: u64 = (ZERO.len() / SECTOR) as u64;
                let mut remaining = end - start;
                while remaining >= SECTORS {
                    digest.update(&ZERO[..]);
                    remaining -= SECTORS;
                }
                digest.update(&ZERO[..remaining as usize * SECTOR]);
            }
        }
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

#[cfg(test)]
mod tests;
