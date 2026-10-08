// SPDX-License-Identifier: BSD-2-Clause
//! PythOS — M1: CPU structures.
//!
//! Established by M0:
//!   1. Limine boots us into 64-bit long mode in the higher half.
//!   2. COM1 carries a log the host test runner can assert on.
//!   3. A panic reports its source location and terminates the VM.
//!   4. QEMU exits with a status the runner can decode (33 / 35).
//!
//! Added by M1:
//!   5. Our own GDT and TSS, replacing the bootloader's.
//!   6. Our own IDT with all 32 exception vectors populated.
//!   7. Exceptions are reported on the serial console with full machine
//!      state, and #DF is handled on a dedicated IST stack so that a broken
//!      stack pointer cannot escalate to a silent triple fault.
//!
//! Added by M2:
//!   8. Limine's memory map is read and reported.
//!   9. A bitmap frame allocator over the usable regions, which never hands
//!      out frame 0, the kernel image, or its own metadata, and which
//!      detects a double free rather than corrupting itself quietly.
//!
//! Added by M3:
//!  10. EFER.NXE, SMEP, SMAP and PGE enabled from CPUID, not assumed.
//!  11. Our own page tables: a physical window that is NX and covers only
//!      RAM, the kernel image mapped W^X section by section, and the
//!      double-fault stack behind a genuinely unmapped guard page.
//!  12. map/unmap/translate, with TLB invalidation enforced by the type
//!      system.
//!  13. The kernel moved onto a stack of its own, because the one Limine
//!      gives us sits in the memory we are about to reclaim.
//!  14. The bootloader's memory reclaimed, after copying out everything
//!      still being read.
//!
//! Added by M4:
//!  15. A kernel heap in KHEAP, growing on demand, with guard pages at
//!      both ends. `Box`, `Vec`, `BTreeMap` and `String` work from here
//!      on.
//!  16. Size-classed free lists, so that both `alloc` and `dealloc` are
//!      O(1) with no searching -- a precondition for allocating on M9's
//!      IPC path. See ADR 016.
//!
//! Deliberately absent: external interrupts (the PIC is masked as Limine
//! left it), threads, and user space.

#![no_std]
#![no_main]

// The heap arrives in M4; `alloc` is pulled in here rather than being
// implied, because `#[global_allocator]` has to exist before anything can
// use it. See kernel/src/memory/heap.rs.
extern crate alloc;

mod apic;
mod cap;
mod captest;
mod cpu;
mod faults;
mod gdt;
mod heaptest;
mod idt;
mod interrupts;
mod io;
mod ioapic;
mod ipctest;
mod irqtest;
mod memory;
mod memtest;
mod panic;
mod qemu;
mod roottask;
mod roottest;
mod sched;
mod serial;
mod syscall;
mod threadtest;
mod usertest;
mod vmtest;
mod vspacetest;
mod vspaceuser;

use limine::request::{
    BootloaderInfoRequest, ExecutableAddressRequest, ExecutableCmdlineRequest, HhdmRequest,
    MemmapRequest, StackSizeRequest,
};
use limine::{BaseRevision, RequestsEndMarker, RequestsStartMarker};

/// Name reported on the serial console.
const KERNEL_NAME: &str = "PythOS";

/// Version reported on the serial console. Kept in step with Cargo.toml.
const KERNEL_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "-M8");

/// Lowest address of the higher half under the -2 GiB kernel code model.
///
/// With 4-level paging the canonical address space has a hole in the middle;
/// the top 2 GiB is the only region reachable from 32-bit sign-extended
/// displacements, which is what `-C code-model=kernel` assumes.
///
/// Reference: Intel SDM Vol. 1, section 3.3.7.1 "Canonical Addressing";
/// System V AMD64 ABI, section 3.5.1 (the "kernel" code model).
const HIGHER_HALF_BASE: u64 = 0xffff_ffff_8000_0000;

/// Stack Limine should hand us, in bytes.
///
/// The protocol's default is small and the formatting machinery in
/// `core::fmt` is not frugal with stack. 64 KiB leaves room without
/// allocating anything -- Limine carves this out of its own memory before
/// handing control over, so the kernel still allocates nothing itself.
const STACK_SIZE: u64 = 64 * 1024;

// ---- Limine requests ---------------------------------------------------
//
// Every item below must survive to the final binary with its address
// recorded in `.limine_requests`, because nothing in the kernel references
// them -- the bootloader finds them by scanning that section before we ever
// run. Two attributes make that work:
//
//   #[used]           stops rustc from discarding the static as dead code
//   #[link_section]   places it where the linker script KEEP()s it
//
// Without both, the requests vanish and every `response()` returns None at
// runtime: a silent boot with an empty serial console.

/// Negotiates the boot protocol revision.
///
/// Revision 3 is what this kernel is written against. The crate's
/// `BaseRevision::new()` would request its own maximum (6), which would make
/// the kernel's expectations drift with crate upgrades, so we pin it.
#[used]
#[link_section = ".limine_requests"]
static BASE_REVISION: BaseRevision = BaseRevision::with_revision(3);

#[used]
#[link_section = ".limine_requests"]
static BOOTLOADER_INFO: BootloaderInfoRequest = BootloaderInfoRequest::new();

#[used]
#[link_section = ".limine_requests"]
static EXECUTABLE_ADDRESS: ExecutableAddressRequest = ExecutableAddressRequest::new();

#[used]
#[link_section = ".limine_requests"]
static HHDM: HhdmRequest = HhdmRequest::new();

#[used]
#[link_section = ".limine_requests"]
static CMDLINE: ExecutableCmdlineRequest = ExecutableCmdlineRequest::new();

/// The machine's memory map: which physical ranges exist and what each is
/// for. Without it the kernel has no idea which memory it may touch.
#[used]
#[link_section = ".limine_requests"]
static MEMMAP: MemmapRequest = MemmapRequest::new();

#[used]
#[link_section = ".limine_requests"]
static STACK: StackSizeRequest = StackSizeRequest::new(STACK_SIZE);

// The markers bracket the request section so the bootloader can scan a
// bounded range instead of the whole image. They must come first and last in
// their own sections; the linker script orders the three sections.
#[used]
#[link_section = ".limine_requests_start"]
static REQUESTS_START: RequestsStartMarker = RequestsStartMarker::new();

#[used]
#[link_section = ".limine_requests_end"]
static REQUESTS_END: RequestsEndMarker = RequestsEndMarker::new();

// ---- Symbols defined by the linker script ------------------------------

unsafe extern "C" {
    static __kernel_image_start: u8;
    static __kernel_image_end: u8;
    static __bss_start: u8;
    static __bss_end: u8;
}

/// Address of a linker-defined symbol.
///
/// These symbols have no value of their own; only their addresses are
/// meaningful. Taking a reference and casting it is the standard way to read
/// one, and it never dereferences the (possibly zero-sized) storage.
fn linker_symbol(symbol: &'static u8) -> u64 {
    core::ptr::from_ref(symbol) as u64
}

// ---- Entry point -------------------------------------------------------

/// Kernel entry point.
///
/// Limine enters here in 64-bit long mode with paging enabled, interrupts
/// disabled, and a valid stack, per the Limine boot protocol. The name
/// matches `ENTRY(kstart)` in the linker script.
///
/// `extern "C"` because the bootloader calls this across an ABI boundary;
/// `no_mangle` so the linker script can find it by name.
#[unsafe(no_mangle)]
pub extern "C" fn kstart() -> ! {
    // Serial first: without it there is no way to report anything at all,
    // including a failure to bring up serial.
    let uart_ok = serial::init();

    println!();
    println!("{} {}", KERNEL_NAME, KERNEL_VERSION);
    println!(
        "[boot] serial: COM1 115200 8N1 ({})",
        if uart_ok {
            "self-test ok"
        } else {
            "self-test FAILED"
        }
    );
    // `ready()` must agree with what `init()` just told us; if it does not,
    // the driver's state tracking is broken and later code cannot trust it.
    assert_eq!(
        serial::ready(),
        uart_ok,
        "serial readiness flag disagrees with init() result"
    );

    // If the bootloader did not honour the revision we asked for, every
    // response pointer below is meaningless and must not be trusted.
    assert!(
        BASE_REVISION.is_supported(),
        "Limine base revision 3 is not supported by this bootloader"
    );

    let info = BOOTLOADER_INFO
        .response()
        .expect("Limine did not answer the bootloader info request");
    println!("[boot] bootloader: {} {}", info.name(), info.version());

    let address = EXECUTABLE_ADDRESS
        .response()
        .expect("Limine did not answer the executable address request");
    let virtual_base = address.virtual_base;
    let physical_base = address.physical_base;
    println!(
        "[boot] higher-half base: {:#x} (phys {:#x}, slide {:#x})",
        virtual_base,
        physical_base,
        virtual_base.wrapping_sub(physical_base)
    );

    let hhdm = HHDM
        .response()
        .expect("Limine did not answer the HHDM request");
    println!("[boot] hhdm offset: {:#x}", hhdm.offset);

    // The stack size request has no data; the mere presence of a response
    // means Limine complied. If it did not, we are running on a smaller
    // stack than we sized our formatting code for.
    assert!(
        STACK.response().is_some(),
        "Limine did not honour the stack size request"
    );
    println!("[boot] stack: {} KiB (granted)", STACK_SIZE / 1024);

    // SAFETY: These four are linker-defined symbols. We only take their
    // addresses -- the storage behind them is never read or written, and
    // never dereferenced -- so there is no aliasing or initialisation
    // requirement to violate. The `static`s exist solely to give the symbols
    // a name the borrow checker can attach a lifetime to.
    let (image_start, image_end, bss_start, bss_end) = unsafe {
        (
            linker_symbol(&__kernel_image_start),
            linker_symbol(&__kernel_image_end),
            linker_symbol(&__bss_start),
            linker_symbol(&__bss_end),
        )
    };
    println!(
        "[boot] image: {:#x} .. {:#x} ({} KiB)",
        image_start,
        image_end,
        (image_end - image_start) / 1024
    );
    println!("[boot] bss:   {:#x} .. {:#x}", bss_start, bss_end);

    // A higher-half kernel that is not actually in the higher half means the
    // link address and the code model disagree, and every absolute address
    // we compute from here would be wrong.
    assert!(
        virtual_base >= HIGHER_HALF_BASE,
        "executable virtual base is not in the higher half"
    );

    // Limine hands back an empty string rather than a null pointer when no
    // command line was configured.
    let cmdline = CMDLINE.response().map(|r| r.cmdline()).unwrap_or("");
    println!(
        "[boot] cmdline: {}",
        if cmdline.is_empty() {
            "<none>"
        } else {
            cmdline
        }
    );

    // ---- CPU structures (M1) -------------------------------------------
    //
    // Order matters. The IDT's gates name a code selector from our GDT, and
    // the double-fault gate's IST index is meaningless until the TSS is
    // loaded, so the GDT must come first.
    //
    // Interrupts are still disabled at this point: Limine enters the kernel
    // with IF clear and nothing above re-enables it. That is what makes it
    // safe to replace the descriptor tables out from under the CPU.

    // SAFETY: First and only call, on the single-threaded boot path with
    // interrupts disabled (Limine guarantees IF is clear on entry and we
    // never set it). Replacing the GDT invalidates the selectors cached
    // from the bootloader's table, which is why `gdt::init` reloads CS, the
    // data registers and the task register before returning.
    unsafe {
        gdt::init();
    }
    println!(
        "[cpu] gdt: loaded (cs={:#06x}, ds={:#06x}, tss armed)",
        gdt::KERNEL_CODE_SELECTOR,
        gdt::KERNEL_DATA_SELECTOR
    );

    let (ist_low, ist_high) = gdt::ist1_range();
    println!(
        "[cpu] ist1: {:#018x} .. {:#018x} ({} KiB) for #DF",
        ist_low,
        ist_high,
        (ist_high - ist_low) / 1024
    );

    // SAFETY: First and only call, immediately after `gdt::init`, still
    // with interrupts disabled. Every gate references KERNEL_CODE_SELECTOR,
    // which the GDT now provides, and the #DF gate's IST1 slot is armed.
    unsafe {
        idt::init();
    }
    println!(
        "[cpu] idt: {} exception vectors installed, #DF on IST{}",
        interrupts::EXCEPTION_COUNT,
        gdt::DOUBLE_FAULT_IST_INDEX
    );
    println!(
        "[cpu] gdt: user cs={:#06x}, user ss={:#06x} (ring 3, ADR 029)",
        gdt::USER_CODE_SELECTOR,
        gdt::USER_DATA_SELECTOR
    );

    // ---- CPU features (M3) ---------------------------------------------
    //
    // Before any page table, because EFER.NXE decides whether bit 63 of an
    // entry means "no execute" or "reserved, fault on use". Every NX
    // mapping built below depends on it.

    // SAFETY: First and only call, single-threaded, interrupts disabled.
    // Modifies EFER and CR4; no page table currently in use sets bit 63,
    // so changing that bit's meaning cannot invalidate a live mapping.
    //
    // The result is also stashed in a static, because `kmain` runs on a
    // different stack and nothing in this frame survives the switch.
    unsafe {
        cpu::init();
    }

    // ---- syscall/sysret (M8) -------------------------------------------
    //
    // After `gdt::init`, because STAR names selectors from the GDT and
    // the const assertions there tie its arithmetic to the layout. The
    // stub itself needs nothing but an address.
    //
    // Enabled here rather than later so that `syscall` is a working
    // instruction before any thread exists -- there is then no window
    // in which a user thread could be scheduled with SCE still clear,
    // where its first syscall would be #UD.
    //
    // SAFETY: First and only call, after `gdt::init`, with interrupts
    // disabled. The entry point obeys ADR 030's contract.
    unsafe {
        cpu::enable_syscall(syscall::entry_address());
    }

    // ---- Physical memory (M2) ------------------------------------------
    //
    // After the IDT, so that a fault while walking the memory map produces
    // a report rather than a triple fault.

    let kernel_slide = virtual_base.wrapping_sub(physical_base);

    // SAFETY: First and only call, on the single-threaded boot path. The
    // HHDM offset and the kernel slide come from the responses validated
    // above, so they describe this boot. Nothing has allocated a frame yet,
    // which is what lets the allocator claim storage for its own bitmap out
    // of the memory it is about to manage.
    unsafe {
        memory::init(&MEMMAP, hhdm.offset, kernel_slide);
    }

    // ---- Virtual memory (M3) -------------------------------------------
    //
    // Copy before switching. Limine's responses live in memory we are about
    // to reclaim, and the crate hands them out as `&'static`, so nothing
    // will warn when they go stale. See ADR 013.

    // SAFETY: First and only call, while the responses are still valid.
    unsafe {
        memory::bootinfo::capture(&BOOTLOADER_INFO, &CMDLINE, &MEMMAP);
    }

    memory::vm::set_kernel_slide(kernel_slide);

    let memmap_entries = MEMMAP
        .response()
        .expect("Limine did not answer the memory map request")
        .entries();

    // SAFETY: After cpu::init (NX enabled, or the NX bits below would be
    // reserved) and after the frame allocator is up, since building tables
    // needs frames. Interrupts are disabled.
    unsafe {
        memory::vm::init(memmap_entries);
    }

    // IST1 moved during vm::init; report where it landed now rather than
    // the bootstrap address printed earlier.
    let (ist_low, ist_high) = gdt::ist1_range();
    println!(
        "[cpu] ist1 now: {:#018x} .. {:#018x} ({} KiB), guard page below",
        ist_low,
        ist_high,
        (ist_high - ist_low) / 1024
    );

    // ---- Off the bootloader's stack ------------------------------------
    //
    // Everything below this line runs on a stack the kernel allocated, and
    // nothing in this frame reaches it: changing RSP abandons the locals,
    // the saved registers and the return address all at once. That is why
    // `kstart` ends here instead of calling `kmain` and carrying on.
    //
    // The reason it must happen at all is `reclaim`. Limine's stack is in
    // BOOTLOADER_RECLAIMABLE memory -- on this machine at physical
    // 0x0ff6e000, inside the 0x0ff45000..0x0ffdf000 reclaimable region --
    // so poisoning that memory while standing on it corrupts the live
    // stack. The kernel took a #GP at rip=0xdededededededede exactly once
    // before this existed, which was the poison pattern arriving via `ret`.
    //
    // SAFETY: vm::init mapped the main stack in the tables now in CR3 and
    // verified them before the switch; interrupts are still disabled; and
    // `kmain` diverges.
    unsafe { memory::vm::switch_to_main_stack(kmain) }
}

/// The kernel proper, running on a stack the kernel owns.
///
/// Reached only through [`memory::vm::switch_to_main_stack`], which jumps
/// rather than calls -- there is no return address on the stack and no
/// caller to return to.
///
/// `extern "C"` so that the jump in the stack-switch assembly is to a
/// function with a known ABI rather than to a Rust item whose calling
/// convention is unspecified.
extern "C" fn kmain() -> ! {
    println!("[boot] now on the kernel's own stack");

    // Off the bootloader's tables *and* off its stack, so its memory is
    // ours to take. SAFETY: After capture(), after vm::init switched CR3,
    // and after the stack switch above; reclaim asserts all three before
    // releasing anything.
    let reclaimed = unsafe { memory::bootinfo::reclaim() };

    // The copies had to outlive the memory they came from -- that was the
    // entire point of capture(). Reading them back here proves it on every
    // boot, not only in the vm-test scenario.
    let boot_info = memory::bootinfo::get();
    println!(
        "[boot] after reclaim: bootloader still reported as {} {} ({} frames gained)",
        boot_info.bootloader_name(),
        boot_info.bootloader_version(),
        reclaimed
    );

    // The command line comes from our copy, never from Limine's response.
    // The pointer Limine returned addresses memory that reclaim has just
    // poisoned and handed to the allocator; reading it now yields 0xDE
    // bytes. `kstart`'s binding cannot reach here -- it died with the old
    // stack -- which is a sturdier guarantee than remembering not to use
    // it.
    let cmdline = boot_info.cmdline();

    // What the CPU supports, from the static cpu::init filled in. The
    // `Features` value it returned lived in `kstart`'s frame, which no
    // longer exists.
    let features = cpu::features();

    // ---- The heap's physical budget (M5) --------------------------------
    //
    // Before heap::init, not after. From M5 the heap draws its pages from
    // a reserve taken here rather than calling the frame allocator on
    // demand, because ADR 022 retires that allocator at the end of boot
    // and a lazily-growing heap can never satisfy its precondition.
    //
    // Putting this after heap::init looked right and was not: init maps
    // the first page, which would draw from a reserve that did not exist
    // yet. The tripwire in heap_frame_source caught it on the first run.
    //
    // SAFETY: First and only call, after vm::init and reclaim so the
    // allocator holds everything it is going to hold, and before
    // heap::init.
    unsafe {
        cap::boot::reserve_heap_budget();
    }

    // ---- Kernel heap (M4) ----------------------------------------------
    //
    // After reclaim, so the frames the bootloader gave back are available
    // to the heap, and after vm::init, since growing the heap maps pages
    // through the kernel's own tables.
    //
    // SAFETY: First and only call, single-threaded, interrupts disabled,
    // with the page tables and the reserve both in place.
    unsafe {
        memory::heap::init();
    }

    // ---- Scenarios that need the frame allocator alive -----------------
    //
    // The frame allocator's life ends a few lines below, when
    // `cap::boot::init` hands every remaining frame to the capability
    // system and retires it (ADR 022). Anything that allocates directly
    // -- the M2 allocator tests, and every M3 scenario, since mapping a
    // page needs frames for the intermediate tables -- has to run before
    // that point.
    //
    // This split is not bookkeeping; it is the allocator's lifetime made
    // visible. The alternative, discovered by running it, is that
    // `mem-test` panics with "65338 frames are held by the kernel's own
    // structures" -- which was true, and was the capability system
    // holding them.
    //
    // Each scenario diverges (exits QEMU), so none of them returns here.

    // Negative-path hook inherited from M0: a plain panic, no exception
    // involved. First, because it needs nothing at all.
    if cmdline.contains("panic-test") {
        println!("[boot] cmdline requested a panic; triggering now");
        panic!("deliberate panic requested via cmdline (token=panic-test)");
    }

    // The frame allocator tests run against the real memory map of the
    // machine we booted on, and the kernel checks its own invariants --
    // see kernel/src/memtest.rs.
    if cmdline.contains("mem-double-free") {
        println!("[boot] cmdline requested the double-free test");
        memtest::run_double_free();
    }

    if cmdline.contains("mem-test") {
        println!("[boot] cmdline requested the frame allocator tests");
        memtest::run();
    }

    // M3 virtual memory scenarios. Every one of these maps or unmaps,
    // and mapping allocates the intermediate tables.
    if cmdline.contains("vm-stale-tlb") {
        println!("[boot] cmdline requested the stale-TLB check");
        vmtest::run_stale_tlb();
    }

    if cmdline.contains("vm-guard-page") {
        println!("[boot] cmdline requested the guard page test");
        vmtest::run_guard_page();
    }

    if cmdline.contains("vm-nx") {
        println!("[boot] cmdline requested the NX test");
        vmtest::run_nx();
    }

    if cmdline.contains("vm-test") {
        println!("[boot] cmdline requested the virtual memory tests");
        vmtest::run(&features);
    }

    if cmdline.contains("vm-stack-overflow") {
        println!("[boot] cmdline requested the main stack overflow test");
        vmtest::run_stack_overflow();
    }

    // ---- Capability system (M5) ----------------------------------------
    //
    // This retires the frame allocator. After it, the only source of
    // memory is retyping an Untyped (ADR 022).

    cap::report_sizes();

    let memmap_entries = memory::bootinfo::get().memory_map();
    // SAFETY: First and only call, after the heap is up and after every
    // scenario that needs the frame allocator, on the single-threaded
    // boot path with interrupts disabled.
    unsafe {
        cap::boot::init_from_regions(memmap_entries);
    }

    // ---- Threads and interrupts (M6) -----------------------------------
    //
    // Order is forced and each step explains the next:
    //
    //   1. A TCB for the code already running. The first context
    //      switch needs somewhere to save registers *from*, and that
    //      has to exist before anything can switch.
    //   2. The LAPIC timer, armed but not yet able to fire -- IF is
    //      still clear.
    //   3. `sti`. From here the assumption every line written since M2
    //      relied on ("interrupts are disabled") stops holding, and
    //      ADR 025's named regions take over.
    //
    // Doing 3 before 1 would mean a timer tick arriving with no
    // current thread; doing 3 before 2 would mean enabling interrupts
    // with nothing to deliver, which is harmless but pointless.

    // SAFETY: First and only call, after the capability system is up.
    unsafe {
        let boot_tcb = cap::boot::create_boot_tcb();
        sched::init(boot_tcb);
    }

    // SAFETY: First and only call, after the IDT is loaded and with
    // interrupts still disabled. Maps the LAPIC page, which needs the
    // kernel's tables and the heap's frame reserve.
    let lapic_hz = unsafe { apic::init() };
    let _ = lapic_hz;

    // ---- The I/O APIC (M10's debt, paid in M12) ------------------------
    //
    // After `apic::init`, because it maps the page next to the
    // LAPIC's and reuses the same KMMIO region. Still with
    // interrupts disabled, which is what `set_masked` requires.
    //
    // Every line comes up masked. If there is no chip, this says so
    // and the software flag carries the contract alone — a kernel
    // that refused to boot without an I/O APIC would be refusing
    // over a capability it does not yet need.
    //
    // SAFETY: First and only call, after the LAPIC is mapped, with
    // interrupts disabled.
    unsafe {
        ioapic::init();
    }

    // ---- Address spaces (M7) -------------------------------------------
    //
    // Snapshot which top-level entries the kernel occupies, so every
    // VSpace created later can inherit exactly those -- and so that a
    // *new* one appearing afterwards is caught rather than silently
    // leaving older address spaces incomplete (ADR 027, K2).
    //
    // This has to come after `apic::init`, which maps the LAPIC into
    // KMMIO. Running it earlier would record a set that the LAPIC
    // mapping then added to, and the first VSpace creation would fail
    // the very check this exists to perform. The ordering is load-
    // bearing, so it is asserted rather than assumed: K2 compares
    // against this snapshot on every `create_vspace`.
    //
    // SAFETY: Every kernel mapping now exists -- image, PMAP, stacks,
    // heap and the LAPIC page.
    unsafe {
        cap::vspace::record_kernel_entries();
    }

    // SAFETY: The IDT has handlers for every vector that can now fire,
    // and the scheduler has a current thread.
    unsafe {
        sched::enable_interrupts();
    }
    println!(
        "[boot] interrupts enabled; the \"IF is always clear\" assumption \
         that held from M2 to M5 is now replaced by ADR 025's named regions"
    );

    // ---- Scenarios that run on a capability-only kernel ----------------
    //
    // Everything below sees a retired frame allocator and enabled
    // interrupts. The heap still works, because it has its own
    // reserve.

    // #BP is a trap, so this one returns and the kernel carries on to exit
    // successfully. That is the whole point: it proves the dispatcher's
    // `iretq` path works, not just that an exception was caught.
    if cmdline.contains("fault-int3") {
        println!("[boot] cmdline requested a breakpoint exception");
        faults::trigger_breakpoint();
        println!("[boot] execution continued past the breakpoint");
    }

    if cmdline.contains("fault-pf") {
        println!("[boot] cmdline requested a page fault");
        faults::trigger_page_fault();
    }

    if cmdline.contains("fault-df") {
        println!("[boot] cmdline requested a double fault");
        faults::trigger_double_fault();
    }

    // M4 heap scenarios. These run after retirement on purpose: the heap
    // draws from its own reserve, so it must keep working once the frame
    // allocator is gone, and running them here is what shows it does.
    if cmdline.contains("heap-oom") {
        println!("[boot] cmdline requested the heap exhaustion test");
        heaptest::run_oom();
    }

    if cmdline.contains("heap-guard") {
        println!("[boot] cmdline requested the heap guard page test");
        heaptest::run_guard();
    }

    if cmdline.contains("heap-test") {
        println!("[boot] cmdline requested the heap tests");
        heaptest::run();
    }

    // M5 capability scenarios.
    //
    // `cap-test` is checked last — and M9c proved why that mattered.
    //
    // The note here used to read "it is a substring of nothing here",
    // which was true when written and stopped being true the moment
    // M9c added a scenario. That one was spelled `ipc-cap-test`, which
    // *contains* `cap-test`, so the capability suite ran first, filled
    // the object CNode, and the IPC scenario died on `SlotNotEmpty` —
    // a fixture failure reported from a test nobody had asked to run.
    //
    // The IPC scenarios are now spelled `ipc-captransfer` and friends,
    // with no token containing another. Ordering rules are a fragile
    // way to hold this invariant: each one has to be remembered by
    // whoever adds the next scenario, and the failure is silent. Names
    // that cannot overlap need remembering once.
    //
    // `cap-test` is a substring of nothing here, and the one that
    // the three other tokens all begin "cap-", and `contains` does not
    // care about word boundaries; ordering the specific ones first means
    // a future token like "cap-test-extra" cannot silently run the wrong
    // scenario.
    if cmdline.contains("cap-revoke-depth") {
        println!("[boot] cmdline requested the revoke cost measurement");
        captest::run_revoke_depth();
    }

    if cmdline.contains("cap-interrupt-alloc") {
        println!("[boot] cmdline requested the interrupt-allocation tripwire");
        captest::run_interrupt_alloc();
    }

    if cmdline.contains("cap-retired-allocator") {
        println!("[boot] cmdline requested the retired-allocator tripwire");
        captest::run_retired_allocator();
    }

    if cmdline.contains("cap-test") {
        println!("[boot] cmdline requested the capability tests");
        captest::run();
    }

    // M6 thread scenarios. Longest token first, as above.
    if cmdline.contains("thread-stack-overflow") {
        println!("[boot] cmdline requested the thread stack overflow test");
        threadtest::run_stack_overflow();
    }

    if cmdline.contains("thread-interrupt-alloc") {
        println!("[boot] cmdline requested the real-interrupt allocation tripwire");
        threadtest::run_interrupt_alloc();
    }

    if cmdline.contains("thread-kill-running") {
        println!("[boot] cmdline requested the kill-a-running-thread test");
        threadtest::run_kill_running();
    }

    if cmdline.contains("thread-test") {
        println!("[boot] cmdline requested the thread tests");
        threadtest::run();
    }

    // M11 user-space address-space scenarios.
    //
    // **Before** M7's block, not after, and the reason is the rule
    // M9c arrived at the hard way: `contains` does not care about
    // word boundaries. Every token here begins `vspace-user-`, and
    // `vspace-user-kernel-half` contains neither `vspace-kernel-half`
    // nor `vspace-test` -- but `vspace-user-test` would match a
    // hypothetical `vspace-user` check, and M7's `vspace-test` is
    // checked last within its own block for the same reason.
    //
    // Ordering within this block: the longest first.
    if cmdline.contains("vspace-user-kernel-half") {
        println!("[boot] cmdline requested the user kernel-half test");
        vspaceuser::run_kernel_half();
    }

    if cmdline.contains("vspace-user-foreign") {
        println!("[boot] cmdline requested the foreign-vspace test");
        vspaceuser::run_foreign();
    }

    if cmdline.contains("vspace-user-exhaust") {
        println!("[boot] cmdline requested the budget exhaustion test");
        vspaceuser::run_exhaust();
    }

    if cmdline.contains("vspace-user-ro") {
        println!("[boot] cmdline requested the read-only enforcement test");
        vspaceuser::run_read_only();
    }

    if cmdline.contains("vspace-user-test") {
        println!("[boot] cmdline requested the user address space tests");
        vspaceuser::run();
    }

    // M7 address-space scenarios. Longest token first, as above.
    if cmdline.contains("vspace-kernel-half") {
        println!("[boot] cmdline requested the kernel-half mapping test");
        vspacetest::run_kernel_half();
    }

    if cmdline.contains("vspace-isolation") {
        println!("[boot] cmdline requested the isolation test");
        vspacetest::run_isolation();
    }

    if cmdline.contains("vspace-stale-cr3") {
        println!("[boot] cmdline requested the revoked-vspace test");
        vspacetest::run_stale_cr3();
    }

    if cmdline.contains("vspace-test") {
        println!("[boot] cmdline requested the address space tests");
        vspacetest::run();
    }

    // M8 ring 3 scenarios. Longest token first, as above: `contains`
    // does not care about word boundaries, and every token here
    // begins "user-".
    if cmdline.contains("user-privileged") {
        println!("[boot] cmdline requested the privileged-instruction test");
        usertest::run_privileged();
    }

    if cmdline.contains("user-kernel-read") {
        println!("[boot] cmdline requested the kernel-read test");
        usertest::run_kernel_read();
    }

    if cmdline.contains("user-cap-forge") {
        println!("[boot] cmdline requested the capability forgery test");
        usertest::run_cap_forge();
    }

    if cmdline.contains("user-smep") {
        println!("[boot] cmdline requested the SMEP test");
        usertest::run_smep();
    }

    if cmdline.contains("user-smap") {
        println!("[boot] cmdline requested the SMAP test");
        usertest::run_smap();
    }

    if cmdline.contains("user-test") {
        println!("[boot] cmdline requested the ring 3 tests");
        usertest::run();
    }

    // M9a IPC scenarios. Longest token first, as above: `contains`
    // does not care about word boundaries and every token here
    // begins "ipc-".
    if cmdline.contains("ipc-endpoint-revoked") {
        println!("[boot] cmdline requested the revoked-endpoint test");
        ipctest::run_endpoint_revoked();
    }

    if cmdline.contains("ipc-cancel-cost") {
        println!("[boot] cmdline requested the cancellation cost measurement");
        ipctest::run_cancel_cost();
    }

    // M9b, before the M9a line below: `ipc-call-test` *contains*
    // "ipc-test" as a substring, so the shorter check would match
    // first and silently run the wrong scenario. Ordering rather than
    // a tighter pattern, because the tokens are chosen here and the
    // ordering is one line; a regex would be a second thing to keep
    // right.
    if cmdline.contains("ipc-call-test") {
        println!("[boot] cmdline requested the call/reply tests");
        ipctest::run_call();
    }

    if cmdline.contains("ipc-server-dies") {
        println!("[boot] cmdline requested the dead-server test");
        ipctest::run_server_dies();
    }

    if cmdline.contains("ipc-stale-reply") {
        println!("[boot] cmdline requested the stale-reply test");
        ipctest::run_stale_reply();
    }

    if cmdline.contains("ipc-self-call") {
        println!("[boot] cmdline requested the self-call test");
        ipctest::run_self_call();
    }

    // M9c's scenarios. Deliberately **not** spelled `ipc-cap-test`:
    // that contains `cap-test`, which dispatches M5's capability
    // suite, and the first run of this scenario died inside it with
    // `SlotNotEmpty`. See the note above `cap-test`.
    if cmdline.contains("ipc-captransfercost") {
        println!("[boot] cmdline requested the capability transfer measurement");
        ipctest::run_cap_cost();
    }

    if cmdline.contains("ipc-captransferhostile") {
        println!("[boot] cmdline requested the hostile capability transfer test");
        ipctest::run_cap_hostile();
    }

    // After the two above, because `ipc-captransfer` is a prefix of
    // both. Prefixes are the one overlap left, and they are at least
    // visible in a single list -- unlike `cap-test` buried in another
    // milestone's block.
    if cmdline.contains("ipc-captransfer") {
        println!("[boot] cmdline requested the capability transfer tests");
        ipctest::run_cap_transfer();
    }

    if cmdline.contains("ipc-notification-revoked") {
        println!("[boot] cmdline requested the revoked-notification test");
        ipctest::run_notification_revoked();
    }

    if cmdline.contains("ipc-cost") {
        println!("[boot] cmdline requested the IPC cost measurement");
        ipctest::run_ipc_cost();
    }

    if cmdline.contains("ipc-test") {
        println!("[boot] cmdline requested the IPC tests");
        ipctest::run();
    }

    // M10 interrupt scenarios.
    //
    // Every token begins "irq-" and none contains another, which is
    // the rule M9c arrived at the hard way: `ipc-cap-test` contained
    // `cap-test` and silently dispatched a different milestone's
    // suite. Names that cannot overlap need remembering once;
    // ordering rules need remembering by whoever adds the next
    // scenario.
    //
    // `irq-alloc` in particular is *not* spelled
    // `irq-interrupt-alloc`, which would contain M5's
    // `cap-interrupt-alloc`... it would not, in fact, but the near
    // miss is the kind of thing that only has to be wrong once.
    if cmdline.contains("irq-no-cap") {
        println!("[boot] cmdline requested the no-capability interrupt test");
        irqtest::run_no_cap();
    }

    if cmdline.contains("irq-revoked") {
        println!("[boot] cmdline requested the revoked-destination test");
        irqtest::run_revoked();
    }

    if cmdline.contains("irq-alloc") {
        println!("[boot] cmdline requested the device-interrupt allocation tripwire");
        irqtest::run_alloc();
    }

    if cmdline.contains("irq-user") {
        println!("[boot] cmdline requested the ring 3 interrupt test");
        irqtest::run_user();
    }

    if cmdline.contains("irq-test") {
        println!("[boot] cmdline requested the interrupt tests");
        irqtest::run();
    }

    // ---- The root task (M12) -------------------------------------------
    //
    // **Last**, and after every scenario above, because this is the
    // point of no return: `roottask::start` closes the seal, and
    // from then on the kernel may not create an object, map a page
    // or start a thread on its own initiative (ADR 044). Every
    // scenario above does at least one of those in its fixture.
    //
    // The scenarios are therefore not "tests that run before the
    // root task" by accident of ordering — they are tests of a
    // kernel that still has powers the sealed one does not, and
    // each of them exits QEMU before reaching this line. They
    // exercise paths that exist only during boot.
    //
    // Negative (a) is arranged *before* the start, because what it
    // changes is what the handover loop finds — not a different
    // path through it.
    if cmdline.contains("root-no-untyped") {
        roottest::starve();
    }

    // **Only on a boot that asked for it**, and the first version of
    // this started the root task unconditionally — which was wrong
    // in a way worth recording.
    //
    // Most scenarios above exit QEMU and never return. `cap-test`
    // and a few others *do* return, which was harmless while nothing
    // followed them. It stopped being harmless here: `captest` calls
    // `clear_objects()`, which empties the object CNode — including
    // the IRQControl capability boot seeded at slot 254 — and the
    // root task then started on a machine whose capability state a
    // test had already dismantled. The failure was
    // `object slot 254 does not hold the IRQControl capability`,
    // reported against `cap-test`, which had itself passed.
    //
    // The general shape: a scenario that returns leaves the system
    // in whatever state it finished in, and that is fine precisely
    // *because* nothing runs afterwards. Adding an unconditional
    // step at the end of `kmain` broke an invariant nobody had
    // written down, because until now there was nothing to break.
    //
    // So the root task runs when a root scenario asked for it, and
    // the "no scenario" boot — `make run` — gets it too, because
    // that one is the real system rather than a test of it.
    let wants_root_task = cmdline.contains("root-test")
        || cmdline.contains("root-no-untyped")
        || cmdline.contains("root-dies")
        || cmdline.is_empty();

    if wants_root_task {
        // SAFETY: After `cap::boot::init_from_regions`, `sched::init`,
        // `apic::init` and `record_kernel_entries`, with the
        // scheduler running and interrupts enabled.
        unsafe { roottask::start() };
    }

    // `root-no-untyped` is checked before `root-test` so the longer
    // token wins; `contains` does not care about word boundaries
    // (the lesson M9c paid for).
    if cmdline.contains("root-no-untyped") {
        println!("[boot] cmdline requested the starved-root-task test");
        roottest::run_no_untyped();
    }

    if cmdline.contains("root-dies") {
        println!("[boot] cmdline requested the dead-root-task test");
        roottest::run_root_dies();
    }

    if cmdline.contains("root-test") {
        println!("[boot] cmdline requested the root task tests");
        roottest::run();
    }

    println!("[boot] M9a checks passed");
    println!(
        "[boot] exiting QEMU (write {:#04x} -> exit status {})",
        qemu::EXIT_SUCCESS,
        qemu::HOST_STATUS_SUCCESS
    );

    qemu::exit(qemu::EXIT_SUCCESS)
}
