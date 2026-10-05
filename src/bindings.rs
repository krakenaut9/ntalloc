//! Manual bindings for `ExAllocatePool2`'s pool flags.
//!
//! `windows-sys` doesn't expose `POOL_FLAG_NON_PAGED`/`POOL_FLAG_PAGED`, so
//! the values below are hand-transcribed from `wdm.h` instead.

/// Type of the pool-selection flags passed to `ExAllocatePool2`.
pub type POOL_FLAGS = u64;

/// Skip the zero-initialization `ExAllocatePool2` otherwise performs (`0x2`).
///
/// Per the WDK, kernel-mode drivers should not opt out of zeroing for memory
/// that will be copied anywhere untrusted — user mode, the network — since
/// whatever the pool block last held would go with it.
pub const POOL_FLAG_UNINITIALIZED: POOL_FLAGS = 2;

/// Raise an exception instead of returning null when the allocation fails
/// (`0x20`).
///
/// Bound only so that `NtAlloc` can refuse it: the exception is SEH, which
/// Rust cannot catch.
pub const POOL_FLAG_RAISE_ON_FAILURE: POOL_FLAGS = 32;

/// Allocate from non-paged pool (`0x40`).
pub const POOL_FLAG_NON_PAGED: POOL_FLAGS = 64;
/// Allocate from paged pool (`0x100`).
pub const POOL_FLAG_PAGED: POOL_FLAGS = 256;
