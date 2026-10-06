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

mod cpu;
mod faults;
mod gdt;
mod heaptest;
mod idt;
mod interrupts;
mod io;
mod memory;
mod memtest;
mod panic;
mod qemu;
mod serial;
mod vmtest;

use limine::request::{
    BootloaderInfoRequest, ExecutableAddressRequest, ExecutableCmdlineRequest, HhdmRequest,
    MemmapRequest, StackSizeRequest,
};
use limine::{BaseRevision, RequestsEndMarker, RequestsStartMarker};

/// Name reported on the serial console.
const KERNEL_NAME: &str = "PythOS";

/// Version reported on the serial console. Kept in step with Cargo.toml.
const KERNEL_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "-M4");

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

    // ---- Kernel heap (M4) ----------------------------------------------
    //
    // After reclaim, so the frames the bootloader gave back are available
    // to the heap, and after vm::init, since growing the heap maps pages
    // through the kernel's own tables.
    //
    // SAFETY: First and only call, single-threaded, interrupts disabled,
    // with the page tables and the frame allocator both up.
    unsafe {
        memory::heap::init();
    }

    // ---- Fault-injection hooks -----------------------------------------
    //
    // Each of these is reached through a token on the kernel command line,
    // so the exception paths are exercised on the binary we ship rather
    // than on a special build.

    // Negative-path hook inherited from M0: a plain panic, no exception
    // involved.
    if cmdline.contains("panic-test") {
        println!("[boot] cmdline requested a panic; triggering now");
        panic!("deliberate panic requested via cmdline (token=panic-test)");
    }

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

    // M3 virtual memory scenarios.
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

    // M4 heap scenarios.
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

    println!("[boot] M4 checks passed");
    println!(
        "[boot] exiting QEMU (write {:#04x} -> exit status {})",
        qemu::EXIT_SUCCESS,
        qemu::HOST_STATUS_SUCCESS
    );

    qemu::exit(qemu::EXIT_SUCCESS)
}
