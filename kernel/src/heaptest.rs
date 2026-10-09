// SPDX-License-Identifier: MPL-2.0
//! Kernel heap acceptance tests, run inside the kernel.
//!
//! Like `memtest` and `vmtest`, these run against the real heap on the real
//! machine; the kernel checks its own invariants and the host runner
//! asserts on the serial output.

use alloc::alloc::{alloc, dealloc};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::alloc::Layout;

use crate::memory::heap;
use crate::memory::layout as vmlayout;
use crate::println;

/// How many allocations the stress phase holds live at once.
const STRESS_COUNT: usize = 10_000;

/// Largest block the stress phase asks for.
///
/// 4 KiB, not the 64 KiB the milestone's wording suggests, and the
/// arithmetic is why. The size classes are logarithmic, so cycling
/// through them evenly puts 1/n of the allocations in the largest class --
/// and that class alone then dominates the total. With a 64 KiB maximum:
///
///   10000 allocations / 13 classes = 769 in the 64 KiB class
///   769 x 64 KiB = 48 MiB, from one class, against a 64 MiB heap
///
/// The run hit the limit three times before the arithmetic was done
/// rather than guessed. With a 4 KiB maximum the same sweep holds about
/// 10 MiB live, which exercises every class from 16 B upwards and leaves
/// the heap well clear of its limit.
///
/// Sizes above this are not untested: `check_growth` allocates up to
/// 512 KiB in one block, and `run_oom` works in 1 MiB chunks.
const STRESS_MAX_BYTES: usize = 4 * 1024;

/// Byte written either side of a block to detect an overlap.
///
/// Two different values, so that a block that runs off its *end* into the
/// next one's *start* is distinguishable from the reverse.
const CANARY_HEAD: u8 = 0xC5;
const CANARY_TAIL: u8 = 0x3A;

/// Run the heap test suite.
pub fn run() {
    println!("[heaptest] starting kernel heap tests");

    check_initial_state();
    check_collections();
    let baseline = check_stress();
    check_growth();
    check_returned_to_baseline(baseline);

    println!("[heaptest] all kernel heap tests passed");
}

/// 1. The heap exists and reports where it is.
fn check_initial_state() {
    assert!(heap::ready(), "the heap is not initialised");

    let stats = heap::stats();

    assert!(
        stats.mapped_pages >= 1,
        "the heap has no pages mapped; init() should map at least one"
    );
    assert!(
        stats.frontier >= vmlayout::KHEAP_START,
        "the frontier {:#018x} is below the heap's start {:#018x}",
        stats.frontier,
        vmlayout::KHEAP_START
    );
    assert!(
        stats.frontier <= vmlayout::KHEAP_END,
        "the frontier {:#018x} is past the heap's limit {:#018x}",
        stats.frontier,
        vmlayout::KHEAP_END
    );

    println!(
        "[heaptest] initial: base {:#018x}, limit {} MiB, {} page(s) mapped, \
         {} bytes in use",
        vmlayout::KHEAP_START,
        vmlayout::KHEAP_MAX_SIZE / (1024 * 1024),
        stats.mapped_pages,
        stats.used_requested
    );
}

/// 2. The four collections the rest of the kernel will want.
///
/// Not a smoke test: each one exercises a different allocation pattern.
/// `Box` is a single fixed allocation; `Vec` reallocates as it grows;
/// `String` does the same for bytes; `BTreeMap` allocates many small nodes
/// and frees them in an order unrelated to allocation.
fn check_collections() {
    // --- Box ------------------------------------------------------------
    let boxed = Box::new(0x5047_4E49_5041_4D56u64);
    assert_eq!(
        *boxed, 0x5047_4E49_5041_4D56,
        "a Box did not read back the value put into it"
    );
    let boxed_addr = &raw const *boxed as u64;
    assert!(
        (vmlayout::KHEAP_START..vmlayout::KHEAP_END).contains(&boxed_addr),
        "a Box landed at {:#018x}, outside the heap",
        boxed_addr
    );
    drop(boxed);

    // --- Vec, grown past several reallocations --------------------------
    let mut numbers: Vec<u64> = Vec::new();
    for value in 0..1000u64 {
        numbers.push(value * value);
    }
    assert_eq!(numbers.len(), 1000);
    for (index, &value) in numbers.iter().enumerate() {
        assert_eq!(
            value,
            (index as u64) * (index as u64),
            "Vec element {} is wrong after reallocation",
            index
        );
    }

    // --- String ----------------------------------------------------------
    let mut text = String::new();
    for index in 0..200 {
        text.push_str(if index % 2 == 0 { "ab" } else { "cd" });
    }
    assert_eq!(text.len(), 400, "String length is wrong");
    assert!(text.starts_with("abcd"), "String content is wrong");
    assert!(text.ends_with("abcd"), "String content is wrong at the end");

    // --- BTreeMap: many small nodes, freed out of order -----------------
    let mut map: BTreeMap<u64, u64> = BTreeMap::new();
    for key in 0..500u64 {
        map.insert(key, key.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    }
    assert_eq!(map.len(), 500);
    for key in 0..500u64 {
        assert_eq!(
            map.get(&key),
            Some(&key.wrapping_mul(0x9E37_79B9_7F4A_7C15)),
            "BTreeMap lost or corrupted key {}",
            key
        );
    }
    // Remove half, in an order that is not insertion order.
    for key in (0..500u64).step_by(2) {
        assert!(
            map.remove(&key).is_some(),
            "BTreeMap could not remove {}",
            key
        );
    }
    assert_eq!(
        map.len(),
        250,
        "BTreeMap has the wrong length after removals"
    );

    println!(
        "[heaptest] collections: Box, Vec(1000), String(400), BTreeMap(500) \
         all allocate and read back correctly"
    );
}

/// One allocation made by the stress phase, with its canaries.
struct Block {
    ptr: *mut u8,
    layout: Layout,
}

/// 3. Ten thousand allocations across every size class, checked for
///    alignment and for overlap.
///
/// Returns the byte count to come back to in check 5.
fn check_stress() -> usize {
    let baseline = heap::stats().used_requested;

    // Fixed-size storage: there is no second heap to hold the record, and
    // using the heap under test to track the heap under test would make a
    // failure much harder to read.
    let mut blocks: Vec<Block> = Vec::with_capacity(STRESS_COUNT);

    // Cycle through the classes, picking an odd size inside each.
    //
    // Two earlier versions of this got the distribution wrong and ran the
    // heap out of memory -- see STRESS_MAX_BYTES. The index is derived
    // from the byte count rather than written down, because writing "16"
    // for "64 KiB" is exactly the mistake that was made.
    //
    // Classes hold 2^(4+i) bytes, so the class for N bytes is
    // log2(N) - log2(16), and the count is that plus one.
    const STRESS_CLASSES: usize =
        STRESS_MAX_BYTES.trailing_zeros() as usize - 16usize.trailing_zeros() as usize + 1;
    let mut smallest = usize::MAX;
    let mut largest = 0usize;

    for index in 0..STRESS_COUNT {
        // Walk the classes in order, wrapping. Within a class, ask for an
        // odd number of bytes a little below the class size, so the
        // rounding path is exercised rather than hitting exact powers of
        // two every time.
        let class = index % STRESS_CLASSES;
        let class_bytes = 16usize << class;
        let size = if class_bytes <= 16 {
            1 + (index % 14)
        } else {
            (class_bytes / 2) + 1 + (index % (class_bytes / 4))
        };

        let align = match index % 4 {
            0 => 1,
            1 => 8,
            2 => 16,
            _ => 64,
        };

        let layout =
            Layout::from_size_align(size + 2, align).expect("the test built an invalid layout");

        // SAFETY: `layout` has a non-zero size, which is `alloc`'s
        // requirement.
        let ptr = unsafe { alloc(layout) };
        assert!(
            !ptr.is_null(),
            "allocation {} of {} bytes (align {}) returned null; the heap \
             should have panicked rather than failing quietly",
            index,
            size,
            align
        );

        assert!(
            (ptr as usize).is_multiple_of(align),
            "allocation {} of {} bytes came back at {:p}, which is not {} \
             byte aligned",
            index,
            size,
            ptr,
            align
        );

        let addr = ptr as u64;
        assert!(
            (vmlayout::KHEAP_START..vmlayout::KHEAP_END).contains(&addr),
            "allocation {} landed at {:#018x}, outside the heap",
            index,
            addr
        );

        // Write canaries at both ends and fill the middle with a
        // position-dependent pattern. If two live blocks ever overlap, one
        // of them will see the other's bytes.
        //
        // SAFETY: The allocation is `size + 2` bytes and ours alone until
        // it is freed below.
        unsafe {
            ptr.write(CANARY_HEAD);
            ptr.add(size + 1).write(CANARY_TAIL);
            for offset in 0..size {
                ptr.add(1 + offset)
                    .write((index as u8).wrapping_add(offset as u8));
            }
        }

        smallest = smallest.min(size);
        largest = largest.max(size);

        blocks.push(Block { ptr, layout });
    }

    println!(
        "[heaptest] alloc: {} blocks, {} B .. {} KiB, every one aligned and \
         inside the heap",
        STRESS_COUNT,
        smallest,
        largest / 1024
    );

    // Now verify every canary and every byte. Doing this *after* all the
    // allocations is the point: if block N overlapped block M, the write
    // to whichever came later would have corrupted the earlier one.
    for (index, block) in blocks.iter().enumerate() {
        let size = block.layout.size() - 2;
        // SAFETY: The block is live and we wrote these bytes above.
        unsafe {
            assert_eq!(
                block.ptr.read(),
                CANARY_HEAD,
                "the head canary of block {} ({} bytes at {:p}) was \
                 overwritten; another allocation overlaps its start",
                index,
                size,
                block.ptr
            );
            assert_eq!(
                block.ptr.add(size + 1).read(),
                CANARY_TAIL,
                "the tail canary of block {} ({} bytes at {:p}) was \
                 overwritten; another allocation overlaps its end",
                index,
                size,
                block.ptr
            );
            for offset in 0..size {
                let expected = (index as u8).wrapping_add(offset as u8);
                let found = block.ptr.add(1 + offset).read();
                assert_eq!(
                    found, expected,
                    "block {} ({} bytes at {:p}) is corrupted at offset {}",
                    index, size, block.ptr, offset
                );
            }
        }
    }

    println!(
        "[heaptest] canaries: all {} blocks intact; no two allocations \
         overlap",
        STRESS_COUNT
    );

    // Internal fragmentation, measured rather than assumed. ADR 016 claims
    // a worst case of 2x for power-of-two classes.
    let stats = heap::stats();
    let overhead = stats.used_actual as u64 * 100 / stats.used_requested.max(1) as u64;
    assert!(
        overhead <= 200,
        "internal fragmentation is {}%, above the 2x worst case the \
         power-of-two size classes should guarantee",
        overhead
    );
    println!(
        "[heaptest] fragmentation: {} bytes requested, {} bytes reserved \
         ({}% -- the size classes guarantee at most 200%)",
        stats.used_requested, stats.used_actual, overhead
    );

    // Free everything, in reverse. Reverse order on purpose: freeing in
    // allocation order is the case a bump allocator would also survive.
    for block in blocks.iter().rev() {
        // SAFETY: Each pointer came from `alloc` with exactly this layout
        // and has not been freed yet.
        unsafe {
            dealloc(block.ptr, block.layout);
        }
    }

    baseline
}

/// 4. The heap grows when asked for more than it has mapped, and the
///    growth really costs physical memory.
fn check_growth() {
    let before = heap::stats();
    let reserve_before = heap::reserve_remaining();

    // Ask for more than is currently mapped, in one allocation. The heap
    // must map new pages rather than failing.
    let wanted = (before.mapped_pages as usize + 4) * 4096;
    let wanted = wanted.min(512 * 1024); // stay inside the largest class

    let layout = Layout::from_size_align(wanted, 8).expect("invalid layout for the growth test");

    // SAFETY: Non-zero size.
    let ptr = unsafe { alloc(layout) };
    assert!(!ptr.is_null(), "the growth allocation returned null");

    // Touch both ends, proving the whole range is really mapped and not
    // just accounted for.
    //
    // SAFETY: `wanted` bytes starting at `ptr` are ours.
    unsafe {
        ptr.write(CANARY_HEAD);
        ptr.add(wanted - 1).write(CANARY_TAIL);
        assert_eq!(ptr.read(), CANARY_HEAD);
        assert_eq!(ptr.add(wanted - 1).read(), CANARY_TAIL);
    }

    let after = heap::stats();
    let reserve_after = heap::reserve_remaining();

    assert!(
        after.mapped_pages > before.mapped_pages,
        "the heap did not grow: {} pages before and after a {} byte \
         allocation",
        before.mapped_pages,
        wanted
    );

    // Each new page must have cost exactly one frame out of the reserve.
    //
    // Counting pages alone would not show this. From M5 the heap draws
    // from a fixed physical budget taken before the frame allocator
    // retired (ADR 022), and a bookkeeping error there -- mapping a page
    // without charging for it, or charging twice -- would leave the page
    // count looking perfectly healthy while the budget drained at the
    // wrong rate.
    let pages_gained = after.mapped_pages - before.mapped_pages;
    let reserve_spent = reserve_before - reserve_after;
    assert_eq!(
        reserve_spent,
        pages_gained * 4096,
        "the heap mapped {} new pages but took {} bytes from its reserve; \
         those should be the same memory",
        pages_gained,
        reserve_spent
    );

    println!(
        "[heaptest] growth: {} bytes requested, heap grew from {} to {} \
         pages, both ends writable; reserve {} MiB -> {} MiB ({} KiB spent \
         for {} pages)",
        wanted,
        before.mapped_pages,
        after.mapped_pages,
        reserve_before / (1024 * 1024),
        reserve_after / (1024 * 1024),
        reserve_spent / 1024,
        pages_gained
    );

    // SAFETY: From `alloc` with this layout, not yet freed.
    unsafe {
        dealloc(ptr, layout);
    }
}

/// 5. Everything freed; the live byte count is back where it started.
///
/// The pages stay mapped -- the heap does not shrink -- and that is
/// reported rather than hidden.
fn check_returned_to_baseline(baseline: usize) {
    let stats = heap::stats();

    assert_eq!(
        stats.used_requested,
        baseline,
        "after freeing everything, {} bytes are still live but the baseline \
         was {}; {} bytes leaked",
        stats.used_requested,
        baseline,
        stats.used_requested.saturating_sub(baseline)
    );

    // The free lists should now hold the blocks that came back. Reporting
    // the distribution makes the no-coalescing decision visible: a class
    // with many free blocks cannot lend them to any other class.
    let lengths = heap::free_list_lengths();
    let mut populated = 0;
    let mut total_free_blocks = 0usize;
    let mut free_bytes = 0usize;
    for (index, &count) in lengths.iter().enumerate() {
        if count > 0 {
            populated += 1;
            total_free_blocks += count;
            free_bytes += count * heap::size_of_class(index);
        }
    }

    println!(
        "[heaptest] baseline: {} bytes in use (was {}), {} pages still \
         mapped",
        stats.used_requested, baseline, stats.mapped_pages
    );
    println!(
        "[heaptest] free lists: {} blocks across {} of {} classes ({} KiB \
         held for reuse; the heap does not shrink and does not coalesce -- \
         ADR 016)",
        total_free_blocks,
        populated,
        heap::class_count(),
        free_bytes / 1024
    );
    println!(
        "[heaptest] calls: {} alloc, {} dealloc, {} grow",
        stats.alloc_calls, stats.dealloc_calls, stats.grow_calls
    );

    // What the whole run cost in physical memory.
    //
    // The reserve pays for two things: the heap's own pages, and the page
    // tables that address them. The first draft of this check asserted
    // the two figures were equal and failed -- 12611584 bytes spent
    // against 3071 pages (12578816 bytes), a gap of exactly eight frames.
    // Those eight are the intermediate tables, and the assertion was
    // simply wrong about what the reserve is for.
    //
    // So the check is the real relationship instead: the tables are a
    // small, bounded overhead on top of the pages. One PT covers 512
    // pages and one PD covers 512 PTs, so for a heap of this size the
    // tables cannot be more than a few per cent -- and if they were, it
    // would mean `map` had started allocating per-page.
    let spent = heap::reserve_size() - heap::reserve_remaining();
    let page_bytes = stats.mapped_pages * 4096;
    let table_bytes = spent - page_bytes;
    let table_frames = table_bytes / 4096;

    assert!(
        spent >= page_bytes,
        "the heap has {} pages mapped but has spent only {} bytes of its \
         reserve; a page was mapped without being charged for",
        stats.mapped_pages,
        spent
    );

    // Three levels at most, and each table serves 512 entries. Allow a
    // generous ceiling, but one that a per-page allocation would blow
    // straight through.
    let table_ceiling = stats.mapped_pages / 100 + 16;
    assert!(
        table_frames <= table_ceiling,
        "{} frames went on page tables for {} heap pages, above the \
         ceiling of {}; something is allocating a table per page rather \
         than per 512",
        table_frames,
        stats.mapped_pages,
        table_ceiling
    );

    println!(
        "[heaptest] reserve: {} KiB of {} MiB spent -- {} KiB of heap pages \
         plus {} frames of page tables; the heap never returns a page \
         (ADR 016)",
        spent / 1024,
        heap::reserve_size() / (1024 * 1024),
        page_bytes / 1024,
        table_frames
    );
}

// ---- Negative scenarios ------------------------------------------------

/// Allocate until the heap cannot grow any further.
///
/// Expected: a panic naming the requested size and alignment. The two ways
/// this can end -- the 64 MiB limit, or physical memory running out --
/// produce different messages, and either is a pass.
///
/// What must *not* happen: an infinite loop, or `alloc` returning null and
/// the kernel carrying on.
pub fn run_oom() -> ! {
    println!("[heaptest] exhausting the heap; expect a panic naming the size");

    let stats = heap::stats();
    println!(
        "[heaptest] starting from {} pages mapped, limit {} MiB",
        stats.mapped_pages,
        vmlayout::KHEAP_MAX_SIZE / (1024 * 1024)
    );

    // 1 MiB at a time: the largest class, so each allocation takes the
    // slow path and forces the heap to grow. The loop is bounded by the
    // heap limit -- 64 MiB / 1 MiB = 64 iterations before the panic -- so
    // even if the panic failed to fire this could not spin forever.
    let chunk = 1024 * 1024;
    let bound = (vmlayout::KHEAP_MAX_SIZE as usize / chunk) + 8;
    let layout = Layout::from_size_align(chunk, 8).expect("invalid layout");

    for index in 0..bound {
        // SAFETY: Non-zero size. The pointer is deliberately leaked: the
        // point is to run out, and freeing would defeat it.
        let ptr = unsafe { alloc(layout) };
        assert!(
            !ptr.is_null(),
            "allocation {} returned null instead of panicking; out of memory \
             must be loud, not a silent null",
            index
        );

        // Touch the block, volatile, at both ends.
        //
        // This is load-bearing, and the first version of this test did not
        // have it. LLVM recognises `alloc` as an allocation function and
        // is permitted to delete an allocation whose result is never used
        // -- a null check does not count as a use. The test then ran all
        // 72 iterations without the heap growing at all, and failed with
        // "the heap handed out 72 MiB without hitting its 64 MiB limit".
        // The heap was fine; the allocations had been optimised away.
        //
        // The same trap caught `vmtest::recurse` in M3, where an unused
        // stack array was removed and the stack never overflowed. A test
        // whose effect the optimiser can erase is not a test.
        //
        // SAFETY: `chunk` bytes starting at `ptr` are ours and mapped.
        unsafe {
            ptr.write_volatile(CANARY_HEAD);
            ptr.add(chunk - 1).write_volatile(CANARY_TAIL);
        }

        if index % 8 == 0 {
            let stats = heap::stats();
            println!(
                "[heaptest] {} MiB allocated and leaked, {} pages mapped",
                index + 1,
                stats.mapped_pages
            );
        }
    }

    unreachable!(
        "the heap handed out {} MiB without hitting its {} MiB limit",
        bound,
        vmlayout::KHEAP_MAX_SIZE / (1024 * 1024)
    );
}

/// Write to the unmapped page above the heap's maximum extent.
///
/// Expected: #PF with CR2 in the upper guard page. This proves the guard
/// is genuinely absent rather than merely reserved -- the same distinction
/// that M3 drew for stack guard pages, where M1 and M2 had dead space that
/// a deep enough overflow walked straight through.
pub fn run_guard() -> ! {
    let target = vmlayout::KHEAP_GUARD_TOP;

    println!(
        "[heaptest] guard page test: writing to {:#018x}, the unmapped page \
         above the heap's {} MiB limit",
        target,
        vmlayout::KHEAP_MAX_SIZE / (1024 * 1024)
    );
    println!(
        "[heaptest] heap is {:#018x}..{:#018x}, {} page(s) mapped",
        vmlayout::KHEAP_START,
        vmlayout::KHEAP_END,
        heap::stats().mapped_pages
    );

    // SAFETY: This write is meant to fault. The address is in the guard
    // page above KHEAP_END, which nothing maps and the heap can never
    // reach -- `grow` refuses to go past KHEAP_END. The CPU traps on the
    // store itself, so the write never completes and nothing observes it.
    unsafe {
        (target as *mut u64).write_volatile(0xDEAD_BEEF_CAFE_F00D);
    }

    unreachable!("the heap's upper guard page did not fault; it is mapped");
}
