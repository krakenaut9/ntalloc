//! Windows kernel-mode pool allocator for `#![no_std]` Rust drivers.
//!
//! `ntalloc` wraps the WDK pool APIs (`ExAllocatePool2`, `ExFreePool`,
//! `ExFreePoolWithTag`) behind the standard [`GlobalAlloc`] trait and, with
//! the `allocator-api` feature, the unstable [`Allocator`] trait, so ordinary
//! `alloc`-crate types (`Vec`, `Box`, ...) can be backed by kernel pool
//! memory from a `#![no_std]` driver. It builds on stable Rust 1.85 or
//! newer; only `allocator-api` and `allocator-api-markers` need nightly.
//!
//! [`Allocator`]: core::alloc::Allocator
//!
//! # Overview
//!
//! - [`NtAlloc`] is a zero-sized allocator type parameterized over the pool
//!   flags, the memory tag, and whether deallocation uses the tagged or
//!   untagged free routine. The pool flags are exported as `POOL_FLAG_*`
//!   constants, such as [`POOL_FLAG_NON_PAGED`] and
//!   [`POOL_FLAG_UNINITIALIZED`].
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
//!
//! # Features
//!
//! - **`allocator-api`** — implements the unstable `Allocator` trait for
//!   [`NtAlloc`], so a collection can use it directly, as in `Vec::new_in`
//!   and `Box::new_in`, not only as the `#[global_allocator]`. It needs a
//!   nightly compiler, as does any code that uses the trait.
//! - **`allocator-api-markers`** — implements the `StaticAllocator` and
//!   `AllocatorClone` marker traits for [`NtAlloc`]. `alloc` requires them in
//!   places: `Box::pin_in` needs `StaticAllocator`, and cloning an `Arc` or
//!   `Rc`, or using a `BTreeMap` at all, needs `AllocatorClone`. It implies
//!   `allocator-api`. Off by default, because on a nightly that predates
//!   these traits, enabling it fails to compile.
//! - **`usermode-backend`** — for tests only. Allocates from the process heap
//!   (`std::alloc::System`) instead of the kernel pool, so code that allocates
//!   through [`NtAlloc`] can run in tests on a normal machine. Where the pool
//!   would bugcheck with `BAD_POOL_CALLER`, the test process prints what went
//!   wrong and aborts instead: on a double free, on a pointer it never handed
//!   out, and, with `TAGGED_DEALLOC`, on the wrong tag. On Windows any abort
//!   is reported as `STATUS_STACK_BUFFER_OVERRUN`, whatever its cause, and
//!   running the tests with `-- --test-threads=1` shows which test it was.
//!   It brings in `std`, along with the `hashbrown` and `allocator-api2`
//!   crates, so enable it only under `[dev-dependencies]`: through a normal
//!   dependency it reaches the driver itself, which then fails to build with
//!   a duplicate `panic_impl` lang item, as `std`'s panic handler clashes with
//!   the driver's own. [`NtAlloc::kernel_allocate`] always uses the pool,
//!   with or without it.
//!
//!   ```toml
//!   [dev-dependencies]
//!   ntalloc = { version = "0.1", features = ["usermode-backend"] }
//!   ```

#![no_std]
#![cfg_attr(feature = "allocator-api", feature(allocator_api))]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]
// Doctests are this crate's documentation, so let them fail on unused imports
// rather than accumulating stale `use` lines. Scoped to `unused` rather than
// all warnings: this only ever runs for our own doctests, but a blanket deny
// would also break on an unrelated future lint in a dependency's API.
#![doc(test(attr(deny(unused))))]

#[cfg(any(test, feature = "usermode-backend"))]
extern crate std;

mod bindings;
#[cfg(any(test, feature = "usermode-backend"))]
mod usermode;

use crate::bindings::POOL_FLAG_RAISE_ON_FAILURE;
pub use crate::bindings::{
    POOL_FLAG_CACHE_ALIGNED, POOL_FLAG_NON_PAGED, POOL_FLAG_PAGED, POOL_FLAG_SPECIAL_POOL,
    POOL_FLAG_UNINITIALIZED, POOL_FLAG_USE_QUOTA, POOL_FLAGS,
};
#[cfg(feature = "allocator-api")]
use core::alloc::{AllocError, Allocator};
#[cfg(feature = "allocator-api-markers")]
use core::alloc::{AllocatorClone, StaticAllocator};
#[cfg(feature = "allocator-api")]
use core::ptr::{NonNull, slice_from_raw_parts_mut, without_provenance_mut};
use core::{
    alloc::{GlobalAlloc, Layout},
    num::NonZeroUsize,
    ptr::null_mut,
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
/// the [`GlobalAlloc`] trait bound (and, with the `allocator-api` feature,
/// the [`Allocator`] one), and can always be created with
/// [`Default::default()`] at zero runtime cost.
///
/// [`Allocator`]: core::alloc::Allocator
///
/// # Type Parameters
/// - `ALLOC_POOL_FLAGS`: the pool flags passed to `ExAllocatePool2`.
///   [`POOL_FLAG_PAGED`] or [`POOL_FLAG_NON_PAGED`] selects the pool, and
///   [`POOL_FLAG_UNINITIALIZED`], [`POOL_FLAG_CACHE_ALIGNED`],
///   [`POOL_FLAG_USE_QUOTA`] or [`POOL_FLAG_SPECIAL_POOL`] can be added to
///   it, as in the example below. Flags from `wdm.h` that this crate does not
///   export can be passed as raw values. `POOL_FLAG_RAISE_ON_FAILURE` is
///   refused; see [below](#unsupported-configurations).
/// - `MEMORY_TAG`: the 4-byte pool tag attached to every allocation, used by
///   tools such as `!poolused`/`!poolfind` to attribute pool memory back to
///   this allocator. Defaults to [`DEFAULT_MEMORY_TAG`]. The WDK asks for one
///   to four printable ASCII characters (`0x20` to `0x7E`); only a zero tag
///   is refused.
/// - `TAGGED_DEALLOC`: selects the free routine used by [`NtAlloc::deallocate`]:
///   `true` uses `ExFreePoolWithTag` (the tag must match the one the memory
///   was allocated with), `false` uses the untagged `ExFreePool`. Defaults to
///   `true`.
///
/// Prefer the [`PagedAlloc`]/[`NonPagedAlloc`] aliases over naming `NtAlloc`
/// directly, unless the configuration needs more flags than the pool:
///
/// ```
/// use ntalloc::{NtAlloc, POOL_FLAG_NON_PAGED, POOL_FLAG_UNINITIALIZED};
///
/// // Non-paged pool that skips the zeroing ExAllocatePool2 otherwise does.
/// type ScratchAlloc = NtAlloc<{ POOL_FLAG_NON_PAGED | POOL_FLAG_UNINITIALIZED }>;
///
/// assert!(!ScratchAlloc::POOL_ZEROES);
/// ```
///
/// # Unsupported configurations
///
/// These configurations would either fail every allocation or fail in a way
/// Rust cannot handle, so a type that uses one fails to build as soon as
/// anything allocates with it. The error comes from `cargo build`;
/// `cargo check` does not evaluate it.
///
/// - `POOL_FLAG_RAISE_ON_FAILURE` (`0x20`) makes `ExAllocatePool2` raise an
///   SEH exception on failure instead of returning null. Rust cannot catch
///   SEH: left unhandled the exception bugchecks the system, and handled
///   further up it unwinds through Rust frames, which is undefined behaviour.
/// - A zero `MEMORY_TAG`, which `ExAllocatePool2` rejects as invalid.
///
/// Without `POOL_FLAG_RAISE_ON_FAILURE`, failure is reported as null.
/// Fallible APIs such as `Box::try_new_in` return it as an error, and
/// infallible ones such as `Box::new` pass it to `handle_alloc_error`, which
/// in a `#![no_std]` build panics by default: the fail-fast behaviour the flag
/// would otherwise give.
///
/// This is not a full validation of the parameters: any other combination
/// the pool considers invalid fails at run time, like any other allocation
/// failure.
///
/// ```compile_fail,E0080
/// use ntalloc::{NtAlloc, POOL_FLAG_NON_PAGED};
///
/// // 0x20 is POOL_FLAG_RAISE_ON_FAILURE, which this crate does not export.
/// type Raising = NtAlloc<{ POOL_FLAG_NON_PAGED | 0x20 }>;
///
/// let _ = Raising::allocate(64);
/// ```
///
/// ```compile_fail,E0080
/// use ntalloc::{NtAlloc, POOL_FLAG_NON_PAGED};
///
/// type Untagged = NtAlloc<POOL_FLAG_NON_PAGED, 0>;
///
/// let _ = Untagged::allocate(64);
/// ```
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NtAlloc<
    const ALLOC_POOL_FLAGS: POOL_FLAGS,
    const MEMORY_TAG: u32 = DEFAULT_MEMORY_TAG,
    const TAGGED_DEALLOC: bool = true,
>;

/// [`NtAlloc`] backed by the paged pool ([`POOL_FLAG_PAGED`]).
///
/// Memory from the paged pool may be paged out to disk, so it may only be
/// touched at IRQL `APC_LEVEL` or below — that is, strictly below
/// `DISPATCH_LEVEL`. Never touch it from a DPC, from an ISR, or while
/// holding a spin lock.
pub type PagedAlloc<const MEMORY_TAG: u32 = DEFAULT_MEMORY_TAG, const TAGGED_DEALLOC: bool = true> =
    NtAlloc<POOL_FLAG_PAGED, MEMORY_TAG, TAGGED_DEALLOC>;
/// [`NtAlloc`] backed by the non-paged pool ([`POOL_FLAG_NON_PAGED`]).
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
    /// [`POOL_FLAG_UNINITIALIZED`] is among `ALLOC_POOL_FLAGS`, so this is a
    /// property of the type, resolved at compile time. It is `true` for
    /// [`PagedAlloc`] and [`NonPagedAlloc`].
    ///
    /// [`GlobalAlloc::alloc_zeroed`], and with the `allocator-api` feature
    /// [`Allocator::allocate_zeroed`], consult it to skip the redundant second
    /// pass their default implementations would otherwise make over memory the
    /// pool has already zeroed.
    ///
    /// [`Allocator::allocate_zeroed`]: core::alloc::Allocator::allocate_zeroed
    pub const POOL_ZEROES: bool = (ALLOC_POOL_FLAGS & POOL_FLAG_UNINITIALIZED) == 0;

    /// Fails the build for a configuration [`NtAlloc`] refuses: one whose
    /// `ALLOC_POOL_FLAGS` include `POOL_FLAG_RAISE_ON_FAILURE`, or whose
    /// `MEMORY_TAG` is zero.
    ///
    /// With `POOL_FLAG_RAISE_ON_FAILURE`, `ExAllocatePool2` raises an SEH
    /// exception on failure instead of returning null. Unhandled, the
    /// exception bugchecks the system; handled further up, it unwinds through
    /// Rust frames that call `ExAllocatePool2` through a non-unwinding ABI,
    /// which is undefined behaviour. Either way the null that [`GlobalAlloc`]
    /// and [`Allocator`] report failure with never arrives. A zero tag fails
    /// every allocation, since `ExAllocatePool2` rejects it as invalid.
    ///
    /// [`Allocator`]: core::alloc::Allocator
    ///
    /// Both allocation backends evaluate this before anything else, so the
    /// error appears wherever such a type allocates. A constant that depends
    /// on the type's parameters is only evaluated when code is generated for
    /// it, which is why `cargo build` reports it and `cargo check` does not.
    const CONFIGURATION_SUPPORTED: () = {
        assert!(
            (ALLOC_POOL_FLAGS & POOL_FLAG_RAISE_ON_FAILURE) == 0,
            "POOL_FLAG_RAISE_ON_FAILURE is not supported: ExAllocatePool2 would \
             raise an SEH exception on failure, which Rust cannot catch"
        );
        assert!(
            MEMORY_TAG != 0,
            "MEMORY_TAG is zero: ExAllocatePool2 fails every allocation with a \
             zero tag"
        );
    };

    /// Returns the address, one pointer-width before `ptr`, where the
    /// original (pre-alignment) pointer is stored by
    /// [`NtAlloc::save_original_pointer`].
    ///
    /// `ptr` must be a pointer returned by [`NtAlloc::allocate_aligned`] for
    /// a layout that required realignment; the slot this names is only
    /// guaranteed to exist, and to belong to the caller, for such a pointer.
    /// That restriction is why this is not part of the public API: nothing
    /// in the signature can enforce it.
    #[inline]
    #[expect(
        clippy::cast_ptr_alignment,
        reason = "a realigned address is aligned to more than MIN_ALIGNMENT"
    )]
    fn original_pointer_location(ptr: *mut u8) -> *mut *mut u8 {
        // SAFETY: `ptr` is an address allocate_aligned realigned, which lies at
        // least MIN_ALIGNMENT bytes into the same pool block, so one pointer
        // back is still inside it.
        unsafe { ptr.cast::<*mut u8>().sub(1) }
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
    #[inline]
    fn save_original_pointer(ptr: *mut u8, original_ptr: *mut u8) {
        let original_ptr_location = Self::original_pointer_location(ptr);
        // SAFETY: the slot lies in the gap allocate_aligned opened before `ptr`,
        // inside the pool block and outside the region handed out, and is
        // pointer-aligned because `ptr` is aligned to more than MIN_ALIGNMENT.
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
    #[inline]
    fn original_pointer(ptr: *mut u8) -> *mut u8 {
        let original_ptr_location = Self::original_pointer_location(ptr);
        // SAFETY: this is the slot save_original_pointer wrote when
        // allocate_aligned handed `ptr` out; nothing else writes to it.
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
    #[must_use]
    #[inline]
    pub fn kernel_alignment(size: usize) -> usize {
        if size >= PAGE_SIZE as usize {
            PAGE_SIZE as usize
        } else {
            MIN_ALIGNMENT
        }
    }

    /// Allocates `size` bytes directly from the kernel pool via
    /// `ExAllocatePool2`, bypassing the process-heap backend that
    /// [`NtAlloc::allocate`] uses in tests and under the `usermode-backend`
    /// feature. It always reaches the pool, so outside a kernel it can only
    /// be called with a `size` of `0`.
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
    #[must_use = "discarding the pointer leaks the allocation"]
    #[inline]
    pub fn kernel_allocate(size: usize) -> *mut u8 {
        // Fails the build, not anything at runtime, for a configuration this
        // crate refuses; see CONFIGURATION_SUPPORTED.
        let () = Self::CONFIGURATION_SUPPORTED;

        if size == 0 {
            return null_mut();
        }

        // SAFETY: ExAllocatePool2 takes no pointers: only the flags and tag,
        // which CONFIGURATION_SUPPORTED has checked, and a non-zero size. The
        // paged pool's IRQL limit is documented on PagedAlloc.
        unsafe { ExAllocatePool2(ALLOC_POOL_FLAGS, size, MEMORY_TAG) }.cast()
    }

    /// Allocates `size` bytes, returning a null pointer for a zero-size
    /// request.
    ///
    /// With the `usermode-backend` feature, and in this crate's own tests,
    /// allocation is redirected to the process heap so that the allocator's
    /// logic can run without a kernel; otherwise this calls
    /// [`NtAlloc::kernel_allocate`].
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
    #[must_use = "discarding the pointer leaks the allocation"]
    pub fn allocate(size: usize) -> *mut u8 {
        #[cfg(any(test, feature = "usermode-backend"))]
        {
            Self::usermode_allocate(size)
        }

        #[cfg(not(any(test, feature = "usermode-backend")))]
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
    ///
    /// [`Allocator::deallocate`]: core::alloc::Allocator::deallocate
    #[inline]
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
    #[inline]
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
    /// allocation want the [`Allocator`] impl (the `allocator-api` feature),
    /// which returns a dangling one as that trait requires.
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
    /// [`Allocator`]: core::alloc::Allocator
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
    #[must_use = "discarding the pointer leaks the allocation"]
    pub fn allocate_aligned(layout: Layout) -> *mut u8 {
        if layout.size() == 0 {
            return null_mut();
        }

        if Self::is_kernel_aligned(layout) {
            Self::allocate(layout.size())
        } else {
            // Cannot overflow: Layout guarantees size rounded up to align is
            // at most isize::MAX, so size + align is at most isize::MAX + 1.
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
    #[cfg(feature = "allocator-api")]
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

            // SAFETY: allocated_raw was checked non-null above, and the slice
            // pointer keeps its address.
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
    // The heap backend frees through usermode_free instead; these stay
    // compiled, and checked, in every configuration.
    #[cfg_attr(any(test, feature = "usermode-backend"), allow(dead_code))]
    fn deallocate_tagged(ptr: *mut u8) {
        // SAFETY: the only caller, deallocate, passes on its own contract: a
        // non-null pool block of this configuration, allocated with this
        // MEMORY_TAG and not yet freed.
        unsafe {
            ExFreePoolWithTag(ptr.cast(), MEMORY_TAG);
        }
    }

    /// Frees `ptr` with the untagged `ExFreePool`.
    ///
    /// `ptr` must be non-null (unlike C's `free`, passing null here is not a
    /// safe no-op) and must have been allocated from the same pool as this
    /// allocator targets.
    #[cfg_attr(any(test, feature = "usermode-backend"), allow(dead_code))]
    fn deallocate_non_tagged(ptr: *mut u8) {
        // SAFETY: the only caller, deallocate, passes on its own contract: a
        // non-null pool block of this configuration, not yet freed.
        unsafe {
            ExFreePool(ptr.cast());
        }
    }

    /// Frees a pointer previously returned by [`NtAlloc::allocate`]. Normally
    /// that is a pool block, which [`NtAlloc::kernel_allocate`] returns too,
    /// freed with the routine `TAGGED_DEALLOC` selects. Under the
    /// `usermode-backend` feature both functions use the process heap instead.
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
        #[cfg(any(test, feature = "usermode-backend"))]
        {
            Self::usermode_free(ptr);
        }

        #[cfg(not(any(test, feature = "usermode-backend")))]
        {
            if TAGGED_DEALLOC {
                Self::deallocate_tagged(ptr);
            } else {
                Self::deallocate_non_tagged(ptr);
            }
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

        // SAFETY: original_ptr is the address `allocate` returned — either
        // `ptr` itself when no realignment happened, or the value stashed by
        // allocate_aligned — and this fn's own contract covers the rest.
        unsafe { Self::deallocate(original_ptr) };
    }
}

// SAFETY: [`GlobalAlloc`] asks for blocks that fit the layout and stay valid
// until freed, and for no method to unwind. For [`NtAlloc`]:
//
// - `alloc` returns null or `layout.size()` bytes aligned to `layout.align()`:
//   the pool's own block when its alignment suffices, otherwise an address
//   realigned inside a block over-allocated by `layout.align()`;
// - a block belongs to the pool, not to the allocator value, so it stays valid
//   until `dealloc` hands it back, recovering the pool's own address from the
//   same layout;
// - `alloc_zeroed` returns zeroed memory: the pool zeroes it unless
//   [`POOL_FLAG_UNINITIALIZED`] is set, and then it writes the zeroes itself;
// - in a release build nothing on the pool path panics; its `debug_assert!`s
//   fire only on a bug here or on a caller that broke the contract, and the
//   heap backend aborts rather than unwinds on misuse.
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

// SAFETY: [`Allocator`] asks that a block stay valid until it is deallocated,
// however the allocator value is copied, moved or dropped, and that a copy
// behave like the original. For [`NtAlloc`]:
//
// - blocks come from the pool (or the heap backend), never from the value,
//   which is a zero-sized `Copy` type that owns nothing, so no copy, move or
//   drop of it can invalidate one;
// - every value of one type makes the same pool calls, so any of them can free
//   what another allocated;
// - `allocate` reports exactly `layout.size()` bytes, so the layout the free
//   path receives is the one the stash decision was made with;
// - a zero-size request gets a dangling, aligned pointer, which `deallocate`
//   recognizes and never frees.
#[cfg(feature = "allocator-api")]
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
                    .write_bytes(0, allocated.len());
            }
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
#[cfg(feature = "allocator-api-markers")]
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
#[cfg(feature = "allocator-api-markers")]
unsafe impl<const ALLOC_POOL_FLAGS: POOL_FLAGS, const MEMORY_TAG: u32, const TAGGED_DEALLOC: bool>
    AllocatorClone for NtAlloc<ALLOC_POOL_FLAGS, MEMORY_TAG, TAGGED_DEALLOC>
{
}

#[cfg(test)]
mod tests;
