//! The allocator's own API, called directly from outside the crate. The test
//! harness itself uses the default global allocator here.

// Without the feature `NtAlloc` would call the real kernel pool, which a
// test process cannot do.
#![cfg(feature = "usermode-backend")]
#![feature(allocator_api)]
#![cfg_attr(feature = "allocator-api-markers", feature(btreemap_alloc))]

use core::alloc::{GlobalAlloc, Layout};
use ntalloc::{DEFAULT_MEMORY_TAG, NonPagedAlloc, PagedAlloc};
use std::alloc::Allocator;

type Paged = PagedAlloc<DEFAULT_MEMORY_TAG, true>;

/// The same pool, freed with `ExFreePoolWithTag`, under another tag.
type OtherTag = PagedAlloc<{ u32::from_ne_bytes(*b"Othr") }, true>;

#[test]
fn allocate_and_deallocate_pair_up() {
    // Before the heap backend could free from a pointer alone, the free half
    // of this pair went to the kernel and crashed.
    for size in [1usize, 16, 4095, 4096, 4097, 100_000] {
        let ptr = Paged::allocate(size);
        assert!(!ptr.is_null(), "size {size}");
        assert_eq!(ptr.addr() % Paged::kernel_alignment(size), 0, "size {size}");

        // SAFETY: `ptr` holds `size` writable bytes.
        unsafe { ptr.write_bytes(0xA5, size) };
        // SAFETY: from `Paged::allocate`, never realigned, freed once.
        unsafe { Paged::deallocate(ptr) };
    }
}

#[test]
fn allocate_aligned_honours_every_alignment() {
    for align in [1usize, 8, 16, 32, 64, 4096, 8192, 65_536] {
        for size in [1usize, 17, 4096, 10_000] {
            let layout = Layout::from_size_align(size, align).unwrap();
            let ptr = Paged::allocate_aligned(layout);
            assert!(!ptr.is_null(), "size {size}, align {align}");
            assert_eq!(ptr.addr() % align, 0, "size {size}, align {align}");

            // SAFETY: `ptr` holds `size` writable bytes.
            unsafe { ptr.write_bytes(0x5A, size) };
            // SAFETY: from `allocate_aligned` with this exact layout.
            unsafe { Paged::deallocate_aligned(ptr, layout) };
        }
    }
}

#[test]
fn realloc_keeps_contents_and_alignment() {
    let allocator = Paged::default();
    let small = Layout::from_size_align(100, 64).unwrap();

    // SAFETY: each call follows `GlobalAlloc`'s contract: a non-zero layout,
    // then the same block and layout passed back.
    unsafe {
        let ptr = allocator.alloc(small);
        assert!(!ptr.is_null());
        for (offset, byte) in (0..100u8).enumerate() {
            ptr.add(offset).write(byte);
        }

        let grown = allocator.realloc(ptr, small, 10_000);
        assert!(!grown.is_null());
        assert_eq!(grown.addr() % 64, 0);
        assert!(
            (0..100u8)
                .enumerate()
                .all(|(offset, byte)| *grown.add(offset) == byte)
        );

        allocator.dealloc(grown, Layout::from_size_align(10_000, 64).unwrap());
    }
}

#[test]
fn collections_work_through_the_allocator_api() {
    let mut v: Vec<u64, NonPagedAlloc> = Vec::new_in(NonPagedAlloc::default());
    v.extend(0..50_000);
    v.truncate(10);
    v.shrink_to_fit();
    assert!(v.iter().eq((0..10).collect::<Vec<u64>>().iter()));

    let boxed = Box::new_in([7u8; 5000], Paged::default());
    assert!(boxed.iter().all(|&b| b == 7));

    // Zero-sized elements never reach the backend at all.
    let mut units: Vec<(), Paged> = Vec::new_in(Paged::default());
    units.extend(std::iter::repeat_n((), 1000));
    assert_eq!(units.len(), 1000);
}

#[test]
fn an_impossible_size_is_null_not_a_panic() {
    // `ExAllocatePool2` returns null for a request it cannot meet, so the
    // heap backend must too. These sizes cannot even form a `Layout`.
    assert!(Paged::allocate(usize::MAX).is_null());
    assert!(Paged::allocate(isize::MAX as usize).is_null());

    // This one is a valid layout that the heap itself refuses. Miri cannot
    // model that: it stops with resource exhaustion instead of returning null.
    if !cfg!(miri) {
        let huge = Layout::from_size_align(isize::MAX as usize - 4095, 4096).unwrap();
        // SAFETY: a non-zero layout.
        assert!(unsafe { Paged::default().alloc(huge) }.is_null());
        assert!(Paged::default().allocate(huge).is_err());
    }
}

/// The exported pool flags compose into configurations a consumer can name,
/// and `POOL_FLAG_UNINITIALIZED` hands zeroing over to the zeroing entry points.
#[test]
fn exported_pool_flags_compose_into_working_allocators() {
    use ntalloc::{
        NtAlloc, POOL_FLAG_CACHE_ALIGNED, POOL_FLAG_NON_PAGED, POOL_FLAG_PAGED,
        POOL_FLAG_SPECIAL_POOL, POOL_FLAG_UNINITIALIZED, POOL_FLAG_USE_QUOTA, POOL_FLAGS,
    };

    const SCRATCH: POOL_FLAGS = POOL_FLAG_NON_PAGED | POOL_FLAG_UNINITIALIZED;
    type Scratch = NtAlloc<SCRATCH>;
    const SPECIAL: POOL_FLAGS =
        POOL_FLAG_PAGED | POOL_FLAG_CACHE_ALIGNED | POOL_FLAG_USE_QUOTA | POOL_FLAG_SPECIAL_POOL;
    type Special = NtAlloc<SPECIAL>;

    const { assert!(!Scratch::POOL_ZEROES) };
    const { assert!(Special::POOL_ZEROES) };

    // The pool leaves this memory uninitialized, so `alloc_zeroed` must zero
    // it itself; under Miri, reading bytes it skipped is an error.
    let layout = Layout::from_size_align(256, 64).unwrap();
    // SAFETY: a non-zero layout, freed below with the same layout.
    let ptr = unsafe { Scratch::default().alloc_zeroed(layout) };
    assert!(!ptr.is_null());
    // SAFETY: `ptr` holds `layout.size()` bytes that `alloc_zeroed` initialized.
    let bytes = unsafe { core::slice::from_raw_parts(ptr, layout.size()) };
    assert!(bytes.iter().all(|&b| b == 0));
    // SAFETY: from `alloc_zeroed` with this layout, freed once.
    unsafe { Scratch::default().dealloc(ptr, layout) };

    let ptr = Special::allocate(64);
    assert!(!ptr.is_null());
    // SAFETY: from `Special::allocate`, never realigned, freed once.
    unsafe { Special::deallocate(ptr) };
}

/// Runs `test` from this binary in a child process with `trigger` set, for
/// misuse that ends the process, and returns what the child wrote to stderr.
///
/// The child keeps the harness's usual output capture, as a consumer's tests
/// would, so this also checks that the backend's message survives the abort.
fn run_aborting_child(test: &str, trigger: &str) -> String {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test])
        .env(trigger, "1")
        .output()
        .unwrap();

    assert!(!output.status.success(), "the child was expected to abort");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A double free aborts, where the pool would bugcheck with
/// `BAD_POOL_CALLER` 0x06 or 0x07, rather than corrupting the heap.
#[test]
#[cfg_attr(miri, ignore = "Miri cannot start processes")]
fn a_double_free_aborts_instead_of_corrupting_the_heap() {
    if std::env::var_os("NTALLOC_DOUBLE_FREE_CHILD").is_some() {
        let ptr = Paged::allocate(64);
        assert!(!ptr.is_null());
        // SAFETY: from `Paged::allocate`, never realigned.
        unsafe { Paged::deallocate(ptr) };
        // SAFETY: not upheld, on purpose: this frees `ptr` a second time, the
        // misuse under test. The backend finds no live block at this address
        // and aborts before touching any memory.
        unsafe { Paged::deallocate(ptr) };
        unreachable!("the second free must abort");
    }

    let stderr = run_aborting_child(
        "a_double_free_aborts_instead_of_corrupting_the_heap",
        "NTALLOC_DOUBLE_FREE_CHILD",
    );
    assert!(
        stderr.contains("never allocated or has already freed"),
        "{stderr}"
    );
}

/// Freeing with the wrong tag aborts when `TAGGED_DEALLOC` is set, where
/// `ExFreePoolWithTag` would bugcheck with `BAD_POOL_CALLER` 0x0A.
#[test]
#[cfg_attr(miri, ignore = "Miri cannot start processes")]
fn freeing_with_the_wrong_tag_aborts() {
    if std::env::var_os("NTALLOC_WRONG_TAG_CHILD").is_some() {
        let ptr = Paged::allocate(64);
        assert!(!ptr.is_null());
        // SAFETY: not upheld, on purpose: `ptr` came from a configuration with
        // another tag, the misuse under test. The backend compares the tags
        // and aborts before touching any memory.
        unsafe { OtherTag::deallocate(ptr) };
        unreachable!("freeing with the wrong tag must abort");
    }

    let stderr = run_aborting_child(
        "freeing_with_the_wrong_tag_aborts",
        "NTALLOC_WRONG_TAG_CHILD",
    );
    assert!(
        stderr.contains("with tag 'Othr', but it was allocated with tag 'Rust'"),
        "{stderr}"
    );
}

/// A block freed behind the backend's back, here straight to `System`, aborts
/// as soon as the heap hands its address out again, rather than the backend
/// taking the new block for the old one.
#[test]
#[cfg_attr(miri, ignore = "Miri cannot start processes")]
fn freeing_outside_the_backend_aborts() {
    if std::env::var_os("NTALLOC_FREED_OUTSIDE_CHILD").is_some() {
        let layout = Layout::from_size_align(64, Paged::kernel_alignment(64)).unwrap();
        let ptr = Paged::allocate(64);
        assert!(!ptr.is_null());
        // SAFETY: `System` handed out `ptr` for exactly this layout, so freeing
        // it there is sound. It is still a misuse, on purpose: the backend
        // records the block as live, which is what this test is about.
        unsafe { std::alloc::System.dealloc(ptr, layout) };

        // The heap reuses a freed block within a few allocations of its size,
        // and the backend must abort when it does. The bound only stops a heap
        // that never reuses it from running forever.
        let mut kept = Vec::with_capacity(1_000_000);
        for _ in 0..1_000_000 {
            kept.push(Paged::allocate(64));
        }
        unreachable!("the heap never handed the address out again");
    }

    let stderr = run_aborting_child(
        "freeing_outside_the_backend_aborts",
        "NTALLOC_FREED_OUTSIDE_CHILD",
    );
    assert!(stderr.contains("again while it was live"), "{stderr}");
}

/// Every allocation and free updates one table shared by all threads.
#[test]
fn threads_share_the_backend_safely() {
    let (threads, rounds): (u8, usize) = if cfg!(miri) { (4, 30) } else { (8, 3_000) };

    std::thread::scope(|scope| {
        for fill in 0..threads {
            scope.spawn(move || {
                let mut live = Vec::new();
                for i in 0..rounds {
                    let size = 1 + (usize::from(fill) * 131 + i * 17) % 5_000;
                    let ptr = Paged::allocate(size);
                    assert!(!ptr.is_null());
                    // SAFETY: `ptr` holds `size` writable bytes.
                    unsafe { ptr.write_bytes(fill, size) };
                    live.push((ptr, size));

                    if i % 3 == 0 {
                        let (ptr, size) = live.swap_remove(i % live.len());
                        free_checked(ptr, size, fill);
                    }
                }
                for (ptr, size) in live {
                    free_checked(ptr, size, fill);
                }
            });
        }
    });
}

/// Checks that no other thread wrote into the block, then frees it.
fn free_checked(ptr: *mut u8, size: usize, fill: u8) {
    // SAFETY: a live block of `size` bytes, written with `fill`.
    let bytes = unsafe { core::slice::from_raw_parts(ptr, size) };
    assert!(bytes.iter().all(|&b| b == fill));
    // SAFETY: from `Paged::allocate`, never realigned, freed once.
    unsafe { Paged::deallocate(ptr) };
}

#[cfg(feature = "allocator-api-markers")]
#[test]
fn shared_and_ordered_collections_work_with_the_marker_traits() {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    // Cloning an `Arc` needs `AllocatorClone`, as does any `BTreeMap`.
    let shared = Arc::new_in(5u32, Paged::default());
    let clone = Arc::clone(&shared);
    assert_eq!(*clone, 5);

    let mut map = BTreeMap::new_in(Paged::default());
    for i in 0..1000 {
        map.insert(i, i * 2);
    }
    assert_eq!(map[&999], 1998);
}
