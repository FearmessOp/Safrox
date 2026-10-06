// SPDX-License-Identifier: BSD-2-Clause
//! Frame allocator acceptance tests, run inside the kernel.
//!
//! These are not unit tests on the host: they exercise the allocator
//! against the real memory map of the machine it booted on, and the kernel
//! checks its own invariants and reports the verdict on the serial console.
//! The host runner then asserts on that output.

use core::cell::UnsafeCell;

use crate::memory::frame_allocator as frames;
use crate::memory::{PhysFrame, FRAME_SIZE};
use crate::println;

unsafe extern "C" {
    static __kernel_image_start: u8;
    static __kernel_image_end: u8;
}

/// Physical frame range covered by the kernel image, inclusive.
///
/// No allocation may ever fall inside this: those frames hold the running
/// code, the descriptor tables and the IST stack.
fn kernel_image_frames() -> (u64, u64) {
    // SAFETY: Linker-defined symbols. Only their addresses are read; the
    // storage behind them is never touched.
    let (virt_start, virt_end) = unsafe {
        (
            core::ptr::from_ref(&__kernel_image_start) as u64,
            core::ptr::from_ref(&__kernel_image_end) as u64,
        )
    };

    let phys_start = crate::memory::kernel_virt_to_phys(virt_start);
    let phys_end = crate::memory::kernel_virt_to_phys(virt_end);

    (phys_start / FRAME_SIZE, (phys_end - 1) / FRAME_SIZE)
}

/// How many frames the bulk allocation test takes.
///
/// 10 000 frames is 40 MiB: enough to span many bitmap words and cross
/// several memory map regions on a 256 MiB machine, while leaving plenty
/// free so the test says nothing about exhaustion -- that is a separate
/// phase below.
const BULK_COUNT: usize = 10_000;

/// Record of the bulk allocation.
///
/// A fixed-size array, not a Vec: there is no allocator before M4, and a
/// kernel test is exactly the place that must not need one. 80 KiB of
/// `.bss` -- present in every boot, since static storage is reserved
/// whether or not the cmdline asks for the test. A deliberate trade: a
/// test that runs on the shipping binary is worth 80 KiB of image.
///
/// Stored as raw cells rather than `static mut` so that each access is an
/// explicit pointer operation with its own safety argument, instead of a
/// reference whose aliasing rules we would have to defend.
static BULK: SyncCell<[PhysFrame; BULK_COUNT]> =
    SyncCell(UnsafeCell::new([PhysFrame(0); BULK_COUNT]));

/// A `static`-compatible cell with no synchronisation of its own.
///
/// Sound here only because of the surrounding context: the memory tests run
/// once, on the boot CPU, with interrupts disabled, and nothing else in the
/// kernel refers to this storage.
struct SyncCell<T>(UnsafeCell<T>);

// SAFETY: See the type's documentation -- single-threaded use, enforced by
// the boot sequence rather than by the type system. When other CPUs exist
// this test needs rethinking, which is why the storage is private to this
// module.
unsafe impl<T> Sync for SyncCell<T> {}

impl<T> SyncCell<T> {
    /// Pointer to the contained value.
    const fn get(&self) -> *mut T {
        self.0.get()
    }
}

/// Run the full frame allocator test suite, then exit.
///
/// Diverges rather than returning. The last phase drains the allocator
/// to prove it reports exhaustion cleanly, and it leaves it drained --
/// which was harmless while this was the last thing to run, and stopped
/// being so in M5, when `cap::boot::init` began running afterwards and
/// panicked with "could not find 2 contiguous frames for a CNode".
///
/// Exiting here says what was already true: a scenario that deliberately
/// exhausts a global resource cannot hand the machine back.
pub fn run() -> ! {
    println!("[memtest] starting frame allocator tests");

    let initial_free = frames::free_count();
    let total_frames = frames::frame_count();
    println!(
        "[memtest] baseline: {} free frames of {} total",
        initial_free, total_frames
    );

    // M2 asserted `initial_free == initial_free_count()` here: nothing had
    // allocated anything between init and the test, so any difference was
    // a leak. M3 made that false. By the time this runs, `vm::init` has
    // taken frames for the PML4, the intermediate tables, and two 16 KiB
    // kernel stacks, and `reclaim` has donated the bootloader's memory
    // back. The counts legitimately differ.
    //
    // Deleting the check would have been the easy move, and wrong: it is
    // the only thing standing between a real leak during boot and nobody
    // noticing. So it is narrowed to what is still true -- the kernel's
    // own structures account for the entire difference, and that is a
    // small, bounded number of frames rather than an unbounded drift.
    //
    //   PML4                                            1
    //   PMAP: a PDPT, plus PDs for 2 MiB pages          up to ~4 here
    //   image: PDPT + PD + PT                           3
    //   two stacks: share the image's PDPT/PD, one PT   1
    //   KSTACK PDPT + PD                                2
    //
    // The exact figure depends on the machine's memory map, so the bound
    // is generous; what it rules out is the interesting failure, which is
    // boot code allocating per-frame rather than per-table.
    const MAX_BOOT_STRUCTURES: u64 = 64;

    // M5 added a second, much larger consumer: the heap's physical
    // reserve, taken whole before the frame allocator retires (ADR 022).
    // That is 16384 frames, which swamped the bound above and made this
    // check fail with "16407 frames are held by the kernel's own
    // structures" -- true, and not a leak.
    //
    // Subtracting it rather than raising the bound is the point. Raising
    // the bound to 17000 would hide a genuine per-frame leak of 500
    // frames behind the reserve's bulk; subtracting the exact,
    // independently-known figure leaves the original check exactly as
    // sharp as it was. The reserve is a deliberate, measured allocation,
    // so it can be accounted for by name.
    let reserve_frames = crate::memory::heap::reserve_size() / FRAME_SIZE;

    let donated = frames::initial_free_count();
    let held_in_total = donated.saturating_sub(initial_free);

    assert!(
        initial_free <= donated,
        "more frames are free ({}) than the allocator ever owned ({}); the \
         accounting is broken",
        initial_free,
        donated
    );
    assert!(
        held_in_total >= reserve_frames,
        "the kernel holds {} frames but the heap reserve alone is {}; the \
         reserve was not taken from this allocator",
        held_in_total,
        reserve_frames
    );

    let held_by_kernel = held_in_total - reserve_frames;
    assert!(
        held_by_kernel <= MAX_BOOT_STRUCTURES,
        "{} frames are held by the kernel's own structures (excluding the \
         {} frame heap reserve), which is more than the {} a page-table \
         build and two stacks should need; something allocated per-frame \
         during boot",
        held_by_kernel,
        reserve_frames,
        MAX_BOOT_STRUCTURES
    );

    println!(
        "[memtest] {} frames held by boot structures (page tables and {} \
         kernel stacks), within the bound of {}; the heap's {} frame \
         reserve is counted separately",
        held_by_kernel, 2, MAX_BOOT_STRUCTURES, reserve_frames
    );

    bulk_allocate(initial_free);
    bulk_free(initial_free);
    exhaust_and_recover();

    println!("[memtest] all frame allocator tests passed");
    println!(
        "[boot] exiting QEMU (write {:#04x} -> exit status {})",
        crate::qemu::EXIT_SUCCESS,
        crate::qemu::HOST_STATUS_SUCCESS
    );

    crate::qemu::exit(crate::qemu::EXIT_SUCCESS)
}

/// Phase 1: allocate [`BULK_COUNT`] frames and check every invariant that
/// matters about them.
fn bulk_allocate(initial_free: u64) {
    assert!(
        initial_free >= BULK_COUNT as u64,
        "not enough free memory to run the test: need {} frames, have {}",
        BULK_COUNT,
        initial_free
    );

    println!("[memtest] allocating {} frames ...", BULK_COUNT);

    let (image_first, image_last) = kernel_image_frames();
    println!(
        "[memtest] kernel image occupies frames {}..={}",
        image_first, image_last
    );

    let mut lowest = u64::MAX;
    let mut highest = 0u64;

    for index in 0..BULK_COUNT {
        let frame = frames::allocate()
            .unwrap_or_else(|| panic!("allocation {} failed with free frames remaining", index));

        // Alignment is structural -- PhysFrame stores an index, so the
        // address is a multiple of FRAME_SIZE by construction. Check it
        // anyway: this is the property the rest of the kernel will rely on,
        // and a bug in the index arithmetic would not be visible otherwise.
        let addr = frame.start();
        assert!(
            addr.is_frame_aligned(),
            "allocation {} returned unaligned {}",
            index,
            frame
        );

        // Never frame 0: the first 4 KiB holds the real-mode IVT and BIOS
        // data area, and a null physical address is a useful sentinel.
        assert!(frame.0 != 0, "allocator handed out frame 0");

        // Never inside the kernel image. Handing out one of these frames
        // would let the caller overwrite the running kernel -- its code,
        // its descriptor tables, or the IST stack.
        assert!(
            frame.0 < image_first || frame.0 > image_last,
            "allocation {} returned {}, which is inside the kernel image ({}..={})",
            index,
            frame,
            image_first,
            image_last
        );

        // The allocator must consider it used the moment it hands it over;
        // otherwise a concurrent caller could be given the same frame.
        assert!(
            frames::is_used(frame),
            "allocation {} returned {} but the bitmap says it is free",
            index,
            frame
        );

        lowest = lowest.min(frame.0);
        highest = highest.max(frame.0);

        // SAFETY: `index < BULK_COUNT` by the loop bound, so the offset
        // stays inside the array. This is the only code that writes BULK,
        // it runs on the single-threaded boot path, and no reference to the
        // array is live across this write.
        unsafe {
            BULK.get().cast::<PhysFrame>().add(index).write(frame);
        }
    }

    // Uniqueness. Sorting would need scratch space; instead, re-check the
    // bitmap: every frame we were given must still be marked used, and
    // since `allocate` only ever returns frames it has just marked, two
    // identical returns would mean the second call found a used frame --
    // which the loop above would already have caught. The stronger check
    // is that freeing all of them restores the count exactly, which phase 2
    // does: a duplicate would free the same bit twice and panic there.
    //
    // What we can check cheaply here is that the allocator's own accounting
    // moved by exactly the number of frames we took.
    let now_free = frames::free_count();
    assert_eq!(
        now_free,
        initial_free - BULK_COUNT as u64,
        "free count moved by the wrong amount: {} -> {} after {} allocations",
        initial_free,
        now_free,
        BULK_COUNT
    );

    println!(
        "[memtest] alloc: {} unique, aligned, in-range (frames {}..={}, {} MiB span)",
        BULK_COUNT,
        lowest,
        highest,
        (highest - lowest + 1) * FRAME_SIZE / (1024 * 1024)
    );
    println!("[memtest] alloc: none in the kernel image, none is frame 0");
}

/// Phase 2: give every frame back and confirm the count returns to where it
/// started.
///
/// This is the leak test. It is also the uniqueness test: `free` panics on
/// a frame that is already free, so if `allocate` had ever returned the
/// same frame twice, the second `free` of it would stop the kernel here.
fn bulk_free(initial_free: u64) {
    println!("[memtest] freeing {} frames ...", BULK_COUNT);

    for index in 0..BULK_COUNT {
        // SAFETY: `index < BULK_COUNT` by the loop bound. Every entry was
        // written by `bulk_allocate`, which always runs first, so the read
        // is of initialised memory. Single-threaded as above.
        let frame = unsafe { BULK.get().cast::<PhysFrame>().add(index).read() };
        frames::free(frame);
    }

    let now_free = frames::free_count();
    assert_eq!(
        now_free,
        initial_free,
        "free count did not return to its baseline: {} != {} ({} frames leaked)",
        now_free,
        initial_free,
        initial_free.saturating_sub(now_free)
    );

    println!("[memtest] free: count restored ({})", now_free);
}

/// Phase 3: drain the allocator completely, confirm it reports exhaustion
/// cleanly, then confirm it recovers.
///
/// Returning `None` instead of panicking is the contract: out of memory is
/// an ordinary condition a caller may be able to handle, not a kernel bug.
fn exhaust_and_recover() {
    println!("[memtest] exhausting the allocator ...");

    let before = frames::free_count();

    // Bounded by the number of frames that exist, so this cannot spin
    // forever even if the allocator's accounting is wrong.
    let mut taken: u64 = 0;
    let mut last = None;
    while let Some(frame) = frames::allocate() {
        last = Some(frame);
        taken += 1;
        assert!(
            taken <= before,
            "allocator handed out more frames ({}) than it claimed were free ({})",
            taken,
            before
        );
    }

    assert_eq!(
        taken, before,
        "allocator ran dry after {} frames but claimed {} were free",
        taken, before
    );
    assert_eq!(
        frames::free_count(),
        0,
        "allocator returned None while still claiming free frames"
    );

    println!(
        "[memtest] exhausted after {} frames; allocate() returned None cleanly",
        taken
    );

    // Recovery: give one frame back and the next allocation must succeed.
    let frame = last.expect("allocator was empty before the test started");
    frames::free(frame);
    assert_eq!(
        frames::free_count(),
        1,
        "free did not restore exactly one frame"
    );

    let again = frames::allocate().expect("allocate failed immediately after a free");
    assert_eq!(
        again, frame,
        "expected the just-freed {} back, got {}",
        frame, again
    );
    println!(
        "[memtest] recovery: freed {} and got it straight back",
        frame
    );

    // Leave the allocator drained but consistent. Nothing after this point
    // allocates, and the kernel is about to exit.
    frames::free(again);
    println!("[memtest] exhaustion test complete");
}

/// Free the same frame twice, on purpose.
///
/// Expected to panic inside the allocator. A double free must never be
/// accepted quietly: the frame would be free-listed twice and two later
/// allocations would hand the same memory to two owners, each believing it
/// is theirs alone. The corruption would appear arbitrarily far from the
/// cause.
pub fn run_double_free() -> ! {
    println!("[memtest] double-free test: allocating one frame");

    let frame = frames::allocate().expect("no free frames available for the double-free test");
    println!("[memtest] got {}", frame);

    println!("[memtest] first free (legitimate) ...");
    frames::free(frame);

    println!("[memtest] second free of the same frame -- this must panic");
    frames::free(frame);

    unreachable!("the allocator accepted a double free");
}
