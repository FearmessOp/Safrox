// SPDX-License-Identifier: BSD-2-Clause
//! Physical frame allocator: a bitmap over all of physical memory.
//!
//! One bit per 4 KiB frame. Set means the frame is in use or must never be
//! handed out; clear means it is free. Starting from "everything is used"
//! and clearing only what Limine marked usable means any hole in the memory
//! map -- including regions Limine did not describe at all -- is excluded
//! by construction rather than by an explicit check that could be forgotten.
//!
//! See docs/adr/007-bitmap-frame-allocator.md for why a bitmap and not a
//! free list, and where the bitmap itself lives.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use limine::memmap;

use super::{PhysAddr, PhysFrame, FRAME_SIZE};
use crate::println;

/// Bits in one bitmap word.
const BITS_PER_WORD: u64 = 64;

/// The allocator's state.
///
/// Lives in a `static mut` guarded by `INITIALISED`, not behind a
/// capability. That is deliberate but temporary: the capability object
/// model does not exist yet, and when it arrives (M5) this becomes the
/// implementation behind Untyped rather than something callable directly.
/// Until then the only callers are in this crate and on the single-threaded
/// boot path with interrupts disabled.
struct FrameAllocator {
    /// One bit per frame, covering frames `0..frame_count`.
    ///
    /// A raw pointer rather than a slice because the storage is carved out
    /// of the memory being managed; there is no Rust allocation behind it
    /// and no lifetime that describes it.
    bitmap: *mut u64,
    /// Number of `u64` words in the bitmap.
    bitmap_words: u64,
    /// Total number of frames the bitmap covers, including unusable ones.
    frame_count: u64,
    /// Frames currently free.
    free_count: u64,
    /// Frames that were free immediately after init. Used by the tests to
    /// show that a free/alloc cycle leaks nothing.
    initial_free_count: u64,
    /// Hint for the next search, so repeated allocation does not rescan the
    /// bitmap from zero every time. Purely an optimisation: correctness
    /// does not depend on it being accurate.
    next_hint: u64,
}

// SAFETY: The raw pointer makes this !Sync by default. Access is confined to
// this module, every entry point goes through `with()`, and M2 runs on one
// CPU with interrupts disabled. When other CPUs arrive this needs a real
// lock -- recorded in docs/adr/007.
unsafe impl Sync for FrameAllocator {}

impl FrameAllocator {
    const fn new() -> Self {
        Self {
            bitmap: core::ptr::null_mut(),
            bitmap_words: 0,
            frame_count: 0,
            free_count: 0,
            initial_free_count: 0,
            next_hint: 0,
        }
    }

    /// Read one bitmap word.
    ///
    /// # Safety
    ///
    /// `word` must be less than `self.bitmap_words`, and the bitmap must be
    /// initialised.
    unsafe fn word(&self, word: u64) -> u64 {
        debug_assert!(word < self.bitmap_words);
        // SAFETY: The caller guarantees the index is in range, so the
        // pointer stays inside the allocation reserved in `init`. The
        // storage was fully written there, so the read is of initialised
        // memory.
        unsafe { self.bitmap.add(word as usize).read() }
    }

    /// Write one bitmap word.
    ///
    /// # Safety
    ///
    /// Same requirements as [`word`](Self::word).
    unsafe fn set_word(&mut self, word: u64, value: u64) {
        debug_assert!(word < self.bitmap_words);
        // SAFETY: Index in range by the caller's guarantee; the region is
        // exclusively owned by the allocator, so there is no aliasing write.
        unsafe { self.bitmap.add(word as usize).write(value) }
    }

    /// Whether a frame is marked used.
    ///
    /// Frames beyond the bitmap read as used, so an out-of-range index can
    /// never be mistaken for free memory.
    ///
    /// # Safety
    ///
    /// The bitmap must be initialised.
    unsafe fn is_used(&self, frame: u64) -> bool {
        if frame >= self.frame_count {
            return true;
        }
        // SAFETY: frame < frame_count, and bitmap_words covers frame_count
        // bits by construction in `init`.
        let word = unsafe { self.word(frame / BITS_PER_WORD) };
        word & (1u64 << (frame % BITS_PER_WORD)) != 0
    }

    /// Mark a frame used. Returns true if it changed state.
    ///
    /// # Safety
    ///
    /// The bitmap must be initialised.
    unsafe fn mark_used(&mut self, frame: u64) -> bool {
        if frame >= self.frame_count {
            return false;
        }
        let index = frame / BITS_PER_WORD;
        let mask = 1u64 << (frame % BITS_PER_WORD);

        // SAFETY: index is in range because frame < frame_count.
        let word = unsafe { self.word(index) };
        if word & mask != 0 {
            return false;
        }
        // SAFETY: as above.
        unsafe { self.set_word(index, word | mask) };
        self.free_count -= 1;
        true
    }

    /// Mark a frame free. Returns true if it changed state.
    ///
    /// # Safety
    ///
    /// The bitmap must be initialised.
    unsafe fn mark_free(&mut self, frame: u64) -> bool {
        if frame >= self.frame_count {
            return false;
        }
        let index = frame / BITS_PER_WORD;
        let mask = 1u64 << (frame % BITS_PER_WORD);

        // SAFETY: index is in range because frame < frame_count.
        let word = unsafe { self.word(index) };
        if word & mask == 0 {
            return false;
        }
        // SAFETY: as above.
        unsafe { self.set_word(index, word & !mask) };
        self.free_count += 1;
        true
    }

    /// Mark every frame overlapping `[start, end)` as used.
    ///
    /// # Safety
    ///
    /// The bitmap must be initialised.
    unsafe fn reserve_range(&mut self, start: PhysAddr, end: PhysAddr) -> u64 {
        let first = PhysFrame::containing(start).0;
        // The last frame is the one containing the final byte, so step back
        // one before dividing -- otherwise an end that is exactly on a frame
        // boundary would reserve one frame too many.
        let last = if end.0 == 0 {
            0
        } else {
            PhysFrame::containing(PhysAddr(end.0 - 1)).0
        };

        let mut reserved = 0;
        let mut frame = first;
        while frame <= last && frame < self.frame_count {
            // SAFETY: frame < frame_count checked by the loop condition.
            if unsafe { self.mark_used(frame) } {
                reserved += 1;
            }
            frame += 1;
        }
        reserved
    }
}

/// The single allocator instance.
///
/// Wrapped in an `UnsafeCell` rather than declared `static mut` so that
/// every access is an explicit pointer operation carrying its own safety
/// argument, instead of a reference whose aliasing rules would have to be
/// defended separately.
static ALLOCATOR: AllocatorCell = AllocatorCell(UnsafeCell::new(FrameAllocator::new()));

/// `static`-compatible storage for the allocator, with no synchronisation.
struct AllocatorCell(UnsafeCell<FrameAllocator>);

// SAFETY: Sound only because of the surrounding context -- one CPU,
// interrupts disabled, and every entry point funnelled through `with`,
// which the module documents as non-reentrant. When other CPUs arrive this
// needs a real lock; see docs/adr/007.
unsafe impl Sync for AllocatorCell {}

/// Set once `init` has finished. Every public entry point checks it, so a
/// call that slips in before initialisation panics with a clear message
/// instead of dereferencing a null bitmap.
static INITIALISED: AtomicBool = AtomicBool::new(false);

/// Set once boot has handed the machine's memory to the capability
/// system. After that the allocator is retired (ADR 022).
///
/// From this point the only way to obtain memory is to retype an
/// Untyped, which means every allocation is charged to some component's
/// budget. An `allocate()` call after retirement is a kernel bug -- a
/// path that was supposed to go through a capability and does not -- so
/// it panics rather than quietly working.
static RETIRED: AtomicBool = AtomicBool::new(false);

/// Frames handed out through the public entry points.
///
/// **Counted at the call, not derived from the bitmap**, and that is
/// the whole point. `free_count` already tells us how many frames are
/// free; a second figure derived from it would agree with it by
/// construction and could not disagree even if a frame had been lost.
///
/// These two are produced by different mechanisms — one by setting
/// bits, one by counting calls — so M12's accounting can hold them up
/// against each other. A frame whose bit was set without going through
/// `allocate` or `allocate_specific`, or a path that took memory twice
/// and counted once, makes them diverge.
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

/// Frames returned through [`free`].
///
/// [`release`] is deliberately *not* counted here: it donates memory
/// the allocator never owned, and it raises `initial_free_count` to
/// match. Counting it as a return would make the balance below come
/// out right for the wrong reason.
static FREES: AtomicU64 = AtomicU64::new(0);

/// How many frames have been handed out, counted at the call sites.
pub fn allocation_count() -> u64 {
    ALLOCATIONS.load(Ordering::Acquire)
}

/// How many frames have been given back, counted at the call sites.
pub fn free_count_calls() -> u64 {
    FREES.load(Ordering::Acquire)
}

/// Nesting depth of interrupt handlers, for the allocation tripwire.
///
/// M2's safety argument for `unsafe impl Sync` had three legs: one CPU,
/// interrupts disabled, and a single non-reentrant entry point. M6 will
/// enable interrupts and knock out the second. Rather than add a lock
/// that protects nothing on a single CPU, the rule becomes explicit:
/// **interrupt handlers do not allocate**, and this counter catches a
/// violation at the moment it happens instead of leaving a corrupted
/// bitmap to be discovered later.
///
/// Not an `AtomicU32` because it is only ever touched from one CPU with
/// interrupts disabled at the point of change; `AtomicUsize` is used for
/// the same reason the other statics here are -- it is the cheapest way
/// to have a mutable static without `unsafe`.
static INTERRUPT_DEPTH: AtomicUsize = AtomicUsize::new(0);

/// Record that an interrupt handler has been entered.
///
/// Paired with [`exit_interrupt`]. Called from the exception dispatcher.
pub fn enter_interrupt() {
    INTERRUPT_DEPTH.fetch_add(1, Ordering::Acquire);
}

/// Record that an interrupt handler has returned.
pub fn exit_interrupt() {
    let previous = INTERRUPT_DEPTH.fetch_sub(1, Ordering::Release);
    debug_assert!(previous > 0, "interrupt depth underflow");
}

/// Whether execution is currently inside an interrupt handler.
pub fn in_interrupt() -> bool {
    INTERRUPT_DEPTH.load(Ordering::Acquire) != 0
}

/// Retire the allocator. Boot calls this after the root Untypeds exist.
///
/// # Safety
///
/// Every frame the kernel will ever need must already have been taken,
/// and the capability system must own the rest.
pub unsafe fn retire() {
    assert!(
        INITIALISED.load(Ordering::Acquire),
        "retiring a frame allocator that was never initialised"
    );
    assert!(
        !RETIRED.swap(true, Ordering::AcqRel),
        "the frame allocator was retired twice"
    );
}

/// Whether the allocator has been retired.
pub fn is_retired() -> bool {
    RETIRED.load(Ordering::Acquire)
}

/// The two checks every allocation path runs.
///
/// Separated out so that the message names which rule was broken; "the
/// allocator panicked" is much less useful than "something allocated
/// from an interrupt handler".
fn check_allocation_allowed() {
    assert!(
        !in_interrupt(),
        "frame allocation from inside an interrupt handler. Handlers must \
         not allocate: there is no sensible behaviour when memory runs out \
         in an interrupt, and the allocator is not reentrant. See ADR 022."
    );
    assert!(
        !RETIRED.load(Ordering::Acquire),
        "frame allocation after the allocator was retired. From the end of \
         boot, memory comes only from retyping an Untyped, so that every \
         allocation is charged to a budget. See ADR 022."
    );
}

/// Run `f` with exclusive access to the allocator.
///
/// # Safety
///
/// The caller must not already be inside `with`: re-entering would create
/// two `&mut` to the same static.
unsafe fn with<R>(f: impl FnOnce(&mut FrameAllocator) -> R) -> R {
    assert!(
        INITIALISED.load(Ordering::Acquire),
        "frame allocator used before memory::init()"
    );
    // SAFETY: The caller guarantees no re-entry, and M2 runs on a single
    // CPU with interrupts disabled, so no other context can be holding a
    // reference. The cell is initialised, checked just above.
    unsafe { f(&mut *ALLOCATOR.0.get()) }
}

// ---- Symbols from the linker script ------------------------------------

unsafe extern "C" {
    static __kernel_image_start: u8;
    static __kernel_image_end: u8;
}

/// Build the bitmap and populate it from the memory map.
///
/// # Safety
///
/// Call exactly once, before any frame is allocated, with the memory map
/// Limine produced. The kernel must still be running on the direct map
/// Limine set up, because the bitmap is written through it.
pub unsafe fn init(entries: &[&memmap::Entry]) {
    assert!(
        !INITIALISED.load(Ordering::Acquire),
        "frame allocator initialised twice"
    );

    // --- How many frames must the bitmap cover? -------------------------
    //
    // Up to the highest address of any frame the allocator may *ever*
    // manage. Reserved regions above that need no bits: nothing will ask
    // about them, and `is_used` reports out-of-range frames as used anyway.
    //
    // "Ever" is doing real work in that sentence. M2 sized this by the
    // highest USABLE address, which was correct while usable memory was
    // the only kind the allocator handled. M3's reclaim then donates the
    // bootloader-reclaimable regions -- and on the test machine the last
    // of those, 0x0ff45000..0x0ffdf000, starts exactly where the last
    // usable region ends. Every frame in it was out of range, and the
    // first `release` panicked with "out-of-range frame 65349".
    //
    // Sizing by both kinds costs a few hundred bytes of bitmap and makes
    // the bound a property of what the allocator can be given rather than
    // of what it starts with.
    let mut highest_managed_end: u64 = 0;
    let mut highest_usable_end: u64 = 0;
    for entry in entries {
        match entry.type_ {
            memmap::MEMMAP_USABLE => {
                highest_usable_end = highest_usable_end.max(entry.base + entry.length);
                highest_managed_end = highest_managed_end.max(entry.base + entry.length);
            }
            // Not released here -- ADR 008 keeps it until the kernel is off
            // the bootloader's tables and stack -- but it is released
            // eventually, so it needs bits.
            memmap::MEMMAP_BOOTLOADER_RECLAIMABLE => {
                highest_managed_end = highest_managed_end.max(entry.base + entry.length);
            }
            _ => {}
        }
    }
    assert!(
        highest_usable_end > 0,
        "memory map contains no usable regions"
    );

    let frame_count = highest_managed_end.div_ceil(FRAME_SIZE);
    let bitmap_words = frame_count.div_ceil(BITS_PER_WORD);
    let bitmap_bytes = bitmap_words * 8;

    // --- Where does the bitmap go? --------------------------------------
    //
    // In the memory it manages. The kernel performs no allocation of its
    // own, and a static array would have to be sized for the largest
    // machine we might ever boot on -- wasting that much image on every
    // smaller one. So: find the first usable region big enough, put the
    // bitmap at its start, and then mark those frames used so the allocator
    // cannot hand its own metadata out.
    let mut bitmap_phys: Option<u64> = None;
    for entry in entries {
        if entry.type_ == memmap::MEMMAP_USABLE && entry.length >= bitmap_bytes {
            bitmap_phys = Some(entry.base);
            break;
        }
    }
    let bitmap_phys = bitmap_phys.expect("no usable region large enough to hold the frame bitmap");

    // The bitmap is reached through Limine's higher-half direct map: the
    // kernel runs with paging on and cannot touch a physical address
    // directly. HHDM is a loan from the bootloader for now; M3 replaces it
    // with page tables we build ourselves.
    let hhdm_offset = super::hhdm_offset();
    let bitmap_virt = hhdm_offset + bitmap_phys;

    println!(
        "[mem] bitmap: {} KiB for {} frames, at phys {:#018x} (virt {:#018x})",
        bitmap_bytes / 1024,
        frame_count,
        bitmap_phys,
        bitmap_virt
    );

    // SAFETY: `bitmap_virt` is inside Limine's direct map, which covers all
    // physical memory and is writable; `bitmap_phys` came from a usable
    // entry with at least `bitmap_bytes` to spare, so the whole range is
    // backed. Nothing else owns this memory: the allocator does not exist
    // yet, so nothing can have handed it out. The pointer is 8-byte aligned
    // because Limine's usable regions start on a page boundary.
    let bitmap = bitmap_virt as *mut u64;

    // SAFETY: Exclusive access -- no other reference to the allocator can
    // exist, because INITIALISED is still false and `with` refuses to run
    // until it is set at the end of this function.
    let allocator = unsafe { &mut *ALLOCATOR.0.get() };
    allocator.bitmap = bitmap;
    allocator.bitmap_words = bitmap_words;
    allocator.frame_count = frame_count;
    allocator.free_count = 0;
    allocator.next_hint = 0;

    // Start from "every frame is used". Clearing only what the memory map
    // calls usable means gaps -- addresses no entry describes -- stay
    // reserved without needing a separate pass to find them.
    for word in 0..bitmap_words {
        // SAFETY: word < bitmap_words, and the backing storage is that
        // many words long.
        unsafe { allocator.set_word(word, u64::MAX) };
    }

    // --- Release the usable regions -------------------------------------
    //
    // Only whole frames: a region that starts or ends mid-frame contributes
    // only the frames it fully covers, because handing out a partial frame
    // would give the caller memory that is not theirs.
    let mut released: u64 = 0;
    for entry in entries {
        if entry.type_ != memmap::MEMMAP_USABLE {
            continue;
        }

        let first = entry.base.div_ceil(FRAME_SIZE);
        let last_exclusive = (entry.base + entry.length) / FRAME_SIZE;

        let mut frame = first;
        while frame < last_exclusive && frame < frame_count {
            // SAFETY: frame < frame_count, checked by the loop condition.
            if unsafe { allocator.mark_free(frame) } {
                released += 1;
            }
            frame += 1;
        }
    }

    // --- Take back what must never be handed out ------------------------

    // Frame 0. The real-mode IVT, the BIOS data area and assorted firmware
    // structures live in the first few KiB, and a null physical address is
    // a useful sentinel. Some of this may be marked usable; we exclude it
    // regardless.
    //
    // Reference: OSDev wiki, "Memory Map (x86)".
    // SAFETY: Frame 0 is within the bitmap whenever frame_count > 0, which
    // the assertion above guarantees.
    let frame_zero_taken = unsafe { allocator.mark_used(0) };

    // The kernel image. Limine marks it EXECUTABLE_AND_MODULES rather than
    // usable, so this is belt and braces -- but the cost of being wrong is
    // the allocator handing out the memory holding its own code.
    //
    // SAFETY: These are linker-defined symbols; only their addresses are
    // read, never the storage behind them.
    let (image_start, image_end) = unsafe {
        (
            core::ptr::from_ref(&__kernel_image_start) as u64,
            core::ptr::from_ref(&__kernel_image_end) as u64,
        )
    };
    // The image addresses are virtual; convert to physical using the slide
    // the bootloader reported.
    let image_phys_start = super::kernel_virt_to_phys(image_start);
    let image_phys_end = super::kernel_virt_to_phys(image_end);

    // SAFETY: The bitmap is initialised and the range is clamped to
    // frame_count inside reserve_range.
    let image_frames =
        unsafe { allocator.reserve_range(PhysAddr(image_phys_start), PhysAddr(image_phys_end)) };

    // Whether or not the pass above changed any bits, every frame of the
    // image must now read as used. In practice Limine already marks the
    // image EXECUTABLE_AND_MODULES, so `reserve_range` usually finds the
    // bits already set and reports zero -- which looks indistinguishable
    // from "we forgot to reserve it". Assert the property itself rather
    // than trusting the count.
    let image_first = PhysFrame::containing(PhysAddr(image_phys_start)).0;
    let image_last = PhysFrame::containing(PhysAddr(image_phys_end - 1)).0;
    let mut frame = image_first;
    while frame <= image_last {
        // SAFETY: The bitmap was fully written above, and `is_used`
        // reports out-of-range frames as used rather than reading past
        // the end, so no bounds check is needed here.
        let used = unsafe { allocator.is_used(frame) };
        assert!(
            used,
            "kernel image frame {} is free; the allocator could hand out its own code",
            frame
        );
        frame += 1;
    }

    // The bitmap itself.
    // SAFETY: As above.
    let bitmap_frames = unsafe {
        allocator.reserve_range(PhysAddr(bitmap_phys), PhysAddr(bitmap_phys + bitmap_bytes))
    };

    allocator.initial_free_count = allocator.free_count;

    println!(
        "[mem] frames: {} released; image {}..={} ({} newly reserved, rest already \
         marked by the memory map), bitmap {} reserved{}",
        released,
        image_first,
        image_last,
        image_frames,
        bitmap_frames,
        if frame_zero_taken {
            ", frame 0 reserved"
        } else {
            ", frame 0 already reserved"
        }
    );
    println!(
        "[mem] image frames verified in use: {}..={}",
        image_first, image_last
    );
    println!(
        "[mem] allocator ready: {} free frames ({} MiB)",
        allocator.free_count,
        allocator.free_count * FRAME_SIZE / (1024 * 1024)
    );

    INITIALISED.store(true, Ordering::Release);
}

/// Take a free frame, or `None` if none are left.
///
/// Returning `None` rather than panicking is the point: running out of
/// memory is an ordinary condition that a caller may be able to handle, not
/// a kernel bug. Nothing in a kernel path may assume allocation succeeds.
pub fn allocate() -> Option<PhysFrame> {
    // Two rules, both from ADR 022: handlers do not allocate, and the
    // allocator is retired once the capability system owns memory.
    // Checked before anything else so the panic names the rule rather
    // than the symptom.
    check_allocation_allowed();

    // SAFETY: Not called from inside `with`; see the module-level note on
    // single-CPU, interrupts-disabled operation.
    unsafe {
        with(|allocator| {
            // Two passes: from the hint to the end, then from the start to
            // the hint. Bounded by the bitmap size either way -- no kernel
            // path may contain an unbounded loop.
            //
            // `find_free` and `mark_used` require an initialised bitmap,
            // which `with` asserted; the enclosing unsafe block carries
            // that argument.
            let found = find_free(allocator, allocator.next_hint, allocator.frame_count)
                .or_else(|| find_free(allocator, 0, allocator.next_hint))?;

            let changed = allocator.mark_used(found);
            debug_assert!(changed, "allocator picked a frame that was already used");

            allocator.next_hint = found + 1;
            ALLOCATIONS.fetch_add(1, Ordering::AcqRel);
            Some(PhysFrame(found))
        })
    }
}

/// Scan `[from, to)` for a clear bit.
///
/// # Safety
///
/// The bitmap must be initialised.
unsafe fn find_free(allocator: &FrameAllocator, from: u64, to: u64) -> Option<u64> {
    if from >= to {
        return None;
    }

    let first_word = from / BITS_PER_WORD;
    let last_word = (to - 1) / BITS_PER_WORD;

    for index in first_word..=last_word.min(allocator.bitmap_words.saturating_sub(1)) {
        // SAFETY: index is clamped to bitmap_words - 1.
        let word = unsafe { allocator.word(index) };
        if word == u64::MAX {
            continue; // every frame in this word is taken
        }

        // trailing_ones gives the index of the first clear bit directly;
        // the alternative is a bit-by-bit loop over 64 positions.
        let bit = word.trailing_ones() as u64;
        let frame = index * BITS_PER_WORD + bit;

        if frame >= from && frame < to {
            return Some(frame);
        }

        // The first clear bit in this word fell outside the window; walk
        // the remaining bits of the word individually.
        for bit in (bit + 1)..BITS_PER_WORD {
            if word & (1u64 << bit) == 0 {
                let frame = index * BITS_PER_WORD + bit;
                if frame >= from && frame < to {
                    return Some(frame);
                }
            }
        }
    }

    None
}
/// Claim one specific frame, if it is free.
///
/// Returns whether the frame was taken. The ordinary `allocate()` picks
/// a frame for the caller; this one is for boot code that must claim a
/// *particular* physical address -- building the root Untypeds needs
/// aligned, contiguous blocks, which "give me any frame" cannot express.
///
/// Boot-time only, like every other allocation path here (ADR 022).
pub fn allocate_specific(frame: PhysFrame) -> bool {
    check_allocation_allowed();

    // SAFETY: Not called from inside `with`.
    unsafe {
        with(|allocator| {
            if frame.0 >= allocator.frame_count {
                return false;
            }
            // The bitmap covers frame_count bits, and the index is in
            // range; the enclosing unsafe block carries the argument.
            if allocator.is_used(frame.0) {
                return false;
            }
            let changed = allocator.mark_used(frame.0);
            debug_assert!(changed, "mark_used disagreed with is_used");
            ALLOCATIONS.fetch_add(1, Ordering::AcqRel);
            true
        })
    }
}

/// Return a frame to the allocator.
///
/// # Panics
///
/// If the frame is already free. A double free is not a recoverable
/// situation: the second free would put the same frame on the free list
/// twice, and two later allocations would hand the same memory to two
/// owners who each believe they have it exclusively. The resulting
/// corruption appears arbitrarily far from the cause, so the only useful
/// response is to stop here, where the evidence is.
pub fn free(frame: PhysFrame) {
    // SAFETY: Not called from inside `with`.
    unsafe {
        with(|allocator| {
            assert!(
                frame.0 < allocator.frame_count,
                "free of out-of-range {}",
                frame
            );

            // The range check above is what makes these calls sound; the
            // enclosing `unsafe` block carries the SAFETY comment.
            if !allocator.is_used(frame.0) {
                panic!("double free detected: {} is already free", frame);
            }

            let changed = allocator.mark_free(frame.0);
            debug_assert!(changed, "mark_free disagreed with is_used");
            FREES.fetch_add(1, Ordering::AcqRel);

            // Reuse freed memory first: it is more likely to be in cache,
            // and it keeps the bitmap's low end dense.
            if frame.0 < allocator.next_hint {
                allocator.next_hint = frame.0;
            }
        });
    }
}

/// Hand a frame to the allocator that it has never managed before.
///
/// Distinct from [`free`] on purpose. `free` is the counterpart of
/// `allocate` and treats an already-free frame as a double free, which is
/// a bug. This is for memory that was excluded from the start and is now
/// being donated -- the bootloader's, once ADR 013's gates are satisfied.
/// The frame is expected to be marked used, because every frame outside a
/// USABLE region starts that way.
///
/// # Panics
///
/// If the frame is already free. That would mean the allocator already
/// owned it, which makes this a double donation.
pub fn release(frame: PhysFrame) {
    // SAFETY: Not called from inside `with`.
    unsafe {
        with(|allocator| {
            assert!(
                frame.0 < allocator.frame_count,
                "release of out-of-range {}",
                frame
            );

            // Range checked above; the enclosing unsafe block carries the
            // argument.
            assert!(
                allocator.is_used(frame.0),
                "release of {}, which the allocator already considers free",
                frame
            );

            let changed = allocator.mark_free(frame.0);
            debug_assert!(changed, "mark_free disagreed with is_used");

            // Donated memory joins the pool, so the baseline moves with it
            // -- otherwise the leak check in memtest would see the extra
            // frames as a discrepancy.
            allocator.initial_free_count += 1;

            if frame.0 < allocator.next_hint {
                allocator.next_hint = frame.0;
            }
        });
    }
}

/// Number of frames currently free.
pub fn free_count() -> u64 {
    // SAFETY: Not called from inside `with`.
    unsafe { with(|allocator| allocator.free_count) }
}

/// Number of frames that were free immediately after initialisation.
pub fn initial_free_count() -> u64 {
    // SAFETY: Not called from inside `with`.
    unsafe { with(|allocator| allocator.initial_free_count) }
}

/// Total number of frames the bitmap covers, usable or not.
pub fn frame_count() -> u64 {
    // SAFETY: Not called from inside `with`.
    unsafe { with(|allocator| allocator.frame_count) }
}

/// Whether a frame is currently marked used.
///
/// Exposed for the acceptance tests, which check that no allocated frame
/// overlaps the kernel image or frame 0.
pub fn is_used(frame: PhysFrame) -> bool {
    // SAFETY: Not called from inside `with`.
    unsafe { with(|allocator| allocator.is_used(frame.0)) }
}
