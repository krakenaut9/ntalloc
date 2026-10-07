//! The `POOL_FLAG_*` values that `ExAllocatePool2` takes, transcribed from
//! `wdm.h` (WDK 10.0.28000.0), since `windows-sys` does not bind them.
//!
//! Only the flags an allocator for Rust data has a use for are bound, and the
//! crate root re-exports them. `POOL_FLAG_SESSION` is reserved for the
//! operating system; `POOL_FLAG_NON_PAGED_EXECUTE` allocates executable
//! memory, which drivers built for memory integrity (HVCI) must avoid;
//! `POOL_FLAG_TAGGED_VA` is undocumented, and as a required flag it makes the
//! allocation fail on Windows versions that do not know it; the rest are
//! reserved. `NtAlloc` does not refuse any of them, so a driver that needs one
//! can pass its raw value. `POOL_FLAG_RAISE_ON_FAILURE` is bound but not
//! exported, so that `NtAlloc` can refuse it.

/// The pool flags passed to `ExAllocatePool2`: a combination of `POOL_FLAG_*`
/// values, like the `POOL_FLAGS` type in `wdm.h`.
///
/// The low 32 bits are required flags: if the pool does not recognise one, or
/// cannot satisfy them all, the allocation fails. The high 32 bits are
/// optional flags, which the pool satisfies when it can and ignores when it
/// does not recognise them.
#[allow(non_camel_case_types, reason = "named as in wdm.h")]
pub type POOL_FLAGS = u64;

/// Charge the allocation to the pool quota of the process the allocating
/// thread runs in (`0x1`).
///
/// The WDK describes it for highest-level drivers that allocate memory to
/// satisfy an I/O request in the context of the process that made it;
/// lower-level drivers need not specify it. A `#[global_allocator]` runs in
/// whatever process context the allocating thread happens to be in, so it
/// rarely belongs there.
pub const POOL_FLAG_USE_QUOTA: POOL_FLAGS = 0x1;

/// Leave the allocation uninitialized rather than zeroing it (`0x2`).
///
/// `ExAllocatePool2` zeroes every allocation unless this is set. The WDK
/// warns never to copy uninitialized memory anywhere untrusted, such as user
/// mode or the network, since whatever the pool block last held would go with
/// it. With this flag [`NtAlloc::POOL_ZEROES`] is `false`, and the entry points
/// that promise zeroed memory, such as [`GlobalAlloc::alloc_zeroed`], zero it
/// themselves.
///
/// [`NtAlloc::POOL_ZEROES`]: crate::NtAlloc::POOL_ZEROES
/// [`GlobalAlloc::alloc_zeroed`]: core::alloc::GlobalAlloc::alloc_zeroed
pub const POOL_FLAG_UNINITIALIZED: POOL_FLAGS = 0x2;

/// Ask for a cache-aligned allocation (`0x8`).
///
/// The WDK treats this as best effort, not to be relied on where cache
/// alignment is needed for correctness. `NtAlloc` does not rely on it: a
/// `Layout` that needs more alignment than the pool guarantees is realigned
/// by [`NtAlloc::allocate_aligned`] either way.
///
/// [`NtAlloc::allocate_aligned`]: crate::NtAlloc::allocate_aligned
pub const POOL_FLAG_CACHE_ALIGNED: POOL_FLAGS = 0x8;

/// Raise an exception instead of returning null when the allocation fails
/// (`0x20`).
///
/// Bound only so that `NtAlloc` can refuse it: the exception is SEH, which
/// Rust cannot catch. Not exported.
pub const POOL_FLAG_RAISE_ON_FAILURE: POOL_FLAGS = 0x20;

/// Allocate from non-paged pool, as non-executable memory (`0x40`).
///
/// Non-paged memory is always resident, so it may be touched at any IRQL.
/// Allocating it requires IRQL `DISPATCH_LEVEL` or below. [`NonPagedAlloc`]
/// uses this pool.
///
/// [`NonPagedAlloc`]: crate::NonPagedAlloc
pub const POOL_FLAG_NON_PAGED: POOL_FLAGS = 0x40;

/// Allocate from paged pool (`0x100`).
///
/// Paged memory may be paged out, so it may only be allocated and touched at
/// IRQL `APC_LEVEL` or below; memory that code at `DISPATCH_LEVEL` will access
/// must come from non-paged pool instead. It is non-executable except on x86.
/// [`PagedAlloc`] uses this pool.
///
/// [`PagedAlloc`]: crate::PagedAlloc
pub const POOL_FLAG_PAGED: POOL_FLAGS = 0x100;

/// Allocate from special pool, the pool used for debugging, when it is
/// available (`0x1_0000_0000`).
///
/// This is an optional flag: if special pool cannot be used, the allocator
/// tries normal pool instead.
pub const POOL_FLAG_SPECIAL_POOL: POOL_FLAGS = 0x1_0000_0000;
