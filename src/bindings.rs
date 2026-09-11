//! Manual bindings for `ExAllocatePool2`'s pool flags.
//!
//! `windows-sys` doesn't expose `POOL_FLAG_NON_PAGED`/`POOL_FLAG_PAGED`, so
//! the values below are hand-transcribed from `wdm.h` instead.

/// Type of the pool-selection flags passed to `ExAllocatePool2`.
pub type POOL_FLAGS = u64;

/// Allocate from non-paged pool (`0x40`).
pub const POOL_FLAG_NON_PAGED: POOL_FLAGS = 64;
/// Allocate from paged pool (`0x100`).
pub const POOL_FLAG_PAGED: POOL_FLAGS = 256;
