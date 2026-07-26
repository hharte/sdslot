// SPDX-License-Identifier: MIT OR Apache-2.0
//! The aligned transfer buffer. Windows raw I/O and Linux `O_DIRECT` both
//! require buffer alignment; 4 KiB satisfies every page/sector size in
//! practice — one aligned path everywhere.

use std::alloc::{alloc_zeroed, dealloc, Layout as AllocLayout};
use std::ops::{Deref, DerefMut};

pub const BUFFER_ALIGN: usize = 4096;

/// Heap buffer with explicit 4 KiB alignment, zero-initialized.
pub struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
}

impl AlignedBuf {
    pub fn new(len: usize) -> AlignedBuf {
        assert!(len > 0);
        let layout = AllocLayout::from_size_align(len, BUFFER_ALIGN).expect("buffer layout");
        let ptr = unsafe { alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "allocation of {len} bytes failed");
        AlignedBuf { ptr, len }
    }

    pub fn zero(&mut self) {
        self.fill(0);
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        let layout = AllocLayout::from_size_align(self.len, BUFFER_ALIGN).unwrap();
        unsafe { dealloc(self.ptr, layout) };
    }
}

impl Deref for AlignedBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

// Sole owner of its allocation; safe to move across threads.
unsafe impl Send for AlignedBuf {}
