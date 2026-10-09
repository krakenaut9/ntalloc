# ntalloc

[![CI](https://github.com/krakenaut9/ntalloc/actions/workflows/ci.yml/badge.svg)](https://github.com/krakenaut9/ntalloc/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/ntalloc.svg)](https://crates.io/crates/ntalloc)
[![docs.rs](https://docs.rs/ntalloc/badge.svg)](https://docs.rs/ntalloc)
[![MSRV](https://img.shields.io/badge/rustc-1.85+-blue.svg)](https://blog.rust-lang.org/2025/02/20/Rust-1.85.0.html)

A `#[global_allocator]` for Windows kernel drivers written in `#![no_std]` Rust.
`ntalloc` serves the `alloc` crate (`Vec`, `Box`, `String` and the rest) from the
kernel pool through `ExAllocatePool2`, on stable Rust. With the nightly-only
`allocator-api` feature it also implements the `Allocator` trait, so a single
collection can use a pool of its own.

## Highlights

- Builds on **stable** Rust 1.85; nightly only for the opt-in `Allocator` API
- One zero-sized type, configured entirely at compile time: the pool flags, the
  pool tag, and whether freeing checks the tag
- Any alignment a `Layout` asks for, including `#[repr(align(64))]` types and
  alignments larger than a page
- `POOL_FLAG_RAISE_ON_FAILURE` and invalid pool tags are refused when the
  driver is built, not discovered when it runs
- Testable in user mode with the `usermode-backend` feature, and checked under
  Miri

## Requirements

- **Windows 10, version 2004 or later.** `ExAllocatePool2` does not exist on
  earlier versions.
- **Rust 1.85 or later.**
- **`panic = "abort"` in the driver's own profiles.** A kernel driver cannot
  unwind, and profile settings in a dependency never apply to the crate that
  uses it.

```toml
[profile.dev]
panic = "abort"

[profile.release]
panic = "abort"
```

## Quick start

```toml
[dependencies]
ntalloc = "0.1"
```

In the driver's `lib.rs`, next to `#![no_std]` and its panic handler:

```rust,no_run
extern crate alloc;

use alloc::vec::Vec;
use ntalloc::NonPagedAlloc;

#[global_allocator]
static GLOBAL: NonPagedAlloc = NonPagedAlloc {};

// Anywhere in the driver, the alloc types now draw on the non-paged pool.
let ids: Vec<u32> = (1..=4).collect();
assert_eq!(ids.len(), 4);
```

Every allocation the driver makes through `alloc` now comes from the non-paged
pool under the tag `Rust`, and goes back to it when freed.

## Choosing a pool

| Alias           | Pool flag             | Allocate at             | Touch the memory at |
| --------------- | --------------------- | ----------------------- | ------------------- |
| `NonPagedAlloc` | `POOL_FLAG_NON_PAGED` | IRQL ≤ `DISPATCH_LEVEL` | any IRQL            |
| `PagedAlloc`    | `POOL_FLAG_PAGED`     | IRQL ≤ `APC_LEVEL`      | IRQL ≤ `APC_LEVEL`  |

A global allocator serves every allocation in the driver, including those made
in a DPC or while a spin lock is held, so `NonPagedAlloc` is the safe choice for
`#[global_allocator]`. `PagedAlloc` suits code that never allocates or touches
the memory at `DISPATCH_LEVEL`, typically through the `Allocator` API below.

## When the pool runs out

`ExAllocatePool2` returns null when it cannot serve a request, and `ntalloc`
passes that on. Fallible APIs such as `Vec::try_reserve` report it as an error.
Infallible ones such as `Vec::push` and `Box::new` call `handle_alloc_error`,
which in a `#![no_std]` build panics and hands control to the driver's panic
handler. Where running out of pool must be survivable, reserve with the
fallible APIs first.

## Tags and flags

Both aliases are shorthand for
`NtAlloc<ALLOC_POOL_FLAGS, MEMORY_TAG, TAGGED_DEALLOC>`, which takes any
combination of pool flags:

```rust
use ntalloc::{NonPagedAlloc, NtAlloc, POOL_FLAG_NON_PAGED, POOL_FLAG_UNINITIALIZED};

// Shows up as "MyDr" in !poolused and !poolfind.
type DriverAlloc = NonPagedAlloc<{ u32::from_ne_bytes(*b"MyDr") }>;

// Non-paged pool without the zeroing ExAllocatePool2 does by default.
type ScratchAlloc = NtAlloc<{ POOL_FLAG_NON_PAGED | POOL_FLAG_UNINITIALIZED }>;

assert!(DriverAlloc::POOL_ZEROES);
assert!(!ScratchAlloc::POOL_ZEROES);
```

- `MEMORY_TAG` defaults to `DEFAULT_MEMORY_TAG`, which the debugger shows as
  `Rust`. Build your own with `u32::from_ne_bytes`, as above, and it appears
  exactly as written. A tag is one to four printable ASCII characters, padded
  with zero bytes at the end when shorter, and needs at least one letter or
  digit.
- `TAGGED_DEALLOC`, `true` by default, frees with `ExFreePoolWithTag`, which
  checks the tag; `false` frees with `ExFreePool`.
- `ExAllocatePool2` zeroes every block unless `POOL_FLAG_UNINITIALIZED` is set,
  and `POOL_ZEROES` says which applies. `alloc_zeroed` and `allocate_zeroed`
  return zeroed memory either way. Microsoft advises against skipping the
  zeroing for memory that may be copied to user mode or over the network.
- The flags exported with the type `POOL_FLAGS` are `POOL_FLAG_PAGED`,
  `POOL_FLAG_NON_PAGED`, `POOL_FLAG_UNINITIALIZED`, `POOL_FLAG_CACHE_ALIGNED`,
  `POOL_FLAG_USE_QUOTA` and `POOL_FLAG_SPECIAL_POOL`. Other flags from `wdm.h`
  can be passed as raw values, with the one exception below.
- Some configurations fail the build: `POOL_FLAG_RAISE_ON_FAILURE`, which
  would make a failed allocation raise an SEH exception Rust cannot handle; a
  zero tag, which makes every allocation fail; any other tag that breaks the
  rules above; and `"BIG "`, a tag the kernel reserves. The error, E0080, comes
  from `cargo build`; `cargo check` does not report it.

## Alignment

`ExAllocatePool2` aligns a block to `MIN_ALIGNMENT`, 16 bytes on 64-bit
Windows, or to the page size for a block of a page or more; `kernel_alignment`
gives the figure for a size. A `Layout` that asks for more is served by
allocating `layout.align()` extra bytes, rounding the pointer up, and storing
the pool's own pointer in the `POINTER_SIZE` bytes just before the one handed
out. `GlobalAlloc` and `Allocator` take care of both directions.

The same steps are available as functions. Memory from `allocate_aligned` must go
back through `deallocate_aligned` with the same `Layout`; `allocate` and
`deallocate` never realign, and pair only with each other:

```rust,no_run
use core::alloc::Layout;
use ntalloc::NonPagedAlloc;

// Call through an alias like this one: `NonPagedAlloc::allocate_aligned(..)`
// cannot infer the defaults of the alias's parameters (error E0284).
type Alloc = NonPagedAlloc;

let layout = Layout::from_size_align(256, 64).unwrap();
let ptr = Alloc::allocate_aligned(layout);
assert!(!ptr.is_null());

// SAFETY: `ptr` came from `allocate_aligned` with this layout, and is freed once.
unsafe { Alloc::deallocate_aligned(ptr, layout) };
```

`kernel_allocate` is `allocate` without the test backend: it always reaches the
pool, with or without `usermode-backend`.

## The `Allocator` API (nightly)

With the `allocator-api` feature, `NtAlloc` implements `core::alloc::Allocator`,
so one collection can use a different pool or tag from the global allocator:

```rust,ignore
#![feature(allocator_api)]

use ntalloc::PagedAlloc;

// A paged-pool buffer, for code that runs at PASSIVE_LEVEL.
let mut name: Vec<u16, PagedAlloc> = Vec::new_in(PagedAlloc::default());
name.extend("rule".encode_utf16());
```

`allocator-api-markers` adds the `StaticAllocator` and `AllocatorClone` marker
traits, which `Box::pin_in`, cloning an `Arc` or `Rc`, and `BTreeMap` require.
It needs a nightly recent enough to define them.

## Testing in user mode

With the `usermode-backend` feature, code that allocates through `NtAlloc` runs
in ordinary tests, on the process heap instead of the kernel pool:

```toml
[dev-dependencies]
ntalloc = { version = "0.1", features = ["usermode-backend"] }
```

Where the pool would bugcheck with `BAD_POOL_CALLER`, the test process prints
what went wrong and aborts instead: on a double free, on a pointer the allocator
never handed out, and, with `TAGGED_DEALLOC`, on the wrong tag. Windows reports
any abort as `STATUS_STACK_BUFFER_OVERRUN`, so run the tests with
`-- --test-threads=1` to see which test it was.

Enable the feature only under `[dev-dependencies]`. It brings in `std`, so as a
normal dependency it reaches the driver itself, which then fails to build with a
duplicate `panic_impl` lang item.

## Feature flags

| Feature                 | Toolchain      | Adds                                                                 |
| ----------------------- | -------------- | -------------------------------------------------------------------- |
| `allocator-api`         | nightly        | the `Allocator` impl                                                 |
| `allocator-api-markers` | recent nightly | `StaticAllocator` and `AllocatorClone`; implies `allocator-api`      |
| `usermode-backend`      | stable         | the process-heap backend, for `[dev-dependencies]` only              |

There are no default features.

## Public API at a glance

| Item                                      | What it is                                                     |
| ----------------------------------------- | -------------------------------------------------------------- |
| `NtAlloc`                                 | the allocator: pool flags, tag, and tagged or untagged free    |
| `PagedAlloc`, `NonPagedAlloc`             | `NtAlloc` on the paged and non-paged pool                      |
| `POOL_FLAGS`, `POOL_FLAG_*`               | the pool flags `ExAllocatePool2` takes                         |
| `DEFAULT_MEMORY_TAG`                      | the default tag, `Rust`                                        |
| `MIN_ALIGNMENT`, `POINTER_SIZE`           | the pool's smallest alignment; a stashed pointer's size        |
| `POOL_ZEROES`                             | whether a configuration's blocks arrive zeroed                 |
| `kernel_alignment`                        | the alignment the pool guarantees for a size                   |
| `allocate`, `deallocate`                  | a block of a given size, at the pool's own alignment           |
| `allocate_aligned`, `deallocate_aligned`  | a block for a `Layout`, realigned when needed                  |
| `kernel_allocate`                         | `allocate`, always from the pool                               |
| `GlobalAlloc`, `Allocator`                | the trait impls; `Allocator` with the `allocator-api` feature  |

The full reference is on [docs.rs](https://docs.rs/ntalloc).

## Minimum supported Rust version

**1.85**, set by the 2024 edition. The `allocator-api` and
`allocator-api-markers` features additionally require a nightly compiler;
everything else builds on stable.

The MSRV is treated as a compatibility promise: raising it is a breaking change
and will come with a minor version bump before 1.0.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
