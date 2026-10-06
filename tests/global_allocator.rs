//! `NtAlloc` as the program's `#[global_allocator]`, the way a driver uses it.
//! Everything this binary allocates goes through it.
//!
//! It runs without the standard test harness (`harness = false` in
//! `Cargo.toml`). The harness frees one of its own over-aligned allocations
//! through a `Box`, and Miri's aliasing models reject that for any allocator
//! that serves over-alignment from a larger block, the standard library's own
//! `System` included, because freeing has to touch memory outside the part
//! the `Box` covers. Without the harness Miri can check every case here except
//! the over-aligned one, for the same reason.

#[cfg(feature = "usermode-backend")]
#[global_allocator]
static GLOBAL: ntalloc::PagedAlloc = ntalloc::PagedAlloc {};

fn main() {
    // Without the feature `NtAlloc` would call the real kernel pool, which a
    // test process cannot do.
    #[cfg(feature = "usermode-backend")]
    checks::run();
}

#[cfg(feature = "usermode-backend")]
mod checks {
    use std::collections::{BTreeMap, HashMap};

    /// Each check, and whether Miri can run it (see the module docs).
    const CHECKS: [(&str, fn(), bool); 6] = [
        (
            "a_vec_survives_many_reallocations",
            a_vec_survives_many_reallocations,
            true,
        ),
        (
            "zeroed_allocations_read_back_as_zero",
            zeroed_allocations_read_back_as_zero,
            true,
        ),
        (
            "sizes_around_the_page_boundary_keep_their_contents",
            sizes_around_the_page_boundary_keep_their_contents,
            true,
        ),
        (
            "ordinary_collections_work_on_top_of_it",
            ordinary_collections_work_on_top_of_it,
            true,
        ),
        (
            "threads_allocate_through_it_at_once",
            threads_allocate_through_it_at_once,
            true,
        ),
        (
            "over_aligned_types_get_their_alignment",
            over_aligned_types_get_their_alignment,
            false,
        ),
    ];

    /// `native` normally, `miri` under Miri, which interprets every byte it
    /// touches. Sizes that straddle the page boundary are kept either way,
    /// since those decide the realignment; only the bulk shrinks.
    fn scaled(native: usize, miri: usize) -> usize {
        if cfg!(miri) { miri } else { native }
    }

    pub fn run() {
        for (name, check, under_miri) in CHECKS {
            if cfg!(miri) && !under_miri {
                println!("{name} ... skipped under Miri");
                continue;
            }
            check();
            println!("{name} ... ok");
        }
    }

    fn a_vec_survives_many_reallocations() {
        let count = u32::try_from(scaled(100_000, 5_000)).unwrap();
        let mut v = Vec::new();
        for i in 0..count {
            v.push(i);
        }

        assert!(v.iter().copied().eq(0..count));
    }

    fn zeroed_allocations_read_back_as_zero() {
        // `vec![0; n]` allocates through `GlobalAlloc::alloc_zeroed`.
        let largest = scaled(100_000, 8_193);
        for len in [1, 15, 16, 17, 4095, 4096, 4097, largest] {
            let v = vec![0u8; len];

            assert!(v.iter().all(|&b| b == 0), "len {len}");
        }
    }

    fn sizes_around_the_page_boundary_keep_their_contents() {
        let largest = scaled(65_537, 12_289);
        for size in [1, 15, 16, 17, 4095, 4096, 4097, 8191, 8192, largest] {
            let pattern = || (0..=u8::MAX).cycle();
            let mut v: Vec<u8> = Vec::with_capacity(size);
            v.extend(pattern().take(size));

            assert!(v.iter().copied().eq(pattern().take(size)), "size {size}");
        }
    }

    fn ordinary_collections_work_on_top_of_it() {
        let mut words = String::new();
        let mut by_len: BTreeMap<usize, Vec<String>> = BTreeMap::new();
        let mut counts: HashMap<String, usize> = HashMap::new();
        let count = scaled(5_000, 300);
        for i in 0..count {
            let word = format!("word{i}");
            words.push_str(&word);
            by_len.entry(word.len()).or_default().push(word.clone());
            *counts.entry(word).or_default() += 1;
        }

        assert_eq!(counts.len(), count);
        assert_eq!(by_len.values().map(Vec::len).sum::<usize>(), count);
        assert!(words.starts_with("word0word1"));
    }

    fn threads_allocate_through_it_at_once() {
        let threads = u8::try_from(scaled(8, 3)).unwrap();
        let rounds = scaled(5_000, 24);
        let longest = scaled(5_000, 400);

        std::thread::scope(|scope| {
            for fill in 0..threads {
                scope.spawn(move || {
                    let mut kept: Vec<Box<[u8]>> = Vec::new();
                    for i in 0..rounds {
                        let len = 1 + (usize::from(fill) * 131 + i * 17) % longest;
                        // Shrinking to fit goes through `GlobalAlloc::realloc`.
                        kept.push(vec![fill; len].into_boxed_slice());
                        if i % 3 == 0 {
                            let gone = kept.swap_remove(i % kept.len());
                            assert!(gone.iter().all(|&b| b == fill));
                        }
                    }
                    assert!(kept.iter().all(|b| b.iter().all(|&x| x == fill)));
                });
            }
        });
    }

    fn over_aligned_types_get_their_alignment() {
        #[repr(align(64))]
        struct Align64(#[allow(dead_code)] u8);
        #[repr(align(4096))]
        struct Align4096(#[allow(dead_code)] u8);
        #[repr(align(16384))]
        struct Align16384(#[allow(dead_code)] u8);

        // Several of each, so they are not all the first block of a fresh heap.
        let a: Vec<_> = (0..32).map(|i| Box::new(Align64(i))).collect();
        let b: Vec<_> = (0..32).map(|i| Box::new(Align4096(i))).collect();
        let c: Vec<_> = (0..32).map(|i| Box::new(Align16384(i))).collect();

        assert!(a.iter().all(|x| addr(&**x) % 64 == 0));
        assert!(b.iter().all(|x| addr(&**x) % 4096 == 0));
        assert!(c.iter().all(|x| addr(&**x) % 16384 == 0));
    }

    fn addr<T>(value: &T) -> usize {
        core::ptr::from_ref(value).addr()
    }
}
