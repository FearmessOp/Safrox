// SPDX-License-Identifier: BSD-2-Clause
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
/// Vectors 0..=31 are architecturally defined; 32..=255 are available for
/// external interrupts, which arrive in a later milestone.
///
/// Reference: Intel SDM Vol. 3A, Table 6-1 "Protected-Mode Exceptions and
/// Interrupts".
pub const EXCEPTION_COUNT: usize = 32;

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
/// Reference: Intel SDM Vol. 3A, Figure 6-9 "Page-Fault Error Code".
fn page_fault_cause(error_code: u64) -> (&'static str, &'static str, &'static str) {
    // Bit 0, P: 0 = the fault was caused by a non-present page.
    let present = if error_code & (1 << 0) != 0 {
        "protection-violation"
    } else {
        "not-present"
    };
    // Bit 1, W/R: 1 = the access was a write.
    let access = if error_code & (1 << 1) != 0 {
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
#[unsafe(no_mangle)]
unsafe extern "C" fn exception_dispatch(frame: *mut ExceptionFrame) {
    // SAFETY: The stubs pass a pointer to the frame they pushed on the
    // current stack. It is properly aligned (the stack is 16-byte aligned at
    // entry and every push is 8 bytes), fully initialised (every field was
    // written by a `push`), and valid for the whole call because the stub
    // does not touch the stack below it until we return. We take a shared
    // reference: nothing here mutates the saved state.
    let frame = unsafe { &*frame };

    match frame.vector {
        3 => handle_breakpoint(frame),
        8 => handle_double_fault(frame),
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
}

// Addresses of the 32 stubs, indexed by vector. Defined by the assembly
// below; read by `idt::init` to populate the gate descriptors.
// (Plain comments: rustdoc does not document items in extern blocks.)
unsafe extern "C" {
    // `isr_table[v]` is the entry point for vector `v`.
    pub static isr_table: [unsafe extern "C" fn(); EXCEPTION_COUNT];
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
    // ---- the table -----------------------------------------------------
    //
    // A plain array of 32 absolute addresses, so Rust can index it instead
    // of needing 32 separate `extern` declarations.
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
