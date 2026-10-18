//! Private in-memory sector store: an immutable owned base under a durable
//! overlay and an unflushed (volatile) overlay, in 512-byte sectors.
//!
//! All operations complete synchronously and touch no host storage. A
//! completed write is unflushed; reads see it at once. [`Store::flush`] makes
//! every earlier completed write durable. [`Store::discard_unflushed`] drops
//! every unflushed write and keeps durable data; when a machine applies it is
//! not decided here. [`Clone`] is an owned capture: it preserves both layers
//! separately, never flushes, and stays independent of later source changes.
//! A zero sector is ordinary data, never a hole that exposes older layers.
//! Failed operations change nothing.

// TODO: Re-evaluate: separate backends consume this canonical helper.
// Crate-private access cannot cross packages; representation remains private.
use std::{collections::BTreeMap, sync::Arc};

// Explicit canonical paths also support the standalone QEMU Rust FFI build.
// The PV worker imports this store through its common library dependency.
#[cfg_attr(seedbox_disk_abi, path = "disk/codec.rs")]
pub mod codec;
// Only the QEMU static library (`--cfg seedbox_disk_abi`) exports this ABI.
#[cfg(any(test, seedbox_disk_abi))]
#[cfg_attr(seedbox_disk_abi, path = "disk/abi.rs")]
mod abi;
#[cfg_attr(seedbox_disk_abi, path = "disk/request.rs")]
pub mod request;

pub const SECTOR: usize = 512;

/// One immutable sector. Writes replace sectors and never mutate them, so
/// captures may share them.
type Sector = Arc<[u8; SECTOR]>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The request reaches past the last sector.
    Range,
    /// A transfer length is not a whole number of sectors.
    Alignment,
    /// A base would have no sectors.
    Empty,
}

#[derive(Clone)]
enum Base {
    Zero,
    Image(Arc<[u8]>),
}

// TODO: Re-evaluate: one backend-neutral store keeps durability semantics in a
// single place for future block devices; the frozen Machine contract alone
// cannot share them. Private, synchronous and local: it fixes no device
// latency, boot-end policy, wire format or public definition.
#[derive(Clone)]
pub struct Store {
    base: Base,
    capacity: u64,
    durable: BTreeMap<u64, Sector>,
    unflushed: BTreeMap<u64, Sector>,
}

impl Store {
    /// A store of `sectors` zero sectors.
    pub fn zeroed(sectors: u64) -> Result<Self, Error> {
        if sectors == 0 {
            return Err(Error::Empty);
        }
        Ok(Self::over(Base::Zero, sectors))
    }

    /// A store over an immutable image of one or more whole sectors. An
    /// `Arc` image is shared, not copied; captures share it too.
    pub fn from_image(image: impl Into<Arc<[u8]>>) -> Result<Self, Error> {
        let image: Arc<[u8]> = image.into();
        if image.is_empty() {
            return Err(Error::Empty);
        }
        if !image.len().is_multiple_of(SECTOR) {
            return Err(Error::Alignment);
        }
        let sectors = (image.len() / SECTOR) as u64;
        Ok(Self::over(Base::Image(image), sectors))
    }

    fn over(base: Base, capacity: u64) -> Self {
        Self {
            base,
            capacity,
            durable: BTreeMap::new(),
            unflushed: BTreeMap::new(),
        }
    }

    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Validate a whole transfer before any effect.
    fn check(&self, sector: u64, length: usize) -> Result<(), Error> {
        if !length.is_multiple_of(SECTOR) {
            return Err(Error::Alignment);
        }
        match sector.checked_add((length / SECTOR) as u64) {
            Some(end) if end <= self.capacity => Ok(()),
            _ => Err(Error::Range),
        }
    }

    fn sector(&self, index: u64) -> &[u8] {
        if let Some(data) = self
            .unflushed
            .get(&index)
            .or_else(|| self.durable.get(&index))
        {
            return &data[..];
        }
        match &self.base {
            Base::Zero => &[0; SECTOR],
            Base::Image(image) => {
                let at = index as usize * SECTOR;
                &image[at..at + SECTOR]
            }
        }
    }

    /// Read whole sectors from `sector`: the latest completed write, flushed
    /// or not. On error `buffer` is unchanged.
    pub fn read(&self, sector: u64, buffer: &mut [u8]) -> Result<(), Error> {
        self.check(sector, buffer.len())?;
        // Enumerate the chunks: an open sector range would overflow past u64::MAX.
        for (offset, out) in buffer.as_chunks_mut::<SECTOR>().0.iter_mut().enumerate() {
            out.copy_from_slice(self.sector(sector + offset as u64));
        }
        Ok(())
    }

    /// Complete a write of whole sectors. It stays unflushed until a flush.
    pub fn write(&mut self, sector: u64, data: &[u8]) -> Result<(), Error> {
        self.check(sector, data.len())?;
        for (offset, chunk) in data.as_chunks::<SECTOR>().0.iter().enumerate() {
            self.unflushed
                .insert(sector + offset as u64, Arc::new(*chunk));
        }
        Ok(())
    }

    /// Complete a flush: every earlier completed write becomes durable.
    pub fn flush(&mut self) {
        self.durable.append(&mut self.unflushed);
    }

    /// Capture the surviving disk after selecting the latest unflushed sectors.
    ///
    /// Visits each latest unflushed sector exactly once in ascending sector
    /// order. Selected sectors become durable in the returned independent
    /// store; all other unflushed sectors are lost. The count is the number
    /// lost, not the number of write requests. The source is unchanged and
    /// this helper draws no randomness: the caller owns the dedicated policy
    /// stream, including the zero-draw default. Reads of actual sector bytes
    /// also permit stopped disk evidence without hashing a snapshot encoding.
    // TODO: Re-evaluate: both backend boot-end policies need the same ordered
    // selection while store representation stays private; cloning and reading
    // alone cannot observe an unflushed overwrite equal to durable bytes.
    pub fn crashed_keeping(&self, mut survives: impl FnMut(u64, &[u8]) -> bool) -> (Self, u64) {
        let mut next = self.clone();
        next.unflushed.clear();
        let mut lost = 0;
        for (&sector, bytes) in &self.unflushed {
            if survives(sector, &bytes[..]) {
                next.durable.insert(sector, bytes.clone());
            } else {
                lost += 1;
            }
        }
        (next, lost)
    }

    /// Drop every unflushed write; durable data and the base are kept.
    pub fn discard_unflushed(&mut self) {
        self.unflushed.clear();
    }
}

#[cfg(test)]
mod tests;
