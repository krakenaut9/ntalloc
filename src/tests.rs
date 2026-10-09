use super::*;
use core::ptr::without_provenance_mut;

#[test]
fn require_realignment_test() {
    let size = 16;

    for shift in 0..=16u32 {
        let alignment = 1usize << shift;
        let layout = Layout::from_size_align(size, alignment).unwrap();
        // is_kernel_aligned is true when the pool already satisfies the
        // layout, i.e. when no realignment is needed.
        let already_satisfied = PagedAlloc::<DEFAULT_MEMORY_TAG, true>::is_kernel_aligned(layout);

        if alignment <= MIN_ALIGNMENT {
            assert!(
                already_satisfied,
                "alignment {alignment} is within MIN_ALIGNMENT, so the pool \
                 should already satisfy it and no realignment is needed"
            );
        } else {
            assert!(
                !already_satisfied,
                "alignment {alignment} exceeds what the pool guarantees for \
                 a {size}-byte request, so realignment is needed"
            );
        }
    }
}

#[test]
fn original_pointer_location_test() {
    // Stepping back a slot has to stay inside one allocation, so this
    // needs real memory rather than a bare address.
    let mut slots = [null_mut::<u8>(); 2];
    let base = slots.as_mut_ptr();
    // SAFETY: `slots` has two elements, so one past the first is in bounds.
    let ptr = unsafe { base.add(1) }.cast::<u8>();
    let original_ptr_location =
        PagedAlloc::<DEFAULT_MEMORY_TAG, true>::original_pointer_location(ptr);
    assert_eq!(original_ptr_location, base);
}

#[test]
fn save_original_pointer_test() {
    // The slot is written as a whole pointer, so it must be pointer
    // aligned, as it always is in real use: the realigned pointer is
    // aligned to more than MIN_ALIGNMENT and the slot sits one pointer
    // before it. A byte buffer would only be 1-byte aligned.
    let mut slots = [null_mut::<u8>(); 2];
    // SAFETY: `slots` has two elements, so one past the first is in bounds.
    let aligned_ptr = unsafe { slots.as_mut_ptr().add(1) }.cast::<u8>();
    let original_ptr = without_provenance_mut::<u8>(0x1000);
    PagedAlloc::<DEFAULT_MEMORY_TAG, true>::save_original_pointer(aligned_ptr, original_ptr);
    assert_eq!(
        PagedAlloc::<DEFAULT_MEMORY_TAG, true>::original_pointer(aligned_ptr),
        original_ptr
    );
}

#[test]
fn align_up_to_test_less_than_min_alignment() {
    let ptr = without_provenance_mut::<u8>(0x1003);
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
    let ptr = without_provenance_mut::<u8>(0x1003);
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
    let ptr = without_provenance_mut::<u8>(0x1003);
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
    let ptr = without_provenance_mut::<u8>(0x1003);
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
    assert_eq!((aligned_ptr as usize) % layout.align(), 0);

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
    assert_eq!((aligned_ptr as usize) % layout.align(), 0);

    // SAFETY: aligned_ptr came from allocate_aligned with this exact layout.
    unsafe { PagedAlloc::<DEFAULT_MEMORY_TAG, true>::deallocate_aligned(aligned_ptr, layout) };
}

#[test]
fn allocate_aligned_test_alignment_equal_to_page_size() {
    let layout = Layout::from_size_align(16, PAGE_SIZE as usize).unwrap();
    let aligned_ptr = PagedAlloc::<DEFAULT_MEMORY_TAG, true>::allocate_aligned(layout);
    assert!(!aligned_ptr.is_null());
    assert_eq!((aligned_ptr as usize) % layout.align(), 0);

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
    assert_eq!((aligned_ptr as usize) % layout.align(), 0);

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
            without_provenance_mut::<u8>(base),
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
                without_provenance_mut::<u8>(base),
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

#[cfg(feature = "allocator-api")]
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
            );
        }
    }
}

#[cfg(feature = "allocator-api")]
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
    // SAFETY: `empty` came from this allocator with `empty_layout`, and
    // `full_layout` is no smaller.
    let grown =
        unsafe { allocator.grow(empty.cast(), empty_layout, full_layout) }.expect("grow from zero");
    assert_eq!(grown.len(), 64);

    // Shrinking to zero must still release the real allocation.
    // SAFETY: `grown` came from `grow` with `full_layout`, and `empty_layout`
    // is no larger.
    let shrunk = unsafe { allocator.shrink(grown.cast(), full_layout, empty_layout) }
        .expect("shrink to zero");
    assert_eq!(shrunk.len(), 0);

    // SAFETY: `shrunk` came from `shrink` with `empty_layout`.
    unsafe { allocator.deallocate(shrunk.cast(), empty_layout) };
}

#[cfg(feature = "allocator-api")]
#[test]
fn allocator_zeroed_accepts_zero_sized_layouts() {
    let allocator = PagedAlloc::<DEFAULT_MEMORY_TAG, true>::default();
    let layout = Layout::from_size_align(0, 32).unwrap();

    let empty = allocator
        .allocate_zeroed(layout)
        .expect("zero-size allocate_zeroed");

    assert_eq!(empty.len(), 0);
    // SAFETY: `empty` came from `allocate_zeroed` with `layout`.
    unsafe { allocator.deallocate(empty.cast(), layout) };
}

#[test]
fn kernel_allocate_guards_zero_without_reaching_the_pool() {
    // kernel_allocate never uses the heap backend, so this genuinely calls
    // the shipped code path. It is safe to run in a test binary for exactly
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
#[cfg(feature = "allocator-api")]
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
                "size {size} align {align}: allocate_slice must report the \
                 requested size, or the fitting range widens"
            );

            // Walk every size in min..=max that could change the answer.
            let mut probe = size;
            while let Ok(other) = Layout::from_size_align(probe, align) {
                assert_eq!(
                    !Alloc::is_kernel_aligned(other),
                    stashed,
                    "size {size} align {align}: freeing with size {probe} \
                     (legal, max is {reported}) flips the stash decision"
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

        #[cfg(feature = "allocator-api")]
        {
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
        );
    }

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

    #[cfg(feature = "allocator-api")]
    {
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

        #[cfg(feature = "allocator-api")]
        {
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

/// The tag rules accept what `ExAllocatePool2` documents as valid, including
/// shorter tags padded with zero bytes, and refuse the rest for the right
/// reason.
#[test]
fn tag_rules_follow_the_documented_format() {
    let valid = [
        *b"Rust",
        *b"MyDr",
        *b"1234",
        *b"ab\0\0",
        *b"A\0\0\0",
        *b"a b ",
        *b"~ok~",
        // Its only digit is the last character, which the letter-or-digit
        // check must still reach.
        *b"---1",
        *b"BIG\0",
    ];
    for bytes in valid {
        let tag = u32::from_ne_bytes(bytes);
        assert!(tag_is_well_formed(tag), "{bytes:?} should be well-formed");
        assert!(
            tag_has_letter_or_digit(tag),
            "{bytes:?} should have a letter or digit"
        );
        assert_ne!(
            tag, BIG_POOL_TAG,
            "{bytes:?} should not be the reserved tag"
        );
    }

    // Not one to four printable characters followed only by zero padding.
    let malformed = [
        *b"\0abc",
        *b"a\0bc",
        *b"ab\0c",
        *b"\0\0\0a",
        // Just outside the documented range at each end, written out so the
        // test pins the range rather than following MIN_TAG_CHAR/MAX_TAG_CHAR.
        [0x1F, b'a', b'b', b'c'],
        [b'a', 0x7F, b'b', b'c'],
        [b'R', b'u', b's', 0x80],
        [0xFF; 4],
        [0; 4],
    ];
    for bytes in malformed {
        assert!(
            !tag_is_well_formed(u32::from_ne_bytes(bytes)),
            "{bytes:?} should not be well-formed"
        );
    }

    // Well-formed, but with no letter or digit.
    for bytes in [*b"    ", *b"!!!!", *b"-~-~", *b" \0\0\0"] {
        let tag = u32::from_ne_bytes(bytes);
        assert!(tag_is_well_formed(tag), "{bytes:?} should be well-formed");
        assert!(
            !tag_has_letter_or_digit(tag),
            "{bytes:?} should have no letter or digit"
        );
    }

    assert_eq!(BIG_POOL_TAG.to_ne_bytes(), *b"BIG ");
}

/// A tag shorter than four characters, padded with zero bytes, passes the
/// build-time checks and allocates.
#[test]
fn a_zero_padded_tag_allocates() {
    type Short = NtAlloc<POOL_FLAG_NON_PAGED, { u32::from_ne_bytes(*b"ab\0\0") }>;

    let ptr = Short::allocate(64);
    assert!(!ptr.is_null());
    // SAFETY: from Short::allocate, never realigned, freed once.
    unsafe { Short::deallocate(ptr) };
}

/// Both marker traits have empty bodies, so nothing else in the suite
/// notices if an impl is removed or gated wrongly. These bounds are
/// checked at compile time; the call sites exist only to instantiate them.
#[cfg(feature = "allocator-api-markers")]
#[test]
fn the_marker_traits_are_implemented() {
    fn assert_markers<A: StaticAllocator + AllocatorClone>() {}

    assert_markers::<PagedAlloc>();
    assert_markers::<NonPagedAlloc>();
}
