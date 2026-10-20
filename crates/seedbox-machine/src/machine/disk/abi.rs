//! C ABI over one [`Store`] for the patched QEMU block device.
//!
//! The static library (`qemu/seedbox_disk.rs`) is built with `panic=abort`,
//! so no unwind crosses into C: an internal panic aborts QEMU and the machine
//! fails. Entry points check null pointers, counts, lengths and encoded
//! formats, reporting violations as `false` or null without touching the
//! store. Pointer provenance, alignment and lifetimes stated under each
//! `# Safety` remain the caller's responsibility, and guest segments must not
//! alias the store or the output scalars. C applies no
//! durability policy: flushes happen only inside [`seedbox_disk_execute`], and
//! nothing here discards unflushed data; power loss belongs to the Machine.
//! Guest memory is only accessed through raw copies to and from local
//! buffers, so overlapping guest iovecs never form aliasing Rust references.

// QEMU calls the one shared request and overlay
// policy without duplicating it in C; a handle, iovec streams and SBXSTOR1
// bytes are the narrowest boundary that keeps execution sector-bounded.

use super::{
    codec::encoded_limit,
    request::{self, Buffers},
    Store, SECTOR,
};
use std::{ffi::c_void, ptr};

/// Matches `struct iovec`; QEMU checks the layout at compile time.
#[repr(C)]
pub struct SeedboxIov {
    pub base: *mut u8,
    pub len: usize,
}

/// An opaque store handle owned by C between open and close.
pub struct SeedboxDisk(Store);

/// QEMU's own limit on one chain (`VIRTQUEUE_MAX_SIZE`).
const MAX_SEGMENTS: usize = 1024;

/// Open a store of `sectors` sectors: zeroed when `image` is null and
/// `image_len` is zero, otherwise over a copy of exactly `sectors * 512` image
/// bytes. Returns null on invalid input.
///
/// # Safety
/// A non-null `image` must be readable for `image_len` bytes.
#[no_mangle]
pub unsafe extern "C" fn seedbox_disk_open(
    sectors: u64,
    image: *const u8,
    image_len: usize,
) -> *mut SeedboxDisk {
    let store = if image.is_null() {
        if image_len != 0 {
            return ptr::null_mut();
        }
        Store::zeroed(sectors)
    } else {
        let expected = sectors.checked_mul(SECTOR as u64);
        if expected != Some(image_len as u64) {
            return ptr::null_mut();
        }
        Store::from_image(std::slice::from_raw_parts(image, image_len).to_vec())
    };
    match store {
        Ok(store) => Box::into_raw(Box::new(SeedboxDisk(store))),
        Err(_) => ptr::null_mut(),
    }
}

/// Release a handle; null is ignored.
///
/// # Safety
/// `disk` is null or an unreleased handle from [`seedbox_disk_open`].
#[no_mangle]
pub unsafe extern "C" fn seedbox_disk_close(disk: *mut SeedboxDisk) {
    if !disk.is_null() {
        drop(Box::from_raw(disk));
    }
}

/// Capacity in 512-byte sectors; zero for null.
///
/// # Safety
/// `disk` is null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn seedbox_disk_capacity(disk: *const SeedboxDisk) -> u64 {
    if disk.is_null() {
        return 0;
    }
    (*disk).0.capacity()
}

/// Execute one virtio-blk request whose device-readable bytes are `out` and
/// device-writable bytes are `in_` (QEMU's `out_sg`/`in_sg`). On `true` it
/// completed, with `*written` bytes (status included) and `*status`. On
/// `false` the request is malformed or the arguments invalid; nothing past
/// the header was read and nothing was written.
///
/// # Safety
/// `disk` is a live handle; `written` and `status` are writable; each array
/// is readable for its count; every segment is valid for its length for the
/// whole call. Segments may overlap each other.
#[no_mangle]
pub unsafe extern "C" fn seedbox_disk_execute(
    disk: *mut SeedboxDisk,
    out: *const SeedboxIov,
    out_count: usize,
    in_: *const SeedboxIov,
    in_count: usize,
    written: *mut u32,
    status: *mut u8,
) -> bool {
    if disk.is_null() || written.is_null() || status.is_null() {
        return false;
    }
    let (Some(readable), Some(writable)) = (segments(out, out_count), segments(in_, in_count))
    else {
        return false;
    };
    let mut buffers = Iovecs { readable, writable };
    match request::execute(&mut (*disk).0, &mut buffers) {
        Ok(completion) => {
            *written = completion.written;
            *status = completion.status;
            true
        }
        Err(request::Malformed) => false,
    }
}

/// Encode both overlays (SBXSTOR1) into a new buffer for
/// [`seedbox_disk_bytes_free`]. This never flushes or changes the store.
///
/// # Safety
/// `disk` is a live handle; `bytes` and `len` are writable.
#[no_mangle]
pub unsafe extern "C" fn seedbox_disk_encode(
    disk: *const SeedboxDisk,
    bytes: *mut *mut u8,
    len: *mut usize,
) -> bool {
    if disk.is_null() || bytes.is_null() || len.is_null() {
        return false;
    }
    let encoded = (*disk).0.encode_overlays().into_boxed_slice();
    *len = encoded.len();
    *bytes = Box::into_raw(encoded).cast();
    true
}

/// Release a buffer from [`seedbox_disk_encode`]; null is ignored.
///
/// # Safety
/// `bytes` and `len` are exactly as returned and not yet released.
#[no_mangle]
pub unsafe extern "C" fn seedbox_disk_bytes_free(bytes: *mut u8, len: usize) {
    if !bytes.is_null() {
        drop(Box::from_raw(ptr::slice_from_raw_parts_mut(bytes, len)));
    }
}

/// Replace both overlays from SBXSTOR1 bytes of the same capacity. On
/// `false` the store is unchanged.
///
/// # Safety
/// `disk` is a live handle; a non-null `bytes` is readable for `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn seedbox_disk_replace_overlays(
    disk: *mut SeedboxDisk,
    bytes: *const u8,
    len: usize,
) -> bool {
    if disk.is_null() || (bytes.is_null() && len != 0) {
        return false;
    }
    let input = if len == 0 {
        &[][..]
    } else {
        std::slice::from_raw_parts(bytes, len)
    };
    (*disk).0.replace_overlays(input).is_ok()
}

/// Reads up to `length` bytes into `buffer`, returning how many it read;
/// fewer than requested means the source ended or failed.
pub type SeedboxRead =
    unsafe extern "C" fn(context: *mut c_void, buffer: *mut u8, length: usize) -> usize;

/// Replace both overlays from a `length`-byte SBXSTOR1 stream pulled through
/// `read` in bounded chunks, so a large stream needs no caller-side buffer and
/// memory grows only with bytes actually read. A length beyond any valid
/// encoding for this capacity, or beyond `isize::MAX`, is rejected before
/// reading; a short stream or
/// an invalid encoding returns `false` with the store unchanged.
///
/// # Safety
/// `disk` is a live handle; `read` honors its contract for `context`.
#[no_mangle]
pub unsafe extern "C" fn seedbox_disk_replace_overlays_from(
    disk: *mut SeedboxDisk,
    length: u64,
    read: Option<SeedboxRead>,
    context: *mut c_void,
) -> bool {
    let Some(read) = read else {
        return false;
    };
    if disk.is_null() || u128::from(length) > encoded_limit((*disk).0.capacity()) {
        return false;
    }
    // A Vec holds at most isize::MAX bytes; refuse larger lengths unread.
    let Some(length) = usize::try_from(length)
        .ok()
        .filter(|&length| length <= isize::MAX as usize)
    else {
        return false;
    };
    let mut bytes = Vec::new();
    let mut chunk = vec![0u8; 1 << 16];
    while bytes.len() < length {
        let want = chunk.len().min(length - bytes.len());
        if read(context, chunk.as_mut_ptr(), want) != want || bytes.try_reserve(want).is_err() {
            return false;
        }
        bytes.extend_from_slice(&chunk[..want]);
    }
    (*disk).0.replace_overlays(&bytes).is_ok()
}

/// Validated segment list of one direction.
struct Segments {
    spans: Vec<(*mut u8, usize)>,
    length: usize,
}

unsafe fn segments(array: *const SeedboxIov, count: usize) -> Option<Segments> {
    if count > MAX_SEGMENTS || (array.is_null() && count != 0) {
        return None;
    }
    let mut spans = Vec::with_capacity(count);
    let mut length = 0usize;
    for index in 0..count {
        let segment = &*array.add(index);
        if segment.base.is_null() && segment.len != 0 {
            return None;
        }
        length = length.checked_add(segment.len)?;
        spans.push((segment.base, segment.len));
    }
    Some(Segments { spans, length })
}

impl Segments {
    /// Visit the pieces covering `length` bytes from `offset`, in order.
    fn each(&self, mut offset: usize, mut length: usize, mut visit: impl FnMut(*mut u8, usize)) {
        for &(base, span) in &self.spans {
            if length == 0 {
                break;
            }
            if offset >= span {
                offset -= span;
                continue;
            }
            let count = (span - offset).min(length);
            // SAFETY: within this segment, which the caller keeps valid.
            visit(unsafe { base.add(offset) }, count);
            (offset, length) = (0, length - count);
        }
        assert_eq!(length, 0, "request stream access out of range");
    }
}

struct Iovecs {
    readable: Segments,
    writable: Segments,
}

impl Buffers for Iovecs {
    fn readable(&self) -> usize {
        self.readable.length
    }
    fn writable(&self) -> usize {
        self.writable.length
    }
    fn read(&mut self, offset: usize, out: &mut [u8]) {
        let mut done = 0;
        self.readable.each(offset, out.len(), |at, count| {
            // SAFETY: a valid guest segment copied into a local buffer.
            unsafe { ptr::copy_nonoverlapping(at, out[done..].as_mut_ptr(), count) };
            done += count;
        });
    }
    fn write(&mut self, offset: usize, bytes: &[u8]) {
        let mut done = 0;
        self.writable.each(offset, bytes.len(), |at, count| {
            // SAFETY: store or local bytes copied into a valid guest segment.
            unsafe { ptr::copy_nonoverlapping(bytes[done..].as_ptr(), at, count) };
            done += count;
        });
    }
}

#[cfg(test)]
mod tests;
