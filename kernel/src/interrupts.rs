// SPDX-License-Identifier: MPL-2.0
//! Exception entry points and the common dispatcher.
//!
//! # Why assembly stubs
//!
//! The CPU's exception entry is not a function call. Two things make a plain
//! `extern "C"` Rust function the wrong shape for an IDT entry:
//!
//!   1. **Some vectors push an error code, some do not.** The CPU pushes an
//!      extra qword for #DF, #TS, #NP, #SS, #GP, #PF, #AC, #CP, #VC and #SX;
//!      for every other vector it does not. The handler has to know which
//!      kind it is to find the frame -- and to leave the stack balanced on
//!      return.
//!   2. **Returning needs `iretq`, not `ret`.** The frame the CPU pushed
//!      (RIP, CS, RFLAGS, RSP, SS) is only consumed correctly by `iretq`.
//!
//! So each of the 32 vectors gets a tiny stub that normalises the two cases
//! into one shape and then jumps to shared code. The stubs push a dummy zero
//! error code where the CPU pushed none, so from the dispatcher's point of
//! view every frame looks identical.
//!
//! The alternative -- `x86-interrupt` ABI functions, one per vector -- was
//! rejected: it hides exactly the mechanism this kernel is supposed to make
//! explicit, and it is an unstable compiler feature. See
//! docs/adr/006-exception-stub-ortak-handler.md.
//!
//! Reference: Intel SDM Vol. 3A, Chapter 6 "Interrupt and Exception
//! Handling", in particular Table 6-1 (vector assignments), 6.13 (error
//! code), and Figure 6-8 (stack usage on transfers to interrupt handlers).

use core::arch::global_asm;

use crate::gdt;
use crate::println;
use crate::qemu;

/// Number of CPU-reserved exception vectors.
///
/// Vectors 0..=31 are architecturally defined; 32..=255 are available
/// for external interrupts. M6 uses two of them: the LAPIC timer on 32
/// and the spurious vector on 255.
///
/// Reference: Intel SDM Vol. 3A, Table 6-1 "Protected-Mode Exceptions and
/// Interrupts".
pub const EXCEPTION_COUNT: usize = 32;

/// Total stubs generated: the 32 exceptions, M6's two, and one per
/// interrupt line (M10).
///
/// All of them get the same treatment as an exception -- a stub that
/// normalises the frame and jumps to the common tail -- because a
/// handler that has to care whether it was entered from an exception
/// or an interrupt is a handler with two shapes.
pub const STUB_COUNT: usize = 34 + crate::cap::irq::MAX_IRQ;

/// Index into `isr_table` for the LAPIC timer. Not the vector number:
/// the table is dense and the timer's vector (32) happens to equal its
/// index, while the spurious vector (255) does not.
pub const TIMER_STUB_INDEX: usize = 32;

/// Index into `isr_table` for the spurious interrupt.
pub const SPURIOUS_STUB_INDEX: usize = 33;

/// Index into `isr_table` of the stub for IRQ 0 (M10).
///
/// Line `n`'s stub is at `IRQ_STUB_BASE + n` and delivers on vector
/// [`crate::apic::irq_to_vector`]`(n)`. The two numbers differ by one
/// and that is deliberate rather than an off-by-one waiting to
/// happen: the table has been dense since M6, and the spurious vector
/// (index 33, vector 255) already established that an index is not a
/// vector.
pub const IRQ_STUB_BASE: usize = 34;

const _: () = {
    assert!(
        IRQ_STUB_BASE + crate::cap::irq::MAX_IRQ == STUB_COUNT,
        "the IRQ stubs do not fill the table exactly"
    );
    // The assembly below writes the vector numbers as literals, so
    // this is what keeps them honest if the base ever moves.
    assert!(
        crate::apic::IRQ_VECTOR_BASE == 33,
        "the IRQ stubs push literal vector numbers starting at 33; \
         IRQ_VECTOR_BASE has moved and the assembly has not"
    );
    assert!(
        crate::cap::irq::MAX_IRQ == 16,
        "there are exactly 16 IRQ stubs written out below"
    );
};

/// Machine state at the point an exception was taken.
///
/// Field order mirrors the stack exactly, lowest address first. The general
/// registers are pushed by our stub in reverse order (`push r15` first, so
/// r15 ends up at the highest address of that group); the last five fields
/// are what the CPU itself pushed.
///
/// `#[repr(C)]` so the compiler cannot reorder fields out from under the
/// assembly that builds this layout.
///
/// Reference: Intel SDM Vol. 3A, Figure 6-8 "Stack Usage on Transfers to
/// Interrupt and Exception-Handling Routines".
#[repr(C)]
#[derive(Debug)]
pub struct ExceptionFrame {
    // --- pushed by our stub, in `push` order r15 .. rax -------------------
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,

    // --- pushed by our stub ----------------------------------------------
    /// Which exception this is, 0..=31.
    pub vector: u64,
    /// The CPU's error code, or 0 for vectors that do not produce one.
    pub error_code: u64,

    // --- pushed by the CPU -----------------------------------------------
    /// Address of the faulting instruction (for faults) or the one after it
    /// (for traps such as #BP).
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    /// RSP at the time of the exception. In 64-bit mode the CPU always
    /// pushes SS:RSP, even without a privilege change -- unlike 32-bit mode.
    pub rsp: u64,
    pub ss: u64,
}

/// Human-readable name for an exception vector.
///
/// Reference: Intel SDM Vol. 3A, Table 6-1.
fn vector_name(vector: u64) -> &'static str {
    match vector {
        0 => "#DE divide error",
        1 => "#DB debug",
        2 => "NMI",
        3 => "#BP breakpoint",
        4 => "#OF overflow",
        5 => "#BR bound range exceeded",
        6 => "#UD invalid opcode",
        7 => "#NM device not available",
        8 => "#DF double fault",
        9 => "coprocessor segment overrun",
        10 => "#TS invalid TSS",
        11 => "#NP segment not present",
        12 => "#SS stack-segment fault",
        13 => "#GP general protection",
        14 => "#PF page fault",
        15 => "reserved",
        16 => "#MF x87 floating-point",
        17 => "#AC alignment check",
        18 => "#MC machine check",
        19 => "#XM SIMD floating-point",
        20 => "#VE virtualisation",
        21 => "#CP control protection",
        28 => "#HV hypervisor injection",
        29 => "#VC VMM communication",
        30 => "#SX security",
        _ => "reserved",
    }
}

/// Whether the CPU pushes an error code for this vector.
///
/// Reference: Intel SDM Vol. 3A, Table 6-1, "Error Code" column.
const fn pushes_error_code(vector: usize) -> bool {
    matches!(vector, 8 | 10 | 11 | 12 | 13 | 14 | 17 | 21 | 29 | 30)
}

/// Read CR2, which holds the linear address that caused a page fault.
///
/// Only meaningful immediately after a #PF: any later page fault, including
/// one taken inside the handler, overwrites it.
///
/// Reference: Intel SDM Vol. 3A, 6.15 "Exception and Interrupt Reference",
/// #PF entry.
fn read_cr2() -> u64 {
    let value: u64;
    // SAFETY: `mov rax, cr2` is a plain register read. It requires CPL 0,
    // which holds inside an exception handler entered from ring 0. The
    // instruction has no side effects and touches no memory.
    unsafe {
        core::arch::asm!("mov {}, cr2", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Decode the #PF error code into something readable.
///
/// Five bits, not three. M1 decoded only P, W/R and U/S, which was enough
/// for the faults M1 could produce -- but it made an instruction fetch
/// indistinguishable from a read, and it said nothing at all about a
/// reserved-bit violation. M3 produces both:
///
/// * The NX test's entire proof is bit 4. An error code of 0x11 is a
///   *fetch* from a no-execute page; the old decoder printed it as
///   "supervisor read protection-violation", which is not what happened
///   and which would have let a broken NX mapping look like an ordinary
///   permission fault.
/// * Bit 3 is what the CPU reports when EFER.NXE is clear and an entry
///   sets bit 63 anyway -- exactly the failure `cpu::init` asserts against.
///   Naming it means that if the assertion were ever removed, the symptom
///   would still be legible instead of appearing as a mysterious fault.
///
/// Reference: Intel SDM Vol. 3A, Figure 6-9 "Page-Fault Error Code" and
/// Table 6-7 ("Page-Fault Error Code Bits").
fn page_fault_cause(error_code: u64) -> (&'static str, &'static str, &'static str) {
    // Bit 0, P: 0 = the fault was caused by a non-present page.
    let present = if error_code & (1 << 0) != 0 {
        "protection-violation"
    } else {
        "not-present"
    };

    // Bits 1, 3 and 4 together say what the access was. Order matters:
    //
    // * Bit 4 (I/D) is an instruction fetch, and the CPU only ever sets it
    //   when EFER.NXE is enabled -- so seeing it is also evidence that
    //   cpu::init did its job. A fetch is never a write, so it is checked
    //   first.
    // * Bit 3 (RSVD) means a reserved bit was set in some level of the
    //   paging structures. It overrides the W/R interpretation because the
    //   walk failed before the access type mattered.
    let access = if error_code & (1 << 3) != 0 {
        "reserved-bit set in a paging structure"
    } else if error_code & (1 << 4) != 0 {
        "instruction fetch"
    } else if error_code & (1 << 1) != 0 {
        "write"
    } else {
        "read"
    };

    // Bit 2, U/S: 1 = the access came from user mode.
    let mode = if error_code & (1 << 2) != 0 {
        "user"
    } else {
        "supervisor"
    };

    (present, access, mode)
}

/// Print the full exception report on the serial console.
fn report(frame: &ExceptionFrame) {
    println!();
    println!(
        "*** EXCEPTION: {} (vector {}) ***",
        vector_name(frame.vector),
        frame.vector
    );
    println!(
        "[trap] error_code={:#018x}  rip={:#018x}  cs={:#06x}",
        frame.error_code, frame.rip, frame.cs
    );
    println!(
        "[trap] rflags={:#018x}  rsp={:#018x}  ss={:#06x}",
        frame.rflags, frame.rsp, frame.ss
    );

    if frame.vector == 14 {
        let cr2 = read_cr2();
        let (present, access, mode) = page_fault_cause(frame.error_code);
        println!(
            "[trap] cr2={:#018x}  ({} {} {})",
            cr2, mode, access, present
        );

        // **A scenario may have predicted this address** (M14).
        //
        // `fb-out-of-bounds` computes where the faulting store
        // should land, from the framebuffer's geometry and the
        // address the component maps at, and arms the prediction
        // before the component runs. Checking it here is what turns
        // "a #PF happened" into "the #PF happened *there*".
        //
        // It has to be here rather than in the scenario: the
        // component's last two actions are a report and the store,
        // with no scheduling point between them, so no kernel thread
        // runs again before the fault. The only code that sees both
        // the prediction and CR2 is this handler.
        crate::fbtest::check_predicted_fault(cr2);
    }
}

/// Handle a breakpoint (#BP, vector 3).
///
/// #BP is a *trap*, not a fault: the CPU pushes the address of the
/// instruction *after* `int3`, so returning with `iretq` resumes normally
/// without re-executing anything. That is what makes this the one exception
/// in M1 we can return from, and it is how the acceptance test proves the
/// dispatcher really does return rather than merely surviving.
///
/// Reference: Intel SDM Vol. 3A, 6.15, #BP entry ("Saved Instruction
/// Pointer: ... points to the instruction following the INT3").
fn handle_breakpoint(frame: &ExceptionFrame) {
    report(frame);
    println!("[trap] #BP is a trap; resuming at rip={:#018x}", frame.rip);
}

/// Handle a double fault (#DF, vector 8).
///
/// A double fault means the CPU could not deliver an earlier exception. It
/// is not recoverable: the architecture does not define what state the
/// machine is in, and `iretq` from here is explicitly not supported.
///
/// This runs on the IST1 stack (see [`gdt::DOUBLE_FAULT_IST_INDEX`]), and the
/// first thing it does is prove that -- if the CPU had not switched stacks,
/// the most likely cause of the double fault (a bad RSP) would have made
/// this handler fault too, escalating to a triple fault and a silent reset.
///
/// Reference: Intel SDM Vol. 3A, 6.15, #DF entry; 7.7 "Interrupt Stack
/// Table".
fn handle_double_fault(frame: &ExceptionFrame) -> ! {
    report(frame);

    // Verify we are actually on the dedicated stack. We check the *current*
    // RSP, not the one in the frame: frame.rsp is the stack that was in use
    // when the fault hit, which is exactly the one we are avoiding.
    let current_rsp: u64;
    // SAFETY: Reading RSP into a general register has no side effects and
    // touches no memory. The value is only used for a range comparison.
    unsafe {
        core::arch::asm!("mov {}, rsp", out(reg) current_rsp, options(nomem, nostack, preserves_flags));
    }

    let (ist_low, ist_high) = gdt::ist1_range();
    let on_ist = current_rsp >= ist_low && current_rsp < ist_high;

    println!(
        "[trap] IST1 stack: {:#018x} .. {:#018x}, handler rsp={:#018x}",
        ist_low, ist_high, current_rsp
    );
    println!(
        "[trap] IST1: {}",
        if on_ist {
            "ok"
        } else {
            "NOT ACTIVE -- handler is running on the faulting stack"
        }
    );

    // Deliberately not a `panic!`: a double fault has already demonstrated
    // that the normal exception path is broken, and the panic handler would
    // add another layer of formatting on a machine in an undefined state.
    // Report, then leave immediately through the same exit the panic path
    // uses, so the host sees status 35 either way.
    println!(
        "[trap] unrecoverable; signalling QEMU (write {:#04x} -> exit status {})",
        qemu::EXIT_FAILURE,
        qemu::HOST_STATUS_FAILURE
    );
    qemu::exit(qemu::EXIT_FAILURE)
}

/// Common dispatcher. Every one of the 32 stubs ends up here.
///
/// Returning from this function makes the stub pop the saved registers and
/// execute `iretq`, which resumes the interrupted code. For anything that is
/// not a trap we never return: we report and exit instead, because M1 has no
/// mechanism for resuming from a fault.
///
/// # Safety
///
/// Called only from the assembly stubs below, with `frame` pointing at the
/// register block they just built on the current stack.
/// Vector the LAPIC timer is configured to deliver on.
///
/// Duplicated from `apic` rather than imported, because the dispatcher
/// must not depend on the APIC being initialised -- and a `const`
/// assertion below keeps the two in step.
const TIMER_VECTOR: u64 = 32;

/// Vector the LAPIC uses for spurious interrupts.
const SPURIOUS_VECTOR: u64 = 255;

/// First and last vector carrying a user-owned interrupt line.
const IRQ_VECTOR_FIRST: u64 = crate::apic::IRQ_VECTOR_BASE as u64;
const IRQ_VECTOR_LAST: u64 =
    crate::apic::IRQ_VECTOR_BASE as u64 + crate::apic::IRQ_VECTOR_COUNT as u64 - 1;

const _: () = {
    assert!(
        TIMER_VECTOR == crate::apic::TIMER_VECTOR as u64,
        "the dispatcher and the APIC disagree about the timer vector"
    );
    assert!(
        SPURIOUS_VECTOR == crate::apic::SPURIOUS_VECTOR as u64,
        "the dispatcher and the APIC disagree about the spurious vector"
    );
    assert!(
        IRQ_VECTOR_FIRST > TIMER_VECTOR && IRQ_VECTOR_LAST < SPURIOUS_VECTOR,
        "the IRQ vector range overlaps the timer or the spurious vector"
    );
};

/// Handle an interrupt on a user-owned line (M10, ADR 042).
///
/// Four steps, and the fourth is here rather than in `cap::irq`
/// because every vector in this file ends with an EOI and having one
/// place that writes it is worth more than keeping the four steps in
/// one function.
///
/// The EOI comes **after** the handler, which is the opposite of
/// [`handle_timer`] and for a reason worth stating: the timer
/// acknowledges first because `sched::tick` may not return for a long
/// time, and an unacknowledged LAPIC would stop the clock that is
/// supposed to take the CPU back. [`crate::cap::irq::handle_interrupt`]
/// never switches threads — it signals a Notification, which makes a
/// thread *runnable* and returns — so there is no such window, and
/// acknowledging afterwards keeps the invariant that the line is
/// already masked by the time the LAPIC will deliver anything else.
///
/// # Safety
///
/// Called from an IRQ stub, with IF clear.
unsafe fn handle_irq(vector: u64) {
    let irq = crate::apic::vector_to_irq(vector as u8).unwrap_or_else(|| {
        panic!(
            "vector {} reached handle_irq but does not map to a line; the \
             IDT has a gate the vector table does not know about",
            vector
        )
    });

    // The `irq-alloc` scenario arms a hook here. It does nothing
    // unless armed, and when armed it allocates — which is the point:
    // ADR 022's tripwire has been live since M6 but only the *timer*
    // has ever exercised it, and a timer interrupt is the kernel's
    // own. This is the first time it is reached from a path a user
    // component triggers.
    crate::irqtest::allocate_from_handler_if_armed();

    // SAFETY: We are in interrupt context with IF clear, which is
    // exactly `handle_interrupt`'s precondition.
    unsafe {
        crate::cap::irq::handle_interrupt(irq);
    }

    // SAFETY: Once per interrupt, from its handler.
    unsafe {
        crate::apic::end_of_interrupt();
    }
}

/// Handle the periodic timer.
///
/// Three things, in this order, and the order matters:
///
/// 1. Count the tick.
/// 2. Acknowledge the LAPIC. Until EOI is written no further interrupt
///    at this priority is delivered, so a handler that switches threads
///    *before* acknowledging stops the timer for as long as the other
///    thread runs -- which, if that thread never yields, is forever.
/// 3. Let the scheduler decide whether to switch.
///
/// Step 3 may not return for a long time: it switches to another thread
/// and comes back only when something switches here again.
///
/// # Safety
///
/// Called from the timer stub, with IF clear (interrupt gate).
unsafe fn handle_timer() {
    // SAFETY: Called once per timer interrupt, from its handler.
    unsafe {
        crate::apic::count_and_acknowledge();
    }

    // The `thread-interrupt-alloc` scenario arms a hook here. It does
    // nothing unless armed, and when armed it panics -- which is the
    // point: ADR 022's tripwire has never been exercised from a real
    // interrupt handler before M6, because there were no interrupts.
    crate::threadtest::allocate_from_handler_if_armed();

    // SAFETY: We are in the timer handler with interrupts disabled.
    unsafe {
        crate::sched::tick();
    }
}

/// Common dispatcher. Every stub ends up here.
///
/// Returning from this function makes the stub pop the saved registers and
/// execute `iretq`, which resumes the interrupted code. For anything that is
/// not a trap we never return: we report and exit instead, because M1 has no
/// mechanism for resuming from a fault.
///
/// # Safety
///
/// Called only from the assembly stubs below, with `frame` pointing at the
/// register block they just built on the current stack.
#[unsafe(no_mangle)]
unsafe extern "C" fn exception_dispatch(frame: *mut ExceptionFrame) {
    // SAFETY: The stubs pass a pointer to the frame they pushed on the
    // current stack. It is properly aligned (the stack is 16-byte aligned at
    // entry and every push is 8 bytes), fully initialised (every field was
    // written by a `push`), and valid for the whole call because the stub
    // does not touch the stack below it until we return. We take a shared
    // reference: nothing here mutates the saved state.
    let frame = unsafe { &*frame };

    // Every entry through this path is interrupt context, and ADR 022's
    // rule is that interrupt context does not allocate. In M5 this
    // counter was incremented only by a test; from M6 it is maintained
    // for real, which is what makes the tripwire a protection rather
    // than a gesture.
    //
    // Paired with `exit_interrupt` on every path out -- including the
    // ones that never return, where it does not matter because the
    // machine is stopping.
    crate::memory::frame_allocator::enter_interrupt();

    match frame.vector {
        3 => handle_breakpoint(frame),
        8 => handle_double_fault(frame),
        TIMER_VECTOR => {
            // SAFETY: Entered through an interrupt gate, so IF is clear.
            unsafe {
                handle_timer();
            }
        }
        SPURIOUS_VECTOR => {
            // The LAPIC delivers this when an interrupt is withdrawn
            // between being raised and being dispatched. It needs no
            // EOI -- that is the one thing the spurious vector is
            // specified not to want.
            //
            // Reference: Intel SDM Vol. 3A, 11.9 "Spurious Interrupt".
            crate::apic::count_spurious();
        }
        vector @ IRQ_VECTOR_FIRST..=IRQ_VECTOR_LAST => {
            // SAFETY: Entered through an interrupt gate, so IF is
            // clear, and the vector is in the IRQ range by the
            // pattern above.
            unsafe {
                handle_irq(vector);
            }
        }
        _ => {
            report(frame);
            // No fault-recovery machinery exists yet, and resuming would
            // re-execute the faulting instruction forever. Panic so the
            // report goes out through the one path the tests already know,
            // with a host status of 35.
            panic!(
                "unhandled exception: {} (vector {})",
                vector_name(frame.vector),
                frame.vector
            );
        }
    }

    crate::memory::frame_allocator::exit_interrupt();
}

// Addresses of the stubs. Defined by the assembly below; read by
// `idt::init` to populate the gate descriptors.
// (Plain comments: rustdoc does not document items in extern blocks.)
unsafe extern "C" {
    // `isr_table[i]` is the entry point for stub `i`. For i < 32 that
    // is exception vector i; 32 and 33 are the timer and the spurious
    // vector, whose actual vector numbers come from `apic`.
    pub static isr_table: [unsafe extern "C" fn(); STUB_COUNT];
}

// ---- The stubs ---------------------------------------------------------
//
// Generated by a macro_rules-driven global_asm! so that all 32 look alike
// and the error-code/no-error-code distinction is made in exactly one place.
//
// Each stub leaves this on the stack before jumping to the common tail:
//
//      [rsp + 0]   vector
//      [rsp + 8]   error code (real, or 0 for vectors that push none)
//      [rsp + 16]  rip      <- the CPU's frame starts here
//      [rsp + 24]  cs
//      [rsp + 32]  rflags
//      [rsp + 40]  rsp
//      [rsp + 48]  ss
//
// The common tail then pushes the 15 general registers, which puts the
// complete ExceptionFrame at RSP.

/// Emit one stub for a vector whose exception pushes no error code.
///
/// The dummy `push 0` keeps the frame layout identical to the error-code
/// case. Without it the dispatcher would have to branch on the vector just
/// to find RIP.
macro_rules! isr_no_error_code {
    ($vector:literal) => {
        concat!(
            ".align 16\n",
            "isr_stub_",
            stringify!($vector),
            ":\n",
            "    push 0\n", // dummy error code
            "    push ",
            stringify!($vector),
            "\n", // vector
            "    jmp isr_common\n",
        )
    };
}

/// Emit one stub for a vector whose exception pushes a real error code.
macro_rules! isr_with_error_code {
    ($vector:literal) => {
        concat!(
            ".align 16\n",
            "isr_stub_",
            stringify!($vector),
            ":\n",
            // The CPU already pushed the error code; just add the vector.
            "    push ",
            stringify!($vector),
            "\n",
            "    jmp isr_common\n",
        )
    };
}

global_asm!(
    ".section .text",
    ".code64",
    // ---- common tail ---------------------------------------------------
    //
    // Entered with vector and error code on the stack. Saves every general
    // register, calls the Rust dispatcher, restores, and returns with iretq.
    //
    // All 15 general-purpose registers are saved, not just the callee-saved
    // ones: an exception can interrupt code at any instruction boundary, so
    // the SysV calling convention's "caller-saved" categories do not apply.
    // RSP is excluded because the CPU already pushed it.
    ".align 16",
    "isr_common:",
    "    push rax",
    "    push rbx",
    "    push rcx",
    "    push rdx",
    "    push rsi",
    "    push rdi",
    "    push rbp",
    "    push r8",
    "    push r9",
    "    push r10",
    "    push r11",
    "    push r12",
    "    push r13",
    "    push r14",
    "    push r15",
    // Clear the direction flag: the SysV ABI requires DF=0 on entry to a C
    // function, and the interrupted code may have left it set.
    //
    // Reference: System V AMD64 ABI, section 3.2.1 ("the direction flag
    // DF ... must be clear on function entry and return").
    "    cld",
    // SysV AMD64: first integer argument goes in RDI. RSP now points at the
    // complete ExceptionFrame.
    "    mov rdi, rsp",
    "    call exception_dispatch",
    "    pop r15",
    "    pop r14",
    "    pop r13",
    "    pop r12",
    "    pop r11",
    "    pop r10",
    "    pop r9",
    "    pop r8",
    "    pop rbp",
    "    pop rdi",
    "    pop rsi",
    "    pop rdx",
    "    pop rcx",
    "    pop rbx",
    "    pop rax",
    // Discard the vector and error code we pushed; iretq expects RSP to
    // point at the CPU's own frame.
    "    add rsp, 16",
    "    iretq",
    // ---- the 32 stubs --------------------------------------------------
    isr_no_error_code!(0),
    isr_no_error_code!(1),
    isr_no_error_code!(2),
    isr_no_error_code!(3),
    isr_no_error_code!(4),
    isr_no_error_code!(5),
    isr_no_error_code!(6),
    isr_no_error_code!(7),
    isr_with_error_code!(8),
    isr_no_error_code!(9),
    isr_with_error_code!(10),
    isr_with_error_code!(11),
    isr_with_error_code!(12),
    isr_with_error_code!(13),
    isr_with_error_code!(14),
    isr_no_error_code!(15),
    isr_no_error_code!(16),
    isr_with_error_code!(17),
    isr_no_error_code!(18),
    isr_no_error_code!(19),
    isr_no_error_code!(20),
    isr_with_error_code!(21),
    isr_no_error_code!(22),
    isr_no_error_code!(23),
    isr_no_error_code!(24),
    isr_no_error_code!(25),
    isr_no_error_code!(26),
    isr_no_error_code!(27),
    isr_no_error_code!(28),
    isr_with_error_code!(29),
    isr_with_error_code!(30),
    isr_no_error_code!(31),
    // ---- external interrupts (M6) --------------------------------------
    //
    // Same shape as an exception stub: push a dummy error code, push
    // the vector, jump to the common tail. The dispatcher then cannot
    // tell -- and should not care -- whether it was entered from a
    // fault or from the LAPIC.
    //
    // The vector numbers are written literally rather than through the
    // macro, because the macro derives the label from the number and
    // these two are not at their table index (255 is at index 33).
    ".align 16",
    "isr_stub_timer:",
    "    push 0",
    "    push 32",
    "    jmp isr_common",
    ".align 16",
    "isr_stub_spurious:",
    "    push 0",
    "    push 255",
    "    jmp isr_common",
    // ---- interrupt lines (M10) -----------------------------------------
    //
    // One per line, vectors 33..=48. Written out rather than
    // generated by the macro for the same reason the two above are:
    // the macro derives its label from the vector number, and these
    // labels are named by *line* (`isr_stub_irq_0` delivers on vector
    // 33). Keeping the label in line-space is what makes the table
    // below readable; the `const` assertions above are what keep the
    // literals honest.
    ".align 16",
    "isr_stub_irq_0:",
    "    push 0",
    "    push 33",
    "    jmp isr_common",
    ".align 16",
    "isr_stub_irq_1:",
    "    push 0",
    "    push 34",
    "    jmp isr_common",
    ".align 16",
    "isr_stub_irq_2:",
    "    push 0",
    "    push 35",
    "    jmp isr_common",
    ".align 16",
    "isr_stub_irq_3:",
    "    push 0",
    "    push 36",
    "    jmp isr_common",
    ".align 16",
    "isr_stub_irq_4:",
    "    push 0",
    "    push 37",
    "    jmp isr_common",
    ".align 16",
    "isr_stub_irq_5:",
    "    push 0",
    "    push 38",
    "    jmp isr_common",
    ".align 16",
    "isr_stub_irq_6:",
    "    push 0",
    "    push 39",
    "    jmp isr_common",
    ".align 16",
    "isr_stub_irq_7:",
    "    push 0",
    "    push 40",
    "    jmp isr_common",
    ".align 16",
    "isr_stub_irq_8:",
    "    push 0",
    "    push 41",
    "    jmp isr_common",
    ".align 16",
    "isr_stub_irq_9:",
    "    push 0",
    "    push 42",
    "    jmp isr_common",
    ".align 16",
    "isr_stub_irq_10:",
    "    push 0",
    "    push 43",
    "    jmp isr_common",
    ".align 16",
    "isr_stub_irq_11:",
    "    push 0",
    "    push 44",
    "    jmp isr_common",
    ".align 16",
    "isr_stub_irq_12:",
    "    push 0",
    "    push 45",
    "    jmp isr_common",
    ".align 16",
    "isr_stub_irq_13:",
    "    push 0",
    "    push 46",
    "    jmp isr_common",
    ".align 16",
    "isr_stub_irq_14:",
    "    push 0",
    "    push 47",
    "    jmp isr_common",
    ".align 16",
    "isr_stub_irq_15:",
    "    push 0",
    "    push 48",
    "    jmp isr_common",
    // ---- the table -----------------------------------------------------
    //
    // A plain array of absolute addresses, so Rust can index it instead
    // of needing one `extern` declaration per stub.
    ".section .rodata",
    ".align 8",
    ".global isr_table",
    "isr_table:",
    ".quad isr_stub_0,  isr_stub_1,  isr_stub_2,  isr_stub_3",
    ".quad isr_stub_4,  isr_stub_5,  isr_stub_6,  isr_stub_7",
    ".quad isr_stub_8,  isr_stub_9,  isr_stub_10, isr_stub_11",
    ".quad isr_stub_12, isr_stub_13, isr_stub_14, isr_stub_15",
    ".quad isr_stub_16, isr_stub_17, isr_stub_18, isr_stub_19",
    ".quad isr_stub_20, isr_stub_21, isr_stub_22, isr_stub_23",
    ".quad isr_stub_24, isr_stub_25, isr_stub_26, isr_stub_27",
    ".quad isr_stub_28, isr_stub_29, isr_stub_30, isr_stub_31",
    ".quad isr_stub_timer, isr_stub_spurious",
    ".quad isr_stub_irq_0,  isr_stub_irq_1,  isr_stub_irq_2,  isr_stub_irq_3",
    ".quad isr_stub_irq_4,  isr_stub_irq_5,  isr_stub_irq_6,  isr_stub_irq_7",
    ".quad isr_stub_irq_8,  isr_stub_irq_9,  isr_stub_irq_10, isr_stub_irq_11",
    ".quad isr_stub_irq_12, isr_stub_irq_13, isr_stub_irq_14, isr_stub_irq_15",
);

/// Compile-time cross-check: the error-code classification used by the
/// assembly stubs above must match [`pushes_error_code`].
///
/// If the two ever disagree, every frame for the affected vector would be
/// shifted by eight bytes and the reported RIP would be garbage -- a bug
/// that is very hard to see in a hex dump. This makes it a build failure.
const _: () = {
    // Vectors whose stubs were emitted with `isr_with_error_code!`.
    const ASM_WITH_ERROR_CODE: [usize; 10] = [8, 10, 11, 12, 13, 14, 17, 21, 29, 30];

    let mut vector = 0;
    while vector < EXCEPTION_COUNT {
        let mut in_asm_list = false;
        let mut i = 0;
        while i < ASM_WITH_ERROR_CODE.len() {
            if ASM_WITH_ERROR_CODE[i] == vector {
                in_asm_list = true;
            }
            i += 1;
        }
        assert!(
            in_asm_list == pushes_error_code(vector),
            "stub error-code classification disagrees with pushes_error_code()"
        );
        vector += 1;
    }
};
