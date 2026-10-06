// SPDX-License-Identifier: BSD-2-Clause
//! The kernel heap: size-classed free lists over KHEAP.
//!
//! This is what makes `Box`, `Vec`, `BTreeMap` and `String` work. The
//! design is driven by one constraint from M9: IPC will allocate, and a
//! kernel path may not contain an unbounded loop. So both `alloc` and
//! `dealloc` are O(1) with no searching at all -- not "usually fast",
//! bounded. See ADR 016 for the alternatives that were rejected for
//! failing exactly that test.
//!
//! Layout of the area:
//!
//! ```text
//!   KHEAP_BASE   +------------------+  unmapped: underflow lands here
//!                |   guard page     |
//!   KHEAP_START  +------------------+  first byte the heap may use
//!                |                  |
//!                |  mapped on demand|  the frontier moves up as needed
//!                |                  |
//!                +------------------+  <- frontier
//!                |   not yet mapped |
//!   KHEAP_END    +------------------+  the 64 MiB limit
//!                |   guard page     |  unmapped: overrun lands here
//!                +------------------+
//! ```

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use super::layout;
use super::paging::{self, FrameSource, PageFlags};
use super::vm;
use super::{PhysFrame, VirtAddr, FRAME_SIZE};
use crate::println;

/// Smallest block the heap hands out.
///
/// 16 bytes, for two reasons: a free block stores a `*mut u8` in its first
/// 8 bytes, so it cannot be smaller than that; and 16 is the alignment
/// `max_align_t` requires on x86-64, so every block is suitably aligned
/// for any ordinary type without extra work.
const MIN_BLOCK: usize = 16;

/// Largest block the heap hands out: 1 MiB.
///
/// A single allocation above this panics rather than falling back to
/// something slower. See ADR 016: the fallback would need a search for
/// contiguous free pages, which is the unbounded loop this design exists
/// to avoid.
const MAX_BLOCK: usize = 1024 * 1024;

/// Number of size classes: 16 B, 32 B, ... 1 MiB.
///
/// `log2(MAX_BLOCK) - log2(MIN_BLOCK) + 1` = 20 - 4 + 1 = 17.
const CLASS_COUNT: usize = 17;

/// Size class index for a block of `size` bytes.
///
/// Rounds up to the next power of two, then takes the log. No loop: both
/// operations are single instructions on x86-64 (`lzcnt` and a subtract).
///
/// Returns `None` if the request is larger than the biggest class.
const fn class_for(size: usize) -> Option<usize> {
    if size > MAX_BLOCK {
        return None;
    }
    let rounded = if size <= MIN_BLOCK {
        MIN_BLOCK
    } else {
        size.next_power_of_two()
    };
    Some(rounded.trailing_zeros() as usize - MIN_BLOCK.trailing_zeros() as usize)
}

/// Bytes in the blocks of class `index`.
const fn class_size(index: usize) -> usize {
    MIN_BLOCK << index
}

// ---- Compile-time checks ----------------------------------------------

const _: () = {
    assert!(
        MIN_BLOCK >= core::mem::size_of::<usize>(),
        "a free block must be able to hold the next pointer inside itself"
    );
    assert!(
        MIN_BLOCK.is_power_of_two(),
        "MIN_BLOCK must be a power of two"
    );
    assert!(
        MAX_BLOCK.is_power_of_two(),
        "MAX_BLOCK must be a power of two"
    );
    assert!(
        class_size(CLASS_COUNT - 1) == MAX_BLOCK,
        "CLASS_COUNT does not match the MIN_BLOCK..MAX_BLOCK range"
    );
};

// ---- State -------------------------------------------------------------

/// Heap bookkeeping.
///
/// Everything is a plain integer or pointer: there is no per-block header,
/// no bitmap, no tree. The free lists live *inside* the free blocks.
struct Heap {
    /// Head of each class's free list, or null. A free block's first eight
    /// bytes hold the address of the next free block in the same class.
    free: [*mut u8; CLASS_COUNT],

    /// First byte not yet handed out. Everything below this and above
    /// `KHEAP_START` has been carved into blocks at some point.
    frontier: u64,

    /// First byte not yet mapped. `frontier` may not pass this without
    /// `grow` being called.
    mapped_end: u64,

    /// Live bytes, as the caller asked for them.
    used_requested: usize,
    /// Live bytes, as the heap actually reserved them (rounded to a class).
    /// The gap between the two is internal fragmentation.
    used_actual: usize,

    /// Running totals, for the acceptance test.
    alloc_calls: u64,
    dealloc_calls: u64,
    grow_calls: u64,
}

// SAFETY: The raw pointers make this !Sync by default. Access is confined
// to this module and funnelled through `with()`, which the module documents
// as non-reentrant; M4 runs on one CPU with interrupts disabled. A second
// CPU needs a real lock -- noted in ADR 016 and the known gaps.
unsafe impl Sync for Heap {}

impl Heap {
    const fn new() -> Self {
        Heap {
            free: [ptr::null_mut(); CLASS_COUNT],
            frontier: layout::KHEAP_START,
            mapped_end: layout::KHEAP_START,
            used_requested: 0,
            used_actual: 0,
            alloc_calls: 0,
            dealloc_calls: 0,
            grow_calls: 0,
        }
    }
}

/// `static`-compatible storage with no synchronisation of its own.
struct HeapCell(core::cell::UnsafeCell<Heap>);

// SAFETY: As for `Heap` -- single CPU, interrupts disabled, one entry
// point.
unsafe impl Sync for HeapCell {}

static HEAP: HeapCell = HeapCell(core::cell::UnsafeCell::new(Heap::new()));

/// Set once `init` has run. Allocating before then is a bug, not an OOM,
/// and the panic says which.
static INITIALISED: AtomicBool = AtomicBool::new(false);

/// Guards against `alloc` being re-entered through its own slow path.
///
/// `grow` maps a page, which allocates frames for intermediate tables,
/// which must not come back round into the heap. Nothing in that path does
/// today -- `GlobalFrames` goes straight to the bitmap allocator -- but the
/// flag turns a future mistake into an immediate, legible panic instead of
/// a corrupted free list.
static IN_ALLOC: AtomicBool = AtomicBool::new(false);

/// Run `f` with exclusive access to the heap.
///
/// # Safety
///
/// The caller must not already be inside `with`.
unsafe fn with<R>(f: impl FnOnce(&mut Heap) -> R) -> R {
    // SAFETY: The caller guarantees no re-entry, and M4 runs on a single
    // CPU with interrupts disabled, so no other context holds a reference.
    unsafe { f(&mut *HEAP.0.get()) }
}

// ---- Growing -----------------------------------------------------------

// ---- The heap's physical reserve ---------------------------------------
//
// M4 let the heap call `frame_allocator::allocate()` whenever it needed a
// page, and ADR 016 recorded that as a conscious relaxation of "the kernel
// does not allocate on its own behalf" -- the heap had no budget because
// budgets are a capability concept and capabilities were still in M5.
//
// M5 arrived and broke it. ADR 022 retires the frame allocator once the
// root Untypeds exist, and its precondition is that "every frame the
// kernel will ever need has already been taken". The heap cannot satisfy
// that: it takes frames *lazily*, so there is always a frame it will need
// and has not taken. The first allocation after boot panicked with the
// retirement message, and `run-heaptest` caught it.
//
// The fix is the budget the heap never had. Boot sets aside a contiguous
// physical reserve before retiring the allocator, and the heap bump-
// allocates frames out of it. Three things follow, all improvements on
// what M4 had:
//
//   * The heap's physical cost is decided once, at boot, and printed --
//     rather than drifting upwards as a side effect of allocation.
//   * Exhausting the reserve is a distinct, named failure.
//   * The frame allocator really can retire, because nothing allocates
//     from it afterwards.
//
// The reserve is `KHEAP_MAX_SIZE`, which makes the 64 MiB limit real
// rather than notional: previously the heap could be stopped short of its
// own limit by a machine that ran out of RAM first, and the two failures
// were hard to tell apart.

/// The heap's physical reserve.
///
/// One struct rather than three statics: the three fields are only ever
/// meaningful together, and a bump allocator whose base and length could
/// drift apart would be a strange thing to have written.
struct Reserve {
    /// Base of the reserve, or zero before boot sets it.
    base: u64,
    /// Bytes in the reserve.
    size: u64,
    /// Bytes already handed out.
    used: u64,
}

/// `static`-compatible storage, with no synchronisation of its own.
struct ReserveCell(core::cell::UnsafeCell<Reserve>);

// SAFETY: Written once by boot before the heap is initialised, and
// afterwards touched only from inside `with`, which is non-reentrant and
// runs on one CPU with interrupts disabled -- the same argument as the
// heap itself.
unsafe impl Sync for ReserveCell {}

static RESERVE: ReserveCell = ReserveCell(core::cell::UnsafeCell::new(Reserve {
    base: 0,
    size: 0,
    used: 0,
}));

/// Hand the heap its physical reserve.
///
/// Called once by `cap::boot`, after the heap is up and before the frame
/// allocator retires.
///
/// # Safety
///
/// `base` must name `size` bytes of contiguous physical memory that the
/// caller has claimed from the frame allocator and will not hand to
/// anybody else. Both must be frame-aligned.
pub unsafe fn set_reserve(base: super::PhysAddr, size: u64) {
    assert!(
        base.0.is_multiple_of(FRAME_SIZE) && size.is_multiple_of(FRAME_SIZE),
        "the heap reserve must be frame-aligned: {} bytes at {}",
        size,
        base
    );

    // SAFETY: Single-threaded boot path, called once before heap::init,
    // so nothing else can hold a reference to the reserve.
    unsafe {
        let reserve = &mut *RESERVE.0.get();
        assert_eq!(
            reserve.used, 0,
            "the heap reserve is being replaced after the heap has already \
             drawn {} bytes from it",
            reserve.used
        );
        reserve.base = base.0;
        reserve.size = size;
    }

    println!(
        "[heap] reserve: {} MiB of contiguous physical memory at {}, taken \
         before the frame allocator retired (ADR 022)",
        size / (1024 * 1024),
        base
    );
}

/// Bytes of the reserve still unused, for the acceptance test.
pub fn reserve_remaining() -> u64 {
    // SAFETY: Not called from inside `heap_frame_source`; single CPU,
    // interrupts disabled.
    unsafe {
        let reserve = &*RESERVE.0.get();
        reserve.size - reserve.used
    }
}

/// Total size of the reserve.
pub fn reserve_size() -> u64 {
    // SAFETY: As above.
    unsafe { (*RESERVE.0.get()).size }
}

/// A [`FrameSource`] that draws from the heap's reserve.
///
/// `paging::map` allocates the intermediate page tables through whatever
/// source it is handed. M4 passed `GlobalFrames`, which goes to the
/// bitmap allocator -- and that is retired by the time the heap grows
/// (ADR 022), so the first growth after boot panicked.
///
/// The leaf page and the tables that address it come from the same
/// reserve. They have to: a budget that covers the data but not the
/// structure needed to reach it is not a budget. The cost is ~0.2% --
/// one table per 512 pages, three levels deep -- and the reserve is
/// sized in whole pages anyway.
struct ReserveFrames;

impl FrameSource for ReserveFrames {
    fn alloc_frame(&mut self) -> Option<PhysFrame> {
        heap_frame_source()
    }
}

/// The single point where the heap takes physical memory.
///
/// A bump pointer over the reserve. No search and no free: the heap
/// never returns a page once mapped, which is the behaviour M4 had, now
/// with a stated bound instead of an open claim on the machine.
fn heap_frame_source() -> Option<PhysFrame> {
    // SAFETY: Reached only from `grow`, which runs inside `with`; one
    // CPU, interrupts disabled, no other reference live.
    unsafe {
        let reserve = &mut *RESERVE.0.get();

        assert!(
            reserve.base != 0,
            "the heap tried to grow before boot gave it a physical reserve; \
             cap::boot::reserve_heap_budget must run before heap::init \
             (ADR 022)"
        );

        if reserve.used + FRAME_SIZE > reserve.size {
            return None;
        }

        let frame = PhysFrame((reserve.base + reserve.used) / FRAME_SIZE);
        reserve.used += FRAME_SIZE;
        Some(frame)
    }
}

/// Why the heap could not grow.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GrowError {
    /// The 64 MiB limit would be exceeded.
    AtLimit { requested: u64, limit: u64 },
    /// The physical reserve is used up.
    OutOfFrames { mapped_pages: u64 },
    /// `paging::map` refused. Should not happen -- the address range is
    /// ours and unmapped -- so the error is carried rather than swallowed.
    MapFailed { page: VirtAddr },
}

/// Map pages until `mapped_end` covers `needed_end`.
///
/// # Safety
///
/// Must be called with exclusive access to `heap` (that is, from inside
/// `with`), and not re-entered.
unsafe fn grow(heap: &mut Heap, needed_end: u64) -> Result<u64, GrowError> {
    if needed_end > layout::KHEAP_END {
        return Err(GrowError::AtLimit {
            requested: needed_end - layout::KHEAP_START,
            limit: layout::KHEAP_MAX_SIZE,
        });
    }

    let target = needed_end.div_ceil(FRAME_SIZE) * FRAME_SIZE;
    let mut mapped = 0u64;

    // SAFETY: Single-threaded boot path; no other borrow of the PML4 is
    // live. The heap's address range belongs to this module alone.
    let table = unsafe { vm::kernel_table() };
    let mut source = ReserveFrames;

    while heap.mapped_end < target {
        let frame = heap_frame_source().ok_or(GrowError::OutOfFrames {
            mapped_pages: (heap.mapped_end - layout::KHEAP_START) / FRAME_SIZE,
        })?;

        let page = VirtAddr::new(heap.mapped_end);

        // The heap holds data, never code: RW and NX, no USER. GLOBAL
        // because it is a kernel mapping present in every address space,
        // same reasoning as the rest of the kernel half (ADR 012).
        let flags = PageFlags::READ | PageFlags::WRITE | PageFlags::GLOBAL;

        // SAFETY: `table` is the live PML4; `frame` just came from the
        // allocator so nothing else owns it; `page` is inside KHEAP and
        // unmapped, since `mapped_end` tracks exactly how far we have got.
        let guard = unsafe { paging::map(table, page, frame, flags, &mut source) }
            .map_err(|_| GrowError::MapFailed { page })?;
        guard.flush();

        heap.mapped_end += FRAME_SIZE;
        mapped += 1;
    }

    if mapped > 0 {
        heap.grow_calls += 1;

        // Report sparsely. The first version printed on every growth,
        // which during the stress test meant hundreds of lines -- each
        // size class takes the slow path the first time it is used, and
        // the serial port is slow enough that the logging dominated the
        // run. Reporting at powers of two keeps the shape visible (the
        // reader can watch it double) without burying everything else.
        let pages = (heap.mapped_end - layout::KHEAP_START) / FRAME_SIZE;
        if pages.is_power_of_two() || heap.grow_calls == 1 {
            println!(
                "[heap] grew to {} pages ({} KiB mapped) after {} growths",
                pages,
                (heap.mapped_end - layout::KHEAP_START) / 1024,
                heap.grow_calls
            );
        }
    }

    Ok(mapped)
}

/// Carve a fresh block of class `index` off the frontier.
///
/// # Safety
///
/// As [`grow`].
unsafe fn carve(heap: &mut Heap, index: usize) -> Result<*mut u8, GrowError> {
    let size = class_size(index) as u64;

    // Align the frontier up to the class size. Classes are powers of two,
    // so a block aligned to its own size satisfies any alignment up to
    // that size -- which is what makes `alloc` able to ignore `align`
    // beyond picking the class.
    let start = heap.frontier.next_multiple_of(size);
    let end = start + size;

    // SAFETY: Caller guarantees exclusive access.
    unsafe {
        grow(heap, end)?;
    }

    heap.frontier = end;
    Ok(start as *mut u8)
}

// ---- The allocator -----------------------------------------------------

/// Size-classed free-list allocator over KHEAP.
pub struct KernelHeap;

// SAFETY: `alloc` returns either null or a pointer to `layout.size()`
// bytes, aligned to at least `layout.align()`, inside memory this module
// owns and has mapped read-write. `dealloc` is only ever called with a
// pointer and layout from a previous `alloc`, which is the caller's
// obligation under the trait. Blocks never overlap: the frontier only
// moves forward, and a block is on exactly one free list or handed out,
// never both.
unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        assert!(
            INITIALISED.load(Ordering::Acquire),
            "allocation of {} bytes before heap::init(); the kernel tried to \
             use the heap during early boot, which is a bug and not an \
             out-of-memory condition",
            layout.size()
        );

        // Re-entry would corrupt the free lists: `grow` is in the middle of
        // updating them when it maps a page. Nothing in the mapping path
        // allocates today; this makes a future change fail loudly.
        assert!(
            !IN_ALLOC.swap(true, Ordering::Acquire),
            "the heap was re-entered from inside an allocation; something on \
             the page-mapping path now allocates"
        );

        // SAFETY: Not called from inside `with` -- the flag above proves
        // it.
        let result = unsafe { with(|heap| allocate_in(heap, layout)) };

        IN_ALLOC.store(false, Ordering::Release);
        result
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // No loop, no search, no coalescing: push onto the class's list.
        // This is the function M9's IPC path cares about most, and it is
        // branch-free apart from the class lookup.
        //
        // SAFETY: Not called from inside `with`. The caller guarantees
        // `ptr` came from `alloc` with this same `layout`.
        unsafe {
            with(|heap| {
                let Some(index) = class_for(block_size(layout)) else {
                    // Unreachable: alloc would have panicked. Freeing it
                    // would corrupt the lists, so stop here instead.
                    panic!(
                        "dealloc of {} bytes, which is larger than any size \
                         class; this pointer did not come from this heap",
                        layout.size()
                    );
                };

                // Write the current head into the block, then make the
                // block the head. Two stores, no reads of other blocks.
                //
                // SAFETY: `ptr` is a live block of at least MIN_BLOCK
                // bytes, so the first eight are ours to use now that the
                // caller has given it back.
                ptr.cast::<*mut u8>().write(heap.free[index]);
                heap.free[index] = ptr;

                heap.used_requested -= layout.size();
                heap.used_actual -= class_size(index);
                heap.dealloc_calls += 1;
            });
        }
    }
}

/// Bytes a `Layout` actually needs from the heap.
///
/// The larger of size and alignment: a class-sized block is aligned to its
/// own size, so asking for a class at least as large as the alignment is
/// what makes the returned pointer correctly aligned.
const fn block_size(layout: Layout) -> usize {
    let size = layout.size();
    let align = layout.align();
    let needed = if size > align { size } else { align };
    if needed < MIN_BLOCK {
        MIN_BLOCK
    } else {
        needed
    }
}

/// The allocation itself, with the heap already borrowed.
fn allocate_in(heap: &mut Heap, layout: Layout) -> *mut u8 {
    let needed = block_size(layout);

    let Some(index) = class_for(needed) else {
        panic!(
            "heap allocation of {} bytes (align {}) exceeds the largest size \
             class ({}); the kernel heap is for small objects -- a large \
             contiguous region should come from the frame allocator directly",
            layout.size(),
            layout.align(),
            MAX_BLOCK
        );
    };

    // --- Fast path: the class has a free block ---------------------------
    //
    // One load, one store. No loop, no search. This is the whole point of
    // the design (ADR 016).
    let head = heap.free[index];
    let block = if !head.is_null() {
        // SAFETY: `head` is a block this allocator previously handed out
        // and had returned; its first eight bytes hold the next pointer.
        heap.free[index] = unsafe { head.cast::<*mut u8>().read() };
        head
    } else {
        // --- Slow path: carve a new block off the frontier ---------------
        //
        // SAFETY: We are inside `with`, so access is exclusive, and the
        // IN_ALLOC flag prevents re-entry.
        match unsafe { carve(heap, index) } {
            Ok(block) => block,
            Err(GrowError::AtLimit { requested, limit }) => {
                panic!(
                    "heap exhausted: allocating {} bytes (align {}, class {}) \
                     would take the heap to {} KiB, past its {} KiB limit. \
                     {} bytes are live in {} allocations.",
                    layout.size(),
                    layout.align(),
                    class_size(index),
                    requested / 1024,
                    limit / 1024,
                    heap.used_requested,
                    heap.alloc_calls - heap.dealloc_calls
                );
            }
            Err(GrowError::OutOfFrames { mapped_pages }) => {
                panic!(
                    "the heap's physical reserve is exhausted while growing \
                     for a {} byte allocation (align {}, class {}); {} pages \
                     were mapped out of a {} KiB reserve and {} bytes are \
                     live. The reserve is fixed at boot (ADR 022), so this \
                     means the heap needs a larger one, not that the machine \
                     is out of memory.",
                    layout.size(),
                    layout.align(),
                    class_size(index),
                    mapped_pages,
                    reserve_size() / 1024,
                    heap.used_requested
                );
            }
            Err(GrowError::MapFailed { page }) => {
                panic!(
                    "mapping heap page {} failed while allocating {} bytes \
                     (align {}); the heap's address range should be ours \
                     alone",
                    page,
                    layout.size(),
                    layout.align()
                );
            }
        }
    };

    heap.used_requested += layout.size();
    heap.used_actual += class_size(index);
    heap.alloc_calls += 1;

    debug_assert!(
        (block as usize).is_multiple_of(layout.align()),
        "block is not aligned to the requested alignment"
    );

    block
}

#[global_allocator]
static ALLOCATOR: KernelHeap = KernelHeap;

// ---- Init and reporting ------------------------------------------------

/// Bytes of heap mapped at init, before any allocation.
///
/// One page. The heap grows on demand, so this is a starting point rather
/// than a reservation -- but mapping nothing at all would mean the very
/// first allocation takes the slow path, and the acceptance test wants to
/// see a heap that exists before it is used.
const INITIAL_PAGES: u64 = 1;

/// Statistics, for the acceptance test and the boot log.
#[derive(Clone, Copy, Debug)]
pub struct Stats {
    pub used_requested: usize,
    pub used_actual: usize,
    pub mapped_pages: u64,
    pub frontier: u64,
    pub alloc_calls: u64,
    pub dealloc_calls: u64,
    pub grow_calls: u64,
}

/// Current heap statistics.
pub fn stats() -> Stats {
    // SAFETY: Not called from inside `with`.
    unsafe {
        with(|heap| Stats {
            used_requested: heap.used_requested,
            used_actual: heap.used_actual,
            mapped_pages: (heap.mapped_end - layout::KHEAP_START) / FRAME_SIZE,
            frontier: heap.frontier,
            alloc_calls: heap.alloc_calls,
            dealloc_calls: heap.dealloc_calls,
            grow_calls: heap.grow_calls,
        })
    }
}

/// Number of blocks currently on the free list of each class.
///
/// Walks the lists, so it is O(free blocks) -- which is exactly why it is
/// a diagnostic and not part of `alloc`. Used by the acceptance test to
/// show where freed memory went.
pub fn free_list_lengths() -> [usize; CLASS_COUNT] {
    // SAFETY: Not called from inside `with`.
    unsafe {
        with(|heap| {
            let mut lengths = [0usize; CLASS_COUNT];
            for (index, &head) in heap.free.iter().enumerate() {
                let mut node = head;
                let mut count = 0usize;
                // Bounded by the number of blocks that fit in the heap, so
                // a cycle in the list cannot hang the kernel -- it trips
                // the assertion instead.
                let bound = (layout::KHEAP_MAX_SIZE as usize) / class_size(index) + 1;
                while !node.is_null() {
                    count += 1;
                    assert!(
                        count <= bound,
                        "free list for class {} ({} bytes) is longer than the \
                         heap can hold; it has a cycle, which means a block \
                         was freed twice",
                        index,
                        class_size(index)
                    );
                    // SAFETY: Every node on a free list is a block whose
                    // first eight bytes hold the next pointer.
                    node = node.cast::<*mut u8>().read();
                }
                lengths[index] = count;
            }
            lengths
        })
    }
}

/// Bring the heap up.
///
/// # Safety
///
/// Call once, after `vm::init` (the heap maps pages through the kernel's
/// own tables) and with interrupts disabled.
pub unsafe fn init() {
    assert!(
        !INITIALISED.load(Ordering::Acquire),
        "heap initialised twice"
    );

    // SAFETY: Nothing else holds a reference: INITIALISED is still false,
    // so no allocation can be in flight.
    unsafe {
        with(|heap| {
            let target = layout::KHEAP_START + INITIAL_PAGES * FRAME_SIZE;
            // SAFETY: Exclusive access, not re-entered.
            match grow(heap, target) {
                Ok(_) => {}
                Err(error) => panic!("could not map the initial heap page: {:?}", error),
            }
        });
    }

    INITIALISED.store(true, Ordering::Release);

    let stats = stats();
    println!(
        "[heap] base {:#018x}..{:#018x} (limit {} MiB), {} page(s) mapped",
        layout::KHEAP_START,
        layout::KHEAP_END,
        layout::KHEAP_MAX_SIZE / (1024 * 1024),
        stats.mapped_pages
    );
    println!(
        "[heap] guard pages: {:#018x} (below), {:#018x} (above)",
        layout::KHEAP_BASE,
        layout::KHEAP_GUARD_TOP
    );
    println!(
        "[heap] {} size classes, {} B .. {} KiB; alloc and dealloc are O(1) \
         with no search (ADR 016)",
        CLASS_COUNT,
        MIN_BLOCK,
        MAX_BLOCK / 1024
    );
}

/// Whether the heap is usable.
pub fn ready() -> bool {
    INITIALISED.load(Ordering::Acquire)
}

/// Bytes in a class, for the tests.
pub fn size_of_class(index: usize) -> usize {
    class_size(index)
}

/// How many size classes there are, for the tests.
pub const fn class_count() -> usize {
    CLASS_COUNT
}
