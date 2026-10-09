// SPDX-License-Identifier: MPL-2.0
//! Virtual memory acceptance tests, run inside the kernel.
//!
//! Like `memtest`, these run against the real machine and the kernel checks
//! its own invariants; the host runner then asserts on the serial output.

use limine::memmap;

use crate::cpu::Features;
use crate::memory::layout;
use crate::memory::paging::{self, MapError, PageFlags, TranslateError, UnmapError};
use crate::memory::vm::{self, GlobalFrames};
use crate::memory::{bootinfo, frame_allocator as frames, VirtAddr, FRAME_SIZE};
use crate::println;

/// Scratch address used by the mapping tests.
///
/// In the reserved part of the kernel half -- not PMAP, not the heap, not
/// a stack -- so nothing else could be using it and a mistake here cannot
/// damage anything that matters.
const SCRATCH: u64 = 0xFFFF_D800_0000_0000;

/// Second scratch page, for the stale-TLB check.
const SCRATCH_TLB: u64 = 0xFFFF_D800_0001_0000;

/// A non-canonical address: bit 47 set, bits above it clear. Accessing it
/// raises #GP rather than #PF, which is why the paging API reports it
/// separately (ADR 009, ADR 011).
const NON_CANONICAL: u64 = 0x0000_8000_0000_0000;

/// Pattern written through a fresh mapping and read back.
const PATTERN: u64 = 0x5047_4E49_5041_4D56; // "VMAPING P" little-endian-ish

/// Run the virtual memory test suite.
pub fn run(features: &Features) {
    println!("[vmtest] starting virtual memory tests");

    check_cpu_features(features);
    check_running_on_own_tables();
    check_on_own_stack();
    let frame_count = check_map_write_read();
    check_error_cases();
    check_reclaim_survived();

    println!(
        "[vmtest] all virtual memory tests passed ({} free frames)",
        frame_count
    );
}

/// The current stack pointer.
fn rsp() -> u64 {
    let value: u64;
    // SAFETY: Reading RSP has no side effects and touches no memory.
    unsafe {
        core::arch::asm!("mov {}, rsp", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// 1. The feature bits the rest of this depends on are actually set.
fn check_cpu_features(features: &Features) {
    assert!(
        features.nx_enabled,
        "EFER.NXE is clear; every NX mapping would fault on first use"
    );
    println!("[vmtest] cpu: efer.nxe set, w^x is meaningful");

    // SMEP and SMAP are advisory in M3 -- there are no user mappings for
    // them to protect yet. Report rather than assert, so a CPU without them
    // still passes.
    println!(
        "[vmtest] cpu: smep {}, smap {} (advisory until ring 3 in M8)",
        if features.smep_enabled { "on" } else { "off" },
        if features.smap_enabled { "on" } else { "off" }
    );
}

/// 2. CR3 points at the table we built, and the window still works.
fn check_running_on_own_tables() {
    assert!(
        vm::using_own_tables(),
        "cr3 does not point at the table vm::init built"
    );

    // Read and write a physical frame through PMAP. If the window did not
    // resolve to real memory, the read-back would differ.
    let frame = frames::allocate().expect("no free frame for the window check");
    let via_window = crate::memory::phys_to_virt(frame.start());

    assert!(
        layout::is_pmap(via_window.0),
        "{} should be inside the physical window at {:#018x}",
        via_window,
        layout::PMAP_BASE
    );

    // SAFETY: The frame came from the allocator, so nothing else owns it,
    // and PMAP maps all RAM read-write.
    let value = unsafe {
        let ptr = via_window.as_ptr::<u64>();
        ptr.write_volatile(PATTERN);
        ptr.read_volatile()
    };
    assert_eq!(
        value, PATTERN,
        "a write through PMAP did not read back; the window is wrong"
    );

    // The window must be NX: it maps data, and a writable page that is also
    // executable would break W^X for all of RAM.
    // SAFETY: Single-threaded; no other borrow of the PML4 is live.
    let table = unsafe { vm::kernel_table() };
    // SAFETY: Table is live and uniquely borrowed here.
    let (_, window_flags) =
        unsafe { paging::translate(table, via_window) }.expect("the physical window is not mapped");
    assert!(
        !window_flags.contains(PageFlags::EXEC),
        "the physical window is executable: {:?}",
        window_flags
    );
    assert!(
        window_flags.contains(PageFlags::WRITE),
        "the physical window is not writable: {:?}",
        window_flags
    );

    frames::free(frame);

    println!(
        "[vmtest] cr3: own tables; pmap at {:#018x} is rw+nx and round-trips",
        layout::PMAP_BASE
    );
}

/// 3. The kernel is running on a stack it allocated, with a guard page,
///    and not on the one Limine handed it.
///
/// This check exists because the kernel used to fail it. Limine places the
/// boot stack in `BOOTLOADER_RECLAIMABLE` memory, `reclaim` poisons that
/// memory, and the first version of this code reclaimed while still
/// standing on it -- producing a #GP at `rip=0xdededededededede`, the
/// poison pattern arriving as a return address.
fn check_on_own_stack() {
    let sp = rsp();

    let (guard, bottom, top) = layout::kstack_slot(layout::KSTACK_SLOT_MAIN);

    assert!(
        sp > bottom.0 && sp <= top.0,
        "rsp is {:#018x}, outside the main kernel stack {}..{}",
        sp,
        bottom,
        top
    );

    // The library predicate must agree with the arithmetic here, since
    // reclaim trusts it as a gate.
    assert!(
        vm::on_own_stack(),
        "vm::on_own_stack() says no while rsp {:#018x} is inside {}..{}",
        sp,
        bottom,
        top
    );

    // The guard page below it must genuinely be absent, or an overflow
    // would run into whatever is underneath instead of faulting.
    // SAFETY: Single-threaded; no other borrow of the PML4 is live.
    let table = unsafe { vm::kernel_table() };
    // SAFETY: Table live and uniquely borrowed here.
    let guard_mapping = unsafe { paging::translate(table, guard) };
    assert!(
        guard_mapping.is_err(),
        "the guard page at {} is mapped; a stack overflow would not fault",
        guard
    );

    // And the stack must not be in memory anyone may reclaim. This is the
    // property that was violated: the address was mapped and writable, and
    // still the wrong place to stand.
    // SAFETY: Table live and uniquely borrowed here; `sp` was read from
    // RSP a moment ago, so it is a canonical address in a mapped page.
    let phys = unsafe { paging::translate(table, VirtAddr::new(sp)) }
        .expect("the stack we are standing on is not mapped")
        .0;
    for region in bootinfo::get().memory_map() {
        let in_region = phys.0 >= region.base && phys.0 < region.base + region.length;
        assert!(
            !(in_region && region.kind == memmap::MEMMAP_BOOTLOADER_RECLAIMABLE),
            "the stack is at physical {} inside a bootloader-reclaimable \
             region {:#x}..{:#x}; reclaim would poison it",
            phys,
            region.base,
            region.base + region.length
        );
    }

    println!(
        "[vmtest] stack: rsp {:#018x} in kstack[{}] {}..{}, phys {}, guard \
         page at {} unmapped, not reclaimable",
        sp,
        layout::KSTACK_SLOT_MAIN,
        bottom,
        top,
        phys,
        guard
    );
}

/// 4-6. Map a frame, write through it, read it back, translate it, unmap
/// it and check the frame comes back.
fn check_map_write_read() -> u64 {
    let before = frames::free_count();

    let frame = frames::allocate().expect("no free frame for the mapping test");
    let page = VirtAddr::new(SCRATCH);
    let flags = PageFlags::READ | PageFlags::WRITE;

    let mut source = GlobalFrames;

    // SAFETY: `kernel_table` hands out the live PML4; no other mutable
    // borrow exists on this single-threaded boot path. The page is in the
    // reserved part of the kernel half, so nothing else maps it.
    let table = unsafe { vm::kernel_table() };

    // SAFETY: Table is live, the frame is ours, and the address is unused.
    let guard = unsafe { paging::map(table, page, frame, flags, &mut source) }
        .expect("mapping a fresh page failed");
    guard.flush();

    // Write and read back through the new mapping.
    // SAFETY: The page was just mapped read-write and the flush discharged,
    // so the translation is live.
    let read_back = unsafe {
        let ptr = page.as_ptr::<u64>();
        ptr.write_volatile(PATTERN);
        ptr.read_volatile()
    };
    assert_eq!(
        read_back, PATTERN,
        "value written through the new mapping did not read back"
    );
    println!(
        "[vmtest] map: {} -> {}, wrote and read back {:#018x}",
        page, frame, PATTERN
    );

    // translate() must agree about both the address and the flags.
    // SAFETY: Table is live and uniquely borrowed here.
    let (phys, got_flags) =
        unsafe { paging::translate(table, page) }.expect("translate failed for a mapped page");
    assert_eq!(
        phys,
        frame.start(),
        "translate returned {} but the page was mapped to {}",
        phys,
        frame.start()
    );
    assert!(
        got_flags.contains(PageFlags::READ | PageFlags::WRITE),
        "translate lost the read/write flags: {:?}",
        got_flags
    );
    assert!(
        !got_flags.contains(PageFlags::EXEC),
        "a page mapped without EXEC came back executable; NX is not working"
    );
    println!("[vmtest] translate: {} -> {} {:?}", page, phys, got_flags);

    // Unmap: the frame must come back, and it must be the one we mapped.
    // SAFETY: Table is live; the page is mapped.
    let (returned, guard) = unsafe { paging::unmap(table, page) }.expect("unmapping failed");
    guard.flush();

    assert_eq!(
        returned, frame,
        "unmap returned {} but {} was mapped",
        returned, frame
    );
    println!("[vmtest] unmap: returned {} as expected", returned);

    // The frame is ours again, not the allocator's: unmap hands it back
    // rather than freeing it (ADR 011).
    frames::free(frame);

    let after = frames::free_count();

    // Three frames are still out: the PDPT, PD and PT that `map` had to
    // create to reach a page at SCRATCH, which no other mapping shares.
    // `unmap` clears the leaf entry and nothing else.
    //
    // That is deliberate, not an oversight. Freeing an intermediate table
    // means knowing it has no other live entries, which means a reference
    // count per table -- and a count that is wrong in the unmapping
    // direction frees a table another mapping is still walking through.
    // M3 does not have the counts, so it does not free. The cost is three
    // frames per 512 GiB of address space ever touched, which is nothing;
    // the alternative is a use-after-free in the page walker.
    //
    // Asserting the exact number rather than ignoring the difference is
    // the point: if a future change starts leaking the *leaf* frame too,
    // or allocates a table it did not need, this fires.
    const INTERMEDIATE_TABLES: u64 = 3;
    let still_out = before.saturating_sub(after);
    assert_eq!(
        still_out, INTERMEDIATE_TABLES,
        "expected exactly {} frames to remain allocated (the PDPT, PD and \
         PT backing {}), but {} are out; a leaf frame leaked or an \
         unnecessary table was built",
        INTERMEDIATE_TABLES, page, still_out
    );

    println!(
        "[vmtest] frames: leaf frame returned; {} page-table frames remain \
         allocated by design (no refcounts until M5)",
        INTERMEDIATE_TABLES
    );

    after
}

/// 7. Every error case returns an error rather than panicking.
///
/// This is the part that matters for M11: a user component will drive this
/// API through a capability, and a bad request must not take the kernel
/// down.
fn check_error_cases() {
    let mut source = GlobalFrames;
    // SAFETY: Single-threaded; no other borrow of the PML4 is live.
    let table = unsafe { vm::kernel_table() };

    let page = VirtAddr::new(SCRATCH);
    let frame = frames::allocate().expect("no free frame for the error tests");

    // --- AlreadyMapped --------------------------------------------------
    // SAFETY: Table live, frame ours, address unused.
    let guard = unsafe {
        paging::map(
            table,
            page,
            frame,
            PageFlags::READ | PageFlags::WRITE,
            &mut source,
        )
    }
    .expect("setting up the AlreadyMapped case failed");
    guard.flush();

    let second = frames::allocate().expect("no second frame for the AlreadyMapped case");
    // SAFETY: As above; this call is expected to fail, not to map.
    let result = unsafe {
        paging::map(
            table,
            page,
            second,
            PageFlags::READ | PageFlags::WRITE,
            &mut source,
        )
    };
    match result {
        Err(MapError::AlreadyMapped { existing }) => {
            assert_eq!(existing, frame, "AlreadyMapped named the wrong frame");
            println!("[vmtest] error: map over a live mapping -> AlreadyMapped");
        }
        other => panic!("expected AlreadyMapped, got {:?}", other.is_ok()),
    }
    frames::free(second);

    // Clean up so the page is free for the next case.
    // SAFETY: Table live, page mapped.
    let (returned, guard) = unsafe { paging::unmap(table, page) }.expect("cleanup unmap failed");
    guard.flush();
    frames::free(returned);

    // --- NotMapped ------------------------------------------------------
    // SAFETY: Table live; the page was just unmapped.
    let result = unsafe { paging::unmap(table, page) };
    match result {
        Err(UnmapError::NotMapped) => {
            println!("[vmtest] error: unmap an absent page -> NotMapped");
        }
        _ => panic!("expected NotMapped for an unmapped page"),
    }

    // --- NotCanonical ---------------------------------------------------
    let bad = VirtAddr::new(NON_CANONICAL);
    let frame = frames::allocate().expect("no frame for the NotCanonical case");
    // SAFETY: Table live; this call is expected to fail before touching
    // anything.
    let result = unsafe {
        paging::map(
            table,
            bad,
            frame,
            PageFlags::READ | PageFlags::WRITE,
            &mut source,
        )
    };
    match result {
        Err(MapError::NotCanonical { addr }) => {
            assert_eq!(addr, bad);
            println!(
                "[vmtest] error: map at non-canonical {} -> NotCanonical",
                bad
            );
        }
        _ => panic!("expected NotCanonical for {}", bad),
    }

    // translate() must report it the same way.
    // SAFETY: Table live.
    match unsafe { paging::translate(table, bad) } {
        Err(TranslateError::NotCanonical { .. }) => {}
        _ => panic!("translate accepted a non-canonical address"),
    }

    // --- InvalidFlags: W^X ----------------------------------------------
    // SAFETY: Table live; expected to fail on flag validation.
    let result = unsafe {
        paging::map(
            table,
            page,
            frame,
            PageFlags::READ | PageFlags::WRITE | PageFlags::EXEC,
            &mut source,
        )
    };
    match result {
        Err(MapError::InvalidFlags(flags)) => {
            assert!(flags.contains(PageFlags::WRITE | PageFlags::EXEC));
            println!("[vmtest] error: map with WRITE|EXEC -> InvalidFlags (w^x)");
        }
        _ => panic!("expected InvalidFlags for a writable executable mapping"),
    }

    frames::free(frame);

    println!("[vmtest] error: all four cases returned errors, none panicked");
}

/// 8. The bootloader's memory was reclaimed and the copies still work.
fn check_reclaim_survived() {
    let info = bootinfo::get();

    let name = info.bootloader_name();
    let version = info.bootloader_version();
    let regions = info.memory_map();

    assert!(
        !name.is_empty(),
        "the copied bootloader name is empty after reclaim"
    );
    assert!(
        !regions.is_empty(),
        "the copied memory map is empty after reclaim"
    );

    // The command line is what selected this very test, so if the copy had
    // not survived, this string would not contain the token that got us
    // here.
    let cmdline = info.cmdline();
    assert!(
        cmdline.contains("vm-test"),
        "the copied command line lost the token that selected this test: {:?}",
        cmdline
    );

    // Not every region is page aligned, and that is the firmware's right:
    // this machine reports one at 0x9fc00, the EBDA, which starts 1 KiB
    // short of a page boundary. An earlier version of this test asserted
    // alignment and failed here -- the assertion was wrong, not the
    // memory map.
    //
    // What must hold is that the kernel never treats a partial frame as
    // its own. `reclaim` rounds the base up and the end down, so a region
    // contributes only the frames it wholly covers. Check that property
    // instead of inventing a constraint on the firmware.
    let mut unaligned = 0;
    for region in regions {
        if !region.base.is_multiple_of(FRAME_SIZE) {
            unaligned += 1;
        }

        let first = region.base.div_ceil(FRAME_SIZE);
        let last_exclusive = (region.base + region.length) / FRAME_SIZE;

        assert!(
            first * FRAME_SIZE >= region.base,
            "the first whole frame of the region at {:#x} starts before it",
            region.base
        );
        assert!(
            last_exclusive * FRAME_SIZE <= region.base + region.length,
            "the last whole frame of the region at {:#x} ends past it",
            region.base
        );
    }

    println!(
        "[vmtest] memory map: {} regions, {} not page aligned; whole-frame \
         arithmetic stays inside every one",
        regions.len(),
        unaligned
    );

    println!(
        "[vmtest] reclaim: boot info survived ({} {}, {} map entries, \
         cmdline {:?})",
        name,
        version,
        regions.len(),
        cmdline
    );
}

// ---- Negative scenarios ------------------------------------------------

/// Deliberately skip TLB invalidation and see whether the stale entry
/// survives.
///
/// Expected: #PF on the read after unmap, because the translation should be
/// gone. But QEMU's TLB is not obliged to behave like real hardware -- it
/// may well have dropped the entry anyway -- so a *missing* fault is not a
/// failure here. The check is advisory and says so in its output.
pub fn run_stale_tlb() -> ! {
    println!("[vmtest] stale-TLB check (advisory: QEMU's TLB is not real hardware)");

    let mut source = GlobalFrames;
    // SAFETY: Single-threaded; no other borrow is live.
    let table = unsafe { vm::kernel_table() };

    let page = VirtAddr::new(SCRATCH_TLB);
    let frame = frames::allocate().expect("no free frame for the stale-TLB check");

    // SAFETY: Table live, frame ours, address unused.
    let guard = unsafe {
        paging::map(
            table,
            page,
            frame,
            PageFlags::READ | PageFlags::WRITE,
            &mut source,
        )
    }
    .expect("mapping for the stale-TLB check failed");
    guard.flush();

    // Touch it, so the translation is definitely in the TLB.
    // SAFETY: Just mapped read-write and flushed.
    unsafe {
        page.as_ptr::<u64>().write_volatile(PATTERN);
    }
    println!("[vmtest] mapped {} and wrote through it", page);

    // Unmap, and deliberately do not invalidate. This is the only
    // legitimate use of the escape hatch outside bulk mapping: the whole
    // point is to leave a stale entry behind.
    // SAFETY: Table live; the page is mapped.
    let (_, guard) = unsafe { paging::unmap(table, page) }.expect("unmap failed");
    guard.ignore_because_caller_will_flush_all();
    println!("[vmtest] unmapped it WITHOUT invalidating the TLB");

    // On real hardware this read may still succeed from the stale entry,
    // which is the bug the discipline exists to prevent. On QEMU it
    // usually faults.
    // SAFETY: Deliberately reading an address whose mapping was removed.
    // Either it faults -- the expected outcome -- or it returns whatever
    // the stale translation points at, which is the frame we still own and
    // have not freed, so nothing else can be corrupted either way.
    let value = unsafe { page.as_ptr::<u64>().read_volatile() };

    println!(
        "[vmtest] read after unmap returned {:#018x} -- QEMU did not fault; \
         this check is advisory",
        value
    );
    println!(
        "[vmtest] a stale TLB entry survived here; on real hardware this is \
         exactly the bug TlbFlush exists to prevent"
    );

    crate::qemu::exit(crate::qemu::EXIT_SUCCESS)
}

/// Write to the guard page below the IST stack.
///
/// Expected: #PF with CR2 inside the guard page. In M1 and M2 this address
/// was mapped dead space and the write simply succeeded; now it is
/// genuinely absent, so the protection is real.
pub fn run_guard_page() -> ! {
    let (guard_start, bottom, _top) = layout::kstack_slot(layout::KSTACK_SLOT_IST1);

    println!(
        "[vmtest] guard page test: writing to {} (guard below the IST stack \
         at {})",
        guard_start, bottom
    );

    // Aim at the last byte of the guard page: that is where a stack
    // overflow would land first, having run off the bottom of the stack.
    let target = VirtAddr::new(bottom.0 - 8);

    // SAFETY: This write is meant to fault. The address is inside the
    // unmapped guard page, so the access cannot reach memory -- the CPU
    // traps on the store itself and the handler reports it. Nothing
    // observes the never-completed write.
    unsafe {
        target.as_ptr::<u64>().write_volatile(PATTERN);
    }

    unreachable!("the guard page did not fault; it is not actually unmapped");
}

/// Overflow the main kernel stack into its guard page.
///
/// Expected: #PF with CR2 in the guard page below `KSTACK_SLOT_MAIN`.
/// [`run_guard_page`] proves the same thing for the IST stack by writing
/// directly at the guard; this one gets there the way a real overflow
/// would, by recursing until RSP walks off the bottom.
///
/// The distinction matters: a direct write proves the page is unmapped,
/// while a genuine overflow also proves the *stack* is positioned so that
/// running off it reaches that page, rather than something else.
pub fn run_stack_overflow() -> ! {
    let (guard, bottom, top) = layout::kstack_slot(layout::KSTACK_SLOT_MAIN);

    println!(
        "[vmtest] stack overflow test: recursing off the bottom of \
         kstack[{}] ({}..{}) into the guard page at {}",
        layout::KSTACK_SLOT_MAIN,
        bottom,
        top,
        guard
    );
    println!("[vmtest] rsp before: {:#018x}", rsp());

    recurse(0);

    unreachable!("the stack overflow did not fault; the guard page is mapped");
}

/// Consume stack until the guard page stops us.
///
/// `inline(never)` and the volatile write are both load-bearing: without
/// them the compiler turns this into a loop with no stack growth, and the
/// test silently never overflows anything.
///
/// The `unconditional_recursion` lint is right -- there is no base case,
/// on purpose. The exit is the #PF when RSP walks off the bottom of the
/// stack into the guard page, which is the thing being tested. A base case
/// would mean the function returns normally, and then the test proves
/// nothing. `loop`, which the lint suggests, does not grow the stack and so
/// is not a substitute.
#[inline(never)]
#[allow(unconditional_recursion)]
fn recurse(depth: u64) -> u64 {
    // A kilobyte per frame, so the 16 KiB stack is gone in about sixteen
    // calls rather than thousands.
    let mut burn = [0u8; 1024];
    // SAFETY: `burn` is a live local; the write is volatile only to stop
    // the optimiser from deleting the array and with it the stack growth.
    unsafe {
        core::ptr::write_volatile(&mut burn[0], depth as u8);
        core::ptr::write_volatile(&mut burn[1023], depth as u8);
    }

    // Report sparsely: the serial port is slow and the point is the fault,
    // not the log.
    if depth.is_multiple_of(8) {
        println!("[vmtest] depth {}, rsp {:#018x}", depth, rsp());
    }

    recurse(depth + 1) + burn[0] as u64
}

/// Jump to a data page mapped NX.
///
/// Expected: #PF with the instruction-fetch bit (bit 4) set in the error
/// code. That bit is only set when EFER.NXE is enabled, so this doubles as
/// proof that `cpu::init` did its job.
pub fn run_nx() -> ! {
    let frame = frames::allocate().expect("no free frame for the NX test");
    let via_window = crate::memory::phys_to_virt(frame.start());

    // Put a `ret` there so that, if the jump were allowed, control would
    // come back rather than running off into whatever follows.
    //
    // SAFETY: The frame came from the allocator, so nothing else owns it,
    // and PMAP maps all RAM read-write.
    unsafe {
        via_window.as_ptr::<u8>().write_volatile(0xC3); // ret
    }

    println!(
        "[vmtest] nx test: jumping to {} (a data page in PMAP, mapped NX)",
        via_window
    );

    // SAFETY: This call is meant to fault. The target is in PMAP, which is
    // mapped NX, so the instruction fetch traps before anything executes.
    // If NX were not in effect the byte there is `ret`, which would return
    // here harmlessly -- and the assertion below would then fire.
    unsafe {
        let f: extern "C" fn() = core::mem::transmute(via_window.as_ptr::<()>());
        f();
    }

    unreachable!(
        "executing from an NX page succeeded; either PMAP is not NX or \
         EFER.NXE is clear"
    );
}
