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
use windows_sys::Wdk::System::SystemServices::{
    ExAllocatePool2, ExFreePool, ExFreePoolWithTag, PAGE_SIZE,
};

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

pub const POINTER_SIZE: usize = size_of::<*const u8>();
/// The minimum alignment of pointer returned by ExAllocatePool2.
pub const MIN_ALIGNMENT: usize = POINTER_SIZE * 2;

impl<const ALLOC_POOL_FLAGS: POOL_FLAGS, const MEMORY_TAG: u32, const TAGGED_DEALLOC: bool>
    KAlloc<ALLOC_POOL_FLAGS, MEMORY_TAG, TAGGED_DEALLOC>
{
    #[inline(always)]
    pub fn original_pointer_location(ptr: *mut u8) -> *mut *mut u8 {
        unsafe { (ptr.cast::<*mut u8>()).offset(-1) }
    }

    #[inline(always)]
    pub fn save_original_pointer(ptr: *mut u8, original_ptr: *mut u8) {
        let original_ptr_location = Self::original_pointer_location(ptr);
        unsafe {
            *original_ptr_location = original_ptr;
        }
    }

    #[inline(always)]
    pub fn original_pointer(ptr: *mut u8) -> *mut u8 {
        let original_ptr_location = Self::original_pointer_location(ptr);
        unsafe { *original_ptr_location }
    }

    #[inline(always)]
    pub fn kernel_alignment(size: usize) -> usize {
        if size >= PAGE_SIZE as usize {
            PAGE_SIZE as usize
        } else {
            MIN_ALIGNMENT
        }
    }

    #[cfg(test)]
    #[inline(always)]
    fn usermode_allocate(size: usize) -> *mut u8 {
        unsafe {
            alloc::alloc::alloc(
                Layout::from_size_align(size, Self::kernel_alignment(size))
                    .expect("Failed to create layout"),
            )
        }
    }

    #[inline(always)]
    pub fn kernel_allocate(size: usize) -> *mut u8 {
        if size == 0 {
            return null_mut();
        }

        unsafe {
            ExAllocatePool2(ALLOC_POOL_FLAGS, size, MEMORY_TAG) as *mut u8
        }
    }

    pub fn allocate(size: usize) -> *mut u8 {
        if size == 0 {
            return null_mut();
        }

        #[cfg(test)]
        {
            Self::usermode_allocate(size)
        }

        #[cfg(not(test))]
        {
            Self::kernel_allocate(size)
        }
    }

    #[inline(always)]
    fn is_kernel_aligned(layout: Layout) -> bool {
        // `ExAllocatePool2` uses an alignment of `PAGE_SIZE` or minimum pool
        // alignment depending on the requested size. See documentation:
        // https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdm/nf-wdm-exallocatepool2#remarks
        let pool_alignment = Self::kernel_alignment(layout.size());

        layout.align() <= pool_alignment
    }

    #[inline(always)]
    pub fn align_up_to(allocated_buffer: *mut u8, alignment: usize) -> *mut u8 {
        let alignment_mask_base = !(alignment - 1);
        ((allocated_buffer as usize & alignment_mask_base) + alignment) as *mut u8
    }

    pub fn allocate_aligned(layout: Layout) -> *mut u8 {
        if Self::is_kernel_aligned(layout) {
            Self::allocate(layout.size())
        } else {
            let extended_buffer_size = layout.size() + layout.align();
            let buffer = Self::allocate(extended_buffer_size);
            if buffer.is_null() {
                return null_mut();
            }

            let aligned = Self::align_up_to(buffer, layout.align());
            Self::save_original_pointer(aligned, buffer);

            aligned
        }
    }

    fn allocate_slice(layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let allocated_raw = Self::allocate_aligned(layout);

        if allocated_raw.is_null() {
            Err(AllocError)
        } else {
            let allocated_slice = slice_from_raw_parts_mut(allocated_raw, layout.size());

            Ok(unsafe { NonNull::new_unchecked(allocated_slice) })
        }
    }

    fn deallocate_tagged(ptr: *mut u8) {
        unsafe {
            ExFreePoolWithTag(ptr.cast(), MEMORY_TAG);
        }
    }

    fn deallocate_non_tagged(ptr: *mut u8) {
        unsafe {
            ExFreePool(ptr.cast());
        }
    }

    pub fn deallocate(ptr: *mut u8) {
        if TAGGED_DEALLOC {
            Self::deallocate_tagged(ptr);
        } else {
            Self::deallocate_non_tagged(ptr);
        }
    }

    #[cfg(test)]
    fn usermode_deallocate(original_ptr: *mut u8, layout: Layout) {
        let allocated_size = layout.size() + if Self::is_kernel_aligned(layout) {
            0
        } else {
            layout.align()
        };

        unsafe {
            // For usermode tests, we need to deallocate the original pointer with the correct layout.
            alloc::alloc::dealloc(
                original_ptr,
                Layout::from_size_align(
                    allocated_size,
                    Self::kernel_alignment(allocated_size),
                )
                .expect("Failed to create layout"),
            );
        }
    }

    fn deallocate_aligned(ptr: *mut u8, layout: Layout) {
        if ptr.is_null() {
            return;
        }

        let original_ptr = if Self::is_kernel_aligned(layout) {
            ptr
        } else {
            Self::original_pointer(ptr)
        };

        debug_assert!(
            !original_ptr.is_null(),
            "Original pointer is null. This may indicate that the pointer was not allocated by this allocator or has already been deallocated."
        );

        #[cfg(test)]
        Self::usermode_deallocate(original_ptr, layout);

        #[cfg(not(test))]
        Self::deallocate(original_ptr);
    }
}

unsafe impl<const ALLOC_POOL_FLAGS: POOL_FLAGS, const MEMORY_TAG: u32, const TAGGED_DEALLOC: bool>
    GlobalAlloc for KAlloc<ALLOC_POOL_FLAGS, MEMORY_TAG, TAGGED_DEALLOC>
{
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::allocate_aligned(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        Self::deallocate_aligned(ptr, layout);
    }
}

unsafe impl<const ALLOC_POOL_FLAGS: POOL_FLAGS, const MEMORY_TAG: u32, const TAGGED_DEALLOC: bool>
    Allocator for KAlloc<ALLOC_POOL_FLAGS, MEMORY_TAG, TAGGED_DEALLOC>
{
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        Self::allocate_slice(layout)
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        Self::deallocate_aligned(ptr.as_ptr(), layout);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn require_realignment_test() {
        let size = 16;
        let alignments = (0..=16).map(|i| 1 << i).collect::<Vec<usize>>();
        for &alignment in alignments.iter() {
            let layout = Layout::from_size_align(size, alignment).unwrap();
            let requires_realignment =
                PagedAlloc::<DEFAULT_MEMORY_TAG, true>::is_kernel_aligned(layout);
            if alignment <= MIN_ALIGNMENT {
                assert!(
                    requires_realignment,
                    "Expected realignment for alignment {} but got false",
                    alignment
                );
            } else {
                assert!(
                    !requires_realignment,
                    "Expected no realignment for alignment {} but got true",
                    alignment
                );
            }
        }
    }

    #[test]
    fn original_pointer_location_test() {
        let ptr = 0x1000 as *mut u8;
        let original_ptr_location =
            PagedAlloc::<DEFAULT_MEMORY_TAG, true>::original_pointer_location(ptr);
        assert_eq!(
            original_ptr_location,
            (ptr as usize - POINTER_SIZE) as *mut *mut u8
        );
    }

    #[test]
    fn save_original_pointer_test() {
        let mut buffer: Vec<u8> = Vec::with_capacity(16);
        let aligned_ptr = unsafe { buffer.as_mut_ptr().byte_add(POINTER_SIZE) }; // simulate aligned pointer
        let original_ptr = 0x1000 as *mut u8;
        PagedAlloc::<DEFAULT_MEMORY_TAG, true>::save_original_pointer(aligned_ptr, original_ptr);
        let original_ptr_location =
            PagedAlloc::<DEFAULT_MEMORY_TAG, true>::original_pointer_location(aligned_ptr);
        let saved_original_ptr = unsafe { *original_ptr_location };
        assert_eq!(saved_original_ptr, original_ptr);
    }

    #[test]
    fn align_up_to_test_less_than_min_alignment() {
        let ptr = 0x1003 as *mut u8;
        let alignment = 4;
        let aligned_ptr = PagedAlloc::<DEFAULT_MEMORY_TAG, true>::align_up_to(ptr, alignment);
        assert_eq!(aligned_ptr as usize % alignment, 0);
    }

    #[test]
    fn align_up_to_test_greater_than_min_alignment() {
        let ptr = 0x1003 as *mut u8;
        let alignment = 64;
        let aligned_ptr = PagedAlloc::<DEFAULT_MEMORY_TAG, true>::align_up_to(ptr, alignment);
        assert_eq!(aligned_ptr as usize % alignment, 0);
    }

    #[test]
    fn align_up_to_test_page_size_alignment() {
        let ptr = 0x1003 as *mut u8;
        let alignment = PAGE_SIZE as usize;
        let aligned_ptr = PagedAlloc::<DEFAULT_MEMORY_TAG, true>::align_up_to(ptr, alignment);
        assert_eq!(aligned_ptr as usize % alignment, 0);
    }

    #[test]
    fn align_up_to_test_greater_than_page_size_alignment() {
        let ptr = 0x1003 as *mut u8;
        let alignment = PAGE_SIZE as usize * 2;
        let aligned_ptr = PagedAlloc::<DEFAULT_MEMORY_TAG, true>::align_up_to(ptr, alignment);
        assert_eq!(aligned_ptr as usize % alignment, 0);
    }

    #[test]
    fn kernel_alignment_test() {
        assert_eq!(
            PagedAlloc::<DEFAULT_MEMORY_TAG, true>::kernel_alignment(1),
            MIN_ALIGNMENT
        );
        assert_eq!(
            PagedAlloc::<DEFAULT_MEMORY_TAG, true>::kernel_alignment(16),
            MIN_ALIGNMENT
        );
        assert_eq!(
            PagedAlloc::<DEFAULT_MEMORY_TAG, true>::kernel_alignment(PAGE_SIZE as usize - 1),
            MIN_ALIGNMENT
        );
        assert_eq!(
            PagedAlloc::<DEFAULT_MEMORY_TAG, true>::kernel_alignment(PAGE_SIZE as usize),
            PAGE_SIZE as usize
        );
        assert_eq!(
            PagedAlloc::<DEFAULT_MEMORY_TAG, true>::kernel_alignment(PAGE_SIZE as usize + 1),
            PAGE_SIZE as usize
        );
    }

    #[test]
    fn allocate_aligned_test_requires_realignment() {
        let layout = Layout::from_size_align(16, 64).unwrap();
        let aligned_ptr = PagedAlloc::<DEFAULT_MEMORY_TAG, true>::allocate_aligned(layout);
        assert!(!aligned_ptr.is_null());
        assert!((aligned_ptr as usize) % layout.align() == 0);

        let original_ptr = PagedAlloc::<DEFAULT_MEMORY_TAG, true>::original_pointer(aligned_ptr);
        assert!(!original_ptr.is_null());
        assert!(original_ptr < aligned_ptr);

        PagedAlloc::<DEFAULT_MEMORY_TAG, true>::deallocate_aligned(aligned_ptr, layout);
    }

    #[test]
    fn allocate_aligned_test_no_realignment() {
        let layout = Layout::from_size_align(16, 8).unwrap();
        let aligned_ptr = PagedAlloc::<DEFAULT_MEMORY_TAG, true>::allocate_aligned(layout);
        assert!(!aligned_ptr.is_null());
        assert!((aligned_ptr as usize) % layout.align() == 0);

        PagedAlloc::<DEFAULT_MEMORY_TAG, true>::deallocate_aligned(aligned_ptr, layout);
    }

    #[test]
    fn allocate_aligned_test_alignment_equal_to_page_size() {
        let layout = Layout::from_size_align(16, PAGE_SIZE as usize).unwrap();
        let aligned_ptr = PagedAlloc::<DEFAULT_MEMORY_TAG, true>::allocate_aligned(layout);
        assert!(!aligned_ptr.is_null());
        assert!((aligned_ptr as usize) % layout.align() == 0);

        let original_ptr = PagedAlloc::<DEFAULT_MEMORY_TAG, true>::original_pointer(aligned_ptr);
        assert!(!original_ptr.is_null());
        assert!(original_ptr < aligned_ptr);

        PagedAlloc::<DEFAULT_MEMORY_TAG, true>::deallocate_aligned(aligned_ptr, layout);
    }

    #[test]
    fn allocate_aligned_test_alignment_greater_than_page_size() {
        let layout = Layout::from_size_align(16, PAGE_SIZE as usize * 2).unwrap();
        let aligned_ptr = PagedAlloc::<DEFAULT_MEMORY_TAG, true>::allocate_aligned(layout);
        assert!(!aligned_ptr.is_null());
        assert!((aligned_ptr as usize) % layout.align() == 0);

        let original_ptr = PagedAlloc::<DEFAULT_MEMORY_TAG, true>::original_pointer(aligned_ptr);
        assert!(!original_ptr.is_null());
        assert!(original_ptr < aligned_ptr);

        PagedAlloc::<DEFAULT_MEMORY_TAG, true>::deallocate_aligned(aligned_ptr, layout);
    }
}
