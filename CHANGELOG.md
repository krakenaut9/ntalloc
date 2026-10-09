# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

While the version is below 1.0, a minor bump may contain breaking changes.

## [Unreleased]

First public release.

### Added

- `NtAlloc<ALLOC_POOL_FLAGS, MEMORY_TAG, TAGGED_DEALLOC>`, a zero-sized
  allocator over `ExAllocatePool2` configured entirely at compile time: the
  pool flags, the pool tag, and whether freeing goes through
  `ExFreePoolWithTag` or `ExFreePool`.
- `PagedAlloc` and `NonPagedAlloc`, aliases for the two pools that default the
  tag to `DEFAULT_MEMORY_TAG` (shown as `Rust` by `!poolused`) and free with
  the tag checked.
- A `GlobalAlloc` implementation, so either alias can be a driver's
  `#[global_allocator]`. `alloc_zeroed` skips the second zeroing pass when the
  pool has already zeroed the block, which `POOL_ZEROES` reports.
- Any alignment a `Layout` asks for, including alignments larger than a page.
  `kernel_alignment` gives what the pool guarantees for a size; anything more
  is served by over-allocating and storing the pool's pointer just before the
  aligned one. `MIN_ALIGNMENT` and `POINTER_SIZE` describe that layout.
- `allocate`, `allocate_aligned`, `deallocate`, `deallocate_aligned` and
  `kernel_allocate`, the same operations as associated functions.
- `POOL_FLAGS` and the flags an allocator for Rust data has a use for:
  `POOL_FLAG_PAGED`, `POOL_FLAG_NON_PAGED`, `POOL_FLAG_UNINITIALIZED`,
  `POOL_FLAG_CACHE_ALIGNED`, `POOL_FLAG_USE_QUOTA` and
  `POOL_FLAG_SPECIAL_POOL`. Other `wdm.h` flags can be passed as raw values.
- Configurations that cannot work fail the build: `POOL_FLAG_RAISE_ON_FAILURE`,
  whose SEH exception Rust cannot handle, and a pool tag that is zero, is not
  one to four printable ASCII characters, has no letter or digit, or is
  `"BIG "`.
- The `allocator-api` feature (nightly): an `Allocator` implementation, so
  collections can use the allocator directly with `Vec::new_in` or
  `Box::new_in`. Zero-sized requests succeed without reaching the pool.
- The `allocator-api-markers` feature (nightly): the `StaticAllocator` and
  `AllocatorClone` marker traits.
- The `usermode-backend` feature, for tests: allocations come from the process
  heap, so code that uses the allocator can run on a normal machine.

### Notes

- `#![no_std]` with no default features. Everything but the two `allocator-api`
  features builds on stable; the minimum supported Rust version is 1.85.
- Requires Windows 10, version 2004 or later, where `ExAllocatePool2` exists,
  and `panic = "abort"` in the driver's own profiles.

[Unreleased]: https://github.com/krakenaut9/ntalloc/commits/HEAD
