use super::Result;
use std::{marker::PhantomData, ptr::NonNull, rc::Rc};
/// Guest RAM as a host device model sees it. Reading is free; every write goes
/// through `write`, so the mapping sees each one.
pub(super) trait Ram {
    fn bytes(&self) -> &[u8];
    /// Panics outside the mapping, as slice indexing does.
    fn write(&mut self, at: usize, bytes: &[u8]);
}
// Device unit tests run against plain buffers. Production code cannot: only
// `Guest` implements `Ram` there, so a host write cannot go around it.
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
    }
}
/// Process-local mappings; all access is confined to the stopped worker thread.
pub(super) struct Memory {
    pointer: NonNull<u8>,
    length: usize,
    layout: Option<super::boot::RamLayout>,
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
    pub(super) unsafe fn modify<T>(&self, f: impl FnOnce(&mut [u8]) -> T) -> T {
        f(std::slice::from_raw_parts_mut(self.ptr(), self.length))
    }
    /// Device access to stopped guest RAM: writes go through [`Ram::write`].
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
    }
}
impl Drop for Memory {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr().cast(), self.length);
        }
    }
}
