use crate::Result;
use std::{marker::PhantomData, ptr::NonNull, rc::Rc};

/// Stable zero-filled host pages. Intentionally !Send/!Sync: VM and page access
/// are serialized on one thread. Backends hold an Rc so VM teardown always
/// precedes munmap. Raw guest access requires the vCPU to be stopped.
pub struct Memory {
    ptr: NonNull<u8>,
    len: usize,
    _thread: PhantomData<Rc<()>>,
}
impl Memory {
    pub fn new(len: usize) -> Result<Self> {
        if len == 0 || !len.is_multiple_of(crate::PAGE_SIZE) {
            return Err("memory size must be a positive multiple of 16 KiB".into());
        }
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(format!("mmap: {}", std::io::Error::last_os_error()));
        }
        Ok(Self {
            ptr: NonNull::new(ptr.cast()).ok_or("mmap returned null")?,
            len,
            _thread: PhantomData,
        })
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }
    /// # Safety
    /// Guest stopped; no concurrent guest/host writes or outstanding mut slices.
    pub unsafe fn bytes(&self) -> &[u8] {
        std::slice::from_raw_parts(self.as_ptr(), self.len)
    }
    /// # Safety
    /// Guest stopped; no outstanding host reference to RAM. The callback
    /// bounds the mutable borrow to a single stopped-device transaction.
    pub unsafe fn with_bytes_mut<T>(&self, operation: impl FnOnce(&mut [u8]) -> T) -> T {
        operation(std::slice::from_raw_parts_mut(self.as_ptr(), self.len))
    }
    /// # Safety
    /// Guest stopped; no outstanding host references to the affected bytes.
    pub unsafe fn write(&self, offset: usize, bytes: &[u8]) -> Result<()> {
        if offset
            .checked_add(bytes.len())
            .is_none_or(|end| end > self.len)
        {
            return Err("guest memory write out of bounds".into());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.as_ptr().add(offset), bytes.len());
        Ok(())
    }
    /// Replace these pages with a private (copy-on-write) mapping of `file`
    /// from `offset`, which must be page aligned.
    ///
    /// # Safety
    /// No backend maps these pages and no host reference to them is live.
    pub unsafe fn map_file(&self, file: &std::fs::File, offset: u64) -> Result<()> {
        use std::os::fd::AsRawFd;
        if offset % crate::PAGE_SIZE as u64 != 0 {
            return Err("file mapping offset is not page aligned".into());
        }
        let ptr = libc::mmap(
            self.as_ptr().cast(),
            self.len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_FIXED,
            file.as_raw_fd(),
            offset as libc::off_t,
        );
        if ptr == libc::MAP_FAILED {
            return Err(format!(
                "map checkpoint RAM: {}",
                std::io::Error::last_os_error()
            ));
        }
        if ptr.cast::<u8>() != self.as_ptr() {
            return Err("checkpoint RAM mapped at another address".into());
        }
        Ok(())
    }
    /// # Safety
    /// Guest stopped, with no outstanding reference to the page.
    pub unsafe fn word(&self, offset: usize) -> Result<u64> {
        if !offset.is_multiple_of(8) || offset.checked_add(8).is_none_or(|end| end > self.len) {
            return Err("unaligned/out of bounds time-page read".into());
        }
        Ok(u64::from_le(
            self.as_ptr().add(offset).cast::<u64>().read_volatile(),
        ))
    }
    /// # Safety
    /// Guest stopped, with no outstanding reference to the page.
    pub unsafe fn set_word(&self, offset: usize, value: u64) -> Result<()> {
        if !offset.is_multiple_of(8) || offset.checked_add(8).is_none_or(|end| end > self.len) {
            return Err("unaligned/out of bounds time-page write".into());
        }
        self.as_ptr()
            .add(offset)
            .cast::<u64>()
            .write_volatile(value.to_le());
        Ok(())
    }
}
impl Drop for Memory {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.as_ptr().cast(), self.len);
        }
    }
}
