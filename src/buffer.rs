//! A heap buffer with a chosen alignment.
//!
//! Direct I/O requires the memory it reads into and writes from to be aligned
//! to the device block size. The standard library's `Vec<u8>` only guarantees
//! alignment for `u8` (one byte), so pages own one of these instead: a single
//! allocation of exactly `page_size` bytes, aligned to `page_size`, which
//! satisfies every platform's Direct I/O alignment rule for the page sizes this
//! crate accepts.

use std::alloc::{self, Layout};
use std::ptr::NonNull;
use std::slice;

/// An owned, zero-initialized heap buffer with a fixed size and alignment.
///
/// Semantically this is a `Box<[u8]>` whose start address is guaranteed aligned
/// to `layout.align()`. It owns its allocation and frees it on drop.
pub(crate) struct AlignedBuffer {
    ptr: NonNull<u8>,
    layout: Layout,
}

// SAFETY: an `AlignedBuffer` is a unique owner of a plain-bytes heap allocation
// with no interior mutability and no thread affinity — exactly like `Box<[u8]>`,
// which is `Send + Sync`. The raw pointer only suppresses the auto-impls; moving
// the owner between threads or sharing `&AlignedBuffer` is sound.
unsafe impl Send for AlignedBuffer {}
// SAFETY: see the `Send` justification above; shared access is read-only through
// `&self` and the bytes carry no interior mutability.
unsafe impl Sync for AlignedBuffer {}

impl AlignedBuffer {
    /// Allocate `size` zeroed bytes aligned to `align`.
    ///
    /// `size` and `align` must both be powers of two with `size >= align`, which
    /// the page-size validation upstream guarantees. The allocation is zeroed,
    /// so a freshly created buffer is a valid all-zero page body.
    pub(crate) fn new_zeroed(size: usize, align: usize) -> Self {
        // `from_size_align` only fails on an invalid alignment or an overflowing
        // rounded size; callers pass validated power-of-two page sizes, so this
        // cannot fail in practice. Treat a violation as the allocator would.
        let layout = match Layout::from_size_align(size, align) {
            Ok(layout) => layout,
            Err(_) => alloc::handle_alloc_error(
                // Fall back to a maximally-aligned layout solely to feed
                // `handle_alloc_error`, which diverges.
                Layout::new::<u8>(),
            ),
        };

        // SAFETY: `layout` has a non-zero size (page sizes are >= 4096), so
        // `alloc_zeroed` is being called with a valid, non-zero layout.
        let raw = unsafe { alloc::alloc_zeroed(layout) };
        let ptr = match NonNull::new(raw) {
            Some(ptr) => ptr,
            None => alloc::handle_alloc_error(layout),
        };
        Self { ptr, layout }
    }

    /// The buffer contents as a shared byte slice.
    #[inline]
    pub(crate) fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` points to `layout.size()` initialized (zeroed, then
        // written) bytes that this buffer uniquely owns for its whole lifetime;
        // the borrow ties the slice to `&self`.
        unsafe { slice::from_raw_parts(self.ptr.as_ptr(), self.layout.size()) }
    }

    /// The buffer contents as an exclusive byte slice.
    #[inline]
    pub(crate) fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as `as_slice`, plus `&mut self` guarantees no other reference
        // to these bytes exists for the duration of the returned borrow.
        unsafe { slice::from_raw_parts_mut(self.ptr.as_ptr(), self.layout.size()) }
    }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        // SAFETY: `ptr` was returned by `alloc_zeroed` with exactly `layout`,
        // has not been freed before, and is freed once here.
        unsafe { alloc::dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

impl Clone for AlignedBuffer {
    fn clone(&self) -> Self {
        let mut copy = Self::new_zeroed(self.layout.size(), self.layout.align());
        copy.as_mut_slice().copy_from_slice(self.as_slice());
        copy
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn test_aligned_buffer_is_aligned_and_zeroed() {
        let buf = AlignedBuffer::new_zeroed(8192, 4096);
        assert_eq!(buf.as_slice().len(), 8192);
        assert_eq!(buf.as_slice().as_ptr() as usize % 4096, 0);
        assert!(buf.as_slice().iter().all(|&b| b == 0));
    }

    #[test]
    fn test_aligned_buffer_clone_is_independent() {
        let mut a = AlignedBuffer::new_zeroed(4096, 4096);
        a.as_mut_slice()[0] = 0xAB;
        let b = a.clone();
        a.as_mut_slice()[0] = 0xCD;
        assert_eq!(b.as_slice()[0], 0xAB);
        assert_eq!(a.as_slice()[0], 0xCD);
    }
}
