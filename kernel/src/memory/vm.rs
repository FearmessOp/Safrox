// SPDX-License-Identifier: BSD-2-Clause
//! Building our own address space and switching to it.
//!
//! Limine hands us a working set of page tables, but they are the
//! bootloader's: they live in `BOOTLOADER_RECLAIMABLE`, and the protocol
//! does not specify their permissions. We build our own so that
//!
//! * the physical window is NX and covers only RAM,
//! * the kernel image is mapped W^X, section by section,
//! * kernel stacks have genuinely unmapped guard pages,
//! * and the bootloader's memory becomes reclaimable.
//!
//! See ADR 010 for why, and docs/address-space.md for the layout.

use core::sync::atomic::{AtomicU64, Ordering};

use limine::memmap;

use super::frame_allocator;
use super::layout;
use super::paging::{self, FrameSource, PageFlags, PageTable};
use super::{PhysAddr, PhysFrame, VirtAddr, FRAME_SIZE};
use crate::println;

/// 2 MiB, the large page size at PD level.
const PAGE_2M: u64 = 2 * 1024 * 1024;

/// 1 GiB, the large page size at PDPT level.
const PAGE_1G: u64 = 1024 * 1024 * 1024;

/// Physical frame of the PML4 we build. Zero until [`init`] runs.
static KERNEL_PML4: AtomicU64 = AtomicU64::new(0);

/// Top of the kernel's main stack. Zero until [`init`] maps it.
static MAIN_STACK_TOP: AtomicU64 = AtomicU64::new(0);

/// Adapter that lets the paging code draw frames from M2's allocator.
///
/// The indirection is the point: `paging::map` never reaches for a global
/// allocator, so when M5 replaces this with an Untyped capability the
/// mapping code does not change. See ADR 011.
pub struct GlobalFrames;

impl FrameSource for GlobalFrames {
    fn alloc_frame(&mut self) -> Option<PhysFrame> {
        frame_allocator::allocate()
    }
}

// ---- Linker symbols ----------------------------------------------------

unsafe extern "C" {
    static __kernel_image_start: u8;
    static __kernel_image_end: u8;
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
    static __writable_start: u8;
    static __writable_end: u8;
}

/// Address of a linker-defined symbol.
///
/// These have no value, only an address; the storage behind them is never
/// read.
fn sym(symbol: &'static u8) -> u64 {
    core::ptr::from_ref(symbol) as u64
}

/// The kernel image's sections, as virtual address ranges.
struct ImageSections {
    image_start: u64,
    image_end: u64,
    text: (u64, u64),
    rodata: (u64, u64),
    writable: (u64, u64),
}

fn image_sections() -> ImageSections {
    // SAFETY: All eight are linker-defined symbols; only their addresses
    // are taken, never the storage behind them.
    unsafe {
        ImageSections {
            image_start: sym(&__kernel_image_start),
            image_end: sym(&__kernel_image_end),
            text: (sym(&__text_start), sym(&__text_end)),
            rodata: (sym(&__rodata_start), sym(&__rodata_end)),
            writable: (sym(&__writable_start), sym(&__writable_end)),
        }
    }
}

// ---- Building the tables -----------------------------------------------

/// Map `[virt, virt + size)` onto `[phys, phys + size)` using the largest
/// pages that fit.
///
/// Only used for the physical window, where every page has identical
/// permissions. The general `map` deals in 4 KiB pages and refuses to
/// guess about size (ADR 011).
///
/// # Safety
///
/// `table` must be a live PML4 under construction -- not the active one,
/// since this creates mappings in bulk without flushing.
unsafe fn map_window(
    table: &mut PageTable,
    virt: u64,
    phys: u64,
    size: u64,
    flags: PageFlags,
    source: &mut impl FrameSource,
) -> u64 {
    let mut mapped = 0u64;
    let mut offset = 0u64;

    while offset < size {
        let remaining = size - offset;
        let va = virt + offset;
        let pa = phys + offset;

        // Pick the largest page whose size divides both addresses and
        // fits in what is left. Fewer entries means fewer tables and less
        // TLB pressure.
        let step = if remaining >= PAGE_1G
            && va.is_multiple_of(PAGE_1G)
            && pa.is_multiple_of(PAGE_1G)
        {
            PAGE_1G
        } else if remaining >= PAGE_2M && va.is_multiple_of(PAGE_2M) && pa.is_multiple_of(PAGE_2M) {
            PAGE_2M
        } else {
            FRAME_SIZE
        };

        // SAFETY: The caller guarantees `table` is an inactive PML4 under
        // construction, so creating entries in it cannot affect the
        // translations in use right now.
        unsafe {
            map_one(table, VirtAddr::new(va), PhysAddr(pa), step, flags, source);
        }

        mapped += step;
        offset += step;
    }

    mapped
}

/// Map a single page of `size` bytes.
///
/// # Safety
///
/// As [`map_window`]. `size` must be 4 KiB, 2 MiB or 1 GiB, and both
/// addresses aligned to it.
unsafe fn map_one(
    table: &mut PageTable,
    virt: VirtAddr,
    phys: PhysAddr,
    size: u64,
    flags: PageFlags,
    source: &mut impl FrameSource,
) {
    if size == FRAME_SIZE {
        // SAFETY: The caller guarantees the table is under construction.
        let guard = unsafe {
            paging::map(table, virt, PhysFrame::containing(phys), flags, source)
                .expect("failed to map a page while building the kernel address space")
        };
        // Nothing is live yet: these tables are not in CR3, so there can be
        // no stale entry for them. One full flush follows the switch.
        guard.ignore_because_caller_will_flush_all();
    } else {
        // SAFETY: As above; `map_huge` checks the level/size pairing.
        unsafe {
            paging::map_huge(table, virt, phys, size, flags, source)
                .expect("failed to map a large page while building the address space");
        }
    }
}

/// Whether the frame allocator may hand out frames from this region, and
/// therefore whether it is real RAM that belongs in the physical window.
///
/// Four types, decided in M3: usable memory, both reclaimable kinds, and
/// the region holding the kernel image itself. Everything else --
/// `reserved`, the framebuffer, ACPI NVS, bad memory -- stays out. MMIO
/// needs a different cache type and gets its own range (KMMIO), and
/// mapping a device window as ordinary write-back RAM is a good way to
/// lose writes.
fn is_real_ram(type_: u64) -> bool {
    matches!(
        type_,
        memmap::MEMMAP_USABLE
            | memmap::MEMMAP_BOOTLOADER_RECLAIMABLE
            | memmap::MEMMAP_ACPI_RECLAIMABLE
            | memmap::MEMMAP_EXECUTABLE_AND_MODULES
    )
}

/// Build the kernel's page tables and switch to them.
///
/// After this returns, CR3 points at a table this kernel constructed, the
/// physical window is ours, the image is W^X, and the IST stack has a real
/// guard page.
///
/// # Safety
///
/// Call once, after [`crate::cpu::init`] (NX must be enabled before any
/// table sets the NX bit) and after the frame allocator is up. Interrupts
/// must be disabled.
pub unsafe fn init(entries: &[&memmap::Entry]) {
    // The window we are about to build must land where Limine's already is.
    // If it did not, every pointer computed before the switch -- the frame
    // allocator's bitmap among them -- would silently point somewhere else.
    // Stopping is better than recomputing and missing one. See ADR 010.
    let hhdm = super::hhdm_offset();
    assert_eq!(
        hhdm,
        layout::PMAP_BASE,
        "Limine put its HHDM at {:#x} but PMAP_BASE is {:#x}; the switch \
         would invalidate every pointer computed before it",
        hhdm,
        layout::PMAP_BASE
    );

    let mut source = GlobalFrames;

    let pml4_frame = source
        .alloc_frame()
        .expect("no frame available for the kernel PML4");

    // SAFETY: The frame came from the allocator, so nothing else owns it,
    // and Limine's window maps all RAM writably. Zeroing before use: a
    // recycled frame holds whatever the last owner left, and stale bits in
    // a PML4 are bogus mappings.
    let pml4 = unsafe {
        let ptr = super::phys_to_virt(pml4_frame.start()).as_ptr::<PageTable>();
        ptr.write(PageTable::empty());
        &mut *ptr
    };

    println!(
        "[vm] building kernel address space (pml4 at {})",
        pml4_frame
    );

    // --- The physical window --------------------------------------------
    //
    // RW, NX, GLOBAL. Only real RAM: see is_real_ram.
    //
    // GLOBAL because these mappings are identical in every address space;
    // marking them so keeps them in the TLB across the CR3 reloads that
    // address-space switching will bring in M11. It also means changing
    // one later needs invlpg, not just a CR3 write (ADR 012).
    let window_flags = PageFlags::READ | PageFlags::WRITE | PageFlags::GLOBAL;

    let mut window_bytes = 0u64;
    for entry in entries {
        if !is_real_ram(entry.type_) {
            continue;
        }

        // Round out to page boundaries: a region that starts or ends
        // mid-page still needs its partial pages mapped, or an access near
        // the edge faults.
        let start = entry.base & !(FRAME_SIZE - 1);
        let end = (entry.base + entry.length).div_ceil(FRAME_SIZE) * FRAME_SIZE;

        assert!(
            end <= layout::PMAP_SIZE,
            "physical address {:#x} is beyond the {} TiB window",
            end,
            layout::PMAP_SIZE / (1024 * 1024 * 1024 * 1024)
        );

        // SAFETY: `pml4` is under construction and not in CR3.
        window_bytes += unsafe {
            map_window(
                pml4,
                layout::PMAP_BASE + start,
                start,
                end - start,
                window_flags,
                &mut source,
            )
        };
    }

    println!(
        "[vm] pmap: {} MiB of RAM at {:#018x}, rw+nx+global",
        window_bytes / (1024 * 1024),
        layout::PMAP_BASE
    );

    // --- The kernel image, section by section ---------------------------
    //
    // This is where W^X is won. Limine maps the image as one blob; we map
    // .text read-execute, .rodata read-only, and everything writable
    // read-write+NX.
    let sections = image_sections();
    let slide = KERNEL_SLIDE_CACHE.load(Ordering::Acquire);

    // SAFETY: `pml4` is under construction.
    unsafe {
        map_image_section(
            pml4,
            "text",
            sections.text,
            slide,
            PageFlags::READ | PageFlags::EXEC | PageFlags::GLOBAL,
            &mut source,
        );
        map_image_section(
            pml4,
            "rodata",
            sections.rodata,
            slide,
            PageFlags::READ | PageFlags::GLOBAL,
            &mut source,
        );
        map_image_section(
            pml4,
            "writable",
            sections.writable,
            slide,
            PageFlags::READ | PageFlags::WRITE | PageFlags::GLOBAL,
            &mut source,
        );
    }

    // --- Kernel stacks --------------------------------------------------
    //
    // IST1 moves out of the image and into KSTACK, with an unmapped page
    // below it. In M1 this stack sat at the start of .bss, directly above
    // the Limine response structures, and an overflow overwrote them
    // silently. M2 put dead space below it, which was better but still
    // mapped. Now the page below is genuinely absent and an overflow
    // faults.
    let (guard, bottom, top) = layout::kstack_slot(layout::KSTACK_SLOT_IST1);

    // SAFETY: `pml4` is under construction.
    let stack_frames = unsafe { map_stack(pml4, bottom, layout::KSTACK_SIZE, &mut source) };

    println!(
        "[vm] kstack[{}]: {} .. {} ({} KiB, {} frames), guard page at {} (ist1)",
        layout::KSTACK_SLOT_IST1,
        bottom,
        top,
        layout::KSTACK_SIZE / 1024,
        stack_frames,
        guard
    );

    // The main stack. The kernel is still running on the one Limine gave
    // it, and that stack is *inside* bootloader-reclaimable memory -- on
    // this machine at physical 0x0ff6e000, within the
    // 0x0ff45000..0x0ffdf000 reclaimable region. Reclaiming without moving
    // first poisons the live stack, and the next `ret` loads 0xdedededede-
    // dededede off it. That is not a hypothetical: it is what the first
    // version of this code did. See ADR 013.
    let (main_guard, main_bottom, main_top) = layout::kstack_slot(layout::KSTACK_SLOT_MAIN);

    // SAFETY: `pml4` is under construction.
    let main_frames = unsafe { map_stack(pml4, main_bottom, layout::KSTACK_SIZE, &mut source) };

    MAIN_STACK_TOP.store(main_top.0, Ordering::Release);

    println!(
        "[vm] kstack[{}]: {} .. {} ({} KiB, {} frames), guard page at {} (main)",
        layout::KSTACK_SLOT_MAIN,
        main_bottom,
        main_top,
        layout::KSTACK_SIZE / 1024,
        main_frames,
        main_guard
    );

    // --- Verify before switching ----------------------------------------
    //
    // The instant CR3 changes, the next instruction fetch goes through the
    // new tables. If the code we are executing is not mapped there, that
    // fetch faults -- and if the IDT or the fault handler's stack is also
    // unmapped, the fault cannot be delivered either: triple fault, silent
    // reset, no output at all. So check first.
    //
    // SAFETY: `pml4` is a table we built; nothing else references it.
    unsafe {
        verify_before_switch(pml4, &sections);
    }

    KERNEL_PML4.store(pml4_frame.start().0, Ordering::Release);

    println!("[vm] switching cr3 to {} ...", pml4_frame);

    // SAFETY: verify_before_switch has just confirmed that the current
    // instruction pointer, the current stack and the IDT are all mapped in
    // the new tables with usable permissions.
    unsafe {
        paging::load_table(pml4_frame);
    }

    // Writing CR3 drops non-global entries, but the kernel mappings we
    // just made are GLOBAL and would survive from the old tables if any
    // were cached. Nothing stale should exist -- the addresses are new --
    // but a full flush here costs one instruction at boot and removes the
    // question.
    paging::flush_all();

    println!("[vm] now running on our own page tables");

    // Prove the window works by reading through it: translate our own
    // .text start and check it round-trips.
    // SAFETY: `pml4` is now the active table and still uniquely borrowed.
    unsafe {
        let probe = VirtAddr::new(sections.text.0);
        let (phys, flags) = paging::translate(pml4, probe)
            .expect("kernel .text is not mapped in the table we just installed");
        println!("[vm] self-check: {} -> {} {:?}", probe, phys, flags);
    }

    // Hand IST1 its new stack. Until this point the TSS still points at the
    // old in-image stack, which is no longer mapped -- so a double fault
    // between the CR3 switch and here would be unrecoverable. The window is
    // a few dozen instructions with interrupts disabled.
    // SAFETY: The stack is mapped, 16-byte aligned by construction, and
    // nothing is using it yet.
    unsafe {
        crate::gdt::set_ist1(top.0);
    }
    println!("[vm] ist1 now points at the guarded stack");
}

/// Cache of the kernel image slide, so `init` does not need it passed in.
static KERNEL_SLIDE_CACHE: AtomicU64 = AtomicU64::new(0);

/// Record the kernel's virtual-minus-physical slide for [`init`].
pub fn set_kernel_slide(slide: u64) {
    KERNEL_SLIDE_CACHE.store(slide, Ordering::Release);
}

/// Map one section of the kernel image with the permissions it needs.
///
/// # Safety
///
/// `table` must be under construction.
unsafe fn map_image_section(
    table: &mut PageTable,
    name: &str,
    range: (u64, u64),
    slide: u64,
    flags: PageFlags,
    source: &mut impl FrameSource,
) {
    let (start, end) = range;
    assert!(
        start.is_multiple_of(FRAME_SIZE) && end.is_multiple_of(FRAME_SIZE),
        "section {} is not page aligned ({:#x}..{:#x}); permissions have \
         page granularity, so two sections sharing a page would force the \
         looser permission onto both",
        name,
        start,
        end
    );

    let mut pages = 0u64;
    let mut virt = start;
    while virt < end {
        let phys = PhysAddr(virt - slide);
        // SAFETY: The caller guarantees the table is under construction.
        let guard = unsafe {
            paging::map(
                table,
                VirtAddr::new(virt),
                PhysFrame::containing(phys),
                flags,
                source,
            )
            .expect("failed to map a kernel image page")
        };
        guard.ignore_because_caller_will_flush_all();

        pages += 1;
        virt += FRAME_SIZE;
    }

    println!(
        "[vm] image .{}: {:#018x}..{:#018x} ({} pages) {:?}",
        name, start, end, pages, flags
    );
}

/// Map a kernel stack. The page below `bottom` is left unmapped.
///
/// # Safety
///
/// `table` must be under construction.
unsafe fn map_stack(
    table: &mut PageTable,
    bottom: VirtAddr,
    size: u64,
    source: &mut impl FrameSource,
) -> u64 {
    let flags = PageFlags::READ | PageFlags::WRITE | PageFlags::GLOBAL;

    let mut frames = 0u64;
    let mut offset = 0u64;
    while offset < size {
        let frame = source
            .alloc_frame()
            .expect("no frame available for a kernel stack");

        // SAFETY: The caller guarantees the table is under construction.
        let guard = unsafe {
            paging::map(
                table,
                VirtAddr::new(bottom.0 + offset),
                frame,
                flags,
                source,
            )
            .expect("failed to map a kernel stack page")
        };
        guard.ignore_because_caller_will_flush_all();

        frames += 1;
        offset += FRAME_SIZE;
    }

    frames
}

/// Confirm the new tables can keep the kernel running before CR3 changes.
///
/// # Safety
///
/// `table` must be the PML4 about to be installed.
unsafe fn verify_before_switch(table: &PageTable, sections: &ImageSections) {
    // Every page of the image. Missing even one means a fetch or a data
    // access could fault immediately after the switch.
    let mut virt = sections.image_start;
    let mut checked = 0u64;
    while virt < sections.image_end {
        // SAFETY: The caller guarantees `table` is a live PML4.
        let result = unsafe { paging::translate(table, VirtAddr::new(virt)) };
        assert!(
            result.is_ok(),
            "kernel image page {:#018x} is not mapped in the new tables; \
             switching would fault on the next instruction",
            virt
        );
        checked += 1;
        virt += FRAME_SIZE;
    }

    // The current stack. Limine gave us this one and it lives in memory the
    // window covers, so it should be reachable -- but "should" is what this
    // check is for.
    let rsp: u64;
    // SAFETY: Reading RSP has no side effects.
    unsafe {
        core::arch::asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack, preserves_flags));
    }
    // SAFETY: As above.
    let stack_ok = unsafe { paging::translate(table, VirtAddr::new(rsp)) }.is_ok();
    assert!(
        stack_ok,
        "the current stack at {:#018x} is not mapped in the new tables",
        rsp
    );

    // .text specifically must be executable, or the first fetch after the
    // switch fails even though the page is present.
    // SAFETY: As above.
    let (_, text_flags) = unsafe { paging::translate(table, VirtAddr::new(sections.text.0)) }
        .expect("kernel .text is not mapped");
    assert!(
        text_flags.contains(PageFlags::EXEC),
        "kernel .text is mapped without EXEC; the first instruction fetch \
         after the switch would fault"
    );
    assert!(
        !text_flags.contains(PageFlags::WRITE),
        "kernel .text is writable; W^X is not established"
    );

    println!(
        "[vm] pre-switch check: {} image pages mapped, stack reachable, \
         .text is rx",
        checked
    );
}

/// Whether CR3 points at the table we built.
///
/// Gate 2 of the reclaim procedure: releasing the bootloader's memory while
/// running on its page tables would free the mappings underneath us.
/// See ADR 013.
pub fn using_own_tables() -> bool {
    let ours = KERNEL_PML4.load(Ordering::Acquire);
    ours != 0 && paging::active_table().start().0 == ours
}

/// Whether RSP is inside the main kernel stack we allocated.
///
/// Gate 3 of the reclaim procedure. Limine's stack lives in
/// `BOOTLOADER_RECLAIMABLE` memory, so reclaiming while still on it
/// overwrites the live stack -- including the return address of the
/// function doing the reclaiming. See ADR 013.
pub fn on_own_stack() -> bool {
    let top = MAIN_STACK_TOP.load(Ordering::Acquire);
    if top == 0 {
        return false;
    }
    let bottom = top - layout::KSTACK_SIZE;

    let rsp: u64;
    // SAFETY: Reading RSP has no side effects and touches no memory.
    unsafe {
        core::arch::asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack, preserves_flags));
    }

    rsp > bottom && rsp <= top
}

/// Move onto the kernel's own stack and call `entry` there. Never returns.
///
/// Changing RSP abandons everything on the old stack: locals, saved
/// registers, and the return address of this very function. So it cannot
/// return, and nothing may be passed by reference to something stack-
/// allocated in a caller's frame. The handoff is a single `extern "C"`
/// function pointer and the `jmp` that reaches it.
///
/// Why this exists at all: Limine places the stack it hands the kernel in
/// `BOOTLOADER_RECLAIMABLE` memory. `bootinfo::reclaim` poisons that memory
/// before releasing it, which means reclaiming while still on Limine's
/// stack overwrites the live stack. The first version of this kernel did
/// exactly that and took a #GP at `rip=0xdededededededede` -- the poison
/// pattern, loaded off the stack by a `ret`.
///
/// # Safety
///
/// [`init`] must have run, so the main stack is mapped in the active
/// tables. Interrupts must be disabled: an interrupt arriving with RSP
/// mid-update would push onto whatever the half-written value points at.
/// `entry` must not return.
pub unsafe fn switch_to_main_stack(entry: extern "C" fn() -> !) -> ! {
    let top = MAIN_STACK_TOP.load(Ordering::Acquire);
    assert!(top != 0, "switch_to_main_stack() before vm::init()");

    // The ABI wants RSP ≡ 0 (mod 16) at a function's entry *after* its
    // return address is pushed. We jump rather than call, so there is no
    // return address: align to 16 and subtract 8 to imitate the layout a
    // `call` would have produced. Getting this wrong misaligns every SSE
    // spill in the callee.
    //
    // Reference: System V AMD64 ABI, section 3.2.2 "The Stack Frame".
    let entry_rsp = (top & !0xF) - 8;

    println!(
        "[vm] switching stack: {:#018x} -> {:#018x} (off the bootloader's \
         stack, which reclaim is about to poison)",
        current_rsp(),
        entry_rsp
    );

    // SAFETY: `entry_rsp` is inside the 16 KiB mapped by `map_stack` for
    // KSTACK_SLOT_MAIN in the table now in CR3, with a guard page below it.
    // Interrupts are disabled, so nothing can push onto a half-set RSP.
    // `entry` has no return path, which is what makes abandoning this
    // frame sound.
    unsafe {
        core::arch::asm!(
            "mov rsp, {rsp}",
            // RBP too: a stale frame pointer into the old stack would make
            // any backtrace walk straight into poisoned memory.
            "xor rbp, rbp",
            "jmp {entry}",
            rsp = in(reg) entry_rsp,
            entry = in(reg) entry,
            options(noreturn),
        )
    }
}

/// The current stack pointer, for reporting.
fn current_rsp() -> u64 {
    let rsp: u64;
    // SAFETY: Reading RSP has no side effects and touches no memory.
    unsafe {
        core::arch::asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack, preserves_flags));
    }
    rsp
}

/// Physical address of the kernel's PML4.
///
/// The scheduler needs this without borrowing the table: a thread whose
/// `vspace_root` is zero runs in the kernel's address space (ADR 027),
/// and the switch has to compare that against CR3.
pub fn kernel_pml4() -> PhysAddr {
    let frame = KERNEL_PML4.load(Ordering::Acquire);
    assert!(frame != 0, "kernel_pml4() before vm::init()");
    PhysAddr(frame)
}

/// The kernel PML4, for code that needs to map something after boot.
///
/// # Safety
///
/// The caller must not create a second mutable borrow, and must discharge
/// any `TlbFlush` the mapping calls return.
pub unsafe fn kernel_table<'a>() -> &'a mut PageTable {
    let frame = KERNEL_PML4.load(Ordering::Acquire);
    assert!(frame != 0, "kernel_table() before vm::init()");
    // SAFETY: The frame holds the PML4 built by `init`, reachable through
    // the window. The caller promises not to alias it.
    unsafe { &mut *super::phys_to_virt(PhysAddr(frame)).as_ptr::<PageTable>() }
}
