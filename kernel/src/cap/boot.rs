// SPDX-License-Identifier: BSD-2-Clause
//! Building the first CSpace.
//!
//! Every capability in the system descends from one created here. Boot
//! walks the memory map, turns each usable region into one or more root
//! Untypeds, puts their capabilities into the init component's CSpace,
//! and then **retires the frame allocator** (ADR 022): from that point
//! the only way to obtain memory is to retype an Untyped, so every
//! allocation is charged to somebody's budget.
//!
//! The CSpace built here has the shape ADR 018 requires: a root CNode
//! whose slots hold only CNodes, and leaf CNodes holding everything else.
//!
//! ```text
//!   root CNode (radix 4, 16 slots)
//!   +----------+----------+----------+-----
//!   | CNode    | CNode    | (empty)  | ...
//!   | "untyped"| "objects"|          |
//!   +----------+----------+----------+-----
//!        |           |
//!        |           +--> leaf CNode: retyped objects go here
//!        |
//!        +--> leaf CNode: one slot per root Untyped
//! ```

use limine::memmap;

use super::cnode::{self, CNode, Slot};
use super::cptr::CSpace;
use super::{CapType, ObjectRef, Rights, SLOT_SIZE, UNTYPED_MAX_BITS, UNTYPED_MIN_BITS};
use crate::memory::bootinfo::MemoryRegion;
use crate::memory::{frame_allocator, PhysAddr, FRAME_SIZE};
use crate::println;

/// Radix of the root CNode: 16 slots, so up to 16 leaf CNodes.
///
/// Small on purpose. ADR 018 fixed the hierarchy at two levels, so the
/// root is an index of CNodes rather than a store of capabilities, and
/// M13 is expected to want a handful.
const ROOT_RADIX: u8 = 4;

/// Radix of the leaf CNode holding the root Untypeds: 256 slots.
const UNTYPED_RADIX: u8 = 8;

/// Radix of the leaf CNode the tests retype into: 256 slots.
const OBJECT_RADIX: u8 = 8;

/// Index of the Untyped CNode within the root.
pub const ROOT_SLOT_UNTYPED: usize = 0;

/// Index of the general-purpose object CNode within the root.
pub const ROOT_SLOT_OBJECTS: usize = 1;

/// Largest root Untyped we will create: 2^22 = 4 MiB.
///
/// A region bigger than this is split into several. Two reasons to cap
/// it: an Untyped is revoked as a unit, so a huge one makes revocation
/// coarse; and ADR 020's watermark never goes backwards, so a component
/// that wants to reclaim memory needs Untypeds small enough to throw
/// away whole.
const MAX_ROOT_UNTYPED_BITS: u8 = 22;

/// The init component's CSpace, once [`init`] has built it.
static mut INIT_CSPACE: Option<CSpace> = None;

/// How much memory the root Untypeds cover, for the acceptance test.
static mut ROOT_UNTYPED_BYTES: u64 = 0;

/// How many root Untypeds were created.
static mut ROOT_UNTYPED_COUNT: usize = 0;

/// The init CSpace.
///
/// # Panics
///
/// Before [`init`] has run.
pub fn init_cspace() -> CSpace {
    // SAFETY: Written once by `init` on the single-threaded boot path and
    // read-only afterwards.
    unsafe {
        let addr = &raw const INIT_CSPACE;
        (*addr).expect("cap::boot::init_cspace() before cap::boot::init()")
    }
}

/// Total bytes covered by the root Untypeds.
pub fn root_untyped_bytes() -> u64 {
    // SAFETY: As above.
    unsafe {
        let addr = &raw const ROOT_UNTYPED_BYTES;
        *addr
    }
}

/// Number of root Untypeds created.
pub fn root_untyped_count() -> usize {
    // SAFETY: As above.
    unsafe {
        let addr = &raw const ROOT_UNTYPED_COUNT;
        *addr
    }
}

/// Allocate a run of contiguous frames for a CNode.
///
/// CNodes must be contiguous because a CPtr indexes them as an array,
/// and `allocate()` promises only "some free frame" -- nothing about
/// adjacency. So this looks for the run first and claims it afterwards,
/// rather than allocating hopefully and giving frames back when the run
/// breaks. Claiming is what `allocate_specific` exists for.
///
/// The search walks the bitmap once, so it is bounded by the size of
/// physical memory and cannot loop. It is also boot-time only: this is
/// among the last things to touch the allocator before it retires.
fn allocate_contiguous(frames: u64) -> Option<PhysAddr> {
    if frames == 0 {
        return None;
    }

    let total = frame_allocator::frame_count();
    if frames > total {
        return None;
    }

    // Frame 0 is never handed out, so starting at 1 skips a guaranteed
    // miss. `run_start` tracks the beginning of the current unbroken
    // stretch of free frames.
    let mut run_start = 1u64;
    let mut frame = 1u64;

    while frame < total {
        if frame_allocator::is_used(crate::memory::PhysFrame(frame)) {
            // The run is broken; the next candidate starts after it.
            run_start = frame + 1;
        } else if frame - run_start + 1 == frames {
            // Long enough. Claim every frame before returning: until
            // they are claimed, `allocate()` could still hand one out.
            for index in 0..frames {
                let taken =
                    frame_allocator::allocate_specific(crate::memory::PhysFrame(run_start + index));
                assert!(
                    taken,
                    "frame {} was free during the scan but could not be \
                     claimed; something else allocated while boot was \
                     building the CSpace",
                    run_start + index
                );
            }
            return Some(PhysAddr(run_start * FRAME_SIZE));
        }
        frame += 1;
    }

    None
}

/// Create a CNode object and return its base address.
///
/// Boot-time only.
fn create_cnode(radix: u8) -> PhysAddr {
    let bytes = (SLOT_SIZE as u64) << radix;
    let frames = bytes.div_ceil(FRAME_SIZE);

    let base = allocate_contiguous(frames).unwrap_or_else(|| {
        panic!(
            "could not find {} contiguous frames for a CNode of radix {}",
            frames, radix
        )
    });

    // Zero it: a slot full of stale bits would be read as a capability.
    //
    // SAFETY: The frames came from the allocator so nothing else owns
    // them, and PMAP maps all RAM writably.
    unsafe {
        let ptr = crate::memory::phys_to_virt(base).as_ptr::<u8>();
        core::ptr::write_bytes(ptr, 0, (frames * FRAME_SIZE) as usize);
    }

    base
}

/// Whether a memory region is RAM the capability system may hand out.
///
/// Narrower than it looks: a region being *usable* is necessary but not
/// sufficient. The kernel image, its page tables, its stacks, the frame
/// bitmap and the heap reserve all live in memory that the map calls
/// usable, and giving those frames to init would let init overwrite the
/// kernel. The loop below therefore checks the allocator frame by frame
/// and only claims blocks that are still free.
fn is_candidate(kind: u64) -> bool {
    kind == memmap::MEMMAP_USABLE || kind == memmap::MEMMAP_BOOTLOADER_RECLAIMABLE
}

/// Reserve the heap's physical budget.
///
/// The heap maps pages on demand, so "every frame the kernel will ever
/// need has already been taken" -- ADR 022's precondition for retiring
/// the frame allocator -- can never be true of it while it allocates
/// lazily. Either the heap stops being lazy or it gets a budget; a budget
/// is cheaper, and it is what every other consumer of memory will have
/// once capabilities are in use.
///
/// Called from `kmain` *before* `heap::init`, and so before the root
/// Untypeds exist. Both orderings matter and neither is obvious:
///
///   * Before `heap::init`, because init maps the first page and would
///     otherwise draw from a reserve that does not exist yet. The
///     tripwire in `heap_frame_source` caught exactly that.
///   * Before the Untypeds, because this memory would otherwise be handed
///     to init along with everything else.
///
/// # Safety
///
/// Call once, after `memory::init` and `vm::init`, before `heap::init`.
pub unsafe fn reserve_heap_budget() {
    let heap_bytes = crate::memory::layout::KHEAP_MAX_SIZE;
    let heap_frames = heap_bytes / FRAME_SIZE;

    // The reserve pays for the page tables as well as the pages.
    //
    // It has to: `paging::map` allocates the intermediate tables, and if
    // those came from anywhere else the heap would have a budget for its
    // data and an open claim for its structure. But sizing the reserve
    // at exactly KHEAP_MAX_SIZE then makes the heap's own 64 MiB limit
    // unreachable -- `heap-oom` ran out of reserve at 16350 of 16384
    // pages, 34 frames short, which is precisely what the tables cost.
    //
    // So the reserve is the heap plus its tables. For a 64 MiB heap at
    // 4 KiB pages:
    //
    //   PTs:  16384 pages / 512 =  32
    //   PDs:     32 PTs   / 512 =   1
    //   PDPT:                       1
    //   (the PML4 entry already exists)
    //
    // 34 frames, which is what was observed. Rounded up to 64 for the
    // boundary cases -- a range that straddles two PDs needs one more of
    // each -- and so that the figure does not need revisiting if the
    // heap's base moves.
    const TABLE_FRAMES: u64 = 64;
    let frames = heap_frames + TABLE_FRAMES;
    let size = frames * FRAME_SIZE;

    let base = allocate_contiguous(frames).unwrap_or_else(|| {
        panic!(
            "could not find {} contiguous frames ({} MiB) for the kernel \
             heap's reserve; the machine has less free contiguous memory \
             than the heap's own limit plus its page tables",
            frames,
            size / (1024 * 1024)
        )
    });

    // SAFETY: `allocate_contiguous` claimed every frame in the range, so
    // nothing else owns it, and both ends are frame-aligned.
    unsafe {
        crate::memory::heap::set_reserve(base, size);
    }
}

/// Bring up the capability system.
///
/// Builds the init CSpace, fills it with root Untypeds covering the free
/// memory, and retires the frame allocator.
///
/// Takes the regions from `bootinfo`'s *copy* rather than from Limine's
/// response: by the time this runs, `reclaim` has poisoned the
/// bootloader's memory and the original entries are gone. ADR 013 exists
/// because the first version of M3 learned that the hard way.
///
/// # Safety
///
/// Call once, after `memory::init`, `vm::init`, `bootinfo::reclaim` and
/// `heap::init`, with interrupts disabled.
pub unsafe fn init_from_regions(regions: &[MemoryRegion]) {
    let free_before = frame_allocator::free_count();

    assert!(
        crate::memory::heap::reserve_size() > 0,
        "cap::boot::init ran before the heap was given its reserve; the \
         Untypeds below would take the memory the heap needs"
    );

    // --- The CSpace skeleton --------------------------------------------

    let root_base = create_cnode(ROOT_RADIX);
    let untyped_base = create_cnode(UNTYPED_RADIX);
    let object_base = create_cnode(OBJECT_RADIX);

    // SAFETY: Just created, zeroed, and the right size.
    let space = unsafe { CSpace::new(root_base, ROOT_RADIX) };

    // The root's slots hold CNodes and nothing else -- that is the rule
    // that makes a CPtr mean the same thing regardless of the tree's
    // shape (ADR 018).
    //
    // SAFETY: Indices in range; the slots were zeroed, so they are empty.
    unsafe {
        cnode::seed_root(
            space.root_slot(ROOT_SLOT_UNTYPED),
            ObjectRef(untyped_base),
            CapType::CNode,
            Rights::ALL,
            UNTYPED_RADIX as u32,
            false,
        )
        .expect("the freshly zeroed root slot was not empty");

        cnode::seed_root(
            space.root_slot(ROOT_SLOT_OBJECTS),
            ObjectRef(object_base),
            CapType::CNode,
            Rights::ALL,
            OBJECT_RADIX as u32,
            false,
        )
        .expect("the freshly zeroed root slot was not empty");
    }

    // SAFETY: The CNode was created with this radix.
    let untyped_cnode = unsafe { CNode::new(untyped_base, UNTYPED_RADIX) };

    // --- Root Untypeds ---------------------------------------------------
    //
    // One pass over the memory map. For each candidate region, take the
    // frames the allocator still has and turn maximal aligned blocks into
    // Untypeds. Aligned blocks only: ADR 020 requires an Untyped to be
    // 2^k bytes at a 2^k boundary so that carving objects out of it is a
    // single mask.

    let mut created = 0usize;
    let mut total_bytes = 0u64;

    for region in regions {
        if !is_candidate(region.kind) {
            continue;
        }

        let mut addr = region.base.next_multiple_of(FRAME_SIZE);
        let end = region.base + region.length;

        while addr < end {
            if created >= untyped_cnode.len() {
                break;
            }

            // Largest aligned power-of-two block that starts here and
            // fits in what is left.
            let mut bits = MAX_ROOT_UNTYPED_BITS;
            while bits >= UNTYPED_MIN_BITS {
                let size = 1u64 << bits;
                if addr.is_multiple_of(size) && addr + size <= end {
                    break;
                }
                bits -= 1;
            }
            if bits < UNTYPED_MIN_BITS {
                // Nothing aligned fits; skip to the next frame boundary.
                addr = (addr + FRAME_SIZE) & !(FRAME_SIZE - 1);
                continue;
            }

            let size = 1u64 << bits;
            let frames = size / FRAME_SIZE;

            // Only claim the block if every frame in it is still free.
            // A frame that is taken belongs to the kernel -- its image,
            // its page tables, its stacks, the bitmap -- and handing it
            // to init would let init overwrite the kernel.
            let mut all_free = true;
            for index in 0..frames {
                let frame = crate::memory::PhysFrame((addr / FRAME_SIZE) + index);
                if frame_allocator::is_used(frame) {
                    all_free = false;
                    break;
                }
            }

            if !all_free {
                addr += FRAME_SIZE;
                continue;
            }

            // Claim every frame so nothing else hands them out.
            for index in 0..frames {
                let frame = crate::memory::PhysFrame((addr / FRAME_SIZE) + index);
                let taken = frame_allocator::allocate_specific(frame);
                debug_assert!(taken, "a frame checked as free could not be claimed");
            }

            // SAFETY: Every frame in the block is now ours.
            unsafe {
                super::untyped::Untyped::initialise(PhysAddr(addr));
            }

            // SAFETY: `created` is below the CNode's length, checked at
            // the top of the loop; the slot was zeroed.
            unsafe {
                let slot = untyped_cnode.slot(created);
                cnode::seed_root(
                    slot,
                    ObjectRef(PhysAddr(addr)),
                    CapType::Untyped,
                    Rights::ALL,
                    bits as u32,
                    false,
                )
                .expect("a freshly zeroed Untyped slot was not empty");
            }

            created += 1;
            total_bytes += size;
            addr += size;
        }
    }

    // SAFETY: Single-threaded boot path; nothing else reads these yet.
    unsafe {
        let cspace = &raw mut INIT_CSPACE;
        cspace.write(Some(space));
        let bytes = &raw mut ROOT_UNTYPED_BYTES;
        bytes.write(total_bytes);
        let count = &raw mut ROOT_UNTYPED_COUNT;
        count.write(created);
    }

    println!(
        "[cap] init cspace: root radix {} at {}, untyped cnode radix {}, \
         object cnode radix {}",
        ROOT_RADIX, root_base, UNTYPED_RADIX, OBJECT_RADIX
    );
    println!(
        "[cap] {} root untypeds covering {} MiB (largest 2^{} = {} MiB)",
        created,
        total_bytes / (1024 * 1024),
        MAX_ROOT_UNTYPED_BITS,
        (1u64 << MAX_ROOT_UNTYPED_BITS) / (1024 * 1024)
    );

    let free_after = frame_allocator::free_count();
    println!(
        "[cap] frame allocator: {} free before, {} after; {} frames became \
         untyped memory",
        free_before,
        free_after,
        free_before - free_after
    );

    // --- Retirement -------------------------------------------------------
    //
    // From here, memory comes only from retyping an Untyped. ADR 022: the
    // allocator was a boot-time tool, and keeping it callable would leave
    // a path by which the kernel allocates without a budget.
    //
    // SAFETY: Every frame the kernel needs has been taken, and the rest
    // belongs to the root Untypeds.
    unsafe {
        frame_allocator::retire();
    }
    println!(
        "[cap] frame allocator retired; allocation now goes through retype \
         only (ADR 022)"
    );
}

/// Slot in the object CNode reserved for the boot thread's TCB.
///
/// Fixed rather than allocated, and high enough to stay out of the way
/// of the test fixtures, which work upward from zero. The boot
/// thread's TCB must outlive every test, so it cannot share a slot
/// with anything that gets cleared.
pub const BOOT_TCB_SLOT: usize = 255;

/// Give the currently executing code a TCB.
///
/// The kernel is already running: it has a stack (KSTACK slot 1,
/// mapped by `vm::init`) and a context, and what it lacks is somewhere
/// to *save* that context when it first switches away. This retypes a
/// TCB and fills in the stack bounds, without touching the context --
/// which is correct, because the context is live in the CPU's
/// registers and gets written by the first `switch_context`.
///
/// # Safety
///
/// Call once, after `init_from_regions` and before `sched::init`, on
/// the main kernel stack.
pub unsafe fn create_boot_tcb() -> &'static mut super::tcb::Tcb {
    // SAFETY: Caller's guarantee that `init_from_regions` has run, and
    // ROOT_SLOT_OBJECTS is seeded with a CNode there.
    let cnode = unsafe { leaf_cnode(ROOT_SLOT_OBJECTS) };

    // From the last root Untyped, so the test fixtures' Untypeds stay
    // untouched. The boot TCB is permanent and must not share an
    // Untyped with anything that gets revoked.
    let index = root_untyped_count() - 1;
    // SAFETY: Index below the count.
    let parent = unsafe { root_untyped(index) };

    // SAFETY: Live Untyped and CNode; the slot was zeroed at boot.
    unsafe { super::untyped::retype(parent, CapType::Tcb, 0, 1, &cnode, BOOT_TCB_SLOT) }
        .expect("could not retype the boot thread's TCB");

    // SAFETY: Index in range.
    let slot = unsafe { cnode.slot(BOOT_TCB_SLOT) };
    // SAFETY: The slot holds a live TCB and nothing else borrows it.
    let tcb = unsafe { super::tcb::from_slot(slot) }.expect("the boot TCB slot is not a TCB");

    // The main stack, already mapped by vm::init. Recorded here so
    // that a stack-overflow report can name this thread, and so the
    // acceptance test can check that no other thread's RSP falls
    // inside it.
    let (_, bottom, top) =
        crate::memory::layout::kstack_slot(crate::memory::layout::KSTACK_SLOT_MAIN);

    // Not `configure`: that writes a fresh context for a thread that
    // has never run, and this thread is running right now. Its
    // registers are in the CPU, and the first switch away from it is
    // what writes them here.
    //
    // SAFETY: The TCB was just retyped and zeroed, and nothing else
    // refers to it yet.
    unsafe {
        tcb.adopt_running(top, bottom, crate::memory::layout::KSTACK_SLOT_MAIN);
    }

    println!(
        "[cap] boot tcb: object slot {}, stack {} .. {} (the main stack, \
         mapped by vm::init)",
        BOOT_TCB_SLOT, bottom, top
    );

    tcb
}

/// Borrow the leaf CNode at a root index.
///
/// # Safety
///
/// `init` must have run and the index must name a CNode slot.
pub unsafe fn leaf_cnode(root_index: usize) -> CNode {
    let space = init_cspace();
    // SAFETY: Caller's guarantee.
    let slot = unsafe { space.root_slot(root_index) };
    assert_eq!(
        slot.cap_type(),
        CapType::CNode,
        "root slot {} does not hold a CNode",
        root_index
    );
    // SAFETY: The capability says how many slots the object has.
    unsafe { CNode::new(slot.object().0, slot.as_radix()) }
}

/// Borrow a root Untyped's slot by index.
///
/// # Safety
///
/// As [`leaf_cnode`].
pub unsafe fn root_untyped(index: usize) -> &'static mut Slot {
    // SAFETY: Caller's guarantee.
    let cnode = unsafe { leaf_cnode(ROOT_SLOT_UNTYPED) };
    assert!(
        index < root_untyped_count(),
        "root untyped {} does not exist ({} were created)",
        index,
        root_untyped_count()
    );
    // SAFETY: Index checked.
    unsafe { cnode.slot(index) }
}
/// The first root Untyped with at least `bytes` left in it.
///
/// # Why this exists
///
/// Test fixtures used to name a root Untyped by index — `root_untyped(0)`,
/// `root_untyped(1)` — and that worked for as long as the indices chosen
/// happened to be large ones. M6's thread tests pick 0 and 1, which are
/// 4 MiB here; M9's IPC tests needed a third thread, picked index 1
/// again, and ran it dry partway through a stack:
///
/// ```text
/// [PANIC] could not retype the stack frames:
///         NotEnoughMemory { remaining: 10240, needed: 18432 }
/// ```
///
/// The first fix was to pick different indices, and it failed the same
/// way with different numbers — because the mistake was not *which*
/// index but that there was an index at all. A fixture that names a
/// resource by position is asserting something about the memory map it
/// has no business knowing, and it keeps working by luck until the day
/// somebody adds one more object.
///
/// So fixtures say what they need and this finds it. ADR 023 records
/// the rule.
///
/// # The panic
///
/// Deliberately verbose, because "there is no room" and "I looked in
/// the wrong place" are different failures and the difference is not
/// visible from a bare `NotEnoughMemory`. The message carries the
/// request, how many Untypeds were examined, and the largest amount
/// any of them had left — so a reader can tell exhaustion from a
/// lookup that never saw the big ones.
///
/// # Safety
///
/// As [`leaf_cnode`]: `init_from_regions` must have run.
pub unsafe fn root_untyped_with_room(bytes: u64) -> &'static mut Slot {
    let count = root_untyped_count();
    let mut largest_remaining = 0u64;
    let mut found: Option<usize> = None;

    for index in 0..count {
        // SAFETY: Index below the count, and the caller guarantees the
        // CSpace is built.
        let slot = unsafe { root_untyped(index) };
        // SAFETY: A root slot holds a live Untyped capability.
        let Ok(untyped) = (unsafe { super::untyped::Untyped::from_slot(slot) }) else {
            continue;
        };

        let remaining = untyped.remaining();
        if remaining > largest_remaining {
            largest_remaining = remaining;
        }
        if remaining >= bytes && found.is_none() {
            found = Some(index);
            // Not breaking: the loop also computes `largest_remaining`
            // for the panic below, and a scan of ~100 slots at fixture
            // time costs nothing. Returning early would make the
            // diagnostic worse in exchange for microseconds nobody is
            // waiting on.
        }
    }

    match found {
        // SAFETY: Index came from the loop above, so it is in range.
        Some(index) => unsafe { root_untyped(index) },
        None => panic!(
            "no root Untyped has {} bytes free: {} were examined and the \
             most any of them had left was {}. Either the request is \
             larger than a root Untyped can be (the largest is 2^{} = \
             {} bytes) or earlier fixtures have used the memory up",
            bytes,
            count,
            largest_remaining,
            MAX_ROOT_UNTYPED_BITS,
            1u64 << MAX_ROOT_UNTYPED_BITS
        ),
    }
}

const _: () = {
    assert!(
        MAX_ROOT_UNTYPED_BITS <= UNTYPED_MAX_BITS,
        "root Untypeds cannot be larger than an Untyped may be"
    );
    assert!(
        ROOT_SLOT_UNTYPED < (1 << ROOT_RADIX) && ROOT_SLOT_OBJECTS < (1 << ROOT_RADIX),
        "a reserved root slot index is outside the root CNode"
    );
};
