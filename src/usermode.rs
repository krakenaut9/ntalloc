//! The process-heap backend: in this crate's own tests and under the
//! `usermode-backend` feature, [`NtAlloc::allocate`] and
//! [`NtAlloc::deallocate`] use it instead of the kernel pool.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;

use crate::NtAlloc;
use crate::bindings::POOL_FLAGS;

/// A live block of the process-heap backend, as recorded in
/// [`USERMODE_BLOCKS`].
struct UsermodeBlock {
    /// The layout it was allocated with, which freeing it needs again.
    layout: Layout,
    /// The `MEMORY_TAG` of the configuration that allocated it.
    tag: u32,
}

/// The map behind [`USERMODE_BLOCKS`], from block address to record, with its
/// memory from `System`. It hashes with `DefaultHasher` under fixed keys
/// rather than random ones: random keys protect against input chosen to
/// collide, and the input here is addresses the heap picked.
type UsermodeBlockTable = std::collections::HashMap<
    usize,
    UsermodeBlock,
    std::hash::BuildHasherDefault<std::hash::DefaultHasher>,
    std::alloc::System,
>;

/// Live blocks of the process-heap backend, by address.
///
/// It lets [`NtAlloc::deallocate`] free a block from its pointer alone, as
/// `ExFreePool` does. It also turns the frees for which the pool would
/// bugcheck with `BAD_POOL_CALLER` into an abort, rather than heap corruption
/// or a passing test: a pointer the backend never handed out, one already
/// freed, and, with `TAGGED_DEALLOC`, one freed with the wrong tag.
///
/// It is a hash map, so recording or finding a block takes constant time on
/// average, however many are live. Its own storage comes from `System`
/// directly, never from the global allocator, which may be this crate.
/// `HashMap` has no `const` constructor that takes an allocator, so
/// [`with_usermode_blocks`] creates the map on first use.
///
/// A size header just before each block would be simpler, but freeing would
/// then read memory outside the block through the caller's pointer. Rust's
/// aliasing rules forbid that once the caller has narrowed the pointer to the
/// block, as the standard library does through `Box`.
static USERMODE_BLOCKS: std::sync::Mutex<Option<UsermodeBlockTable>> = std::sync::Mutex::new(None);

/// Runs `f` on the table of live blocks while holding its lock, creating the
/// table on first use. The table allocates only from `System`, so working on
/// it never calls back into this crate's allocator, which would try to take
/// the lock it already holds.
fn with_usermode_blocks<R>(f: impl FnOnce(&mut UsermodeBlockTable) -> R) -> R {
    let mut table = USERMODE_BLOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(table.get_or_insert_with(|| {
        UsermodeBlockTable::with_hasher_in(std::hash::BuildHasherDefault::new(), std::alloc::System)
    }))
}

/// Reports a misuse of the process-heap backend, then aborts the process.
///
/// It aborts rather than panics because it runs inside `GlobalAlloc` and
/// `Allocator` methods, neither of which may unwind. The message goes straight
/// to stderr rather than through `eprintln!`: the test harness captures
/// `eprintln!` output and only shows it once a test has finished, which an
/// aborted test never does.
#[cold]
fn usermode_misuse(message: core::fmt::Arguments<'_>) -> ! {
    use std::io::Write as _;

    // Nothing useful can be done if stderr itself fails.
    let _ = writeln!(std::io::stderr(), "ntalloc: {message}");
    std::process::abort()
}

impl<const ALLOC_POOL_FLAGS: POOL_FLAGS, const MEMORY_TAG: u32, const TAGGED_DEALLOC: bool>
    NtAlloc<ALLOC_POOL_FLAGS, MEMORY_TAG, TAGGED_DEALLOC>
{
    /// Stand-in for [`NtAlloc::kernel_allocate`] in this crate's own tests and
    /// under the `usermode-backend` feature: allocates `size` bytes from the
    /// process heap, so the allocator's alignment and pointer-stashing logic
    /// can run without a kernel.
    ///
    /// It mirrors the pool wherever the rest of the crate relies on it:
    ///
    /// - The block is aligned as `ExAllocatePool2` aligns one, to
    ///   [`NtAlloc::kernel_alignment`] for `size`.
    /// - It is freed from its pointer alone, as `ExFreePool` frees one, so
    ///   [`NtAlloc::deallocate`] needs no `Layout`: every live block is
    ///   recorded in [`USERMODE_BLOCKS`].
    /// - It is zeroed unless the pool would leave it uninitialized, via
    ///   [`NtAlloc::POOL_ZEROES`]; otherwise the tests would exercise a
    ///   backend that differs from the shipped one on exactly the property
    ///   [`GlobalAlloc::alloc_zeroed`] depends on.
    /// - A zero `size` returns null without allocating, as in
    ///   [`NtAlloc::kernel_allocate`], and so does a size too large to
    ///   allocate, for which `ExAllocatePool2` would return null too.
    /// - It refuses the same configurations, through
    ///   [`NtAlloc::CONFIGURATION_SUPPORTED`], so a configuration the shipped
    ///   path will not build cannot pass its tests.
    ///
    /// The heap is `std::alloc::System`, not the global allocator: with this
    /// crate as the `#[global_allocator]`, the global allocator would be this
    /// function, and it would recurse until the stack overflowed.
    #[inline(always)]
    pub(crate) fn usermode_allocate(size: usize) -> *mut u8 {
        let () = Self::CONFIGURATION_SUPPORTED;

        if size == 0 {
            return null_mut();
        }

        let Ok(layout) = Layout::from_size_align(size, Self::kernel_alignment(size)) else {
            return null_mut();
        };

        // SAFETY: `layout` has a non-zero size.
        let block = unsafe {
            if Self::POOL_ZEROES {
                std::alloc::System.alloc_zeroed(layout)
            } else {
                std::alloc::System.alloc(layout)
            }
        };

        if !block.is_null() {
            let record = UsermodeBlock {
                layout,
                tag: MEMORY_TAG,
            };
            let previous = with_usermode_blocks(|blocks| blocks.insert(block.addr(), record));
            // The heap reuses an address only once it is free, so a live record
            // here means the block was freed behind the backend's back.
            if previous.is_some() {
                usermode_misuse(format_args!(
                    "the heap handed out {block:p} again while it was live: freed outside NtAlloc?"
                ));
            }
        }

        block
    }

    /// Frees a block from [`NtAlloc::usermode_allocate`] given only its
    /// pointer, as `ExFreePool` frees a pool block, by looking it up in
    /// [`USERMODE_BLOCKS`].
    ///
    /// Where the pool would bugcheck with `BAD_POOL_CALLER`, this reports the
    /// misuse and aborts the process, through [`usermode_misuse`]: for a
    /// pointer the backend never handed out, for one already freed, and, when
    /// `TAGGED_DEALLOC` selects `ExFreePoolWithTag`, for a block allocated
    /// under a different `MEMORY_TAG`.
    pub(crate) fn usermode_free(ptr: *mut u8) {
        let block = with_usermode_blocks(|blocks| blocks.remove(&ptr.addr()));

        let Some(block) = block else {
            usermode_misuse(format_args!(
                "freed {ptr:p}, which the usermode backend never allocated or has already freed"
            ));
        };

        if TAGGED_DEALLOC && block.tag != MEMORY_TAG {
            usermode_misuse(format_args!(
                "freed {ptr:p} with tag '{}', but it was allocated with tag '{}'",
                MEMORY_TAG.to_ne_bytes().escape_ascii(),
                block.tag.to_ne_bytes().escape_ascii(),
            ));
        }

        // SAFETY: `ptr` is a live block that usermode_allocate allocated with
        // `block.layout` and recorded; the record is now removed, so the block
        // is freed exactly once.
        unsafe { std::alloc::System.dealloc(ptr, block.layout) };
    }
}
