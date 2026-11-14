use super::Result;
use std::{cell::RefCell, marker::PhantomData, ptr::NonNull, rc::Rc};
/// Bytes of one tracked RAM page: the unit a retained snapshot copies, shares
/// and rewinds, and the host page a guest write fault covers.
#[cfg(target_arch = "aarch64")]
pub(super) const GRANULE: usize = 16384;
#[cfg(target_arch = "x86_64")]
pub(super) const GRANULE: usize = 4096;
/// Guest RAM as a host device model sees it. Reading is free; every write goes
/// through `write`, so a tracked mapping knows which pages changed.
pub(super) trait Ram {
    fn bytes(&self) -> &[u8];
    /// Panics outside the mapping, as slice indexing does.
    fn write(&mut self, at: usize, bytes: &[u8]);
}
// Device unit tests run against plain buffers. Production code cannot: only
// `Guest` implements `Ram` there, so a host write cannot skip tracking.
#[cfg(test)]
impl Ram for [u8] {
    fn bytes(&self) -> &[u8] {
        self
    }
    fn write(&mut self, at: usize, bytes: &[u8]) {
        self[at..at + bytes.len()].copy_from_slice(bytes);
    }
}
#[cfg(test)]
impl Ram for Vec<u8> {
    fn bytes(&self) -> &[u8] {
        self
    }
    fn write(&mut self, at: usize, bytes: &[u8]) {
        self[at..at + bytes.len()].copy_from_slice(bytes);
    }
}
/// The stopped guest's RAM, handed to device models by [`Memory::guest`].
pub(super) struct Guest<'a>(&'a Memory);
impl Ram for Guest<'_> {
    fn bytes(&self) -> &[u8] {
        unsafe { self.0.bytes() }
    }
    fn write(&mut self, at: usize, bytes: &[u8]) {
        let ram = unsafe { std::slice::from_raw_parts_mut(self.0.ptr(), self.0.length) };
        ram[at..at + bytes.len()].copy_from_slice(bytes);
        self.0.mark(at, bytes.len());
    }
}
/// A set of RAM pages that also remembers them in insertion order, so it is
/// emptied in time proportional to its size, not to RAM.
pub(super) struct PageSet {
    marked: Vec<u64>,
    pages: Vec<u32>,
}
impl PageSet {
    pub(super) fn new(pages: usize) -> Self {
        Self {
            marked: vec![0; pages.div_ceil(64)],
            pages: Vec::new(),
        }
    }
    /// Whether the page was absent.
    pub(super) fn insert(&mut self, page: usize) -> bool {
        let bit = 1 << (page % 64);
        let absent = self.marked[page / 64] & bit == 0;
        if absent {
            self.marked[page / 64] |= bit;
            self.pages.push(page as u32);
        }
        absent
    }
    /// Empty the set, returning its pages in insertion order.
    pub(super) fn take(&mut self) -> Vec<u32> {
        let pages = std::mem::take(&mut self.pages);
        for &page in &pages {
            self.marked[page as usize / 64] = 0;
        }
        pages
    }
}
/// Process-local mappings; all access is confined to the stopped worker thread.
pub(super) struct Memory {
    pointer: NonNull<u8>,
    length: usize,
    layout: Option<super::boot::RamLayout>,
    /// Pages written since live RAM last matched a retained version. `None`
    /// until the first retained snapshot: an untracked mapping records nothing.
    dirty: RefCell<Option<PageSet>>,
    _thread: PhantomData<Rc<()>>,
}
impl Memory {
    pub(super) fn new(length: usize) -> Result<Self> {
        if length == 0 || !length.is_multiple_of(16384) {
            return Err("PV RAM alignment".into());
        }
        let pointer = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANON | libc::MAP_PRIVATE,
                -1,
                0,
            )
        };
        if pointer == libc::MAP_FAILED {
            return Err(format!("PV mmap: {}", std::io::Error::last_os_error()));
        }
        Ok(Self {
            pointer: NonNull::new(pointer.cast()).ok_or("PV null mapping")?,
            length,
            layout: None,
            dirty: RefCell::new(None),
            _thread: PhantomData,
        })
    }
    /// Allocate sparse address-indexed backing under an already checked RAM layout.
    // Native slots and loaders take capacity from this layout; the backing
    // length includes holes and is not the configured guest capacity.
    pub(super) fn ram(layout: super::boot::RamLayout) -> Result<Self> {
        let mut memory = Self::new(layout.span)?;
        memory.layout = Some(layout);
        Ok(memory)
    }
    pub(super) fn layout(&self) -> Result<&super::boot::RamLayout> {
        self.layout
            .as_ref()
            .ok_or_else(|| "PV mapping is not guest RAM".into())
    }
    pub(super) fn ptr(&self) -> *mut u8 {
        self.pointer.as_ptr()
    }
    pub(super) fn len(&self) -> usize {
        self.length
    }
    /// Caller must hold a stopped vCPU and no outstanding mutable host borrow.
    pub(super) unsafe fn bytes(&self) -> &[u8] {
        std::slice::from_raw_parts(self.ptr(), self.length)
    }
    /// Raw access. On a tracked mapping every page counts as written, so
    /// device models use [`Memory::guest`] instead.
    pub(super) unsafe fn modify<T>(&self, f: impl FnOnce(&mut [u8]) -> T) -> T {
        let result = f(std::slice::from_raw_parts_mut(self.ptr(), self.length));
        self.mark(0, self.length);
        result
    }
    /// Device access to stopped guest RAM: writes record their pages.
    /// Caller must hold a stopped vCPU and no outstanding host borrow.
    pub(super) unsafe fn guest<T>(&self, f: impl FnOnce(&mut Guest<'_>) -> T) -> T {
        f(&mut Guest(self))
    }
    pub(super) fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<()> {
        if offset
            .checked_add(bytes.len())
            .is_none_or(|end| end > self.length)
        {
            return Err("PV RAM write outside mapping".into());
        }
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr().add(offset), bytes.len());
        }
        self.mark(offset, bytes.len());
        Ok(())
    }
    pub(super) fn word(&self, offset: usize) -> u64 {
        assert!(offset.is_multiple_of(8) && offset + 8 <= self.length);
        unsafe { u64::from_le(self.ptr().add(offset).cast::<u64>().read_volatile()) }
    }
    pub(super) fn set_word(&self, offset: usize, value: u64) {
        assert!(offset.is_multiple_of(8) && offset + 8 <= self.length);
        unsafe {
            self.ptr()
                .add(offset)
                .cast::<u64>()
                .write_volatile(value.to_le());
        }
        self.mark(offset, 8);
    }
    pub(super) fn pages(&self) -> usize {
        self.length / GRANULE
    }
    /// Start recording written pages. Nothing is recorded before this.
    pub(super) fn track(&self) {
        self.dirty
            .borrow_mut()
            .get_or_insert_with(|| PageSet::new(self.pages()));
    }
    pub(super) fn tracked(&self) -> bool {
        self.dirty.borrow().is_some()
    }
    /// Record a write to these bytes.
    pub(super) fn mark(&self, offset: usize, length: usize) {
        if length == 0 {
            return;
        }
        if let Some(dirty) = self.dirty.borrow_mut().as_mut() {
            for page in offset / GRANULE..=(offset + length - 1) / GRANULE {
                dirty.insert(page);
            }
        }
    }
    /// Record a write somewhere in this page.
    pub(super) fn mark_page(&self, page: usize) {
        if let Some(dirty) = self.dirty.borrow_mut().as_mut() {
            dirty.insert(page);
        }
    }
    /// The pages recorded since the last call, each once, in first-write order.
    pub(super) fn take_dirty(&self) -> Vec<u32> {
        self.dirty
            .borrow_mut()
            .as_mut()
            .map_or_else(Vec::new, PageSet::take)
    }
    /// Put back a page's retained contents (`None` is the zero page) without
    /// recording a write. The guest may have executed the old contents, so the
    /// instruction cache is invalidated: a VM that keeps its mapping of the
    /// page otherwise runs the stale code. A recreated VM was not seen to, but
    /// nothing documents that it cannot.
    /// Caller must hold a stopped vCPU and no outstanding host borrow.
    pub(super) unsafe fn rewind_page(&self, page: usize, contents: Option<&[u8]>) {
        let live = std::slice::from_raw_parts_mut(self.ptr().add(page * GRANULE), GRANULE);
        match contents {
            Some(contents) => live.copy_from_slice(contents),
            None => live.fill(0),
        }
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        sys_icache_invalidate(live.as_mut_ptr().cast(), GRANULE);
    }
}
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
unsafe extern "C" {
    fn sys_icache_invalidate(start: *mut std::ffi::c_void, length: usize);
}
impl Drop for Memory {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr().cast(), self.length);
        }
    }
}
