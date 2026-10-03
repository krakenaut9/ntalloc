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
//! - [`NtAlloc`] is a zero-sized allocator type parameterized over the pool
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
//! at least one page (see [`NtAlloc::kernel_alignment`]). Rust [`Layout`]s can
//! request stricter alignment than that, so when a request exceeds what the
//! pool allocator natively guarantees, [`NtAlloc::allocate_aligned`]
//! over-allocates by `layout.align()` extra bytes, rounds the returned
//! pointer up to the requested alignment, and stashes the original
//! (unaligned) pointer in the pointer-sized slot immediately before the
//! aligned pointer. There is always room for that slot because
//! `ExAllocatePool2` never returns a base below [`MIN_ALIGNMENT`], so
//! rounding up moves the pointer by a multiple of [`MIN_ALIGNMENT`] — two
//! pointers — and never by less. [`NtAlloc::deallocate_aligned`] reads that
//! slot back out to recover the pointer that must be passed to
//! `ExFreePool`/`ExFreePoolWithTag`, which is why an over-aligned allocation
//! has to be released through it rather than through [`NtAlloc::deallocate`].
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

use crate::bindings::{POOL_FLAG_NON_PAGED, POOL_FLAG_PAGED, POOL_FLAG_UNINITIALIZED, POOL_FLAGS};
use alloc::alloc::{AllocError, Allocator};
use core::{
    alloc::{AllocatorClone, GlobalAlloc, Layout, StaticAllocator},
    num::NonZeroUsize,
    ptr::{NonNull, null_mut, slice_from_raw_parts_mut, without_provenance_mut},
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
/// `NtAlloc` is a zero-sized type: all of its behavior is selected entirely at
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
/// - `TAGGED_DEALLOC`: selects the free routine used by [`NtAlloc::deallocate`]:
///   `true` uses `ExFreePoolWithTag` (the tag must match the one the memory
///   was allocated with), `false` uses the untagged `ExFreePool`. Defaults to
///   `true`.
///
/// Prefer the [`PagedAlloc`]/[`NonPagedAlloc`] aliases over naming `NtAlloc`
/// directly.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NtAlloc<
    const ALLOC_POOL_FLAGS: POOL_FLAGS,
    const MEMORY_TAG: u32 = DEFAULT_MEMORY_TAG,
    const TAGGED_DEALLOC: bool = true,
>;

/// [`NtAlloc`] backed by the paged pool (`POOL_FLAG_PAGED`).
///
/// Memory from the paged pool may be paged out to disk, so it may only be
/// touched at IRQL `APC_LEVEL` or below — that is, strictly below
/// `DISPATCH_LEVEL`. Never touch it from a DPC, from an ISR, or while
/// holding a spin lock.
pub type PagedAlloc<const MEMORY_TAG: u32 = DEFAULT_MEMORY_TAG, const TAGGED_DEALLOC: bool = true> =
    NtAlloc<POOL_FLAG_PAGED, MEMORY_TAG, TAGGED_DEALLOC>;
/// [`NtAlloc`] backed by the non-paged pool (`POOL_FLAG_NON_PAGED`).
///
/// Non-paged pool memory is always resident and safe to access at any IRQL,
/// at the cost of being a more constrained system-wide resource than paged
/// pool.
pub type NonPagedAlloc<
    const MEMORY_TAG: u32 = DEFAULT_MEMORY_TAG,
    const TAGGED_DEALLOC: bool = true,
> = NtAlloc<POOL_FLAG_NON_PAGED, MEMORY_TAG, TAGGED_DEALLOC>;

/// Size, in bytes, of a pointer on the target platform.
pub const POINTER_SIZE: usize = size_of::<*const u8>();
/// The minimum alignment `ExAllocatePool2` guarantees for allocations smaller
/// than a page. Mirrors the WDK's `MEMORY_ALLOCATION_ALIGNMENT` (two
/// pointers: 16 bytes on 64-bit targets, 8 bytes on 32-bit).
pub const MIN_ALIGNMENT: usize = POINTER_SIZE * 2;

impl<const ALLOC_POOL_FLAGS: POOL_FLAGS, const MEMORY_TAG: u32, const TAGGED_DEALLOC: bool>
    NtAlloc<ALLOC_POOL_FLAGS, MEMORY_TAG, TAGGED_DEALLOC>
{
    /// Whether allocations from this configuration arrive zero-initialized.
    ///
    /// `ExAllocatePool2` zeroes every block it returns unless
    /// `POOL_FLAG_UNINITIALIZED` is among `ALLOC_POOL_FLAGS`, so this is a
    /// property of the type, resolved at compile time. It is `true` for
    /// [`PagedAlloc`] and [`NonPagedAlloc`].
    ///
    /// The [`GlobalAlloc::alloc_zeroed`] and [`Allocator::allocate_zeroed`]
    /// implementations consult it to skip the redundant second pass their
    /// default implementations would otherwise make over memory the pool has
    /// already zeroed.
    pub const POOL_ZEROES: bool = (ALLOC_POOL_FLAGS & POOL_FLAG_UNINITIALIZED) == 0;

    /// Returns the address, one pointer-width before `ptr`, where the
    /// original (pre-alignment) pointer is stored by
    /// [`NtAlloc::save_original_pointer`].
    ///
    /// `ptr` must be a pointer returned by [`NtAlloc::allocate_aligned`] for
    /// a layout that required realignment; the slot this names is only
    /// guaranteed to exist, and to belong to the caller, for such a pointer.
    /// That restriction is why this is not part of the public API: nothing
    /// in the signature can enforce it.
    #[inline(always)]
    fn original_pointer_location(ptr: *mut u8) -> *mut *mut u8 {
        unsafe { (ptr.cast::<*mut u8>()).offset(-1) }
    }

    /// Stores `original_ptr` in the pointer-sized slot immediately before
    /// `ptr`, so it can later be recovered by [`NtAlloc::original_pointer`]
    /// and passed to the real free routine.
    ///
    /// `ptr` must have at least [`POINTER_SIZE`] bytes of valid, writable
    /// memory immediately before it. [`NtAlloc::allocate_aligned`] guarantees
    /// that: the gap [`NtAlloc::align_up_to`] opens is a multiple of
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
    /// [`NtAlloc::save_original_pointer`].
    ///
    /// `ptr` must be a pointer returned by [`NtAlloc::allocate_aligned`] for
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

    /// Test-only stand-in for [`NtAlloc::kernel_allocate`]: allocates `size`
    /// bytes from the ordinary usermode heap, so the allocator's alignment
    /// and pointer-stashing logic can be exercised by `cargo test` without a
    /// running kernel.
    ///
    /// Guards zero exactly as [`NtAlloc::kernel_allocate`] does — `alloc` has
    /// the same non-zero requirement `ExAllocatePool2` has — so that the
    /// shipped path carries no check its backend already makes.
    ///
    /// Mirrors the pool's zeroing too, via [`NtAlloc::POOL_ZEROES`]. `alloc`
    /// promises nothing about initialization while `ExAllocatePool2` always
    /// zeroes, so without this the tests would exercise a backend that
    /// behaves differently from the shipped one on exactly the property
    /// [`GlobalAlloc::alloc_zeroed`] depends on.
    #[cfg(test)]
    #[inline(always)]
    fn usermode_allocate(size: usize) -> *mut u8 {
        if size == 0 {
            return null_mut();
        }

        let layout = Layout::from_size_align(size, Self::kernel_alignment(size))
            .expect("Failed to create layout");

        unsafe {
            if Self::POOL_ZEROES {
                alloc::alloc::alloc_zeroed(layout)
            } else {
                alloc::alloc::alloc(layout)
            }
        }
    }

    /// Allocates `size` bytes directly from the kernel pool via
    /// `ExAllocatePool2`, bypassing the usermode-test shim used by
    /// [`NtAlloc::allocate`].
    ///
    /// # Parameters
    /// - `size`: the number of bytes to allocate. A zero-size request never
    ///   reaches the pool: `ExAllocatePool2` documents `NumberOfBytes` as
    ///   "a non-zero number of bytes", and lists only memory exhaustion and
    ///   a bad tag or flags as reasons it returns null — so it defines no
    ///   behaviour at all for `0`. This returns null instead.
    ///
    /// # Returns
    /// A pointer to the allocated memory, or a null pointer if `size` is `0`
    /// or the allocation fails.
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
        if size == 0 {
            return null_mut();
        }

        unsafe { ExAllocatePool2(ALLOC_POOL_FLAGS, size, MEMORY_TAG) as *mut u8 }
    }

    /// Allocates `size` bytes, returning a null pointer for a zero-size
    /// request.
    ///
    /// Under `#[cfg(test)]`, allocation is redirected to the ordinary
    /// usermode heap so this crate's own tests can exercise the allocator's
    /// logic without a kernel; otherwise this calls [`NtAlloc::kernel_allocate`].
    ///
    /// The zero-size check lives in each backend rather than here, so the
    /// shipped path performs it once instead of twice.
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
    /// `layout.size()` (see [`NtAlloc::kernel_alignment`]), meaning no
    /// over-allocation/realignment is needed to satisfy it.
    ///
    /// This runs twice per allocation: once in [`NtAlloc::allocate_aligned`] to
    /// decide whether to stash the pool's own pointer, and again in
    /// [`NtAlloc::deallocate_aligned`] to decide whether to read it back. The
    /// two answers must agree — if the free path decides "not stashed" for a
    /// pointer that was, it hands `ExFreePool` an address inside the block and
    /// corrupts the pool.
    ///
    /// Agreement is not automatic, because this depends on `layout.size()` and
    /// [`Allocator::deallocate`] accepts any size that *fits* rather than the
    /// exact one. Two separate properties currently guarantee it; both are
    /// checked by `free_path_decision_cannot_flip`.
    #[inline(always)]
    fn is_kernel_aligned(layout: Layout) -> bool {
        let pool_alignment = Self::kernel_alignment(layout.size());

        layout.align() <= pool_alignment
    }

    /// Rounds `allocated_buffer` up to the next address that is a multiple
    /// of `alignment`. The result is always strictly greater than
    /// `allocated_buffer`, even when it was already aligned, which is what
    /// leaves [`NtAlloc::allocate_aligned`] room to stash a pointer just
    /// before the address it hands out.
    ///
    /// How much room is not guaranteed by this function alone. For an
    /// arbitrary address the gap can be a single byte, too small to hold a
    /// pointer. It is sufficient in [`NtAlloc::allocate_aligned`] only
    /// because the address there comes from `ExAllocatePool2` and is
    /// therefore aligned to at least [`MIN_ALIGNMENT`], while `alignment`
    /// is a larger power of two — so the gap is a multiple of
    /// [`MIN_ALIGNMENT`] and never smaller than it.
    ///
    /// # Parameters
    /// - `allocated_buffer`: the pointer to align. This function does not
    ///   dereference the pointer, so any address is acceptable.
    /// - `alignment`: the desired alignment, in bytes. [`NonZeroUsize`]
    ///   rather than `usize` because `alignment - 1` below would otherwise
    ///   underflow on `0`, wrapping to a null result in release builds. It
    ///   must also be a power of two — the masking trick silently returns a
    ///   non-multiple otherwise — which the type cannot express, so a
    ///   `debug_assert!` covers that half. [`Layout`] guarantees both
    ///   properties, and every caller's alignment comes from a [`Layout`].
    ///
    /// # Returns
    /// The next address after `allocated_buffer` that is a multiple of
    /// `alignment`, as a pointer still derived from `allocated_buffer` and
    /// therefore still carrying its provenance.
    ///
    /// # Note
    /// Does not check for address overflow; with overflow checks disabled
    /// (the default in release builds), an `allocated_buffer` near the top
    /// of the address space combined with a large `alignment` would wrap
    /// silently — worth noting since kernel-mode pointers often sit near
    /// that boundary.
    #[inline(always)]
    fn align_up_to(allocated_buffer: *mut u8, alignment: NonZeroUsize) -> *mut u8 {
        debug_assert!(
            alignment.is_power_of_two(),
            "alignment must be a power of two; the mask below is meaningless otherwise"
        );

        let alignment = alignment.get();
        let alignment_mask_base = !(alignment - 1);

        // `map_addr` rather than a cast through `usize`: it produces the same
        // address but keeps the result derived from `allocated_buffer`, so the
        // pointer still carries provenance over the pool block it points into.
        // Casting out to an integer and back drops that lineage, leaving the
        // compiler to guess which allocation was meant — defined under
        // exposed provenance, but the one construct Miri cannot check.
        allocated_buffer.map_addr(|addr| (addr & alignment_mask_base) + alignment)
    }

    /// Allocates memory satisfying `layout`, handling alignment requirements
    /// stronger than what the kernel pool allocator natively provides.
    ///
    /// If `layout.align()` is no stronger than [`NtAlloc::kernel_alignment`]
    /// for the requested size, this simply forwards to [`NtAlloc::allocate`].
    /// Otherwise it over-allocates by `layout.align()` extra bytes, rounds
    /// the result up to `layout.align()`, and
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
    /// A zero-size `layout` always returns a null pointer, and never reaches
    /// the pool. [`GlobalAlloc`] calls a zero-size request undefined
    /// behaviour but invites implementations to "guarantee a sentinel address
    /// or a null pointer in response to a zero-size allocation request" —
    /// this is that guarantee. Callers needing a usable pointer for an empty
    /// allocation want the [`Allocator`] impl, which returns a dangling one
    /// as that trait requires.
    ///
    /// # Note
    /// A pointer returned from this function when realignment was required
    /// cannot be freed with the plain [`NtAlloc::deallocate`] — that function
    /// has no way to recover the original pointer. Free it with
    /// [`NtAlloc::deallocate_aligned`], passing the same `layout`, or through
    /// this type's [`GlobalAlloc`]/[`Allocator`] implementations, which do
    /// the same thing. (For a pointer that did *not* need realignment,
    /// [`NtAlloc::deallocate`] is fine either way — but since only `layout`
    /// distinguishes the two cases, always pairing with
    /// [`NtAlloc::deallocate_aligned`] is the rule that cannot go wrong.)
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
        if layout.size() == 0 {
            return null_mut();
        }

        if Self::is_kernel_aligned(layout) {
            Self::allocate(layout.size())
        } else {
            let extended_buffer_size = layout.size() + layout.align();
            let buffer = Self::allocate(extended_buffer_size);
            if buffer.is_null() {
                return null_mut();
            }

            // SAFETY: Layout guarantees a non-zero power-of-two alignment.
            let alignment = unsafe { NonZeroUsize::new_unchecked(layout.align()) };
            let aligned = Self::align_up_to(buffer, alignment);

            debug_assert!(
                (aligned as usize).saturating_add(layout.size())
                    <= (buffer as usize).saturating_add(extended_buffer_size),
                "the realigned region must fit inside the block actually allocated"
            );
            debug_assert!(
                (aligned as usize) - (buffer as usize) >= POINTER_SIZE,
                "the gap must be wide enough to hold the stashed pointer"
            );

            Self::save_original_pointer(aligned, buffer);

            aligned
        }
    }

    /// Implements the [`Allocator::allocate`] contract: allocates memory for
    /// `layout` via [`NtAlloc::allocate_aligned`] and wraps it as a
    /// `NonNull<[u8]>` slice.
    ///
    /// # Returns
    /// - `Ok(_)` — a `NonNull<[u8]>` slice of `layout.size()` bytes.
    /// - `Err(AllocError)` — if the underlying pool allocation fails, and
    ///   also when `layout.size()` is `0` and `layout.align()` is no greater
    ///   than [`MIN_ALIGNMENT`], since [`NtAlloc::allocate`] short-circuits a
    ///   zero-size request to a null pointer. The [`Allocator::allocate`]
    ///   implementation documents that case in full.
    fn allocate_slice(layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if layout.size() == 0 {
            // Allocator, unlike GlobalAlloc, requires zero-sized allocations
            // to succeed, and ExAllocatePool2 has no defined behaviour for a
            // zero-size request — so this must not reach the pool. A dangling
            // but correctly aligned pointer is exactly what the contract asks
            // for, and deallocate_aligned discards it without freeing.
            let dangling = without_provenance_mut::<u8>(layout.align());
            let empty = slice_from_raw_parts_mut(dangling, 0);

            // SAFETY: layout.align() is a non-zero power of two, so the
            // address is non-null.
            return Ok(unsafe { NonNull::new_unchecked(empty) });
        }

        let allocated_raw = Self::allocate_aligned(layout);

        if allocated_raw.is_null() {
            Err(AllocError)
        } else {
            // Exactly layout.size(), never the larger block really taken from
            // the pool. Allocator permits reporting more, but whatever is
            // reported becomes `max` in the range of sizes that
            // Allocator::deallocate will then accept — and the free path
            // recomputes the stash decision from the size it is handed. Keeping
            // max == min leaves callers one legal size and no way to move that
            // decision. This line is load-bearing; see is_kernel_aligned.
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

    /// Frees a pointer previously returned by [`NtAlloc::kernel_allocate`] (or
    /// [`NtAlloc::allocate`] outside of tests), using the free routine
    /// selected by `TAGGED_DEALLOC`.
    ///
    /// This does **not** account for the pointer-stashing done by
    /// [`NtAlloc::allocate_aligned`] for over-aligned requests — see that
    /// function's documentation for when it is safe to call this directly.
    ///
    /// # Safety
    /// The caller must guarantee all of the following:
    ///
    /// - `ptr` is non-null. Unlike [`NtAlloc::deallocate_aligned`], this
    ///   function does not check, and `ExFreePool`/`ExFreePoolWithTag`
    ///   bugchecks on a null pointer rather than ignoring it the way C's
    ///   `free` would.
    /// - `ptr` came from the same `NtAlloc` configuration: the same pool
    ///   flags, the same `MEMORY_TAG`, and the same `TAGGED_DEALLOC`.
    ///   Freeing with a mismatched tag corrupts the pool.
    /// - `ptr` is the address the pool returned, not one that
    ///   [`NtAlloc::allocate_aligned`] adjusted for over-alignment. Those go
    ///   to [`NtAlloc::deallocate_aligned`] instead.
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

    /// Test-only counterpart to [`NtAlloc::usermode_allocate`]: frees a
    /// pointer previously produced by it, reconstructing the exact
    /// [`Layout`] that was used to allocate it (including any extra bytes
    /// added for realignment), since the usermode global allocator requires
    /// the original layout to deallocate correctly.
    #[cfg(test)]
    fn usermode_deallocate(original_ptr: *mut u8, layout: Layout) {
        let allocated_size = layout.size()
            + if Self::is_kernel_aligned(layout) {
                0
            } else {
                layout.align()
            };

        unsafe {
            alloc::alloc::dealloc(
                original_ptr,
                Layout::from_size_align(allocated_size, Self::kernel_alignment(allocated_size))
                    .expect("Failed to create layout"),
            );
        }
    }

    /// Frees a pointer returned by [`NtAlloc::allocate_aligned`] for `layout`,
    /// recovering and freeing the original (pre-realignment) pointer when
    /// `layout`'s alignment required over-allocation.
    ///
    /// This is the counterpart to [`NtAlloc::allocate_aligned`]: a pointer
    /// from that function must be freed through this one, because for an
    /// over-aligned request the address handed out is not the address the
    /// pool returned, and only this function knows how to recover it.
    ///
    /// Passing a null `ptr` is a no-op.
    ///
    /// # Safety
    /// The caller must guarantee all of the following:
    ///
    /// - `ptr` was returned by [`NtAlloc::allocate_aligned`] on this same
    ///   `NtAlloc` instantiation — the same pool flags, the same
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

        if layout.size() == 0 {
            // Nothing was ever taken from the pool for a zero-size layout:
            // allocate_aligned returns null, and allocate_slice returns a
            // dangling pointer. Either way there is nothing to give back.
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
    GlobalAlloc for NtAlloc<ALLOC_POOL_FLAGS, MEMORY_TAG, TAGGED_DEALLOC>
{
    /// Allocates memory satisfying `layout` via [`NtAlloc::allocate_aligned`].
    ///
    /// # Safety
    /// This method upholds the [`GlobalAlloc::alloc`] contract: `layout`
    /// must have non-zero size, and the caller must pass the exact same
    /// `layout` to [`GlobalAlloc::dealloc`] (or `realloc`) when freeing the
    /// returned pointer.
    ///
    /// Should a zero-size `layout` arrive anyway, this returns null rather
    /// than reaching `ExAllocatePool2`, which documents no behaviour for a
    /// zero-size request. See [`NtAlloc::allocate_aligned`].
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::allocate_aligned(layout)
    }

    /// Frees a pointer previously returned by [`GlobalAlloc::alloc`] (or
    /// `realloc`) on this same allocator, correctly recovering any pointer
    /// adjustment [`NtAlloc::allocate_aligned`] made for over-aligned
    /// requests.
    ///
    /// # Safety
    /// `ptr` must have been returned by a prior call to [`GlobalAlloc::alloc`]
    /// (or `realloc`) on this same `NtAlloc` instantiation, and `layout` must
    /// be the exact layout used for that allocation.
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded straight from GlobalAlloc::dealloc, whose own
        // contract already requires exactly what deallocate_aligned does.
        unsafe { Self::deallocate_aligned(ptr, layout) };
    }

    /// Allocates zero-initialized memory satisfying `layout`.
    ///
    /// The default implementation of this method allocates and then writes
    /// zeroes over the whole block. `ExAllocatePool2` has already done that,
    /// so when [`NtAlloc::POOL_ZEROES`] holds this simply returns the
    /// allocation and the second pass is skipped — the condition is a const,
    /// so only one branch survives compilation.
    ///
    /// For an over-aligned request the returned region still arrives zeroed:
    /// it lies inside the larger block the pool zeroed, and the pointer
    /// [`NtAlloc::allocate_aligned`] stashes goes in the bytes *before* it.
    ///
    /// # Safety
    /// As [`GlobalAlloc::alloc`]: `layout` must have non-zero size, and the
    /// same `layout` must come back on the free path.
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = Self::allocate_aligned(layout);

        // The left operand is a const, so when the pool zeroes this whole
        // branch — the null test included — is compiled away. When it does
        // not, null still has to be excluded: it means the allocation failed
        // or `layout` was zero-sized (see [`NtAlloc::allocate_aligned`]), and
        // null is not a valid write target even for a zero-length write.
        if !Self::POOL_ZEROES && !ptr.is_null() {
            // SAFETY: the allocation succeeded, so layout.size() bytes from
            // `ptr` are valid for writes.
            unsafe { ptr.write_bytes(0, layout.size()) };
        }

        ptr
    }
}

unsafe impl<const ALLOC_POOL_FLAGS: POOL_FLAGS, const MEMORY_TAG: u32, const TAGGED_DEALLOC: bool>
    Allocator for NtAlloc<ALLOC_POOL_FLAGS, MEMORY_TAG, TAGGED_DEALLOC>
{
    /// Allocates memory satisfying `layout` via [`NtAlloc::allocate_aligned`],
    /// returning it as a `NonNull<[u8]>` slice per the [`Allocator::allocate`]
    /// contract.
    ///
    /// # Returns
    /// - `Ok(_)` — a `NonNull<[u8]>` slice of exactly `layout.size()` bytes
    ///   (never more, even though the real underlying allocation may be
    ///   larger than that once realignment padding is accounted for).
    /// - `Ok(_)` — for a zero-size `layout`, a dangling pointer aligned to
    ///   `layout.align()` with a length of `0`. [`Allocator`] requires
    ///   zero-sized allocations to succeed, unlike [`GlobalAlloc`], and the
    ///   pool is never involved. Passing it back to
    ///   [`Allocator::deallocate`] is correct and does nothing.
    /// - `Err(AllocError)` — only if the underlying pool allocation fails.
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        Self::allocate_slice(layout)
    }

    /// Allocates zero-initialized memory satisfying `layout`.
    ///
    /// Skips the second zeroing pass the default implementation would make
    /// whenever [`NtAlloc::POOL_ZEROES`] holds, for the reasons given on
    /// [`GlobalAlloc::alloc_zeroed`]. A zero-sized `layout` yields the same
    /// dangling, length-`0` slice [`Allocator::allocate`] returns, which is
    /// trivially zeroed.
    fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let allocated = Self::allocate_slice(layout)?;

        if !Self::POOL_ZEROES {
            // SAFETY: allocate_slice returned a block of exactly this length.
            unsafe {
                allocated
                    .cast::<u8>()
                    .as_ptr()
                    .write_bytes(0, allocated.len())
            };
        }

        Ok(allocated)
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

// SAFETY: [`StaticAllocator`] asks that nothing about the allocator's own
// lifetime can invalidate memory it handed out. For [`NtAlloc`]:
//
// - it has no `Drop` impl, so dropping one releases nothing;
// - it is a zero-sized type holding no state;
// - its lifetime expiring frees nothing, because it owns nothing;
// - nothing reachable from safe code can invalidate an allocation. The safe
//   public surface is [`NtAlloc::POOL_ZEROES`], [`NtAlloc::kernel_alignment`],
//   [`NtAlloc::kernel_allocate`], [`NtAlloc::allocate`] and
//   [`NtAlloc::allocate_aligned`], none of which free anything. Of the five
//   functions that do return memory, two are `unsafe`
//   ([`NtAlloc::deallocate`] and [`NtAlloc::deallocate_aligned`]) and the other
//   three are private and called only from inside those two;
// - all of the above hold for every equivalent allocator, since instances
//   sharing the same const parameters are indistinguishable.
//
// A pool block therefore stays valid until the matching `ExFreePool` or
// `ExFreePoolWithTag`, regardless of what happens to the allocator value.
unsafe impl<const ALLOC_POOL_FLAGS: POOL_FLAGS, const MEMORY_TAG: u32, const TAGGED_DEALLOC: bool>
    StaticAllocator for NtAlloc<ALLOC_POOL_FLAGS, MEMORY_TAG, TAGGED_DEALLOC>
{
}

// SAFETY: [`AllocatorClone`] asks that a clone be interchangeable with the
// original, so that either can free what the other allocated. [`NtAlloc`] is a
// zero-sized type carrying no state: the pool, the tag and the free routine all
// come from its const parameters, so two values of the same type forward to the
// same pool calls identically. Moving or dropping one frees nothing, so blocks
// stay valid while clones live, and the derived `Clone` copies a zero-sized
// value and cannot unwind. The type is also `Copy`, which preserves that
// equivalence exactly as cloning does.
unsafe impl<const ALLOC_POOL_FLAGS: POOL_FLAGS, const MEMORY_TAG: u32, const TAGGED_DEALLOC: bool>
    AllocatorClone for NtAlloc<ALLOC_POOL_FLAGS, MEMORY_TAG, TAGGED_DEALLOC>
{
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn require_realignment_test() {
        let size = 16;

        for shift in 0..=16u32 {
            let alignment = 1usize << shift;
            let layout = Layout::from_size_align(size, alignment).unwrap();
            // is_kernel_aligned is true when the pool already satisfies the
            // layout, i.e. when no realignment is needed.
            let already_satisfied =
                PagedAlloc::<DEFAULT_MEMORY_TAG, true>::is_kernel_aligned(layout);

            if alignment <= MIN_ALIGNMENT {
                assert!(
                    already_satisfied,
                    "alignment {alignment} is within MIN_ALIGNMENT, so the pool                      should already satisfy it and no realignment is needed"
                );
            } else {
                assert!(
                    !already_satisfied,
                    "alignment {alignment} exceeds what the pool guarantees for                      a {size}-byte request, so realignment is needed"
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
        let aligned_ptr = PagedAlloc::<DEFAULT_MEMORY_TAG, true>::align_up_to(
            ptr,
            NonZeroUsize::new(alignment).unwrap(),
        );
        assert_eq!(aligned_ptr as usize % alignment, 0);
        assert!(aligned_ptr as usize > ptr as usize);
    }

    #[test]
    fn align_up_to_test_greater_than_min_alignment() {
        let ptr = 0x1003 as *mut u8;
        let alignment = 64;
        let aligned_ptr = PagedAlloc::<DEFAULT_MEMORY_TAG, true>::align_up_to(
            ptr,
            NonZeroUsize::new(alignment).unwrap(),
        );
        assert_eq!(aligned_ptr as usize % alignment, 0);
        assert!(aligned_ptr as usize > ptr as usize);
    }

    #[test]
    fn align_up_to_test_page_size_alignment() {
        let ptr = 0x1003 as *mut u8;
        let alignment = PAGE_SIZE as usize;
        let aligned_ptr = PagedAlloc::<DEFAULT_MEMORY_TAG, true>::align_up_to(
            ptr,
            NonZeroUsize::new(alignment).unwrap(),
        );
        assert_eq!(aligned_ptr as usize % alignment, 0);
        assert!(aligned_ptr as usize > ptr as usize);
    }

    #[test]
    fn align_up_to_test_greater_than_page_size_alignment() {
        let ptr = 0x1003 as *mut u8;
        let alignment = PAGE_SIZE as usize * 2;
        let aligned_ptr = PagedAlloc::<DEFAULT_MEMORY_TAG, true>::align_up_to(
            ptr,
            NonZeroUsize::new(alignment).unwrap(),
        );
        assert_eq!(aligned_ptr as usize % alignment, 0);
        assert!(aligned_ptr as usize > ptr as usize);
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

    #[test]
    fn align_up_to_advances_from_an_already_aligned_base() {
        // The case the stash scheme actually depends on. Returning an
        // already-aligned base unchanged would leave no room in front of it.
        for alignment in [MIN_ALIGNMENT, 64, PAGE_SIZE as usize] {
            let base = alignment * 4;
            let aligned = PagedAlloc::<DEFAULT_MEMORY_TAG, true>::align_up_to(
                base as *mut u8,
                NonZeroUsize::new(alignment).unwrap(),
            ) as usize;

            assert_eq!(
                aligned,
                base + alignment,
                "alignment {alignment}: expected a full step past an aligned base"
            );
        }
    }

    #[test]
    fn align_up_to_always_leaves_room_for_the_stash() {
        // The gap must hold a pointer. That is not true for an arbitrary base
        // -- it can be a single byte -- and holds here only because
        // ExAllocatePool2 never returns a base below MIN_ALIGNMENT. Walk every
        // residue such a base can have.
        for alignment in [32usize, 64, 128, PAGE_SIZE as usize] {
            for step in 0..(alignment / MIN_ALIGNMENT) {
                let base = 0x10_0000 + step * MIN_ALIGNMENT;
                let aligned = PagedAlloc::<DEFAULT_MEMORY_TAG, true>::align_up_to(
                    base as *mut u8,
                    NonZeroUsize::new(alignment).unwrap(),
                ) as usize;
                let gap = aligned - base;

                assert!(
                    gap >= POINTER_SIZE,
                    "alignment {alignment}, base {base:#x}: gap {gap} cannot hold a pointer"
                );
                assert_eq!(
                    gap % MIN_ALIGNMENT,
                    0,
                    "gap should be a MIN_ALIGNMENT multiple"
                );
            }
        }
    }

    #[test]
    fn zero_size_allocations_never_reach_the_pool() {
        // ExAllocatePool2 documents NumberOfBytes as non-zero and defines no
        // behaviour for 0, so every entry point has to stop short of it.
        assert!(PagedAlloc::<DEFAULT_MEMORY_TAG, true>::allocate(0).is_null());

        for alignment in [1usize, 8, MIN_ALIGNMENT, 32, PAGE_SIZE as usize] {
            let layout = Layout::from_size_align(0, alignment).unwrap();
            let ptr = PagedAlloc::<DEFAULT_MEMORY_TAG, true>::allocate_aligned(layout);

            assert!(
                ptr.is_null(),
                "alignment {alignment}: a zero-size request must not allocate"
            );
        }
    }

    #[test]
    fn allocator_accepts_zero_sized_layouts() {
        // Allocator, unlike GlobalAlloc, requires these to succeed. Freeing
        // the dangling pointer again must be a no-op rather than a crash.
        for alignment in [1usize, 8, MIN_ALIGNMENT, 32, PAGE_SIZE as usize] {
            let layout = Layout::from_size_align(0, alignment).unwrap();
            let allocated = PagedAlloc::<DEFAULT_MEMORY_TAG, true>::allocate_slice(layout);

            let slice = allocated
                .unwrap_or_else(|_| panic!("alignment {alignment}: zero-size layout was rejected"));

            assert_eq!(slice.len(), 0, "alignment {alignment}: length must be 0");
            assert_eq!(
                slice.as_ptr().cast::<u8>() as usize % alignment,
                0,
                "alignment {alignment}: dangling pointer must still be aligned"
            );

            // SAFETY: came from allocate_slice with this exact layout.
            unsafe {
                PagedAlloc::<DEFAULT_MEMORY_TAG, true>::deallocate_aligned(
                    slice.as_ptr().cast::<u8>(),
                    layout,
                )
            };
        }
    }

    #[test]
    fn allocator_grow_from_zero_and_shrink_to_zero() {
        // The default grow/shrink implementations allocate, copy, then free the
        // old block -- so a zero-size layout reaches deallocate from a path the
        // direct tests never take.
        let allocator = PagedAlloc::<DEFAULT_MEMORY_TAG, true>::default();
        let empty_layout = Layout::from_size_align(0, MIN_ALIGNMENT).unwrap();
        let full_layout = Layout::from_size_align(64, MIN_ALIGNMENT).unwrap();

        let empty = allocator
            .allocate(empty_layout)
            .expect("zero-size allocate");
        assert_eq!(empty.len(), 0);

        // Growing away from the dangling pointer must not try to free it.
        let grown = unsafe { allocator.grow(empty.cast(), empty_layout, full_layout) }
            .expect("grow from zero");
        assert_eq!(grown.len(), 64);

        // Shrinking to zero must still release the real allocation.
        let shrunk = unsafe { allocator.shrink(grown.cast(), full_layout, empty_layout) }
            .expect("shrink to zero");
        assert_eq!(shrunk.len(), 0);

        unsafe { allocator.deallocate(shrunk.cast(), empty_layout) };
    }

    #[test]
    fn allocator_zeroed_accepts_zero_sized_layouts() {
        let allocator = PagedAlloc::<DEFAULT_MEMORY_TAG, true>::default();
        let layout = Layout::from_size_align(0, 32).unwrap();

        let empty = allocator
            .allocate_zeroed(layout)
            .expect("zero-size allocate_zeroed");

        assert_eq!(empty.len(), 0);
        unsafe { allocator.deallocate(empty.cast(), layout) };
    }

    #[test]
    fn kernel_allocate_guards_zero_without_reaching_the_pool() {
        // kernel_allocate has no cfg(test) shim, so this genuinely calls the
        // shipped code path. It is safe to run in a test binary for exactly
        // one argument: 0, where the guard returns before ExAllocatePool2.
        // Remove the guard and this reaches the pool from user mode and dies.
        assert!(PagedAlloc::<DEFAULT_MEMORY_TAG, true>::kernel_allocate(0).is_null());
    }

    /// Pins the one property that keeps the allocate/free stash decision
    /// consistent: [`NtAlloc::allocate_slice`] reports exactly `layout.size()`,
    /// so `min == max` in the range [`Allocator::deallocate`] accepts and a
    /// caller has exactly one legal size to free with.
    ///
    /// The reported size is read back from `allocate_slice` rather than
    /// assumed, so reporting the larger block really taken from the pool —
    /// which the `Allocator` contract permits — widens the range here and
    /// fails the flip check below.
    ///
    /// This does *not* guard the `size + align` over-allocation; that is
    /// checked by the `debug_assert!`s inside [`NtAlloc::allocate_aligned`],
    /// which is the only place both values are in scope.
    #[test]
    fn free_path_decision_cannot_flip() {
        type Alloc = PagedAlloc<DEFAULT_MEMORY_TAG, true>;
        let page = PAGE_SIZE as usize;

        for shift in 0..14u32 {
            let align = 1usize << shift;
            let bands = (1..=3).flat_map(|m| (m * page - 2)..=(m * page + 2));

            for size in (1usize..=200).chain(bands) {
                let Ok(layout) = Layout::from_size_align(size, align) else {
                    continue;
                };
                let stashed = !Alloc::is_kernel_aligned(layout);

                // Observed, not derived: this is the `max` of the fitting range.
                let slice = Alloc::allocate_slice(layout).expect("allocation");
                let reported = slice.len();
                // SAFETY: came from allocate_slice with this exact layout.
                unsafe { Alloc::deallocate_aligned(slice.as_ptr().cast::<u8>(), layout) };

                assert_eq!(
                    reported, size,
                    "size {size} align {align}: allocate_slice must report the                      requested size, or the fitting range widens"
                );

                // Walk every size in min..=max that could change the answer.
                let mut probe = size;
                while let Ok(other) = Layout::from_size_align(probe, align) {
                    assert_eq!(
                        !Alloc::is_kernel_aligned(other),
                        stashed,
                        "size {size} align {align}: freeing with size {probe}                          (legal, max is {reported}) flips the stash decision"
                    );
                    if probe >= reported {
                        break;
                    }
                    probe = ((probe / page) + 1) * page;
                    if probe > reported {
                        probe = reported;
                    }
                }
            }
        }
    }

    /// Dirties a block, releases it, then takes the same shape again and
    /// checks every byte reads zero. Recycling is what makes this meaningful:
    /// a fresh block from the OS is zero anyway, so only a reused one proves
    /// the allocator itself zeroes.
    #[test]
    fn allocations_come_back_zeroed() {
        type Alloc = PagedAlloc<DEFAULT_MEMORY_TAG, true>;
        const { assert!(Alloc::POOL_ZEROES, "the default aliases must zero") };

        for &(size, align) in &[(256usize, 16usize), (256, 64), (100, 4096), (4096, 4096)] {
            let layout = Layout::from_size_align(size, align).unwrap();

            let first = Alloc::allocate_aligned(layout);
            assert!(!first.is_null());
            // SAFETY: `size` bytes from `first` are ours.
            unsafe { first.write_bytes(0xAB, size) };
            // SAFETY: `first` came from allocate_aligned with this layout.
            unsafe { Alloc::deallocate_aligned(first, layout) };

            let second = Alloc::allocate_aligned(layout);
            assert!(!second.is_null());
            // SAFETY: `size` bytes from `second` are ours.
            let dirty = (0..size)
                .filter(|i| unsafe { *second.add(*i) } != 0)
                .count();
            // SAFETY: `second` came from allocate_aligned with this layout.
            unsafe { Alloc::deallocate_aligned(second, layout) };

            assert_eq!(
                dirty, 0,
                "size {size} align {align}: {dirty} bytes were not zeroed"
            );
        }
    }

    /// The stashed pointer lives in the bytes *before* the returned address,
    /// so it must not appear inside the region handed to the caller. This is
    /// the case where skipping the second zeroing pass could plausibly leak
    /// something.
    #[test]
    fn realignment_stash_stays_out_of_the_zeroed_region() {
        type Alloc = PagedAlloc<DEFAULT_MEMORY_TAG, true>;

        for &(size, align) in &[(64usize, 64usize), (100, 128), (16, 4096)] {
            let layout = Layout::from_size_align(size, align).unwrap();
            assert!(!Alloc::is_kernel_aligned(layout), "case must realign");

            let ptr = Alloc::allocate_aligned(layout);
            assert!(!ptr.is_null());

            let original = Alloc::original_pointer(ptr);
            assert!(original < ptr, "the stash must precede the region");

            // SAFETY: `size` bytes from `ptr` are ours.
            let dirty = (0..size).filter(|i| unsafe { *ptr.add(*i) } != 0).count();
            // SAFETY: `ptr` came from allocate_aligned with this layout.
            unsafe { Alloc::deallocate_aligned(ptr, layout) };

            assert_eq!(dirty, 0, "size {size} align {align}: region was not zeroed");
        }
    }

    /// Both zeroing entry points must honour their guarantee, whichever
    /// branch of [`NtAlloc::POOL_ZEROES`] is compiled in.
    #[test]
    fn zeroing_entry_points_honour_their_guarantee() {
        type Alloc = PagedAlloc<DEFAULT_MEMORY_TAG, true>;
        let allocator = Alloc::default();

        for &(size, align) in &[(256usize, 16usize), (256, 64), (4096, 4096)] {
            let layout = Layout::from_size_align(size, align).unwrap();

            // GlobalAlloc::alloc_zeroed
            // SAFETY: layout has non-zero size, and the same layout is used to free.
            let ptr = unsafe { allocator.alloc_zeroed(layout) };
            assert!(!ptr.is_null());
            // SAFETY: `size` bytes from `ptr` are ours.
            let dirty = (0..size).filter(|i| unsafe { *ptr.add(*i) } != 0).count();
            // SAFETY: `ptr` came from alloc_zeroed with this layout.
            unsafe { allocator.dealloc(ptr, layout) };
            assert_eq!(dirty, 0, "alloc_zeroed left {dirty} non-zero bytes");

            // Allocator::allocate_zeroed
            let slice = allocator.allocate_zeroed(layout).expect("allocate_zeroed");
            assert_eq!(slice.len(), size);
            let base = slice.as_ptr().cast::<u8>();
            // SAFETY: `size` bytes from `base` are ours.
            let dirty = (0..size).filter(|i| unsafe { *base.add(*i) } != 0).count();
            // SAFETY: came from allocate_zeroed with this layout.
            unsafe { allocator.deallocate(slice.cast(), layout) };
            assert_eq!(dirty, 0, "allocate_zeroed left {dirty} non-zero bytes");
        }
    }

    /// Exercises the other branch of [`NtAlloc::POOL_ZEROES`]. A configuration
    /// carrying `POOL_FLAG_UNINITIALIZED` gets no zeroing from the pool, so
    /// the zeroing entry points have to do it themselves — this is the branch
    /// that keeps the override from becoming a soundness hole, and no other
    /// test reaches it.
    #[test]
    fn an_uninitialized_pool_still_zeroes_where_it_promises_to() {
        type Uninit =
            NtAlloc<{ POOL_FLAG_NON_PAGED | POOL_FLAG_UNINITIALIZED }, DEFAULT_MEMORY_TAG, true>;
        const {
            assert!(
                !Uninit::POOL_ZEROES,
                "this configuration opts out of zeroing"
            )
        };

        let allocator = Uninit::default();
        let size = 512usize;
        let layout = Layout::from_size_align(size, 64).unwrap();

        // Dirty and release, so the next request is served from recycled,
        // non-zero memory rather than a fresh page.
        let dirtied = Uninit::allocate_aligned(layout);
        assert!(!dirtied.is_null());
        // SAFETY: `size` bytes from `dirtied` are ours.
        unsafe { dirtied.write_bytes(0xAB, size) };
        // SAFETY: `dirtied` came from allocate_aligned with this layout.
        unsafe { Uninit::deallocate_aligned(dirtied, layout) };

        // SAFETY: layout has non-zero size and the same one frees it.
        let ptr = unsafe { allocator.alloc_zeroed(layout) };
        assert!(!ptr.is_null());
        // SAFETY: `size` bytes from `ptr` are ours.
        let dirty = (0..size).filter(|i| unsafe { *ptr.add(*i) } != 0).count();
        // SAFETY: `ptr` came from alloc_zeroed with this layout.
        unsafe { allocator.dealloc(ptr, layout) };
        assert_eq!(dirty, 0, "alloc_zeroed left {dirty} bytes unzeroed");

        // Dirty again: allocate_zeroed must be shown to zero on its own, not
        // to inherit a block the previous assertion already cleaned.
        let dirtied = Uninit::allocate_aligned(layout);
        assert!(!dirtied.is_null());
        // SAFETY: `size` bytes from `dirtied` are ours.
        unsafe { dirtied.write_bytes(0xCD, size) };
        // SAFETY: `dirtied` came from allocate_aligned with this layout.
        unsafe { Uninit::deallocate_aligned(dirtied, layout) };

        let slice = allocator.allocate_zeroed(layout).expect("allocate_zeroed");
        let base = slice.as_ptr().cast::<u8>();
        // SAFETY: `size` bytes from `base` are ours.
        let dirty = (0..size).filter(|i| unsafe { *base.add(*i) } != 0).count();
        // SAFETY: came from allocate_zeroed with this layout.
        unsafe { allocator.deallocate(slice.cast(), layout) };
        assert_eq!(dirty, 0, "allocate_zeroed left {dirty} bytes unzeroed");
    }

    /// Covers the zero-size corner of both zeroing entry points, under both
    /// values of [`NtAlloc::POOL_ZEROES`].
    ///
    /// The interesting cell is [`Allocator::allocate_zeroed`] with a zero-size
    /// layout on a pool that does not zero: the block it gets back is the
    /// dangling, no-provenance pointer [`Allocator::allocate`] synthesises,
    /// and it then writes `0` bytes through it. That is sound — a
    /// no-provenance pointer may be used for zero-sized accesses when
    /// suitably aligned — but it is the one place the two features meet, so
    /// it is pinned here rather than left to inference.
    #[test]
    fn zero_size_meets_the_zeroing_paths() {
        type Zeroing = PagedAlloc<DEFAULT_MEMORY_TAG, true>;
        type NonZeroing =
            NtAlloc<{ POOL_FLAG_NON_PAGED | POOL_FLAG_UNINITIALIZED }, DEFAULT_MEMORY_TAG, true>;
        const { assert!(Zeroing::POOL_ZEROES) };
        const { assert!(!NonZeroing::POOL_ZEROES) };

        for align in [1usize, 8, MIN_ALIGNMENT, 64, PAGE_SIZE as usize] {
            let layout = Layout::from_size_align(0, align).unwrap();

            // GlobalAlloc::alloc_zeroed -- null for a zero-size layout, and no
            // write is attempted through it on either configuration.
            // SAFETY: a zero-size layout breaks GlobalAlloc's contract, but
            // this implementation defines it as null rather than reaching the
            // pool; nothing is dereferenced.
            let zeroing = unsafe { Zeroing::default().alloc_zeroed(layout) };
            // SAFETY: as above.
            let non_zeroing = unsafe { NonZeroing::default().alloc_zeroed(layout) };
            assert!(
                zeroing.is_null(),
                "align {align}: alloc_zeroed must be null"
            );
            assert!(
                non_zeroing.is_null(),
                "align {align}: alloc_zeroed must be null without pool zeroing"
            );

            // Allocator::allocate_zeroed -- succeeds with a dangling, aligned,
            // length-0 slice on both, and the non-zeroing one writes 0 bytes
            // through a no-provenance pointer on the way.
            let zeroing_slice = Zeroing::default()
                .allocate_zeroed(layout)
                .expect("zero-size allocate_zeroed on a zeroing pool");
            // This one writes 0 bytes through the no-provenance pointer.
            let non_zeroing_slice = NonZeroing::default()
                .allocate_zeroed(layout)
                .expect("zero-size allocate_zeroed on a non-zeroing pool");

            for (label, slice) in [
                ("zeroing", zeroing_slice),
                ("non-zeroing", non_zeroing_slice),
            ] {
                assert_eq!(slice.len(), 0, "align {align} ({label}): length must be 0");
                assert_eq!(
                    slice.as_ptr().cast::<u8>() as usize % align,
                    0,
                    "align {align} ({label}): pointer must still be aligned"
                );
            }

            // Freeing is a no-op for a zero-size layout on any configuration,
            // but each block goes back through the allocator that made it.
            // SAFETY: both came from allocate_zeroed with this exact layout.
            unsafe {
                Zeroing::deallocate_aligned(zeroing_slice.as_ptr().cast::<u8>(), layout);
                NonZeroing::deallocate_aligned(non_zeroing_slice.as_ptr().cast::<u8>(), layout);
            }
        }
    }
}
