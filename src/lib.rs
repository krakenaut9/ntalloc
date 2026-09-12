//! Windows kernel-mode pool allocator for `#![no_std]` Rust drivers.
//!
//! `ntalloc` wraps the WDK pool APIs (`ExAllocatePool2`, `ExFreePool`,
//! `ExFreePoolWithTag`) behind the standard [`GlobalAlloc`] trait and the
//! unstable [`Allocator`] trait, so ordinary `alloc`-crate types (`Vec`,
//! `Box`, ...) can be backed by kernel pool memory from a `#![no_std]`
//! driver.
//!
//! # Overview
//!
//! - [`KAlloc`] is a zero-sized allocator type parameterized over the pool
//!   flags, the memory tag, and whether deallocation uses the tagged or
//!   untagged free routine.
//! - [`PagedAlloc`] and [`NonPagedAlloc`] are convenience aliases for the
//!   paged and non-paged pool, respectively.
//! - Allocations are tagged with a 4-byte ASCII tag (see
//!   [`DEFAULT_MEMORY_TAG`]) so they can be attributed back to this allocator
//!   with tools such as `!poolused`.
//!
//! # Design
//!
//! `ExAllocatePool2` only guarantees an allocation aligned to
//! [`MIN_ALIGNMENT`] (the WDK's `MEMORY_ALLOCATION_ALIGNMENT`: two pointers,
//! i.e. 16 bytes on 64-bit targets), or to the page size for allocations of
//! at least one page (see [`KAlloc::kernel_alignment`]). Rust [`Layout`]s can
//! request stricter alignment than that, so when a request exceeds what the
//! pool allocator natively guarantees, [`KAlloc::allocate_aligned`]
//! over-allocates by `layout.align()` extra bytes, rounds the returned
//! pointer up to the requested alignment, and stashes the original
//! (unaligned) pointer in the pointer-sized slot immediately before the
//! aligned pointer (see [`KAlloc::align_up_to`] for why that slot is always
//! available). [`KAlloc::deallocate_aligned`] reads that slot back out to
//! recover the pointer that must be passed to
//! `ExFreePool`/`ExFreePoolWithTag`, which is why an over-aligned allocation
//! has to be released through it rather than through [`KAlloc::deallocate`].
//!
//! # Example
//!
//! ```no_run
//! use ntalloc::{NonPagedAlloc, DEFAULT_MEMORY_TAG};
//!
//! type Alloc = NonPagedAlloc<DEFAULT_MEMORY_TAG, true>;
//!
//! let ptr = Alloc::allocate(64);
//! assert!(!ptr.is_null());
//! // SAFETY: `ptr` came from `Alloc::allocate` and needed no realignment.
//! unsafe { Alloc::deallocate(ptr) };
//! ```

#![no_std]
#![feature(allocator_api)]
#![allow(non_camel_case_types)]
#![deny(unsafe_op_in_unsafe_fn)]
#![allow(unsafe_code)]
#![deny(missing_docs)]

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

/// Default memory tag for allocations, visible via kernel debugger pool
/// inspection tools such as `!poolused`/`!poolfind`.
///
/// Built with `from_ne_bytes` deliberately. Every Windows target is
/// little-endian and the kernel reads a pool tag as a little-endian `u32`,
/// so native byte order is what makes the debugger print `Rust` rather than
/// `tsuR`. This is not a portability oversight; do not change it to
/// `from_be_bytes`.
pub const DEFAULT_MEMORY_TAG: u32 = u32::from_ne_bytes(*b"Rust");

/// A stateless, `#![no_std]`-friendly Windows kernel pool allocator.
///
/// `KAlloc` is a zero-sized type: all of its behavior is selected entirely at
/// compile time via its const generic parameters, and every operation is
/// exposed as an associated function, so no instance ever needs to be
/// constructed to use them directly. An instance is still needed to satisfy
/// the [`GlobalAlloc`]/[`Allocator`] trait bounds, and can always be created
/// with [`Default::default()`] at zero runtime cost.
///
/// # Type Parameters
/// - `ALLOC_POOL_FLAGS`: the pool flags passed to `ExAllocatePool2`,
///   selecting which pool (e.g. paged or non-paged) allocations come from.
/// - `MEMORY_TAG`: the 4-byte pool tag attached to every allocation, used by
///   tools such as `!poolused`/`!poolfind` to attribute pool memory back to
///   this allocator. Defaults to [`DEFAULT_MEMORY_TAG`].
/// - `TAGGED_DEALLOC`: selects the free routine used by [`KAlloc::deallocate`]:
///   `true` uses `ExFreePoolWithTag` (the tag must match the one the memory
///   was allocated with), `false` uses the untagged `ExFreePool`. Defaults to
///   `true`.
///
/// Prefer the [`PagedAlloc`]/[`NonPagedAlloc`] aliases over naming `KAlloc`
/// directly.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KAlloc<
    const ALLOC_POOL_FLAGS: POOL_FLAGS,
    const MEMORY_TAG: u32 = DEFAULT_MEMORY_TAG,
    const TAGGED_DEALLOC: bool = true,
>;

/// [`KAlloc`] backed by the paged pool (`POOL_FLAG_PAGED`).
///
/// Memory from the paged pool may be paged out to disk, so it may only be
/// touched at IRQL `APC_LEVEL` or below — that is, strictly below
/// `DISPATCH_LEVEL`. Never touch it from a DPC, from an ISR, or while
/// holding a spin lock.
pub type PagedAlloc<const MEMORY_TAG: u32 = DEFAULT_MEMORY_TAG, const TAGGED_DEALLOC: bool = true> =
    KAlloc<POOL_FLAG_PAGED, MEMORY_TAG, TAGGED_DEALLOC>;
/// [`KAlloc`] backed by the non-paged pool (`POOL_FLAG_NON_PAGED`).
///
/// Non-paged pool memory is always resident and safe to access at any IRQL,
/// at the cost of being a more constrained system-wide resource than paged
/// pool.
pub type NonPagedAlloc<
    const MEMORY_TAG: u32 = DEFAULT_MEMORY_TAG,
    const TAGGED_DEALLOC: bool = true,
> = KAlloc<POOL_FLAG_NON_PAGED, MEMORY_TAG, TAGGED_DEALLOC>;

/// Size, in bytes, of a pointer on the target platform.
pub const POINTER_SIZE: usize = size_of::<*const u8>();
/// The minimum alignment `ExAllocatePool2` guarantees for allocations smaller
/// than a page. Mirrors the WDK's `MEMORY_ALLOCATION_ALIGNMENT` (two
/// pointers: 16 bytes on 64-bit targets, 8 bytes on 32-bit).
pub const MIN_ALIGNMENT: usize = POINTER_SIZE * 2;

impl<const ALLOC_POOL_FLAGS: POOL_FLAGS, const MEMORY_TAG: u32, const TAGGED_DEALLOC: bool>
    KAlloc<ALLOC_POOL_FLAGS, MEMORY_TAG, TAGGED_DEALLOC>
{
    /// Returns the address, one pointer-width before `ptr`, where the
    /// original (pre-alignment) pointer is stored by
    /// [`KAlloc::save_original_pointer`].
    ///
    /// `ptr` must be a pointer returned by [`KAlloc::allocate_aligned`] for
    /// a layout that required realignment; the slot this names is only
    /// guaranteed to exist, and to belong to the caller, for such a pointer.
    /// That restriction is why this is not part of the public API: nothing
    /// in the signature can enforce it.
    #[inline(always)]
    fn original_pointer_location(ptr: *mut u8) -> *mut *mut u8 {
        unsafe { (ptr.cast::<*mut u8>()).offset(-1) }
    }

    /// Stores `original_ptr` in the pointer-sized slot immediately before
    /// `ptr`, so it can later be recovered by [`KAlloc::original_pointer`]
    /// and passed to the real free routine.
    ///
    /// `ptr` must have at least [`POINTER_SIZE`] bytes of valid, writable
    /// memory immediately before it. [`KAlloc::allocate_aligned`] guarantees
    /// that: the gap [`KAlloc::align_up_to`] opens is a multiple of
    /// [`MIN_ALIGNMENT`], which is two pointers wide. `original_ptr` is the
    /// pointer the underlying allocation actually returned, to be restored
    /// on deallocation.
    #[inline(always)]
    fn save_original_pointer(ptr: *mut u8, original_ptr: *mut u8) {
        let original_ptr_location = Self::original_pointer_location(ptr);
        unsafe {
            *original_ptr_location = original_ptr;
        }
    }

    /// Reads back the pointer previously stored by
    /// [`KAlloc::save_original_pointer`].
    ///
    /// `ptr` must be a pointer returned by [`KAlloc::allocate_aligned`] for
    /// which realignment was required; for any other pointer the slot read
    /// here holds unrelated memory.
    #[inline(always)]
    fn original_pointer(ptr: *mut u8) -> *mut u8 {
        let original_ptr_location = Self::original_pointer_location(ptr);
        unsafe { *original_ptr_location }
    }

    /// Returns the alignment guarantee `ExAllocatePool2` provides for an
    /// allocation of `size` bytes.
    ///
    /// Per the [`ExAllocatePool2` documentation][remarks], allocations of at
    /// least one page are page-aligned; smaller allocations are only
    /// guaranteed [`MIN_ALIGNMENT`].
    ///
    /// [remarks]: https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdm/nf-wdm-exallocatepool2#remarks
    ///
    /// # Parameters
    /// - `size`: the requested allocation size, in bytes.
    ///
    /// # Returns
    /// `PAGE_SIZE` if `size` is at least one page, otherwise [`MIN_ALIGNMENT`].
    ///
    /// # Examples
    /// ```
    /// use ntalloc::{PagedAlloc, MIN_ALIGNMENT, DEFAULT_MEMORY_TAG};
    /// use windows_sys::Wdk::System::SystemServices::PAGE_SIZE;
    ///
    /// type Alloc = PagedAlloc<DEFAULT_MEMORY_TAG, true>;
    ///
    /// assert_eq!(Alloc::kernel_alignment(16), MIN_ALIGNMENT);
    /// assert_eq!(Alloc::kernel_alignment(PAGE_SIZE as usize), PAGE_SIZE as usize);
    /// ```
    #[inline(always)]
    pub fn kernel_alignment(size: usize) -> usize {
        if size >= PAGE_SIZE as usize {
            PAGE_SIZE as usize
        } else {
            MIN_ALIGNMENT
        }
    }

    /// Test-only stand-in for [`KAlloc::kernel_allocate`]: allocates `size`
    /// bytes from the ordinary usermode heap, so the allocator's alignment
    /// and pointer-stashing logic can be exercised by `cargo test` without a
    /// running kernel.
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

    /// Allocates `size` bytes directly from the kernel pool via
    /// `ExAllocatePool2`, bypassing the zero-size and usermode-test shims
    /// used by [`KAlloc::allocate`].
    ///
    /// # Parameters
    /// - `size`: the number of bytes to allocate. Unlike [`KAlloc::allocate`],
    ///   `0` is not special-cased here and is forwarded to `ExAllocatePool2`
    ///   as-is.
    ///
    /// # Returns
    /// A pointer to the allocated memory, or a null pointer if the allocation
    /// fails.
    ///
    /// # Examples
    /// ```no_run
    /// use ntalloc::{NonPagedAlloc, DEFAULT_MEMORY_TAG};
    ///
    /// let ptr = NonPagedAlloc::<DEFAULT_MEMORY_TAG, true>::kernel_allocate(64);
    /// assert!(!ptr.is_null());
    /// ```
    #[inline(always)]
    pub fn kernel_allocate(size: usize) -> *mut u8 {
        unsafe {
            ExAllocatePool2(ALLOC_POOL_FLAGS, size, MEMORY_TAG) as *mut u8
        }
    }

    /// Allocates `size` bytes, returning a null pointer for a zero-size
    /// request.
    ///
    /// Under `#[cfg(test)]`, allocation is redirected to the ordinary
    /// usermode heap so this crate's own tests can exercise the allocator's
    /// logic without a kernel; otherwise this calls [`KAlloc::kernel_allocate`].
    ///
    /// # Parameters
    /// - `size`: the number of bytes to allocate.
    ///
    /// # Returns
    /// A pointer to the allocated memory, or a null pointer if `size` is `0`
    /// or the allocation fails.
    ///
    /// # Examples
    /// ```no_run
    /// use ntalloc::{NonPagedAlloc, DEFAULT_MEMORY_TAG};
    ///
    /// type Alloc = NonPagedAlloc<DEFAULT_MEMORY_TAG, true>;
    ///
    /// let ptr = Alloc::allocate(64);
    /// assert!(!ptr.is_null());
    /// // SAFETY: `ptr` came from `Alloc::allocate` and needed no realignment.
    /// unsafe { Alloc::deallocate(ptr) };
    /// ```
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

    /// Returns whether `layout`'s alignment is already satisfied by whatever
    /// alignment the kernel pool allocator natively guarantees for
    /// `layout.size()` (see [`KAlloc::kernel_alignment`]), meaning no
    /// over-allocation/realignment is needed to satisfy it.
    #[inline(always)]
    fn is_kernel_aligned(layout: Layout) -> bool {
        let pool_alignment = Self::kernel_alignment(layout.size());

        layout.align() <= pool_alignment
    }

    /// Rounds `allocated_buffer` up to the next address that is a multiple
    /// of `alignment`. The result is always strictly greater than
    /// `allocated_buffer`, even when it was already aligned, which is what
    /// leaves [`KAlloc::allocate_aligned`] room to stash a pointer just
    /// before the address it hands out.
    ///
    /// How much room is not guaranteed by this function alone. For an
    /// arbitrary address the gap can be a single byte, too small to hold a
    /// pointer. It is sufficient in [`KAlloc::allocate_aligned`] only
    /// because the address there comes from `ExAllocatePool2` and is
    /// therefore aligned to at least [`MIN_ALIGNMENT`], while `alignment`
    /// is a larger power of two — so the gap is a multiple of
    /// [`MIN_ALIGNMENT`] and never smaller than it.
    ///
    /// # Parameters
    /// - `allocated_buffer`: the pointer to align. This function does not
    ///   dereference the pointer, so it is safe to call with any address.
    /// - `alignment`: the desired alignment, in bytes. Must be a non-zero
    ///   power of two, as required by [`Layout`].
    ///
    /// # Returns
    /// The next pointer after `allocated_buffer` that is aligned to
    /// `alignment`.
    ///
    /// # Note
    /// Does not check for address overflow; with overflow checks disabled
    /// (the default in release builds), an `allocated_buffer` near the top
    /// of the address space combined with a large `alignment` would wrap
    /// silently — worth noting since kernel-mode pointers often sit near
    /// that boundary.
    ///
    /// # Examples
    /// ```
    /// use ntalloc::{PagedAlloc, DEFAULT_MEMORY_TAG};
    ///
    /// let ptr = 0x1003 as *mut u8;
    /// let aligned = PagedAlloc::<DEFAULT_MEMORY_TAG, true>::align_up_to(ptr, 64);
    /// assert_eq!(aligned as usize % 64, 0);
    /// assert!(aligned as usize > ptr as usize);
    /// ```
    #[inline(always)]
    pub fn align_up_to(allocated_buffer: *mut u8, alignment: usize) -> *mut u8 {
        let alignment_mask_base = !(alignment - 1);
        ((allocated_buffer as usize & alignment_mask_base) + alignment) as *mut u8
    }

    /// Allocates memory satisfying `layout`, handling alignment requirements
    /// stronger than what the kernel pool allocator natively provides.
    ///
    /// If `layout.align()` is no stronger than [`KAlloc::kernel_alignment`]
    /// for the requested size, this simply forwards to [`KAlloc::allocate`].
    /// Otherwise it over-allocates by `layout.align()` extra bytes, rounds
    /// the result up to `layout.align()` with [`KAlloc::align_up_to`], and
    /// stashes the original pointer just before the returned one (see the
    /// crate-level documentation for details).
    ///
    /// # Parameters
    /// - `layout`: the size and alignment to allocate for.
    ///
    /// # Returns
    /// A pointer to memory satisfying `layout`, or a null pointer on
    /// allocation failure.
    ///
    /// # Note
    /// A pointer returned from this function when realignment was required
    /// cannot be freed with the plain [`KAlloc::deallocate`] — that function
    /// has no way to recover the original pointer. Free it with
    /// [`KAlloc::deallocate_aligned`], passing the same `layout`, or through
    /// this type's [`GlobalAlloc`]/[`Allocator`] implementations, which do
    /// the same thing. (For a pointer that did *not* need realignment,
    /// [`KAlloc::deallocate`] is fine either way — but since only `layout`
    /// distinguishes the two cases, always pairing with
    /// [`KAlloc::deallocate_aligned`] is the rule that cannot go wrong.)
    ///
    /// # Examples
    /// ```no_run
    /// use core::alloc::Layout;
    /// use ntalloc::{NonPagedAlloc, DEFAULT_MEMORY_TAG};
    ///
    /// type Alloc = NonPagedAlloc<DEFAULT_MEMORY_TAG, true>;
    ///
    /// let layout = Layout::from_size_align(16, 64).unwrap();
    /// let ptr = Alloc::allocate_aligned(layout);
    /// assert!(!ptr.is_null());
    /// assert_eq!(ptr as usize % layout.align(), 0);
    ///
    /// // SAFETY: `ptr` came from `allocate_aligned` with this exact layout.
    /// unsafe { Alloc::deallocate_aligned(ptr, layout) };
    /// ```
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

    /// Implements the [`Allocator::allocate`] contract: allocates memory for
    /// `layout` via [`KAlloc::allocate_aligned`] and wraps it as a
    /// `NonNull<[u8]>` slice.
    ///
    /// # Returns
    /// - `Ok(_)` — a `NonNull<[u8]>` slice of `layout.size()` bytes.
    /// - `Err(AllocError)` — if the underlying pool allocation fails, and
    ///   also when `layout.size()` is `0` and `layout.align()` is no greater
    ///   than [`MIN_ALIGNMENT`], since [`KAlloc::allocate`] short-circuits a
    ///   zero-size request to a null pointer. The [`Allocator::allocate`]
    ///   implementation documents that case in full.
    fn allocate_slice(layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let allocated_raw = Self::allocate_aligned(layout);

        if allocated_raw.is_null() {
            Err(AllocError)
        } else {
            let allocated_slice = slice_from_raw_parts_mut(allocated_raw, layout.size());

            Ok(unsafe { NonNull::new_unchecked(allocated_slice) })
        }
    }

    /// Frees `ptr` with `ExFreePoolWithTag`, using this allocator's
    /// `MEMORY_TAG`.
    ///
    /// `ptr` must be non-null (unlike C's `free`, passing null here is not a
    /// safe no-op) and must have been allocated from the same pool with the
    /// same `MEMORY_TAG`; passing a pointer allocated with a different tag
    /// corrupts the pool.
    fn deallocate_tagged(ptr: *mut u8) {
        unsafe {
            ExFreePoolWithTag(ptr.cast(), MEMORY_TAG);
        }
    }

    /// Frees `ptr` with the untagged `ExFreePool`.
    ///
    /// `ptr` must be non-null (unlike C's `free`, passing null here is not a
    /// safe no-op) and must have been allocated from the same pool as this
    /// allocator targets.
    fn deallocate_non_tagged(ptr: *mut u8) {
        unsafe {
            ExFreePool(ptr.cast());
        }
    }

    /// Frees a pointer previously returned by [`KAlloc::kernel_allocate`] (or
    /// [`KAlloc::allocate`] outside of tests), using the free routine
    /// selected by `TAGGED_DEALLOC`.
    ///
    /// This does **not** account for the pointer-stashing done by
    /// [`KAlloc::allocate_aligned`] for over-aligned requests — see that
    /// function's documentation for when it is safe to call this directly.
    ///
    /// # Safety
    /// The caller must guarantee all of the following:
    ///
    /// - `ptr` is non-null. Unlike [`KAlloc::deallocate_aligned`], this
    ///   function does not check, and `ExFreePool`/`ExFreePoolWithTag`
    ///   bugchecks on a null pointer rather than ignoring it the way C's
    ///   `free` would.
    /// - `ptr` came from the same `KAlloc` configuration: the same pool
    ///   flags, the same `MEMORY_TAG`, and the same `TAGGED_DEALLOC`.
    ///   Freeing with a mismatched tag corrupts the pool.
    /// - `ptr` is the address the pool returned, not one that
    ///   [`KAlloc::allocate_aligned`] adjusted for over-alignment. Those go
    ///   to [`KAlloc::deallocate_aligned`] instead.
    /// - `ptr` has not already been freed, and is not used afterwards.
    ///
    /// # Examples
    /// ```no_run
    /// use ntalloc::{NonPagedAlloc, DEFAULT_MEMORY_TAG};
    ///
    /// type Alloc = NonPagedAlloc<DEFAULT_MEMORY_TAG, true>;
    ///
    /// let ptr = Alloc::allocate(64);
    /// assert!(!ptr.is_null());
    /// // SAFETY: `ptr` came from `Alloc::allocate` and needed no realignment.
    /// unsafe { Alloc::deallocate(ptr) };
    /// ```
    pub unsafe fn deallocate(ptr: *mut u8) {
        if TAGGED_DEALLOC {
            Self::deallocate_tagged(ptr);
        } else {
            Self::deallocate_non_tagged(ptr);
        }
    }

    /// Test-only counterpart to [`KAlloc::usermode_allocate`]: frees a
    /// pointer previously produced by it, reconstructing the exact
    /// [`Layout`] that was used to allocate it (including any extra bytes
    /// added for realignment), since the usermode global allocator requires
    /// the original layout to deallocate correctly.
    #[cfg(test)]
    fn usermode_deallocate(original_ptr: *mut u8, layout: Layout) {
        let allocated_size = layout.size() + if Self::is_kernel_aligned(layout) {
            0
        } else {
            layout.align()
        };

        unsafe {
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

    /// Frees a pointer returned by [`KAlloc::allocate_aligned`] for `layout`,
    /// recovering and freeing the original (pre-realignment) pointer when
    /// `layout`'s alignment required over-allocation.
    ///
    /// This is the counterpart to [`KAlloc::allocate_aligned`]: a pointer
    /// from that function must be freed through this one, because for an
    /// over-aligned request the address handed out is not the address the
    /// pool returned, and only this function knows how to recover it.
    ///
    /// Passing a null `ptr` is a no-op.
    ///
    /// # Safety
    /// The caller must guarantee all of the following:
    ///
    /// - `ptr` was returned by [`KAlloc::allocate_aligned`] on this same
    ///   `KAlloc` instantiation — the same pool flags, the same
    ///   `MEMORY_TAG`, and the same `TAGGED_DEALLOC` — or is null.
    /// - `layout` is the *exact* `Layout` that was passed to that call.
    ///   `layout` alone decides whether the original pointer is read back
    ///   from the slot before `ptr` or `ptr` is freed directly, so a
    ///   mismatched layout will either hand `ExFreePool` an interior
    ///   pointer or read [`POINTER_SIZE`] bytes of unrelated memory and
    ///   free whatever they happen to contain.
    /// - `ptr` has not already been freed, and is not used afterwards.
    ///
    /// None of these can be checked at runtime: an interior pointer and a
    /// valid one are indistinguishable, and the pool allocator has no way to
    /// report that a tag or a size did not match.
    pub unsafe fn deallocate_aligned(ptr: *mut u8, layout: Layout) {
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
        // SAFETY: original_ptr is the pool's own address — either `ptr`
        // itself when no realignment happened, or the value stashed by
        // allocate_aligned — and this fn's own contract covers the rest.
        unsafe {
            Self::deallocate(original_ptr)
        };
    }
}

unsafe impl<const ALLOC_POOL_FLAGS: POOL_FLAGS, const MEMORY_TAG: u32, const TAGGED_DEALLOC: bool>
    GlobalAlloc for KAlloc<ALLOC_POOL_FLAGS, MEMORY_TAG, TAGGED_DEALLOC>
{
    /// Allocates memory satisfying `layout` via [`KAlloc::allocate_aligned`].
    ///
    /// # Safety
    /// This method upholds the [`GlobalAlloc::alloc`] contract: `layout`
    /// must have non-zero size, and the caller must pass the exact same
    /// `layout` to [`GlobalAlloc::dealloc`] (or `realloc`) when freeing the
    /// returned pointer.
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::allocate_aligned(layout)
    }

    /// Frees a pointer previously returned by [`GlobalAlloc::alloc`] (or
    /// `realloc`) on this same allocator, correctly recovering any pointer
    /// adjustment [`KAlloc::allocate_aligned`] made for over-aligned
    /// requests.
    ///
    /// # Safety
    /// `ptr` must have been returned by a prior call to [`GlobalAlloc::alloc`]
    /// (or `realloc`) on this same `KAlloc` instantiation, and `layout` must
    /// be the exact layout used for that allocation.
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded straight from GlobalAlloc::dealloc, whose own
        // contract already requires exactly what deallocate_aligned does.
        unsafe { Self::deallocate_aligned(ptr, layout) };
    }
}

unsafe impl<const ALLOC_POOL_FLAGS: POOL_FLAGS, const MEMORY_TAG: u32, const TAGGED_DEALLOC: bool>
    Allocator for KAlloc<ALLOC_POOL_FLAGS, MEMORY_TAG, TAGGED_DEALLOC>
{
    /// Allocates memory satisfying `layout` via [`KAlloc::allocate_aligned`],
    /// returning it as a `NonNull<[u8]>` slice per the [`Allocator::allocate`]
    /// contract.
    ///
    /// # Returns
    /// - `Ok(_)` — a `NonNull<[u8]>` slice of exactly `layout.size()` bytes
    ///   (never more, even though the real underlying allocation may be
    ///   larger than that once realignment padding is accounted for).
    /// - `Err(AllocError)` — if the underlying pool allocation fails, or if
    ///   `layout.size()` is `0` *and* `layout.align()` is no greater than
    ///   [`MIN_ALIGNMENT`] — in that case [`KAlloc::allocate`] short-circuits
    ///   to a null pointer, reported here as an error rather than a dangling
    ///   pointer. A zero-size layout with a *larger* alignment instead takes
    ///   the over-allocation path in [`KAlloc::allocate_aligned`] and
    ///   returns a real, non-null (if functionally empty) allocation.
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        Self::allocate_slice(layout)
    }

    /// Frees a pointer previously returned by [`Allocator::allocate`] (or
    /// `grow`/`shrink`) on this same allocator.
    ///
    /// # Safety
    /// `ptr` must denote memory currently allocated via this allocator, and
    /// `layout` must be the same layout that was used to allocate it, per
    /// the [`Allocator::deallocate`] contract.
    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: forwarded straight from Allocator::deallocate, whose own
        // contract already requires exactly what deallocate_aligned does.
        unsafe { Self::deallocate_aligned(ptr.as_ptr(), layout) };
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

        // SAFETY: aligned_ptr came from allocate_aligned with this exact layout.
        unsafe { PagedAlloc::<DEFAULT_MEMORY_TAG, true>::deallocate_aligned(aligned_ptr, layout) };
    }

    #[test]
    fn allocate_aligned_test_no_realignment() {
        let layout = Layout::from_size_align(16, 8).unwrap();
        let aligned_ptr = PagedAlloc::<DEFAULT_MEMORY_TAG, true>::allocate_aligned(layout);
        assert!(!aligned_ptr.is_null());
        assert!((aligned_ptr as usize) % layout.align() == 0);

        // SAFETY: aligned_ptr came from allocate_aligned with this exact layout.
        unsafe { PagedAlloc::<DEFAULT_MEMORY_TAG, true>::deallocate_aligned(aligned_ptr, layout) };
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

        // SAFETY: aligned_ptr came from allocate_aligned with this exact layout.
        unsafe { PagedAlloc::<DEFAULT_MEMORY_TAG, true>::deallocate_aligned(aligned_ptr, layout) };
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

        // SAFETY: aligned_ptr came from allocate_aligned with this exact layout.
        unsafe { PagedAlloc::<DEFAULT_MEMORY_TAG, true>::deallocate_aligned(aligned_ptr, layout) };
    }
}
