//! Immutable versions of guest RAM that share every page they have in common.
//!
//! A version is a two-level tree of reference-counted pages; an absent page is
//! zero. Taking a version copies only the pages written since the version live
//! RAM last matched. Two versions can differ only where their slots hold
//! different pages, so a rewind finds what to put back by comparing
//! references, never contents. Releasing a version frees exactly the pages no
//! other version shares.
use crate::machine::pv::worker::memory::GRANULE;
use std::{cell::Cell, io::Write, rc::Rc};

/// Pages under one leaf: 4 MiB of RAM at 16 KiB pages, 2 MiB at 4 KiB.
#[cfg(target_arch = "aarch64")]
const FANOUT: usize = 256;
#[cfg(target_arch = "x86_64")]
const FANOUT: usize = 512;
static ZERO: [u8; GRANULE] = [0; GRANULE];

/// One immutable page, counted by the store that made it for as long as any
/// version holds it.
struct Page {
    bytes: Box<[u8]>,
    live: Rc<Cell<usize>>,
}
impl Drop for Page {
    fn drop(&mut self) {
        self.live.set(self.live.get() - 1);
    }
}
type Slot = Option<Rc<Page>>;
#[derive(Clone)]
struct Leaf([Slot; FANOUT]);

/// One version of RAM. A clone shares every page.
#[derive(Clone)]
pub(super) struct Tree {
    leaves: Vec<Option<Rc<Leaf>>>,
    pages: usize,
}
impl Tree {
    /// All of `pages` zero.
    pub(super) fn zero(pages: usize) -> Self {
        Self {
            leaves: vec![None; pages.div_ceil(FANOUT)],
            pages,
        }
    }
    fn slot(&self, page: usize) -> Option<&Rc<Page>> {
        self.leaves[page / FANOUT].as_ref()?.0[page % FANOUT].as_ref()
    }
    /// The page's contents; `None` is the zero page.
    pub(super) fn page(&self, page: usize) -> Option<&[u8]> {
        self.slot(page).map(|page| &*page.bytes)
    }
    fn set(&mut self, page: usize, slot: Slot) {
        let leaf = &mut self.leaves[page / FANOUT];
        if leaf.is_none() && slot.is_none() {
            return;
        }
        let leaf = leaf.get_or_insert_with(|| Rc::new(Leaf(std::array::from_fn(|_| None))));
        // Copies the leaf's references, not its pages, when another version shares it.
        Rc::make_mut(leaf).0[page % FANOUT] = slot;
    }
    /// Call `each` with every page whose contents can differ between the two
    /// versions: the pages whose slots do not hold the same page.
    pub(super) fn differing(&self, other: &Tree, mut each: impl FnMut(usize)) {
        debug_assert_eq!(self.pages, other.pages);
        for (index, pair) in self.leaves.iter().zip(&other.leaves).enumerate() {
            let (ours, theirs) = match pair {
                (None, None) => continue,
                (Some(ours), Some(theirs)) if Rc::ptr_eq(ours, theirs) => continue,
                (ours, theirs) => (ours.as_deref(), theirs.as_deref()),
            };
            for slot in 0..FANOUT {
                let same = match (
                    ours.and_then(|leaf| leaf.0[slot].as_ref()),
                    theirs.and_then(|leaf| leaf.0[slot].as_ref()),
                ) {
                    (None, None) => true,
                    (Some(ours), Some(theirs)) => Rc::ptr_eq(ours, theirs),
                    _ => false,
                };
                if !same {
                    each(index * FANOUT + slot);
                }
            }
        }
    }
    /// Write `count` bytes of this version starting at byte `offset`.
    pub(super) fn write(
        &self,
        offset: usize,
        count: usize,
        out: &mut impl Write,
    ) -> std::io::Result<()> {
        let mut at = offset;
        let end = offset + count;
        while at < end {
            let within = at % GRANULE;
            let length = (GRANULE - within).min(end - at);
            let page = self.page(at / GRANULE).unwrap_or(&ZERO);
            out.write_all(&page[within..within + length])?;
            at += length;
        }
        Ok(())
    }
    /// The first page of `live` RAM that differs from this version.
    pub(super) fn mismatch(&self, live: &[u8]) -> Option<usize> {
        (0..self.pages).find(|&page| {
            self.page(page).unwrap_or(&ZERO) != &live[page * GRANULE..(page + 1) * GRANULE]
        })
    }
}

/// A store's page budget is spent: release versions before taking another.
#[derive(Debug, PartialEq, Eq)]
pub(in super::super) struct Full;

/// Makes the pages of one worker's versions and counts those still held.
pub(super) struct Pages {
    live: Rc<Cell<usize>>,
}
impl Pages {
    pub(super) fn new() -> Self {
        Self {
            live: Rc::new(Cell::new(0)),
        }
    }
    /// Bytes of page contents held by every version together; a page shared
    /// by several versions counts once.
    pub(super) fn held(&self) -> u64 {
        self.live.get() as u64 * GRANULE as u64
    }
    /// `base` with each of `written` replaced by its contents in `live` RAM.
    /// A page that still equals `base` stays shared and an all-zero page is
    /// stored as absent. Returns the version and the pages it had to copy, or
    /// [`Full`] if the store would hold more than `limit` bytes.
    pub(super) fn version(
        &self,
        base: &Tree,
        written: impl IntoIterator<Item = usize>,
        live: &[u8],
        limit: u64,
    ) -> Result<(Tree, usize), Full> {
        let mut tree = base.clone();
        let mut copied = 0;
        for page in written {
            let bytes = &live[page * GRANULE..(page + 1) * GRANULE];
            if base.page(page).unwrap_or(&ZERO) == bytes {
                continue;
            }
            if zero(bytes) {
                tree.set(page, None);
                continue;
            }
            if self.held() + GRANULE as u64 > limit {
                return Err(Full);
            }
            self.live.set(self.live.get() + 1);
            copied += 1;
            tree.set(
                page,
                Some(Rc::new(Page {
                    bytes: bytes.into(),
                    live: self.live.clone(),
                })),
            );
        }
        Ok((tree, copied))
    }
}
fn zero(bytes: &[u8]) -> bool {
    let (chunks, rest) = bytes.as_chunks::<16>();
    chunks.iter().all(|chunk| u128::from_ne_bytes(*chunk) == 0) && rest.iter().all(|&b| b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGES: usize = FANOUT * 3 + 7;
    const UNLIMITED: u64 = u64::MAX;

    fn ram() -> Vec<u8> {
        vec![0; PAGES * GRANULE]
    }
    fn all() -> std::ops::Range<usize> {
        0..PAGES
    }
    fn differing(a: &Tree, b: &Tree) -> Vec<usize> {
        let mut pages = Vec::new();
        a.differing(b, |page| pages.push(page));
        pages
    }
    fn contents(tree: &Tree) -> Vec<u8> {
        let mut out = Vec::new();
        tree.write(0, PAGES * GRANULE, &mut out).unwrap();
        out
    }

    #[test]
    fn a_version_holds_only_non_zero_pages_and_reads_back_whole() {
        let store = Pages::new();
        let mut live = ram();
        live[5] = 1;
        live[(FANOUT + 3) * GRANULE + 100] = 2;
        live[PAGES * GRANULE - 1] = 3;
        let (tree, copied) = store
            .version(&Tree::zero(PAGES), all(), &live, UNLIMITED)
            .unwrap();
        assert_eq!(copied, 3);
        assert_eq!(store.held(), 3 * GRANULE as u64);
        assert_eq!(contents(&tree), live);
        assert_eq!(tree.mismatch(&live), None);
        assert!(tree.page(1).is_none());
        // A range that starts and ends inside pages.
        let mut part = Vec::new();
        tree.write(3, 2 * GRANULE, &mut part).unwrap();
        assert_eq!(part, live[3..3 + 2 * GRANULE]);
    }

    #[test]
    fn versions_share_unwritten_pages_and_differ_only_where_written() {
        let store = Pages::new();
        let mut live = ram();
        for page in [0, 1, FANOUT, 2 * FANOUT + 1] {
            live[page * GRANULE] = 1;
        }
        let (first, _) = store
            .version(&Tree::zero(PAGES), all(), &live, UNLIMITED)
            .unwrap();
        // Change one page, zero one, populate one, and rewrite one unchanged.
        live[GRANULE] = 9;
        live[FANOUT * GRANULE] = 0;
        live[7 * GRANULE + 1] = 4;
        let written = [1, FANOUT, 7, 0];
        let (second, copied) = store.version(&first, written, &live, UNLIMITED).unwrap();
        assert_eq!(copied, 2, "the changed and the populated page");
        assert_eq!(store.held(), 6 * GRANULE as u64);
        assert_eq!(contents(&second), live);
        assert_eq!(differing(&first, &second), [1, 7, FANOUT]);
        assert_eq!(differing(&second, &first), [1, 7, FANOUT]);
        assert!(differing(&second, &second.clone()).is_empty());
        assert_eq!(second.mismatch(&live), None);
        assert_eq!(first.mismatch(&live), Some(1));
        // The untouched leaf is one allocation in both versions.
        assert!(Rc::ptr_eq(
            first.leaves[2].as_ref().unwrap(),
            second.leaves[2].as_ref().unwrap()
        ));
    }

    #[test]
    fn releasing_a_version_frees_exactly_its_unshared_pages() {
        let store = Pages::new();
        let mut live = ram();
        live[0] = 1;
        live[GRANULE] = 1;
        let (first, _) = store
            .version(&Tree::zero(PAGES), all(), &live, UNLIMITED)
            .unwrap();
        live[GRANULE] = 2;
        let (second, _) = store.version(&first, [1], &live, UNLIMITED).unwrap();
        assert_eq!(store.held(), 3 * GRANULE as u64);
        drop(first);
        assert_eq!(store.held(), 2 * GRANULE as u64);
        assert_eq!(contents(&second), live);
        drop(second);
        assert_eq!(store.held(), 0);
    }

    #[test]
    fn a_full_store_refuses_a_version_and_keeps_what_it_holds() {
        let store = Pages::new();
        let mut live = ram();
        live[0] = 1;
        let limit = 2 * GRANULE as u64;
        let (first, _) = store
            .version(&Tree::zero(PAGES), all(), &live, limit)
            .unwrap();
        live[GRANULE] = 1;
        live[2 * GRANULE] = 1;
        assert_eq!(
            store.version(&first, [1, 2], &live, limit).err(),
            Some(Full)
        );
        assert_eq!(store.held(), GRANULE as u64, "the refused pages are freed");
        let (second, copied) = store.version(&first, [1], &live, limit).unwrap();
        assert_eq!(copied, 1);
        assert_eq!(second.page(1).unwrap()[0], 1);
    }
}
