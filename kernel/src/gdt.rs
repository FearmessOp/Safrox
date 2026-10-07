// SPDX-License-Identifier: BSD-2-Clause
//! Global Descriptor Table and Task State Segment.
//!
//! Limine hands us a working GDT, but it is the bootloader's, living in
//! bootloader-reclaimable memory. We build our own so that the descriptors
//! are ours to reason about and so that we can install a TSS -- which is the
//! only way to get an Interrupt Stack Table, and therefore the only way to
//! survive a double fault caused by a bad stack pointer.
//!
//! Layout (index -> selector):
//!
//! ```text
//!   0  0x00  null
//!   1  0x08  kernel code, ring 0, 64-bit   <- STAR[47:32] points here
//!   2  0x10  kernel data, ring 0
//!   3  0x18  TSS, 16 bytes -- occupies slots 3 and 4
//!   4  0x20    (the TSS descriptor's upper half)
//!   5  0x28  user code 32-bit, ring 3      <- STAR[63:48] points here
//!   6  0x30  user data, ring 3
//!   7  0x38  user code 64-bit, ring 3
//! ```
//!
//! # Why the user segments are in that order (M8)
//!
//! The order is not a choice. `sysret` derives both selectors from
//! `STAR[63:48]` by fixed arithmetic:
//!
//! * `sysretq` loads CS from base + 16 and SS from base + 8.
//! * `sysretl` would load CS from base + 0.
//!
//! So the sequence must be **user CS32, user SS, user CS64** -- the
//! 64-bit code segment after the 32-bit one, which reads backwards until
//! you see the arithmetic.
//!
//! Slot 5 is never used: this kernel never executes `sysretl`. It still
//! holds a *valid* descriptor rather than zero, because a placeholder
//! that happens to be invalid is a `#GP` waiting for the day somebody
//! adds 32-bit support.
//!
//! Getting the order wrong is hard to diagnose -- `sysret` loads a wrong
//! descriptor silently and the failure surfaces at the first user
//! instruction, somewhere unrelated. The `const` assertions at the end of
//! this file tie the indices to the arithmetic `sysret` performs, so a
//! mistake is a build failure instead.
//!
//! See ADR 029.
//!
//! Reference: Intel SDM Vol. 3A, Chapter 3 "Protected-Mode Memory
//! Management" and Chapter 7 "Task Management"; Vol. 2B, "SYSRET".

use core::arch::asm;
use core::mem::size_of;
use core::sync::atomic::{AtomicU64, Ordering};

/// Selector for the kernel code segment (GDT index 1, RPL 0).
///
/// A segment selector is `index << 3 | TI << 2 | RPL`. TI=0 selects the GDT,
/// RPL=0 is ring 0.
///
/// Reference: Intel SDM Vol. 3A, 3.4.2 "Segment Selectors", Figure 3-6.
pub const KERNEL_CODE_SELECTOR: u16 = 1 << 3;

/// Selector for the kernel data segment (GDT index 2, RPL 0).
pub const KERNEL_DATA_SELECTOR: u16 = 2 << 3;

/// Selector for the TSS descriptor (GDT index 3, RPL 0).
const TSS_SELECTOR: u16 = 3 << 3;

// ---- User segments (M8) ------------------------------------------------

/// GDT index of the 32-bit user code segment.
///
/// Unused -- this kernel never executes `sysretl` -- but the slot must
/// exist, because `sysret` computes the other two selectors from it.
const USER_CODE32_INDEX: u16 = 5;

/// GDT index of the user data segment.
const USER_DATA_INDEX: u16 = 6;

/// GDT index of the 64-bit user code segment.
const USER_CODE64_INDEX: u16 = 7;

/// Requested privilege level 3, OR'd into a user selector.
///
/// A selector loaded at CPL 3 must have RPL 3, or the descriptor's own
/// DPL check fails and the transition raises #GP.
const RPL_USER: u16 = 3;

/// Selector for the user data segment (GDT index 6, RPL 3) = `0x33`.
pub const USER_DATA_SELECTOR: u16 = (USER_DATA_INDEX << 3) | RPL_USER;

/// Selector for the 64-bit user code segment (GDT index 7, RPL 3) = `0x3b`.
pub const USER_CODE_SELECTOR: u16 = (USER_CODE64_INDEX << 3) | RPL_USER;

/// Value for `IA32_STAR[63:48]`, the base `sysret` derives from.
///
/// Note it is the index of the *32-bit* code segment, with RPL 3. The
/// CPU adds 8 for SS and 16 for the 64-bit CS.
pub const STAR_USER_BASE: u16 = (USER_CODE32_INDEX << 3) | RPL_USER;

/// Value for `IA32_STAR[47:32]`, the base `syscall` derives from.
///
/// `syscall` loads CS from this and SS from this + 8, both forced to
/// RPL 0 -- so the field holds the kernel code selector and the kernel
/// data segment must sit immediately after it.
pub const STAR_KERNEL_BASE: u16 = KERNEL_CODE_SELECTOR;

/// IST slot used for the double-fault handler.
///
/// The IST is 1-based in the IDT's `ist` field: 0 means "no IST, use the
/// normal stack", 1..=7 select `tss.ist[0..=6]`.
///
/// Reference: Intel SDM Vol. 3A, 7.7 "Interrupt Stack Table" and Figure 7-11
/// (the 64-bit TSS layout).
pub const DOUBLE_FAULT_IST_INDEX: u8 = 1;

/// Bootstrap stack for the double-fault handler.
///
/// The whole point of an IST entry is that the CPU switches to this stack
/// unconditionally, whatever RSP contained when the fault hit. A double
/// fault frequently *means* the current stack is unusable -- overflowed,
/// unmapped, misaligned -- so handling it on that same stack would fault
/// again and escalate to a triple fault: a silent reset with no diagnostic.
///
/// This copy lives in `.bss` and is only used between `gdt::init` and
/// `vm::init`. It has no guard page, so a deep overflow here still
/// scribbles over neighbouring data -- but the window is a few hundred
/// instructions of single-threaded boot code with interrupts disabled, and
/// the alternative (no IST at all until paging is up) is worse.
///
/// Once page tables exist, `vm::init` allocates a real stack in KSTACK with
/// an unmapped page below it and calls [`set_ist1`]. See ADR 009 and the
/// note at the end of linker.ld.
///
/// 4 KiB: enough for one exception frame and a short report, which is all
/// that can happen before the switch.
const BOOTSTRAP_IST_SIZE: usize = 4096;

/// 16-byte aligned because the SysV AMD64 ABI wants RSP 16-byte aligned at
/// function entry and the CPU does not re-align it for us.
#[repr(C, align(16))]
struct IstStack([u8; BOOTSTRAP_IST_SIZE]);

/// Backing storage for the bootstrap IST1 stack.
///
/// `static mut` is unavoidable: the CPU writes here asynchronously when it
/// pushes an exception frame, which Rust's aliasing model cannot describe.
static mut BOOTSTRAP_IST1: IstStack = IstStack([0; BOOTSTRAP_IST_SIZE]);

/// Current IST1 range, as `(lowest, one-past-highest)`.
///
/// The double-fault handler reads this to prove it is running on the IST
/// stack rather than on whatever stack was current when the fault hit. It
/// changes once, when `vm::init` swaps the bootstrap stack for a guarded
/// one.
static IST1_BOTTOM: AtomicU64 = AtomicU64::new(0);
static IST1_TOP: AtomicU64 = AtomicU64::new(0);

/// Inclusive-exclusive address range of the active IST1 stack.
pub fn ist1_range() -> (u64, u64) {
    (
        IST1_BOTTOM.load(Ordering::Acquire),
        IST1_TOP.load(Ordering::Acquire),
    )
}

/// Set the stack the CPU switches to when an interrupt raises the
/// privilege level from ring 3 to ring 0.
///
/// Must be called on **every thread switch**, not once at boot. Each
/// thread has its own kernel stack (M6), and `TSS.RSP0` is what the CPU
/// reads when it takes an interrupt in ring 3.
///
/// # What sharing one RSP0 would do
///
/// Two user threads with the same RSP0 share a kernel stack. Thread A
/// enters the kernel, writes its frame, gets preempted; thread B enters
/// the kernel, starts writing from the *same* top, and overwrites A's
/// frame. When A resumes, its stack is somebody else's.
///
/// The symptom is random corruption -- a wrong return address, a
/// mangled local, or nothing at all, depending on which bytes were
/// overwritten. It is not deterministic and its cause is nowhere near
/// where it is seen. See ADR 029.
///
/// Note the syscall path does **not** use this: `syscall` leaves RSP
/// alone, and the stub reads `KERNEL_RSP` instead (ADR 030). The two
/// hold the same value and `sched` writes them together.
///
/// # Safety
///
/// `top` must be one past the last byte of a mapped, writable, 16-byte
/// aligned kernel stack belonging to the thread about to run.
pub unsafe fn set_rsp0(top: u64) {
    debug_assert!(
        top.is_multiple_of(16),
        "RSP0 must be 16-byte aligned; the CPU does not realign it and \
         every SSE spill in the kernel would be misaligned"
    );

    // SAFETY: The TSS is a static this module owns. The CPU only reads
    // RSP0 when it takes an interrupt that raises the privilege level,
    // and the caller holds the scheduler's interrupt-free region across
    // this write (ADR 025) -- so the CPU cannot observe a half-written
    // value. The write is a single aligned qword in any case.
    unsafe {
        let tss = &raw mut TSS;
        (*tss).privilege_stack_table[0] = top;
    }
}

/// What `TSS.RSP0` currently holds.
///
/// Read back by the acceptance test, which needs to show that two user
/// threads enter the kernel on *different* stacks. A counter would not
/// do: "RSP0 is being updated" is satisfied by an implementation that
/// writes it once, because the first value is already correct.
pub fn rsp0() -> u64 {
    // SAFETY: A single aligned qword read from a static this module
    // owns. Racing with `set_rsp0` is impossible on one CPU outside an
    // interrupt, and the value is only reported.
    unsafe {
        let tss = &raw const TSS;
        (*tss).privilege_stack_table[0]
    }
}

/// Point IST1 at a different stack.
///
/// Called by `vm::init` once a guarded stack exists in KSTACK. Between the
/// CR3 switch and this call the TSS still names the bootstrap stack, which
/// the new tables do not map -- so a double fault in that window is
/// unrecoverable. The window is a few dozen instructions with interrupts
/// disabled.
///
/// # Safety
///
/// `top` must be one past the last byte of a mapped, writable, 16-byte
/// aligned stack that nothing else is using.
pub unsafe fn set_ist1(top: u64) {
    assert!(
        top.is_multiple_of(16),
        "IST stack top must be 16-byte aligned"
    );

    // SAFETY: The TSS is a static this module owns; the only other reader
    // is the CPU, and it only consults the IST on an exception, which
    // cannot happen here because interrupts are disabled and this code
    // does not fault. The write is a single aligned qword.
    unsafe {
        let tss = &raw mut TSS;
        (*tss).interrupt_stack_table[(DOUBLE_FAULT_IST_INDEX - 1) as usize] = top;
    }

    IST1_BOTTOM.store(top - crate::memory::layout::KSTACK_SIZE, Ordering::Release);
    IST1_TOP.store(top, Ordering::Release);
}

/// Top of the bootstrap IST1 stack.
fn bootstrap_ist1_top() -> u64 {
    let base = &raw const BOOTSTRAP_IST1 as u64;
    base + BOOTSTRAP_IST_SIZE as u64
}

// ---- Task State Segment ------------------------------------------------

/// The 64-bit Task State Segment.
///
/// In long mode the TSS no longer holds a task context -- hardware task
/// switching is gone. What remains is a table of stack pointers: RSP0..2 for
/// privilege-level changes, and IST1..7 for unconditional stack switches
/// selected per interrupt vector.
///
/// `#[repr(C, packed)]`: the layout is dictated by hardware, and the natural
/// alignment of `u64` would otherwise make the compiler insert padding after
/// `reserved_0`, shifting every stack pointer by four bytes.
///
/// Reference: Intel SDM Vol. 3A, Figure 7-11 "64-Bit TSS Format".
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct TaskStateSegment {
    /// Offset 0x00. Reserved, must be zero.
    reserved_0: u32,
    /// Offset 0x04. Stack pointers for rings 0-2, used when an interrupt
    /// raises the privilege level. Unused until M8 introduces ring 3, but
    /// the fields must exist because the layout is fixed.
    privilege_stack_table: [u64; 3],
    /// Offset 0x1C. Reserved.
    reserved_1: u64,
    /// Offset 0x24. IST1..IST7. Index 0 here is "IST1" in the IDT.
    interrupt_stack_table: [u64; 7],
    /// Offset 0x5C. Reserved.
    reserved_2: u64,
    /// Offset 0x64. Reserved.
    reserved_3: u16,
    /// Offset 0x66. Offset from the TSS base to the I/O permission bitmap.
    /// Setting it to the TSS size means "no bitmap present", which denies
    /// port I/O to anything below CPL 0.
    ///
    /// Reference: Intel SDM Vol. 3A, 20.5.2 "I/O Permission Bit Map".
    iomap_base: u16,
}

impl TaskStateSegment {
    const fn new() -> Self {
        Self {
            reserved_0: 0,
            privilege_stack_table: [0; 3],
            reserved_1: 0,
            interrupt_stack_table: [0; 7],
            reserved_2: 0,
            reserved_3: 0,
            // No I/O bitmap: the limit equals the TSS size.
            iomap_base: size_of::<TaskStateSegment>() as u16,
        }
    }
}

/// The one TSS this (single-CPU) kernel uses.
///
/// `static mut` because the CPU reads it directly after `ltr`, and because
/// `init()` must write the IST pointer into it before that happens.
static mut TSS: TaskStateSegment = TaskStateSegment::new();

// ---- Descriptors -------------------------------------------------------

// Access byte (descriptor byte 5) bits. Intel SDM Vol. 3A, Figure 3-8
// "Segment Descriptor".

/// Bit 7, P: segment present.
const ACCESS_PRESENT: u8 = 1 << 7;
/// Bit 4, S: descriptor type. 1 = code or data, 0 = system (TSS, LDT, gate).
const ACCESS_CODE_DATA: u8 = 1 << 4;
/// Bit 3, E: executable. Set for a code segment.
const ACCESS_EXECUTABLE: u8 = 1 << 3;
/// Bit 1, RW: readable for a code segment, writable for a data segment.
const ACCESS_RW: u8 = 1 << 1;

/// Bits 6:5, DPL = 3: the segment may be used at ring 3.
///
/// A code segment with DPL 3 is what makes CPL 3 possible at all; a data
/// segment with DPL 3 is what lets ring 3 load it into SS.
const ACCESS_DPL3: u8 = 3 << 5;

/// System-segment type 0b1001: 64-bit TSS, available (not busy).
///
/// Reference: Intel SDM Vol. 3A, Table 3-2 "System-Segment and Gate-Descriptor
/// Types", 64-bit mode column.
const TSS_TYPE_AVAILABLE_64: u8 = 0b1001;

// Flags occupy the high nibble of descriptor byte 6, i.e. descriptor bits
// 55:52. They are defined here already shifted into that nibble, so the
// values below are the *descriptor* bit positions, not 0..3 offsets.
//
// Getting this wrong is quietly fatal: if L is dropped, the CPU treats the
// segment as 32-bit compatibility mode, and the far return that loads CS
// faults with no IDT yet installed -- a triple fault and a silent reset.

/// Descriptor bit 53, L: 64-bit code segment. When L is set, D (bit 54) must
/// be clear.
///
/// Reference: Intel SDM Vol. 3A, 3.4.5 "Segment Descriptors", L flag, and
/// 5.2.1 "Code-Segment Descriptor in 64-bit Mode".
const FLAG_LONG_MODE: u8 = 1 << 5;

/// Descriptor bit 54, D/B: 32-bit default operand size.
///
/// Set for the 32-bit user code segment, where L must be clear. The two
/// are mutually exclusive: L=1 means 64-bit, D=1 means 32-bit, and both
/// set is an invalid descriptor.
const FLAG_DB_32BIT: u8 = 1 << 6;

/// Descriptor bit 55, G: granularity. With G set, the limit counts 4 KiB
/// pages instead of bytes. Long mode ignores the limit for code and data
/// segments, but a sane value keeps the descriptor unsurprising in a
/// debugger.
const FLAG_GRANULARITY_4K: u8 = 1 << 7;

/// Build an 8-byte code or data segment descriptor.
///
/// In 64-bit mode the base and limit of code/data segments are ignored: the
/// base is treated as 0 and the limit as unbounded. The fields are still
/// encoded because the descriptor format did not change.
///
/// Reference: Intel SDM Vol. 3A, 3.4.5 and 5.2.1 "Code-Segment Descriptor in
/// 64-bit Mode".
const fn segment_descriptor(access: u8, flags: u8) -> u64 {
    let limit: u64 = 0xF_FFFF; // 20-bit limit, all ones
    let base: u64 = 0;

    // The flags constants are already positioned in the high nibble of a
    // byte, which lands at descriptor bits 55:52. Masking with 0xF0 keeps
    // only that nibble, so a stray low bit cannot collide with limit[19:16]
    // sitting directly below it.
    (limit & 0xFFFF)                        // limit 15:0
        | ((base & 0xFF_FFFF) << 16)        // base 23:0
        | ((access as u64) << 40)           // access byte
        | (((limit >> 16) & 0xF) << 48)     // limit 19:16
        | (((flags as u64) & 0xF0) << 48)   // flags nibble, bits 55:52
        | (((base >> 24) & 0xFF) << 56) // base 31:24
}

/// Build the two halves of a 16-byte TSS descriptor.
///
/// System descriptors in long mode are twice as wide as code/data ones so
/// that they can hold a full 64-bit base address. The low half looks like a
/// classic descriptor; the high half carries base bits 63:32.
///
/// Reference: Intel SDM Vol. 3A, Figure 7-4 "Format of TSS and LDT
/// Descriptors in 64-bit Mode".
fn tss_descriptor(base: u64, limit: u32) -> (u64, u64) {
    let low = (limit as u64 & 0xFFFF)                       // limit 15:0
        | ((base & 0xFF_FFFF) << 16)                        // base 23:0
        | ((TSS_TYPE_AVAILABLE_64 as u64) << 40)            // type, S=0
        | ((ACCESS_PRESENT as u64) << 40)                   // P
        | (((limit as u64 >> 16) & 0xF) << 48)              // limit 19:16
        | (((base >> 24) & 0xFF) << 56); // base 31:24

    let high = (base >> 32) & 0xFFFF_FFFF; // base 63:32

    (low, high)
}

/// The GDT itself: eight 8-byte slots.
///
/// Slots 3 and 4 form the 16-byte TSS descriptor; 5 through 7 are the
/// user segments M8 added, in the order `sysret` dictates.
///
/// `static mut` because `lgdt` makes the CPU read this memory directly, and
/// because the TSS descriptor can only be filled in at run time (it embeds
/// the TSS's address).
static mut GDT: [u64; 8] = [0; 8];

/// Operand for `lgdt`: a 16-bit limit followed by a 64-bit base.
///
/// `#[repr(C, packed)]` because the CPU reads exactly 10 bytes; natural
/// alignment would insert six bytes of padding between the two fields and
/// the CPU would load garbage as the base.
///
/// Reference: Intel SDM Vol. 2A, "LGDT/LIDT - Load Global/Interrupt
/// Descriptor Table Register".
#[repr(C, packed)]
struct DescriptorTablePointer {
    /// Size of the table in bytes, minus one.
    limit: u16,
    /// Linear address of the table.
    base: u64,
}

/// Build our GDT, load it, and switch to our own segment selectors.
///
/// After this returns, every segment register and the task register point
/// into memory this kernel owns, and IST1 is armed.
///
/// # Safety
///
/// Must be called exactly once, early in boot, with interrupts disabled. It
/// replaces the GDT the bootloader installed; any descriptor cached in a
/// segment register from the old table is reloaded here, but an interrupt
/// arriving mid-sequence would be dispatched through a half-updated state.
pub unsafe fn init() {
    // SAFETY: Single-threaded boot path, interrupts disabled, and this
    // function is called exactly once (enforced by its contract and by the
    // single call site in kstart). No other code reads GDT or TSS until
    // `lgdt`/`ltr` below, so these writes cannot race with the CPU's own
    // accesses.
    unsafe {
        // Arm the double-fault stack before the TSS becomes visible to the
        // CPU: once `ltr` executes, a double fault could arrive at any time.
        //
        // This is the bootstrap stack; `vm::init` replaces it with a
        // guarded one as soon as page tables exist.
        let bootstrap_top = bootstrap_ist1_top();
        (&raw mut TSS).write_volatile({
            let mut tss = TaskStateSegment::new();
            tss.interrupt_stack_table[(DOUBLE_FAULT_IST_INDEX - 1) as usize] = bootstrap_top;
            tss
        });

        IST1_BOTTOM.store(bootstrap_top - BOOTSTRAP_IST_SIZE as u64, Ordering::Release);
        IST1_TOP.store(bootstrap_top, Ordering::Release);

        let tss_base = &raw const TSS as u64;
        // The TSS limit is the last valid byte offset, hence size - 1.
        let tss_limit = (size_of::<TaskStateSegment>() - 1) as u32;
        let (tss_low, tss_high) = tss_descriptor(tss_base, tss_limit);

        GDT[0] = 0; // Null descriptor. Required: selector 0 must be invalid.
        GDT[1] = segment_descriptor(
            ACCESS_PRESENT | ACCESS_CODE_DATA | ACCESS_EXECUTABLE | ACCESS_RW,
            FLAG_LONG_MODE | FLAG_GRANULARITY_4K,
        );
        GDT[2] = segment_descriptor(
            ACCESS_PRESENT | ACCESS_CODE_DATA | ACCESS_RW,
            FLAG_GRANULARITY_4K,
        );
        GDT[3] = tss_low;
        GDT[4] = tss_high;

        // ---- User segments (M8, ADR 029) ----
        //
        // The order is `sysret`'s, not ours: it computes SS from
        // STAR[63:48] + 8 and the 64-bit CS from + 16. So 32-bit code,
        // then data, then 64-bit code.

        // Slot 5: 32-bit user code. Never executed -- this kernel has no
        // `sysretl` path -- but a real descriptor rather than zero, so
        // that adding one later does not start from a silent #GP.
        //
        // L clear and D set: the two are mutually exclusive, and a
        // descriptor with both is invalid.
        GDT[USER_CODE32_INDEX as usize] = segment_descriptor(
            ACCESS_PRESENT | ACCESS_CODE_DATA | ACCESS_EXECUTABLE | ACCESS_RW | ACCESS_DPL3,
            FLAG_DB_32BIT | FLAG_GRANULARITY_4K,
        );

        // Slot 6: user data. This is what SS holds at CPL 3, and what
        // `iretq` loads from the frame.
        GDT[USER_DATA_INDEX as usize] = segment_descriptor(
            ACCESS_PRESENT | ACCESS_CODE_DATA | ACCESS_RW | ACCESS_DPL3,
            FLAG_GRANULARITY_4K,
        );

        // Slot 7: 64-bit user code. The one that actually runs.
        GDT[USER_CODE64_INDEX as usize] = segment_descriptor(
            ACCESS_PRESENT | ACCESS_CODE_DATA | ACCESS_EXECUTABLE | ACCESS_RW | ACCESS_DPL3,
            FLAG_LONG_MODE | FLAG_GRANULARITY_4K,
        );

        let pointer = DescriptorTablePointer {
            limit: (size_of::<[u64; 8]>() - 1) as u16,
            base: &raw const GDT as u64,
        };

        load_gdt(&pointer);
        reload_segment_registers();
        load_task_register();
    }
}

/// Execute `lgdt`.
///
/// # Safety
///
/// `pointer` must describe a well-formed GDT that remains valid for as long
/// as the CPU might consult it -- which, for a GDT in a static, is forever.
unsafe fn load_gdt(pointer: &DescriptorTablePointer) {
    // SAFETY: The caller guarantees the descriptor table is well-formed and
    // permanently live. `lgdt` only loads GDTR; it does not itself validate
    // or use any descriptor, so no fault can arise from the instruction
    // alone. Segment registers still hold the bootloader's cached
    // descriptors until the reload below.
    unsafe {
        asm!("lgdt [{}]", in(reg) pointer, options(readonly, nostack, preserves_flags));
    }
}

/// Load CS with our code selector and the data registers with ours.
///
/// # Safety
///
/// A valid GDT with our layout must already be loaded.
unsafe fn reload_segment_registers() {
    // SAFETY: GDT is loaded and contains a 64-bit ring-0 code segment at
    // index 1 and a data segment at index 2.
    //
    // CS cannot be written with `mov`; it only changes through a control
    // transfer. 64-bit mode has no far jump to an absolute address, so we
    // synthesise a far return: push the target selector and offset, then
    // `retfq` pops them into CS:RIP. This is the standard sequence -- see
    // the OSDev wiki, "GDT Tutorial", Reloading Segment Registers.
    //
    // The label is `2` rather than the conventional `1`: on x86, labels made
    // only of the digits 0 and 1 are ambiguous with binary literals because
    // of an LLVM bug, and rustc denies them.
    // See <https://github.com/llvm/llvm-project/issues/99547>.
    unsafe {
        asm!(
            "push {code_sel}",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            code_sel = in(reg) KERNEL_CODE_SELECTOR as u64,
            tmp = lateout(reg) _,
            options(preserves_flags),
        );
    }

    // The data registers are loaded in a *separate* asm block, deliberately.
    //
    // Folding them into the block above looks tidier and is wrong: `lateout`
    // tells the compiler the scratch register may reuse an input's register,
    // because a lateout is written only after every input has been read.
    // A `data_sel` input read *after* the `lea` violates that, and the
    // compiler is free to allocate both to RAX -- which it does. The `lea`
    // then overwrites the selector with a code address, `mov ds, ax` loads
    // garbage, and the resulting #GP arrives before any IDT exists: triple
    // fault, silent reset, no output.
    //
    // Separate blocks make the overlap impossible to express.
    //
    // SS must hold a writable data segment (or null). DS and ES are ignored
    // for addressing in 64-bit mode but are reloaded anyway so no stale
    // bootloader descriptor lingers. FS and GS are left alone: their bases
    // live in MSRs and nothing uses them yet.
    //
    // SAFETY: Same preconditions as above; CS now names our code segment.
    unsafe {
        asm!(
            "mov ds, {sel:x}",
            "mov es, {sel:x}",
            "mov ss, {sel:x}",
            sel = in(reg) KERNEL_DATA_SELECTOR,
            options(nostack, preserves_flags),
        );
    }
}

/// Execute `ltr`, making the TSS active.
///
/// # Safety
///
/// The GDT must already contain a valid, non-busy 64-bit TSS descriptor at
/// [`TSS_SELECTOR`], and the TSS it points at must be initialised.
unsafe fn load_task_register() {
    // SAFETY: `init()` wrote the TSS and its descriptor immediately above,
    // and the GDT is loaded. The descriptor is of type "TSS (available)", so
    // `ltr` will not fault on a busy TSS. After this the CPU may consult the
    // IST at any time.
    //
    // Reference: Intel SDM Vol. 2A, "LTR - Load Task Register".
    unsafe {
        asm!("ltr {:x}", in(reg) TSS_SELECTOR, options(nostack, preserves_flags));
    }
}

// ---- The selectors `sysret` and `syscall` derive, checked at build time
//
// `sysret` does not read the GDT to find its selectors; it computes them
// by adding a fixed offset to STAR[63:48]. If the user descriptors are
// not at exactly those indices, it loads the wrong one *silently*, and
// the failure appears at the first user instruction -- somewhere
// unrelated to the mistake.
//
// These assertions reproduce the CPU's arithmetic. Moving a slot is then
// a build failure rather than a #GP two milestones later.
//
// Reference: Intel SDM Vol. 2B, "SYSRET" and "SYSCALL"; ADR 029.

const _: () = {
    // `sysretq` loads SS from STAR[63:48] + 8, with RPL forced to 3.
    assert!(
        (STAR_USER_BASE & !3) + 8 == (USER_DATA_SELECTOR & !3),
        "sysret would load SS from the wrong GDT slot: STAR[63:48] + 8 \
         must be the user data segment"
    );

    // `sysretq` loads CS from STAR[63:48] + 16, with RPL forced to 3.
    assert!(
        (STAR_USER_BASE & !3) + 16 == (USER_CODE_SELECTOR & !3),
        "sysret would load CS from the wrong GDT slot: STAR[63:48] + 16 \
         must be the 64-bit user code segment"
    );

    // `syscall` loads SS from STAR[47:32] + 8, with RPL forced to 0. So
    // the kernel data segment must sit immediately after the kernel
    // code segment.
    assert!(
        STAR_KERNEL_BASE + 8 == KERNEL_DATA_SELECTOR,
        "syscall would load SS from the wrong GDT slot: STAR[47:32] + 8 \
         must be the kernel data segment"
    );

    // Every index named above must be inside the table.
    assert!(
        (USER_CODE64_INDEX as usize) < 8,
        "a user segment index is past the end of the GDT"
    );

    // The user selectors must carry RPL 3. A user descriptor loaded with
    // RPL 0 fails the privilege check and raises #GP -- and the check
    // that fails is the descriptor's own DPL, so the diagnostic points
    // at the descriptor rather than at the selector.
    assert!(
        USER_CODE_SELECTOR & 3 == 3 && USER_DATA_SELECTOR & 3 == 3,
        "the user selectors must have RPL 3"
    );

    // And the TSS must not overlap them: it is 16 bytes, so it occupies
    // two slots and the first user segment starts after both.
    assert!(
        (TSS_SELECTOR >> 3) + 2 <= USER_CODE32_INDEX,
        "the TSS descriptor's upper half overlaps the first user segment"
    );
};
