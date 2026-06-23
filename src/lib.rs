#![no_std]
#![feature(allocator_api)]
#![allow(non_camel_case_types)]
#![deny(unsafe_op_in_unsafe_fn)]
#![allow(unsafe_code)]
//#![deny(missing_docs)]

extern crate alloc;

mod bindings;

use crate::bindings::{POOL_FLAG_NON_PAGED, POOL_FLAG_PAGED, POOL_FLAGS};
use alloc::alloc::{AllocError, Allocator};
use core::{
    alloc::{GlobalAlloc, Layout},
    ptr::{NonNull, null_mut, slice_from_raw_parts_mut},
};
use windows_sys::Wdk::System::SystemServices::{ExAllocatePool2, ExFreePool, ExFreePoolWithTag};

/// Default memory tag for allocations.
/// The tag is represented as a 4-byte ASCII string. Here, we use "Rust" as the default tag.
pub const DEFAULT_MEMORY_TAG: u32 = u32::from_ne_bytes(*b"Rust");

#[derive(Default, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KAlloc<
    const ALLOC_POOL_FLAGS: POOL_FLAGS,
    const MEMORY_TAG: u32 = DEFAULT_MEMORY_TAG,
    const TAGGED_DEALLOC: bool = true,
>;

pub type PagedAlloc<const MEMORY_TAG: u32 = DEFAULT_MEMORY_TAG, const TAGGED_DEALLOC: bool = true> =
    KAlloc<POOL_FLAG_PAGED, MEMORY_TAG, TAGGED_DEALLOC>;
pub type NonPagedAlloc<
    const MEMORY_TAG: u32 = DEFAULT_MEMORY_TAG,
    const TAGGED_DEALLOC: bool = true,
> = KAlloc<POOL_FLAG_NON_PAGED, MEMORY_TAG, TAGGED_DEALLOC>;

impl<const ALLOC_POOL_FLAGS: POOL_FLAGS, const MEMORY_TAG: u32, const TAGGED_DEALLOC: bool>
    KAlloc<ALLOC_POOL_FLAGS, MEMORY_TAG, TAGGED_DEALLOC>
{
    pub fn kernel_allocate(size: usize) -> *mut u8 {
        if size == 0 {
            return null_mut();
        }

        unsafe { ExAllocatePool2(ALLOC_POOL_FLAGS, size, MEMORY_TAG) }.cast()
    }

    fn kernel_allocate_slice(
        size: usize,
    ) -> Result<NonNull<[u8]>, AllocError> {
        let allocated_raw = Self::kernel_allocate(size);

        if allocated_raw.is_null() {
            Err(AllocError)
        } else {
            let allocated_slice = slice_from_raw_parts_mut(allocated_raw, size);

            Ok(unsafe { NonNull::new_unchecked(allocated_slice) })
        }
    }

    fn kernel_deallocate_tagged(ptr: *mut u8) {
        unsafe {
            ExFreePoolWithTag(ptr.cast(), MEMORY_TAG);
        }
    }

    fn kernel_deallocate_non_tagged(ptr: *mut u8) {
        unsafe {
            ExFreePool(ptr.cast());
        }
    }

    fn kernel_deallocate(ptr: *mut u8) {
        if TAGGED_DEALLOC {
            Self::kernel_deallocate_tagged(ptr);
        } else {
            Self::kernel_deallocate_non_tagged(ptr);
        }
    }
}

unsafe impl<const ALLOC_POOL_FLAGS: POOL_FLAGS, const MEMORY_TAG: u32, const TAGGED_DEALLOC: bool>
    GlobalAlloc for KAlloc<ALLOC_POOL_FLAGS, MEMORY_TAG, TAGGED_DEALLOC>
{
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::kernel_allocate(layout.size())
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        Self::kernel_deallocate(ptr);
    }
}

unsafe impl<const ALLOC_POOL_FLAGS: POOL_FLAGS, const MEMORY_TAG: u32, const TAGGED_DEALLOC: bool>
    Allocator for KAlloc<ALLOC_POOL_FLAGS, MEMORY_TAG, TAGGED_DEALLOC>
{
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        Self::kernel_allocate_slice(layout.size())
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, _layout: Layout) {
        Self::kernel_deallocate(ptr.as_ptr());
    }
}