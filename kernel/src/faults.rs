// SPDX-License-Identifier: MPL-2.0
//! Deliberate fault triggers for the M1 acceptance tests.
//!
//! These exist so the exception machinery can be exercised on the shipping
//! binary rather than on a special build: each is reached through a token on
//! the kernel command line (see `boot/limine.conf.in`).

use core::arch::asm;

use crate::println;

/// Address used by the page-fault test.
///
/// Requirements:
///   * **Canonical.** A non-canonical address raises #GP, not #PF, because
///     the canonical check happens before the page walk. Bits 63:47 must all
///     equal bit 47; 0x0000_6000_0000_0000 satisfies this with bit 47 clear.
///   * **Unmapped.** Limine maps the kernel in the higher half and a direct
///     map of physical memory at the HHDM offset. This address is in the
///     lower half, far above any identity mapping Limine leaves behind, so
///     no page table entry covers it.
///
/// Reference: Intel SDM Vol. 1, 3.3.7.1 "Canonical Addressing"; Vol. 3A,
/// 6.15 (#GP is raised for non-canonical accesses, #PF for unmapped ones).
pub const PAGE_FAULT_ADDRESS: u64 = 0x0000_6000_0000_0000;

/// Execute `int3` to raise a breakpoint exception (#BP, vector 3).
///
/// #BP is a trap, so the handler returns here and execution continues --
/// which is the point of the test: it proves the dispatcher's `iretq` path
/// works, not merely that the CPU reached a handler.
pub fn trigger_breakpoint() {
    println!("[fault] executing int3 ...");

    // SAFETY: `int3` is a one-byte software interrupt that vectors through
    // IDT entry 3, which `idt::init` has filled with a present interrupt
    // gate. The handler reports and returns, and because #BP is a trap the
    // saved RIP already points past this instruction -- so `iretq` resumes
    // at the next statement rather than looping. The instruction touches no
    // memory and clobbers no registers.
    //
    // Reference: Intel SDM Vol. 2A, "INT n/INTO/INT3/INT1"; Vol. 3A, 6.15
    // (#BP entry).
    unsafe {
        asm!("int3", options(nomem, nostack));
    }

    println!("[fault] returned from int3 -- handler resumed execution");
}

/// Write to a canonical but unmapped address, raising a page fault
/// (#PF, vector 14).
///
/// Never returns: #PF is a fault, so the saved RIP points *at* the faulting
/// instruction. Returning would re-execute the store and fault again
/// forever, so the dispatcher panics instead.
pub fn trigger_page_fault() -> ! {
    println!(
        "[fault] writing to unmapped canonical address {:#018x} ...",
        PAGE_FAULT_ADDRESS
    );

    // SAFETY: This is deliberately an invalid write -- the whole point is to
    // provoke #PF. It is sound in the Rust sense only because control never
    // returns to Rust code afterwards: the CPU traps on the store itself,
    // the dispatcher reports it and terminates the VM. Nothing observes the
    // (never-completed) write, and no Rust-visible object is involved,
    // because the address belongs to no allocation.
    unsafe {
        asm!(
            "mov byte ptr [{addr}], 0x42",
            addr = in(reg) PAGE_FAULT_ADDRESS,
            options(nostack),
        );
    }

    // The dispatcher does not return for #PF.
    unreachable!("page fault did not terminate the VM");
}

/// Provoke a double fault (#DF, vector 8).
///
/// # How
///
/// Point RSP at an unmapped address, then raise an exception. The CPU tries
/// to push the exception frame, the push itself faults, and a fault taken
/// while delivering another fault is by definition a double fault.
///
/// # Why this way
///
/// This produces a #DF whose cause is specifically *a broken stack* -- the
/// exact scenario the IST exists for. A #DF manufactured some other way
/// (say, by making vector 14 not-present) would be delivered on a perfectly
/// good stack and would prove nothing about the stack switch. Here, if IST1
/// were not armed, the CPU would try to push the #DF frame onto the same
/// broken RSP, fail again, and triple-fault: the machine would reset
/// silently with no output at all.
///
/// So the test is self-validating in both directions. Working IST produces a
/// report; broken IST produces a reset, which the runner detects by seeing
/// the kernel banner a second time.
///
/// Reference: Intel SDM Vol. 3A, 6.15 (#DF entry: "a second exception
/// occurred while the processor was attempting to call the handler for a
/// prior exception"); 7.7 "Interrupt Stack Table".
pub fn trigger_double_fault() -> ! {
    println!("[fault] corrupting rsp and faulting to force #DF ...");

    // SAFETY: This intentionally destroys the stack pointer, so no Rust code
    // may run afterwards -- and none does: the very next instruction faults,
    // the CPU fails to push its frame, and control transfers to the #DF
    // handler on the IST1 stack, which never returns here.
    //
    // The sequence must be exactly this: setting RSP and faulting have to be
    // adjacent, with nothing in between that could touch the stack. Writing
    // it as one asm block guarantees the compiler cannot insert a spill, a
    // call, or a stack-probe between them.
    //
    // The address is canonical (bit 47 clear, bits 63:48 clear) so the push
    // raises #PF rather than #GP -- either escalates to #DF, but #PF is the
    // case the IST is really protecting against.
    unsafe {
        asm!(
            "mov rsp, {bad}",
            // Any exception will do now; `int3` is the shortest and needs no
            // operands. The CPU must push a frame to deliver it, and that
            // push is what fails.
            "int3",
            bad = in(reg) PAGE_FAULT_ADDRESS,
            options(nomem, nostack),
        );
    }

    unreachable!("double fault did not terminate the VM");
}
