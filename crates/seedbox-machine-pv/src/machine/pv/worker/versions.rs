//! Retained snapshots: whole stopped machines kept in this worker's memory.
//!
//! A version holds everything a capture file holds. Its RAM is a [`Tree`]
//! that shares every unchanged page with the other versions, so taking one
//! costs the pages written since the last, and going back to one costs the
//! pages in which live RAM can differ from it. An exported version is, byte
//! for byte, the file a capture at that moment would have written.
//!
//! Tracking starts at the first snapshot kept. From then on live RAM equals
//! `current` everywhere but on the pages in the mapping's dirty set: guest
//! writes reach that set through the backend, host writes through
//! [`super::memory::Ram`].
mod tree;
use super::{arch, block, write_snapshot, Backend, Capture, Machine, Result, Store};
use std::{collections::BTreeMap, path::Path};
pub(super) use tree::Full;
use tree::{Pages, Tree};

const UNKNOWN: &str = "PV unknown retained snapshot";
/// One retained snapshot.
struct Version {
    capture: Capture,
    /// The PV page.
    page: Box<[u8]>,
    ram: Tree,
    disk: Option<Store>,
}
/// The retained snapshots of one machine.
pub(super) struct Versions {
    pages: Pages,
    held: BTreeMap<u64, Version>,
    /// The version last taken or rewound to; `None` before the first.
    current: Option<Tree>,
    next: u64,
}
/// A snapshot that was taken.
pub(super) struct Taken {
    pub(super) id: u64,
    /// RAM pages it had to copy.
    pub(super) pages: usize,
}
impl Versions {
    pub(super) fn new() -> Self {
        Self {
            pages: Pages::new(),
            held: BTreeMap::new(),
            current: None,
            next: 0,
        }
    }
    /// Bytes of RAM page contents the versions hold together; a page they
    /// share counts once.
    pub(super) fn held(&self) -> u64 {
        self.pages.held()
    }
    /// Forget a version, freeing the pages no other version shares.
    pub(super) fn release(&mut self, id: u64) -> Result<()> {
        self.held
            .remove(&id)
            .map(drop)
            .ok_or_else(|| UNKNOWN.into())
    }
}
impl<B: Backend> Machine<B> {
    /// Keep the stopped machine as a version, copying the RAM pages written
    /// since the last one. [`Full`] if the versions would then hold more than
    /// `limit` bytes of RAM pages: nothing is kept and nothing is lost.
    ///
    /// `verify` also compares all of RAM with the version, which fails only
    /// if a write escaped tracking.
    pub(super) fn snapshot(
        &mut self,
        limit: u64,
        verify: bool,
    ) -> Result<std::result::Result<Taken, Full>> {
        let capture = self.header()?;
        let tracked = self.ram.tracked();
        if tracked {
            self.native.collect()?;
        }
        let written = self.ram.take_dirty();
        let pages = self.ram.pages();
        // The first version is all of RAM over an empty one.
        let (base, written) = match &self.versions.current {
            Some(current) => (current.clone(), written),
            None => (Tree::zero(pages), (0..pages as u32).collect()),
        };
        // The vCPU is stopped and no host borrow of RAM is outstanding.
        let live = unsafe { self.ram.bytes() };
        let version = self.versions.pages.version(
            &base,
            written.iter().map(|&page| page as usize),
            live,
            limit,
        );
        let (ram, copied) = match version {
            Ok(version) => version,
            Err(Full) => {
                // Still written since `current`.
                for &page in &written {
                    self.ram.mark_page(page as usize);
                }
                return Ok(Err(Full));
            }
        };
        // Writes are recorded from the first version kept: a refused first
        // snapshot withholds nothing from the guest afterwards.
        if !tracked {
            self.native.track()?;
            self.ram.track();
        }
        if verify {
            if let Some(page) = ram.mismatch(live) {
                // The dirty set no longer holds what was written since
                // `current`, and pages other than this first one may differ
                // too: count every page as written again.
                self.ram.mark(0, self.ram.len());
                return Err(format!("PV snapshot missed a write to RAM page {page}"));
            }
        }
        let id = self.versions.next;
        self.versions.next += 1;
        self.versions.held.insert(
            id,
            Version {
                capture,
                page: unsafe { self.page.bytes() }.into(),
                ram: ram.clone(),
                disk: self.block.as_ref().map(|block| block.store().clone()),
            },
        );
        self.versions.current = Some(ram);
        Ok(Ok(Taken { id, pages: copied }))
    }
    /// Make the machine the one this version was taken from, whatever it has
    /// done since, even ended its boot. Returns the RAM pages rewritten: those
    /// written since `current`, and those in which `current` and the version
    /// differ.
    ///
    /// A failure leaves a machine that cannot run but can be rewound again.
    /// `verify` also compares all of RAM with the version afterwards.
    pub(super) fn rewind(&mut self, id: u64, verify: bool) -> Result<usize> {
        let version = self.versions.held.get(&id).ok_or(UNKNOWN)?;
        let current = self
            .versions
            .current
            .as_ref()
            .ok_or("PV rewind before a snapshot")?;
        self.native.reset()?;
        current.differing(&version.ram, |page| self.ram.mark_page(page));
        let pages = self.ram.take_dirty();
        for &page in &pages {
            // No vCPU has entered since the reset.
            unsafe {
                self.ram
                    .rewind_page(page as usize, version.ram.page(page as usize))
            };
        }
        self.versions.current = Some(version.ram.clone());
        if verify {
            if let Some(page) = version.ram.mismatch(unsafe { self.ram.bytes() }) {
                // `current` is already the version, so a later rewind would
                // rewrite none of the pages that differ from it: count every
                // page as written.
                self.ram.mark(0, self.ram.len());
                return Err(format!("PV rewind left RAM page {page} differing"));
            }
        }
        unsafe {
            self.page
                .modify(|bytes| bytes.copy_from_slice(&version.page))
        };
        self.state = version.capture.state.clone();
        self.block = match (
            &version.capture.block,
            &version.disk,
            &version.capture.disk_faults,
        ) {
            (Some(transport), Some(store), Some(faults)) => Some(block::Block::restore(
                transport.clone(),
                store.clone(),
                faults.clone(),
            )),
            _ => None,
        };
        self.output.clear();
        self.ended = false;
        self.resume = false;
        arch::reset(&mut self.native, &self.entry)?;
        self.native.restore(&version.capture.architecture)?;
        Ok(pages.len())
    }
    /// Write a version as the snapshot file a capture would have written
    /// when it was taken.
    pub(super) fn export(&self, id: u64, path: &Path) -> Result<()> {
        let version = self.versions.held.get(&id).ok_or(UNKNOWN)?;
        write_snapshot(
            path,
            &version.capture,
            &version.page,
            self.ram.layout()?,
            |file, offset, count| version.ram.write(offset, count, file),
            version.disk.as_ref(),
        )
    }
}
